use std::sync::Arc;

use mtg_ir::{
    CardTypeMask, CoerciveDiscardSpec, Condition, CopyModificationSpec, Duration, EffectStep,
    LibraryChoiceBy, LibraryPosition, PartialRevealSpec, PerOpponentAttackMode, RoundMode,
    SelectionMod, Selector, SpecialRevealTag, TokenSpec, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{AttackTarget, ChoiceRequest, DecisionState};
use crate::eval::{BoardView, EvalContext, evaluate_amount, evaluate_condition};
use crate::events::{AtomicEventKind, emit_event};
use crate::filter::{find_objects, matches_object_filter};
use crate::id::{ObjectId, PlayerId};
use crate::keywords::has_indestructible;
use crate::replace::{self, ReplacementOutcome, ReplacementResult};
use crate::state::{
    CopiableState, DirtyMask, EngineError, GameObject, GameState, LkiSnapshot, ObjectFlags,
    PendingPromptContext, PendingStepContinuation, PromptOwner, RareObjectState, StepPauseState,
    TOKEN_CARD_DEF_ID, TimedCopiableBackup, UntilSourceLeavesReturn, YesNoPausePurpose,
};
use crate::zones::{
    self, CreateTokenOptions, MoveObjectOutcome, MoveRawOptions, MoveRawOutcome, ZonePosition,
};

use super::vm::{
    OP_TAG_BOUNCE, OP_TAG_CREATE_TOKEN, OP_TAG_DESTROY, OP_TAG_DISCARD, OP_TAG_EXILE, OP_TAG_MILL,
    OP_TAG_MOVE, OP_TAG_SACRIFICE, SelectionResult, StepExecOutcome, append_step_binding,
    handle_zone_move_outcome, move_opts_for_zone, rare_turn_state_mut, record_step_bindings,
    resolve_selection_mod, resolve_selector_objects, resolve_step_actor, resolve_step_amount,
    resolve_target_players, set_waiting_on_replacement, vm_controller, vm_source, with_vm_eval,
};

fn try_regenerate_permanent(state: &mut GameState, id: ObjectId) -> bool {
    let cant = state
        .rare_turn_state()
        .is_some_and(|r| r.cant_be_regenerated_this_turn.contains(&id));
    if cant {
        return false;
    }
    let has_shield = state.rare_turn_state().is_some_and(|r| {
        r.regeneration_shields
            .iter()
            .any(|(oid, c)| *oid == id && *c > 0)
    });
    if !has_shield {
        return false;
    }
    let rts = rare_turn_state_mut(state);
    if let Some((_, c)) = rts
        .regeneration_shields
        .iter_mut()
        .find(|(oid, _)| *oid == id)
    {
        *c = c.saturating_sub(1);
    }
    rts.regeneration_shields.retain(|(_, c)| *c > 0);
    state.prune_rare_turn_state_if_empty();

    // CR 701.15a: Tap it, remove all damage marked on it, and remove it from combat.
    if let Some(obj) = state.object_mut(id) {
        obj.flags.insert(ObjectFlags::TAPPED);
        obj.damage_marked = 0;
        obj.deathtouch_damage_marked = false;
    }
    crate::combat::remove_from_combat(state, id);
    state.mark_dirty(DirtyMask::TAPPED | DirtyMask::COMBAT);
    true
}

fn can_be_sacrificed(state: &GameState, id: ObjectId, forced_by_opponent: bool) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield || obj.is_phased_out() {
        return false;
    }
    let cache = state.evaluate_board();
    !(forced_by_opponent
        && cache
            .player(obj.controller)
            .is_some_and(|ep| ep.immune_to_opp_forced_sacrifice))
}

pub(crate) fn apply_post_zone_move_effects(
    state: &mut GameState,
    step: &EffectStep,
    op_tag: u8,
    raw: &MoveRawOutcome,
) {
    let ctrl = vm_controller(state);
    let src = vm_source(state);
    let final_id = raw.new_id.unwrap_or(raw.old_id);
    let pre_types = raw.lki.as_ref().map(|l| l.types).unwrap_or_else(|| {
        if raw.ceased_to_exist {
            return CardTypeMask::empty();
        }
        let cache = state.evaluate_board();
        if let Some(ev) = cache.get(final_id) {
            ev.types
        } else if let Some(obj) = state.object(final_id) {
            let ctx = EvalContext::new(obj.controller).with_board(BoardView::Cached(cache));
            crate::eval::off_battlefield_characteristics(state, final_id, obj, obj.zone, &ctx).types
        } else {
            CardTypeMask::empty()
        }
    });
    let obj_ctrl = raw
        .lki
        .as_ref()
        .map(|l| l.controller)
        .or_else(|| state.object(final_id).map(|o| o.controller))
        .unwrap_or(ctrl);
    let obj_owner = raw
        .lki
        .as_ref()
        .map(|l| l.owner)
        .or_else(|| state.object(final_id).map(|o| o.owner))
        .unwrap_or(ctrl);

    match op_tag {
        OP_TAG_EXILE if !raw.ceased_to_exist && raw.to_zone == Zone::Exile => {
            if let Some(r) = state.rare_or_default(final_id) {
                r.exiled_by = src;
                if let mtg_ir::EffectOp::Exile {
                    until_source_leaves: true,
                    ..
                } = &step.op
                    && let Some(s) = src
                {
                    r.until_source_leaves_return = Some(UntilSourceLeavesReturn {
                        source: s,
                        return_zone: Zone::Battlefield,
                    });
                }
            }
            state.mark_dirty(DirtyMask::EXILE);
            if let mtg_ir::EffectOp::Exile {
                exile_with_counters: Some((cid, cnt)),
                ..
            } = &step.op
            {
                state.add_counter(final_id, *cid, u32::from(*cnt));
            }
        }
        OP_TAG_SACRIFICE => {
            let lki = raw
                .lki
                .clone()
                .unwrap_or_else(|| Arc::new(super::vm::empty_lki(raw.old_id, obj_ctrl)));
            emit_event(
                state,
                AtomicEventKind::Sacrificed {
                    player: obj_ctrl,
                    object: raw.old_id,
                    lki,
                },
            );
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                if !pre_types.contains(CardTypeMask::LAND) {
                    vm.step_way_stats.sacrificed_nonland_this_way = true;
                }
                vm.step_way_stats.distinct_sacrificed_types_mask |= pre_types.bits();
            }
        }
        OP_TAG_DISCARD => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                if !pre_types.contains(CardTypeMask::LAND) {
                    vm.step_way_stats.discarded_nonland_this_way = true;
                }
                if pre_types.contains(CardTypeMask::CREATURE) {
                    vm.step_way_stats.discarded_creature_this_way = true;
                } else {
                    vm.step_way_stats.discarded_noncreature_this_way = true;
                }
            }
            emit_event(
                state,
                AtomicEventKind::Discard {
                    player: obj_owner,
                    object: final_id,
                    dest_zone: raw.to_zone,
                    is_cycle: false,
                    cause_controller: Some(ctrl),
                },
            );
        }
        OP_TAG_MOVE if !raw.ceased_to_exist => {
            if let mtg_ir::EffectOp::Move {
                tapped,
                under_your_control,
                enters_tapped_and_attacking,
                unearth,
                with_counters,
                attached_to,
                ..
            } = &step.op
            {
                if *unearth && let Some(o) = state.object_mut(final_id) {
                    o.flags.insert(ObjectFlags::UNEARTH_LEAVE_EXILE);
                }
                if let Some((cid, cnt)) = *with_counters {
                    state.add_counter(final_id, cid, u32::from(cnt));
                }
                if raw.to_zone == Zone::Battlefield {
                    if (*tapped || *enters_tapped_and_attacking)
                        && let Some(o) = state.object_mut(final_id)
                        && !o.flags.contains(ObjectFlags::TAPPED)
                    {
                        o.flags.insert(ObjectFlags::TAPPED);
                        state.mark_dirty(DirtyMask::TAPPED);
                    }
                    if *under_your_control
                        && state
                            .object(final_id)
                            .is_some_and(|o| o.zone == Zone::Battlefield && o.controller != ctrl)
                    {
                        let _ = zones::transfer_control_raw(state, final_id, ctrl);
                    }
                    if *enters_tapped_and_attacking
                        && state.combat().active
                        && !state
                            .combat()
                            .attackers
                            .iter()
                            .any(|a| a.attacker == final_id)
                        && let Some(did) = state
                            .players()
                            .iter()
                            .find(|p| !p.lost && p.id != ctrl)
                            .map(|p| p.id)
                    {
                        state
                            .combat_mut()
                            .attackers
                            .push(crate::state::AttackerEntry {
                                attacker: final_id,
                                target: AttackTarget::Player { id: did },
                            });
                        state.mark_dirty(DirtyMask::COMBAT);
                    }
                    if let Some(sel) = attached_to.as_deref()
                        && let Some(hid) = resolve_selector_objects(state, sel).into_iter().next()
                        && state
                            .object(hid)
                            .is_some_and(|o| o.zone == Zone::Battlefield)
                    {
                        let _ = zones::attach_raw(state, final_id, hid);
                    }
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn resume_zone_move_batch(
    state: &mut GameState,
    step: &EffectStep,
    batch: &[ObjectId],
    start_idx: usize,
    op_tag: u8,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    let bind_as = step.bind_as.as_deref();

    for (offset, &id) in batch[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        let Some(obj) = state.object(id) else {
            continue;
        };
        let obj_zone = obj.zone;
        if op_tag == OP_TAG_DESTROY {
            if obj_zone != Zone::Battlefield || has_indestructible(state, id) {
                continue;
            }
            if try_regenerate_permanent(state, id) {
                continue;
            }
        }
        let (dest, opts) = match op_tag {
            OP_TAG_DESTROY => (Zone::Graveyard, MoveRawOptions::default()),
            OP_TAG_EXILE => (Zone::Exile, MoveRawOptions::default()),
            OP_TAG_BOUNCE => (Zone::Hand, MoveRawOptions::default()),
            OP_TAG_SACRIFICE => (Zone::Graveyard, MoveRawOptions::default()),
            OP_TAG_DISCARD => (
                Zone::Graveyard,
                MoveRawOptions {
                    is_discard: true,
                    discard_caused_by: Some(ctrl),
                    ..Default::default()
                },
            ),
            OP_TAG_MILL => (Zone::Graveyard, MoveRawOptions::default()),
            OP_TAG_MOVE => {
                if let mtg_ir::EffectOp::Move {
                    to,
                    tapped,
                    under_your_control,
                    position,
                    entered_transformed,
                    entered_flipped,
                    enters_tapped_and_attacking,
                    ..
                } = &step.op
                {
                    let dest = to.unwrap_or(Zone::Battlefield);
                    let pos = match position {
                        Some(LibraryPosition::Top) => ZonePosition::Top,
                        Some(LibraryPosition::Bottom) => ZonePosition::Bottom,
                        Some(LibraryPosition::SecondOrBottom) => ZonePosition::FromTop(1),
                        _ => ZonePosition::Default,
                    };
                    let mut opts = move_opts_for_zone(
                        dest,
                        *tapped || *enters_tapped_and_attacking,
                        if *under_your_control {
                            Some(ctrl)
                        } else {
                            None
                        },
                        pos,
                    );
                    opts.entered_transformed = *entered_transformed;
                    opts.entered_flipped = *entered_flipped;
                    (dest, opts)
                } else {
                    (Zone::Battlefield, MoveRawOptions::default())
                }
            }
            _ => (Zone::Graveyard, MoveRawOptions::default()),
        };

        let mv = zones::move_object(state, id, dest, opts, None)?;
        let moved_raw = if let MoveObjectOutcome::Moved(ref r) = mv {
            Some(r.clone())
        } else {
            None
        };
        let res = handle_zone_move_outcome(state, mv, bind_as, batch.to_vec(), idx, op_tag)?;
        match res {
            StepExecOutcome::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            StepExecOutcome::EnterSubframe => {
                return Ok(StepExecOutcome::EnterSubframe);
            }
            _ => {}
        }

        if let Some(ref raw) = moved_raw {
            apply_post_zone_move_effects(state, step, op_tag, raw);
        }
    }

    if op_tag == OP_TAG_MOVE
        && let mtg_ir::EffectOp::Move {
            may_shuffle: true, ..
        } = &step.op
    {
        let actor = resolve_step_actor(state, step);
        let prompt_id = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::OptionalChoice {
                chooser: actor,
                purpose: YesNoPausePurpose::MayShuffleAfterMoveOrLook,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::YesNo {
            prompt_id,
            label: "Shuffle your library?".into(),
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: actor,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(actor);
        return Ok(StepExecOutcome::Paused {
            prompt_id,
            chooser: actor,
        });
    }

    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_destroy_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    cant_be_regenerated: bool,
) -> Result<StepExecOutcome, EngineError> {
    if cant_be_regenerated {
        let rts = rare_turn_state_mut(state);
        for &id in &targets {
            if !rts.cant_be_regenerated_this_turn.contains(&id) {
                rts.cant_be_regenerated_this_turn.push(id);
            }
        }
    }

    let valid: Vec<ObjectId> = targets
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
        })
        .collect();
    if valid.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    crate::events::begin_simultaneous_batch(state, &valid);
    let res = resume_zone_move_batch(state, step, &valid, 0, OP_TAG_DESTROY);
    crate::events::end_simultaneous_batch(state);
    res
}

pub(crate) fn exec_destroy(
    state: &mut GameState,
    step: &EffectStep,
    cant_be_regenerated: bool,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let actor = resolve_step_actor(state, step);
    let targets =
        match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };

    run_destroy_targets(state, step, targets, cant_be_regenerated)
}

pub(crate) fn run_exile_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    face_down: bool,
    look_only: bool,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    if look_only {
        for &id in &targets {
            zones::reveal_to_player(state, id, actor);
        }
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.did_reveal_this_way = true;
        }
        let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
        record_step_bindings(state, step.bind_as.as_deref(), &acted);
        return Ok(StepExecOutcome::Done);
    }
    if face_down {
        let src = vm_source(state);
        for &id in &targets {
            let outcome =
                zones::exile_face_down(state, id, crate::state::FaceDownSpec::default(), src)?;
            let final_id = outcome.new_id.unwrap_or(outcome.old_id);
            append_step_binding(state, step.bind_as.as_deref(), final_id, outcome.lki);
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.step_way_stats.exiled_count_this_way += 1;
            }
        }
        return Ok(StepExecOutcome::Done);
    }
    resume_zone_move_batch(state, step, &targets, 0, OP_TAG_EXILE)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_exile(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
    _duration: Option<Duration>,
    _until_source_leaves: bool,
    _until_opponent_becomes_monarch: bool,
    exile_all_except_bottom: bool,
    face_down: bool,
    look_only: bool,
    _exile_with_counters: Option<(mtg_ir::CounterId, u16)>,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    if exile_all_except_bottom {
        if let Some(p) = state.player(actor) {
            let len = p.library.len();
            if len > 1 {
                let to_exile: Vec<ObjectId> = p.library[1..].to_vec();
                return resume_zone_move_batch(state, step, &to_exile, 0, OP_TAG_EXILE);
            }
        }
        return Ok(StepExecOutcome::Done);
    }
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let targets =
        match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    run_exile_targets(state, step, targets, face_down, look_only)
}

pub(crate) fn exec_bounce(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let actor = resolve_step_actor(state, step);
    let targets =
        match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    resume_zone_move_batch(state, step, &targets, 0, OP_TAG_BOUNCE)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_move(
    state: &mut GameState,
    step: &EffectStep,
    from: Option<Zone>,
    _to: Option<Zone>,
    select: &SelectionMod,
    _tapped: bool,
    _under_your_control: bool,
    topmost_only: bool,
    _may_shuffle: bool,
    _position: Option<LibraryPosition>,
    from_top: Option<u16>,
    _library_choice_by: Option<LibraryChoiceBy>,
    _random_order: bool,
    _entered_transformed: bool,
    _entered_flipped: bool,
    _enters_tapped_and_attacking: bool,
    _as_enchantment_only: bool,
    _unearth: bool,
    _with_counters: Option<(mtg_ir::CounterId, u16)>,
    _attached_to: Option<&Selector>,
    condition: Option<&Condition>,
) -> Result<StepExecOutcome, EngineError> {
    if let Some(cond) = condition
        && !with_vm_eval(state, |ctx, _| evaluate_condition(state, cond, ctx))
    {
        return Ok(StepExecOutcome::Done);
    }
    let actor = resolve_step_actor(state, step);
    let mut raw_cands = resolve_selector_objects(state, &step.subject);
    if raw_cands.is_empty()
        && let Some(ft) = from_top
        && let Some(p) = state.player(actor)
    {
        let take_n = usize::from(ft).min(p.library.len());
        raw_cands = p.library[p.library.len() - take_n..]
            .iter()
            .copied()
            .rev()
            .collect();
    }
    if let Some(fz) = from {
        raw_cands.retain(|&id| state.object(id).is_some_and(|o| o.zone == fz));
    }
    if topmost_only && raw_cands.len() > 1 {
        raw_cands.truncate(1);
    }
    let default_zm = from.map_or(ZoneMask::ALL, ZoneMask::from_zone);
    let targets = match resolve_selection_mod(state, raw_cands, select, actor, default_zm)? {
        SelectionResult::Paused { prompt_id, chooser } => {
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        SelectionResult::Ready(v) => v,
    };

    resume_zone_move_batch(state, step, &targets, 0, OP_TAG_MOVE)
}

pub(crate) fn exec_sacrifice(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    let players = resolve_target_players(state, step);
    let chooser = players.first().copied().unwrap_or(ctrl);
    let forced_by_opp = chooser != ctrl;

    let mut raw_cands = resolve_selector_objects(state, &step.subject);
    if raw_cands.is_empty() {
        raw_cands = state
            .player(chooser)
            .map(|p| p.battlefield.clone())
            .unwrap_or_default();
    }
    raw_cands.retain(|&id| can_be_sacrificed(state, id, forced_by_opp));

    let targets =
        match resolve_selection_mod(state, raw_cands, select, chooser, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    resume_zone_move_batch(state, step, &targets, 0, OP_TAG_SACRIFICE)
}

pub(crate) fn run_discard_players(
    state: &mut GameState,
    step: &EffectStep,
    players: &[PlayerId],
    select: &SelectionMod,
    amt: u16,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    for (idx, &pid) in players.iter().enumerate() {
        let immune = pid != ctrl
            && state
                .evaluate_board()
                .player(pid)
                .is_some_and(|ep| ep.immune_to_opp_forced_discard);
        if immune {
            continue;
        }
        let hand: Vec<ObjectId> = state
            .player(pid)
            .map(|p| p.hand.clone())
            .unwrap_or_default();
        if hand.is_empty() {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.step_way_stats.player_didnt_discard_nonland_this_way = true;
            }
            continue;
        }
        let mut eff_select = select.clone();
        if eff_select.choice_count.is_none() && amt > 0 {
            eff_select.choice_count = Some(Box::new(mtg_ir::Amount::Const { n: i32::from(amt) }));
        }
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                remaining_players: players[idx + 1..].iter().copied().collect(),
                ..Default::default()
            }));
        }
        let picked = match resolve_selection_mod(state, hand, &eff_select, pid, ZoneMask::HAND)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
        return finish_discard_picked(state, step, picked);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn finish_discard_picked(
    state: &mut GameState,
    step: &EffectStep,
    picked: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    if picked.is_empty() {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.player_didnt_discard_nonland_this_way = true;
        }
        return continue_discard_remaining(state, step);
    }
    match resume_zone_move_batch(state, step, &picked, 0, OP_TAG_DISCARD)? {
        StepExecOutcome::Done => continue_discard_remaining(state, step),
        other => Ok(other),
    }
}

pub(crate) fn exec_discard(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as u16;
    if amt == 0 && select.choice_count.is_none() {
        return Ok(StepExecOutcome::Done);
    }
    run_discard_players(state, step, &players, select, amt)
}

pub(crate) fn run_discard_hand_players(
    state: &mut GameState,
    step: &EffectStep,
    players: &[PlayerId],
    draw_that_many: bool,
    draw_after: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    for (idx, &pid) in players.iter().enumerate() {
        let hand = state
            .player(pid)
            .map(|p| p.hand.clone())
            .unwrap_or_default();
        let last_discarded = hand.len();
        if !hand.is_empty() {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                    remaining_players: players[idx + 1..].iter().copied().collect(),
                    proliferate_chosen_players: smallvec::smallvec![pid],
                    proliferate_count: last_discarded as u32,
                    ..Default::default()
                }));
            }
            let res = resume_zone_move_batch(state, step, &hand, 0, OP_TAG_DISCARD)?;
            if !matches!(res, StepExecOutcome::Done) {
                return Ok(res);
            }
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pending_continuation = None;
                vm.step_way_stats.player_discarded_hand_this_way = true;
            }
        }
        let draw_n = if draw_that_many {
            last_discarded as u32
        } else if let Some(amt) = draw_after {
            with_vm_eval(state, |ctx, _| {
                evaluate_amount(state, amt, ctx).max(0) as u32
            })
        } else {
            0
        };
        for _ in 0..draw_n {
            let _ = zones::draw_one_raw(state, pid)?;
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_discard_hand(
    state: &mut GameState,
    step: &EffectStep,
    draw_that_many: bool,
    draw_after: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    run_discard_hand_players(state, step, &players, draw_that_many, draw_after)
}

pub(crate) fn continue_discard_remaining(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let Some(cont) = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pending_continuation.take())
    else {
        return Ok(StepExecOutcome::Done);
    };
    match &step.op {
        mtg_ir::EffectOp::Discard { select } => {
            if !cont.remaining_players.is_empty() {
                let amt = resolve_step_amount(state, step, 1).max(0) as u16;
                return run_discard_players(state, step, &cont.remaining_players, select, amt);
            }
        }
        mtg_ir::EffectOp::DiscardHand {
            draw_that_many,
            draw_after,
        } => {
            if let Some(&cur_pid) = cont.proliferate_chosen_players.first() {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.step_way_stats.player_discarded_hand_this_way = true;
                }
                let draw_n = if *draw_that_many {
                    cont.proliferate_count
                } else if let Some(amt) = draw_after.as_deref() {
                    with_vm_eval(state, |ctx, _| {
                        evaluate_amount(state, amt, ctx).max(0) as u32
                    })
                } else {
                    0
                };
                for _ in 0..draw_n {
                    let _ = zones::draw_one_raw(state, cur_pid)?;
                }
            }
            if !cont.remaining_players.is_empty() {
                return run_discard_hand_players(
                    state,
                    step,
                    &cont.remaining_players,
                    *draw_that_many,
                    draw_after.as_deref(),
                );
            }
        }
        _ => {}
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_reveal_and_discard(
    state: &mut GameState,
    step: &EffectStep,
    spec: Option<&CoerciveDiscardSpec>,
    partial_reveal: Option<&PartialRevealSpec>,
    coercive_count: Option<&mtg_ir::Amount>,
    include_graveyard: bool,
    _discard_all_matching_sacrificed_colors: bool,
    _discard_all_with_chosen_name: bool,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let Some(target_pid) = players.first().copied() else {
        return Ok(StepExecOutcome::Done);
    };
    let mut hand = state
        .player(target_pid)
        .map(|p| p.hand.clone())
        .unwrap_or_default();
    if include_graveyard && let Some(p) = state.player(target_pid) {
        hand.extend(p.graveyard.iter().copied());
    }
    if hand.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    // Reveal hand (or partial subset).
    match partial_reveal {
        Some(PartialRevealSpec::RandomOne) => {
            let idx = state.rng_mut().next_index(hand.len());
            zones::reveal_to_all(state, hand[idx]);
        }
        Some(PartialRevealSpec::Count { amount }) => {
            let n = with_vm_eval(state, |ctx, _| {
                evaluate_amount(state, amount, ctx).max(0) as usize
            });
            for &id in hand.iter().take(n) {
                zones::reveal_to_all(state, id);
            }
        }
        None => {
            for &id in &hand {
                zones::reveal_to_all(state, id);
            }
        }
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.step_way_stats.did_reveal_this_way = true;
    }

    let Some(cs) = spec else {
        return Ok(StepExecOutcome::Done);
    };
    let chooser = with_vm_eval(state, |ctx, _| {
        crate::eval::resolve_player_ref(state, &cs.chooser, ctx).unwrap_or(ctx.controller)
    });
    let matching: Vec<ObjectId> = if let Some(f) = cs.filter.as_deref() {
        with_vm_eval(state, |ctx, _| {
            hand.iter()
                .copied()
                .filter(|&id| matches_object_filter(state, id, f, ctx))
                .collect()
        })
    } else {
        hand
    };
    if matching.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let count = coercive_count
        .map(|a| with_vm_eval(state, |ctx, _| evaluate_amount(state, a, ctx).max(0) as u16))
        .unwrap_or(1);
    let sel_mod = SelectionMod {
        choice_count: Some(Box::new(mtg_ir::Amount::Const {
            n: i32::from(count),
        })),
        up_to: cs.optional_choice,
        ..Default::default()
    };
    let picked = match resolve_selection_mod(state, matching, &sel_mod, chooser, ZoneMask::HAND)? {
        SelectionResult::Paused { prompt_id, chooser } => {
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        SelectionResult::Ready(v) => v,
    };
    run_reveal_and_discard_picked(state, step, Some(cs), &picked)
}

pub(crate) fn run_reveal_and_discard_picked(
    state: &mut GameState,
    step: &EffectStep,
    spec: Option<&CoerciveDiscardSpec>,
    picked: &[ObjectId],
) -> Result<StepExecOutcome, EngineError> {
    if picked.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let tag = if spec.is_some_and(|cs| cs.destination == Zone::Exile) {
        OP_TAG_EXILE
    } else {
        OP_TAG_DISCARD
    };
    resume_zone_move_batch(state, step, picked, 0, tag)
}

pub(crate) fn exec_mill(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as usize;
    if amt == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let mut total_milled = 0i32;
    let mut to_mill = Vec::new();
    for pid in players {
        let Some(p) = state.player(pid) else {
            continue;
        };
        let n = amt.min(p.library.len());
        for &id in p.library[p.library.len() - n..].iter().rev() {
            to_mill.push(id);
            total_milled += 1;
        }
    }
    for &id in &to_mill {
        let types = {
            let cache = state.evaluate_board();
            let obj = state.object(id);
            obj.map_or(CardTypeMask::empty(), |o| {
                let ctx = EvalContext::new(o.controller).with_board(BoardView::Cached(cache));
                crate::eval::off_battlefield_characteristics(state, id, o, Zone::Library, &ctx)
                    .types
            })
        };
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            if types.contains(CardTypeMask::CREATURE) {
                vm.step_way_stats.creature_card_milled_this_way = true;
            }
            if types.contains(CardTypeMask::LAND) {
                vm.step_way_stats.land_card_milled_this_way = true;
            }
        }
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_effect_amount = total_milled;
    }
    resume_zone_move_batch(state, step, &to_mill, 0, OP_TAG_MILL)
}

pub(crate) fn run_phase_out_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let mut acted = Vec::new();
    for id in targets {
        if zones::phase_out_permanent(state, id) {
            acted.push((id, None));
        }
    }
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_phase_out(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let targets =
        match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    run_phase_out_targets(state, step, targets)
}

pub(crate) fn apply_resolved_create_token(
    state: &mut GameState,
    step: &EffectStep,
    out: &ReplacementOutcome,
) -> Result<(), EngineError> {
    if out.mods.skipped {
        return Ok(());
    }
    let crate::state::ReplaceableEventKind::CreateToken {
        controller,
        spec,
        count,
        source,
    } = &out.event
    else {
        return Ok(());
    };
    let opts = out.mods.token_opts.clone().unwrap_or_default();
    let bind_as = step.bind_as.as_deref();
    let mut created_ids = Vec::new();

    if !out.mods.replace_tokens.is_empty() {
        for (rep_spec, rep_cnt) in &out.mods.replace_tokens {
            let arc_spec = Arc::new(rep_spec.clone());
            for _ in 0..*rep_cnt {
                let tid = zones::create_token_arc_raw(
                    state,
                    *controller,
                    Arc::clone(&arc_spec),
                    opts.clone(),
                )?;
                emit_event(
                    state,
                    AtomicEventKind::TokenCreated {
                        player: *controller,
                        object: tid,
                    },
                );
                created_ids.push(tid);
            }
        }
    } else if let Some(base_spec) = spec.as_deref() {
        let arc_spec = Arc::new(base_spec.clone());
        for _ in 0..*count {
            let tid = zones::create_token_arc_raw(
                state,
                *controller,
                Arc::clone(&arc_spec),
                opts.clone(),
            )?;
            emit_event(
                state,
                AtomicEventKind::TokenCreated {
                    player: *controller,
                    object: tid,
                },
            );
            created_ids.push(tid);
        }
    }

    for (extra_spec, extra_cnt) in &out.mods.extra_tokens {
        let arc_spec = Arc::new(extra_spec.clone());
        for _ in 0..*extra_cnt {
            let tid = zones::create_token_arc_raw(
                state,
                *controller,
                Arc::clone(&arc_spec),
                opts.clone(),
            )?;
            emit_event(
                state,
                AtomicEventKind::TokenCreated {
                    player: *controller,
                    object: tid,
                },
            );
            created_ids.push(tid);
        }
    }

    if opts.attacking && state.combat().active {
        let def = state
            .players()
            .iter()
            .find(|p| !p.lost && p.id != *controller)
            .map(|p| p.id);
        if let Some(did) = def {
            for &tid in &created_ids {
                state
                    .combat_mut()
                    .attackers
                    .push(crate::state::AttackerEntry {
                        attacker: tid,
                        target: AttackTarget::Player { id: did },
                    });
            }
            state.mark_dirty(DirtyMask::COMBAT);
        }
    }

    let _ = source;
    let acted: Vec<_> = created_ids.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, bind_as, &acted);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_token(
    state: &mut GameState,
    step: &EffectStep,
    token: Option<&TokenSpec>,
    dynamic_pt: Option<&(Box<mtg_ir::Amount>, Box<mtg_ir::Amount>)>,
    tapped_and_attacking: bool,
    per_opponent_tapped_attacking: Option<PerOpponentAttackMode>,
    _multiply_by_opponents_did_this_way: bool,
    _benthic_anomaly_clone: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let count = resolve_step_amount(state, step, 1).max(0) as u32;
    let mut eff_spec = token.cloned();
    if let (Some(spec), Some((p_amt, t_amt))) = (eff_spec.as_mut(), dynamic_pt) {
        let (p, t) = with_vm_eval(state, |ctx, _| {
            (
                evaluate_amount(state, p_amt, ctx),
                evaluate_amount(state, t_amt, ctx),
            )
        });
        spec.power = p;
        spec.toughness = t;
    }

    let eff_count = if let Some(mode) = per_opponent_tapped_attacking {
        let opp_count = state
            .players()
            .iter()
            .filter(|p| !p.lost && p.id != ctrl)
            .count() as u32;
        match mode {
            PerOpponentAttackMode::All => opp_count,
            PerOpponentAttackMode::Other => opp_count.saturating_sub(1),
        }
    } else {
        count
    };
    if eff_count == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let tapped = tapped_and_attacking || per_opponent_tapped_attacking.is_some();
    let opts = CreateTokenOptions {
        tapped,
        attacking: tapped,
        created_by_source: src,
        owner: Some(ctrl),
    };
    match replace::resolve_create_token(state, ctrl, eff_spec, eff_count, src, opts)? {
        ReplacementResult::Paused { prompt_id, chooser } => {
            set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_CREATE_TOKEN);
            Ok(StepExecOutcome::Paused { prompt_id, chooser })
        }
        ReplacementResult::Resolved(out) => {
            apply_resolved_create_token(state, step, &out)?;
            Ok(StepExecOutcome::Done)
        }
    }
}

fn resolve_copy_lki(state: &GameState, target: ObjectId) -> Option<Arc<LkiSnapshot>> {
    if let Some(vm) = state.resolution_vms().last() {
        if let Some(ref lki) = vm.source_lki
            && lki.object_id == target
        {
            return Some(Arc::clone(lki));
        }
        if let Some(ref lki) = vm.event_lki
            && (lki.object_id == target || vm.event_object == Some(target))
        {
            return Some(Arc::clone(lki));
        }
    }
    if let Some(frame) = state.resolution_frames().last()
        && let Some(lki) = frame.bindings.iter().rev().find_map(|b| {
            b.objects
                .iter()
                .position(|&id| id == target)
                .and_then(|i| b.lki.get(i))
                .cloned()
        })
    {
        return Some(lki);
    }
    state.lki(target).cloned().map(Arc::new)
}

fn halve_copy_pt(v: i32, round: RoundMode) -> i32 {
    match round {
        RoundMode::Up => (v + 1).div_euclid(2),
        RoundMode::Down => v.div_euclid(2),
    }
}

fn compose_copy_modifications(
    mut base: CopyModificationSpec,
    next: CopyModificationSpec,
) -> CopyModificationSpec {
    let (name, override_name) = if let Some(ov) = next.override_name {
        (next.name.or(base.name), Some(ov))
    } else if next.name.is_some() && !next.keep_self_name {
        (next.name, None)
    } else {
        (base.name, base.override_name)
    };

    let (set_types, add_types) = if next.set_types.is_some() {
        (next.set_types, next.add_types)
    } else {
        (base.set_types, base.add_types | next.add_types)
    };

    let add_supertypes = (base.add_supertypes & !next.remove_supertypes) | next.add_supertypes;
    let remove_supertypes =
        (base.remove_supertypes & !next.add_supertypes) | next.remove_supertypes;

    let (set_subtypes, add_subtypes) = if next.set_subtypes.is_some() {
        (next.set_subtypes, next.add_subtypes)
    } else {
        let mut merged = base.add_subtypes;
        for st in next.add_subtypes {
            if !merged.contains(&st) {
                merged.push(st);
            }
        }
        (base.set_subtypes, merged)
    };

    let (set_colors, add_colors) = if next.set_colors.is_some() {
        (next.set_colors, next.add_colors)
    } else {
        (base.set_colors, base.add_colors | next.add_colors)
    };

    let mut add_keywords = base.add_keywords;
    for kw in next.add_keywords {
        if !add_keywords.contains(&kw) {
            add_keywords.push(kw);
        }
    }

    if let Some(base_round) = base.halve_pt
        && (next.power.is_some() || next.toughness.is_some() || next.halve_pt.is_some())
    {
        base.power = base.power.map(|p| halve_copy_pt(p, base_round));
        base.toughness = base.toughness.map(|t| halve_copy_pt(t, base_round));
    }
    let power = next.power.or(base.power);
    let toughness = next.toughness.or(base.toughness);
    let halve_pt = if next.halve_pt.is_some() {
        next.halve_pt
    } else if next.power.is_none() && next.toughness.is_none() {
        base.halve_pt
    } else {
        None
    };

    let mut added_ability_texts = base.added_ability_texts;
    added_ability_texts.extend(next.added_ability_texts);

    CopyModificationSpec {
        name,
        keep_self_name: next.keep_self_name,
        override_name,
        power,
        toughness,
        set_colors,
        add_colors,
        add_types,
        set_types,
        add_subtypes,
        set_subtypes,
        add_supertypes,
        remove_supertypes,
        add_keywords,
        no_mana_cost: base.no_mana_cost || next.no_mana_cost,
        added_ability_texts,
        halve_pt,
        keep_this_ability: base.keep_this_ability || next.keep_this_ability,
        additional_counters: next.additional_counters,
        conditional_counters: next.conditional_counters,
        conditional_type_counters: next.conditional_type_counters,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_copy_permanent_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    not_legendary: bool,
    tapped: bool,
    tapped_and_attacking: bool,
    per_opponent_tapped_attacking: Option<PerOpponentAttackMode>,
    copy_mod: Option<&CopyModificationSpec>,
    except_pt: Option<(i32, i32)>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let count = resolve_step_amount(state, step, 1).max(0) as u32;
    let copies_per = if per_opponent_tapped_attacking.is_some() {
        state
            .players()
            .iter()
            .filter(|p| !p.lost && p.id != ctrl)
            .count() as u32
    } else {
        count
    };

    let mut mods_owned = copy_mod.cloned();
    if not_legendary {
        let m = mods_owned.get_or_insert_with(CopyModificationSpec::default);
        m.remove_supertypes |= mtg_ir::SupertypeMask::LEGENDARY;
    }
    if let Some((ep, et)) = except_pt {
        let m = mods_owned.get_or_insert_with(CopyModificationSpec::default);
        m.power = Some(ep);
        m.toughness = Some(et);
    }

    let mut created = Vec::new();
    for target in targets {
        for _ in 0..copies_per {
            let tid = if state
                .object(target)
                .is_some_and(|o| o.zone == Zone::Battlefield)
            {
                zones::create_token_copy_raw(
                    state,
                    ctrl,
                    target,
                    mods_owned.clone(),
                    tapped || tapped_and_attacking,
                )?
            } else if let Some(lki) = resolve_copy_lki(state, target) {
                zones::create_token_copy_from_lki_raw(
                    state,
                    ctrl,
                    &lki,
                    mods_owned.clone(),
                    tapped || tapped_and_attacking,
                )?
            } else if state.object(target).is_some() {
                zones::create_token_copy_raw(
                    state,
                    ctrl,
                    target,
                    mods_owned.clone(),
                    tapped || tapped_and_attacking,
                )?
            } else {
                return Err(EngineError::IllegalAction(format!(
                    "source {target:?} does not exist"
                )));
            };
            emit_event(
                state,
                AtomicEventKind::TokenCreated {
                    player: ctrl,
                    object: tid,
                },
            );
            if tapped_and_attacking
                && state.combat().active
                && let Some(did) = state
                    .players()
                    .iter()
                    .find(|p| !p.lost && p.id != ctrl)
                    .map(|p| p.id)
            {
                state
                    .combat_mut()
                    .attackers
                    .push(crate::state::AttackerEntry {
                        attacker: tid,
                        target: AttackTarget::Player { id: did },
                    });
                state.mark_dirty(DirtyMask::COMBAT);
            }
            created.push((tid, None));
        }
    }
    record_step_bindings(state, step.bind_as.as_deref(), &created);
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_copy_permanent(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
    not_legendary: bool,
    tapped: bool,
    tapped_and_attacking: bool,
    per_opponent_tapped_attacking: Option<PerOpponentAttackMode>,
    copy_mod: Option<&CopyModificationSpec>,
    except_pt: Option<(i32, i32)>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let targets =
        match resolve_selection_mod(state, raw_cands, select, ctrl, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    run_copy_permanent_targets(
        state,
        step,
        targets,
        not_legendary,
        tapped,
        tapped_and_attacking,
        per_opponent_tapped_attacking,
        copy_mod,
        except_pt,
    )
}

fn copiable_from_live_object(
    state: &GameState,
    donor: ObjectId,
    donor_obj: &GameObject,
    copy_mod: Option<&CopyModificationSpec>,
) -> (Arc<CopiableState>, Option<Arc<TokenSpec>>) {
    let donor_rare = state.rare(donor).cloned();
    let copiable = if let Some(existing) = donor_rare.as_ref().and_then(|r| r.copiable.as_deref()) {
        let composed_mods = match (existing.modifications.clone(), copy_mod.cloned()) {
            (Some(base), Some(next)) => Some(compose_copy_modifications(base, next)),
            (Some(base), None) => Some(base),
            (None, Some(next)) => Some(next),
            (None, None) => None,
        };
        Arc::new(CopiableState {
            modifications: composed_mods,
            ..existing.clone()
        })
    } else {
        Arc::new(CopiableState {
            card_def: donor_obj.card_def,
            face_index: Some(donor_obj.active_face_index()),
            fused: donor_rare.as_ref().is_some_and(|r| r.fused),
            is_single_face_copy: donor_obj.flags.contains(ObjectFlags::TRANSFORMED)
                || donor_rare.as_ref().is_some_and(|r| r.is_single_face_copy),
            prototyped: donor_obj.flags.contains(ObjectFlags::PROTOTYPED),
            merged_components: donor_rare
                .as_ref()
                .map(|r| r.merged_components.iter().map(|m| m.card_def).collect())
                .unwrap_or_default(),
            except: None,
            modifications: copy_mod.cloned(),
        })
    };
    let ts = (copiable.card_def == TOKEN_CARD_DEF_ID)
        .then(|| {
            donor_rare
                .as_ref()
                .and_then(|r| r.token_spec.as_ref().map(Arc::clone))
        })
        .flatten();
    (copiable, ts)
}

pub(crate) fn exec_become_copy(
    state: &mut GameState,
    step: &EffectStep,
    copy_source: &Selector,
    duration: Option<Duration>,
    copy_mod: Option<&CopyModificationSpec>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let subjects = resolve_selector_objects(state, &step.subject);
    let Some(donor) = resolve_selector_objects(state, copy_source)
        .into_iter()
        .next()
    else {
        return Ok(StepExecOutcome::Done);
    };
    let (copiable, donor_token_spec) = if let Some(donor_obj) = state
        .object(donor)
        .filter(|o| o.zone == Zone::Battlefield)
        .cloned()
    {
        copiable_from_live_object(state, donor, &donor_obj, copy_mod)
    } else if let Some(lki) = resolve_copy_lki(state, donor) {
        let copiable = Arc::new(CopiableState {
            card_def: lki.card_def,
            face_index: Some(lki.active_face_index),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: copy_mod.cloned(),
        });
        let ts = (lki.card_def == TOKEN_CARD_DEF_ID).then(|| {
            let (power, toughness) = lki.pt.map_or((0, 0), |p| (p.power, p.toughness));
            Arc::new(TokenSpec {
                name: lki.name.clone(),
                power,
                toughness,
                colors: lki.colors,
                types: lki.types,
                supertypes: lki.supertypes,
                subtypes: lki.subtypes.iter().copied().collect(),
                type_line: String::new(),
                keywords: lki.keywords.iter().copied().collect(),
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: lki.supertypes.contains(mtg_ir::SupertypeMask::LEGENDARY),
            })
        });
        (copiable, ts)
    } else if let Some(donor_obj) = state.object(donor).cloned() {
        copiable_from_live_object(state, donor, &donor_obj, copy_mod)
    } else {
        return Ok(StepExecOutcome::Done);
    };

    let timed_dur = duration.filter(|&d| d != Duration::Permanent);
    let mut acted = Vec::new();
    for id in subjects {
        if state.object(id).is_none() {
            continue;
        }
        if let Some(dur) = timed_dur {
            let prev_copiable = state.rare(id).and_then(|r| r.copiable.clone());
            let prev_token_spec = state.rare(id).and_then(|r| r.token_spec.clone());
            let (expires_turn, expires_on_player) =
                crate::state::compute_timed_expiry(state, dur, ctrl);
            rare_turn_state_mut(state)
                .timed_copiable_backups
                .push(TimedCopiableBackup {
                    object: id,
                    source: src,
                    prev_copiable,
                    prev_token_spec,
                    duration: dur,
                    expires_turn,
                    expires_on_player,
                });
        }
        if let Some(rare) = state.rare_or_default(id) {
            rare.copiable = Some(Arc::clone(&copiable));
            if let Some(ts) = donor_token_spec.as_ref() {
                rare.token_spec = Some(Arc::clone(ts));
            } else if copiable.card_def != TOKEN_CARD_DEF_ID {
                rare.token_spec = None;
            }
        }
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::CONTINUOUS_EFFECTS);
        acted.push((id, None));
    }
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_create_emblem(
    state: &mut GameState,
    step: &EffectStep,
    abilities: &[String],
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let ts = state.allocate_timestamp();
    let spec = TokenSpec {
        name: "Emblem".into(),
        power: 0,
        toughness: 0,
        colors: mtg_ir::ColorMask::empty(),
        types: CardTypeMask::EMBLEM,
        supertypes: mtg_ir::SupertypeMask::empty(),
        subtypes: SmallVec::new(),
        type_line: "Emblem".into(),
        keywords: SmallVec::new(),
        tapped: false,
        quoted_abilities: abilities.to_vec(),
        legendary: false,
    };
    let rare = RareObjectState {
        token_spec: Some(Arc::new(spec)),
        ..Default::default()
    };
    let obj = GameObject::new(
        TOKEN_CARD_DEF_ID,
        ctrl,
        Zone::Command,
        ts,
        ObjectFlags::IS_EMBLEM,
    );
    let oid = state.insert_object(obj, Some(Arc::new(rare)));
    if let Some(p) = state.player_mut(ctrl) {
        p.command.push(oid);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    append_step_binding(state, step.bind_as.as_deref(), oid, None);
    Ok(StepExecOutcome::Done)
}

#[allow(dead_code)]
fn _keep_used(
    _: SpecialRevealTag,
    _: fn(&GameState, ObjectId, &mtg_ir::ObjectFilter, &EvalContext<'_>) -> bool,
) {
    let _ = find_objects;
    let _ = matches_object_filter;
}
