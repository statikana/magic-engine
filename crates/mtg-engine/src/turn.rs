use mtg_ir::{AbilityDef, Affected, Duration, GamePhase, ObjectFilter, RuleMod, StaticBody, Zone};
use smallvec::SmallVec;

use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState};
use crate::casting::prepare_next_untargeted_stack_trigger;
use crate::choices::ReplacementResumeOutcome;
use crate::combat::{
    CombatOutcome, any_combatant_has_first_strike, end_combat_phase_immediately,
    enter_combat_phase, enter_declare_attackers_step, enter_declare_blockers_step,
    execute_combat_damage_step,
};
use crate::config::MulliganRule;
use crate::eval::{BoardView, EvalContext, evaluate_condition};
use crate::events::{AtomicEventKind, emit_event};
use crate::faces::{
    face_opts_for_object, for_each_active_ability_with_card_on_faces, is_saga_card,
    resolve_object_faces,
};
use crate::filter::matches_object_filter;
use crate::id::{ObjectId, PlayerId};
use crate::keywords::{has_keyword, has_phasing};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::mana::{clear_mana_pools_at_cleanup, empty_mana_pools_for_step};
use crate::replace::{
    ReplacementOutcome, ReplacementResult, clear_eot_prevention_shields,
    prune_stale_prevention_shields, resolve_draw, resolve_put_counters, resolve_untap,
};
use crate::sba::{SbaOutcome, check_sbas, check_sbas_with_cleanup, cleanup_eliminated_player};
use crate::stack::resolve_top_of_stack;
use crate::state::{
    DirtyMask, EngineError, GameState, LayerCache, ObjectFlags, PendingPromptContext, PromptOwner,
    ReplaceableEventKind, ReplacementResumeHook, TimedCopiableBackup, TurnPromptKind,
    TurnReplacementContext,
};
use crate::triggers::{check_state_triggers, clear_turn_trigger_flags_only, flush_triggers};
use crate::zones::{
    MoveObjectOutcome, MoveRawOptions, ZonePosition, draw_one_raw, move_object, move_object_raw,
    phase_in_untap_step, phase_out_permanent, shuffle_library,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnAdvanceOutcome {
    Continued,
    Paused { prompt_id: u32, chooser: PlayerId },
    GameOver { winner: Option<PlayerId> },
}

impl From<CombatOutcome> for TurnAdvanceOutcome {
    #[inline]
    fn from(v: CombatOutcome) -> Self {
        match v {
            CombatOutcome::Continued => TurnAdvanceOutcome::Continued,
            CombatOutcome::Paused { prompt_id, chooser } => {
                TurnAdvanceOutcome::Paused { prompt_id, chooser }
            }
            CombatOutcome::GameOver { winner } => TurnAdvanceOutcome::GameOver { winner },
        }
    }
}

pub fn begin_opening_mulligans(state: &mut GameState) -> Result<(), EngineError> {
    let n = state.players().len();
    for i in 0..n {
        let pid = PlayerId(i as u8);
        if let Some(p) = state.player_mut(pid) {
            p.has_kept_hand = p.lost;
            p.mulligan_count = 0;
        }
        state.set_mulligan_bottom_remaining(pid, 0);
    }
    let starter = first_alive_from(state, state.active_player()).unwrap_or(PlayerId(0));
    state.set_active_player(starter);
    state.set_priority_player(starter);
    state.set_phase(GamePhase::Main1);
    state.set_turn_number(1);
    state.set_turn_serial(1);
    state.set_last_normal_turn_player(Some(starter));
    state.clear_passed_priority();
    state.set_decision(DecisionState::Mulligan);
    Ok(())
}

pub fn legal_turn_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let mut actions = Vec::new();
    match state.decision() {
        DecisionState::Mulligan => {
            if player != state.priority_player() {
                return actions;
            }
            let Some(p) = state.player(player) else {
                return actions;
            };
            if p.lost {
                return actions;
            }
            let bottom_rem = state.mulligan_bottom_remaining(player);
            if bottom_rem > 0 {
                for &oid in &p.hand {
                    actions.push(Action::BottomCard { object: oid });
                }
            } else if !p.has_kept_hand {
                actions.push(Action::KeepHand);
                let max_mulls = usize::from(state.config().starting_hand_size);
                if usize::from(p.mulligan_count) < max_mulls && !p.hand.is_empty() {
                    actions.push(Action::TakeMulligan);
                }
            }
        }
        DecisionState::Priority
            if state.pending_prompt().is_none()
                && state.pending_replacement().is_none()
                && state.pending_trigger_order().is_none()
                && state.casting().is_none()
                && player == state.priority_player() =>
        {
            actions.push(Action::PassPriority);
        }
        _ => {}
    }
    actions
}

pub fn apply_mulligan_action(
    state: &mut GameState,
    player: PlayerId,
    action: &Action,
) -> Result<bool, EngineError> {
    if !matches!(state.decision(), DecisionState::Mulligan) {
        return Err(EngineError::IllegalAction(
            "mulligan action outside Mulligan decision state".into(),
        ));
    }
    if player != state.priority_player() {
        return Err(EngineError::IllegalAction(
            "not this player's turn to mulligan".into(),
        ));
    }
    let bottom_rem = state.mulligan_bottom_remaining(player);

    match action {
        Action::TakeMulligan => {
            if bottom_rem > 0 {
                return Err(EngineError::IllegalAction(
                    "must finish putting cards on bottom before anything else".into(),
                ));
            }
            let (already_kept, hand_ids, mull_count) = state
                .player(player)
                .map(|p| (p.has_kept_hand, p.hand.clone(), p.mulligan_count))
                .ok_or_else(|| EngineError::IllegalAction("unknown player".into()))?;
            let max_mulls = usize::from(state.config().starting_hand_size);
            if already_kept || usize::from(mull_count) >= max_mulls || hand_ids.is_empty() {
                return Err(EngineError::IllegalAction(
                    "cannot take another mulligan".into(),
                ));
            }
            // Insert hand cards at bottom (index 0) in hand order so `shuffle_library`'s
            // pre-shuffle reverse feeds `[L_0..L_{k-1}, H_0..H_{m-1}]` to `Mulberry32`
            // Fisher-Yates, matching TS `mulliganRedraw` (`zones.ts:4725-4734`).
            for oid in hand_ids {
                let _ = move_object_raw(
                    state,
                    oid,
                    Zone::Library,
                    MoveRawOptions {
                        position: ZonePosition::Bottom,
                        ..Default::default()
                    },
                    None,
                )?;
            }
            shuffle_library(state, player, 0)?;
            let draw_n = usize::from(state.config().starting_hand_size);
            for _ in 0..draw_n {
                let _ = draw_one_raw(state, player)?;
            }
            if let Some(p) = state.player_mut(player) {
                p.mulligan_count = p.mulligan_count.saturating_add(1);
            }
            state.mark_dirty(DirtyMask::PLAYER_STATE);
            Ok(false)
        }
        Action::KeepHand => {
            if bottom_rem > 0 {
                return Err(EngineError::IllegalAction(
                    "already kept hand; finish bottoming cards".into(),
                ));
            }
            let (already_kept, hand_len, mull_count) = state
                .player(player)
                .map(|p| (p.has_kept_hand, p.hand.len(), p.mulligan_count))
                .ok_or_else(|| EngineError::IllegalAction("unknown player".into()))?;
            if already_kept {
                return Err(EngineError::IllegalAction(
                    "player has already kept their opening hand".into(),
                ));
            }
            let free = match state.config().mulligan {
                MulliganRule::London { free_first } => u8::from(free_first),
            };
            let to_bottom = mull_count.saturating_sub(free).min(hand_len as u8);
            if let Some(p) = state.player_mut(player) {
                p.has_kept_hand = true;
            }
            state.set_mulligan_bottom_remaining(player, to_bottom);
            state.mark_dirty(DirtyMask::PLAYER_STATE);
            advance_mulligan_or_start_turn_one(state)
        }
        Action::BottomCard { object } => {
            if bottom_rem == 0 {
                return Err(EngineError::IllegalAction(
                    "no cards remaining to put on bottom".into(),
                ));
            }
            let in_hand = state
                .player(player)
                .is_some_and(|p| p.hand.contains(object));
            if !in_hand {
                return Err(EngineError::IllegalAction(
                    "card to bottom is not in player's hand".into(),
                ));
            }
            let _ = move_object_raw(
                state,
                *object,
                Zone::Library,
                MoveRawOptions {
                    position: ZonePosition::Bottom,
                    ..Default::default()
                },
                None,
            )?;
            let next_rem = bottom_rem - 1;
            state.set_mulligan_bottom_remaining(player, next_rem);
            if next_rem > 0 {
                return Ok(false);
            }
            advance_mulligan_or_start_turn_one(state)
        }
        _ => Err(EngineError::IllegalAction(
            "expected mulligan action".into(),
        )),
    }
}

fn advance_mulligan_or_start_turn_one(state: &mut GameState) -> Result<bool, EngineError> {
    if let Some(next_p) = find_next_mulligan_player(state) {
        state.set_priority_player(next_p);
        state.set_decision(DecisionState::Mulligan);
        return Ok(false);
    }

    // All players kept & bottomed. Process PregameBeginOnBattlefield ("Leylines") in APNAP order.
    let active = first_alive_from(state, state.active_player()).unwrap_or(PlayerId(0));
    state.set_active_player(active);
    let n = state.players().len();
    for offset in 0..n {
        let pid = PlayerId(((usize::from(active.0) + offset) % n) as u8);
        if state.player(pid).is_some_and(|p| p.lost) {
            continue;
        }
        let hand_ids = state
            .player(pid)
            .map(|p| p.hand.clone())
            .unwrap_or_default();
        for oid in hand_ids {
            if object_has_pregame_begin_on_battlefield(state, oid) {
                let _ = move_object(
                    state,
                    oid,
                    Zone::Battlefield,
                    MoveRawOptions {
                        controller: Some(pid),
                        ..Default::default()
                    },
                    None,
                )?;
            }
        }
    }

    state.set_phase(GamePhase::Main1);
    state.set_turn_number(1);
    state.set_turn_serial(1);
    state.set_last_normal_turn_player(Some(active));
    state.clear_passed_priority();
    emit_event(
        state,
        AtomicEventKind::PhaseStepEntered {
            phase: GamePhase::Main1,
            active_player: active,
        },
    );
    let _ = grant_priority_with_sba_and_triggers(state)?;
    Ok(true)
}

fn find_next_mulligan_player(state: &GameState) -> Option<PlayerId> {
    let n = state.players().len();
    let cur = usize::from(state.priority_player().0);
    // CR 103.5 Stage 1: every active player keeps or mulligans before anyone bottoms cards.
    for offset in 1..=n {
        let pid = PlayerId(((cur + offset) % n) as u8);
        if let Some(p) = state.player(pid)
            && !p.lost
            && !p.has_kept_hand
        {
            return Some(pid);
        }
    }
    // CR 103.5 Stage 2: starting with the starting player and proceeding in turn order,
    // each player who took at least one mulligan puts cards on the bottom.
    let start = usize::from(state.active_player().0);
    for offset in 0..n {
        let pid = PlayerId(((start + offset) % n) as u8);
        if let Some(p) = state.player(pid)
            && !p.lost
            && state.mulligan_bottom_remaining(pid) > 0
        {
            return Some(pid);
        }
    }
    None
}

pub fn pass_priority(
    state: &mut GameState,
    player: PlayerId,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if let DecisionState::GameOver { winner } = state.decision() {
        return Ok(TurnAdvanceOutcome::GameOver { winner: *winner });
    }
    if !matches!(state.decision(), DecisionState::Priority) {
        return Err(EngineError::IllegalAction(
            "cannot pass priority outside Priority state".into(),
        ));
    }
    if state.pending_prompt().is_some()
        || state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
        || state.casting().is_some()
    {
        return Err(EngineError::IllegalAction(
            "cannot pass priority while prompt or casting is active".into(),
        ));
    }
    if state.priority_player() != player {
        return Err(EngineError::IllegalAction(
            "player does not hold priority".into(),
        ));
    }

    state.mark_passed_priority(player);
    let alive = alive_player_mask(state);
    if (state.passed_priority_mask() & alive) != alive {
        // Rotate priority cleanly WITHOUT invalidating the LayerCache (`t1_f17_05`).
        if let Some(next_p) = next_alive_after(state, player) {
            state.set_priority_player(next_p);
        }
        return Ok(TurnAdvanceOutcome::Continued);
    }

    state.clear_passed_priority();
    if !state.stack().is_empty() {
        let completed = resolve_top_of_stack(state)?;
        if let Some(paused) = check_paused_or_gameover(state) {
            return Ok(paused);
        }
        if completed {
            return grant_priority_with_sba_and_triggers(state);
        }
        return Ok(TurnAdvanceOutcome::Continued);
    }

    advance_phase_or_step(state)
}

pub fn apply_concede(
    state: &mut GameState,
    player: PlayerId,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if let Some(p) = state.player_mut(player) {
        p.lost = true;
        p.has_kept_hand = true;
    }
    state.set_mulligan_bottom_remaining(player, 0);
    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
    emit_event(state, AtomicEventKind::PlayerLostGame { player });
    cleanup_eliminated_player(state, player);

    let alive: SmallVec<[PlayerId; 4]> = state
        .players()
        .iter()
        .filter(|p| !p.lost)
        .map(|p| p.id)
        .collect();
    if alive.len() <= 1 {
        let winner = alive.first().copied();
        state.set_pending_prompt(None);
        state.set_pending_replacement(None);
        state.set_pending_replacement_resume(None);
        state.set_pending_trigger_order(None);
        state.set_pending_sba_batch(None);
        state.set_pending_sba_in_cleanup(false);
        state.set_pending_land_play(None);
        state.set_casting(None);
        state.clear_pending_mana_delayed_riders();
        state.resolution_frames_mut().clear();
        state.resolution_vms_mut().clear();
        state.combat_mut().pending_damage = None;
        state.set_decision(DecisionState::GameOver { winner });
        return Ok(TurnAdvanceOutcome::GameOver { winner });
    }

    if state.casting().is_some_and(|c| c.caster == player) {
        state.set_casting(None);
        state.clear_pending_mana_delayed_riders();
        if matches!(state.decision(), DecisionState::CastingSequence) {
            state.set_decision(DecisionState::Priority);
        }
    }

    let mut need_flush_triggers = false;
    if state
        .pending_trigger_order()
        .is_some_and(|to| to.controller == player)
    {
        state.set_pending_trigger_order(None);
        if matches!(state.decision(), DecisionState::Prompt { .. }) {
            state.set_decision(DecisionState::Priority);
        }
        need_flush_triggers = true;
    }

    if state
        .pending_replacement()
        .is_some_and(|pr| pr.chooser == player)
    {
        state.set_pending_replacement(None);
        let hook = state.take_pending_replacement_resume();
        let _ = state.take_pending_land_play();
        if matches!(state.decision(), DecisionState::Prompt { .. }) {
            state.set_decision(DecisionState::Priority);
        }
        match hook {
            Some(ReplacementResumeHook::ResolutionFrame) => {
                match crate::effects::abort_resolution_prompt_on_concede(state, player)? {
                    crate::effects::FrameOutcome::Completed => {
                        crate::stack::finish_or_resume_stack_resolution(state)?;
                        return Ok(check_paused_or_gameover(state)
                            .unwrap_or(TurnAdvanceOutcome::Continued));
                    }
                    crate::effects::FrameOutcome::Paused { prompt_id, chooser } => {
                        return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
                    }
                    crate::effects::FrameOutcome::GameOver { winner } => {
                        return Ok(TurnAdvanceOutcome::GameOver { winner });
                    }
                }
            }
            Some(ReplacementResumeHook::CombatDamageStep) => {
                return Ok(crate::combat::resume_combat_damage_replacement_on_concede(
                    state, player,
                )?
                .into());
            }
            Some(ReplacementResumeHook::SbaLoop { in_cleanup }) => {
                crate::events::end_simultaneous_batch(state);
                let _ = state.take_pending_sba_batch();
                state.set_pending_sba_in_cleanup(false);
                match check_sbas_with_cleanup(state, in_cleanup)? {
                    SbaOutcome::Done => {}
                    SbaOutcome::Paused { prompt_id, chooser } => {
                        return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
                    }
                    SbaOutcome::GameOver { winner } => {
                        return Ok(TurnAdvanceOutcome::GameOver { winner });
                    }
                }
                if in_cleanup {
                    return finish_cleanup_after_sba(state);
                }
                return grant_priority_with_sba_and_triggers(state);
            }
            Some(ReplacementResumeHook::TurnStep(TurnReplacementContext::UntapStep)) => {
                return finish_untap_to_upkeep(state);
            }
            Some(ReplacementResumeHook::TurnStep(TurnReplacementContext::CleanupDiscard {
                ..
            })) => {
                return run_cleanup_step(state);
            }
            Some(ReplacementResumeHook::TurnStep(TurnReplacementContext::DrawStep { .. }))
            | Some(ReplacementResumeHook::StackResolutionFinish { .. }) => {
                flush_triggers(state);
                let _ = prepare_next_untargeted_stack_trigger(state)?;
                return grant_priority_with_sba_and_triggers(state);
            }
            None => {}
        }
    }

    if state
        .pending_prompt()
        .is_some_and(|pp| pp.chooser == player)
    {
        let pp = state.take_pending_prompt();
        if matches!(state.decision(), DecisionState::Prompt { .. }) {
            state.set_decision(DecisionState::Priority);
        }
        if let Some(pp) = pp {
            match pp.owner {
                PromptOwner::ResolutionFrame => {
                    match crate::effects::abort_resolution_prompt_on_concede(state, player)? {
                        crate::effects::FrameOutcome::Completed => {
                            crate::stack::finish_or_resume_stack_resolution(state)?;
                            return Ok(check_paused_or_gameover(state)
                                .unwrap_or(TurnAdvanceOutcome::Continued));
                        }
                        crate::effects::FrameOutcome::Paused { prompt_id, chooser } => {
                            return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
                        }
                        crate::effects::FrameOutcome::GameOver { winner } => {
                            return Ok(TurnAdvanceOutcome::GameOver { winner });
                        }
                    }
                }
                PromptOwner::Combat(kind) => {
                    return Ok(crate::combat::resume_combat_prompt_on_concede(
                        state, player, kind,
                    )?
                    .into());
                }
                PromptOwner::Turn(TurnPromptKind::CleanupDiscard { .. }) => {
                    return run_cleanup_step(state);
                }
                PromptOwner::Turn(TurnPromptKind::UntapOptional { .. }) => {
                    return finish_untap_to_upkeep(state);
                }
                PromptOwner::Turn(TurnPromptKind::EndTurnMay) => {
                    return grant_priority_with_sba_and_triggers(state);
                }
                PromptOwner::Sba(_) => {
                    let in_cleanup = state.pending_sba_in_cleanup();
                    state.set_pending_sba_in_cleanup(false);
                    match check_sbas_with_cleanup(state, in_cleanup)? {
                        SbaOutcome::Done => {
                            if in_cleanup {
                                return finish_cleanup_after_sba(state);
                            }
                            return grant_priority_with_sba_and_triggers(state);
                        }
                        SbaOutcome::Paused { prompt_id, chooser } => {
                            return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
                        }
                        SbaOutcome::GameOver { winner } => {
                            return Ok(TurnAdvanceOutcome::GameOver { winner });
                        }
                    }
                }
                PromptOwner::Casting(_)
                | PromptOwner::ManaAbility(_)
                | PromptOwner::CostPayment(_)
                | PromptOwner::Replacement
                | PromptOwner::TriggerOrder => {}
            }
        }
    }

    if matches!(state.decision(), DecisionState::Mulligan) {
        if state.active_player() == player
            && let Some(new_active) = first_alive_from(state, player)
        {
            state.set_active_player(new_active);
            state.set_last_normal_turn_player(Some(new_active));
        }
        if state.priority_player() == player
            || state
                .player(state.priority_player())
                .is_some_and(|p| p.lost)
        {
            if let Some(next_p) = find_next_mulligan_player(state) {
                state.set_priority_player(next_p);
            } else {
                let _ = advance_mulligan_or_start_turn_one(state)?;
            }
        }
        return Ok(TurnAdvanceOutcome::Continued);
    }

    if state.pending_prompt().is_none()
        && state.pending_replacement().is_none()
        && state.pending_trigger_order().is_none()
        && state.casting().is_none()
    {
        match state.decision() {
            DecisionState::DeclareAttackers => {
                if state.player(state.active_player()).is_some_and(|p| p.lost)
                    || state.priority_player() == player
                {
                    let combat = state.combat_mut();
                    combat.attackers.clear();
                    combat.attackers_declared = true;
                    state.mark_dirty(DirtyMask::COMBAT);
                    return grant_priority_with_sba_and_triggers(state);
                }
                return Ok(TurnAdvanceOutcome::Continued);
            }
            DecisionState::DeclareBlockers => {
                return Ok(crate::combat::advance_declare_blockers_on_concede(state)?.into());
            }
            DecisionState::AssignCombatDamage => {
                if state.player(state.active_player()).is_some_and(|p| p.lost)
                    || state.combat().attackers.is_empty()
                {
                    return grant_priority_with_sba_and_triggers(state);
                }
                return Ok(TurnAdvanceOutcome::Continued);
            }
            _ => {}
        }
    }

    if (state.priority_player() == player
        || state
            .player(state.priority_player())
            .is_some_and(|p| p.lost))
        && let Some(next_p) = next_alive_after(state, state.priority_player())
    {
        state.set_priority_player(next_p);
    }

    if need_flush_triggers {
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
        if let Some(paused) = check_paused_or_gameover(state) {
            return Ok(paused);
        }
    }

    if matches!(state.decision(), DecisionState::Priority)
        && state.pending_prompt().is_none()
        && state.pending_replacement().is_none()
        && state.pending_trigger_order().is_none()
        && state.casting().is_none()
    {
        let alive_mask = alive_player_mask(state);
        if (state.passed_priority_mask() & alive_mask) == alive_mask {
            state.clear_passed_priority();
            if !state.stack().is_empty() {
                let completed = resolve_top_of_stack(state)?;
                if let Some(paused) = check_paused_or_gameover(state) {
                    return Ok(paused);
                }
                if completed {
                    return grant_priority_with_sba_and_triggers(state);
                }
                return Ok(TurnAdvanceOutcome::Continued);
            }
            return advance_phase_or_step(state);
        }
    }
    Ok(TurnAdvanceOutcome::Continued)
}

pub fn resume_turn_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<TurnAdvanceOutcome, EngineError> {
    let Some(pp) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no pending turn prompt to resume".into(),
        ));
    };
    if pp.chooser != actor || pp.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "turn prompt actor or prompt_id mismatch".into(),
        ));
    }
    let PromptOwner::Turn(ref kind) = pp.owner else {
        return Err(EngineError::IllegalAction(
            "pending prompt is not owned by turn".into(),
        ));
    };

    match kind {
        TurnPromptKind::CleanupDiscard { .. } => {
            let ChoiceAnswer::PickObject { id } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickObject for CleanupDiscard".into(),
                ));
            };
            if !state.player(actor).is_some_and(|p| p.hand.contains(&id)) {
                return Err(EngineError::IllegalAction(
                    "chosen discard card is not in hand".into(),
                ));
            }
        }
        TurnPromptKind::UntapOptional { .. } => {
            if !matches!(answer, ChoiceAnswer::YesNo { .. }) {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for UntapOptional".into(),
                ));
            }
        }
        TurnPromptKind::EndTurnMay => {
            if !matches!(answer, ChoiceAnswer::YesNo { .. }) {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for EndTurnMay".into(),
                ));
            }
        }
    }

    let Some(pp) = state.take_pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no pending turn prompt to resume".into(),
        ));
    };
    let PromptOwner::Turn(kind) = pp.owner else {
        state.set_pending_prompt(Some(pp));
        return Err(EngineError::IllegalAction(
            "pending prompt is not owned by turn".into(),
        ));
    };
    if matches!(state.decision(), DecisionState::Prompt { .. }) {
        state.set_decision(DecisionState::Priority);
    }

    match kind {
        TurnPromptKind::CleanupDiscard { remaining, picked } => {
            let ChoiceAnswer::PickObject { id } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickObject for CleanupDiscard".into(),
                ));
            };
            let _ = picked;
            let next_rem = remaining.saturating_sub(1);
            match move_object(
                state,
                id,
                Zone::Graveyard,
                MoveRawOptions {
                    is_discard: true,
                    discard_caused_by: None,
                    ..Default::default()
                },
                None,
            )? {
                MoveObjectOutcome::Paused {
                    prompt_id: pid,
                    chooser,
                } => {
                    state.set_pending_replacement_resume(Some(ReplacementResumeHook::TurnStep(
                        TurnReplacementContext::CleanupDiscard {
                            remaining_excess: next_rem,
                        },
                    )));
                    return Ok(TurnAdvanceOutcome::Paused {
                        prompt_id: pid,
                        chooser,
                    });
                }
                MoveObjectOutcome::Moved(raw) => {
                    let actual_id = raw.new_id.unwrap_or(raw.old_id);
                    emit_event(
                        state,
                        AtomicEventKind::Discard {
                            player: actor,
                            object: actual_id,
                            dest_zone: raw.to_zone,
                            is_cycle: false,
                            cause_controller: None,
                        },
                    );
                }
                MoveObjectOutcome::Skipped { .. } | MoveObjectOutcome::ExecuteInstead { .. } => {}
            }
            continue_cleanup_discard_or_finish(state, actor, next_rem)
        }
        TurnPromptKind::UntapOptional {
            candidates,
            next_idx,
        } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for UntapOptional".into(),
                ));
            };
            if value && let Some(&oid) = candidates.get(next_idx) {
                match untap_single_permanent(state, oid, Some(actor))? {
                    UntapSingleResult::Paused {
                        prompt_id: pid,
                        chooser,
                    } => {
                        return Ok(TurnAdvanceOutcome::Paused {
                            prompt_id: pid,
                            chooser,
                        });
                    }
                    UntapSingleResult::Done => {}
                }
            }
            prompt_or_finish_optional_untaps(state, actor, candidates, next_idx + 1)
        }
        TurnPromptKind::EndTurnMay => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for EndTurnMay".into(),
                ));
            };
            if value {
                end_the_turn_immediately(state, actor)
            } else {
                grant_priority_with_sba_and_triggers(state)
            }
        }
    }
}

pub fn resume_turn_after_replacement(
    state: &mut GameState,
    ctx: TurnReplacementContext,
    outcome: ReplacementResumeOutcome,
) -> Result<TurnAdvanceOutcome, EngineError> {
    match (ctx, outcome) {
        (_, ReplacementResumeOutcome::Paused { prompt_id, chooser }) => {
            Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser })
        }
        (TurnReplacementContext::UntapStep, _) => finish_untap_to_upkeep(state),
        (
            TurnReplacementContext::DrawStep { remaining_draws },
            ReplacementResumeOutcome::EffectResolved(res),
        ) => {
            apply_draw_replacement_outcome(state, state.active_player(), *res)?;
            if remaining_draws > 0 {
                continue_draw_step_loop(state, state.active_player(), remaining_draws)
            } else {
                grant_priority_with_sba_and_triggers(state)
            }
        }
        (
            TurnReplacementContext::CleanupDiscard { remaining_excess },
            ReplacementResumeOutcome::ZoneMove(mv),
        ) => {
            let active = state.active_player();
            if let MoveObjectOutcome::Moved(raw) = mv {
                let actual_id = raw.new_id.unwrap_or(raw.old_id);
                emit_event(
                    state,
                    AtomicEventKind::Discard {
                        player: active,
                        object: actual_id,
                        dest_zone: raw.to_zone,
                        is_cycle: false,
                        cause_controller: None,
                    },
                );
            }
            continue_cleanup_discard_or_finish(state, active, remaining_excess)
        }
        _ => grant_priority_with_sba_and_triggers(state),
    }
}

pub fn end_the_turn_immediately(
    state: &mut GameState,
    _actor: PlayerId,
) -> Result<TurnAdvanceOutcome, EngineError> {
    // CR 725.1b: Exile all spells and abilities from the stack.
    let stack_objs: Vec<ObjectId> = state.stack().to_vec();
    state.stack_mut().clear();
    state.stack_entries_mut().clear();
    state.resolution_vms_mut().clear();
    state.resolution_frames_mut().clear();
    state.set_casting(None);
    state.set_pending_prompt(None);
    state.set_pending_replacement(None);
    state.set_pending_trigger_order(None);
    state.set_pending_replacement_resume(None);
    if matches!(state.decision(), DecisionState::Prompt { .. }) {
        state.set_decision(DecisionState::Priority);
    }
    state.clear_passed_priority();

    for oid in stack_objs {
        if state.object(oid).is_some_and(|o| o.zone == Zone::Stack) {
            let _ = move_object_raw(state, oid, Zone::Exile, MoveRawOptions::default(), None)?;
        }
    }

    let _ = end_combat_phase_immediately(state)?;
    empty_mana_pools_for_step(state);
    state.set_phase(GamePhase::Cleanup);
    emit_event(
        state,
        AtomicEventKind::PhaseStepEntered {
            phase: GamePhase::Cleanup,
            active_player: state.active_player(),
        },
    );
    run_cleanup_step(state)
}

pub fn grant_priority_with_sba_and_triggers(
    state: &mut GameState,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if let Some(paused) = check_paused_or_gameover(state) {
        return Ok(paused);
    }
    if matches!(state.decision(), DecisionState::Prompt { .. }) {
        state.set_decision(DecisionState::Priority);
    }
    match check_sbas(state)? {
        SbaOutcome::Done => {}
        SbaOutcome::Paused { prompt_id, chooser } => {
            return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
        }
        SbaOutcome::GameOver { winner } => {
            return Ok(TurnAdvanceOutcome::GameOver { winner });
        }
    }
    check_state_triggers(state);
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    if let Some(paused) = check_paused_or_gameover(state) {
        return Ok(paused);
    }

    let prio = if state.player(state.active_player()).is_some_and(|p| !p.lost) {
        state.active_player()
    } else {
        first_alive_from(state, state.active_player()).unwrap_or(PlayerId(0))
    };
    state.set_priority_player(prio);
    state.clear_passed_priority();
    state.set_decision(DecisionState::Priority);
    Ok(TurnAdvanceOutcome::Continued)
}

pub(crate) fn advance_phase_or_step(
    state: &mut GameState,
) -> Result<TurnAdvanceOutcome, EngineError> {
    let prev_phase = state.phase();
    if prev_phase == GamePhase::Cleanup {
        return run_cleanup_step(state);
    }

    empty_mana_pools_for_step(state);
    if prev_phase == GamePhase::EndCombat {
        // Expire `UntilEndOfCombat` effects and leave combat.
        expire_effects_for_duration(state, Duration::UntilEndOfCombat);
        let _ = end_combat_phase_immediately(state)?;
    }

    let next_phase = match prev_phase {
        GamePhase::Untap => GamePhase::Upkeep,
        GamePhase::Upkeep => GamePhase::Draw,
        GamePhase::Draw => GamePhase::Main1,
        GamePhase::Main1 => {
            if should_skip_next_combat(state, state.active_player()) {
                GamePhase::Main2
            } else {
                GamePhase::BeginCombat
            }
        }
        GamePhase::BeginCombat => GamePhase::DeclareAttackers,
        GamePhase::DeclareAttackers => {
            if state.combat().attackers.is_empty() {
                expire_effects_for_duration(state, Duration::UntilEndOfCombat);
                let _ = end_combat_phase_immediately(state)?;
                if state.additional_combat_phases() > 0 {
                    if state.additional_main_after_combat() {
                        state.set_additional_main_after_combat(false);
                        GamePhase::Main2
                    } else {
                        state.set_additional_combat_phases(state.additional_combat_phases() - 1);
                        GamePhase::BeginCombat
                    }
                } else {
                    GamePhase::Main2
                }
            } else {
                GamePhase::DeclareBlockers
            }
        }
        GamePhase::DeclareBlockers => {
            if any_combatant_has_first_strike(state) {
                GamePhase::FirstStrikeDamage
            } else {
                GamePhase::CombatDamage
            }
        }
        GamePhase::FirstStrikeDamage => GamePhase::CombatDamage,
        GamePhase::CombatDamage => GamePhase::EndCombat,
        GamePhase::EndCombat => {
            if state.additional_combat_phases() > 0 {
                if state.additional_main_after_combat() {
                    state.set_additional_main_after_combat(false);
                    GamePhase::Main2
                } else {
                    state.set_additional_combat_phases(state.additional_combat_phases() - 1);
                    GamePhase::BeginCombat
                }
            } else {
                GamePhase::Main2
            }
        }
        GamePhase::Main2 => {
            if state.additional_combat_phases() > 0 {
                state.set_additional_combat_phases(state.additional_combat_phases() - 1);
                GamePhase::BeginCombat
            } else {
                GamePhase::EndStep
            }
        }
        GamePhase::EndStep => GamePhase::Cleanup,
        GamePhase::Cleanup => unreachable!(),
    };

    enter_phase(state, next_phase)
}

fn enter_phase(state: &mut GameState, phase: GamePhase) -> Result<TurnAdvanceOutcome, EngineError> {
    let active = state.active_player();
    state.set_phase(phase);
    state.clear_passed_priority();
    if phase == GamePhase::BeginCombat {
        state.set_combat_phases_this_turn(state.combat_phases_this_turn().saturating_add(1));
    }

    // Expire `UntilYourNextTurn` / `UntilNextTurn` when active player enters Untap (or Main1 on extra? No, active player's turn starts at Untap).
    if phase == GamePhase::Untap {
        expire_until_next_turn_effects_for(state, active);
        prune_stale_prevention_shields(state);
    }

    emit_event(
        state,
        AtomicEventKind::PhaseStepEntered {
            phase,
            active_player: active,
        },
    );

    match phase {
        GamePhase::Untap => execute_untap_step(state),
        GamePhase::Upkeep => {
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            grant_priority_with_sba_and_triggers(state)
        }
        GamePhase::Draw => execute_draw_step(state),
        GamePhase::Main1 => {
            advance_saga_lore_counters(state, active)?;
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            grant_priority_with_sba_and_triggers(state)
        }
        GamePhase::BeginCombat => Ok(enter_combat_phase(state)?.into()),
        GamePhase::DeclareAttackers => Ok(enter_declare_attackers_step(state)?.into()),
        GamePhase::DeclareBlockers => Ok(enter_declare_blockers_step(state)?.into()),
        GamePhase::FirstStrikeDamage => Ok(execute_combat_damage_step(state, true)?.into()),
        GamePhase::CombatDamage => Ok(execute_combat_damage_step(state, false)?.into()),
        GamePhase::EndCombat => {
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            grant_priority_with_sba_and_triggers(state)
        }
        GamePhase::Main2 | GamePhase::EndStep => {
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            grant_priority_with_sba_and_triggers(state)
        }
        GamePhase::Cleanup => run_cleanup_step(state),
    }
}

pub(crate) fn execute_untap_step(state: &mut GameState) -> Result<TurnAdvanceOutcome, EngineError> {
    let active = state.active_player();

    // 1. Day/Night check at beginning of turn (`CR 730.2`).
    if let Some(is_night) = state.day_night()
        && let Some(prev_active) = state.turn_history().previous_turn_active_player
    {
        let prev_spells = state
            .turn_history()
            .spells_cast_last_turn
            .iter()
            .find(|(p, _)| *p == prev_active)
            .map_or(0, |&(_, c)| c);
        if !is_night && prev_spells == 0 {
            emit_event(
                state,
                AtomicEventKind::DayNightChanged {
                    previous_night: Some(false),
                    now_night: true,
                },
            );
        } else if is_night && prev_spells >= 2 {
            emit_event(
                state,
                AtomicEventKind::DayNightChanged {
                    previous_night: Some(true),
                    now_night: false,
                },
            );
        }
    }

    // 2. CR 502.1 + CR 702.26a: Simultaneous phasing.
    let active_bf = state
        .player(active)
        .map(|p| p.battlefield.clone())
        .unwrap_or_default();
    let to_phase_out: SmallVec<[ObjectId; 4]> = active_bf
        .iter()
        .copied()
        .filter(|&id| {
            state.object(id).is_some_and(|o| !o.is_phased_out()) && has_phasing(state, id)
        })
        .collect();
    let _phased_in = phase_in_untap_step(state, active);
    for id in to_phase_out {
        let _ = phase_out_permanent(state, id);
    }

    // 3. Clear summoning sickness / entered-this-turn / loyalty activations on active player's permanents.
    let bf_after_phase = state
        .player(active)
        .map(|p| p.battlefield.clone())
        .unwrap_or_default();
    for id in bf_after_phase {
        if let Some(obj) = state.object_mut(id) {
            obj.flags
                .remove(ObjectFlags::SUMMONING_SICK | ObjectFlags::ENTERED_THIS_TURN);
        }
        if let Some(rare) = state.rare_mut(id) {
            rare.used_loyalty_this_turn = false;
        }
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::PLAYER_STATE);

    // 4. Collect all tapped permanents eligible to untap: active player's + `UntapDuringOtherUntapSteps`.
    let (mandatory_untaps, optional_untaps) = {
        let cache = state.evaluate_board();
        collect_untap_step_candidates(state, cache, active)
    };
    let bf_for_clear = state
        .player(active)
        .map(|p| p.battlefield.clone())
        .unwrap_or_default();
    for id in bf_for_clear {
        if let Some(rare) = state.rare_mut(id)
            && rare.doesnt_untap_next_untap
        {
            rare.doesnt_untap_next_untap = false;
            state.remove_rare_if_default(id);
        }
    }

    // Apply mandatory untaps (handling Stun counters `CR 122.1d` and `resolve_untap`).
    for id in mandatory_untaps {
        match untap_single_permanent(state, id, Some(active))? {
            UntapSingleResult::Paused { prompt_id, chooser } => {
                state.set_pending_replacement_resume(Some(ReplacementResumeHook::TurnStep(
                    TurnReplacementContext::UntapStep,
                )));
                return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
            }
            UntapSingleResult::Done => {}
        }
    }

    if !optional_untaps.is_empty() {
        return prompt_or_finish_optional_untaps(state, active, optional_untaps, 0);
    }
    finish_untap_to_upkeep(state)
}

fn finish_untap_to_upkeep(state: &mut GameState) -> Result<TurnAdvanceOutcome, EngineError> {
    // CR 502.4: No priority during untap step; transition directly to Upkeep.
    enter_phase(state, GamePhase::Upkeep)
}

fn prompt_or_finish_optional_untaps(
    state: &mut GameState,
    active: PlayerId,
    candidates: Vec<ObjectId>,
    idx: usize,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if idx >= candidates.len() {
        return finish_untap_to_upkeep(state);
    }
    let prompt_id = state.allocate_prompt_id();
    let req = ChoiceRequest::YesNo {
        prompt_id,
        label: "Untap this permanent during your untap step?".into(),
    };
    state.set_decision(DecisionState::Prompt {
        request: req.clone(),
    });
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: active,
        request: req,
        owner: PromptOwner::Turn(TurnPromptKind::UntapOptional {
            candidates,
            next_idx: idx,
        }),
    }));
    Ok(TurnAdvanceOutcome::Paused {
        prompt_id,
        chooser: active,
    })
}

enum UntapSingleResult {
    Done,
    Paused { prompt_id: u32, chooser: PlayerId },
}

fn untap_single_permanent(
    state: &mut GameState,
    id: ObjectId,
    actor: Option<PlayerId>,
) -> Result<UntapSingleResult, EngineError> {
    let Some(obj) = state.object(id) else {
        return Ok(UntapSingleResult::Done);
    };
    if obj.zone != Zone::Battlefield
        || obj.is_phased_out()
        || !obj.flags.contains(ObjectFlags::TAPPED)
    {
        return Ok(UntapSingleResult::Done);
    }
    // CR 122.1d: If permanent has a stun counter, remove one stun counter instead of untapping.
    if let Some(stun_cid) = state.db().counter_id("stun") {
        let stun_cnt = state.counter_count(id, stun_cid);
        if stun_cnt > 0 {
            state.set_counter(id, stun_cid, stun_cnt - 1);
            emit_event(
                state,
                AtomicEventKind::CounterChanged {
                    object: id,
                    counter: stun_cid,
                    previous_count: stun_cnt,
                    new_count: stun_cnt - 1,
                    actor,
                },
            );
            return Ok(UntapSingleResult::Done);
        }
    }
    match resolve_untap(state, id)? {
        ReplacementResult::Paused { prompt_id, chooser } => {
            Ok(UntapSingleResult::Paused { prompt_id, chooser })
        }
        ReplacementResult::Resolved(outcome) => {
            if !outcome.mods.skipped
                && let Some(obj_mut) = state.object_mut(id)
                && obj_mut.flags.contains(ObjectFlags::TAPPED)
            {
                obj_mut.flags.remove(ObjectFlags::TAPPED);
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object: id,
                        now_tapped: false,
                        actor,
                    },
                );
            }
            Ok(UntapSingleResult::Done)
        }
    }
}

fn collect_untap_step_candidates(
    state: &GameState,
    cache: &LayerCache,
    active: PlayerId,
) -> (Vec<ObjectId>, Vec<ObjectId>) {
    let mut mandatory = Vec::new();
    let mut optional = Vec::new();

    // Check UntapStepCap (e.g. Winter Orb) on active player's RuleMods.
    type UntapCapEntry = (Option<ObjectId>, PlayerId, Option<ObjectFilter>, u16);
    let mut caps: SmallVec<[UntapCapEntry; 2]> = SmallVec::new();
    if let Some(pe) = cache.player(active) {
        for rm in &pe.active_rule_mods {
            if let RuleMod::UntapStepCap { filter, max } = &rm.rule {
                caps.push((rm.source, rm.controller, filter.as_deref().cloned(), *max));
            }
        }
    }

    let mut capped_remaining: SmallVec<[u16; 2]> = caps.iter().map(|c| c.3).collect();

    for p in state.players() {
        if p.lost {
            continue;
        }
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() || !obj.flags.contains(ObjectFlags::TAPPED) {
                continue;
            }
            let eval_ctrl = cache.get(id).map_or(obj.controller, |e| e.controller);
            let is_active_ctrl = eval_ctrl == active;
            let untaps_other =
                !is_active_ctrl && has_untap_during_other_untap_steps(state, cache, id);
            if !is_active_ctrl && !untaps_other {
                continue;
            }

            if state.rare(id).is_some_and(|r| r.doesnt_untap_next_untap) {
                continue;
            }
            if has_doesnt_untap_static(state, cache, id) {
                continue;
            }

            // Respect UntapStepCap limits.
            let mut blocked_by_cap = false;
            for (ci, (src_opt, ctrl, flt_opt, _)) in caps.iter().enumerate() {
                let matches_cap = match (flt_opt, src_opt) {
                    (Some(f), Some(src)) => {
                        let ctx = EvalContext::for_source(*ctrl, *src)
                            .with_board(BoardView::Cached(cache));
                        matches_object_filter(state, id, f, &ctx)
                    }
                    (Some(f), None) => {
                        let ctx = EvalContext::new(*ctrl).with_board(BoardView::Cached(cache));
                        matches_object_filter(state, id, f, &ctx)
                    }
                    (None, _) => true,
                };
                if matches_cap {
                    if capped_remaining[ci] == 0 {
                        blocked_by_cap = true;
                        break;
                    }
                    capped_remaining[ci] -= 1;
                }
            }
            if blocked_by_cap {
                continue;
            }

            if has_may_choose_not_to_untap(state, cache, id) {
                optional.push(id);
            } else {
                mandatory.push(id);
            }
        }
    }

    (mandatory, optional)
}

pub(crate) fn execute_draw_step(state: &mut GameState) -> Result<TurnAdvanceOutcome, EngineError> {
    let active = state.active_player();
    // Clear `doesnt_untap_next_untap`? It already prevented untap in untap step; clear once expired on that player's untap! Actually clear in untap? Wait: let's clear it in untap if already skipped; here draw step checks:
    let skip_first_turn_2p =
        state.turn_number() == 1 && active == PlayerId(0) && state.players().len() == 2;
    let skip_once = state.player(active).is_some_and(|p| p.skip_draw_step_once);
    if skip_once && let Some(p) = state.player_mut(active) {
        p.skip_draw_step_once = false;
    }
    let skip_static = state
        .evaluate_board()
        .player(active)
        .is_some_and(|pe| pe.skip_draw_step);

    if skip_first_turn_2p || skip_once || skip_static {
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
        return grant_priority_with_sba_and_triggers(state);
    }

    continue_draw_step_loop(state, active, 1)
}

fn continue_draw_step_loop(
    state: &mut GameState,
    active: PlayerId,
    remaining_draws: u32,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if remaining_draws == 0 {
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
        return grant_priority_with_sba_and_triggers(state);
    }
    let cant_draw = state
        .evaluate_board()
        .player(active)
        .is_some_and(|pe| pe.cant_draw_cards);
    if !cant_draw {
        match resolve_draw(state, active, 1, None)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                state.set_pending_replacement_resume(Some(ReplacementResumeHook::TurnStep(
                    TurnReplacementContext::DrawStep {
                        remaining_draws: remaining_draws - 1,
                    },
                )));
                return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(outcome) => {
                apply_draw_replacement_outcome(state, active, *outcome)?;
            }
        }
    }
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    grant_priority_with_sba_and_triggers(state)
}

fn apply_draw_replacement_outcome(
    state: &mut GameState,
    default_player: PlayerId,
    outcome: ReplacementOutcome,
) -> Result<(), EngineError> {
    if outcome.mods.skipped {
        return Ok(());
    }
    let drawer = outcome.mods.draw_stealer.unwrap_or(default_player);
    let count = match outcome.event {
        ReplaceableEventKind::Draw { count, .. } => count,
        _ => 1,
    };
    for _ in 0..count {
        if let Some(drawn_id) = draw_one_raw(state, drawer)? {
            let in_own = if state.phase() == GamePhase::Draw && state.active_player() == drawer {
                state
                    .player(drawer)
                    .map(|p| p.draw_step_draws_this_turn.saturating_add(1))
            } else {
                None
            };
            emit_event(
                state,
                AtomicEventKind::DrawCard {
                    player: drawer,
                    object: drawn_id,
                    in_own_draw_step_ordinal: in_own,
                },
            );
        } else if let Some(p) = state.player_mut(drawer) {
            // CR 121.4: Attempted to draw from an empty library (SBA checks loss).
            p.attempted_empty_library_draw = true;
            state.mark_dirty(DirtyMask::PLAYER_STATE);
        }
    }
    Ok(())
}

fn advance_saga_lore_counters(state: &mut GameState, active: PlayerId) -> Result<(), EngineError> {
    let Some(lore_cid) = state.db().counter_id("lore") else {
        return Ok(());
    };
    let sagas: SmallVec<[ObjectId; 4]> = state
        .player(active)
        .map(|p| {
            p.battlefield
                .iter()
                .copied()
                .filter(|&id| {
                    state.object(id).is_some_and(|o| {
                        !o.is_phased_out()
                            && is_saga_card(state.db(), o.card_def, o.active_face_index)
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    for saga_id in sagas {
        if let ReplacementResult::Resolved(outcome) =
            resolve_put_counters(state, saga_id, lore_cid, 1, Some(active), Some(saga_id))?
            && !outcome.mods.skipped
        {
            let count = match outcome.event {
                ReplaceableEventKind::PutCounters { count, .. } => count,
                _ => 1,
            };
            if count > 0 {
                let prev = state.counter_count(saga_id, lore_cid);
                let next = prev.saturating_add(count);
                state.set_counter(saga_id, lore_cid, next);
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object: saga_id,
                        counter: lore_cid,
                        previous_count: prev,
                        new_count: next,
                        actor: Some(active),
                    },
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn run_cleanup_step(state: &mut GameState) -> Result<TurnAdvanceOutcome, EngineError> {
    let active = state.active_player();
    let max_hand = effective_max_hand_size(state, active);
    if let Some(max_h) = max_hand
        && let Some(p) = state.player(active)
        && p.hand.len() > max_h as usize
    {
        let excess = (p.hand.len() - max_h as usize) as u16;
        return continue_cleanup_discard_or_finish(state, active, excess);
    }
    finish_cleanup_effects_and_advance_turn(state)
}

fn continue_cleanup_discard_or_finish(
    state: &mut GameState,
    active: PlayerId,
    remaining_excess: u16,
) -> Result<TurnAdvanceOutcome, EngineError> {
    if remaining_excess > 0
        && let Some(p) = state.player(active)
        && !p.hand.is_empty()
    {
        let hand_candidates = p.hand.clone();
        let prompt_id = state.allocate_prompt_id();
        let req = ChoiceRequest::ChooseObjects {
            prompt_id,
            candidates: hand_candidates,
            min: 1,
            max: 1,
        };
        state.set_decision(DecisionState::Prompt {
            request: req.clone(),
        });
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: active,
            request: req,
            owner: PromptOwner::Turn(TurnPromptKind::CleanupDiscard {
                remaining: remaining_excess,
                picked: Vec::new(),
            }),
        }));
        return Ok(TurnAdvanceOutcome::Paused {
            prompt_id,
            chooser: active,
        });
    }
    finish_cleanup_effects_and_advance_turn(state)
}

fn finish_cleanup_effects_and_advance_turn(
    state: &mut GameState,
) -> Result<TurnAdvanceOutcome, EngineError> {
    // CR 514.2: Simultaneously remove all damage marked on permanents and end "until end of turn" effects.
    let all_bf: Vec<ObjectId> = state
        .players()
        .iter()
        .flat_map(|p| p.battlefield.iter().copied())
        .collect();
    for oid in all_bf {
        if let Some(obj) = state.object_mut(oid) {
            obj.damage_marked = 0;
            obj.deathtouch_damage_marked = false;
            obj.flags
                .remove(ObjectFlags::DEALT_DAMAGE_THIS_TURN | ObjectFlags::ENTERED_THIS_TURN);
        }
        if let Some(rare) = state.rare_mut(oid) {
            rare.temp_power_mod = 0;
            rare.temp_toughness_mod = 0;
            rare.granted_keywords.clear();
            rare.switched_pt_count = 0;
            rare.dealt_damage_exile_sources.clear();
            rare.exile_instead_of_dying_this_turn = false;
            state.remove_rare_if_default(oid);
        }
    }

    expire_effects_for_duration(state, Duration::EndOfTurn);
    expire_end_of_next_turn_effects_for(state, state.active_player());
    clear_mana_pools_at_cleanup(state);
    clear_eot_prevention_shields(state);
    clear_turn_trigger_flags_only(state);
    state.clear_eot_prevention_state();
    if state.rare_turn_state().is_some() {
        state
            .rare_turn_state_mut()
            .must_attack_player_this_turn
            .clear();
        state.prune_rare_turn_state_if_empty();
    }
    let _ = end_combat_phase_immediately(state)?;

    for i in 0..state.players().len() {
        if let Some(p) = state.player_mut(PlayerId(i as u8)) {
            p.prevent_all_combat_damage_this_turn = false;
            if p.protection_from_everything == Some(Duration::EndOfTurn) {
                p.protection_from_everything = None;
            }
        }
    }
    state.mark_dirty(DirtyMask::ALL);

    // CR 514.3a: Check SBAs + triggers during Cleanup.
    match check_sbas_with_cleanup(state, true)? {
        SbaOutcome::Done => {}
        SbaOutcome::Paused { prompt_id, chooser } => {
            return Ok(TurnAdvanceOutcome::Paused { prompt_id, chooser });
        }
        SbaOutcome::GameOver { winner } => {
            return Ok(TurnAdvanceOutcome::GameOver { winner });
        }
    }
    finish_cleanup_after_sba(state)
}

pub(crate) fn finish_cleanup_after_sba(
    state: &mut GameState,
) -> Result<TurnAdvanceOutcome, EngineError> {
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    if let Some(paused) = check_paused_or_gameover(state) {
        return Ok(paused);
    }

    if !state.stack().is_empty() {
        return grant_priority_with_sba_and_triggers(state);
    }

    // Advance to the next turn!
    advance_to_next_turn(state)
}

fn advance_to_next_turn(state: &mut GameState) -> Result<TurnAdvanceOutcome, EngineError> {
    let finished_player = state.active_player();
    state
        .turn_history_mut()
        .end_turn_rollover_with_player(finished_player);
    for i in 0..state.players().len() {
        if let Some(p) = state.player_mut(PlayerId(i as u8)) {
            p.clear_turn_counters();
        }
    }
    state.mark_dirty(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);

    state.set_additional_combat_phases(0);
    state.set_additional_main_after_combat(false);
    state.set_combat_phases_this_turn(0);

    // Pick next player: LIFO extra turns (`CR 500.7`) first, else normal turn order from last_normal_turn_player.
    let next_player = loop {
        let extra_pop = state
            .rare_turn_state()
            .and_then(|r| r.extra_turn_queue.last().copied());
        if let Some(ep) = extra_pop {
            if state.rare_turn_state().is_some() {
                state.rare_turn_state_mut().extra_turn_queue.pop();
                state.prune_rare_turn_state_if_empty();
            }
            if state.player(ep).is_some_and(|p| p.lost) {
                continue;
            }
            let skip_extra = state
                .evaluate_board()
                .player(ep)
                .is_some_and(|pe| pe.skip_extra_turns);
            if skip_extra || consume_skip_turn(state, ep) {
                continue;
            }
            break ep;
        } else {
            let base = state.last_normal_turn_player().unwrap_or(finished_player);
            let Some(cand) = next_alive_after(state, base) else {
                let winner = state.players().iter().find(|p| !p.lost).map(|p| p.id);
                state.set_decision(DecisionState::GameOver { winner });
                return Ok(TurnAdvanceOutcome::GameOver { winner });
            };
            state.set_last_normal_turn_player(Some(cand));
            if consume_skip_turn(state, cand) {
                continue;
            }
            break cand;
        }
    };

    state.set_active_player(next_player);
    state.set_turn_number(state.turn_number().saturating_add(1));
    state.set_turn_serial(state.turn_serial().saturating_add(1));
    enter_phase(state, GamePhase::Untap)
}

fn effective_max_hand_size(state: &GameState, player: PlayerId) -> Option<u32> {
    if state
        .player(player)
        .is_some_and(|p| p.no_max_hand_size_rest_of_game)
    {
        return None;
    }
    let base = u16::from(state.config().max_hand_size);
    state
        .evaluate_board()
        .player(player)
        .map_or(Some(u32::from(base)), |pe| pe.effective_max_hand_size(base))
}

fn should_skip_next_combat(state: &mut GameState, player: PlayerId) -> bool {
    let mut skip = false;
    if state.rare_turn_state().is_some() {
        let rt = state.rare_turn_state_mut();
        if let Some(pos) = rt
            .skip_next_combat_phases
            .iter()
            .position(|(p, _)| *p == player)
        {
            if rt.skip_next_combat_phases[pos].1 > 0 {
                rt.skip_next_combat_phases[pos].1 -= 1;
                skip = true;
            }
            if rt.skip_next_combat_phases[pos].1 == 0 {
                rt.skip_next_combat_phases.remove(pos);
            }
        }
        state.prune_rare_turn_state_if_empty();
    }
    skip
}

fn consume_skip_turn(state: &mut GameState, player: PlayerId) -> bool {
    let mut skip = false;
    if state.rare_turn_state().is_some() {
        let rt = state.rare_turn_state_mut();
        if let Some(pos) = rt.skip_next_turns.iter().position(|(p, _)| *p == player) {
            if rt.skip_next_turns[pos].1 > 0 {
                rt.skip_next_turns[pos].1 -= 1;
                skip = true;
            }
            if rt.skip_next_turns[pos].1 == 0 {
                rt.skip_next_turns.remove(pos);
            }
        }
        state.prune_rare_turn_state_if_empty();
    }
    skip
}

fn restore_timed_copiable_backups(
    state: &mut GameState,
    mut should_expire: impl FnMut(&TimedCopiableBackup) -> bool,
) {
    let has_backups = state
        .rare_turn_state()
        .is_some_and(|r| !r.timed_copiable_backups.is_empty());
    if !has_backups {
        return;
    }
    let mut kept = Vec::new();
    let mut expired = Vec::new();
    {
        let rts = state.rare_turn_state_mut();
        for b in rts.timed_copiable_backups.drain(..) {
            if should_expire(&b) {
                expired.push(b);
            } else {
                kept.push(b);
            }
        }
        rts.timed_copiable_backups = kept;
    }
    state.prune_rare_turn_state_if_empty();

    if expired.is_empty() {
        return;
    }
    for b in expired.into_iter().rev() {
        if state
            .object(b.object)
            .is_some_and(|o| o.zone == Zone::Battlefield)
        {
            if b.prev_copiable.is_some() || b.prev_token_spec.is_some() {
                if let Some(rare) = state.rare_or_default(b.object) {
                    rare.copiable = b.prev_copiable;
                    rare.token_spec = b.prev_token_spec;
                }
            } else if let Some(rare) = state.rare_mut(b.object) {
                rare.copiable = None;
                rare.token_spec = None;
                state.remove_rare_if_default(b.object);
            }
        }
    }
    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::CONTINUOUS_EFFECTS);
}

pub(crate) fn expire_source_bound_copiable_backups(state: &mut GameState) {
    let Some(rts) = state.rare_turn_state() else {
        return;
    };
    if rts.timed_copiable_backups.is_empty() {
        return;
    }
    let expired_flags: SmallVec<[bool; 4]> = rts
        .timed_copiable_backups
        .iter()
        .map(|b| match b.duration {
            Duration::UntilSourceLeaves => b
                .source
                .is_none_or(|s| state.object(s).is_none_or(|o| o.zone != Zone::Battlefield)),
            Duration::ForAsLongAsYouControlSelf => b.source.is_none_or(|s| {
                state.object(s).is_none_or(|o| {
                    o.zone != Zone::Battlefield
                        || b.expires_on_player.is_some_and(|c| o.controller != c)
                })
            }),
            _ => false,
        })
        .collect();
    if !expired_flags.iter().any(|&x| x) {
        return;
    }
    let mut idx = 0usize;
    restore_timed_copiable_backups(state, |_| {
        let ex = expired_flags.get(idx).copied().unwrap_or(false);
        idx += 1;
        ex
    });
}

#[inline]
fn is_player_turn_effect_expired(
    expires_on_player: Option<PlayerId>,
    expires_turn: Option<u32>,
    fallback_player: PlayerId,
    active: PlayerId,
    cur_turn: u32,
) -> bool {
    let target_player = expires_on_player.unwrap_or(fallback_player);
    target_player == active && expires_turn.is_none_or(|min_turn| cur_turn >= min_turn)
}

fn expire_effects_for_duration(state: &mut GameState, dur: Duration) {
    let cur_turn = state.turn_serial();
    state.retain_continuous_effects(|e| {
        if e.duration == dur {
            return e.expires_turn.is_some_and(|t| t > cur_turn);
        }
        true
    });
    state.retain_player_continuous_effects(|e| {
        if e.duration == dur {
            return e.expires_turn.is_some_and(|t| t > cur_turn);
        }
        true
    });
    restore_timed_copiable_backups(state, |b| {
        b.duration == dur && b.expires_turn.is_none_or(|t| t <= cur_turn)
    });
}

fn expire_end_of_next_turn_effects_for(state: &mut GameState, active: PlayerId) {
    let cur_turn = state.turn_serial();
    state.retain_continuous_effects(|e| {
        if e.duration == Duration::UntilEndOfYourNextTurn {
            return !is_player_turn_effect_expired(
                e.expires_on_player,
                e.expires_turn,
                e.controller,
                active,
                cur_turn,
            );
        }
        true
    });
    state.retain_player_continuous_effects(|e| {
        if e.duration == Duration::UntilEndOfYourNextTurn {
            return !is_player_turn_effect_expired(
                e.expires_on_player,
                e.expires_turn,
                e.controller,
                active,
                cur_turn,
            );
        }
        true
    });
    restore_timed_copiable_backups(state, |b| {
        if b.duration == Duration::UntilEndOfYourNextTurn {
            return b.expires_on_player == Some(active)
                && b.expires_turn.is_none_or(|min_t| cur_turn >= min_t);
        }
        false
    });
    if let Some(p) = state.player_mut(active)
        && p.protection_from_everything == Some(Duration::UntilEndOfYourNextTurn)
    {
        p.protection_from_everything = None;
        state.mark_dirty(DirtyMask::PLAYER_STATE);
    }
}

fn expire_until_next_turn_effects_for(state: &mut GameState, active: PlayerId) {
    let cur_turn = state.turn_serial();
    state.retain_continuous_effects(|e| {
        if e.duration == Duration::UntilYourNextTurn {
            return !is_player_turn_effect_expired(
                e.expires_on_player,
                e.expires_turn,
                e.controller,
                active,
                cur_turn,
            );
        }
        true
    });
    state.retain_player_continuous_effects(|e| {
        if e.duration == Duration::UntilYourNextTurn {
            return !is_player_turn_effect_expired(
                e.expires_on_player,
                e.expires_turn,
                e.controller,
                active,
                cur_turn,
            );
        }
        true
    });
    restore_timed_copiable_backups(state, |b| {
        if b.duration == Duration::UntilYourNextTurn {
            return b.expires_on_player == Some(active)
                && b.expires_turn.is_none_or(|min_t| cur_turn >= min_t);
        }
        false
    });
    if let Some(p) = state.player_mut(active)
        && p.protection_from_everything == Some(Duration::UntilYourNextTurn)
    {
        p.protection_from_everything = None;
        state.mark_dirty(DirtyMask::PLAYER_STATE);
    }
}

fn check_paused_or_gameover(state: &GameState) -> Option<TurnAdvanceOutcome> {
    if let DecisionState::GameOver { winner } = state.decision() {
        return Some(TurnAdvanceOutcome::GameOver { winner: *winner });
    }
    if let Some(pr) = state.pending_replacement() {
        return Some(TurnAdvanceOutcome::Paused {
            prompt_id: pr.prompt_id,
            chooser: pr.chooser,
        });
    }
    if let Some(to) = state.pending_trigger_order() {
        return Some(TurnAdvanceOutcome::Paused {
            prompt_id: to.prompt_id,
            chooser: to.controller,
        });
    }
    if let Some(pp) = state.pending_prompt() {
        return Some(TurnAdvanceOutcome::Paused {
            prompt_id: pp.prompt_id,
            chooser: pp.chooser,
        });
    }
    if state.casting().is_some() {
        return Some(TurnAdvanceOutcome::Continued);
    }
    None
}

fn alive_player_mask(state: &GameState) -> u8 {
    let mut mask = 0u8;
    for p in state.players() {
        if !p.lost && p.id.0 < 8 {
            mask |= 1u8 << p.id.0;
        }
    }
    mask
}

fn first_alive_from(state: &GameState, start: PlayerId) -> Option<PlayerId> {
    let n = state.players().len();
    for offset in 0..n {
        let pid = PlayerId(((usize::from(start.0) + offset) % n) as u8);
        if state.player(pid).is_some_and(|p| !p.lost) {
            return Some(pid);
        }
    }
    None
}

fn next_alive_after(state: &GameState, after: PlayerId) -> Option<PlayerId> {
    let n = state.players().len();
    for offset in 1..=n {
        let pid = PlayerId(((usize::from(after.0) + offset) % n) as u8);
        if state.player(pid).is_some_and(|p| !p.lost) {
            return Some(pid);
        }
    }
    None
}

fn object_has_pregame_begin_on_battlefield(state: &GameState, oid: ObjectId) -> bool {
    let Some(obj) = state.object(oid) else {
        return false;
    };
    let Some(card) = state.db().get(obj.card_def) else {
        return false;
    };
    let Some(face) = card.faces.get(usize::from(obj.active_face_index)) else {
        return false;
    };
    face.abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Static(st) if matches!(st.static_def.body, StaticBody::PregameBeginOnBattlefield)
        )
    })
}

fn has_doesnt_untap_static(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    let mut yes = false;
    for_each_turn_static(state, cache, |src_id, src_ctrl, st| {
        if yes {
            return;
        }
        if let StaticBody::Restriction {
            affected,
            restriction: mtg_ir::RestrictionKind::DoesntUntap,
        } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            yes = true;
        }
    });
    yes
}

fn has_may_choose_not_to_untap(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    if has_keyword(state, id, "doesnt untap unless chosen") {
        return true;
    }
    let mut yes = false;
    for_each_turn_static(state, cache, |src_id, src_ctrl, st| {
        if yes {
            return;
        }
        if let StaticBody::MayChooseNotToUntap { affected } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            yes = true;
        }
    });
    yes
}

fn has_untap_during_other_untap_steps(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    let mut yes = false;
    for_each_turn_static(state, cache, |src_id, src_ctrl, st| {
        if yes {
            return;
        }
        if let StaticBody::UntapDuringOtherUntapSteps { affected } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            yes = true;
        }
    });
    yes
}

fn affected_matches(
    state: &GameState,
    cache: &LayerCache,
    src_id: ObjectId,
    src_ctrl: PlayerId,
    affected: &Affected,
    target_id: ObjectId,
) -> bool {
    match affected {
        Affected::SelfObj => src_id == target_id,
        Affected::AttachedTo => state
            .object(src_id)
            .and_then(|s| s.attached_to)
            .is_some_and(|a| a == target_id),
        Affected::Filter { filter } => {
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            matches_object_filter(state, target_id, filter, &ctx)
        }
    }
}

fn for_each_turn_static<'a>(
    state: &'a GameState,
    cache: &'a LayerCache,
    mut visit: impl FnMut(ObjectId, PlayerId, &'a mtg_ir::StaticDef),
) {
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &perm_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(obj) = state.object(perm_id) else {
                continue;
            };
            if obj.zone == Zone::Battlefield && obj.is_phased_out() {
                continue;
            }
            if obj.zone == Zone::Command && !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            let eval = cache.get(perm_id);
            let extras = cache.extra(perm_id);
            let lost_innate = eval.is_some_and(|e| e.lost_all_abilities)
                || extras.is_some_and(|ex| ex.innate_abilities_removed);
            let ctrl = eval.map_or(obj.controller, |e| e.controller);
            let ctx = EvalContext::for_source(ctrl, perm_id).with_board(BoardView::Cached(cache));

            if !lost_innate
                && !obj.is_face_down()
                && let Some(opts) = face_opts_for_object(state, perm_id)
                && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
            {
                for_each_active_ability_with_card_on_faces(
                    state.db(),
                    obj.card_def,
                    &opts,
                    &view.active_face_indices,
                    |cond| evaluate_condition(state, cond, &ctx),
                    |ab| {
                        if let AbilityDef::Static(st) = ab.ability
                            && st
                                .static_def
                                .condition
                                .as_deref()
                                .is_none_or(|c| evaluate_condition(state, c, &ctx))
                        {
                            visit(perm_id, ctrl, &st.static_def);
                        }
                    },
                );
            }
            if let Some(ex) = extras {
                for granted in &ex.granted_abilities {
                    match granted {
                        GrantedAbilityEntry::Inline {
                            ability,
                            grant_controller,
                            ..
                        } => {
                            if let AbilityDef::Static(st) = ability.as_ref()
                                && st
                                    .static_def
                                    .condition
                                    .as_deref()
                                    .is_none_or(|c| evaluate_condition(state, c, &ctx))
                            {
                                visit(perm_id, *grant_controller, &st.static_def);
                            }
                        }
                        GrantedAbilityEntry::CardAbility {
                            card_def,
                            face_index,
                            ability_index,
                            grant_controller,
                            ..
                        } => {
                            if let Some(card) = state.db().get(*card_def)
                                && let Some(face) = card.faces.get(usize::from(*face_index))
                                && let Some(AbilityDef::Static(st)) =
                                    face.abilities.get(*ability_index)
                                && st
                                    .static_def
                                    .condition
                                    .as_deref()
                                    .is_none_or(|c| evaluate_condition(state, c, &ctx))
                            {
                                visit(perm_id, *grant_controller, &st.static_def);
                            }
                        }
                        GrantedAbilityEntry::IntrinsicBasicMana { .. }
                        | GrantedAbilityEntry::DynamicWard { .. } => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::{GameConfig, MulliganRule};
    use crate::zones::CreateTokenOptions;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, SupertypeMask, TokenSpec,
    };
    use std::sync::Arc;

    fn synth_turn_db() -> Arc<CardDatabase> {
        let mut tables = InternTables::default();
        tables.counters.push("stun".into());
        tables.counters.push("lore".into());
        let plains = CardDef {
            oracle_id: "synth_turn_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Turn Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Turn Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::BASIC,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn synth_state(player_count: u8, free_first: bool) -> GameState {
        let db = synth_turn_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = player_count;
        cfg.mulligan = MulliganRule::London { free_first };
        let decks: Vec<DeckSpec> = (0..player_count).map(|_| deck.clone()).collect();
        GameState::new(db, cfg, &decks, 7).unwrap()
    }

    #[test]
    fn synth_london_mulligan_bottom_and_pass_priority_preserves_cache() {
        let mut state = synth_state(2, true);
        begin_opening_mulligans(&mut state).unwrap();
        assert_eq!(state.decision(), &DecisionState::Mulligan);
        assert_eq!(state.priority_player(), PlayerId(0));

        // P0 mulligans once (free first) and once again (needs 1 bottom), then keeps -> P1 decides.
        assert!(!apply_mulligan_action(&mut state, PlayerId(0), &Action::TakeMulligan).unwrap());
        assert!(!apply_mulligan_action(&mut state, PlayerId(0), &Action::TakeMulligan).unwrap());
        assert!(!apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap());
        assert_eq!(state.mulligan_bottom_remaining(PlayerId(0)), 1);
        assert_eq!(state.priority_player(), PlayerId(1));

        // P1 keeps without mulliganing -> Stage 2 returns to P0 for bottoming.
        assert!(!apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap());
        assert_eq!(state.priority_player(), PlayerId(0));

        let bottom_card = state.player(PlayerId(0)).unwrap().hand[0];
        assert!(
            apply_mulligan_action(
                &mut state,
                PlayerId(0),
                &Action::BottomCard {
                    object: bottom_card,
                },
            )
            .unwrap()
        );
        assert_eq!(state.mulligan_bottom_remaining(PlayerId(0)), 0);
        assert_eq!(state.player(PlayerId(0)).unwrap().hand.len(), 6);
        assert_eq!(state.phase(), GamePhase::Main1);
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.active_player(), PlayerId(0));
        assert_eq!(state.priority_player(), PlayerId(0));

        // Verify pass_priority within the same step does NOT invalidate LayerCache (`t1_f17_05`).
        let rev_before = state.evaluate_board().rev;
        pass_priority(&mut state, PlayerId(0)).unwrap();
        assert_eq!(state.priority_player(), PlayerId(1));
        assert_eq!(state.evaluate_board().rev, rev_before);

        // Pass through rest of P0 Turn 1 -> P1 Turn 2 Draw draws 8th card, Cleanup prompts discard.
        while state.turn_number() == 1 || state.phase() != GamePhase::Draw {
            let p = state.priority_player();
            match state.decision() {
                DecisionState::Priority => {
                    pass_priority(&mut state, p).unwrap();
                }
                DecisionState::DeclareAttackers => {
                    crate::combat::apply_combat_action(&mut state, p, &Action::ConfirmAttackers)
                        .unwrap();
                }
                _ => break,
            }
        }
        assert_eq!(state.turn_number(), 2);
        assert_eq!(state.active_player(), PlayerId(1));
        assert_eq!(state.player(PlayerId(1)).unwrap().hand.len(), 8);

        // Pass to Cleanup -> triggers CleanupDiscard prompt for P1 to discard down to 7.
        while state.turn_number() == 2 && !matches!(state.decision(), DecisionState::Prompt { .. })
        {
            let p = state.priority_player();
            match state.decision() {
                DecisionState::Priority => {
                    pass_priority(&mut state, p).unwrap();
                }
                DecisionState::DeclareAttackers => {
                    crate::combat::apply_combat_action(&mut state, p, &Action::ConfirmAttackers)
                        .unwrap();
                }
                _ => break,
            }
        }
        let pid = state
            .pending_prompt()
            .expect("cleanup discard prompt")
            .prompt_id;
        let discard_id = state.player(PlayerId(1)).unwrap().hand[0];
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(1),
            pid,
            ChoiceAnswer::PickObject { id: discard_id },
        )
        .unwrap();
        assert_eq!(state.player(PlayerId(1)).unwrap().hand.len(), 7);
        assert_eq!(state.turn_number(), 3);
        assert_eq!(state.active_player(), PlayerId(0));
    }

    #[test]
    fn synth_extra_turn_lifo_stun_counter_and_snapshot_roundtrip() {
        let mut state = synth_state(2, true);
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

        // LIFO extra turn queue: push P0 then P1 -> P1 goes first!
        state
            .rare_turn_state_mut()
            .extra_turn_queue
            .push(PlayerId(0));
        state
            .rare_turn_state_mut()
            .extra_turn_queue
            .push(PlayerId(1));

        // Put tapped token with stun counter on P1's battlefield.
        let tok = crate::zones::create_token_raw(
            &mut state,
            PlayerId(1),
            TokenSpec {
                name: "Synth Golem".into(),
                power: 2,
                toughness: 2,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: SmallVec::new(),
                tapped: true,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions {
                tapped: true,
                ..Default::default()
            },
        )
        .unwrap();
        let stun_cid = state.db().counter_id("stun").unwrap();
        state.set_counter(tok, stun_cid, 1);

        // Snapshot round-trip preserves M5b fields cleanly.
        let snap = state.to_snapshot();
        let mut restored = GameState::from_snapshot(synth_turn_db(), snap);
        assert_eq!(restored.turn_serial(), state.turn_serial());

        // End Turn 1 immediately -> advances into Turn 2 (P1's extra turn!), removes stun counter instead of untapping.
        end_the_turn_immediately(&mut restored, PlayerId(0)).unwrap();
        assert_eq!(restored.active_player(), PlayerId(1));
        assert_eq!(restored.counter_count(tok, stun_cid), 0);
        assert!(
            restored
                .object(tok)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn synth_multiplayer_concede_cleans_up_permanents_and_advances_priority() {
        let mut state = synth_state(3, true);
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(2), &Action::KeepHand).unwrap();

        let _tok = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            TokenSpec {
                name: "Synth Soldier".into(),
                power: 1,
                toughness: 1,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();
        assert_eq!(state.player(PlayerId(0)).unwrap().battlefield.len(), 1);
        assert_eq!(state.priority_player(), PlayerId(0));

        // P0 concedes while holding priority in a 3-player game -> permanents leave, game continues on P1!
        let out = apply_concede(&mut state, PlayerId(0)).unwrap();
        assert_eq!(out, TurnAdvanceOutcome::Continued);
        assert!(state.player(PlayerId(0)).unwrap().lost);
        assert!(state.player(PlayerId(0)).unwrap().battlefield.is_empty());
        assert_eq!(state.priority_player(), PlayerId(1));
        assert!(!matches!(state.decision(), DecisionState::GameOver { .. }));
    }

    #[test]
    fn synth_draw_step_ordinal_first_vs_second_draw() {
        let mut tables = InternTables::default();
        tables.counters.push("stun".into());
        tables.counters.push("lore".into());
        let plains = CardDef {
            oracle_id: "synth_draw_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Draw Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Draw Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::BASIC,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let extra_draw_watcher = CardDef {
            oracle_id: "synth_draw_watcher".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Draw Watcher".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Draw Watcher".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::DrawCard,
                        filter: mtg_ir::EventFilter {
                            player: Some(mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            }),
                            detail: Some(mtg_ir::EventDetail::Draw {
                                except_first_in_draw_step: true,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![mtg_ir::EffectStep {
                        op: mtg_ir::EffectOp::GainLife {
                            set_life_total: false,
                        },
                        subject: mtg_ir::Selector::PlayerRef {
                            player: mtg_ir::PlayerRef::You,
                        },
                        actor: None,
                        amount: Some(Box::new(mtg_ir::Amount::Const { n: 2 })),
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Default::default(),
                    }],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains, extra_draw_watcher],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 61).unwrap();
        let watcher_h = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(watcher_h).unwrap().card_def = CardDefId(1);
        move_object(
            &mut state,
            watcher_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        state.set_phase(GamePhase::Draw);
        state.set_active_player(PlayerId(0));
        state.set_turn_number(2);
        execute_draw_step(&mut state).unwrap();

        assert_eq!(
            state.player(PlayerId(0)).unwrap().draw_step_draws_this_turn,
            1
        );
        assert!(
            state.stack().is_empty(),
            "1st draw in draw step must not trigger except_first"
        );
        assert!(state.event_log().iter().any(|ev| matches!(
            ev.kind,
            AtomicEventKind::DrawCard {
                player: PlayerId(0),
                in_own_draw_step_ordinal: Some(1),
                ..
            }
        )));

        // 2nd draw during own draw step emits Some(2) and triggers except_first_in_draw_step!
        execute_draw_step(&mut state).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().draw_step_draws_this_turn,
            2
        );
        assert_eq!(state.stack().len(), 1);
    }

    #[test]
    fn synth_additional_combat_with_additional_main_after_combat() {
        let mut state = synth_state(2, true);
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

        // Enter BeginCombat -> DeclareAttackers (0 attackers) with 1 additional combat + additional main after combat.
        state.set_additional_combat_phases(1);
        state.set_additional_main_after_combat(true);

        // Pass Main1 -> BeginCombat -> DeclareAttackers
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::BeginCombat);
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::DeclareAttackers);
        crate::combat::apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers)
            .unwrap();

        // Passing DeclareAttackers with 0 attackers enters extra Main2 (preserving additional_combat_phases == 1).
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::Main2);
        assert_eq!(state.additional_combat_phases(), 1);

        // Passing Main2 enters the extra BeginCombat!
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::BeginCombat);
        assert_eq!(state.additional_combat_phases(), 0);

        // Completing extra combat enters Main2 then EndStep.
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        crate::combat::apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers)
            .unwrap();
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::Main2);
        pass_priority(&mut state, PlayerId(0)).unwrap();
        pass_priority(&mut state, PlayerId(1)).unwrap();
        assert_eq!(state.phase(), GamePhase::EndStep);
    }

    #[test]
    fn synth_day_night_untap_checks_previous_turn_active_player() {
        let mut state = synth_state(2, true);
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

        // Turn 1 (P0): starts as Day (false), P0 casts 0 spells.
        emit_event(
            &mut state,
            AtomicEventKind::DayNightChanged {
                previous_night: None,
                now_night: false,
            },
        );
        assert_eq!(state.day_night(), Some(false));

        // Advance through P0 Turn 1 -> P1 Turn 2 Untap transitions Day -> Night.
        while state.turn_number() == 1 {
            let p = state.priority_player();
            match state.decision() {
                DecisionState::Priority => {
                    pass_priority(&mut state, p).unwrap();
                }
                DecisionState::DeclareAttackers => {
                    crate::combat::apply_combat_action(&mut state, p, &Action::ConfirmAttackers)
                        .unwrap();
                }
                _ => break,
            }
        }
        assert_eq!(state.turn_number(), 2);
        assert_eq!(state.active_player(), PlayerId(1));
        assert_eq!(state.day_night(), Some(true));

        // Turn 2 (P1): P1 casts 2 spells -> P0 Turn 3 Untap transitions Night -> Day.
        let dummy_card = state.player(PlayerId(1)).unwrap().hand[0];
        for _ in 0..2 {
            emit_event(
                &mut state,
                AtomicEventKind::Cast {
                    player: PlayerId(1),
                    object: dummy_card,
                    stack_item: crate::StackItemId(100),
                    is_copy: false,
                    targets: vec![],
                    mana_spent: 0,
                    colored_mana_spent: 0,
                    kicked: false,
                    from_zone: Zone::Hand,
                },
            );
        }
        while state.turn_number() == 2 {
            let p = state.priority_player();
            match state.decision() {
                DecisionState::Priority => {
                    pass_priority(&mut state, p).unwrap();
                }
                DecisionState::DeclareAttackers => {
                    crate::combat::apply_combat_action(&mut state, p, &Action::ConfirmAttackers)
                        .unwrap();
                }
                DecisionState::Prompt { .. } => {
                    let pp = state.pending_prompt().unwrap();
                    let pid = pp.prompt_id;
                    let chooser = pp.chooser;
                    let pick = match &pp.request {
                        ChoiceRequest::ChooseObjects { candidates, .. } => candidates[0],
                        _ => break,
                    };
                    crate::choices::apply_choice_answer(
                        &mut state,
                        chooser,
                        pid,
                        ChoiceAnswer::PickObject { id: pick },
                    )
                    .unwrap();
                }
                _ => break,
            }
        }
        assert_eq!(state.turn_number(), 3);
        assert_eq!(state.active_player(), PlayerId(0));
        assert_eq!(state.day_night(), Some(false));
    }

    #[test]
    fn synth_mulligan_p0_concede_in_3p_advances_active_player() {
        let mut state = synth_state(3, true);
        begin_opening_mulligans(&mut state).unwrap();
        assert_eq!(state.active_player(), PlayerId(0));

        // P0 concedes during Mulligan; P1 and P2 keep -> Turn 1 starts with P1 active.
        apply_concede(&mut state, PlayerId(0)).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();
        assert!(apply_mulligan_action(&mut state, PlayerId(2), &Action::KeepHand).unwrap());
        assert_eq!(state.phase(), GamePhase::Main1);
        assert_eq!(state.active_player(), PlayerId(1));
        assert_eq!(state.last_normal_turn_player(), Some(PlayerId(1)));
        assert_eq!(state.priority_player(), PlayerId(1));
    }

    #[test]
    fn synth_3p_goad_and_become_copy_until_next_turn_and_until_end_of_next_turn() {
        let mut state = synth_state(3, true);
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(2), &Action::KeepHand).unwrap();
        assert_eq!(state.turn_number(), 1);
        assert_eq!(state.turn_serial(), 1);
        assert_eq!(state.active_player(), PlayerId(0));

        // Create Synth Raider (2/2) for P1, Synth Bear (2/2) and Synth Squire (1/1) for P0.
        let mut p1_raider_spec = TokenSpec {
            name: "Synth Raider".into(),
            power: 2,
            toughness: 2,
            colors: ColorMask::R,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: SmallVec::new(),
            type_line: "Creature".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };
        let raider = crate::zones::create_token_raw(
            &mut state,
            PlayerId(1),
            p1_raider_spec.clone(),
            CreateTokenOptions::default(),
        )
        .unwrap();
        state
            .object_mut(raider)
            .unwrap()
            .flags
            .remove(ObjectFlags::SUMMONING_SICK);

        p1_raider_spec.name = "Synth Bear".into();
        let p0_bear = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            p1_raider_spec,
            CreateTokenOptions::default(),
        )
        .unwrap();

        let squire_spec = TokenSpec {
            name: "Synth Squire".into(),
            power: 1,
            toughness: 1,
            colors: ColorMask::W,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: SmallVec::new(),
            type_line: "Creature".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };
        let p0_squire = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            squire_spec,
            CreateTokenOptions::default(),
        )
        .unwrap();

        let goliath_spec = TokenSpec {
            name: "Synth Goliath".into(),
            power: 5,
            toughness: 5,
            colors: ColorMask::G,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: SmallVec::new(),
            type_line: "Creature".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };

        // On P0 Turn 1:
        // (a) P0 goads P1's Raider with UntilYourNextTurn.
        let (exp_uynt, exp_p_uynt) =
            crate::state::compute_timed_expiry(&state, Duration::UntilYourNextTurn, PlayerId(0));
        let ts1 = state.allocate_timestamp();
        let id1 = state.allocate_continuous_effect_id();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: id1,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                raider
            ]),
            modifier: mtg_ir::LayerMod::GrantAbility {
                ability: Box::new(AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        condition: None,
                        body: mtg_ir::StaticBody::CombatRule {
                            affected: mtg_ir::Affected::SelfObj,
                            rule: mtg_ir::CombatRuleSpec::IsGoaded,
                        },
                    },
                    span: Default::default(),
                })),
            },
            duration: Duration::UntilYourNextTurn,
            timestamp: ts1,
            expires_turn: exp_uynt,
            expires_on_player: exp_p_uynt,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        // (b) P0 turns P0's Bear into a copy of Synth Goliath (5/5) UntilYourNextTurn.
        let bear_prev_tok = state
            .rare(p0_bear)
            .and_then(|r| r.token_spec.as_ref().map(Arc::clone));
        state.rare_or_default(p0_bear).unwrap().token_spec = Some(Arc::new(goliath_spec.clone()));
        state.rare_turn_state_mut().timed_copiable_backups.push(
            crate::state::TimedCopiableBackup {
                object: p0_bear,
                source: None,
                prev_copiable: None,
                prev_token_spec: bear_prev_tok,
                duration: Duration::UntilYourNextTurn,
                expires_turn: exp_uynt,
                expires_on_player: exp_p_uynt,
            },
        );

        // (c) P0 turns P0's Squire into a copy of Synth Goliath (5/5) UntilEndOfYourNextTurn.
        let (exp_ueynt, exp_p_ueynt) = crate::state::compute_timed_expiry(
            &state,
            Duration::UntilEndOfYourNextTurn,
            PlayerId(0),
        );
        let squire_prev_tok = state
            .rare(p0_squire)
            .and_then(|r| r.token_spec.as_ref().map(Arc::clone));
        state.rare_or_default(p0_squire).unwrap().token_spec = Some(Arc::new(goliath_spec));
        state.rare_turn_state_mut().timed_copiable_backups.push(
            crate::state::TimedCopiableBackup {
                object: p0_squire,
                source: None,
                prev_copiable: None,
                prev_token_spec: squire_prev_tok,
                duration: Duration::UntilEndOfYourNextTurn,
                expires_turn: exp_ueynt,
                expires_on_player: exp_p_ueynt,
            },
        );

        // (d) EndOfTurn pump +1/+1 on P0's Squire (expires Turn 1 Cleanup).
        let (exp_eot, exp_p_eot) =
            crate::state::compute_timed_expiry(&state, Duration::EndOfTurn, PlayerId(0));
        let ts2 = state.allocate_timestamp();
        let id2 = state.allocate_continuous_effect_id();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: id2,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer7c,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                p0_squire
            ]),
            modifier: mtg_ir::LayerMod::Pt {
                power: Box::new(mtg_ir::Amount::Const { n: 1 }),
                toughness: Box::new(mtg_ir::Amount::Const { n: 1 }),
            },
            duration: Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: exp_eot,
            expires_on_player: exp_p_eot,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::CONTINUOUS_EFFECTS);

        // Squire is 6/6 on Turn 1, Bear is 5/5.
        {
            let c = state.evaluate_board();
            assert_eq!(c.get(p0_squire).unwrap().pt.unwrap().power, 6);
            assert_eq!(c.get(p0_bear).unwrap().pt.unwrap().power, 5);
        }

        let step_to_turn = |s: &mut GameState, target_turn: u32, stop_at_attackers: bool| {
            const MAX_ITERS: usize = 512;
            for _ in 0..MAX_ITERS {
                if s.turn_number() >= target_turn
                    && (!stop_at_attackers || s.decision() == &DecisionState::DeclareAttackers)
                {
                    return;
                }
                let p = s.priority_player();
                match s.decision() {
                    DecisionState::Priority => {
                        pass_priority(s, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(s, p, &Action::ConfirmAttackers)
                            .unwrap();
                    }
                    DecisionState::DeclareBlockers => {
                        crate::combat::apply_combat_action(s, p, &Action::ConfirmBlockers).unwrap();
                    }
                    DecisionState::Prompt { .. } => {
                        let pp = s.pending_prompt().unwrap();
                        let pid = pp.prompt_id;
                        let chooser = pp.chooser;
                        let pick = match &pp.request {
                            ChoiceRequest::ChooseObjects { candidates, .. } => candidates[0],
                            other => panic!("unexpected prompt in step_to_turn: {other:?}"),
                        };
                        crate::choices::apply_choice_answer(
                            s,
                            chooser,
                            pid,
                            ChoiceAnswer::PickObject { id: pick },
                        )
                        .unwrap();
                    }
                    other => panic!(
                        "unexpected decision while stepping to turn {target_turn}: {other:?}"
                    ),
                }
            }
            panic!(
                "step_to_turn exceeded {MAX_ITERS} iterations (at turn {}, phase {:?}, decision {:?})",
                s.turn_number(),
                s.phase(),
                s.decision()
            );
        };

        // Advance to P1 Turn 2 DeclareAttackers:
        // - EndOfTurn expired at Turn 1 Cleanup (Squire is 5/5, not 6/6)
        // - Both UntilYourNextTurn & UntilEndOfYourNextTurn survived!
        step_to_turn(&mut state, 2, true);
        assert_eq!(state.turn_number(), 2);
        assert_eq!(state.active_player(), PlayerId(1));
        assert_eq!(state.decision(), &DecisionState::DeclareAttackers);
        {
            let c = state.evaluate_board();
            assert_eq!(c.get(p0_bear).unwrap().pt.unwrap().power, 5);
            assert_eq!(c.get(p0_squire).unwrap().pt.unwrap().power, 5);
        }
        // Goaded Raider: ConfirmAttackers with 0 attackers rejected, attacking P0 rejected, attacking P2 accepted!
        assert!(
            crate::combat::apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmAttackers)
                .is_err()
        );
        assert!(
            crate::combat::apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignAttacker {
                    attacker: raider,
                    target: crate::action::AttackTarget::Player { id: PlayerId(0) },
                },
            )
            .is_err()
        );
        crate::combat::apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignAttacker {
                attacker: raider,
                target: crate::action::AttackTarget::Player { id: PlayerId(2) },
            },
        )
        .unwrap();
        crate::combat::apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmAttackers)
            .unwrap();

        // Advance to P2 Turn 3: both UntilYourNextTurn and UntilEndOfYourNextTurn are STILL active!
        step_to_turn(&mut state, 3, false);
        assert_eq!(state.turn_number(), 3);
        assert_eq!(state.active_player(), PlayerId(2));
        {
            let c = state.evaluate_board();
            assert_eq!(c.get(p0_bear).unwrap().pt.unwrap().power, 5);
            assert_eq!(c.get(p0_squire).unwrap().pt.unwrap().power, 5);
            assert!(state.continuous_effects().iter().any(|e| e.id == id1));
        }

        // Advance to P0 Turn 4 Main1:
        // - UntilYourNextTurn (IsGoaded + Bear BecomeCopy) expired at P0 Turn 4 Untap (Bear back to 2/2).
        // - UntilEndOfYourNextTurn (Squire BecomeCopy) is STILL active on P0 Turn 4 until Cleanup!
        step_to_turn(&mut state, 4, false);
        assert_eq!(state.turn_number(), 4);
        assert_eq!(state.active_player(), PlayerId(0));
        {
            let c = state.evaluate_board();
            assert_eq!(c.get(p0_bear).unwrap().pt.unwrap().power, 2);
            assert_eq!(c.get(p0_squire).unwrap().pt.unwrap().power, 5);
            assert!(!state.continuous_effects().iter().any(|e| e.id == id1));
        }

        // Advance to P1 Turn 5: UntilEndOfYourNextTurn expired at P0 Turn 4 Cleanup (Squire back to 1/1).
        step_to_turn(&mut state, 5, false);
        assert_eq!(state.turn_number(), 5);
        assert_eq!(
            state
                .evaluate_board()
                .get(p0_squire)
                .unwrap()
                .pt
                .unwrap()
                .power,
            1
        );
    }

    fn synth_gain_life_step(n: i32) -> mtg_ir::EffectStep {
        mtg_ir::EffectStep {
            op: mtg_ir::EffectOp::GainLife {
                set_life_total: false,
            },
            subject: mtg_ir::Selector::PlayerRef {
                player: mtg_ir::PlayerRef::You,
            },
            actor: None,
            amount: Some(Box::new(mtg_ir::Amount::Const { n })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        }
    }

    fn synth_put_card_on_bf(state: &mut GameState, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            crate::state::GameObject::new(def, owner, Zone::Hand, ts, ObjectFlags::empty()),
            None,
        );
        state.player_mut(owner).unwrap().hand.push(oid);
        let moved = move_object_raw(
            state,
            oid,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        moved.new_id.unwrap_or(oid)
    }

    #[test]
    fn synth_cr503_1a_cleanup_discard_flushes_upkeep_trigger_in_upkeep() {
        let mut tables = InternTables::default();
        tables.counters.push("stun".into());
        tables.counters.push("lore".into());

        let plains = CardDef {
            oracle_id: "synth_cr503_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth CR503 Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth CR503 Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::BASIC,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let upkeep_relic = CardDef {
            oracle_id: "synth_cr503_upkeep_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Upkeep Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Upkeep Relic".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::PhaseBegin,
                        filter: mtg_ir::EventFilter {
                            detail: Some(mtg_ir::EventDetail::Phase {
                                phase: Some(GamePhase::Upkeep),
                                any_main_phase: false,
                                enchanted_controller_upkeep: false,
                                enchanted_player_upkeep: false,
                                enchanted_opponent_end_step: false,
                                phase_chosen_player: false,
                                turn_filter: None,
                            }),
                            ..Default::default()
                        },
                        scope: Some(mtg_ir::TriggerScope::ControllerTurn),
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![synth_gain_life_step(3)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let discard_relic = CardDef {
            oracle_id: "synth_cr514_discard_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Discard Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Discard Relic".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::Discard,
                        filter: mtg_ir::EventFilter {
                            player: Some(mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![synth_gain_life_step(1)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains, upkeep_relic, discard_relic],
                decks: vec![],
            })
            .unwrap(),
        );

        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 2;
        cfg.mulligan = MulliganRule::London { free_first: true };

        // Part A: Single CleanupDiscard (8 cards -> discard 1) must immediately flush P0's Upkeep
        // trigger onto the stack in GamePhase::Upkeep (CR 503.1a / CR 603.3), not delay until Draw.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                cfg.clone(),
                &[deck.clone(), deck.clone()],
                7,
            )
            .unwrap();
            begin_opening_mulligans(&mut state).unwrap();
            apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
            apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

            let _p0_upkeep = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(1));

            // Advance through P0 Turn 1 -> P1 Turn 2 CleanupDiscard (P1 draws 8th card in Draw).
            while state.turn_number() == 1
                || !matches!(state.decision(), DecisionState::Prompt { .. })
            {
                let p = state.priority_player();
                match state.decision() {
                    DecisionState::Priority => {
                        pass_priority(&mut state, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(
                            &mut state,
                            p,
                            &Action::ConfirmAttackers,
                        )
                        .unwrap();
                    }
                    other => panic!("unexpected decision in Part A: {other:?}"),
                }
            }
            assert_eq!(state.turn_number(), 2);
            assert_eq!(state.phase(), GamePhase::Cleanup);

            let pp = state.pending_prompt().unwrap().clone();
            assert_eq!(pp.chooser, PlayerId(1));
            let pick = state.player(PlayerId(1)).unwrap().hand[0];
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(1),
                pp.prompt_id,
                ChoiceAnswer::PickObject { id: pick },
            )
            .unwrap();

            // Immediately on entering P0's Turn 3 Upkeep, trigger is flushed and DecisionState is Priority.
            assert_eq!(state.turn_number(), 3);
            assert_eq!(state.active_player(), PlayerId(0));
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack().len(), 1);
            assert!(state.pending_triggers().is_empty());

            // Resolve the trigger inside Upkeep (NOT delayed until Draw).
            pass_priority(&mut state, PlayerId(0)).unwrap();
            pass_priority(&mut state, PlayerId(1)).unwrap();
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert!(state.stack().is_empty());
            assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);
        }

        // Part B: Chained CleanupDiscard (9 cards -> remaining 2 -> 1 -> 0) AND CR 514.3a
        // cleanup discard trigger flushing in Cleanup before advancing to P0's Upkeep.
        {
            let mut state =
                GameState::new(Arc::clone(&db), cfg, &[deck.clone(), deck], 11).unwrap();
            begin_opening_mulligans(&mut state).unwrap();
            apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
            apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

            let _p0_upkeep = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(1));
            let _p1_discard = synth_put_card_on_bf(&mut state, PlayerId(1), CardDefId(2));

            // Advance to P1's Turn 2 Main1, then add 1 extra card to P1's hand (9 cards total).
            while state.turn_number() == 1 || state.phase() != GamePhase::Main1 {
                let p = state.priority_player();
                match state.decision() {
                    DecisionState::Priority => {
                        pass_priority(&mut state, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(
                            &mut state,
                            p,
                            &Action::ConfirmAttackers,
                        )
                        .unwrap();
                    }
                    other => panic!("unexpected decision advancing to P1 Main1: {other:?}"),
                }
            }
            let ts = state.allocate_timestamp();
            let extra = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(1),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(1)).unwrap().hand.push(extra);
            state.mark_dirty(DirtyMask::TOPOLOGY);
            assert_eq!(state.player(PlayerId(1)).unwrap().hand.len(), 9);

            // Advance to P1's CleanupDiscard prompt (remaining = 2).
            while !matches!(state.decision(), DecisionState::Prompt { .. }) {
                let p = state.priority_player();
                match state.decision() {
                    DecisionState::Priority => {
                        pass_priority(&mut state, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(
                            &mut state,
                            p,
                            &Action::ConfirmAttackers,
                        )
                        .unwrap();
                    }
                    other => panic!("unexpected decision advancing to Cleanup: {other:?}"),
                }
            }

            // 1st CleanupDiscard pick -> chains to 2nd CleanupDiscard prompt (remaining = 1).
            let pp1 = state.pending_prompt().unwrap().clone();
            assert!(matches!(
                pp1.owner,
                PromptOwner::Turn(TurnPromptKind::CleanupDiscard { remaining: 2, .. })
            ));
            let pick1 = state.player(PlayerId(1)).unwrap().hand[0];
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(1),
                pp1.prompt_id,
                ChoiceAnswer::PickObject { id: pick1 },
            )
            .unwrap();
            assert!(matches!(state.decision(), DecisionState::Prompt { .. }));
            let pp2 = state.pending_prompt().unwrap().clone();
            assert!(matches!(
                pp2.owner,
                PromptOwner::Turn(TurnPromptKind::CleanupDiscard { remaining: 1, .. })
            ));

            // 2nd CleanupDiscard pick -> CR 514.3a flushes both Discard triggers in Cleanup!
            let pick2 = state.player(PlayerId(1)).unwrap().hand[0];
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(1),
                pp2.prompt_id,
                ChoiceAnswer::PickObject { id: pick2 },
            )
            .unwrap();
            assert_eq!(state.turn_number(), 2);
            assert_eq!(state.phase(), GamePhase::Cleanup);
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack().len(), 2);
            assert!(state.pending_triggers().is_empty());

            // Resolve both Discard triggers inside Cleanup (20 -> 22), then pass priority on empty
            // stack in Cleanup (CR 514.3a) to re-run cleanup and advance to Turn 3 Upkeep.
            for _ in 0..4 {
                let p = state.priority_player();
                pass_priority(&mut state, p).unwrap();
            }
            assert_eq!(state.player(PlayerId(1)).unwrap().life, 22);
            assert_eq!(state.phase(), GamePhase::Cleanup);
            assert!(state.stack().is_empty());

            pass_priority(&mut state, PlayerId(1)).unwrap();
            pass_priority(&mut state, PlayerId(0)).unwrap();

            // Re-opened Cleanup completes with 7 cards -> advances to P0 Turn 3 Upkeep with trigger on stack.
            assert_eq!(state.turn_number(), 3);
            assert_eq!(state.active_player(), PlayerId(0));
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack().len(), 1);
            assert!(state.pending_triggers().is_empty());

            pass_priority(&mut state, PlayerId(0)).unwrap();
            pass_priority(&mut state, PlayerId(1)).unwrap();
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert!(state.stack().is_empty());
            assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);
        }
    }

    #[test]
    fn synth_cr502_4_untap_optional_flushes_untap_and_upkeep_triggers_in_upkeep() {
        let mut tables = InternTables::default();
        tables.counters.push("stun".into());
        tables.counters.push("lore".into());

        let plains = CardDef {
            oracle_id: "synth_cr502_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth CR502 Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth CR502 Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::BASIC,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let optional_untap_watcher = CardDef {
            oracle_id: "synth_cr502_untap_watcher".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Optional Untap Watcher".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Optional Untap Watcher".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![
                    AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::MayChooseNotToUntap {
                                affected: Affected::SelfObj,
                            },
                        },
                        span: Default::default(),
                    }),
                    AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                        trigger: mtg_ir::TriggerDef {
                            event: mtg_ir::EventKind::BecomesUntapped,
                            filter: mtg_ir::EventFilter {
                                object: Some(mtg_ir::TriggerSubject::SelfObj),
                                ..Default::default()
                            },
                            scope: None,
                            from_zones: None,
                            only_once_per_turn: false,
                            do_this_once_per_turn: false,
                            is_state_trigger: false,
                            or: vec![],
                            chapter_numbers: SmallVec::new(),
                            is_chapter_ability: false,
                            spell_delayed: None,
                        },
                        condition: None,
                        grant_condition: None,
                        targets: vec![],
                        effect: vec![synth_gain_life_step(1)],
                        modal: None,
                        text: None,
                        span: Default::default(),
                    }),
                ],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let upkeep_relic = CardDef {
            oracle_id: "synth_cr502_upkeep_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Upkeep Beacon".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Upkeep Beacon".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::PhaseBegin,
                        filter: mtg_ir::EventFilter {
                            detail: Some(mtg_ir::EventDetail::Phase {
                                phase: Some(GamePhase::Upkeep),
                                any_main_phase: false,
                                enchanted_controller_upkeep: false,
                                enchanted_player_upkeep: false,
                                enchanted_opponent_end_step: false,
                                phase_chosen_player: false,
                                turn_filter: None,
                            }),
                            ..Default::default()
                        },
                        scope: Some(mtg_ir::TriggerScope::ControllerTurn),
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![synth_gain_life_step(3)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains, optional_untap_watcher, upkeep_relic],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 2;
        cfg.mulligan = MulliganRule::London { free_first: true };

        let mut state = GameState::new(db, cfg, &[deck.clone(), deck], 17).unwrap();
        begin_opening_mulligans(&mut state).unwrap();
        apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
        apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

        let w1 = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(1));
        let w2 = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(1));
        let _upkeep_relic = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(2));
        state
            .object_mut(w1)
            .unwrap()
            .flags
            .insert(ObjectFlags::TAPPED);
        state
            .object_mut(w2)
            .unwrap()
            .flags
            .insert(ObjectFlags::TAPPED);
        state.mark_dirty(DirtyMask::TAPPED);

        // Advance through P0 Turn 1 and P1 Turn 2 into P0 Turn 3 UntapOptional prompt.
        while state.turn_number() < 3 {
            let p = state.priority_player();
            match state.decision() {
                DecisionState::Priority => {
                    pass_priority(&mut state, p).unwrap();
                }
                DecisionState::DeclareAttackers => {
                    crate::combat::apply_combat_action(&mut state, p, &Action::ConfirmAttackers)
                        .unwrap();
                }
                DecisionState::Prompt { .. } => {
                    let pp = state.pending_prompt().unwrap().clone();
                    let pick = match &pp.request {
                        ChoiceRequest::ChooseObjects { candidates, .. } => candidates[0],
                        other => panic!("unexpected prompt before Turn 3: {other:?}"),
                    };
                    crate::choices::apply_choice_answer(
                        &mut state,
                        pp.chooser,
                        pp.prompt_id,
                        ChoiceAnswer::PickObject { id: pick },
                    )
                    .unwrap();
                }
                other => panic!("unexpected decision advancing to Turn 3: {other:?}"),
            }
        }

        // First UntapOptional prompt (w1): choose Yes (untap w1).
        assert_eq!(state.turn_number(), 3);
        assert_eq!(state.active_player(), PlayerId(0));
        assert_eq!(state.phase(), GamePhase::Untap);
        let pp1 = state.pending_prompt().unwrap().clone();
        assert!(matches!(
            pp1.owner,
            PromptOwner::Turn(TurnPromptKind::UntapOptional { next_idx: 0, .. })
        ));
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pp1.prompt_id,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();

        // Second UntapOptional prompt (w2): choose No (leave w2 tapped).
        assert_eq!(state.phase(), GamePhase::Untap);
        let pp2 = state.pending_prompt().unwrap().clone();
        assert!(matches!(
            pp2.owner,
            PromptOwner::Turn(TurnPromptKind::UntapOptional { next_idx: 1, .. })
        ));
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pp2.prompt_id,
            ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();

        // Immediately upon entering Upkeep (CR 502.4), BOTH untap trigger (w1) and upkeep trigger
        // flush in GamePhase::Upkeep (prompting P0 with OrderObjects per CR 603.3b).
        assert_eq!(state.phase(), GamePhase::Upkeep);
        assert!(state.pending_triggers().is_empty());
        assert!(
            !state
                .object(w1)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert!(
            state
                .object(w2)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );

        let (order_pid, first_id) = match state.decision() {
            DecisionState::Prompt {
                request: ChoiceRequest::OrderObjects { prompt_id, objects },
            } => {
                assert_eq!(objects.len(), 2);
                (*prompt_id, objects[0])
            }
            other => panic!("expected Upkeep OrderObjects trigger prompt, got {other:?}"),
        };
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            order_pid,
            ChoiceAnswer::OrderNext { id: first_id },
        )
        .unwrap();

        assert_eq!(state.phase(), GamePhase::Upkeep);
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.stack().len(), 2);
        assert!(state.pending_triggers().is_empty());

        // Resolve both triggers inside GamePhase::Upkeep (20 + 1 + 3 = 24).
        for _ in 0..4 {
            let p = state.priority_player();
            pass_priority(&mut state, p).unwrap();
        }
        assert_eq!(state.phase(), GamePhase::Upkeep);
        assert!(state.stack().is_empty());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 24);
    }

    #[test]
    fn synth_resume_turn_prompt_preserves_prompt_on_invalid_answers_and_completes_on_retry() {
        let assert_prompt_preserved =
            |state: &GameState, expected_pp: &crate::state::PendingPromptContext| {
                assert!(crate::choices::is_prompt_active(state));
                assert_eq!(state.pending_prompt(), Some(expected_pp));
                assert!(matches!(state.decision(), DecisionState::Prompt { .. }));
                assert!(
                    !crate::choices::legal_choice_actions(state, expected_pp.chooser).is_empty()
                );
            };

        // Case 1: CleanupDiscard — invalid answers preserve prompt + DecisionState::Prompt, valid retry completes.
        {
            let mut state = synth_state(2, true);
            begin_opening_mulligans(&mut state).unwrap();
            apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
            apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

            let bf_obj = synth_put_card_on_bf(&mut state, PlayerId(1), CardDefId(0));
            let p0_hand_card = state.player(PlayerId(0)).unwrap().hand[0];

            while !matches!(
                state.pending_prompt().map(|p| &p.owner),
                Some(PromptOwner::Turn(TurnPromptKind::CleanupDiscard { .. }))
            ) {
                let p = state.priority_player();
                match state.decision() {
                    DecisionState::Priority => {
                        pass_priority(&mut state, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(
                            &mut state,
                            p,
                            &Action::ConfirmAttackers,
                        )
                        .unwrap();
                    }
                    other => panic!("unexpected decision advancing to CleanupDiscard: {other:?}"),
                }
            }

            let pp = state.pending_prompt().unwrap().clone();
            assert_eq!(pp.chooser, PlayerId(1));
            let valid_hand_card = state.player(PlayerId(1)).unwrap().hand[0];

            // Invalid via choices::apply_choice_answer
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::PickObject { id: bf_obj },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::PickObject { id: p0_hand_card },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pp.prompt_id,
                    ChoiceAnswer::PickObject {
                        id: valid_hand_card,
                    },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id + 1,
                    ChoiceAnswer::PickObject {
                        id: valid_hand_card,
                    },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            // Invalid via direct turn::resume_turn_prompt
            assert!(
                resume_turn_prompt(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::DonePicking
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            assert!(
                resume_turn_prompt(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::PickObject { id: bf_obj },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            // Valid retry completes cleanly
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(1),
                pp.prompt_id,
                ChoiceAnswer::PickObject {
                    id: valid_hand_card,
                },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.player(PlayerId(1)).unwrap().hand.len(), 7);
            assert_eq!(state.turn_number(), 3);
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // Case 2: UntapOptional — invalid answers preserve prompt, valid retry untaps and advances.
        {
            let tables = InternTables::default();
            let plains = CardDef {
                oracle_id: "synth_turn_retry_plains".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Turn Retry Plains".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Turn Retry Plains".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::LAND,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::BASIC,
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            };
            let optional_relic = CardDef {
                oracle_id: "synth_turn_retry_optional_relic".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Turn Retry Optional Relic".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Turn Retry Optional Relic".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::ARTIFACT,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::MayChooseNotToUntap {
                                affected: Affected::SelfObj,
                            },
                        },
                        span: Default::default(),
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            };
            let db = Arc::new(
                CardDatabase::from_bundle(IrV2Bundle {
                    ir_version: IR_V2_VERSION,
                    source_compiler_version: 46,
                    tables,
                    cards: vec![plains, optional_relic],
                    decks: vec![],
                })
                .unwrap(),
            );
            let deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 30],
                sideboard: vec![],
            };
            let mut cfg = GameConfig::standard_1v1();
            cfg.mulligan = MulliganRule::London { free_first: true };
            let mut state = GameState::new(db, cfg, &[deck.clone(), deck], 29).unwrap();
            begin_opening_mulligans(&mut state).unwrap();
            apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
            apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

            let w1 = synth_put_card_on_bf(&mut state, PlayerId(0), CardDefId(1));
            state
                .object_mut(w1)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);

            while state.turn_number() < 3 {
                let p = state.priority_player();
                match state.decision() {
                    DecisionState::Priority => {
                        pass_priority(&mut state, p).unwrap();
                    }
                    DecisionState::DeclareAttackers => {
                        crate::combat::apply_combat_action(
                            &mut state,
                            p,
                            &Action::ConfirmAttackers,
                        )
                        .unwrap();
                    }
                    DecisionState::Prompt { .. } => {
                        let pp = state.pending_prompt().unwrap().clone();
                        let pick = match &pp.request {
                            ChoiceRequest::ChooseObjects { candidates, .. } => candidates[0],
                            other => panic!("unexpected prompt before Turn 3: {other:?}"),
                        };
                        crate::choices::apply_choice_answer(
                            &mut state,
                            pp.chooser,
                            pp.prompt_id,
                            ChoiceAnswer::PickObject { id: pick },
                        )
                        .unwrap();
                    }
                    other => panic!("unexpected decision before Turn 3: {other:?}"),
                }
            }

            let pp = state.pending_prompt().unwrap().clone();
            assert_eq!(pp.chooser, PlayerId(0));
            assert!(matches!(
                pp.owner,
                PromptOwner::Turn(TurnPromptKind::UntapOptional { .. })
            ));

            for bad in [
                ChoiceAnswer::PickObject { id: w1 },
                ChoiceAnswer::DonePicking,
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), pp.prompt_id, bad)
                        .is_err()
                );
                assert_prompt_preserved(&state, &pp);
            }
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pp.prompt_id,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pp.prompt_id + 1,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);
            assert!(
                resume_turn_prompt(
                    &mut state,
                    PlayerId(0),
                    pp.prompt_id,
                    ChoiceAnswer::DonePicking
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp);

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pp.prompt_id,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert!(
                !state
                    .object(w1)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // Case 3: EndTurnMay (`false` and `true`) + wrong PromptOwner
        {
            let mut state = synth_state(2, true);
            begin_opening_mulligans(&mut state).unwrap();
            apply_mulligan_action(&mut state, PlayerId(0), &Action::KeepHand).unwrap();
            apply_mulligan_action(&mut state, PlayerId(1), &Action::KeepHand).unwrap();

            let p0_hand_card = state.player(PlayerId(0)).unwrap().hand[0];
            let pid1 = state.allocate_prompt_id();
            let req1 = ChoiceRequest::YesNo {
                prompt_id: pid1,
                label: "End the turn?".into(),
            };
            let pp1 = crate::state::PendingPromptContext {
                prompt_id: pid1,
                chooser: PlayerId(0),
                request: req1.clone(),
                owner: PromptOwner::Turn(TurnPromptKind::EndTurnMay),
            };
            state.set_pending_prompt(Some(pp1.clone()));
            state.set_decision(DecisionState::Prompt { request: req1 });

            for bad in [
                ChoiceAnswer::DonePicking,
                ChoiceAnswer::PickObject { id: p0_hand_card },
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), pid1, bad)
                        .is_err()
                );
                assert_prompt_preserved(&state, &pp1);
            }
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(1),
                    pid1,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp1);
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid1 + 1,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp1);
            assert!(
                resume_turn_prompt(&mut state, PlayerId(0), pid1, ChoiceAnswer::DonePicking)
                    .is_err()
            );
            assert_prompt_preserved(&state, &pp1);

            // Subcase A: retry false -> remains Turn 1 Main1 with Priority
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid1,
                ChoiceAnswer::YesNo { value: false },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.turn_number(), 1);
            assert_eq!(state.phase(), GamePhase::Main1);
            assert_eq!(state.decision(), &DecisionState::Priority);

            // Subcase B: re-install EndTurnMay, send invalid DonePicking, then retry true -> Turn 2
            let pid2 = state.allocate_prompt_id();
            let req2 = ChoiceRequest::YesNo {
                prompt_id: pid2,
                label: "End the turn?".into(),
            };
            let pp2 = crate::state::PendingPromptContext {
                prompt_id: pid2,
                chooser: PlayerId(0),
                request: req2.clone(),
                owner: PromptOwner::Turn(TurnPromptKind::EndTurnMay),
            };
            state.set_pending_prompt(Some(pp2.clone()));
            state.set_decision(DecisionState::Prompt { request: req2 });

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid2,
                    ChoiceAnswer::DonePicking,
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &pp2);

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid2,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.turn_number(), 2);
            assert_eq!(state.active_player(), PlayerId(1));
            assert_eq!(state.phase(), GamePhase::Upkeep);
            assert_eq!(state.decision(), &DecisionState::Priority);

            // Wrong PromptOwner check via direct resume_turn_prompt
            let pid3 = state.allocate_prompt_id();
            let req3 = ChoiceRequest::YesNo {
                prompt_id: pid3,
                label: "Return commander?".into(),
            };
            let other_pp = crate::state::PendingPromptContext {
                prompt_id: pid3,
                chooser: PlayerId(0),
                request: req3.clone(),
                owner: PromptOwner::Sba(crate::state::SbaPromptKind::CommanderReturn {
                    commander: p0_hand_card,
                    owner: PlayerId(0),
                    from_zone: Zone::Graveyard,
                }),
            };
            state.set_pending_prompt(Some(other_pp.clone()));
            state.set_decision(DecisionState::Prompt { request: req3 });

            assert!(
                resume_turn_prompt(
                    &mut state,
                    PlayerId(0),
                    pid3,
                    ChoiceAnswer::YesNo { value: false },
                )
                .is_err()
            );
            assert_prompt_preserved(&state, &other_pp);
        }
    }
}
