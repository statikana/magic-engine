use mtg_ir::{
    AbilityDef, Affected, CardDatabase, CardDefId, CardLayout, CardTypeMask, CombatRuleSpec,
    KeywordId, ManaCost, RestrictionKind, RuleMod, StaticBody, Zone,
};
use smallvec::SmallVec;

use crate::action::{Action, AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::auto_tap::{AutoTapPolicy, execute_auto_tap_plan, plan_auto_tap};
use crate::casting::prepare_next_untargeted_stack_trigger;
use crate::choices::ReplacementResumeOutcome;
use crate::commander::{is_commander_object, record_commander_combat_damage};
use crate::eval::{BoardView, EvalContext, evaluate_amount, evaluate_condition};
use crate::events::{AtomicEventKind, emit_event};
use crate::faces::{
    face_opts_for_game_object, for_each_active_ability_with_card_on_faces, resolve_object_faces,
};
use crate::filter::matches_object_filter;
use crate::id::{ObjectId, PlayerId};
use crate::keywords::{
    check_evasion_can_block, has_deathtouch, has_double_strike, has_first_strike, has_infect,
    has_lifelink, has_protection_from, has_trample, has_trample_over_planeswalkers, has_wither,
};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::mana::{
    ManaSpendContext, ManaSpendContextKind, can_pay_mana_from_pool, deduct_mana_for_context,
};
use crate::replace::{ReplacementOutcome, ReplacementResult, resolve_damage, resolve_life_gain};
use crate::sba::{SbaOutcome, check_sbas};
use crate::state::{
    AttackerEntry, BlockerEntry, CombatPromptKind, DirtyMask, EngineError, EvaluatedObject,
    GameState, LayerCache, ObjectFlags, PendingCombatDamageState, PendingPromptContext,
    PromptOwner, ReplaceableEventKind, ReplacementResumeHook, ScheduledDamagePacket,
};
use crate::triggers::flush_triggers;
use crate::turn::{TurnAdvanceOutcome, grant_priority_with_sba_and_triggers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombatOutcome {
    Continued,
    Paused { prompt_id: u32, chooser: PlayerId },
    GameOver { winner: Option<PlayerId> },
}

impl From<TurnAdvanceOutcome> for CombatOutcome {
    #[inline]
    fn from(v: TurnAdvanceOutcome) -> Self {
        match v {
            TurnAdvanceOutcome::Continued => CombatOutcome::Continued,
            TurnAdvanceOutcome::Paused { prompt_id, chooser } => {
                CombatOutcome::Paused { prompt_id, chooser }
            }
            TurnAdvanceOutcome::GameOver { winner } => CombatOutcome::GameOver { winner },
        }
    }
}

impl From<SbaOutcome> for CombatOutcome {
    #[inline]
    fn from(v: SbaOutcome) -> Self {
        match v {
            SbaOutcome::Done => CombatOutcome::Continued,
            SbaOutcome::Paused { prompt_id, chooser } => {
                CombatOutcome::Paused { prompt_id, chooser }
            }
            SbaOutcome::GameOver { winner } => CombatOutcome::GameOver { winner },
        }
    }
}

#[inline]
pub fn enter_combat_phase(state: &mut GameState) -> Result<CombatOutcome, EngineError> {
    {
        let combat = state.combat_mut();
        combat.active = true;
        combat.attackers_declared = false;
        combat.attackers.clear();
        combat.blockers.clear();
        combat.damage_assignment_order.clear();
        combat.declared_blockers_players.clear();
        combat.blocked_attackers.clear();
        combat.dealt_first_strike_ids.clear();
        combat.pending_damage = None;
    }
    state.mark_dirty(DirtyMask::COMBAT);
    Ok(grant_priority_with_sba_and_triggers(state)?.into())
}

pub(crate) fn enter_declare_attackers_step(
    state: &mut GameState,
) -> Result<CombatOutcome, EngineError> {
    {
        let combat = state.combat_mut();
        combat.active = true;
        combat.attackers_declared = false;
        combat.attackers.clear();
        combat.blockers.clear();
        combat.damage_assignment_order.clear();
        combat.declared_blockers_players.clear();
        combat.blocked_attackers.clear();
        combat.dealt_first_strike_ids.clear();
        combat.pending_damage = None;
    }
    let active = state.active_player();
    if state.player(active).is_some_and(|p| p.lost) {
        state.combat_mut().attackers_declared = true;
        state.mark_dirty(DirtyMask::COMBAT);
        return Ok(grant_priority_with_sba_and_triggers(state)?.into());
    }
    state.set_priority_player(active);
    state.set_decision(DecisionState::DeclareAttackers);
    state.mark_dirty(DirtyMask::COMBAT);
    Ok(CombatOutcome::Continued)
}

pub(crate) fn enter_declare_blockers_step(
    state: &mut GameState,
) -> Result<CombatOutcome, EngineError> {
    {
        let combat = state.combat_mut();
        combat.blockers.clear();
        combat.damage_assignment_order.clear();
        combat.declared_blockers_players.clear();
        combat.blocked_attackers.clear();
    }
    let defenders = defending_players_in_apnap_order(state);
    if let Some(&first_defender) = defenders.first() {
        state.set_priority_player(first_defender);
        state.set_decision(DecisionState::DeclareBlockers);
        state.mark_dirty(DirtyMask::COMBAT);
        Ok(CombatOutcome::Continued)
    } else {
        // No valid defending player left; grant priority directly.
        state.mark_dirty(DirtyMask::COMBAT);
        Ok(grant_priority_with_sba_and_triggers(state)?.into())
    }
}

pub(crate) fn advance_declare_blockers_on_concede(
    state: &mut GameState,
) -> Result<CombatOutcome, EngineError> {
    if state.player(state.active_player()).is_some_and(|p| p.lost)
        || state.combat().attackers.is_empty()
    {
        return finalize_declare_blockers(state);
    }
    let all_defenders = defending_players_in_apnap_order(state);
    if let Some(&next_def) = all_defenders
        .iter()
        .find(|d| !state.combat().declared_blockers_players.contains(d))
    {
        let cur_prio = state.priority_player();
        let cur_needs_to_declare = all_defenders.contains(&cur_prio)
            && !state.player(cur_prio).is_some_and(|p| p.lost)
            && !state.combat().declared_blockers_players.contains(&cur_prio);
        if !cur_needs_to_declare {
            state.set_priority_player(next_def);
        }
        state.set_decision(DecisionState::DeclareBlockers);
        return Ok(CombatOutcome::Continued);
    }
    if let Some(paused) = maybe_prompt_next_damage_assignment_order(state) {
        return Ok(paused);
    }
    finalize_declare_blockers(state)
}

pub fn remove_from_combat(state: &mut GameState, id: ObjectId) {
    let mut changed = false;
    {
        let combat = state.combat_mut();
        let prev_atk = combat.attackers.len();
        combat.attackers.retain(|e| e.attacker != id);
        if combat.attackers.len() != prev_atk {
            changed = true;
        }
        let prev_blk = combat.blockers.len();
        combat
            .blockers
            .retain(|b| b.blocker != id && b.attacker != id);
        if combat.blockers.len() != prev_blk {
            changed = true;
        }
        let prev_ord = combat.damage_assignment_order.len();
        combat.damage_assignment_order.retain(|(atk, _)| *atk != id);
        for (_, blks) in &mut combat.damage_assignment_order {
            blks.retain(|&b| b != id);
        }
        if combat.damage_assignment_order.len() != prev_ord {
            changed = true;
        }
        let prev_ba = combat.blocked_attackers.len();
        combat.blocked_attackers.retain(|&a| a != id);
        if combat.blocked_attackers.len() != prev_ba {
            changed = true;
        }
        combat.dealt_first_strike_ids.retain(|&a| a != id);
    }
    if changed {
        state.mark_dirty(DirtyMask::COMBAT);
    }
}

pub fn end_combat_phase_immediately(state: &mut GameState) -> Result<CombatOutcome, EngineError> {
    let was_active = state.combat().active;
    {
        let combat = state.combat_mut();
        combat.active = false;
        combat.attackers_declared = false;
        combat.attackers.clear();
        combat.blockers.clear();
        combat.damage_assignment_order.clear();
        combat.declared_blockers_players.clear();
        combat.blocked_attackers.clear();
        combat.dealt_first_strike_ids.clear();
        combat.pending_damage = None;
    }
    if was_active && state.rare_turn_state().is_some() {
        let active = state.active_player();
        let prev = std::mem::take(&mut state.rare_turn_state_mut().must_attack_next_combat);
        let mut keep = Vec::new();
        for id in prev {
            if state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && o.controller != active)
            {
                keep.push(id);
            }
        }
        state.rare_turn_state_mut().must_attack_next_combat = keep;
        state.prune_rare_turn_state_if_empty();
    }
    state.mark_dirty(DirtyMask::COMBAT);
    Ok(CombatOutcome::Continued)
}

#[inline]
pub fn any_combatant_has_first_strike(state: &GameState) -> bool {
    let mut ids: SmallVec<[ObjectId; 16]> = SmallVec::new();
    for a in &state.combat().attackers {
        if !ids.contains(&a.attacker) {
            ids.push(a.attacker);
        }
    }
    for b in &state.combat().blockers {
        if !ids.contains(&b.blocker) {
            ids.push(b.blocker);
        }
    }
    ids.into_iter().any(|id| {
        state
            .object(id)
            .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            && (has_first_strike(state, id) || has_double_strike(state, id))
    })
}

pub fn legal_combat_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let mut actions = Vec::new();
    match state.decision() {
        DecisionState::DeclareAttackers => {
            if player != state.active_player() || player != state.priority_player() {
                return actions;
            }
            let Some(p) = state.player(player) else {
                return actions;
            };
            let cache = state.evaluate_board();
            let ctx = AttackerDeclContext::build(state, cache, player);
            actions.reserve(
                p.battlefield
                    .len()
                    .saturating_mul(ctx.targets.len())
                    .min(32)
                    + 1,
            );
            let attackers = &state.combat().attackers;
            for &id in &p.battlefield {
                let already_attacking = attackers.iter().any(|e| e.attacker == id);
                if already_attacking {
                    actions.push(Action::RemoveAttacker { attacker: id });
                    continue;
                }
                let Some((ev, goaders)) = ctx.can_creature_attack_precheck(state, cache, id) else {
                    continue;
                };
                for (ti, &target) in ctx.targets.iter().enumerate() {
                    let defender = ctx.target_defenders[ti];
                    if ctx.can_creature_attack_target_with_precheck(
                        state, cache, id, ev, &goaders, target, defender,
                    ) {
                        actions.push(Action::AssignAttacker {
                            attacker: id,
                            target,
                        });
                    }
                }
            }
            if validate_attackers_confirmation_with_ctx(state, cache, &ctx).is_ok() {
                actions.push(Action::ConfirmAttackers);
            }
        }
        DecisionState::DeclareBlockers => {
            if player != state.priority_player() {
                return actions;
            }
            if state.player(player).is_none() {
                return actions;
            }
            let cache = state.evaluate_board();
            let ctx = build_defender_block_context(state, cache, player);
            let base = ctx.initial_state(state);
            let min_reqs = max_satisfiable_reqs(&ctx, state, cache, player);
            for (bi, &blocker) in ctx.blockers.iter().enumerate() {
                for (ai, &attacker) in ctx.attackers.iter().enumerate() {
                    if can_assign_edge_monotonic(&ctx, state, cache, player, &base, bi, ai) {
                        let mut next = base.clone();
                        next.assign(bi, ai);
                        if can_complete_state(&ctx, state, cache, player, &next, min_reqs) {
                            actions.push(Action::AssignBlocker { blocker, attacker });
                        }
                    }
                }
            }
            if validate_confirmation_on_state(&ctx, state, cache, player, &base, min_reqs).is_ok() {
                actions.push(Action::ConfirmBlockers);
            }
        }
        _ => {}
    }
    actions
}

pub fn apply_combat_action(
    state: &mut GameState,
    player: PlayerId,
    action: &Action,
) -> Result<CombatOutcome, EngineError> {
    match (state.decision().clone(), action) {
        (DecisionState::DeclareAttackers, Action::AssignAttacker { attacker, target }) => {
            if player != state.active_player() || player != state.priority_player() {
                return Err(EngineError::IllegalAction(
                    "only active player may declare attackers".into(),
                ));
            }
            {
                let cache = state.evaluate_board();
                if !is_valid_attack_target_with_cache(state, cache, player, *target) {
                    return Err(EngineError::IllegalAction("invalid attack target".into()));
                }
                if state
                    .combat()
                    .attackers
                    .iter()
                    .any(|e| e.attacker == *attacker)
                {
                    return Err(EngineError::IllegalAction(
                        "creature is already assigned as an attacker".into(),
                    ));
                }
                if !can_creature_attack_with_cache(state, cache, *attacker, *target, false) {
                    return Err(EngineError::IllegalAction(
                        "creature cannot attack chosen target".into(),
                    ));
                }
            }
            state.combat_mut().attackers.push(AttackerEntry {
                attacker: *attacker,
                target: *target,
            });
            state.mark_dirty(DirtyMask::COMBAT);
            Ok(CombatOutcome::Continued)
        }
        (DecisionState::DeclareAttackers, Action::RemoveAttacker { attacker }) => {
            if player != state.active_player() || player != state.priority_player() {
                return Err(EngineError::IllegalAction(
                    "only active player may modify attackers".into(),
                ));
            }
            let before = state.combat().attackers.len();
            state
                .combat_mut()
                .attackers
                .retain(|e| e.attacker != *attacker);
            if state.combat().attackers.len() == before {
                return Err(EngineError::IllegalAction(
                    "creature is not declared as an attacker".into(),
                ));
            }
            state.mark_dirty(DirtyMask::COMBAT);
            Ok(CombatOutcome::Continued)
        }
        (DecisionState::DeclareAttackers, Action::ConfirmAttackers) => {
            if player != state.active_player() || player != state.priority_player() {
                return Err(EngineError::IllegalAction(
                    "only active player may confirm attackers".into(),
                ));
            }
            let tax = {
                let cache = state.evaluate_board();
                let ctx = AttackerDeclContext::build(state, cache, player);
                validate_attackers_confirmation_with_ctx(state, cache, &ctx)?
            };
            pay_combat_tax_mana(state, player, &tax)?;

            let declared = state.combat().attackers.clone();
            let mut to_tap: SmallVec<[ObjectId; 8]> = SmallVec::new();
            if !declared.is_empty() {
                let cache = state.evaluate_board();
                let vig_kw = resolve_keyword_id_ci(state.db(), "vigilance");
                for entry in &declared {
                    let has_vig =
                        vig_kw.is_some_and(|kw| cache_has_keyword_id(cache, entry.attacker, kw));
                    if !has_vig {
                        to_tap.push(entry.attacker);
                    }
                }
            }
            for id in to_tap {
                if let Some(obj) = state.object_mut(id)
                    && !obj.flags.contains(ObjectFlags::TAPPED)
                {
                    obj.flags.insert(ObjectFlags::TAPPED);
                    state.mark_dirty(DirtyMask::TAPPED);
                    emit_event(
                        state,
                        AtomicEventKind::TapStateChanged {
                            object: id,
                            now_tapped: true,
                            actor: Some(player),
                        },
                    );
                }
            }
            state.combat_mut().attackers_declared = true;
            state.mark_dirty(DirtyMask::COMBAT);
            if !declared.is_empty() {
                emit_event(
                    state,
                    AtomicEventKind::AttackDeclared {
                        player,
                        attackers: declared,
                    },
                );
            }
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            Ok(grant_priority_with_sba_and_triggers(state)?.into())
        }
        (DecisionState::DeclareBlockers, Action::AssignBlocker { blocker, attacker }) => {
            if player != state.priority_player() {
                return Err(EngineError::IllegalAction(
                    "not the current defending player".into(),
                ));
            }
            let Some(blk_obj) = state.object(*blocker) else {
                return Err(EngineError::IllegalAction("blocker does not exist".into()));
            };
            if blk_obj.controller != player || blk_obj.zone != Zone::Battlefield {
                return Err(EngineError::IllegalAction(
                    "blocker not controlled on battlefield".into(),
                ));
            }
            if !attackers_facing_defender(state, player).contains(attacker) {
                return Err(EngineError::IllegalAction(
                    "attacker is not attacking this defending player".into(),
                ));
            }
            if state
                .combat()
                .blockers
                .iter()
                .any(|b| b.blocker == *blocker && b.attacker == *attacker)
            {
                return Err(EngineError::IllegalAction(
                    "blocker already assigned to this attacker".into(),
                ));
            }
            {
                let cache = state.evaluate_board();
                let max_blocks = blocker_max_block_count(state, cache, *blocker);
                let current_blocks = state
                    .combat()
                    .blockers
                    .iter()
                    .filter(|b| b.blocker == *blocker)
                    .count();
                if max_blocks.is_some_and(|m| current_blocks >= m as usize) {
                    return Err(EngineError::IllegalAction(
                        "blocker cannot block additional attackers".into(),
                    ));
                }
                if !can_creature_block_with_cache(state, cache, *blocker, *attacker, true) {
                    return Err(EngineError::IllegalAction(
                        "creature cannot legally block this attacker".into(),
                    ));
                }
                let ctx = build_defender_block_context(state, cache, player);
                let base = ctx.initial_state(state);
                let min_reqs = max_satisfiable_reqs(&ctx, state, cache, player);
                if let (Some(bi), Some(ai)) = (
                    ctx.blockers.iter().position(|&id| id == *blocker),
                    ctx.attackers.iter().position(|&id| id == *attacker),
                ) {
                    if !can_assign_edge_monotonic(&ctx, state, cache, player, &base, bi, ai) {
                        return Err(EngineError::IllegalAction(
                            "blocker assignment cannot lead to a legal block confirmation".into(),
                        ));
                    }
                    let mut next = base;
                    next.assign(bi, ai);
                    if !can_complete_state(&ctx, state, cache, player, &next, min_reqs) {
                        return Err(EngineError::IllegalAction(
                            "blocker assignment cannot lead to a legal block confirmation".into(),
                        ));
                    }
                }
            }
            state.combat_mut().blockers.push(BlockerEntry {
                blocker: *blocker,
                attacker: *attacker,
            });
            state.mark_dirty(DirtyMask::COMBAT);
            Ok(CombatOutcome::Continued)
        }
        (DecisionState::DeclareBlockers, Action::ConfirmBlockers) => {
            if player != state.priority_player() {
                return Err(EngineError::IllegalAction(
                    "not the current defending player".into(),
                ));
            }
            let tax = {
                let cache = state.evaluate_board();
                validate_blockers_confirmation(state, cache, player)?
            };
            pay_combat_tax_mana(state, player, &tax)?;

            if !state.combat().declared_blockers_players.contains(&player) {
                state.combat_mut().declared_blockers_players.push(player);
            }
            let my_attackers = attackers_facing_defender(state, player);
            let blocks_for_defender: Vec<BlockerEntry> = state
                .combat()
                .blockers
                .iter()
                .copied()
                .filter(|b| {
                    state
                        .object(b.blocker)
                        .is_some_and(|o| o.controller == player)
                })
                .collect();
            let mut unblocked_for_defender: Vec<ObjectId> = Vec::new();
            for &atk in &my_attackers {
                let has_blocker = state.combat().blockers.iter().any(|b| b.attacker == atk);
                if has_blocker {
                    if !state.combat().blocked_attackers.contains(&atk) {
                        state.combat_mut().blocked_attackers.push(atk);
                    }
                } else {
                    unblocked_for_defender.push(atk);
                }
            }
            state.mark_dirty(DirtyMask::COMBAT);

            emit_event(
                state,
                AtomicEventKind::BlockersDeclared {
                    defender: player,
                    blocks: blocks_for_defender,
                },
            );
            if !unblocked_for_defender.is_empty() {
                emit_event(
                    state,
                    AtomicEventKind::UnblockedAttackers {
                        defender: player,
                        attackers: unblocked_for_defender,
                    },
                );
            }

            let all_defenders = defending_players_in_apnap_order(state);
            if let Some(&next_def) = all_defenders
                .iter()
                .find(|d| !state.combat().declared_blockers_players.contains(d))
            {
                state.set_priority_player(next_def);
                state.set_decision(DecisionState::DeclareBlockers);
                return Ok(CombatOutcome::Continued);
            }

            if let Some(paused) = maybe_prompt_next_damage_assignment_order(state) {
                return Ok(paused);
            }
            finalize_declare_blockers(state)
        }
        _ => Err(EngineError::IllegalAction(
            "invalid action for current combat state".into(),
        )),
    }
}

pub fn resume_combat_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<CombatOutcome, EngineError> {
    let Some(pp) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no pending combat prompt to resume".into(),
        ));
    };
    if pp.chooser != actor || pp.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "combat prompt actor or prompt_id mismatch".into(),
        ));
    }
    let PromptOwner::Combat(ref kind) = pp.owner else {
        return Err(EngineError::IllegalAction(
            "pending prompt is not owned by combat".into(),
        ));
    };

    match kind {
        CombatPromptKind::DamageAssignmentOrder {
            unordered_blockers, ..
        } => {
            let ChoiceAnswer::OrderNext { id } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected OrderNext answer for DamageAssignmentOrder".into(),
                ));
            };
            if !unordered_blockers.contains(&id) {
                return Err(EngineError::IllegalAction(
                    "ordered blocker is not in remaining candidates".into(),
                ));
            }
        }
        CombatPromptKind::SuperTrampleAsUnblocked { .. } => {
            if !matches!(answer, ChoiceAnswer::YesNo { .. }) {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for SuperTrampleAsUnblocked".into(),
                ));
            }
            if state.combat().pending_damage.is_none() {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            }
        }
        CombatPromptKind::AttackerDamageDivide { .. } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickNumber for AttackerDamageDivide".into(),
                ));
            };
            let ChoiceRequest::ChooseNumber { min, max, .. } = pp.request else {
                return Err(EngineError::IllegalAction(
                    "corrupted request shape for AttackerDamageDivide".into(),
                ));
            };
            if value < min || value > max {
                return Err(EngineError::IllegalAction(
                    "damage assignment out of legal range".into(),
                ));
            }
            if state.combat().pending_damage.is_none() {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            }
        }
        CombatPromptKind::BlockerDamageDivide { .. } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickNumber for BlockerDamageDivide".into(),
                ));
            };
            let ChoiceRequest::ChooseNumber { min, max, .. } = pp.request else {
                return Err(EngineError::IllegalAction(
                    "corrupted request shape for BlockerDamageDivide".into(),
                ));
            };
            if value < min || value > max {
                return Err(EngineError::IllegalAction(
                    "blocker damage assignment out of legal range".into(),
                ));
            }
            if state.combat().pending_damage.is_none() {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            }
        }
        CombatPromptKind::AttackTaxPay | CombatPromptKind::BlockTaxPay => {
            return Err(EngineError::IllegalAction(
                "unexpected tax prompt state".into(),
            ));
        }
    }

    let Some(pp) = state.take_pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no pending combat prompt to resume".into(),
        ));
    };
    let PromptOwner::Combat(kind) = pp.owner else {
        state.set_pending_prompt(Some(pp));
        return Err(EngineError::IllegalAction(
            "pending prompt is not owned by combat".into(),
        ));
    };
    if matches!(state.decision(), DecisionState::Prompt { .. }) {
        state.set_decision(DecisionState::Priority);
    }

    match kind {
        CombatPromptKind::DamageAssignmentOrder {
            attacker,
            mut unordered_blockers,
            mut ordered_blockers,
        } => {
            let ChoiceAnswer::OrderNext { id } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected OrderNext answer for DamageAssignmentOrder".into(),
                ));
            };
            let Some(pos) = unordered_blockers.iter().position(|&b| b == id) else {
                return Err(EngineError::IllegalAction(
                    "ordered blocker is not in remaining candidates".into(),
                ));
            };
            unordered_blockers.remove(pos);
            ordered_blockers.push(id);
            if unordered_blockers.len() == 1 {
                ordered_blockers.push(unordered_blockers.remove(0));
            }
            if !unordered_blockers.is_empty() {
                let next_prompt_id = state.allocate_prompt_id();
                let req = ChoiceRequest::OrderObjects {
                    prompt_id: next_prompt_id,
                    objects: unordered_blockers.clone(),
                };
                state.set_decision(DecisionState::Prompt {
                    request: req.clone(),
                });
                state.set_pending_prompt(Some(PendingPromptContext {
                    prompt_id: next_prompt_id,
                    chooser: actor,
                    request: req,
                    owner: PromptOwner::Combat(CombatPromptKind::DamageAssignmentOrder {
                        attacker,
                        unordered_blockers,
                        ordered_blockers,
                    }),
                }));
                return Ok(CombatOutcome::Paused {
                    prompt_id: next_prompt_id,
                    chooser: actor,
                });
            }

            state
                .combat_mut()
                .damage_assignment_order
                .push((attacker, ordered_blockers));
            state.mark_dirty(DirtyMask::COMBAT);

            if let Some(paused) = maybe_prompt_next_damage_assignment_order(state) {
                return Ok(paused);
            }
            finalize_declare_blockers(state)
        }
        CombatPromptKind::SuperTrampleAsUnblocked {
            is_first_strike,
            attacker,
        } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for SuperTrampleAsUnblocked".into(),
                ));
            };
            let Some(mut pending) = state.combat_mut().pending_damage.take() else {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            };
            pending.chosen_super_trample.push((attacker, value));
            continue_combat_damage_assignment(state, *pending, is_first_strike)
        }
        CombatPromptKind::AttackerDamageDivide {
            is_first_strike,
            attacker,
            blocker_idx,
            remaining_damage,
        } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickNumber for AttackerDamageDivide".into(),
                ));
            };
            let Some(mut pending) = state.combat_mut().pending_damage.take() else {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            };
            let assigned = value as u32;
            let blockers = ordered_blockers_for_attacker(state, attacker);
            let blocker_id = blockers[blocker_idx];
            if assigned > 0 {
                let pkt = make_combat_packet(
                    state,
                    attacker,
                    current_controller(state, attacker),
                    TargetRef::Object { id: blocker_id },
                    assigned,
                    is_commander_object(state, attacker),
                );
                pending.packets.push(pkt);
            }
            let new_remaining = remaining_damage.saturating_sub(assigned);
            let next_blocker_idx = blocker_idx + 1;
            if next_blocker_idx < blockers.len() && new_remaining > 0 {
                // Continue dividing remaining damage across subsequent blockers / trample.
                if let Some(paused) = drive_attacker_blocker_division(
                    state,
                    &mut pending,
                    is_first_strike,
                    attacker,
                    &blockers,
                    next_blocker_idx,
                    new_remaining,
                )? {
                    return Ok(paused);
                }
            } else if new_remaining > 0 && attacker_has_any_trample(state, attacker) {
                push_trample_packet_for_attacker(state, &mut pending, attacker, new_remaining);
            } else if new_remaining > 0 && next_blocker_idx < blockers.len() {
                let pkt = make_combat_packet(
                    state,
                    attacker,
                    current_controller(state, attacker),
                    TargetRef::Object {
                        id: blockers[next_blocker_idx],
                    },
                    new_remaining,
                    is_commander_object(state, attacker),
                );
                pending.packets.push(pkt);
            }
            pending.attacker_cursor += 1;
            continue_combat_damage_assignment(state, *pending, is_first_strike)
        }
        CombatPromptKind::BlockerDamageDivide {
            is_first_strike,
            blocker,
            attacker_idx,
            remaining_damage,
        } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickNumber for BlockerDamageDivide".into(),
                ));
            };
            let Some(mut pending) = state.combat_mut().pending_damage.take() else {
                return Err(EngineError::IllegalAction(
                    "missing pending combat damage state".into(),
                ));
            };
            let assigned = value as u32;
            let blocked_list = attackers_blocked_by(state, blocker);
            let atk_id = blocked_list[attacker_idx];
            if assigned > 0 {
                let pkt = make_combat_packet(
                    state,
                    blocker,
                    current_controller(state, blocker),
                    TargetRef::Object { id: atk_id },
                    assigned,
                    false,
                );
                pending.packets.push(pkt);
            }
            let new_remaining = remaining_damage.saturating_sub(assigned);
            let next_idx = attacker_idx + 1;
            if next_idx < blocked_list.len()
                && new_remaining > 0
                && let Some(paused) = drive_blocker_attacker_division(
                    state,
                    &mut pending,
                    is_first_strike,
                    blocker,
                    &blocked_list,
                    next_idx,
                    new_remaining,
                )
            {
                return Ok(paused);
            }
            pending.blocker_cursor += 1;
            continue_combat_damage_assignment(state, *pending, is_first_strike)
        }
        CombatPromptKind::AttackTaxPay | CombatPromptKind::BlockTaxPay => Err(
            EngineError::IllegalAction("unexpected tax prompt state".into()),
        ),
    }
}

pub fn execute_combat_damage_step(
    state: &mut GameState,
    is_first_strike_step: bool,
) -> Result<CombatOutcome, EngineError> {
    let pending = PendingCombatDamageState {
        is_first_strike_step,
        ..Default::default()
    };
    state.combat_mut().pending_damage = None;
    continue_combat_damage_assignment(state, pending, is_first_strike_step)
}

pub fn resume_combat_damage_after_replacement(
    state: &mut GameState,
    outcome: ReplacementResumeOutcome,
) -> Result<CombatOutcome, EngineError> {
    let resolved = match outcome {
        ReplacementResumeOutcome::Paused { prompt_id, chooser } => {
            return Ok(CombatOutcome::Paused { prompt_id, chooser });
        }
        ReplacementResumeOutcome::EffectResolved(res) => *res,
        ReplacementResumeOutcome::ZoneMove(_) => {
            return Err(EngineError::IllegalAction(
                "unexpected ZoneMove outcome resuming combat damage".into(),
            ));
        }
    };
    let Some(mut pending) = state.combat_mut().pending_damage.take() else {
        return Err(EngineError::IllegalAction(
            "no pending combat damage batch to resume".into(),
        ));
    };
    let pkt_idx = pending.packet_idx;
    if let Some(pkt) = pending.packets.get(pkt_idx).cloned() {
        apply_resolved_combat_damage_packet(state, &mut pending, &pkt, resolved)?;
        pending.packet_idx = pkt_idx + 1;
    } else if pending.lifelink_idx < pending.lifelink_gains.len() {
        apply_resolved_lifelink_gain(state, resolved);
        pending.lifelink_idx += 1;
    }
    resolve_and_apply_damage_packets(state, *pending)
}

pub(crate) fn resume_combat_prompt_on_concede(
    state: &mut GameState,
    conceded: PlayerId,
    kind: CombatPromptKind,
) -> Result<CombatOutcome, EngineError> {
    match kind {
        CombatPromptKind::DamageAssignmentOrder { .. } => {
            if state.player(state.active_player()).is_some_and(|p| p.lost)
                || state.combat().attackers.is_empty()
            {
                return finalize_declare_blockers(state);
            }
            if let Some(paused) = maybe_prompt_next_damage_assignment_order(state) {
                return Ok(paused);
            }
            finalize_declare_blockers(state)
        }
        CombatPromptKind::SuperTrampleAsUnblocked {
            is_first_strike, ..
        }
        | CombatPromptKind::AttackerDamageDivide {
            is_first_strike, ..
        }
        | CombatPromptKind::BlockerDamageDivide {
            is_first_strike, ..
        } => {
            let Some(mut pending) = state.combat_mut().pending_damage.take() else {
                return Ok(grant_priority_with_sba_and_triggers(state)?.into());
            };
            filter_pending_combat_damage_on_concede(state, &mut pending, conceded);
            if state.player(state.active_player()).is_some_and(|p| p.lost)
                || state.combat().attackers.is_empty()
            {
                if pending.packets.is_empty() && pending.lifelink_gains.is_empty() {
                    return Ok(grant_priority_with_sba_and_triggers(state)?.into());
                }
                return resolve_and_apply_damage_packets(state, *pending);
            }
            if matches!(kind, CombatPromptKind::BlockerDamageDivide { .. }) {
                let mut unique_blockers: SmallVec<[ObjectId; 16]> = SmallVec::new();
                for b in &state.combat().blockers {
                    if !unique_blockers.contains(&b.blocker) {
                        unique_blockers.push(b.blocker);
                    }
                }
                pending.blocker_cursor = 0;
                while pending.blocker_cursor < unique_blockers.len() {
                    let blk = unique_blockers[pending.blocker_cursor];
                    if pending.packets.iter().any(|p| p.source == Some(blk)) {
                        pending.blocker_cursor += 1;
                    } else {
                        break;
                    }
                }
            }
            continue_combat_damage_assignment(state, *pending, is_first_strike)
        }
        CombatPromptKind::AttackTaxPay | CombatPromptKind::BlockTaxPay => {
            Ok(grant_priority_with_sba_and_triggers(state)?.into())
        }
    }
}

pub(crate) fn resume_combat_damage_replacement_on_concede(
    state: &mut GameState,
    conceded: PlayerId,
) -> Result<CombatOutcome, EngineError> {
    let Some(mut pending) = state.combat_mut().pending_damage.take() else {
        return Ok(grant_priority_with_sba_and_triggers(state)?.into());
    };
    filter_pending_combat_damage_on_concede(state, &mut pending, conceded);
    resolve_and_apply_damage_packets(state, *pending)
}

fn filter_pending_combat_damage_on_concede(
    state: &GameState,
    pending: &mut PendingCombatDamageState,
    conceded: PlayerId,
) {
    let mut new_packets = Vec::with_capacity(pending.packets.len());
    let mut new_pkt_idx = 0usize;
    for (i, pkt) in pending.packets.drain(..).enumerate() {
        let src_ok = pkt.source_controller != conceded
            && pkt.source.is_none_or(|s| {
                state.object(s).is_some_and(|o| {
                    o.zone == Zone::Battlefield
                        && !o.is_phased_out()
                        && o.controller != conceded
                        && o.owner != conceded
                })
            });
        let tgt_ok = match pkt.target {
            TargetRef::Player { id } => id != conceded && state.player(id).is_some_and(|p| !p.lost),
            TargetRef::Object { id } => state.object(id).is_some_and(|o| {
                o.zone == Zone::Battlefield
                    && !o.is_phased_out()
                    && o.controller != conceded
                    && o.owner != conceded
            }),
            TargetRef::Stack { .. } => false,
        };
        if src_ok && tgt_ok {
            if i < pending.packet_idx {
                new_pkt_idx += 1;
            }
            new_packets.push(pkt);
        }
    }
    pending.packets = new_packets;
    pending.packet_idx = new_pkt_idx;

    let mut new_ll = Vec::with_capacity(pending.lifelink_gains.len());
    let mut new_ll_idx = 0usize;
    for (i, (ctrl, amt)) in pending.lifelink_gains.drain(..).enumerate() {
        if ctrl != conceded && state.player(ctrl).is_some_and(|p| !p.lost) {
            if i < pending.lifelink_idx {
                new_ll_idx += 1;
            }
            new_ll.push((ctrl, amt));
        }
    }
    pending.lifelink_gains = new_ll;
    pending.lifelink_idx = new_ll_idx;

    pending
        .combat_damaged_players
        .retain(|(p, _)| *p != conceded && state.player(*p).is_some_and(|ps| !ps.lost));
}

#[inline]
fn attacker_has_any_trample(state: &GameState, atk: ObjectId) -> bool {
    has_trample(state, atk) || has_trample_over_planeswalkers(state, atk)
}

fn continue_combat_damage_assignment(
    state: &mut GameState,
    mut pending: PendingCombatDamageState,
    is_first_strike_step: bool,
) -> Result<CombatOutcome, EngineError> {
    let attackers = state.combat().attackers.clone();
    while pending.attacker_cursor < attackers.len() {
        let idx = pending.attacker_cursor;
        let entry = attackers[idx];
        let atk = entry.attacker;
        if !combatant_deals_damage_in_step(state, atk, is_first_strike_step) {
            pending.attacker_cursor += 1;
            continue;
        }
        let strike_power = creature_combat_damage_amount(state, atk);
        if strike_power == 0 {
            if is_first_strike_step && !state.combat().dealt_first_strike_ids.contains(&atk) {
                state.combat_mut().dealt_first_strike_ids.push(atk);
            }
            pending.attacker_cursor += 1;
            continue;
        }

        let is_blocked = state.combat().blocked_attackers.contains(&atk);
        let ordered_blockers = ordered_blockers_for_attacker(state, atk);
        let atk_ctrl = current_controller(state, atk);
        let is_cmdr = is_commander_object(state, atk);

        if is_first_strike_step && !state.combat().dealt_first_strike_ids.contains(&atk) {
            state.combat_mut().dealt_first_strike_ids.push(atk);
        }

        if !is_blocked {
            if has_trample_over_planeswalkers(state, atk)
                && matches!(entry.target, AttackTarget::Planeswalker { .. })
            {
                push_trample_packet_for_attacker(state, &mut pending, atk, strike_power);
            } else {
                let pkt = make_combat_packet(
                    state,
                    atk,
                    atk_ctrl,
                    attack_target_to_ref(entry.target),
                    strike_power,
                    is_cmdr,
                );
                pending.packets.push(pkt);
            }
            pending.attacker_cursor += 1;
            continue;
        }

        // Blocked attacker with empty blockers remaining (`CR 510.1c`): only deals damage if Trample.
        if ordered_blockers.is_empty() {
            if attacker_has_any_trample(state, atk) {
                if has_trample_over_planeswalkers(state, atk)
                    && matches!(entry.target, AttackTarget::Planeswalker { .. })
                {
                    push_trample_packet_for_attacker(state, &mut pending, atk, strike_power);
                } else {
                    let pkt = make_combat_packet(
                        state,
                        atk,
                        atk_ctrl,
                        attack_target_to_ref(entry.target),
                        strike_power,
                        is_cmdr,
                    );
                    pending.packets.push(pkt);
                }
            }
            pending.attacker_cursor += 1;
            continue;
        }

        // Check `AssignCombatDamageAsThoughUnblocked` ("super-trample").
        if has_super_trample(state, state.evaluate_board(), atk) {
            let existing_choice = pending
                .chosen_super_trample
                .iter()
                .find(|(id, _)| *id == atk)
                .map(|(_, v)| *v);
            match existing_choice {
                None => {
                    let prompt_id = state.allocate_prompt_id();
                    let req = ChoiceRequest::YesNo {
                        prompt_id,
                        label: "Assign this creature's combat damage as though it weren't blocked?"
                            .into(),
                    };
                    state.combat_mut().pending_damage = Some(Box::new(pending));
                    state.set_decision(DecisionState::Prompt {
                        request: req.clone(),
                    });
                    state.set_pending_prompt(Some(PendingPromptContext {
                        prompt_id,
                        chooser: atk_ctrl,
                        request: req,
                        owner: PromptOwner::Combat(CombatPromptKind::SuperTrampleAsUnblocked {
                            is_first_strike: is_first_strike_step,
                            attacker: atk,
                        }),
                    }));
                    return Ok(CombatOutcome::Paused {
                        prompt_id,
                        chooser: atk_ctrl,
                    });
                }
                Some(true) => {
                    if has_trample_over_planeswalkers(state, atk)
                        && matches!(entry.target, AttackTarget::Planeswalker { .. })
                    {
                        push_trample_packet_for_attacker(state, &mut pending, atk, strike_power);
                    } else {
                        let pkt = make_combat_packet(
                            state,
                            atk,
                            atk_ctrl,
                            attack_target_to_ref(entry.target),
                            strike_power,
                            is_cmdr,
                        );
                        pending.packets.push(pkt);
                    }
                    pending.attacker_cursor += 1;
                    continue;
                }
                Some(false) => {}
            }
        }

        if ordered_blockers.len() == 1 {
            let blk = ordered_blockers[0];
            if attacker_has_any_trample(state, atk) {
                let lethal = lethal_damage_required(state, atk, blk);
                let to_blocker = strike_power.min(lethal);
                let excess = strike_power.saturating_sub(to_blocker);
                if to_blocker > 0 {
                    let pkt = make_combat_packet(
                        state,
                        atk,
                        atk_ctrl,
                        TargetRef::Object { id: blk },
                        to_blocker,
                        is_cmdr,
                    );
                    pending.packets.push(pkt);
                }
                if excess > 0 {
                    push_trample_packet_for_attacker(state, &mut pending, atk, excess);
                }
            } else {
                let pkt = make_combat_packet(
                    state,
                    atk,
                    atk_ctrl,
                    TargetRef::Object { id: blk },
                    strike_power,
                    is_cmdr,
                );
                pending.packets.push(pkt);
            }
            pending.attacker_cursor += 1;
            continue;
        }

        // Multiple blockers: drive damage division sequentially.
        if let Some(paused) = drive_attacker_blocker_division(
            state,
            &mut pending,
            is_first_strike_step,
            atk,
            &ordered_blockers,
            0,
            strike_power,
        )? {
            return Ok(paused);
        }
        pending.attacker_cursor += 1;
    }

    // Now schedule blockers' combat damage.
    let mut unique_blockers: SmallVec<[ObjectId; 16]> = SmallVec::new();
    for b in &state.combat().blockers {
        if !unique_blockers.contains(&b.blocker) {
            unique_blockers.push(b.blocker);
        }
    }
    while pending.blocker_cursor < unique_blockers.len() {
        let blk = unique_blockers[pending.blocker_cursor];
        if !combatant_deals_damage_in_step(state, blk, is_first_strike_step) {
            pending.blocker_cursor += 1;
            continue;
        }
        let strike_power = creature_combat_damage_amount(state, blk);
        if is_first_strike_step && !state.combat().dealt_first_strike_ids.contains(&blk) {
            state.combat_mut().dealt_first_strike_ids.push(blk);
        }
        if strike_power == 0 {
            pending.blocker_cursor += 1;
            continue;
        }
        let blocked_attackers = attackers_blocked_by(state, blk);
        if blocked_attackers.is_empty() {
            pending.blocker_cursor += 1;
            continue;
        }
        if blocked_attackers.len() == 1 {
            let pkt = make_combat_packet(
                state,
                blk,
                current_controller(state, blk),
                TargetRef::Object {
                    id: blocked_attackers[0],
                },
                strike_power,
                false,
            );
            pending.packets.push(pkt);
            pending.blocker_cursor += 1;
            continue;
        }

        if let Some(paused) = drive_blocker_attacker_division(
            state,
            &mut pending,
            is_first_strike_step,
            blk,
            &blocked_attackers,
            0,
            strike_power,
        ) {
            return Ok(paused);
        }
        pending.blocker_cursor += 1;
    }

    resolve_and_apply_damage_packets(state, pending)
}

fn drive_attacker_blocker_division(
    state: &mut GameState,
    pending: &mut PendingCombatDamageState,
    is_first_strike_step: bool,
    attacker: ObjectId,
    ordered_blockers: &[ObjectId],
    idx: usize,
    mut remaining: u32,
) -> Result<Option<CombatOutcome>, EngineError> {
    let atk_ctrl = current_controller(state, attacker);
    let is_cmdr = is_commander_object(state, attacker);
    let trample = attacker_has_any_trample(state, attacker);

    if idx < ordered_blockers.len() && remaining > 0 {
        let blk = ordered_blockers[idx];
        let lethal = lethal_damage_required(state, attacker, blk);
        let is_last = idx + 1 == ordered_blockers.len();
        if remaining <= lethal || (is_last && !trample) {
            let deal = remaining;
            let pkt = make_combat_packet(
                state,
                attacker,
                atk_ctrl,
                TargetRef::Object { id: blk },
                deal,
                is_cmdr,
            );
            pending.packets.push(pkt);
            remaining = 0;
        } else {
            // Player has a genuine choice in [lethal ..= remaining].
            let min_val = lethal.min(remaining) as i32;
            let max_val = remaining as i32;
            let prompt_id = state.allocate_prompt_id();
            let req = ChoiceRequest::ChooseNumber {
                prompt_id,
                min: min_val,
                max: max_val,
            };
            let boxed = Box::new(std::mem::take(pending));
            state.combat_mut().pending_damage = Some(boxed);
            state.set_decision(DecisionState::Prompt {
                request: req.clone(),
            });
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: atk_ctrl,
                request: req,
                owner: PromptOwner::Combat(CombatPromptKind::AttackerDamageDivide {
                    is_first_strike: is_first_strike_step,
                    attacker,
                    blocker_idx: idx,
                    remaining_damage: remaining,
                }),
            }));
            return Ok(Some(CombatOutcome::Paused {
                prompt_id,
                chooser: atk_ctrl,
            }));
        }
    }

    if remaining > 0 && trample {
        push_trample_packet_for_attacker(state, pending, attacker, remaining);
    }
    Ok(None)
}

fn drive_blocker_attacker_division(
    state: &mut GameState,
    pending: &mut PendingCombatDamageState,
    is_first_strike_step: bool,
    blocker: ObjectId,
    blocked_attackers: &[ObjectId],
    idx: usize,
    remaining: u32,
) -> Option<CombatOutcome> {
    if remaining == 0 || idx >= blocked_attackers.len() {
        return None;
    }
    let blk_ctrl = current_controller(state, blocker);
    if idx + 1 == blocked_attackers.len() {
        let pkt = make_combat_packet(
            state,
            blocker,
            blk_ctrl,
            TargetRef::Object {
                id: blocked_attackers[idx],
            },
            remaining,
            false,
        );
        pending.packets.push(pkt);
        return None;
    }
    let prompt_id = state.allocate_prompt_id();
    let req = ChoiceRequest::ChooseNumber {
        prompt_id,
        min: 0,
        max: remaining as i32,
    };
    let boxed = Box::new(std::mem::take(pending));
    state.combat_mut().pending_damage = Some(boxed);
    state.set_decision(DecisionState::Prompt {
        request: req.clone(),
    });
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: blk_ctrl,
        request: req,
        owner: PromptOwner::Combat(CombatPromptKind::BlockerDamageDivide {
            is_first_strike: is_first_strike_step,
            blocker,
            attacker_idx: idx,
            remaining_damage: remaining,
        }),
    }));
    Some(CombatOutcome::Paused {
        prompt_id,
        chooser: blk_ctrl,
    })
}

fn resolve_and_apply_damage_packets(
    state: &mut GameState,
    mut pending: PendingCombatDamageState,
) -> Result<CombatOutcome, EngineError> {
    while pending.packet_idx < pending.packets.len() {
        let pkt = pending.packets[pending.packet_idx].clone();
        // Check protection / player prevent_all_combat_damage_this_turn unless unpreventable.
        let cant_be_prevented =
            crate::replace::is_damage_unpreventable(state, pkt.source, pkt.target, true, false);

        if !cant_be_prevented {
            if let TargetRef::Object { id: tgt_id } = pkt.target
                && let Some(src_id) = pkt.source
                && has_protection_from(state, tgt_id, src_id, false)
            {
                pending.packet_idx += 1;
                continue;
            }
            if let TargetRef::Player { id: tgt_p } = pkt.target
                && (state
                    .player(tgt_p)
                    .is_some_and(|p| p.prevent_all_combat_damage_this_turn)
                    || crate::keywords::player_has_protection_from(state, tgt_p, pkt.source))
            {
                pending.packet_idx += 1;
                continue;
            }
        }

        match resolve_damage(state, pkt.source, pkt.target, pkt.amount, true, false)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                state.combat_mut().pending_damage = Some(Box::new(pending));
                state.set_pending_replacement_resume(Some(ReplacementResumeHook::CombatDamageStep));
                return Ok(CombatOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(outcome) => {
                apply_resolved_combat_damage_packet(state, &mut pending, &pkt, *outcome)?;
                pending.packet_idx += 1;
            }
        }
    }

    // Apply simultaneous Lifelink life gains before SBAs (CR 120.3f).
    while pending.lifelink_idx < pending.lifelink_gains.len() {
        let (ctrl, amt) = pending.lifelink_gains[pending.lifelink_idx];
        if amt == 0 {
            pending.lifelink_idx += 1;
            continue;
        }
        let cant_gain = state
            .evaluate_board()
            .player(ctrl)
            .is_some_and(|pe| pe.cant_gain_life);
        if cant_gain {
            pending.lifelink_idx += 1;
            continue;
        }
        match resolve_life_gain(state, ctrl, amt, None)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                state.combat_mut().pending_damage = Some(Box::new(pending));
                state.set_pending_replacement_resume(Some(ReplacementResumeHook::CombatDamageStep));
                return Ok(CombatOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(outcome) => {
                apply_resolved_lifelink_gain(state, *outcome);
                pending.lifelink_idx += 1;
            }
        }
    }

    // Emit CombatDamageBatchPlayer events (CR 510.2).
    let batches = std::mem::take(&mut pending.combat_damaged_players);
    for (damaged_player, attackers) in batches {
        emit_event(
            state,
            AtomicEventKind::CombatDamageBatchPlayer {
                damaged_player,
                attackers,
            },
        );
    }

    state.combat_mut().pending_damage = None;
    match check_sbas(state)? {
        SbaOutcome::Done => {}
        SbaOutcome::Paused { prompt_id, chooser } => {
            return Ok(CombatOutcome::Paused { prompt_id, chooser });
        }
        SbaOutcome::GameOver { winner } => {
            return Ok(CombatOutcome::GameOver { winner });
        }
    }
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    Ok(grant_priority_with_sba_and_triggers(state)?.into())
}

fn apply_resolved_lifelink_gain(state: &mut GameState, outcome: ReplacementOutcome) {
    if !outcome.mods.skipped
        && let ReplaceableEventKind::LifeGain {
            player,
            amount: final_amt,
            ..
        } = outcome.event
        && final_amt > 0
        && !state
            .evaluate_board()
            .player(player)
            .is_some_and(|pe| pe.life_cant_change || pe.cant_gain_life)
        && let Some(p) = state.player_mut(player)
    {
        p.life = p.life.saturating_add(final_amt as i32);
        state.mark_dirty(DirtyMask::LIFE | DirtyMask::PLAYER_STATE);
        emit_event(
            state,
            AtomicEventKind::LifeChange {
                player,
                delta: final_amt as i32,
            },
        );
    }
}

fn make_combat_packet(
    state: &GameState,
    source: ObjectId,
    source_controller: PlayerId,
    target: TargetRef,
    amount: u32,
    is_commander: bool,
) -> ScheduledDamagePacket {
    ScheduledDamagePacket {
        source: Some(source),
        source_controller,
        target,
        amount,
        is_commander,
        has_deathtouch: has_deathtouch(state, source),
        has_lifelink: has_lifelink(state, source),
        has_infect: has_infect(state, source),
        has_wither: has_wither(state, source),
    }
}

fn apply_resolved_combat_damage_packet(
    state: &mut GameState,
    pending: &mut PendingCombatDamageState,
    pkt: &ScheduledDamagePacket,
    outcome: ReplacementOutcome,
) -> Result<(), EngineError> {
    if outcome.mods.skipped {
        return Ok(());
    }
    let ReplaceableEventKind::Damage {
        source,
        target,
        amount,
        ..
    } = outcome.event
    else {
        return Ok(());
    };
    if amount == 0 {
        return Ok(());
    }

    // Handle redirect split if present, else primary target.
    let deliveries: SmallVec<[(TargetRef, u32); 2]> = if outcome.mods.redirected_damage.is_empty() {
        smallvec::smallvec![(target, amount)]
    } else {
        let mut total_redir = 0u32;
        let mut out: SmallVec<[(TargetRef, u32); 2]> = SmallVec::new();
        for &(rt, ra) in &outcome.mods.redirected_damage {
            if ra > 0 {
                out.push((rt, ra));
                total_redir = total_redir.saturating_add(ra);
            }
        }
        let rem = amount.saturating_sub(total_redir);
        if rem > 0 {
            out.push((target, rem));
        }
        out
    };

    let src_has_deathtouch = pkt.has_deathtouch || source.is_some_and(|s| has_deathtouch(state, s));
    let src_has_infect = pkt.has_infect || source.is_some_and(|s| has_infect(state, s));
    let src_has_wither = pkt.has_wither || source.is_some_and(|s| has_wither(state, s));
    let src_has_lifelink = pkt.has_lifelink || source.is_some_and(|s| has_lifelink(state, s));
    let all_wither = state
        .evaluate_board()
        .players
        .iter()
        .any(|pe| pe.all_damage_has_wither);

    for (tgt, amt) in deliveries {
        if amt == 0 {
            continue;
        }
        match tgt {
            TargetRef::Object { id: obj_id } => {
                let types = state
                    .evaluate_board()
                    .get(obj_id)
                    .map_or(CardTypeMask::empty(), |e| e.types);
                if types.contains(CardTypeMask::CREATURE) {
                    if src_has_infect
                        || src_has_wither
                        || all_wither
                        || outcome.mods.damage_as_minus_counters
                    {
                        if let Some(m1) = state.db().counter_id("-1/-1") {
                            let prev = state.counter_count(obj_id, m1);
                            state.set_counter(obj_id, m1, prev.saturating_add(amt));
                            emit_event(
                                state,
                                AtomicEventKind::CounterChanged {
                                    object: obj_id,
                                    counter: m1,
                                    previous_count: prev,
                                    new_count: prev.saturating_add(amt),
                                    actor: Some(pkt.source_controller),
                                },
                            );
                        }
                    } else if let Some(obj) = state.object_mut(obj_id) {
                        obj.damage_marked = obj.damage_marked.saturating_add(amt as i32);
                        if src_has_deathtouch {
                            obj.deathtouch_damage_marked = true;
                        }
                        state.mark_dirty(DirtyMask::COUNTERS);
                    }
                } else if types.contains(CardTypeMask::PLANESWALKER) {
                    if let Some(loy) = state.db().counter_id("loyalty") {
                        let prev = state.counter_count(obj_id, loy);
                        let new_c = prev.saturating_sub(amt);
                        state.set_counter(obj_id, loy, new_c);
                        emit_event(
                            state,
                            AtomicEventKind::CounterChanged {
                                object: obj_id,
                                counter: loy,
                                previous_count: prev,
                                new_count: new_c,
                                actor: Some(pkt.source_controller),
                            },
                        );
                    }
                } else if types.contains(CardTypeMask::BATTLE)
                    && let Some(def) = state.db().counter_id("defense")
                {
                    let prev = state.counter_count(obj_id, def);
                    let new_c = prev.saturating_sub(amt);
                    state.set_counter(obj_id, def, new_c);
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: obj_id,
                            counter: def,
                            previous_count: prev,
                            new_count: new_c,
                            actor: Some(pkt.source_controller),
                        },
                    );
                }
            }
            TargetRef::Player { id: player_id } => {
                let (life_cant_change, cant_get_counters, dmg_as_infect) = state
                    .evaluate_board()
                    .player(player_id)
                    .map_or((false, false, false), |pe| {
                        (
                            pe.life_cant_change,
                            pe.cant_get_any_counters,
                            pe.damage_to_you_as_infect,
                        )
                    });
                if src_has_infect || dmg_as_infect {
                    if !cant_get_counters && let Some(p) = state.player_mut(player_id) {
                        p.poison = p.poison.saturating_add(amt.min(u32::from(u16::MAX)) as u16);
                        state.mark_dirty(DirtyMask::PLAYER_STATE);
                    }
                } else if !life_cant_change && let Some(p) = state.player_mut(player_id) {
                    p.life = p.life.saturating_sub(amt as i32);
                    state.mark_dirty(DirtyMask::LIFE | DirtyMask::PLAYER_STATE);
                    emit_event(
                        state,
                        AtomicEventKind::LifeChange {
                            player: player_id,
                            delta: -(amt as i32),
                        },
                    );
                }
                if pkt.is_commander
                    && let Some(src_id) = source
                {
                    record_commander_combat_damage(state, src_id, player_id, amt as i32);
                }
                if let Some(src_id) = source {
                    if let Some((_, list)) = pending
                        .combat_damaged_players
                        .iter_mut()
                        .find(|(p, _)| *p == player_id)
                    {
                        if !list.contains(&src_id) {
                            list.push(src_id);
                        }
                    } else {
                        pending
                            .combat_damaged_players
                            .push((player_id, vec![src_id]));
                    }
                }
            }
            TargetRef::Stack { .. } => {}
        }

        if let Some(src_id) = source
            && let Some(src_obj) = state.object_mut(src_id)
        {
            src_obj.flags.insert(ObjectFlags::DEALT_DAMAGE_THIS_TURN);
        }
        emit_event(
            state,
            AtomicEventKind::DamageDealt {
                source,
                source_controller: pkt.source_controller,
                target: tgt,
                amount: amt,
                is_combat: true,
            },
        );
        if src_has_lifelink {
            pending.lifelink_gains.push((pkt.source_controller, amt));
        }
    }
    Ok(())
}

pub fn can_creature_attack(state: &GameState, attacker: ObjectId, target: AttackTarget) -> bool {
    let cache = state.evaluate_board();
    can_creature_attack_with_cache(state, cache, attacker, target, false)
}

fn can_creature_attack_with_cache(
    state: &GameState,
    cache: &LayerCache,
    attacker: ObjectId,
    target: AttackTarget,
    ignore_mandatory: bool,
) -> bool {
    let _ = ignore_mandatory;
    let Some(obj) = state.object(attacker) else {
        return false;
    };
    if obj.zone != Zone::Battlefield
        || obj.is_phased_out()
        || obj.flags.contains(ObjectFlags::TAPPED)
    {
        return false;
    }
    let Some(ev) = cache.get(attacker) else {
        return false;
    };
    if !ev.types.contains(CardTypeMask::CREATURE) {
        return false;
    }
    let defender = attack_target_defender_with_cache(state, cache, target);
    if defender == ev.controller {
        return false;
    }

    let db = state.db();
    let need_as_though_haste = if obj.flags.contains(ObjectFlags::SUMMONING_SICK) {
        let has_haste = !ev.keywords.is_empty()
            && resolve_keyword_id_ci(db, "haste")
                .is_some_and(|kw| cache_has_keyword_id(cache, attacker, kw));
        !has_haste
    } else {
        false
    };
    let need_despite_defender = !ev.keywords.is_empty()
        && resolve_keyword_id_ci(db, "defender")
            .is_some_and(|kw| cache_has_keyword_id(cache, attacker, kw));

    let mut goaders: SmallVec<[PlayerId; 4]> = SmallVec::new();
    if let Some(rare) = state.rare(attacker) {
        for &g in &rare.goaded_by {
            if !goaders.contains(&g) {
                goaders.push(g);
            }
        }
    }

    let mut blocked = false;
    let mut found_as_though_haste = !need_as_though_haste;
    let mut found_despite_defender = !need_despite_defender;

    for_each_combat_static(state, cache, |src_id, src_ctrl, st| {
        if blocked {
            return;
        }
        match &st.body {
            StaticBody::Restriction {
                affected,
                restriction,
            } => match restriction {
                RestrictionKind::CantAttack | RestrictionKind::CantAttackOrBlock => {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, attacker) {
                        blocked = true;
                    }
                }
                RestrictionKind::CantAttackYou => {
                    if src_ctrl == defender
                        && matches!(target, AttackTarget::Player { id } if id == src_ctrl)
                        && affected_matches(state, cache, src_id, src_ctrl, affected, attacker)
                    {
                        blocked = true;
                    }
                }
                RestrictionKind::CantAttackYouOrPlaneswalkers
                    if defender == src_ctrl
                        && matches!(
                            target,
                            AttackTarget::Player { .. } | AttackTarget::Planeswalker { .. }
                        )
                        && affected_matches(state, cache, src_id, src_ctrl, affected, attacker) =>
                {
                    blocked = true;
                }
                _ => {}
            },
            StaticBody::CombatRule { affected, rule } => match rule {
                CombatRuleSpec::CanAttackAsThoughHaste => {
                    if !found_as_though_haste
                        && affected_matches(state, cache, src_id, src_ctrl, affected, attacker)
                    {
                        found_as_though_haste = true;
                    }
                }
                CombatRuleSpec::CanAttackDespiteDefender => {
                    if !found_despite_defender
                        && affected_matches(state, cache, src_id, src_ctrl, affected, attacker)
                    {
                        found_despite_defender = true;
                    }
                }
                CombatRuleSpec::IsGoaded => {
                    if src_ctrl != ev.controller
                        && !goaders.contains(&src_ctrl)
                        && affected_matches(state, cache, src_id, src_ctrl, affected, attacker)
                    {
                        goaders.push(src_ctrl);
                    }
                }
                CombatRuleSpec::CantAttackUnlessDefenderControls { filter } => {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, attacker) {
                        let ctx = EvalContext::for_source(src_ctrl, src_id)
                            .with_board(BoardView::Cached(cache));
                        let ok = state.player(defender).is_some_and(|dp| {
                            dp.battlefield
                                .iter()
                                .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                        });
                        if !ok {
                            blocked = true;
                        }
                    }
                }
                CombatRuleSpec::CantAttackIfDefenderControls { filter } => {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, attacker) {
                        let ctx = EvalContext::for_source(src_ctrl, src_id)
                            .with_board(BoardView::Cached(cache));
                        let bad = state.player(defender).is_some_and(|dp| {
                            dp.battlefield
                                .iter()
                                .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                        });
                        if bad {
                            blocked = true;
                        }
                    }
                }
                CombatRuleSpec::CantAttackUnlessMoreCreaturesThanDefender => {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, attacker) {
                        let atk_cnt = creature_count(state, cache, ev.controller);
                        let def_cnt = creature_count(state, cache, defender);
                        if atk_cnt <= def_cnt {
                            blocked = true;
                        }
                    }
                }
                CombatRuleSpec::CantAttackAlreadyAttackedPlayerThisTurn => {
                    let _ = target;
                }
                CombatRuleSpec::MustAttackAttachedController => {}
                _ => {}
            },
            _ => {}
        }
    });

    if blocked || !found_as_though_haste || !found_despite_defender {
        return false;
    }

    // Goad check: if goaded by player G (via `rare.goaded_by` or `CombatRuleSpec::IsGoaded`),
    // must attack a player other than G if able (`CR 701.38b`).
    if !goaders.is_empty() {
        let targets_goading_player = matches!(target, AttackTarget::Player { id } if goaders.contains(&id))
            || goaders.contains(&defender);
        if targets_goading_player {
            let other_targets = valid_attack_targets_with_cache(state, cache, ev.controller)
                .into_iter()
                .filter(|t| {
                    let d = attack_target_defender_with_cache(state, cache, *t);
                    !goaders.contains(&d)
                })
                .any(|t| t != target);
            if other_targets {
                return false;
            }
        }
    }

    true
}

#[derive(Clone, Copy)]
struct CombatStaticEntry<'a> {
    src_id: ObjectId,
    src_ctrl: PlayerId,
    st: &'a mtg_ir::StaticDef,
}

struct AttackerDeclContext<'a> {
    active: PlayerId,
    targets: SmallVec<[AttackTarget; 8]>,
    target_defenders: SmallVec<[PlayerId; 8]>,
    haste_kw: Option<KeywordId>,
    defender_kw: Option<KeywordId>,
    as_though_haste_statics: SmallVec<[CombatStaticEntry<'a>; 4]>,
    despite_defender_statics: SmallVec<[CombatStaticEntry<'a>; 4]>,
    target_indep_restrictions: SmallVec<[CombatStaticEntry<'a>; 4]>,
    target_dep_restrictions: SmallVec<[CombatStaticEntry<'a>; 4]>,
    goad_statics: SmallVec<[CombatStaticEntry<'a>; 4]>,
    forced_attack_statics: SmallVec<[CombatStaticEntry<'a>; 4]>,
    cant_attack_alone_statics: SmallVec<[CombatStaticEntry<'a>; 4]>,
    global_max: Option<u16>,
    per_defender_max: SmallVec<[(PlayerId, u16); 4]>,
    has_any_attack_tax: bool,
    has_rts_forced: bool,
}

impl<'a> AttackerDeclContext<'a> {
    fn build(state: &'a GameState, cache: &'a LayerCache, active: PlayerId) -> Self {
        let targets = valid_attack_targets_with_cache(state, cache, active);
        let target_defenders: SmallVec<[PlayerId; 8]> = targets
            .iter()
            .map(|&t| attack_target_defender_with_cache(state, cache, t))
            .collect();
        let db = state.db();
        let haste_kw = resolve_keyword_id_ci(db, "haste");
        let defender_kw = resolve_keyword_id_ci(db, "defender");
        let has_any_attack_tax = cache.player(active).is_some_and(|pe| {
            pe.active_rule_mods
                .iter()
                .any(|rm| matches!(rm.rule, RuleMod::AttackTax { .. }))
        });
        let has_rts_forced = state.rare_turn_state().is_some_and(|rts| {
            !rts.must_attack_next_combat.is_empty() || !rts.must_attack_player_this_turn.is_empty()
        });

        let mut as_though_haste_statics = SmallVec::new();
        let mut despite_defender_statics = SmallVec::new();
        let mut target_indep_restrictions = SmallVec::new();
        let mut target_dep_restrictions = SmallVec::new();
        let mut goad_statics = SmallVec::new();
        let mut forced_attack_statics = SmallVec::new();
        let mut cant_attack_alone_statics = SmallVec::new();
        let mut global_max: Option<u16> = None;
        let mut per_defender_max: SmallVec<[(PlayerId, u16); 4]> = SmallVec::new();

        for_each_combat_static(state, cache, |src_id, src_ctrl, st| match &st.body {
            StaticBody::Restriction { restriction, .. } => match restriction {
                RestrictionKind::CantAttack | RestrictionKind::CantAttackOrBlock => {
                    target_indep_restrictions.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                RestrictionKind::CantAttackYou | RestrictionKind::CantAttackYouOrPlaneswalkers => {
                    target_dep_restrictions.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                RestrictionKind::AttacksEachCombat | RestrictionKind::AttacksOrBlocksEachCombat => {
                    forced_attack_statics.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                _ => {}
            },
            StaticBody::CombatRule { rule, .. } => match rule {
                CombatRuleSpec::CanAttackAsThoughHaste => {
                    as_though_haste_statics.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                CombatRuleSpec::CanAttackDespiteDefender => {
                    despite_defender_statics.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                CombatRuleSpec::CantAttackUnlessDefenderControls { .. }
                | CombatRuleSpec::CantAttackIfDefenderControls { .. }
                | CombatRuleSpec::CantAttackUnlessMoreCreaturesThanDefender => {
                    target_dep_restrictions.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                CombatRuleSpec::IsGoaded => {
                    goad_statics.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                CombatRuleSpec::CantAttackAlone => {
                    cant_attack_alone_statics.push(CombatStaticEntry {
                        src_id,
                        src_ctrl,
                        st,
                    });
                }
                CombatRuleSpec::GlobalMaxAttackers {
                    max,
                    against_you_only,
                } => {
                    if *against_you_only {
                        per_defender_max.push((src_ctrl, *max));
                    } else {
                        global_max = Some(global_max.map_or(*max, |m| m.min(*max)));
                    }
                }
                _ => {}
            },
            _ => {}
        });

        Self {
            active,
            targets,
            target_defenders,
            haste_kw,
            defender_kw,
            as_though_haste_statics,
            despite_defender_statics,
            target_indep_restrictions,
            target_dep_restrictions,
            goad_statics,
            forced_attack_statics,
            cant_attack_alone_statics,
            global_max,
            per_defender_max,
            has_any_attack_tax,
            has_rts_forced,
        }
    }

    #[inline]
    fn entries_match(
        state: &GameState,
        cache: &LayerCache,
        entries: &[CombatStaticEntry<'a>],
        target_id: ObjectId,
    ) -> bool {
        for entry in entries {
            let affected = match &entry.st.body {
                StaticBody::Restriction { affected, .. }
                | StaticBody::CombatRule { affected, .. } => affected,
                _ => continue,
            };
            if affected_matches(
                state,
                cache,
                entry.src_id,
                entry.src_ctrl,
                affected,
                target_id,
            ) {
                return true;
            }
        }
        false
    }

    #[inline]
    fn can_creature_attack_precheck(
        &self,
        state: &GameState,
        cache: &'a LayerCache,
        attacker: ObjectId,
    ) -> Option<(&'a EvaluatedObject, SmallVec<[PlayerId; 4]>)> {
        let obj = state.object(attacker)?;
        if obj.zone != Zone::Battlefield
            || obj.is_phased_out()
            || obj.flags.contains(ObjectFlags::TAPPED)
        {
            return None;
        }
        let ev = cache.get(attacker)?;
        if !ev.types.contains(CardTypeMask::CREATURE) {
            return None;
        }
        if obj.flags.contains(ObjectFlags::SUMMONING_SICK) {
            let has_haste = !ev.keywords.is_empty()
                && self
                    .haste_kw
                    .is_some_and(|kw| cache_has_keyword_id(cache, attacker, kw));
            if !has_haste
                && !Self::entries_match(state, cache, &self.as_though_haste_statics, attacker)
            {
                return None;
            }
        }
        if !ev.keywords.is_empty()
            && self
                .defender_kw
                .is_some_and(|kw| cache_has_keyword_id(cache, attacker, kw))
            && !Self::entries_match(state, cache, &self.despite_defender_statics, attacker)
        {
            return None;
        }
        if !self.target_indep_restrictions.is_empty()
            && Self::entries_match(state, cache, &self.target_indep_restrictions, attacker)
        {
            return None;
        }

        let mut goaders: SmallVec<[PlayerId; 4]> = SmallVec::new();
        if let Some(rare) = state.rare(attacker) {
            for &g in &rare.goaded_by {
                if !goaders.contains(&g) {
                    goaders.push(g);
                }
            }
        }
        if !self.goad_statics.is_empty() {
            for entry in &self.goad_statics {
                if entry.src_ctrl != ev.controller
                    && !goaders.contains(&entry.src_ctrl)
                    && let StaticBody::CombatRule { affected, .. } = &entry.st.body
                    && affected_matches(
                        state,
                        cache,
                        entry.src_id,
                        entry.src_ctrl,
                        affected,
                        attacker,
                    )
                {
                    goaders.push(entry.src_ctrl);
                }
            }
        }
        Some((ev, goaders))
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn can_creature_attack_target_with_precheck(
        &self,
        state: &GameState,
        cache: &LayerCache,
        attacker: ObjectId,
        ev: &EvaluatedObject,
        goaders: &[PlayerId],
        target: AttackTarget,
        defender: PlayerId,
    ) -> bool {
        if defender == ev.controller {
            return false;
        }
        if !self.target_dep_restrictions.is_empty() {
            for entry in &self.target_dep_restrictions {
                match &entry.st.body {
                    StaticBody::Restriction {
                        affected,
                        restriction,
                    } => match restriction {
                        RestrictionKind::CantAttackYou => {
                            if entry.src_ctrl == defender
                                && matches!(target, AttackTarget::Player { id } if id == entry.src_ctrl)
                                && affected_matches(
                                    state,
                                    cache,
                                    entry.src_id,
                                    entry.src_ctrl,
                                    affected,
                                    attacker,
                                )
                            {
                                return false;
                            }
                        }
                        RestrictionKind::CantAttackYouOrPlaneswalkers
                            if defender == entry.src_ctrl
                                && matches!(
                                    target,
                                    AttackTarget::Player { .. } | AttackTarget::Planeswalker { .. }
                                )
                                && affected_matches(
                                    state,
                                    cache,
                                    entry.src_id,
                                    entry.src_ctrl,
                                    affected,
                                    attacker,
                                ) =>
                        {
                            return false;
                        }
                        _ => {}
                    },
                    StaticBody::CombatRule { affected, rule } => {
                        if !affected_matches(
                            state,
                            cache,
                            entry.src_id,
                            entry.src_ctrl,
                            affected,
                            attacker,
                        ) {
                            continue;
                        }
                        match rule {
                            CombatRuleSpec::CantAttackUnlessDefenderControls { filter } => {
                                let ctx = EvalContext::for_source(entry.src_ctrl, entry.src_id)
                                    .with_board(BoardView::Cached(cache));
                                let ok = state.player(defender).is_some_and(|dp| {
                                    dp.battlefield
                                        .iter()
                                        .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                                });
                                if !ok {
                                    return false;
                                }
                            }
                            CombatRuleSpec::CantAttackIfDefenderControls { filter } => {
                                let ctx = EvalContext::for_source(entry.src_ctrl, entry.src_id)
                                    .with_board(BoardView::Cached(cache));
                                let bad = state.player(defender).is_some_and(|dp| {
                                    dp.battlefield
                                        .iter()
                                        .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                                });
                                if bad {
                                    return false;
                                }
                            }
                            CombatRuleSpec::CantAttackUnlessMoreCreaturesThanDefender => {
                                let atk_cnt = creature_count(state, cache, ev.controller);
                                let def_cnt = creature_count(state, cache, defender);
                                if atk_cnt <= def_cnt {
                                    return false;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        if !goaders.is_empty() {
            let targets_goading_player = matches!(target, AttackTarget::Player { id } if goaders.contains(&id))
                || goaders.contains(&defender);
            if targets_goading_player {
                let has_other = if ev.controller == self.active {
                    self.targets
                        .iter()
                        .enumerate()
                        .any(|(i, &t)| t != target && !goaders.contains(&self.target_defenders[i]))
                } else {
                    valid_attack_targets_with_cache(state, cache, ev.controller)
                        .into_iter()
                        .any(|t| {
                            t != target
                                && !goaders
                                    .contains(&attack_target_defender_with_cache(state, cache, t))
                        })
                };
                if has_other {
                    return false;
                }
            }
        }

        true
    }

    #[inline]
    fn has_cant_attack_alone(&self, state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
        !self.cant_attack_alone_statics.is_empty()
            && Self::entries_match(state, cache, &self.cant_attack_alone_statics, id)
    }

    #[inline]
    fn is_forced_attacker(&self, state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
        if let Some(rare) = state.rare(id)
            && !rare.goaded_by.is_empty()
        {
            return true;
        }
        if self.has_rts_forced
            && let Some(rts) = state.rare_turn_state()
            && (rts.must_attack_next_combat.contains(&id)
                || rts
                    .must_attack_player_this_turn
                    .iter()
                    .any(|&(oid, _)| oid == id))
        {
            return true;
        }
        if !self.forced_attack_statics.is_empty()
            && Self::entries_match(state, cache, &self.forced_attack_statics, id)
        {
            return true;
        }
        !self.goad_statics.is_empty() && Self::entries_match(state, cache, &self.goad_statics, id)
    }

    #[inline]
    fn max_for_defender(&self, def_p: PlayerId) -> Option<u16> {
        let mut cap: Option<u16> = None;
        for &(dp, m) in &self.per_defender_max {
            if dp == def_p {
                cap = Some(cap.map_or(m, |c| c.min(m)));
            }
        }
        cap
    }
}

pub fn can_creature_block(state: &GameState, blocker: ObjectId, attacker: ObjectId) -> bool {
    let cache = state.evaluate_board();
    can_creature_block_with_cache(state, cache, blocker, attacker, false)
}

fn can_creature_block_with_cache(
    state: &GameState,
    _cache: &LayerCache,
    blocker: ObjectId,
    attacker: ObjectId,
    allow_already_attacking_check: bool,
) -> bool {
    let _ = allow_already_attacking_check;
    let Some(obj) = state.object(blocker) else {
        return false;
    };
    if obj.zone != Zone::Battlefield
        || obj.is_phased_out()
        || obj.flags.contains(ObjectFlags::TAPPED)
    {
        return false;
    }
    check_evasion_can_block(state, blocker, attacker).is_ok()
}

fn validate_attackers_confirmation_with_ctx<'a>(
    state: &GameState,
    cache: &'a LayerCache,
    ctx: &AttackerDeclContext<'a>,
) -> Result<ManaCost, EngineError> {
    let attackers = &state.combat().attackers;
    let has_forced_candidates = !ctx.forced_attack_statics.is_empty()
        || !ctx.goad_statics.is_empty()
        || ctx.has_rts_forced
        || state.player(ctx.active).is_some_and(|p| {
            p.battlefield
                .iter()
                .any(|&cid| state.rare(cid).is_some_and(|r| !r.goaded_by.is_empty()))
        });

    if attackers.is_empty() && !has_forced_candidates {
        return Ok(ManaCost::default());
    }

    if attackers.len() == 1 && ctx.has_cant_attack_alone(state, cache, attackers[0].attacker) {
        return Err(EngineError::IllegalAction(
            "creature cannot attack alone".into(),
        ));
    }

    // GlobalMaxAttackers check (`CR 508.1c`).
    if let Some(max) = ctx.global_max
        && attackers.len() > max as usize
    {
        return Err(EngineError::IllegalAction(
            "too many attackers declared".into(),
        ));
    }
    for &(def_p, max) in &ctx.per_defender_max {
        let count = attackers
            .iter()
            .filter(|e| attack_target_defender_with_cache(state, cache, e.target) == def_p)
            .count();
        if count > max as usize {
            return Err(EngineError::IllegalAction(
                "too many attackers against restricted defending player".into(),
            ));
        }
    }

    // Forced attackers (`AttacksEachCombat` / `Goad` / `must_attack_*`): only mandatory if
    // restrictions and unpaid attack tax permit (`CR 508.1d`).
    if has_forced_candidates {
        let global_saturated_by_forced = ctx.global_max.is_some_and(|mx| {
            attackers.len() >= mx as usize
                && attackers
                    .iter()
                    .all(|e| ctx.is_forced_attacker(state, cache, e.attacker))
        });
        if !global_saturated_by_forced
            && ctx.global_max != Some(0)
            && let Some(p) = state.player(ctx.active)
        {
            for &cid in &p.battlefield {
                if attackers.iter().any(|e| e.attacker == cid) {
                    continue;
                }
                if !ctx.is_forced_attacker(state, cache, cid) {
                    continue;
                }
                let Some((ev, goaders)) = ctx.can_creature_attack_precheck(state, cache, cid)
                else {
                    continue;
                };
                let mut any_tax_free_legal = ctx.targets.iter().enumerate().any(|(ti, &t)| {
                    let def_p = ctx.target_defenders[ti];
                    let target_ok = ctx.max_for_defender(def_p).is_none_or(|mx| {
                        if mx == 0 {
                            return false;
                        }
                        let forced_against_def = attackers
                            .iter()
                            .filter(|e| {
                                attack_target_defender_with_cache(state, cache, e.target) == def_p
                                    && ctx.is_forced_attacker(state, cache, e.attacker)
                            })
                            .count();
                        forced_against_def < mx as usize
                    });
                    target_ok
                        && ctx.can_creature_attack_target_with_precheck(
                            state, cache, cid, ev, &goaders, t, def_p,
                        )
                        && (!ctx.has_any_attack_tax
                            || mana_cost_is_zero(&single_attacker_tax_with_defender(
                                state, cache, ctx.active, cid, def_p,
                            )))
                });
                if any_tax_free_legal && ctx.has_cant_attack_alone(state, cache, cid) {
                    if ctx.global_max == Some(1) {
                        any_tax_free_legal = false;
                    } else {
                        let has_partner = attackers.iter().any(|e| e.attacker != cid)
                            || p.battlefield.iter().any(|&cid2| {
                                if cid2 == cid {
                                    return false;
                                }
                                let Some((ev2, goaders2)) =
                                    ctx.can_creature_attack_precheck(state, cache, cid2)
                                else {
                                    return false;
                                };
                                ctx.targets.iter().enumerate().any(|(ti2, &t2)| {
                                    let def_p2 = ctx.target_defenders[ti2];
                                    ctx.max_for_defender(def_p2) != Some(0)
                                        && ctx.can_creature_attack_target_with_precheck(
                                            state, cache, cid2, ev2, &goaders2, t2, def_p2,
                                        )
                                        && (!ctx.has_any_attack_tax
                                            || mana_cost_is_zero(
                                                &single_attacker_tax_with_defender(
                                                    state, cache, ctx.active, cid2, def_p2,
                                                ),
                                            ))
                                })
                            });
                        if !has_partner {
                            any_tax_free_legal = false;
                        }
                    }
                }
                if any_tax_free_legal {
                    return Err(EngineError::IllegalAction(
                        "a creature that must attack this combat was not declared".into(),
                    ));
                }
            }
        }

        if ctx.has_rts_forced
            && let Some(rts) = state.rare_turn_state()
        {
            for e in attackers {
                for &(oid, req_pid) in &rts.must_attack_player_this_turn {
                    if oid == e.attacker
                        && !matches!(e.target, AttackTarget::Player { id } if id == req_pid)
                    {
                        let req_target = AttackTarget::Player { id: req_pid };
                        let cap_ok = ctx.max_for_defender(req_pid).is_none_or(|mx| {
                            if mx == 0 {
                                return false;
                            }
                            let forced_against_def = attackers
                                .iter()
                                .filter(|other| {
                                    other.attacker != e.attacker
                                        && attack_target_defender_with_cache(
                                            state,
                                            cache,
                                            other.target,
                                        ) == req_pid
                                        && ctx.is_forced_attacker(state, cache, other.attacker)
                                })
                                .count();
                            forced_against_def < mx as usize
                        });
                        let alone_impossible = ctx.global_max == Some(1)
                            && ctx.has_cant_attack_alone(state, cache, e.attacker);
                        if cap_ok
                            && !alone_impossible
                            && ctx
                                .can_creature_attack_precheck(state, cache, e.attacker)
                                .is_some_and(|(ev, goaders)| {
                                    ctx.can_creature_attack_target_with_precheck(
                                        state, cache, e.attacker, ev, &goaders, req_target, req_pid,
                                    )
                                })
                            && (!ctx.has_any_attack_tax
                                || mana_cost_is_zero(&single_attacker_tax_with_defender(
                                    state, cache, ctx.active, e.attacker, req_pid,
                                )))
                        {
                            return Err(EngineError::IllegalAction(
                                "creature must attack designated player this combat if able".into(),
                            ));
                        }
                    }
                }
            }
        }
    }

    let tax = if ctx.has_any_attack_tax && !attackers.is_empty() {
        compute_attack_tax(state, cache, ctx.active, attackers)
    } else {
        ManaCost::default()
    };
    if !can_afford_mana_cost(state, cache, ctx.active, &tax) {
        return Err(EngineError::IllegalAction(
            "cannot pay mana cost required to attack".into(),
        ));
    }
    Ok(tax)
}

const MAX_LOOKAHEAD_NODES: usize = 512;

struct AtkBlockInfo {
    has_menace: bool,
    raw_min_blockers: Option<u16>,
    effective_min: u16,
    max_blockers: Option<u16>,
    must_all: bool,
    must_one: bool,
}

struct BlkBlockInfo {
    can_block: SmallVec<[bool; 8]>,
    max_blocks: Option<u16>,
    cant_block_alone: bool,
    forced: bool,
    tax: ManaCost,
    tax_is_zero: bool,
}

#[derive(Debug, Clone, Copy)]
enum BlockReq {
    MustAllEdge { bi: usize, ai: usize },
    MustOneAttacker { ai: usize },
    ForcedBlocker { bi: usize },
}

struct DefenderBlockContext {
    attackers: SmallVec<[ObjectId; 8]>,
    atk_info: SmallVec<[AtkBlockInfo; 8]>,
    blockers: SmallVec<[ObjectId; 16]>,
    blk_info: SmallVec<[BlkBlockInfo; 16]>,
    global_max_blockers: Option<u16>,
    has_any_block_tax: bool,
    reqs: SmallVec<[BlockReq; 16]>,
    total_possible_reqs: u16,
}

#[derive(Clone)]
struct BlockAssignState {
    blk_counts: SmallVec<[u16; 16]>,
    atk_counts: SmallVec<[u16; 8]>,
    unique_blockers: SmallVec<[usize; 16]>,
    edges: SmallVec<[u64; 16]>,
    overflow_edges: Option<Vec<Vec<bool>>>,
}

impl BlockAssignState {
    fn empty(ctx: &DefenderBlockContext) -> Self {
        let nb = ctx.blockers.len();
        let na = ctx.attackers.len();
        Self {
            blk_counts: smallvec::smallvec![0; nb],
            atk_counts: smallvec::smallvec![0; na],
            unique_blockers: SmallVec::new(),
            edges: smallvec::smallvec![0; nb],
            overflow_edges: if na > 64 {
                Some(vec![vec![false; na]; nb])
            } else {
                None
            },
        }
    }

    #[inline]
    fn has_edge(&self, bi: usize, ai: usize) -> bool {
        if let Some(ref ov) = self.overflow_edges {
            ov[bi][ai]
        } else {
            (self.edges[bi] & (1u64 << ai)) != 0
        }
    }

    #[inline]
    fn assign(&mut self, bi: usize, ai: usize) {
        if self.has_edge(bi, ai) {
            return;
        }
        if let Some(ref mut ov) = self.overflow_edges {
            ov[bi][ai] = true;
        } else {
            self.edges[bi] |= 1u64 << ai;
        }
        if self.blk_counts[bi] == 0 {
            self.unique_blockers.push(bi);
        }
        self.blk_counts[bi] += 1;
        self.atk_counts[ai] += 1;
    }
}

impl DefenderBlockContext {
    fn initial_state(&self, state: &GameState) -> BlockAssignState {
        let mut st = BlockAssignState::empty(self);
        for b in &state.combat().blockers {
            if let Some(bi) = self.blockers.iter().position(|&id| id == b.blocker)
                && let Some(ai) = self.attackers.iter().position(|&id| id == b.attacker)
            {
                st.assign(bi, ai);
            }
        }
        st
    }

    #[inline]
    fn is_req_satisfied(&self, st: &BlockAssignState, req_idx: usize) -> bool {
        match self.reqs[req_idx] {
            BlockReq::MustAllEdge { bi, ai } => st.has_edge(bi, ai),
            BlockReq::MustOneAttacker { ai } => st.atk_counts[ai] >= 1,
            BlockReq::ForcedBlocker { bi } => st.blk_counts[bi] >= 1,
        }
    }

    fn satisfied_reqs(&self, st: &BlockAssignState) -> u16 {
        let mut count = 0u16;
        for ri in 0..self.reqs.len() {
            if self.is_req_satisfied(st, ri) {
                count += 1;
            }
        }
        count
    }
}

fn build_defender_block_context(
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
) -> DefenderBlockContext {
    let mut attackers: SmallVec<[ObjectId; 8]> = SmallVec::new();
    for e in &state.combat().attackers {
        if attack_target_defender_with_cache(state, cache, e.target) == defender {
            attackers.push(e.attacker);
        }
    }

    let menace_kw = resolve_keyword_id_ci(state.db(), "menace");
    let mut atk_info: SmallVec<[AtkBlockInfo; 8]> = attackers
        .iter()
        .map(|&atk| {
            let menace = menace_kw.is_some_and(|kw| cache_has_keyword_id(cache, atk, kw));
            AtkBlockInfo {
                has_menace: menace,
                raw_min_blockers: None,
                effective_min: if menace { 2 } else { 0 },
                max_blockers: None,
                must_all: false,
                must_one: false,
            }
        })
        .collect();

    let mut blockers: SmallVec<[ObjectId; 16]> = SmallVec::new();
    let mut blk_info: SmallVec<[BlkBlockInfo; 16]> = SmallVec::new();
    if let Some(dp) = state.player(defender) {
        for &cid in &dp.battlefield {
            let already_blocking = state.combat().blockers.iter().any(|b| b.blocker == cid);
            let can_block: SmallVec<[bool; 8]> = attackers
                .iter()
                .map(|&atk| can_creature_block_with_cache(state, cache, cid, atk, true))
                .collect();
            if already_blocking || can_block.iter().any(|&ok| ok) {
                blockers.push(cid);
                blk_info.push(BlkBlockInfo {
                    can_block,
                    max_blocks: Some(1),
                    cant_block_alone: false,
                    forced: false,
                    tax: ManaCost::default(),
                    tax_is_zero: true,
                });
            }
        }
    }

    let mut global_max_blockers: Option<u16> = None;
    for_each_combat_static(state, cache, |src_id, src_ctrl, st| match &st.body {
        StaticBody::CombatRule { affected, rule } => match rule {
            CombatRuleSpec::GlobalMaxBlockers { max } => {
                global_max_blockers = Some(global_max_blockers.map_or(*max, |m| m.min(*max)));
            }
            CombatRuleSpec::MinBlockers { min } => {
                for (ai, &atk) in attackers.iter().enumerate() {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, atk) {
                        atk_info[ai].raw_min_blockers =
                            Some(atk_info[ai].raw_min_blockers.map_or(*min, |m| m.max(*min)));
                    }
                }
            }
            CombatRuleSpec::MaxBlockers { max } => {
                for (ai, &atk) in attackers.iter().enumerate() {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, atk) {
                        atk_info[ai].max_blockers =
                            Some(atk_info[ai].max_blockers.map_or(*max, |m| m.min(*max)));
                    }
                }
            }
            CombatRuleSpec::MustBeBlockedIfAble => {
                for (ai, &atk) in attackers.iter().enumerate() {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, atk) {
                        atk_info[ai].must_one = true;
                    }
                }
            }
            CombatRuleSpec::AllAbleBlockers => {
                for (ai, &atk) in attackers.iter().enumerate() {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, atk) {
                        atk_info[ai].must_all = true;
                    }
                }
            }
            CombatRuleSpec::CantBlockAlone => {
                for (bi, &blk) in blockers.iter().enumerate() {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, blk) {
                        blk_info[bi].cant_block_alone = true;
                    }
                }
            }
            CombatRuleSpec::AdditionalBlockCount { count: None } => {
                for (bi, &blk) in blockers.iter().enumerate() {
                    if blk_info[bi].max_blocks.is_some()
                        && affected_matches(state, cache, src_id, src_ctrl, affected, blk)
                    {
                        blk_info[bi].max_blocks = None;
                    }
                }
            }
            CombatRuleSpec::AdditionalBlockCount { count: Some(c) } => {
                for (bi, &blk) in blockers.iter().enumerate() {
                    if let Some(cur) = blk_info[bi].max_blocks.as_mut()
                        && affected_matches(state, cache, src_id, src_ctrl, affected, blk)
                    {
                        *cur = cur.saturating_add(*c);
                    }
                }
            }
            CombatRuleSpec::AdditionalBlockCountDynamic { amount } => {
                let mut dyn_val: Option<u16> = None;
                for (bi, &blk) in blockers.iter().enumerate() {
                    if let Some(cur) = blk_info[bi].max_blocks.as_mut()
                        && affected_matches(state, cache, src_id, src_ctrl, affected, blk)
                    {
                        let c = *dyn_val.get_or_insert_with(|| {
                            let ctx = EvalContext::for_source(src_ctrl, src_id)
                                .with_board(BoardView::Cached(cache));
                            evaluate_amount(state, amount, &ctx).max(0) as u16
                        });
                        *cur = cur.saturating_add(c);
                    }
                }
            }
            _ => {}
        },
        StaticBody::Restriction {
            affected,
            restriction:
                RestrictionKind::BlocksEachCombat | RestrictionKind::AttacksOrBlocksEachCombat,
        } => {
            for (bi, &blk) in blockers.iter().enumerate() {
                if affected_matches(state, cache, src_id, src_ctrl, affected, blk) {
                    blk_info[bi].forced = true;
                }
            }
        }
        _ => {}
    });

    for ainfo in &mut atk_info {
        if ainfo.must_all {
            ainfo.must_one = false;
        }
        ainfo.effective_min = ainfo
            .raw_min_blockers
            .unwrap_or(0)
            .max(if ainfo.has_menace { 2 } else { 0 });
    }

    let mut has_any_block_tax = false;
    if let Some(pe) = cache.player(defender) {
        for rm in &pe.active_rule_mods {
            if let RuleMod::BlockTax {
                generic,
                dynamic_amount,
                affected,
            } = &rm.rule
            {
                let mut dyn_n: Option<u32> = None;
                for (bi, &blk) in blockers.iter().enumerate() {
                    if let Some(aff) = affected
                        && let Some(src_id) = rm.source
                        && !affected_matches(state, cache, src_id, rm.controller, aff, blk)
                    {
                        continue;
                    }
                    if let Some(g) = generic {
                        blk_info[bi].tax.generic =
                            blk_info[bi].tax.generic.saturating_add(u32::from(*g));
                    }
                    if let Some(amt) = dynamic_amount {
                        let n = *dyn_n.get_or_insert_with(|| {
                            let ctx = match rm.source {
                                Some(src) => EvalContext::for_source(rm.controller, src),
                                None => EvalContext::new(rm.controller),
                            }
                            .with_board(BoardView::Cached(cache));
                            evaluate_amount(state, amt, &ctx).max(0) as u32
                        });
                        blk_info[bi].tax.generic = blk_info[bi].tax.generic.saturating_add(n);
                    }
                    blk_info[bi].tax_is_zero = mana_cost_is_zero(&blk_info[bi].tax);
                    if !blk_info[bi].tax_is_zero {
                        has_any_block_tax = true;
                    }
                }
            }
        }
    }

    let mut reqs: SmallVec<[BlockReq; 16]> = SmallVec::new();
    for (ai, ainfo) in atk_info.iter().enumerate() {
        if ainfo.must_all {
            for (bi, binfo) in blk_info.iter().enumerate() {
                if binfo.can_block[ai] && binfo.tax_is_zero {
                    reqs.push(BlockReq::MustAllEdge { bi, ai });
                }
            }
        } else if ainfo.must_one
            && blk_info
                .iter()
                .any(|binfo| binfo.can_block[ai] && binfo.tax_is_zero)
        {
            reqs.push(BlockReq::MustOneAttacker { ai });
        }
    }
    for (bi, binfo) in blk_info.iter().enumerate() {
        if binfo.forced && (0..attackers.len()).any(|ai| binfo.can_block[ai] && binfo.tax_is_zero) {
            reqs.push(BlockReq::ForcedBlocker { bi });
        }
    }
    let total_possible_reqs = reqs.len() as u16;

    DefenderBlockContext {
        attackers,
        atk_info,
        blockers,
        blk_info,
        global_max_blockers,
        has_any_block_tax,
        reqs,
        total_possible_reqs,
    }
}

fn state_block_tax(ctx: &DefenderBlockContext, st: &BlockAssignState) -> ManaCost {
    let mut tax = ManaCost::default();
    if !ctx.has_any_block_tax {
        return tax;
    }
    for &bi in &st.unique_blockers {
        if !ctx.blk_info[bi].tax_is_zero {
            add_mana_cost(&mut tax, &ctx.blk_info[bi].tax);
        }
    }
    tax
}

fn check_hard_restrictions(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
) -> Result<ManaCost, EngineError> {
    if st.unique_blockers.len() == 1 && ctx.blk_info[st.unique_blockers[0]].cant_block_alone {
        return Err(EngineError::IllegalAction(
            "creature cannot block alone".into(),
        ));
    }
    if let Some(max) = ctx.global_max_blockers
        && st.unique_blockers.len() > max as usize
    {
        return Err(EngineError::IllegalAction(
            "too many blockers declared".into(),
        ));
    }
    for ai in 0..ctx.attackers.len() {
        let blk_count = st.atk_counts[ai];
        if blk_count > 0 && blk_count < 2 && ctx.atk_info[ai].has_menace {
            return Err(EngineError::IllegalAction(
                "creature with menace must be blocked by two or more creatures".into(),
            ));
        }
        if let Some(min_b) = ctx.atk_info[ai].raw_min_blockers
            && blk_count > 0
            && blk_count < min_b
        {
            return Err(EngineError::IllegalAction(
                "attacker requires more blockers to be blocked".into(),
            ));
        }
        if let Some(max_b) = ctx.atk_info[ai].max_blockers
            && blk_count > max_b
        {
            return Err(EngineError::IllegalAction(
                "too many blockers assigned to attacker".into(),
            ));
        }
    }
    let tax = state_block_tax(ctx, st);
    if !mana_cost_is_zero(&tax) && !can_afford_mana_cost(state, cache, defender, &tax) {
        return Err(EngineError::IllegalAction(
            "cannot pay mana cost required to block".into(),
        ));
    }
    Ok(tax)
}

fn can_assign_edge_monotonic(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    bi: usize,
    ai: usize,
) -> bool {
    if st.has_edge(bi, ai) || !ctx.blk_info[bi].can_block[ai] {
        return false;
    }
    if ctx.blk_info[bi]
        .max_blocks
        .is_some_and(|m| st.blk_counts[bi] >= m)
    {
        return false;
    }
    if ctx.atk_info[ai]
        .max_blockers
        .is_some_and(|m| st.atk_counts[ai] >= m)
    {
        return false;
    }
    if st.blk_counts[bi] == 0 {
        if ctx
            .global_max_blockers
            .is_some_and(|m| st.unique_blockers.len() >= m as usize)
        {
            return false;
        }
        if ctx.has_any_block_tax && !ctx.blk_info[bi].tax_is_zero {
            let mut next_tax = state_block_tax(ctx, st);
            add_mana_cost(&mut next_tax, &ctx.blk_info[bi].tax);
            if !can_afford_mana_cost(state, cache, defender, &next_tax) {
                return false;
            }
        }
    }
    if st.atk_counts[ai] == 0 && ctx.atk_info[ai].effective_min > 1 {
        if ctx.atk_info[ai]
            .max_blockers
            .is_some_and(|m| m < ctx.atk_info[ai].effective_min)
        {
            return false;
        }
        if ctx
            .global_max_blockers
            .is_some_and(|m| m < ctx.atk_info[ai].effective_min)
        {
            return false;
        }
    }
    true
}

fn quick_bounds_feasible(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    min_reqs: u16,
    req_idx: usize,
) -> bool {
    if let Some(max) = ctx.global_max_blockers
        && st.unique_blockers.len() > max as usize
    {
        return false;
    }
    for ai in 0..ctx.attackers.len() {
        if let Some(max_b) = ctx.atk_info[ai].max_blockers
            && st.atk_counts[ai] > max_b
        {
            return false;
        }
    }
    if ctx.has_any_block_tax {
        let tax = state_block_tax(ctx, st);
        if !mana_cost_is_zero(&tax) && !can_afford_mana_cost(state, cache, defender, &tax) {
            return false;
        }
    }

    let mut deficit_atks: SmallVec<[usize; 8]> = SmallVec::new();
    let mut total_deficit = 0u32;
    for ai in 0..ctx.attackers.len() {
        let cnt = st.atk_counts[ai];
        let eff_min = ctx.atk_info[ai].effective_min;
        if cnt > 0 && cnt < eff_min {
            let need = eff_min - cnt;
            let avail = (0..ctx.blockers.len())
                .filter(|&bi| can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai))
                .count();
            if avail < usize::from(need) {
                return false;
            }
            deficit_atks.push(ai);
            total_deficit += u32::from(need);
        }
    }
    if total_deficit > 0 {
        let mut existing_cap = 0u32;
        let mut new_caps: SmallVec<[u16; 16]> = SmallVec::new();
        for bi in 0..ctx.blockers.len() {
            let rem_cap = ctx.blk_info[bi]
                .max_blocks
                .map_or(ctx.attackers.len() as u16, |m| {
                    m.saturating_sub(st.blk_counts[bi])
                });
            if rem_cap == 0 {
                continue;
            }
            let eligible_deficits = deficit_atks
                .iter()
                .filter(|&&ai| can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai))
                .count() as u16;
            let contrib = rem_cap.min(eligible_deficits);
            if contrib == 0 {
                continue;
            }
            if st.blk_counts[bi] > 0 {
                existing_cap += u32::from(contrib);
            } else {
                new_caps.push(contrib);
            }
        }
        if let Some(gm) = ctx.global_max_blockers {
            let new_slots = (gm as usize).saturating_sub(st.unique_blockers.len());
            if new_caps.len() > new_slots {
                new_caps.sort_unstable_by(|a, b| b.cmp(a));
                new_caps.truncate(new_slots);
            }
        }
        let total_cap = existing_cap + new_caps.iter().map(|&c| u32::from(c)).sum::<u32>();
        if total_cap < total_deficit {
            return false;
        }
    }

    if min_reqs > 0 {
        let cur_sat = ctx.satisfied_reqs(st);
        if cur_sat < min_reqs {
            let mut potential = 0u16;
            for ri in req_idx..ctx.reqs.len() {
                if ctx.is_req_satisfied(st, ri) {
                    continue;
                }
                let can_sat = match ctx.reqs[ri] {
                    BlockReq::MustAllEdge { bi, ai } => {
                        can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                    }
                    BlockReq::MustOneAttacker { ai } => (0..ctx.blockers.len()).any(|bi| {
                        ctx.blk_info[bi].tax_is_zero
                            && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                    }),
                    BlockReq::ForcedBlocker { bi } => (0..ctx.attackers.len()).any(|ai| {
                        ctx.blk_info[bi].tax_is_zero
                            && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                    }),
                };
                if can_sat {
                    potential += 1;
                }
            }
            if cur_sat + potential < min_reqs {
                return false;
            }
        }
    }

    true
}

#[allow(clippy::too_many_arguments)]
fn can_complete_dfs(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    min_reqs: u16,
    req_idx: usize,
    remaining_nodes: &mut usize,
) -> bool {
    if !quick_bounds_feasible(ctx, state, cache, defender, st, min_reqs, req_idx) {
        return false;
    }
    let cur_sat = ctx.satisfied_reqs(st);
    if check_hard_restrictions(ctx, state, cache, defender, st).is_ok() && cur_sat >= min_reqs {
        return true;
    }
    if *remaining_nodes == 0 {
        return true;
    }
    *remaining_nodes -= 1;

    // Priority 1: Deficit attacker (0 < count < effective_min) ordered by MRV.
    let mut best_deficit: Option<(usize, SmallVec<[usize; 16]>)> = None;
    for ai in 0..ctx.attackers.len() {
        let cnt = st.atk_counts[ai];
        if cnt > 0 && cnt < ctx.atk_info[ai].effective_min {
            let mut cands: SmallVec<[usize; 16]> = SmallVec::new();
            for bi in 0..ctx.blockers.len() {
                if can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai) {
                    cands.push(bi);
                }
            }
            let replace = match &best_deficit {
                None => true,
                Some((_, prev)) => cands.len() < prev.len(),
            };
            if replace {
                best_deficit = Some((ai, cands));
            }
        }
    }
    if let Some((ai, cands)) = best_deficit {
        for bi in cands {
            let mut next = st.clone();
            next.assign(bi, ai);
            if can_complete_dfs(
                ctx,
                state,
                cache,
                defender,
                &next,
                min_reqs,
                req_idx,
                remaining_nodes,
            ) {
                return true;
            }
        }
        return false;
    }

    // Priority 2: CantBlockAlone with 1 unique blocker -> try adding a 2nd unique blocker.
    if st.unique_blockers.len() == 1 && ctx.blk_info[st.unique_blockers[0]].cant_block_alone {
        let sole = st.unique_blockers[0];
        for bi2 in 0..ctx.blockers.len() {
            if bi2 == sole {
                continue;
            }
            for ai2 in 0..ctx.attackers.len() {
                if can_assign_edge_monotonic(ctx, state, cache, defender, st, bi2, ai2) {
                    let mut next = st.clone();
                    next.assign(bi2, ai2);
                    if can_complete_dfs(
                        ctx,
                        state,
                        cache,
                        defender,
                        &next,
                        min_reqs,
                        req_idx,
                        remaining_nodes,
                    ) {
                        return true;
                    }
                }
            }
        }
        return false;
    }

    // Priority 3: Satisfy remaining requirements up to `min_reqs`.
    if cur_sat < min_reqs {
        let mut ri = req_idx;
        while ri < ctx.reqs.len() && ctx.is_req_satisfied(st, ri) {
            ri += 1;
        }
        if ri >= ctx.reqs.len() {
            return false;
        }
        match ctx.reqs[ri] {
            BlockReq::MustAllEdge { bi, ai } => {
                if can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai) {
                    let mut next = st.clone();
                    next.assign(bi, ai);
                    if can_complete_dfs(
                        ctx,
                        state,
                        cache,
                        defender,
                        &next,
                        min_reqs,
                        ri + 1,
                        remaining_nodes,
                    ) {
                        return true;
                    }
                }
            }
            BlockReq::MustOneAttacker { ai } => {
                for bi in 0..ctx.blockers.len() {
                    if ctx.blk_info[bi].tax_is_zero
                        && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                    {
                        let mut next = st.clone();
                        next.assign(bi, ai);
                        if can_complete_dfs(
                            ctx,
                            state,
                            cache,
                            defender,
                            &next,
                            min_reqs,
                            ri + 1,
                            remaining_nodes,
                        ) {
                            return true;
                        }
                    }
                }
            }
            BlockReq::ForcedBlocker { bi } => {
                for ai in 0..ctx.attackers.len() {
                    if ctx.blk_info[bi].tax_is_zero
                        && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                    {
                        let mut next = st.clone();
                        next.assign(bi, ai);
                        if can_complete_dfs(
                            ctx,
                            state,
                            cache,
                            defender,
                            &next,
                            min_reqs,
                            ri + 1,
                            remaining_nodes,
                        ) {
                            return true;
                        }
                    }
                }
            }
        }
        // Try skipping `ctx.reqs[ri]` if remaining requirements can still reach `min_reqs`.
        if can_complete_dfs(
            ctx,
            state,
            cache,
            defender,
            st,
            min_reqs,
            ri + 1,
            remaining_nodes,
        ) {
            return true;
        }
    }

    false
}

fn can_complete_state(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    min_reqs: u16,
) -> bool {
    if check_hard_restrictions(ctx, state, cache, defender, st).is_ok()
        && ctx.satisfied_reqs(st) >= min_reqs
    {
        return true;
    }
    let mut remaining_nodes = MAX_LOOKAHEAD_NODES;
    can_complete_dfs(
        ctx,
        state,
        cache,
        defender,
        st,
        min_reqs,
        0,
        &mut remaining_nodes,
    )
}

#[allow(clippy::too_many_arguments)]
fn search_max_reqs(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    mut req_idx: usize,
    best: &mut u16,
    remaining_nodes: &mut usize,
) {
    while req_idx < ctx.reqs.len() && ctx.is_req_satisfied(st, req_idx) {
        req_idx += 1;
    }
    let cur_sat = ctx.satisfied_reqs(st);
    if cur_sat > *best && can_complete_state(ctx, state, cache, defender, st, cur_sat) {
        *best = cur_sat;
    }
    if req_idx >= ctx.reqs.len() || *best == ctx.total_possible_reqs {
        return;
    }
    let max_possible = cur_sat + (ctx.reqs.len() - req_idx) as u16;
    if max_possible <= *best || *remaining_nodes == 0 {
        return;
    }
    *remaining_nodes -= 1;

    match ctx.reqs[req_idx] {
        BlockReq::MustAllEdge { bi, ai } => {
            if can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai) {
                let mut next = st.clone();
                next.assign(bi, ai);
                search_max_reqs(
                    ctx,
                    state,
                    cache,
                    defender,
                    &next,
                    req_idx + 1,
                    best,
                    remaining_nodes,
                );
            }
        }
        BlockReq::MustOneAttacker { ai } => {
            for bi in 0..ctx.blockers.len() {
                if ctx.blk_info[bi].tax_is_zero
                    && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                {
                    let mut next = st.clone();
                    next.assign(bi, ai);
                    search_max_reqs(
                        ctx,
                        state,
                        cache,
                        defender,
                        &next,
                        req_idx + 1,
                        best,
                        remaining_nodes,
                    );
                }
            }
        }
        BlockReq::ForcedBlocker { bi } => {
            for ai in 0..ctx.attackers.len() {
                if ctx.blk_info[bi].tax_is_zero
                    && can_assign_edge_monotonic(ctx, state, cache, defender, st, bi, ai)
                {
                    let mut next = st.clone();
                    next.assign(bi, ai);
                    search_max_reqs(
                        ctx,
                        state,
                        cache,
                        defender,
                        &next,
                        req_idx + 1,
                        best,
                        remaining_nodes,
                    );
                }
            }
        }
    }
    search_max_reqs(
        ctx,
        state,
        cache,
        defender,
        st,
        req_idx + 1,
        best,
        remaining_nodes,
    );
}

fn max_satisfiable_reqs(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
) -> u16 {
    if ctx.reqs.is_empty() {
        return 0;
    }
    let empty = BlockAssignState::empty(ctx);
    if can_complete_state(ctx, state, cache, defender, &empty, ctx.total_possible_reqs) {
        return ctx.total_possible_reqs;
    }
    let mut best = 0u16;
    let mut remaining_nodes = MAX_LOOKAHEAD_NODES;
    search_max_reqs(
        ctx,
        state,
        cache,
        defender,
        &empty,
        0,
        &mut best,
        &mut remaining_nodes,
    );
    best
}

fn validate_confirmation_on_state(
    ctx: &DefenderBlockContext,
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
    st: &BlockAssignState,
    min_reqs: u16,
) -> Result<ManaCost, EngineError> {
    let tax = check_hard_restrictions(ctx, state, cache, defender, st)?;
    if ctx.satisfied_reqs(st) < min_reqs {
        for (ri, req) in ctx.reqs.iter().enumerate() {
            if !ctx.is_req_satisfied(st, ri) {
                return match req {
                    BlockReq::MustAllEdge { .. } => Err(EngineError::IllegalAction(
                        "all creatures able to block must block this attacker".into(),
                    )),
                    BlockReq::MustOneAttacker { .. } => Err(EngineError::IllegalAction(
                        "attacker must be blocked if able".into(),
                    )),
                    BlockReq::ForcedBlocker { .. } => Err(EngineError::IllegalAction(
                        "a creature that must block if able was not assigned".into(),
                    )),
                };
            }
        }
    }
    Ok(tax)
}

fn validate_blockers_confirmation(
    state: &GameState,
    cache: &LayerCache,
    defender: PlayerId,
) -> Result<ManaCost, EngineError> {
    let ctx = build_defender_block_context(state, cache, defender);
    let cur = ctx.initial_state(state);
    let min_reqs = max_satisfiable_reqs(&ctx, state, cache, defender);
    validate_confirmation_on_state(&ctx, state, cache, defender, &cur, min_reqs)
}

fn finalize_declare_blockers(state: &mut GameState) -> Result<CombatOutcome, EngineError> {
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    Ok(grant_priority_with_sba_and_triggers(state)?.into())
}

fn maybe_prompt_next_damage_assignment_order(state: &mut GameState) -> Option<CombatOutcome> {
    let active = state.active_player();
    for entry in &state.combat().attackers {
        let atk = entry.attacker;
        if state
            .combat()
            .damage_assignment_order
            .iter()
            .any(|(a, _)| *a == atk)
        {
            continue;
        }
        let blks: Vec<ObjectId> = state
            .combat()
            .blockers
            .iter()
            .filter(|b| b.attacker == atk)
            .map(|b| b.blocker)
            .collect();
        if blks.len() >= 2 {
            let prompt_id = state.allocate_prompt_id();
            let req = ChoiceRequest::OrderObjects {
                prompt_id,
                objects: blks.clone(),
            };
            state.set_decision(DecisionState::Prompt {
                request: req.clone(),
            });
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: active,
                request: req,
                owner: PromptOwner::Combat(CombatPromptKind::DamageAssignmentOrder {
                    attacker: atk,
                    unordered_blockers: blks,
                    ordered_blockers: Vec::new(),
                }),
            }));
            return Some(CombatOutcome::Paused {
                prompt_id,
                chooser: active,
            });
        }
    }
    None
}

fn ordered_blockers_for_attacker(state: &GameState, attacker: ObjectId) -> SmallVec<[ObjectId; 4]> {
    if let Some((_, ord)) = state
        .combat()
        .damage_assignment_order
        .iter()
        .find(|(a, _)| *a == attacker)
    {
        return ord
            .iter()
            .copied()
            .filter(|&id| {
                state
                    .object(id)
                    .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            })
            .collect();
    }
    state
        .combat()
        .blockers
        .iter()
        .filter(|b| b.attacker == attacker)
        .map(|b| b.blocker)
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
        })
        .collect()
}

fn attackers_blocked_by(state: &GameState, blocker: ObjectId) -> SmallVec<[ObjectId; 4]> {
    state
        .combat()
        .blockers
        .iter()
        .filter(|b| b.blocker == blocker)
        .map(|b| b.attacker)
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
        })
        .collect()
}

fn push_trample_packet_for_attacker(
    state: &GameState,
    pending: &mut PendingCombatDamageState,
    attacker: ObjectId,
    amount: u32,
) {
    if amount == 0 {
        return;
    }
    let Some(entry) = state
        .combat()
        .attackers
        .iter()
        .find(|e| e.attacker == attacker)
    else {
        return;
    };
    let atk_ctrl = current_controller(state, attacker);
    let is_cmdr = is_commander_object(state, attacker);
    if let AttackTarget::Planeswalker { id: pw_id } = entry.target
        && has_trample_over_planeswalkers(state, attacker)
    {
        let base_loyalty = state
            .db()
            .counter_id("loyalty")
            .map_or(0, |cid| state.counter_count(pw_id, cid));
        let already_assigned: u32 = pending
            .packets
            .iter()
            .filter(|p| p.target == TargetRef::Object { id: pw_id })
            .map(|p| p.amount)
            .sum();
        let rem_loyalty = base_loyalty.saturating_sub(already_assigned);
        let to_pw = amount.min(rem_loyalty);
        let excess = amount.saturating_sub(to_pw);
        if to_pw > 0 {
            let pkt = make_combat_packet(
                state,
                attacker,
                atk_ctrl,
                TargetRef::Object { id: pw_id },
                to_pw,
                is_cmdr,
            );
            pending.packets.push(pkt);
        }
        if excess > 0 {
            let pw_ctrl = current_controller(state, pw_id);
            let pkt = make_combat_packet(
                state,
                attacker,
                atk_ctrl,
                TargetRef::Player { id: pw_ctrl },
                excess,
                is_cmdr,
            );
            pending.packets.push(pkt);
        }
        return;
    }
    let target = match entry.target {
        AttackTarget::Player { id } => TargetRef::Player { id },
        AttackTarget::Planeswalker { id } | AttackTarget::Battle { id } => TargetRef::Object { id },
    };
    let pkt = make_combat_packet(state, attacker, atk_ctrl, target, amount, is_cmdr);
    pending.packets.push(pkt);
}

fn lethal_damage_required(state: &GameState, attacker: ObjectId, blocker: ObjectId) -> u32 {
    if has_deathtouch(state, attacker) {
        return 1;
    }
    let toughness = state
        .evaluate_board()
        .get(blocker)
        .and_then(|e| e.pt)
        .map_or(0, |pt| pt.toughness);
    let marked = state.object(blocker).map_or(0, |o| o.damage_marked);
    (toughness - marked).max(0) as u32
}

fn combatant_deals_damage_in_step(
    state: &GameState,
    id: ObjectId,
    is_first_strike_step: bool,
) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield || obj.is_phased_out() {
        return false;
    }
    let Some(ev) = state.evaluate_board().get(id) else {
        return false;
    };
    if !ev.types.contains(CardTypeMask::CREATURE) {
        return false;
    }
    let fs = has_first_strike(state, id);
    let ds = has_double_strike(state, id);
    if is_first_strike_step {
        fs || ds
    } else {
        let already_dealt = state.combat().dealt_first_strike_ids.contains(&id);
        !already_dealt || ds
    }
}

fn creature_combat_damage_amount(state: &GameState, id: ObjectId) -> u32 {
    let cache = state.evaluate_board();
    let Some(pt) = cache.get(id).and_then(|e| e.pt) else {
        return 0;
    };
    let val = if assign_damage_by_toughness(state, cache, id) {
        pt.toughness
    } else {
        pt.power
    };
    val.max(0) as u32
}

#[inline]
fn current_controller(state: &GameState, id: ObjectId) -> PlayerId {
    current_controller_with_cache(state, state.evaluate_board(), id)
}

#[inline]
fn current_controller_with_cache(state: &GameState, cache: &LayerCache, id: ObjectId) -> PlayerId {
    cache
        .get(id)
        .map(|e| e.controller)
        .or_else(|| state.object(id).map(|o| o.controller))
        .unwrap_or(PlayerId(0))
}

#[inline]
fn attack_target_to_ref(t: AttackTarget) -> TargetRef {
    match t {
        AttackTarget::Player { id } => TargetRef::Player { id },
        AttackTarget::Planeswalker { id } | AttackTarget::Battle { id } => TargetRef::Object { id },
    }
}

#[inline]
fn attack_target_defender(state: &GameState, t: AttackTarget) -> PlayerId {
    match t {
        AttackTarget::Player { id } => id,
        AttackTarget::Planeswalker { id } => current_controller(state, id),
        AttackTarget::Battle { id } => state
            .rare(id)
            .and_then(|r| r.protector)
            .unwrap_or_else(|| current_controller(state, id)),
    }
}

#[inline]
fn attack_target_defender_with_cache(
    state: &GameState,
    cache: &LayerCache,
    t: AttackTarget,
) -> PlayerId {
    match t {
        AttackTarget::Player { id } => id,
        AttackTarget::Planeswalker { id } => current_controller_with_cache(state, cache, id),
        AttackTarget::Battle { id } => state
            .rare(id)
            .and_then(|r| r.protector)
            .unwrap_or_else(|| current_controller_with_cache(state, cache, id)),
    }
}

fn valid_attack_targets_with_cache(
    state: &GameState,
    cache: &LayerCache,
    active: PlayerId,
) -> SmallVec<[AttackTarget; 8]> {
    let mut targets: SmallVec<[AttackTarget; 8]> = SmallVec::new();
    for p in state.players() {
        if p.lost || p.id == active {
            continue;
        }
        targets.push(AttackTarget::Player { id: p.id });
        for &perm_id in &p.battlefield {
            let Some(obj) = state.object(perm_id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            if let Some(ev) = cache.get(perm_id) {
                if ev.types.contains(CardTypeMask::PLANESWALKER) {
                    targets.push(AttackTarget::Planeswalker { id: perm_id });
                } else if ev.types.contains(CardTypeMask::BATTLE) {
                    let prot = state
                        .rare(perm_id)
                        .and_then(|r| r.protector)
                        .unwrap_or(ev.controller);
                    if prot != active {
                        targets.push(AttackTarget::Battle { id: perm_id });
                    }
                }
            }
        }
    }
    // Also Battles controlled by `active` whose protector is an opponent (`CR 310.8`).
    if let Some(ap) = state.player(active) {
        for &perm_id in &ap.battlefield {
            if state.object(perm_id).is_some_and(|o| !o.is_phased_out())
                && cache
                    .get(perm_id)
                    .is_some_and(|e| e.types.contains(CardTypeMask::BATTLE))
                && state
                    .rare(perm_id)
                    .and_then(|r| r.protector)
                    .is_some_and(|prot| prot != active)
            {
                targets.push(AttackTarget::Battle { id: perm_id });
            }
        }
    }
    targets
}

#[inline]
fn is_valid_attack_target_with_cache(
    state: &GameState,
    cache: &LayerCache,
    active: PlayerId,
    target: AttackTarget,
) -> bool {
    match target {
        AttackTarget::Player { id } => id != active && state.player(id).is_some_and(|p| !p.lost),
        AttackTarget::Planeswalker { id } => {
            let Some(obj) = state.object(id) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || obj.is_phased_out() {
                return false;
            }
            let Some(ev) = cache.get(id) else {
                return false;
            };
            if !ev.types.contains(CardTypeMask::PLANESWALKER) {
                return false;
            }
            let ctrl = obj.controller;
            ctrl != active && state.player(ctrl).is_some_and(|p| !p.lost)
        }
        AttackTarget::Battle { id } => {
            let Some(obj) = state.object(id) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || obj.is_phased_out() {
                return false;
            }
            let Some(ev) = cache.get(id) else {
                return false;
            };
            if !ev.types.contains(CardTypeMask::BATTLE) {
                return false;
            }
            let ctrl = obj.controller;
            if ctrl != active {
                if !state.player(ctrl).is_some_and(|p| !p.lost) {
                    return false;
                }
                let prot = state
                    .rare(id)
                    .and_then(|r| r.protector)
                    .unwrap_or(ev.controller);
                prot != active
            } else {
                state
                    .rare(id)
                    .and_then(|r| r.protector)
                    .is_some_and(|prot| prot != active)
            }
        }
    }
}

fn defending_players_in_apnap_order(state: &GameState) -> SmallVec<[PlayerId; 4]> {
    let n = state.players().len();
    let active_idx = state.active_player().0 as usize;
    let mut out: SmallVec<[PlayerId; 4]> = SmallVec::new();
    for offset in 1..n {
        let idx = (active_idx + offset) % n;
        let pid = PlayerId(idx as u8);
        if state.player(pid).is_some_and(|p| p.lost) {
            continue;
        }
        let attacked = state
            .combat()
            .attackers
            .iter()
            .any(|e| attack_target_defender(state, e.target) == pid);
        if attacked {
            out.push(pid);
        }
    }
    out
}

fn attackers_facing_defender(state: &GameState, defender: PlayerId) -> Vec<ObjectId> {
    state
        .combat()
        .attackers
        .iter()
        .filter(|e| attack_target_defender(state, e.target) == defender)
        .map(|e| e.attacker)
        .collect()
}

#[inline]
fn mana_cost_is_zero(cost: &ManaCost) -> bool {
    cost.generic == 0 && cost.pips.is_empty()
}

fn can_afford_mana_cost(
    state: &GameState,
    cache: &LayerCache,
    player: PlayerId,
    cost: &ManaCost,
) -> bool {
    if mana_cost_is_zero(cost) {
        return true;
    }
    let ctx = ManaSpendContext {
        player,
        kind: ManaSpendContextKind::WardOrCost,
        ..Default::default()
    };
    if can_pay_mana_from_pool(state, player, cost, &ctx) {
        return true;
    }
    let _ = cache;
    let plan = plan_auto_tap(state, player, cost, &ctx, AutoTapPolicy::default());
    plan.can_pay
}

fn pay_combat_tax_mana(
    state: &mut GameState,
    player: PlayerId,
    cost: &ManaCost,
) -> Result<(), EngineError> {
    if mana_cost_is_zero(cost) {
        return Ok(());
    }
    let ctx = ManaSpendContext {
        player,
        kind: ManaSpendContextKind::WardOrCost,
        ..Default::default()
    };
    if !can_pay_mana_from_pool(state, player, cost, &ctx) {
        let plan = plan_auto_tap(state, player, cost, &ctx, AutoTapPolicy::default());
        if !plan.can_pay {
            return Err(EngineError::IllegalAction(
                "insufficient mana to pay combat tax".into(),
            ));
        }
        execute_auto_tap_plan(state, player, &plan)?;
    }
    deduct_mana_for_context(state, player, cost, &ctx, &[])?;
    Ok(())
}

fn compute_attack_tax(
    state: &GameState,
    cache: &LayerCache,
    attacker_owner: PlayerId,
    attackers: &[AttackerEntry],
) -> ManaCost {
    let mut total = ManaCost::default();
    for entry in attackers {
        let defender = attack_target_defender_with_cache(state, cache, entry.target);
        let single = single_attacker_tax_with_defender(
            state,
            cache,
            attacker_owner,
            entry.attacker,
            defender,
        );
        add_mana_cost(&mut total, &single);
    }
    total
}

fn single_attacker_tax_with_defender(
    state: &GameState,
    cache: &LayerCache,
    attacker_owner: PlayerId,
    attacker: ObjectId,
    defender: PlayerId,
) -> ManaCost {
    let mut total = ManaCost::default();
    let Some(pe) = cache.player(attacker_owner) else {
        return total;
    };
    for rm in &pe.active_rule_mods {
        if let RuleMod::AttackTax {
            generic,
            cost,
            mana_symbol,
            dynamic_amount,
            affected,
            defending,
        } = &rm.rule
        {
            let _ = mana_symbol;
            let def_matches = match defending.as_deref() {
                Some("controller" | "you") => defender == rm.controller,
                _ => defender == rm.controller,
            };
            if !def_matches {
                continue;
            }
            if let Some(aff) = affected
                && let Some(src_id) = rm.source
                && !affected_matches(state, cache, src_id, rm.controller, aff, attacker)
            {
                continue;
            }
            if let Some(g) = generic {
                total.generic = total.generic.saturating_add(u32::from(*g));
            }
            if let Some(c) = cost {
                add_mana_cost(&mut total, c);
            }
            if let Some(amt) = dynamic_amount {
                let ctx = match rm.source {
                    Some(src) => EvalContext::for_source(rm.controller, src),
                    None => EvalContext::new(rm.controller),
                }
                .with_board(BoardView::Cached(cache));
                let n = evaluate_amount(state, amt, &ctx).max(0) as u32;
                total.generic = total.generic.saturating_add(n);
            }
        }
    }
    total
}

fn add_mana_cost(acc: &mut ManaCost, add: &ManaCost) {
    acc.generic = acc.generic.saturating_add(add.generic);
    acc.pips.extend(add.pips.iter().cloned());
}

fn assign_damage_by_toughness(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    let mut yes = false;
    for_each_combat_static(state, cache, |src_id, src_ctrl, st| {
        if yes {
            return;
        }
        if let StaticBody::AssignCombatDamageByToughness { affected } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            yes = true;
        }
    });
    yes
}

fn has_super_trample(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    let mut yes = false;
    for_each_combat_static(state, cache, |src_id, src_ctrl, st| {
        if yes {
            return;
        }
        if let StaticBody::CombatRule {
            affected,
            rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
        } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            yes = true;
        }
    });
    yes
}

fn blocker_max_block_count(state: &GameState, cache: &LayerCache, id: ObjectId) -> Option<u16> {
    let mut max_blocks: Option<u16> = Some(1);
    for_each_combat_static(state, cache, |src_id, src_ctrl, st| {
        let Some(cur) = max_blocks else {
            return;
        };
        if let StaticBody::CombatRule { affected, rule } = &st.body
            && affected_matches(state, cache, src_id, src_ctrl, affected, id)
        {
            match rule {
                CombatRuleSpec::AdditionalBlockCount { count: None } => {
                    max_blocks = None;
                }
                CombatRuleSpec::AdditionalBlockCount { count: Some(c) } => {
                    max_blocks = Some(cur.saturating_add(*c));
                }
                CombatRuleSpec::AdditionalBlockCountDynamic { amount } => {
                    let ctx = EvalContext::for_source(src_ctrl, src_id)
                        .with_board(BoardView::Cached(cache));
                    let c = evaluate_amount(state, amount, &ctx).max(0) as u16;
                    max_blocks = Some(cur.saturating_add(c));
                }
                _ => {}
            }
        }
    });
    max_blocks
}

fn creature_count(state: &GameState, cache: &LayerCache, player: PlayerId) -> usize {
    state.player(player).map_or(0, |p| {
        p.battlefield
            .iter()
            .filter(|&&id| {
                state.object(id).is_some_and(|o| !o.is_phased_out())
                    && cache
                        .get(id)
                        .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            })
            .count()
    })
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

#[inline]
fn resolve_keyword_id_ci(db: &CardDatabase, lower_name: &str) -> Option<KeywordId> {
    if let Some(id) = db.keyword_id(lower_name) {
        return Some(id);
    }
    db.tables
        .keywords
        .iter()
        .enumerate()
        .find(|(_, k)| k.eq_ignore_ascii_case(lower_name))
        .map(|(i, _)| KeywordId(i as u16))
}

#[inline]
fn cache_has_keyword_id(cache: &LayerCache, id: ObjectId, kw: KeywordId) -> bool {
    let Some(eval) = cache.get(id) else {
        return false;
    };
    if cache
        .extra(id)
        .is_some_and(|ex| ex.cant_have_keywords.contains(&kw))
    {
        return false;
    }
    eval.keywords.contains(&kw)
}

#[inline]
fn is_combat_static_body(body: &StaticBody) -> bool {
    matches!(
        body,
        StaticBody::CombatRule { .. }
            | StaticBody::Restriction { .. }
            | StaticBody::AssignCombatDamageByToughness { .. }
    )
}

#[inline]
fn card_has_combat_static(db: &CardDatabase, card_def: CardDefId) -> bool {
    let Some(card) = db.get(card_def) else {
        return false;
    };
    card.faces.iter().any(|face| {
        face.abilities.iter().any(
            |ab| matches!(ab, AbilityDef::Static(st) if is_combat_static_body(&st.static_def.body)),
        )
    })
}

fn for_each_combat_static<'a>(
    state: &'a GameState,
    cache: &'a LayerCache,
    mut visit: impl FnMut(ObjectId, PlayerId, &'a mtg_ir::StaticDef),
) {
    let db = state.db();
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

            if !lost_innate && !obj.is_face_down() {
                let rare = state.rare(perm_id);
                let base_id = rare
                    .and_then(|r| r.copiable.as_deref())
                    .map_or(obj.card_def, |c| c.card_def);
                let has_potential_innate = card_has_combat_static(db, base_id)
                    || rare.is_some_and(|r| {
                        if !r.merged_components.is_empty() {
                            r.merged_components
                                .iter()
                                .skip(1)
                                .any(|mc| !mc.is_token && card_has_combat_static(db, mc.card_def))
                        } else {
                            r.copiable.as_deref().is_some_and(|c| {
                                c.merged_components
                                    .iter()
                                    .skip(1)
                                    .any(|&cid| card_has_combat_static(db, cid))
                            })
                        }
                    });
                if has_potential_innate {
                    let opts = face_opts_for_game_object(obj, rare, None);
                    let base_card = db.get(base_id);
                    let normal_single_face = base_card
                        .is_some_and(|c| c.layout == CardLayout::Normal && !c.faces.is_empty())
                        && rare
                            .is_none_or(|r| r.copiable.is_none() && r.merged_components.is_empty());
                    if normal_single_face {
                        let ctx = EvalContext::for_source(ctrl, perm_id)
                            .with_board(BoardView::Cached(cache));
                        for_each_active_ability_with_card_on_faces(
                            db,
                            obj.card_def,
                            &opts,
                            &[0],
                            |cond| evaluate_condition(state, cond, &ctx),
                            |ab| {
                                if let AbilityDef::Static(st) = ab.ability
                                    && is_combat_static_body(&st.static_def.body)
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
                    } else if let Some(view) = resolve_object_faces(db, obj.card_def, &opts) {
                        let ctx = EvalContext::for_source(ctrl, perm_id)
                            .with_board(BoardView::Cached(cache));
                        for_each_active_ability_with_card_on_faces(
                            db,
                            obj.card_def,
                            &opts,
                            &view.active_face_indices,
                            |cond| evaluate_condition(state, cond, &ctx),
                            |ab| {
                                if let AbilityDef::Static(st) = ab.ability
                                    && is_combat_static_body(&st.static_def.body)
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
                }
            }
            if let Some(ex) = extras
                && !ex.granted_abilities.is_empty()
            {
                let mut ctx_opt: Option<EvalContext<'a>> = None;
                for granted in &ex.granted_abilities {
                    match granted {
                        GrantedAbilityEntry::Inline {
                            ability,
                            grant_controller,
                            ..
                        } => {
                            if let AbilityDef::Static(st) = ability.as_ref()
                                && is_combat_static_body(&st.static_def.body)
                                && st.static_def.condition.as_deref().is_none_or(|c| {
                                    let ctx = ctx_opt.get_or_insert_with(|| {
                                        EvalContext::for_source(ctrl, perm_id)
                                            .with_board(BoardView::Cached(cache))
                                    });
                                    evaluate_condition(state, c, ctx)
                                })
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
                            if let Some(card) = db.get(*card_def)
                                && let Some(face) = card.faces.get(usize::from(*face_index))
                                && let Some(AbilityDef::Static(st)) =
                                    face.abilities.get(*ability_index)
                                && is_combat_static_body(&st.static_def.body)
                                && st.static_def.condition.as_deref().is_none_or(|c| {
                                    let ctx = ctx_opt.get_or_insert_with(|| {
                                        EvalContext::for_source(ctrl, perm_id)
                                            .with_board(BoardView::Cached(cache))
                                    });
                                    evaluate_condition(state, c, ctx)
                                })
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
    use crate::config::GameConfig;
    use crate::zones::CreateTokenOptions;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, SupertypeMask, TokenSpec,
    };
    use std::sync::Arc;

    fn synth_combat_state() -> (
        GameState,
        mtg_ir::KeywordId,
        mtg_ir::KeywordId,
        mtg_ir::KeywordId,
        mtg_ir::KeywordId,
        mtg_ir::KeywordId,
        mtg_ir::KeywordId,
    ) {
        let mut tables = InternTables::default();
        let kw_fs = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("first strike".into());
        let kw_dt = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("deathtouch".into());
        let kw_tr = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("trample".into());
        let kw_ind = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("indestructible".into());
        let kw_ll = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("lifelink".into());
        let kw_men = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("menace".into());

        let land = CardDef {
            oracle_id: "synth_combat_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Combat Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Combat Plains".into(),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);
        (state, kw_fs, kw_dt, kw_tr, kw_ind, kw_ll, kw_men)
    }

    fn spawn_creature(
        state: &mut GameState,
        ctrl: PlayerId,
        name: &str,
        power: i32,
        toughness: i32,
        kws: &[mtg_ir::KeywordId],
    ) -> ObjectId {
        let mut kw_sv = SmallVec::new();
        kw_sv.extend_from_slice(kws);
        let id = crate::zones::create_token_raw(
            state,
            ctrl,
            TokenSpec {
                name: name.into(),
                power,
                toughness,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: kw_sv,
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();
        if let Some(obj) = state.object_mut(id) {
            obj.flags.remove(ObjectFlags::SUMMONING_SICK);
        }
        id
    }

    #[test]
    fn synth_negative_power_clamped_and_deathtouch_trample_over_indestructible() {
        let (mut state, _fs, kw_dt, kw_tr, kw_ind, _ll, _men) = synth_combat_state();
        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Apex", 5, 5, &[kw_dt, kw_tr]);
        let neg_atk = spawn_creature(&mut state, PlayerId(0), "Synth Minus", -2, 3, &[]);
        let blk = spawn_creature(&mut state, PlayerId(1), "Synth Shield", 0, 5, &[kw_ind]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: neg_atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();

        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: blk,
                attacker: atk,
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();

        execute_combat_damage_step(&mut state, false).unwrap();
        // Deathtouch + Trample deals only 1 lethal to indestructible blocker and tramples 4 to P1; neg_atk deals 0 (`CR 510.1a`).
        assert_eq!(state.object(blk).unwrap().damage_marked, 1);
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 16);
    }

    #[test]
    fn synth_first_strike_sba_prevents_blocker_damage_and_lifelink_simultaneous() {
        let (mut state, kw_fs, _dt, _tr, _ind, kw_ll, _men) = synth_combat_state();
        let atk = spawn_creature(
            &mut state,
            PlayerId(0),
            "Synth Striker",
            3,
            2,
            &[kw_fs, kw_ll],
        );
        let blk = spawn_creature(&mut state, PlayerId(1), "Synth Guard", 4, 3, &[]);
        state.player_mut(PlayerId(0)).unwrap().life = 10;

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();

        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: blk,
                attacker: atk,
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();

        assert!(any_combatant_has_first_strike(&state));
        execute_combat_damage_step(&mut state, true).unwrap();
        // Blocker died in First Strike SBAs; P0 gained 3 life via Lifelink; atk stays alive.
        assert!(
            state
                .object(atk)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        );
        assert!(state.player(PlayerId(1)).unwrap().battlefield.is_empty());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 13);

        // Regular combat damage step: blocker is gone so atk takes 0 damage and doesn't double-strike.
        execute_combat_damage_step(&mut state, false).unwrap();
        assert_eq!(state.object(atk).unwrap().damage_marked, 0);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 13);
    }

    #[test]
    fn synth_menace_requires_two_blockers() {
        let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Brute", 3, 3, &[kw_men]);
        let b1 = spawn_creature(&mut state, PlayerId(1), "Synth Elf One", 1, 1, &[]);
        let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Elf Two", 1, 1, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();

        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: atk,
            },
        )
        .unwrap();
        assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_err());

        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b2,
                attacker: atk,
            },
        )
        .unwrap();
        assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
    }

    #[test]
    fn synth_combat_damage_life_lost_this_turn_no_double_count() {
        let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Bear", 3, 3, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();

        execute_combat_damage_step(&mut state, false).unwrap();
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 17);
        assert_eq!(state.player(PlayerId(1)).unwrap().life_lost_this_turn, 3);
        assert_eq!(
            crate::events::TurnEventHistory::get_u32(
                &state.turn_history().life_lost_this_turn,
                PlayerId(1)
            ),
            3
        );
    }

    #[test]
    fn synth_trample_over_planeswalkers_unblocked_and_blocked() {
        let mut tables = InternTables::default();
        let kw_tr_pw = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("trample over planeswalkers".into());
        let loyalty_cid = mtg_ir::CounterId(tables.counters.len() as u16);
        tables.counters.push("loyalty".into());
        let land = CardDef {
            oracle_id: "synth_pw_trample_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Pw Trample Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Pw Trample Land".into(),
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
        let pw_def = CardDef {
            oracle_id: "synth_pw_card".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Planeswalker".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Planeswalker".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::PLANESWALKER,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: Some(3),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land, pw_def],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 71).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        let pw_h = state.player(PlayerId(1)).unwrap().hand[0];
        state.object_mut(pw_h).unwrap().card_def = CardDefId(1);
        let pw = match crate::zones::move_object(
            &mut state,
            pw_h,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected PW on BF"),
        };
        state.set_counter(pw, loyalty_cid, 3);

        // (a) Unblocked 5/5 with trample over planeswalkers deals 3 to PW and 2 to P1.
        let atk5 = spawn_creature(&mut state, PlayerId(0), "Synth Crusher", 5, 5, &[kw_tr_pw]);
        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk5,
                target: AttackTarget::Planeswalker { id: pw },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();
        execute_combat_damage_step(&mut state, false).unwrap();
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 18);
        assert!(state.object(pw).is_none());

        // (b) Blocked 6/6 with trample over planeswalkers blocked by 1/1 -> 1 to blocker, 3 to PW, 2 to P1.
        let pw_h2 = state.player(PlayerId(1)).unwrap().hand[1];
        state.object_mut(pw_h2).unwrap().card_def = CardDefId(1);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let pw2 = match crate::zones::move_object(
            &mut state,
            pw_h2,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected PW2 on BF"),
        };
        state.set_counter(pw2, loyalty_cid, 3);
        state.player_mut(PlayerId(1)).unwrap().life = 20;
        let atk6 = spawn_creature(&mut state, PlayerId(0), "Synth Titan", 6, 6, &[kw_tr_pw]);
        let blk1 = spawn_creature(&mut state, PlayerId(1), "Synth Chump", 1, 1, &[]);
        end_combat_phase_immediately(&mut state).unwrap();

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk6,
                target: AttackTarget::Planeswalker { id: pw2 },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: blk1,
                attacker: atk6,
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();
        execute_combat_damage_step(&mut state, false).unwrap();
        assert!(state.object(blk1).is_none());
        assert!(state.object(pw2).is_none());
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 18);
    }

    #[test]
    fn synth_lifelink_multiple_life_gain_replacements_pause_and_resume() {
        let mut tables = InternTables::default();
        let kw_ll = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("lifelink".into());
        let land = CardDef {
            oracle_id: "synth_ll_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Ll Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Ll Land".into(),
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
        let make_lg_rep = |name: &str, double: bool, add_n: Option<i32>| CardDef {
            oracle_id: name.to_ascii_lowercase(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
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
                abilities: vec![AbilityDef::Replacement(mtg_ir::ReplacementAbility {
                    replacement: mtg_ir::ReplacementDef {
                        replaces: mtg_ir::EventPattern {
                            event: mtg_ir::EventKind::GainLife,
                            filter: mtg_ir::EventFilter {
                                player: Some(mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::You,
                                }),
                                ..Default::default()
                            },
                        },
                        with: mtg_ir::ReplacementWith::LifeGainScale {
                            spec: mtg_ir::LifeGainScaleSpec {
                                double,
                                add: add_n.map(|n| Box::new(mtg_ir::Amount::Const { n })),
                            },
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
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
                cards: vec![
                    land,
                    make_lg_rep("Synth Life Doubler", true, None),
                    make_lg_rep("Synth Life Adder", false, Some(1)),
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            72,
        )
        .unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        for (idx, def_id) in [CardDefId(1), CardDefId(2)].into_iter().enumerate() {
            let h = state.player(PlayerId(0)).unwrap().hand[idx];
            state.object_mut(h).unwrap().card_def = def_id;
            state.mark_dirty(DirtyMask::TOPOLOGY);
            crate::zones::move_object(
                &mut state,
                h,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Angel", 3, 3, &[kw_ll]);
        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();
        apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();

        let CombatOutcome::Paused { prompt_id, chooser } =
            execute_combat_damage_step(&mut state, false).unwrap()
        else {
            panic!("expected ChooseReplacement pause for lifelink gain");
        };
        assert_eq!(chooser, PlayerId(0));

        let mut restored = GameState::from_snapshot(db, state.to_snapshot());
        crate::choices::apply_choice_answer(
            &mut restored,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        assert!(restored.combat().pending_damage.is_none());
        assert_eq!(restored.decision(), &DecisionState::Priority);
        assert_eq!(restored.player(PlayerId(0)).unwrap().life, 27);
        assert_eq!(restored.player(PlayerId(1)).unwrap().life, 17);
    }

    fn synth_combat_3p_state(seed: u64) -> GameState {
        let land = CardDef {
            oracle_id: "synth_combat_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Combat Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Combat Plains".into(),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 3;
        let mut state3 =
            GameState::new(db, cfg, &[deck.clone(), deck.clone(), deck], seed).unwrap();
        state3.set_phase(mtg_ir::GamePhase::BeginCombat);
        state3.set_decision(DecisionState::Priority);
        state3
    }

    #[test]
    fn synth_is_goaded_static_avoids_goader_when_able() {
        let mut state3 = synth_combat_3p_state(73);

        let bear = spawn_creature(&mut state3, PlayerId(0), "Synth Bear", 2, 2, &[]);
        let eff_id = state3.allocate_continuous_effect_id();
        let ts = state3.allocate_timestamp();
        state3.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: eff_id,
            source: None,
            controller: PlayerId(1),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                bear
            ]),
            modifier: mtg_ir::LayerMod::GrantAbility {
                ability: Box::new(AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        condition: None,
                        body: StaticBody::CombatRule {
                            affected: Affected::SelfObj,
                            rule: CombatRuleSpec::IsGoaded,
                        },
                    },
                    span: Default::default(),
                })),
            },
            duration: mtg_ir::Duration::UntilYourNextTurn,
            timestamp: ts,
            expires_turn: Some(state3.turn_serial() + 3),
            expires_on_player: Some(PlayerId(1)),
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        assert!(!can_creature_attack(
            &state3,
            bear,
            AttackTarget::Player { id: PlayerId(1) }
        ));
        assert!(can_creature_attack(
            &state3,
            bear,
            AttackTarget::Player { id: PlayerId(2) }
        ));
    }

    #[test]
    fn synth_must_attack_next_combat_and_must_attack_player() {
        let mut state = synth_combat_3p_state(74);

        let bear = spawn_creature(&mut state, PlayerId(0), "Synth Bear", 2, 2, &[]);
        state
            .rare_turn_state_mut()
            .must_attack_next_combat
            .push(bear);
        state
            .rare_turn_state_mut()
            .must_attack_player_this_turn
            .push((bear, PlayerId(2)));

        enter_declare_attackers_step(&mut state).unwrap();
        // Confirming with 0 attackers fails because bear must attack.
        assert!(apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_err());

        // Attacking P1 fails because bear must attack P2 if able.
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: bear,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        assert!(apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_err());

        // Switching target to P2 succeeds.
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::RemoveAttacker { attacker: bear },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: bear,
                target: AttackTarget::Player { id: PlayerId(2) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();

        end_combat_phase_immediately(&mut state).unwrap();
        assert!(
            state
                .rare_turn_state()
                .is_none_or(|r| !r.must_attack_next_combat.contains(&bear))
        );
    }

    fn grant_object_combat_static(
        state: &mut GameState,
        controller: PlayerId,
        target: ObjectId,
        body: StaticBody,
    ) {
        let eff_id = state.allocate_continuous_effect_id();
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: eff_id,
            source: None,
            controller,
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                target
            ]),
            modifier: mtg_ir::LayerMod::GrantAbility {
                ability: Box::new(AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        condition: None,
                        body,
                    },
                    span: Default::default(),
                })),
            },
            duration: mtg_ir::Duration::UntilYourNextTurn,
            timestamp: ts,
            expires_turn: Some(state.turn_serial() + 3),
            expires_on_player: Some(controller),
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
    }

    fn make_session_creature_card(
        oracle_id: &str,
        name: &str,
        kws: &[mtg_ir::KeywordId],
    ) -> CardDef {
        let mut abilities = Vec::new();
        for &kw in kws {
            abilities.push(AbilityDef::Keyword(mtg_ir::KeywordAbility {
                keyword: mtg_ir::KeywordDef {
                    keyword: kw,
                    cost: None,
                    param: None,
                    n: None,
                    cost_defs: vec![],
                    kicker_costs: vec![],
                    equip_filter: None,
                    enchant_filter: None,
                    enchant_player: None,
                    affinity_filter: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                    awaken_n: None,
                    emerge_filter: None,
                    champion_filter: None,
                    devour_filter: None,
                    devour_x: false,
                    equip_allow_planeswalker: false,
                    backup_ability_start_index: None,
                    gift_kind: None,
                    craft_materials: vec![],
                    craft_any_number: false,
                    craft_share_card_type: false,
                    splice_filter: None,
                    impending_n: None,
                    landwalk_filter: None,
                    suspend_x: false,
                    equip_cost_reduction: None,
                    dynamic_amount: None,
                },
                id: None,
                span: Default::default(),
            }));
        }
        CardDef {
            oracle_id: oracle_id.into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities,
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        }
    }

    fn advance_session_to_declare_blockers(
        session: &mut crate::session::MatchSession,
        p0_cast_targets: &[CardDefId],
        p1_cast_count: usize,
    ) {
        use crate::action::MacroAction;
        use crate::session::DecisionKind;

        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        // Turn 1 (P0 main1): cast requested 0-mana creatures from hand.
        while !(session.state().turn_number() == 1
            && session.state().phase() == mtg_ir::GamePhase::Main1
            && session.current_actor() == Some(PlayerId(0)))
        {
            let actor = session.current_actor().unwrap();
            session.apply_action(actor, Action::PassPriority).unwrap();
        }
        for &def in p0_cast_targets {
            let oid = session.state().players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| {
                    session
                        .state()
                        .object(id)
                        .is_some_and(|o| o.card_def == def)
                })
                .expect("expected card in P0 hand");
            session
                .apply_macro_action(
                    PlayerId(0),
                    MacroAction::CastSpell {
                        object: oid,
                        option_id: 0,
                        from_zone: Zone::Hand,
                        modes: vec![],
                        x_value: None,
                        targets: vec![],
                    },
                )
                .unwrap();
            while !session.state().stack().is_empty() {
                let actor = session.current_actor().unwrap();
                session.apply_action(actor, Action::PassPriority).unwrap();
            }
        }

        // Advance to Turn 2 (P1 main1): cast `p1_cast_count` 0-mana blockers.
        while !(session.state().turn_number() == 2
            && session.state().phase() == mtg_ir::GamePhase::Main1
            && session.current_actor() == Some(PlayerId(1)))
        {
            let actor = session.current_actor().unwrap();
            let act = match session.decision_info().state_kind {
                DecisionKind::DeclareAttackers => Action::ConfirmAttackers,
                DecisionKind::DeclareBlockers => Action::ConfirmBlockers,
                DecisionKind::Prompt => {
                    if let DecisionState::Prompt {
                        request:
                            ChoiceRequest::ChooseObjects {
                                prompt_id,
                                candidates,
                                ..
                            },
                    } = session.state().decision()
                    {
                        let ans = if let Some(&id) = candidates.first() {
                            ChoiceAnswer::PickObject { id }
                        } else {
                            ChoiceAnswer::DonePicking
                        };
                        Action::Answer {
                            prompt_id: *prompt_id,
                            answer: ans,
                        }
                    } else {
                        Action::PassPriority
                    }
                }
                _ => Action::PassPriority,
            };
            session.apply_action(actor, act).unwrap();
        }
        for _ in 0..p1_cast_count {
            let oid = session.state().players()[1].hand[0];
            session
                .apply_macro_action(
                    PlayerId(1),
                    MacroAction::CastSpell {
                        object: oid,
                        option_id: 0,
                        from_zone: Zone::Hand,
                        modes: vec![],
                        x_value: None,
                        targets: vec![],
                    },
                )
                .unwrap();
            while !session.state().stack().is_empty() {
                let actor = session.current_actor().unwrap();
                session.apply_action(actor, Action::PassPriority).unwrap();
            }
        }

        // Advance to Turn 3 DeclareAttackers for P0, attack with all P0 creatures, reach DeclareBlockers.
        while !(session.state().turn_number() == 3
            && session.decision_info().state_kind == DecisionKind::DeclareAttackers)
        {
            let actor = session.current_actor().unwrap();
            let act = match session.decision_info().state_kind {
                DecisionKind::DeclareAttackers => Action::ConfirmAttackers,
                DecisionKind::DeclareBlockers => Action::ConfirmBlockers,
                DecisionKind::Prompt => {
                    if let DecisionState::Prompt {
                        request:
                            ChoiceRequest::ChooseObjects {
                                prompt_id,
                                candidates,
                                ..
                            },
                    } = session.state().decision()
                    {
                        let ans = if let Some(&id) = candidates.first() {
                            ChoiceAnswer::PickObject { id }
                        } else {
                            ChoiceAnswer::DonePicking
                        };
                        Action::Answer {
                            prompt_id: *prompt_id,
                            answer: ans,
                        }
                    } else {
                        Action::PassPriority
                    }
                }
                _ => Action::PassPriority,
            };
            session.apply_action(actor, act).unwrap();
        }

        let assignments: Vec<(ObjectId, AttackTarget)> = session.state().players()[0]
            .battlefield
            .iter()
            .copied()
            .map(|id| (id, AttackTarget::Player { id: PlayerId(1) }))
            .collect();
        session
            .apply_macro_action(PlayerId(0), MacroAction::DeclareAttackers { assignments })
            .unwrap();
        while session.decision_info().state_kind != DecisionKind::DeclareBlockers {
            let actor = session.current_actor().unwrap();
            session.apply_action(actor, Action::PassPriority).unwrap();
        }
    }

    #[test]
    fn synth_menace_single_blocker_no_assign_and_no_soft_lock() {
        let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Raider", 3, 2, &[kw_men]);
        let guard = spawn_creature(&mut state, PlayerId(1), "Synth Guard", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        assert_eq!(
            legal_combat_actions(&state, PlayerId(1)),
            vec![Action::ConfirmBlockers]
        );
        assert!(
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: guard,
                    attacker: atk,
                },
            )
            .is_err()
        );
        assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());

        // MatchSession::legal_actions returns [ConfirmBlockers, Concede] (no soft-lock).
        let mut tables = InternTables::default();
        let kw_men2 = mtg_ir::KeywordId(0);
        tables.keywords.push("menace".into());
        let raider = make_session_creature_card("s_raider", "Synth Raider", &[kw_men2]);
        let guard_def = make_session_creature_card("s_guard", "Synth Guard", &[]);
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![raider, guard_def],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 40],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 40],
            sideboard: vec![],
        };
        let mut session = crate::session::MatchSession::new(
            db,
            GameConfig::standard_1v1(),
            vec![p0_deck, p1_deck],
            5,
        )
        .unwrap();
        advance_session_to_declare_blockers(&mut session, &[CardDefId(0)], 1);
        assert_eq!(
            session.legal_actions(PlayerId(1)),
            vec![Action::ConfirmBlockers, Action::Concede]
        );
    }

    #[test]
    fn synth_menace_flying_two_creatures_only_one_can_block() {
        let mut tables = InternTables::default();
        let kw_men = mtg_ir::KeywordId(0);
        tables.keywords.push("menace".into());
        let kw_fly = mtg_ir::KeywordId(1);
        tables.keywords.push("flying".into());
        let land = CardDef {
            oracle_id: "s_land_mf".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 12).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        let sky_raider = spawn_creature(
            &mut state,
            PlayerId(0),
            "Synth Sky Raider",
            3,
            2,
            &[kw_men, kw_fly],
        );
        let eagle = spawn_creature(&mut state, PlayerId(1), "Synth Eagle", 2, 2, &[kw_fly]);
        let _bear = spawn_creature(&mut state, PlayerId(1), "Synth Bear", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: sky_raider,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        assert_eq!(
            legal_combat_actions(&state, PlayerId(1)),
            vec![Action::ConfirmBlockers]
        );
        assert!(
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: eagle,
                    attacker: sky_raider,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn synth_menace_plus_normal_attacker_shared_pool_no_trap() {
        let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
        let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Menacer", 3, 2, &[kw_men]);
        let a2 = spawn_creature(&mut state, PlayerId(0), "Synth Normal", 2, 2, &[]);
        let b1 = spawn_creature(&mut state, PlayerId(1), "Synth Guard One", 2, 2, &[]);
        let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Guard Two", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        for atk in [a1, a2] {
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
        }
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: a2,
            },
        )
        .unwrap();
        let legal = legal_combat_actions(&state, PlayerId(1));
        assert!(!legal.contains(&Action::AssignBlocker {
            blocker: b2,
            attacker: a1,
        }));
        assert!(legal.contains(&Action::AssignBlocker {
            blocker: b2,
            attacker: a2,
        }));
        assert!(legal.contains(&Action::ConfirmBlockers));
        assert!(
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b2,
                    attacker: a1,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn synth_two_menace_attackers_three_vs_four_blockers() {
        // 3 blockers vs 2 Menace attackers: after B1 -> A1, B2/B3 cannot start blocking A2.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
            let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 1", 3, 2, &[kw_men]);
            let a2 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 2", 3, 2, &[kw_men]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth B1", 2, 2, &[]);
            let b2 = spawn_creature(&mut state, PlayerId(1), "Synth B2", 2, 2, &[]);
            let b3 = spawn_creature(&mut state, PlayerId(1), "Synth B3", 2, 2, &[]);

            enter_declare_attackers_step(&mut state).unwrap();
            for atk in [a1, a2] {
                apply_combat_action(
                    &mut state,
                    PlayerId(0),
                    &Action::AssignAttacker {
                        attacker: atk,
                        target: AttackTarget::Player { id: PlayerId(1) },
                    },
                )
                .unwrap();
            }
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();

            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: a1,
                },
            )
            .unwrap();
            let legal1 = legal_combat_actions(&state, PlayerId(1));
            assert!(!legal1.contains(&Action::ConfirmBlockers));
            assert!(!legal1.contains(&Action::AssignBlocker {
                blocker: b2,
                attacker: a2,
            }));
            assert!(!legal1.contains(&Action::AssignBlocker {
                blocker: b3,
                attacker: a2,
            }));

            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b2,
                    attacker: a1,
                },
            )
            .unwrap();
            let legal2 = legal_combat_actions(&state, PlayerId(1));
            assert!(legal2.contains(&Action::ConfirmBlockers));
            assert!(!legal2.contains(&Action::AssignBlocker {
                blocker: b3,
                attacker: a2,
            }));
            apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();
        }

        // 4 blockers vs 2 Menace attackers: B1 -> A1, then B2 -> A2 IS legal, and completes.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
            let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 1", 3, 2, &[kw_men]);
            let a2 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 2", 3, 2, &[kw_men]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth B1", 2, 2, &[]);
            let b2 = spawn_creature(&mut state, PlayerId(1), "Synth B2", 2, 2, &[]);
            let b3 = spawn_creature(&mut state, PlayerId(1), "Synth B3", 2, 2, &[]);
            let b4 = spawn_creature(&mut state, PlayerId(1), "Synth B4", 2, 2, &[]);

            enter_declare_attackers_step(&mut state).unwrap();
            for atk in [a1, a2] {
                apply_combat_action(
                    &mut state,
                    PlayerId(0),
                    &Action::AssignAttacker {
                        attacker: atk,
                        target: AttackTarget::Player { id: PlayerId(1) },
                    },
                )
                .unwrap();
            }
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();

            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: a1,
                },
            )
            .unwrap();
            assert!(
                legal_combat_actions(&state, PlayerId(1)).contains(&Action::AssignBlocker {
                    blocker: b2,
                    attacker: a2,
                })
            );
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b2,
                    attacker: a2,
                },
            )
            .unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b3,
                    attacker: a1,
                },
            )
            .unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b4,
                    attacker: a2,
                },
            )
            .unwrap();
            assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
        }
    }

    #[test]
    fn synth_menace_with_additional_block_count() {
        let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
        let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 1", 3, 2, &[kw_men]);
        let a2 = spawn_creature(&mut state, PlayerId(0), "Synth Raider 2", 3, 2, &[kw_men]);
        let b1 = spawn_creature(&mut state, PlayerId(1), "Synth TwinGuard", 2, 4, &[]);
        let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Guard", 2, 2, &[]);

        grant_object_combat_static(
            &mut state,
            PlayerId(1),
            b1,
            StaticBody::CombatRule {
                affected: Affected::SelfObj,
                rule: CombatRuleSpec::AdditionalBlockCount { count: Some(1) },
            },
        );

        enter_declare_attackers_step(&mut state).unwrap();
        for atk in [a1, a2] {
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
        }
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: a1,
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b2,
                attacker: a1,
            },
        )
        .unwrap();
        assert!(
            !legal_combat_actions(&state, PlayerId(1)).contains(&Action::AssignBlocker {
                blocker: b1,
                attacker: a2,
            })
        );

        grant_object_combat_static(
            &mut state,
            PlayerId(1),
            b2,
            StaticBody::CombatRule {
                affected: Affected::SelfObj,
                rule: CombatRuleSpec::AdditionalBlockCount { count: Some(1) },
            },
        );
        assert!(
            legal_combat_actions(&state, PlayerId(1)).contains(&Action::AssignBlocker {
                blocker: b1,
                attacker: a2,
            })
        );
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: a2,
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b2,
                attacker: a2,
            },
        )
        .unwrap();
        assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
    }

    #[test]
    fn synth_cant_block_alone_global_max_attacker_max_and_block_tax_traps() {
        // 1. CantBlockAlone sole blocker omitted; with partner allowed.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
            let atk = spawn_creature(&mut state, PlayerId(0), "Synth Brute", 2, 2, &[]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth Shy", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(1),
                b1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CantBlockAlone,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();
            assert_eq!(
                legal_combat_actions(&state, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );
            let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Buddy", 2, 2, &[]);
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: atk,
                },
            )
            .unwrap();
            let legal = legal_combat_actions(&state, PlayerId(1));
            assert!(!legal.contains(&Action::ConfirmBlockers));
            assert!(legal.contains(&Action::AssignBlocker {
                blocker: b2,
                attacker: atk,
            }));
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b2,
                    attacker: atk,
                },
            )
            .unwrap();
            assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
        }

        // 2. GlobalMaxBlockers(1) with 2 blockers -> 2nd unique blocker omitted.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
            let atk = spawn_creature(&mut state, PlayerId(0), "Synth Brute", 2, 2, &[]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth B1", 2, 2, &[]);
            let b2 = spawn_creature(&mut state, PlayerId(1), "Synth B2", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                atk,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::GlobalMaxBlockers { max: 1 },
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: atk,
                },
            )
            .unwrap();
            assert_eq!(
                legal_combat_actions(&state, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );
            assert!(
                apply_combat_action(
                    &mut state,
                    PlayerId(1),
                    &Action::AssignBlocker {
                        blocker: b2,
                        attacker: atk,
                    },
                )
                .is_err()
            );
        }

        // 3. MaxBlockers(1) on attacker with 2 blockers -> 2nd blocker on that attacker omitted.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
            let atk = spawn_creature(&mut state, PlayerId(0), "Synth Stalker", 2, 2, &[]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth B1", 2, 2, &[]);
            let b2 = spawn_creature(&mut state, PlayerId(1), "Synth B2", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                atk,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::MaxBlockers { max: 1 },
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: atk,
                },
            )
            .unwrap();
            assert_eq!(
                legal_combat_actions(&state, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );
            assert!(
                apply_combat_action(
                    &mut state,
                    PlayerId(1),
                    &Action::AssignBlocker {
                        blocker: b2,
                        attacker: atk,
                    },
                )
                .is_err()
            );
        }

        // 4. BlockTax(1) with 1 mana vs Menace -> AssignBlocker omitted; vs normal -> 1st offered, 2nd omitted, confirms paying {1}.
        {
            let mut tables = InternTables::default();
            let kw_men = mtg_ir::KeywordId(0);
            tables.keywords.push("menace".into());
            let land = CardDef {
                oracle_id: "s_land_bt".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Plains".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Plains".into(),
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
            let tax_ench = CardDef {
                oracle_id: "s_block_tax".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Toll".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Toll".into(),
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
                    abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::RuleModifier {
                                rule: RuleMod::BlockTax {
                                    generic: Some(1),
                                    dynamic_amount: None,
                                    affected: None,
                                },
                                who: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::Opponent,
                                },
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
                    cards: vec![land, tax_ench],
                    decks: vec![],
                })
                .unwrap(),
            );
            let p0_deck = DeckSpec {
                commanders: vec![],
                mainboard: {
                    let mut m = vec![CardDefId(1); 2];
                    m.extend(vec![CardDefId(0); 18]);
                    m
                },
                sideboard: vec![],
            };
            let p1_deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 20],
                sideboard: vec![],
            };

            // (a) BlockTax(1) with 1 mana vs Menace -> AssignBlocker omitted
            let mut state_m = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                31,
            )
            .unwrap();
            let toll_id = state_m.players()[0]
                .hand
                .iter()
                .chain(state_m.players()[0].library.iter())
                .copied()
                .find(|&id| {
                    state_m
                        .object(id)
                        .is_some_and(|o| o.card_def == CardDefId(1))
                })
                .unwrap();
            crate::zones::move_object_raw(
                &mut state_m,
                toll_id,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
            state_m.set_phase(mtg_ir::GamePhase::BeginCombat);
            state_m.set_decision(DecisionState::Priority);
            state_m.player_mut(PlayerId(1)).unwrap().mana_pool[5] = 1;
            state_m.mark_dirty(DirtyMask::PLAYER_STATE);
            let atk_m = spawn_creature(&mut state_m, PlayerId(0), "Synth Raider", 3, 2, &[kw_men]);
            let _b1_m = spawn_creature(&mut state_m, PlayerId(1), "Synth B1", 2, 2, &[]);
            let _b2_m = spawn_creature(&mut state_m, PlayerId(1), "Synth B2", 2, 2, &[]);
            enter_declare_attackers_step(&mut state_m).unwrap();
            apply_combat_action(
                &mut state_m,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk_m,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state_m, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state_m).unwrap();
            assert_eq!(
                legal_combat_actions(&state_m, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );

            // (b) BlockTax(1) with 1 mana vs normal attacker -> 1st offered, 2nd omitted, confirms paying {1}
            let mut state_n =
                GameState::new(db, GameConfig::standard_1v1(), &[p0_deck, p1_deck], 32).unwrap();
            let toll_id2 = state_n.players()[0]
                .hand
                .iter()
                .chain(state_n.players()[0].library.iter())
                .copied()
                .find(|&id| {
                    state_n
                        .object(id)
                        .is_some_and(|o| o.card_def == CardDefId(1))
                })
                .unwrap();
            crate::zones::move_object_raw(
                &mut state_n,
                toll_id2,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
            state_n.set_phase(mtg_ir::GamePhase::BeginCombat);
            state_n.set_decision(DecisionState::Priority);
            state_n.player_mut(PlayerId(1)).unwrap().mana_pool[5] = 1;
            state_n.mark_dirty(DirtyMask::PLAYER_STATE);
            let atk_n = spawn_creature(&mut state_n, PlayerId(0), "Synth Bear", 2, 2, &[]);
            let b1_n = spawn_creature(&mut state_n, PlayerId(1), "Synth B1", 2, 2, &[]);
            let b2_n = spawn_creature(&mut state_n, PlayerId(1), "Synth B2", 2, 2, &[]);
            enter_declare_attackers_step(&mut state_n).unwrap();
            apply_combat_action(
                &mut state_n,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: atk_n,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state_n, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state_n).unwrap();
            apply_combat_action(
                &mut state_n,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1_n,
                    attacker: atk_n,
                },
            )
            .unwrap();
            assert_eq!(
                legal_combat_actions(&state_n, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );
            assert!(
                apply_combat_action(
                    &mut state_n,
                    PlayerId(1),
                    &Action::AssignBlocker {
                        blocker: b2_n,
                        attacker: atk_n,
                    },
                )
                .is_err()
            );
            apply_combat_action(&mut state_n, PlayerId(1), &Action::ConfirmBlockers).unwrap();
            assert_eq!(state_n.player(PlayerId(1)).unwrap().mana_pool[5], 0);
        }
    }

    #[test]
    fn synth_forced_blockers_and_cr509_1c_cr508_1d_impossible_requirements() {
        // 1. AllAbleBlockers on A1 + normal A2 vs 1 blocker B1 -> B1->A2 omitted, ConfirmBlockers withheld until B1->A1.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
            let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Lure", 2, 2, &[]);
            let a2 = spawn_creature(&mut state, PlayerId(0), "Synth Normal", 2, 2, &[]);
            let b1 = spawn_creature(&mut state, PlayerId(1), "Synth Guard", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                a1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::AllAbleBlockers,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            for atk in [a1, a2] {
                apply_combat_action(
                    &mut state,
                    PlayerId(0),
                    &Action::AssignAttacker {
                        attacker: atk,
                        target: AttackTarget::Player { id: PlayerId(1) },
                    },
                )
                .unwrap();
            }
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();

            assert_eq!(
                legal_combat_actions(&state, PlayerId(1)),
                vec![Action::AssignBlocker {
                    blocker: b1,
                    attacker: a1,
                }]
            );
            assert!(
                apply_combat_action(
                    &mut state,
                    PlayerId(1),
                    &Action::AssignBlocker {
                        blocker: b1,
                        attacker: a2,
                    },
                )
                .is_err()
            );
            apply_combat_action(
                &mut state,
                PlayerId(1),
                &Action::AssignBlocker {
                    blocker: b1,
                    attacker: a1,
                },
            )
            .unwrap();
            assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
        }

        // 2. MustBeBlockedIfAble + Menace on A1 vs 1 blocker B1 -> AssignBlocker omitted, ConfirmBlockers legal at empty.
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, kw_men) = synth_combat_state();
            let a1 = spawn_creature(&mut state, PlayerId(0), "Synth Taunt", 3, 2, &[kw_men]);
            let _b1 = spawn_creature(&mut state, PlayerId(1), "Synth Guard", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                a1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::MustBeBlockedIfAble,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: a1,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            enter_declare_blockers_step(&mut state).unwrap();
            assert_eq!(
                legal_combat_actions(&state, PlayerId(1)),
                vec![Action::ConfirmBlockers]
            );
            assert!(apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).is_ok());
        }

        // 3. CantAttackAlone + AttacksEachCombat on sole attacker -> ConfirmAttackers succeeds with 0 attackers (CR 508.1d).
        {
            let (mut state, _fs, _dt, _tr, _ind, _ll, _men) = synth_combat_state();
            let solo = spawn_creature(&mut state, PlayerId(0), "Synth Soloist", 3, 3, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                solo,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CantAttackAlone,
                },
            );
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                solo,
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::AttacksEachCombat,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            assert!(
                apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_ok()
            );
        }
    }

    #[test]
    fn synth_macro_declare_blockers_lookahead_and_rollback() {
        use crate::action::MacroAction;

        let mut tables = InternTables::default();
        let kw_men = mtg_ir::KeywordId(0);
        tables.keywords.push("menace".into());
        let men_card = make_session_creature_card("s_macro_men", "Synth Menacer", &[kw_men]);
        let norm_card = make_session_creature_card("s_macro_norm", "Synth Brute", &[]);
        let guard_card = make_session_creature_card("s_macro_guard", "Synth Guard", &[]);
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![men_card, norm_card, guard_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: {
                let mut m = vec![CardDefId(0); 5];
                m.extend(vec![CardDefId(1); 5]);
                m
            },
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(2); 20],
            sideboard: vec![],
        };
        let mut session = crate::session::MatchSession::new(
            db,
            GameConfig::standard_1v1(),
            vec![p0_deck, p1_deck],
            7,
        )
        .unwrap();
        advance_session_to_declare_blockers(&mut session, &[CardDefId(0), CardDefId(1)], 2);

        let menace_atk = session.state().players()[0]
            .battlefield
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let normal_atk = session.state().players()[0]
            .battlefield
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let b1 = session.state().players()[1].battlefield[0];
        let b2 = session.state().players()[1].battlefield[1];

        assert!(
            session
                .apply_macro_action(
                    PlayerId(1),
                    MacroAction::DeclareBlockers {
                        assignments: vec![(b1, menace_atk)],
                    },
                )
                .is_err()
        );
        assert!(session.state().combat().blockers.is_empty());

        assert!(
            session
                .apply_macro_action(
                    PlayerId(1),
                    MacroAction::DeclareBlockers {
                        assignments: vec![(b1, menace_atk), (b2, normal_atk)],
                    },
                )
                .is_err()
        );
        assert!(session.state().combat().blockers.is_empty());

        assert!(
            session
                .apply_macro_action(
                    PlayerId(1),
                    MacroAction::DeclareBlockers {
                        assignments: vec![(b1, menace_atk), (b2, menace_atk)],
                    },
                )
                .is_ok()
        );
    }

    #[test]
    fn synth_attacker_decl_context_restrictions_goad_tax_and_granted_statics() {
        let make_4p_state = |seed: u64,
                             with_tax_all_opponents: bool|
         -> (GameState, mtg_ir::KeywordId, mtg_ir::KeywordId) {
            let mut tables = InternTables::default();
            let kw_haste = mtg_ir::KeywordId(tables.keywords.len() as u16);
            tables.keywords.push("haste".into());
            let kw_def = mtg_ir::KeywordId(tables.keywords.len() as u16);
            tables.keywords.push("defender".into());

            let land = CardDef {
                oracle_id: "synth_adc_plains".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Plains".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Plains".into(),
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
            let cant_attack_you = CardDef {
                oracle_id: "synth_adc_moat".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Moat".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Moat".into(),
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
                    abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::Restriction {
                                affected: Affected::Filter {
                                    filter: Box::new(mtg_ir::ObjectFilter {
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                },
                                restriction: RestrictionKind::CantAttackYou,
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
            let attack_tax = CardDef {
                oracle_id: "synth_adc_tax".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Prison".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Prison".into(),
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
                    abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::RuleModifier {
                                rule: RuleMod::AttackTax {
                                    generic: Some(1),
                                    cost: None,
                                    mana_symbol: None,
                                    dynamic_amount: None,
                                    affected: None,
                                    defending: None,
                                },
                                who: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::Opponent,
                                },
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
                    cards: vec![land, cant_attack_you, attack_tax],
                    decks: vec![],
                })
                .unwrap(),
            );
            let deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 20],
                sideboard: vec![],
            };
            let mut cfg = GameConfig::standard_1v1();
            cfg.player_count = 4;
            let mut st = GameState::new(
                db,
                cfg,
                &[deck.clone(), deck.clone(), deck.clone(), deck],
                seed,
            )
            .unwrap();
            st.set_phase(mtg_ir::GamePhase::BeginCombat);
            st.set_decision(DecisionState::Priority);
            if with_tax_all_opponents {
                for pid in [PlayerId(1), PlayerId(2), PlayerId(3)] {
                    let h = st.player(pid).unwrap().hand[0];
                    st.object_mut(h).unwrap().card_def = CardDefId(2);
                    st.mark_dirty(DirtyMask::TOPOLOGY);
                    crate::zones::move_object_raw(
                        &mut st,
                        h,
                        Zone::Battlefield,
                        crate::zones::MoveRawOptions::default(),
                        None,
                    )
                    .unwrap();
                }
            }
            (st, kw_haste, kw_def)
        };

        // (a) Summoning sickness + haste + CanAttackAsThoughHaste, and defender + CanAttackDespiteDefender.
        {
            let (mut state, kw_haste, kw_def) = make_4p_state(101, false);
            let sick_plain = spawn_creature(&mut state, PlayerId(0), "Synth Sick", 2, 2, &[]);
            state
                .object_mut(sick_plain)
                .unwrap()
                .flags
                .insert(ObjectFlags::SUMMONING_SICK);
            let sick_haste =
                spawn_creature(&mut state, PlayerId(0), "Synth Haster", 2, 2, &[kw_haste]);
            state
                .object_mut(sick_haste)
                .unwrap()
                .flags
                .insert(ObjectFlags::SUMMONING_SICK);
            let wall = spawn_creature(&mut state, PlayerId(0), "Synth Wall", 0, 4, &[kw_def]);

            enter_declare_attackers_step(&mut state).unwrap();
            let legal = legal_combat_actions(&state, PlayerId(0));
            assert!(!legal.iter().any(|a| matches!(
                a,
                Action::AssignAttacker { attacker, .. } if *attacker == sick_plain
            )));
            assert!(!legal.iter().any(|a| matches!(
                a,
                Action::AssignAttacker { attacker, .. } if *attacker == wall
            )));
            assert!(legal.contains(&Action::AssignAttacker {
                attacker: sick_haste,
                target: AttackTarget::Player { id: PlayerId(1) },
            }));

            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                sick_plain,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CanAttackAsThoughHaste,
                },
            );
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                wall,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CanAttackDespiteDefender,
                },
            );
            let legal2 = legal_combat_actions(&state, PlayerId(0));
            assert!(legal2.contains(&Action::AssignAttacker {
                attacker: sick_plain,
                target: AttackTarget::Player { id: PlayerId(2) },
            }));
            assert!(legal2.contains(&Action::AssignAttacker {
                attacker: wall,
                target: AttackTarget::Player { id: PlayerId(3) },
            }));
        }

        // (b) CantAttackYou, CantAttack, and LayerMod::GrantAbility on vanilla token (0 innate statics).
        {
            let (mut state, _, _) = make_4p_state(102, false);
            let moat_h = state.player(PlayerId(1)).unwrap().hand[0];
            state.object_mut(moat_h).unwrap().card_def = CardDefId(1);
            state.mark_dirty(DirtyMask::TOPOLOGY);
            crate::zones::move_object_raw(
                &mut state,
                moat_h,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();

            let bear1 = spawn_creature(&mut state, PlayerId(0), "Synth Bear One", 2, 2, &[]);
            let bear2 = spawn_creature(&mut state, PlayerId(0), "Synth Bear Two", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                bear2,
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::CantAttack,
                },
            );

            enter_declare_attackers_step(&mut state).unwrap();
            let legal = legal_combat_actions(&state, PlayerId(0));
            assert!(!legal.contains(&Action::AssignAttacker {
                attacker: bear1,
                target: AttackTarget::Player { id: PlayerId(1) },
            }));
            assert!(legal.contains(&Action::AssignAttacker {
                attacker: bear1,
                target: AttackTarget::Player { id: PlayerId(2) },
            }));
            assert!(legal.contains(&Action::AssignAttacker {
                attacker: bear1,
                target: AttackTarget::Player { id: PlayerId(3) },
            }));
            assert!(!legal.iter().any(|a| matches!(
                a,
                Action::AssignAttacker { attacker, .. } if *attacker == bear2
            )));
        }

        // (c) rare.goaded_by + CombatRuleSpec::IsGoaded + CantAttackAlone.
        {
            let (mut state, _, _) = make_4p_state(103, false);
            let g1 = spawn_creature(&mut state, PlayerId(0), "Synth Goaded One", 2, 2, &[]);
            state
                .rare_or_default(g1)
                .unwrap()
                .goaded_by
                .push(PlayerId(1));
            state.mark_dirty(DirtyMask::COMBAT);

            let g2 = spawn_creature(&mut state, PlayerId(0), "Synth Goaded Two", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(2),
                g2,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::IsGoaded,
                },
            );
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                g1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CantAttackAlone,
                },
            );

            enter_declare_attackers_step(&mut state).unwrap();
            let legal0 = legal_combat_actions(&state, PlayerId(0));
            assert!(!legal0.contains(&Action::ConfirmAttackers));
            assert!(!legal0.contains(&Action::AssignAttacker {
                attacker: g1,
                target: AttackTarget::Player { id: PlayerId(1) },
            }));
            assert!(legal0.contains(&Action::AssignAttacker {
                attacker: g1,
                target: AttackTarget::Player { id: PlayerId(2) },
            }));
            assert!(!legal0.contains(&Action::AssignAttacker {
                attacker: g2,
                target: AttackTarget::Player { id: PlayerId(2) },
            }));
            assert!(legal0.contains(&Action::AssignAttacker {
                attacker: g2,
                target: AttackTarget::Player { id: PlayerId(1) },
            }));

            // Assign only g1 -> CantAttackAlone + forced g2 omit ConfirmAttackers.
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: g1,
                    target: AttackTarget::Player { id: PlayerId(2) },
                },
            )
            .unwrap();
            assert!(!legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));

            // Assign g2 -> P1: now both forced attackers & CantAttackAlone are satisfied.
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: g2,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
            assert!(
                apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_ok()
            );
        }

        // (d) Pure CantAttackAlone: 1 attacker omits ConfirmAttackers, 2 attackers includes it.
        {
            let (mut state, _, _) = make_4p_state(104, false);
            let solo = spawn_creature(&mut state, PlayerId(0), "Synth Shy", 2, 2, &[]);
            let buddy = spawn_creature(&mut state, PlayerId(0), "Synth Pal", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                solo,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::CantAttackAlone,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: solo,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(!legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
            assert!(
                apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_err()
            );

            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: buddy,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
            assert!(
                apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_ok()
            );
        }

        // (e) GlobalMaxAttackers (against_you_only: false and true) with forced attacker saturation.
        {
            let (mut state_g, _, _) = make_4p_state(105, false);
            let f1 = spawn_creature(&mut state_g, PlayerId(0), "Synth Berserk 1", 2, 2, &[]);
            let f2 = spawn_creature(&mut state_g, PlayerId(0), "Synth Berserk 2", 2, 2, &[]);
            for &f in &[f1, f2] {
                grant_object_combat_static(
                    &mut state_g,
                    PlayerId(0),
                    f,
                    StaticBody::Restriction {
                        affected: Affected::SelfObj,
                        restriction: RestrictionKind::AttacksEachCombat,
                    },
                );
            }
            grant_object_combat_static(
                &mut state_g,
                PlayerId(1),
                f1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::GlobalMaxAttackers {
                        max: 1,
                        against_you_only: false,
                    },
                },
            );
            enter_declare_attackers_step(&mut state_g).unwrap();
            assert!(
                !legal_combat_actions(&state_g, PlayerId(0)).contains(&Action::ConfirmAttackers)
            );
            apply_combat_action(
                &mut state_g,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: f1,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(
                legal_combat_actions(&state_g, PlayerId(0)).contains(&Action::ConfirmAttackers)
            );
            assert!(
                apply_combat_action(&mut state_g, PlayerId(0), &Action::ConfirmAttackers).is_ok()
            );

            let (mut state_d, _, _) = make_4p_state(106, false);
            let d1 = spawn_creature(&mut state_d, PlayerId(0), "Synth Forced 1", 2, 2, &[]);
            let d2 = spawn_creature(&mut state_d, PlayerId(0), "Synth Forced 2", 2, 2, &[]);
            for &d in &[d1, d2] {
                grant_object_combat_static(
                    &mut state_d,
                    PlayerId(0),
                    d,
                    StaticBody::Restriction {
                        affected: Affected::SelfObj,
                        restriction: RestrictionKind::AttacksEachCombat,
                    },
                );
            }
            grant_object_combat_static(
                &mut state_d,
                PlayerId(1),
                d1,
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::GlobalMaxAttackers {
                        max: 1,
                        against_you_only: true,
                    },
                },
            );
            enter_declare_attackers_step(&mut state_d).unwrap();
            apply_combat_action(
                &mut state_d,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: d1,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            // P1's cap is saturated, so d2 must still attack P2 or P3.
            assert!(
                !legal_combat_actions(&state_d, PlayerId(0)).contains(&Action::ConfirmAttackers)
            );
            apply_combat_action(
                &mut state_d,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: d2,
                    target: AttackTarget::Player { id: PlayerId(2) },
                },
            )
            .unwrap();
            assert!(
                legal_combat_actions(&state_d, PlayerId(0)).contains(&Action::ConfirmAttackers)
            );
            assert!(
                apply_combat_action(&mut state_d, PlayerId(0), &Action::ConfirmAttackers).is_ok()
            );
        }

        // (f) RuleMod::AttackTax: blocks unpaid confirmation and excuses forced attackers when tax > 0.
        {
            let (mut state, _, _) = make_4p_state(107, true);
            let forced = spawn_creature(&mut state, PlayerId(0), "Synth Charger", 2, 2, &[]);
            grant_object_combat_static(
                &mut state,
                PlayerId(0),
                forced,
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::AttacksEachCombat,
                },
            );
            enter_declare_attackers_step(&mut state).unwrap();
            // Forced attacker is excused when every opponent has AttackTax > 0 and P0 has 0 mana.
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));

            // Assigning forced attacker to P1 with 0 mana blocks ConfirmAttackers.
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: forced,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(!legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
            assert!(
                apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).is_err()
            );

            // Adding 1 generic mana allows ConfirmAttackers and pays the tax.
            state.player_mut(PlayerId(0)).unwrap().mana_pool[5] = 1;
            state.mark_dirty(DirtyMask::PLAYER_STATE);
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
            apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[5], 0);
        }
    }

    #[test]
    fn synth_for_each_combat_static_transform_emblem_and_lost_abilities() {
        let tables = InternTables::default();
        let land = CardDef {
            oracle_id: "synth_fec_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
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
        let transform_dfc = CardDef {
            oracle_id: "synth_fec_dfc".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Seeker // Synth Loner".into(),
            layout: CardLayout::Transform,
            faces: vec![
                FaceDef {
                    name: "Synth Seeker".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::W,
                        color_indicator: None,
                        pt: Some(mtg_ir::PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        }),
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
                FaceDef {
                    name: "Synth Loner".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::W,
                        color_indicator: None,
                        pt: Some(mtg_ir::PrintedPt::Fixed {
                            power: 3,
                            toughness: 3,
                        }),
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                        static_def: mtg_ir::StaticDef {
                            condition: None,
                            body: StaticBody::CombatRule {
                                affected: Affected::SelfObj,
                                rule: CombatRuleSpec::CantAttackAlone,
                            },
                        },
                        span: Default::default(),
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
            ],
            fully_supported: true,
        };
        let emblem_card = CardDef {
            oracle_id: "synth_fec_emblem".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Emblem".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Emblem".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::empty(),
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
                        body: StaticBody::CombatRule {
                            affected: Affected::Filter {
                                filter: Box::new(mtg_ir::ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(mtg_ir::RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            rule: CombatRuleSpec::CantAttackAlone,
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
        let pacifist_card = CardDef {
            oracle_id: "synth_fec_pacifist".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Pacifist".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Pacifist".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        condition: None,
                        body: StaticBody::Restriction {
                            affected: Affected::SelfObj,
                            restriction: RestrictionKind::CantAttack,
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
                cards: vec![land, transform_dfc, emblem_card, pacifist_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };

        // 1. Transforming DFC: face 0 has no CantAttackAlone, face 1 activates CantAttackAlone.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                108,
            )
            .unwrap();
            state.set_phase(mtg_ir::GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            let h = state.player(PlayerId(0)).unwrap().hand[0];
            state.object_mut(h).unwrap().card_def = CardDefId(1);
            let dfc = crate::zones::move_object_raw(
                &mut state,
                h,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();
            state
                .object_mut(dfc)
                .unwrap()
                .flags
                .remove(ObjectFlags::SUMMONING_SICK);
            state.mark_dirty(DirtyMask::TOPOLOGY);

            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: dfc,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));

            // Transform to face 1: CantAttackAlone is now active.
            state.object_mut(dfc).unwrap().active_face_index = 1;
            state.mark_dirty(DirtyMask::TOPOLOGY);
            assert!(!legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
        }

        // 2. Command-zone non-emblem ignored vs IS_EMBLEM active.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                109,
            )
            .unwrap();
            state.set_phase(mtg_ir::GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            let bear = spawn_creature(&mut state, PlayerId(0), "Synth Bear", 2, 2, &[]);
            let cmd_src = state.player(PlayerId(0)).unwrap().hand[0];
            state.object_mut(cmd_src).unwrap().card_def = CardDefId(2);
            let cmd_id = crate::zones::move_object_raw(
                &mut state,
                cmd_src,
                Zone::Command,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();
            state.mark_dirty(DirtyMask::TOPOLOGY);

            enter_declare_attackers_step(&mut state).unwrap();
            apply_combat_action(
                &mut state,
                PlayerId(0),
                &Action::AssignAttacker {
                    attacker: bear,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            // Without IS_EMBLEM, Command-zone object's static does not apply.
            assert!(legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));

            // With IS_EMBLEM, Command-zone emblem's CantAttackAlone applies.
            state
                .object_mut(cmd_id)
                .unwrap()
                .flags
                .insert(ObjectFlags::IS_EMBLEM);
            state.mark_dirty(DirtyMask::TOPOLOGY);
            assert!(!legal_combat_actions(&state, PlayerId(0)).contains(&Action::ConfirmAttackers));
        }

        // 3. Layer 6 lost_all_abilities suppresses innate CantAttack while preserving GrantedAbilityEntry.
        {
            let mut state =
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 110).unwrap();
            state.set_phase(mtg_ir::GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            let h = state.player(PlayerId(0)).unwrap().hand[0];
            state.object_mut(h).unwrap().card_def = CardDefId(3);
            let pac = crate::zones::move_object_raw(
                &mut state,
                h,
                Zone::Battlefield,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();
            state
                .object_mut(pac)
                .unwrap()
                .flags
                .remove(ObjectFlags::SUMMONING_SICK);
            state.mark_dirty(DirtyMask::TOPOLOGY);

            enter_declare_attackers_step(&mut state).unwrap();
            assert!(
                !legal_combat_actions(&state, PlayerId(0)).contains(&Action::AssignAttacker {
                    attacker: pac,
                    target: AttackTarget::Player { id: PlayerId(1) },
                })
            );

            // RemoveAllAbilities at earlier timestamp suppresses innate CantAttack.
            let eff_rm = state.allocate_continuous_effect_id();
            let ts_rm = state.allocate_timestamp();
            state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
                id: eff_rm,
                source: None,
                controller: PlayerId(1),
                layer: mtg_ir::Layer::Layer6,
                affected: crate::layers::predicates::ContinuousTarget::Objects(
                    smallvec::smallvec![pac],
                ),
                modifier: mtg_ir::LayerMod::RemoveAllAbilities {
                    keep_mana_abilities: false,
                },
                duration: mtg_ir::Duration::UntilYourNextTurn,
                timestamp: ts_rm,
                expires_turn: Some(state.turn_serial() + 3),
                expires_on_player: Some(PlayerId(1)),
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });
            assert!(
                legal_combat_actions(&state, PlayerId(0)).contains(&Action::AssignAttacker {
                    attacker: pac,
                    target: AttackTarget::Player { id: PlayerId(1) },
                })
            );

            // GrantAbility at later timestamp preserves GrantedAbilityEntry CantAttack despite lost_all_abilities.
            grant_object_combat_static(
                &mut state,
                PlayerId(1),
                pac,
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::CantAttack,
                },
            );
            assert!(
                !legal_combat_actions(&state, PlayerId(0)).contains(&Action::AssignAttacker {
                    attacker: pac,
                    target: AttackTarget::Player { id: PlayerId(1) },
                })
            );
        }
    }

    #[test]
    fn synth_confirm_attackers_vigilance_cache_lookup() {
        let mut tables = InternTables::default();
        let kw_vig = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("Vigilance".into());
        let land = CardDef {
            oracle_id: "synth_vig_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 111).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        let vig_atk = spawn_creature(&mut state, PlayerId(0), "Synth Sentinel", 2, 2, &[kw_vig]);
        let norm_atk = spawn_creature(&mut state, PlayerId(0), "Synth Bear", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: vig_atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: norm_atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();

        assert!(
            !state
                .object(vig_atk)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert!(
            state
                .object(norm_atk)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn synth_combat_prompt_invalid_answer_preserves_prompt_and_flushes_block_triggers() {
        let tables = InternTables::default();
        let plains = CardDef {
            oracle_id: "synth_combat_block_trig_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Combat Block Trig Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Combat Block Trig Plains".into(),
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
        let trig_attacker = CardDef {
            oracle_id: "synth_becomes_blocked_attacker".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Becomes Blocked Attacker".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Becomes Blocked Attacker".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 4,
                        toughness: 4,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::BecomesBlocked,
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
                cards: vec![plains, trig_attacker],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 37).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        let ts_atk = state.allocate_timestamp();
        let atk = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts_atk,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().battlefield.push(atk);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let b1 = spawn_creature(&mut state, PlayerId(1), "Synth Guard A", 2, 2, &[]);
        let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Guard B", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: atk,
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b2,
                attacker: atk,
            },
        )
        .unwrap();
        let out = apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();
        let CombatOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected DamageAssignmentOrder pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        let pp = state.pending_prompt().unwrap().clone();

        // Invalid ChoiceAnswer variant + out-of-set candidate preserve prompt & legal actions.
        for bad in [
            ChoiceAnswer::YesNo { value: true },
            ChoiceAnswer::OrderNext { id: atk },
        ] {
            assert!(
                crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                    .is_err()
            );
            assert!(crate::choices::is_prompt_active(&state));
            assert_eq!(state.pending_prompt(), Some(&pp));
            assert!(matches!(state.decision(), DecisionState::Prompt { .. }));
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            let order_ids: Vec<ObjectId> = legal
                .iter()
                .filter_map(|a| match a {
                    Action::Answer {
                        answer: ChoiceAnswer::OrderNext { id },
                        ..
                    } => Some(*id),
                    _ => None,
                })
                .collect();
            assert_eq!(order_ids, vec![b1, b2]);
        }

        // Valid answer completes ordering, clears DecisionState::Prompt -> Priority, and flushes BecomesBlocked trigger!
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::OrderNext { id: b1 },
        )
        .unwrap();
        assert!(state.pending_prompt().is_none());
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.stack().len(), 1);
        assert!(state.pending_triggers().is_empty());
    }

    #[test]
    fn synth_combat_damage_divide_invalid_answer_preserves_prompt() {
        let tables = InternTables::default();
        let plains = CardDef {
            oracle_id: "synth_combat_div_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Combat Div Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Combat Div Plains".into(),
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
        let dying_blocker = CardDef {
            oracle_id: "synth_dying_blocker".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dying Blocker".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Dying Blocker".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::ZoneChange,
                        filter: mtg_ir::EventFilter {
                            object: Some(mtg_ir::TriggerSubject::SelfObj),
                            detail: Some(mtg_ir::EventDetail::ZoneChange {
                                from: mtg_ir::ZoneMask::BATTLEFIELD,
                                to: Some(Zone::Graveyard),
                                from_anywhere: false,
                                not_from_zone: None,
                                not_to_zone: None,
                                enters_untapped: false,
                                dealt_damage_by_self_this_turn: false,
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
                        amount: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
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
                cards: vec![plains, dying_blocker],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 41).unwrap();
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        state.set_decision(DecisionState::Priority);

        let atk = spawn_creature(&mut state, PlayerId(0), "Synth Titan", 5, 5, &[]);
        let ts_b1 = state.allocate_timestamp();
        let b1 = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts_b1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(1)).unwrap().battlefield.push(b1);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let b2 = spawn_creature(&mut state, PlayerId(1), "Synth Guard Two", 2, 2, &[]);

        enter_declare_attackers_step(&mut state).unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(0),
            &Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_combat_action(&mut state, PlayerId(0), &Action::ConfirmAttackers).unwrap();
        enter_declare_blockers_step(&mut state).unwrap();

        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b1,
                attacker: atk,
            },
        )
        .unwrap();
        apply_combat_action(
            &mut state,
            PlayerId(1),
            &Action::AssignBlocker {
                blocker: b2,
                attacker: atk,
            },
        )
        .unwrap();
        let out_blk =
            apply_combat_action(&mut state, PlayerId(1), &Action::ConfirmBlockers).unwrap();
        let CombatOutcome::Paused {
            prompt_id: ord_pid, ..
        } = out_blk
        else {
            panic!("expected DamageAssignmentOrder pause");
        };
        apply_combat_prompt_answer(
            &mut state,
            PlayerId(0),
            ord_pid,
            ChoiceAnswer::OrderNext { id: b1 },
        );

        state.set_phase(mtg_ir::GamePhase::CombatDamage);
        let out_dmg = execute_combat_damage_step(&mut state, false).unwrap();
        let CombatOutcome::Paused {
            prompt_id: div_pid,
            chooser,
        } = out_dmg
        else {
            panic!("expected AttackerDamageDivide pause, got {out_dmg:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        let pp = state.pending_prompt().unwrap().clone();
        assert!(matches!(
            pp.owner,
            PromptOwner::Combat(CombatPromptKind::AttackerDamageDivide { .. })
        ));

        for bad in [
            ChoiceAnswer::YesNo { value: true },
            ChoiceAnswer::PickNumber { value: 1 },
            ChoiceAnswer::PickNumber { value: 99 },
        ] {
            assert!(
                crate::choices::apply_choice_answer(&mut state, PlayerId(0), div_pid, bad).is_err()
            );
            assert!(crate::choices::is_prompt_active(&state));
            assert_eq!(state.pending_prompt(), Some(&pp));
            assert!(matches!(state.decision(), DecisionState::Prompt { .. }));
            assert!(state.combat().pending_damage.is_some());
        }

        // Valid PickNumber { value: 3 } resolves combat damage (b1=3, b2=2), kills both blockers, and flushes b1's death trigger onto stack!
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            div_pid,
            ChoiceAnswer::PickNumber { value: 3 },
        )
        .unwrap();
        assert!(state.pending_prompt().is_none());
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert!(state.combat().pending_damage.is_none());
        assert_eq!(state.stack().len(), 1);
        assert!(state.pending_triggers().is_empty());
    }

    fn apply_combat_prompt_answer(
        state: &mut GameState,
        actor: PlayerId,
        prompt_id: u32,
        answer: ChoiceAnswer,
    ) {
        crate::choices::apply_choice_answer(state, actor, prompt_id, answer).unwrap();
    }
}
