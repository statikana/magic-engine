use mtg_ir::{
    AbilityDef, Affected, Amount, CardTypeMask, ColorMask, CombatRuleSpec, Compare, CompareOp,
    CounterId, CounterTarget, DesignationKind, DoubleOrTripleSpec, DoubleTarget, Duration,
    EffectOp, EffectStep, KeywordId, Layer, LayerMod, ManaColor, MoveCounterSpec, ObjectFilter,
    ObjectPredicate, ParityFilter, PlayerCounterKind, PlayerRef, RelativeControl, RestrictionKind,
    SelectionMod, Selector, Span, StaticAbility, StaticBody, StaticDef, TimedCombatRestrictionKind,
    TimedCombatRestrictionSpec, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{ChoiceRequest, DecisionState};
use crate::eval::evaluate_amount;
use crate::events::{AtomicEventKind, emit_event};
use crate::faces;
use crate::filter::find_objects;
use crate::id::{ObjectId, PlayerId};
use crate::layers::predicates::{ContinuousTarget, TimedContinuousEffect};
use crate::replace::{self, ReplacementOutcome, ReplacementResult};
use crate::state::{
    DirtyMask, EngineError, GameState, ModePausePurpose, ObjectFlags, PendingPromptContext,
    PendingStepContinuation, PromptOwner, StepPauseState,
};
use crate::zones;

use super::vm::{
    OP_TAG_MONSTROUS, OP_TAG_PROLIFERATE, OP_TAG_PROLIFERATE_COUNTER,
    OP_TAG_PROLIFERATE_PLAYER_COUNTER, OP_TAG_PUT_COUNTERS, OP_TAG_PUT_PLAYER_COUNTERS,
    OP_TAG_TAP_OR_UNTAP, OP_TAG_UNTAP, SelectionResult, StepExecOutcome,
    allocate_continuous_effect_id, append_step_binding, rare_turn_state_mut, record_step_bindings,
    resolve_selection_mod, resolve_selector_objects, resolve_step_actor, resolve_step_amount,
    resolve_target_players, set_waiting_on_replacement, vm_controller, vm_source, with_vm_eval,
};

#[inline]
#[allow(clippy::too_many_arguments)]
fn register_timed_effect_with_color(
    state: &mut GameState,
    controller: PlayerId,
    source: Option<ObjectId>,
    layer: Layer,
    affected: ContinuousTarget,
    modifier: LayerMod,
    duration: Duration,
    chosen_color: Option<ColorMask>,
) {
    let id = allocate_continuous_effect_id(state);
    let ts = state.allocate_timestamp();
    let (expires_turn, expires_on_player) =
        crate::state::compute_timed_expiry(state, duration, controller);
    state.add_continuous_effect(TimedContinuousEffect {
        id,
        source,
        controller,
        layer,
        affected,
        modifier,
        duration,
        timestamp: ts,
        expires_turn,
        expires_on_player,
        chosen_color,
        chosen_subtype: None,
        chosen_player: None,
        chosen_card_type: None,
        locked_pt: None,
        is_cda: false,
    });
}

#[inline]
fn register_timed_effect(
    state: &mut GameState,
    controller: PlayerId,
    source: Option<ObjectId>,
    layer: Layer,
    affected: ContinuousTarget,
    modifier: LayerMod,
    duration: Duration,
) {
    register_timed_effect_with_color(
        state, controller, source, layer, affected, modifier, duration, None,
    );
}

pub(crate) fn apply_resolved_put_counters(
    state: &mut GameState,
    outcome: &ReplacementOutcome,
    actor: PlayerId,
) {
    if outcome.mods.skipped {
        return;
    }
    let crate::state::ReplaceableEventKind::PutCounters {
        object,
        counter,
        count,
        ..
    } = outcome.event
    else {
        return;
    };
    if count > 0 {
        let prev = state.counter_count(object, counter);
        state.add_counter(object, counter, count);
        let next = state.counter_count(object, counter);
        let ts = state.allocate_timestamp();
        if let Some(r) = state.rare_or_default(object) {
            if let Some((_, existing_ts)) = r
                .counter_timestamps
                .iter_mut()
                .find(|(cid, _)| *cid == counter)
            {
                *existing_ts = ts;
            } else {
                r.counter_timestamps.push((counter, ts));
            }
        }
        emit_event(
            state,
            AtomicEventKind::CounterChanged {
                object,
                counter,
                previous_count: prev,
                new_count: next,
                actor: Some(actor),
            },
        );
    }
    for &(ecid, ecnt) in &outcome.mods.extra_counters {
        if ecnt > 0 {
            let prev = state.counter_count(object, ecid);
            state.add_counter(object, ecid, ecnt);
            let next = state.counter_count(object, ecid);
            emit_event(
                state,
                AtomicEventKind::CounterChanged {
                    object,
                    counter: ecid,
                    previous_count: prev,
                    new_count: next,
                    actor: Some(actor),
                },
            );
        }
    }
}

pub(crate) fn apply_resolved_put_player_counters(
    state: &mut GameState,
    outcome: &ReplacementOutcome,
) {
    if outcome.mods.skipped {
        return;
    }
    let crate::state::ReplaceableEventKind::PutPlayerCounters {
        player,
        counter,
        count,
        ..
    } = outcome.event
    else {
        return;
    };
    if count == 0 {
        return;
    }
    let name = state
        .db()
        .counter_name(counter)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if let Some(p) = state.player_mut(player) {
        match name.as_str() {
            "poison" => p.poison = p.poison.saturating_add(count as u16),
            "experience" => p.experience = p.experience.saturating_add(count as u16),
            "energy" => p.energy = p.energy.saturating_add(count as u16),
            "rad" => p.rad = p.rad.saturating_add(count as u16),
            _ => p.poison = p.poison.saturating_add(count as u16),
        }
    }
    state.mark_dirty(DirtyMask::PLAYER_STATE);
    if name == "energy" {
        emit_event(
            state,
            AtomicEventKind::EnergyGained {
                player,
                amount: count as u16,
            },
        );
    }
}

pub(crate) fn apply_resolved_untap(
    state: &mut GameState,
    outcome: &ReplacementOutcome,
    actor: PlayerId,
) {
    if outcome.mods.skipped {
        return;
    }
    let crate::state::ReplaceableEventKind::Untap { object } = outcome.event else {
        return;
    };
    if let Some(o) = state.object_mut(object) {
        o.flags.remove(ObjectFlags::TAPPED);
    }
    state.mark_dirty(DirtyMask::TAPPED);
    emit_event(
        state,
        AtomicEventKind::TapStateChanged {
            object,
            now_tapped: false,
            actor: Some(actor),
        },
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_put_counters_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    counter: Option<CounterId>,
    multi_counters: &[(CounterId, Box<Amount>)],
    divided: bool,
    remove_instead_of_add: bool,
    copy_counters_from: Option<&Selector>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    if let Some(from_sel) = copy_counters_from
        && let Some(donor) = resolve_selector_objects(state, from_sel).into_iter().next()
    {
        let donor_counters = state.counters(donor).to_vec();
        for &tid in &targets {
            for &(cid, cnt) in &donor_counters {
                if let ReplacementResult::Resolved(out) =
                    replace::resolve_put_counters(state, tid, cid, cnt, Some(ctrl), src)?
                {
                    apply_resolved_put_counters(state, &out, ctrl);
                }
            }
        }
        let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
        record_step_bindings(state, step.bind_as.as_deref(), &acted);
        return Ok(StepExecOutcome::Done);
    }

    if !multi_counters.is_empty() {
        let pairs: Vec<(CounterId, u32)> = with_vm_eval(state, |ctx, _| {
            multi_counters
                .iter()
                .map(|(cid, amt)| (*cid, evaluate_amount(state, amt, ctx).max(0) as u32))
                .collect()
        });
        for &tid in &targets {
            for &(cid, cnt) in &pairs {
                if cnt == 0 {
                    continue;
                }
                if remove_instead_of_add {
                    let prev = state.counter_count(tid, cid);
                    let removed = state.remove_counter(tid, cid, cnt);
                    if removed > 0 {
                        emit_event(
                            state,
                            AtomicEventKind::CounterChanged {
                                object: tid,
                                counter: cid,
                                previous_count: prev,
                                new_count: prev.saturating_sub(removed),
                                actor: Some(ctrl),
                            },
                        );
                    }
                } else if let ReplacementResult::Resolved(out) =
                    replace::resolve_put_counters(state, tid, cid, cnt, Some(ctrl), src)?
                {
                    apply_resolved_put_counters(state, &out, ctrl);
                }
            }
        }
        let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
        record_step_bindings(state, step.bind_as.as_deref(), &acted);
        return Ok(StepExecOutcome::Done);
    }

    let cid = counter
        .or_else(|| state.db().counter_id("+1/+1"))
        .unwrap_or(CounterId(0));
    let base_amt = resolve_step_amount(state, step, 1).max(0) as u32;

    if divided {
        let (divs, vm_targets) = state
            .resolution_vms()
            .last()
            .map(|v| (v.divisions.clone(), v.targets.clone()))
            .unwrap_or_default();
        for (tref, d_amt) in divs {
            if let crate::action::TargetRef::Object { id: tid } = tref
                && d_amt > 0
                && vm_targets.contains(&tref)
            {
                if let ReplacementResult::Resolved(out) =
                    replace::resolve_put_counters(state, tid, cid, d_amt, Some(ctrl), src)?
                {
                    apply_resolved_put_counters(state, &out, ctrl);
                }
                append_step_binding(state, step.bind_as.as_deref(), tid, None);
            }
        }
        return Ok(StepExecOutcome::Done);
    }

    for (idx, &tid) in targets.iter().enumerate() {
        if base_amt == 0 {
            continue;
        }
        if remove_instead_of_add {
            let prev = state.counter_count(tid, cid);
            let removed = state.remove_counter(tid, cid, base_amt);
            if removed > 0 {
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object: tid,
                        counter: cid,
                        previous_count: prev,
                        new_count: prev.saturating_sub(removed),
                        actor: Some(ctrl),
                    },
                );
            }
            append_step_binding(state, step.bind_as.as_deref(), tid, None);
            continue;
        }
        match replace::resolve_put_counters(state, tid, cid, base_amt, Some(ctrl), src)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                set_waiting_on_replacement(
                    state,
                    prompt_id,
                    targets.clone(),
                    idx,
                    OP_TAG_PUT_COUNTERS,
                );
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_put_counters(state, &out, ctrl);
                if !out.mods.skipped {
                    append_step_binding(state, step.bind_as.as_deref(), tid, None);
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_put_counters(
    state: &mut GameState,
    step: &EffectStep,
    counter: Option<CounterId>,
    multi_counters: &[(CounterId, Box<Amount>)],
    select: &SelectionMod,
    divided: bool,
    remove_instead_of_add: bool,
    copy_counters_from: Option<&Selector>,
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
    run_put_counters_targets(
        state,
        step,
        targets,
        counter,
        multi_counters,
        divided,
        remove_instead_of_add,
        copy_counters_from,
    )
}

pub(crate) fn exec_remove_counters(
    state: &mut GameState,
    step: &EffectStep,
    counter: Option<CounterId>,
    up_to: bool,
    if_unable_sacrifice_self: bool,
    remove_all_from_target_or_player: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let targets = resolve_selector_objects(state, &step.subject);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;
    let mut total_removed = 0u32;

    for tid in targets {
        if remove_all_from_target_or_player {
            let all = state.counters(tid).to_vec();
            for (cid, cnt) in all {
                let removed = state.remove_counter(tid, cid, cnt);
                total_removed += removed;
                if removed > 0 {
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: tid,
                            counter: cid,
                            previous_count: cnt,
                            new_count: 0,
                            actor: Some(ctrl),
                        },
                    );
                }
            }
        } else if let Some(cid) = counter {
            let prev = state.counter_count(tid, cid);
            if prev < amt && !up_to && if_unable_sacrifice_self {
                continue;
            }
            let removed = state.remove_counter(tid, cid, amt);
            total_removed += removed;
            if removed > 0 {
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object: tid,
                        counter: cid,
                        previous_count: prev,
                        new_count: prev.saturating_sub(removed),
                        actor: Some(ctrl),
                    },
                );
            }
        } else if let Some(&(cid, prev)) = state.counters(tid).first() {
            let removed = state.remove_counter(tid, cid, amt);
            total_removed += removed;
            if removed > 0 {
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object: tid,
                        counter: cid,
                        previous_count: prev,
                        new_count: prev.saturating_sub(removed),
                        actor: Some(ctrl),
                    },
                );
            }
        }
    }

    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_counters_removed = total_removed as i32;
        vm.counters_removed_this_way += total_removed as i32;
    }

    if if_unable_sacrifice_self
        && total_removed < amt
        && let Some(src) = vm_source(state)
        && state
            .object(src)
            .is_some_and(|o| o.zone == Zone::Battlefield)
    {
        let _ = zones::move_object(state, src, Zone::Graveyard, Default::default(), None)?;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_move_counters(
    state: &mut GameState,
    step: &EffectStep,
    counter: Option<CounterId>,
    to_filter: Option<&mtg_ir::ObjectFilter>,
    spec: Option<&MoveCounterSpec>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;

    let (from_objs, to_objs, cid_target, move_all) = if let Some(sp) = spec {
        let froms = resolve_selector_objects(state, &sp.from);
        let tos = resolve_selector_objects(state, &step.subject);
        (froms, tos, sp.counter, sp.all)
    } else {
        let mut objs = resolve_selector_objects(state, &step.subject);
        let from_one = objs.first().copied().into_iter().collect::<Vec<_>>();
        let to_list = if let Some(tf) = to_filter {
            with_vm_eval(state, |ctx, _| find_objects(state, tf, ctx))
        } else if objs.len() >= 2 {
            vec![objs.remove(1)]
        } else {
            Vec::new()
        };
        let ct = counter.map_or(CounterTarget::Any, CounterTarget::Specific);
        (from_one, to_list, ct, false)
    };

    let (Some(from_id), Some(to_id)) = (from_objs.first().copied(), to_objs.first().copied())
    else {
        return Ok(StepExecOutcome::Done);
    };

    let kinds: Vec<(CounterId, u32)> = match cid_target {
        CounterTarget::Specific(cid) => {
            let cur = state.counter_count(from_id, cid);
            let n = if move_all { cur } else { cur.min(amt) };
            vec![(cid, n)]
        }
        CounterTarget::Any | CounterTarget::Each => state
            .counters(from_id)
            .iter()
            .map(|&(cid, cnt)| (cid, if move_all { cnt } else { cnt.min(amt) }))
            .collect(),
    };

    for (cid, n) in kinds {
        if n == 0 {
            continue;
        }
        let prev_from = state.counter_count(from_id, cid);
        let removed = state.remove_counter(from_id, cid, n);
        if removed > 0 {
            emit_event(
                state,
                AtomicEventKind::CounterChanged {
                    object: from_id,
                    counter: cid,
                    previous_count: prev_from,
                    new_count: prev_from.saturating_sub(removed),
                    actor: Some(ctrl),
                },
            );
            if let ReplacementResult::Resolved(out) =
                replace::resolve_put_counters(state, to_id, cid, removed, Some(ctrl), src)?
            {
                apply_resolved_put_counters(state, &out, ctrl);
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_put_player_counters_players(
    state: &mut GameState,
    players: &[PlayerId],
    counter: PlayerCounterKind,
    amt: u32,
) -> Result<StepExecOutcome, EngineError> {
    if amt == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let src = vm_source(state);
    let ctrl = vm_controller(state);
    let cname = match counter {
        PlayerCounterKind::Poison => "poison",
        PlayerCounterKind::Experience => "experience",
        PlayerCounterKind::Energy => "energy",
        PlayerCounterKind::Rad => "rad",
        PlayerCounterKind::Ticket => "ticket",
    };
    let cid = state.db().counter_id(cname).unwrap_or(CounterId(0));
    for (idx, &pid) in players.iter().enumerate() {
        match replace::resolve_put_player_counters(state, pid, cid, amt, Some(ctrl), src)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                        remaining_players: players[idx + 1..].iter().copied().collect(),
                        ..Default::default()
                    }));
                }
                set_waiting_on_replacement(
                    state,
                    prompt_id,
                    Vec::new(),
                    0,
                    OP_TAG_PUT_PLAYER_COUNTERS,
                );
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_put_player_counters(state, &out);
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_put_player_counters(
    state: &mut GameState,
    step: &EffectStep,
    counter: PlayerCounterKind,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;
    run_put_player_counters_players(state, &players, counter, amt)
}

pub(crate) fn run_proliferate_counter_queue(
    state: &mut GameState,
    chooser: PlayerId,
    queue: &[(ObjectId, CounterId)],
) -> Result<StepExecOutcome, EngineError> {
    let src = vm_source(state);
    for (idx, &(tid, cid)) in queue.iter().enumerate() {
        match replace::resolve_put_counters(state, tid, cid, 1, Some(chooser), src)? {
            ReplacementResult::Paused {
                prompt_id,
                chooser: rep_chooser,
            } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                        proliferate_counter_queue: queue[idx + 1..].to_vec(),
                        proliferate_chosen_players: smallvec::smallvec![chooser],
                        ..Default::default()
                    }));
                }
                set_waiting_on_replacement(
                    state,
                    prompt_id,
                    Vec::new(),
                    0,
                    OP_TAG_PROLIFERATE_COUNTER,
                );
                return Ok(StepExecOutcome::Paused {
                    prompt_id,
                    chooser: rep_chooser,
                });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_put_counters(state, &out, chooser);
            }
        }
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pending_continuation = None;
    }
    emit_event(state, AtomicEventKind::Proliferated { player: chooser });
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_proliferate_player_counters_from(
    state: &mut GameState,
    chooser: PlayerId,
    mut cont: PendingStepContinuation,
    start_flat_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let count = cont.proliferate_count.max(1);
    let n_players = cont.proliferate_chosen_players.len();
    if n_players > 0 {
        let src = vm_source(state);
        let total = (count as usize) * n_players * 4;
        for flat_idx in start_flat_idx..total {
            let rem = flat_idx % (n_players * 4);
            let p_idx = rem / 4;
            let kind_idx = rem % 4;
            let pid = cont.proliferate_chosen_players[p_idx];
            let has_counter = state.player(pid).is_some_and(|p| match kind_idx {
                0 => p.poison > 0,
                1 => p.rad > 0,
                2 => p.experience > 0,
                _ => p.energy > 0,
            });
            if !has_counter {
                continue;
            }
            let cname = match kind_idx {
                0 => "poison",
                1 => "rad",
                2 => "experience",
                _ => "energy",
            };
            if let Some(cid) = state.db().counter_id(cname) {
                match replace::resolve_put_player_counters(state, pid, cid, 1, Some(chooser), src)?
                {
                    ReplacementResult::Paused {
                        prompt_id,
                        chooser: rep_chooser,
                    } => {
                        cont.proliferate_count = count;
                        cont.remaining_players = smallvec::smallvec![chooser];
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pending_continuation = Some(Box::new(cont));
                        }
                        set_waiting_on_replacement(
                            state,
                            prompt_id,
                            Vec::new(),
                            flat_idx,
                            OP_TAG_PROLIFERATE_PLAYER_COUNTER,
                        );
                        return Ok(StepExecOutcome::Paused {
                            prompt_id,
                            chooser: rep_chooser,
                        });
                    }
                    ReplacementResult::Resolved(out) => {
                        apply_resolved_put_player_counters(state, &out);
                    }
                }
            } else {
                if let Some(p) = state.player_mut(pid) {
                    match kind_idx {
                        0 => p.poison = p.poison.saturating_add(1),
                        1 => p.rad = p.rad.saturating_add(1),
                        2 => p.experience = p.experience.saturating_add(1),
                        _ => p.energy = p.energy.saturating_add(1),
                    }
                }
                state.mark_dirty(DirtyMask::PLAYER_STATE);
                if kind_idx == 3 {
                    emit_event(
                        state,
                        AtomicEventKind::EnergyGained {
                            player: pid,
                            amount: 1,
                        },
                    );
                }
            }
        }
    }

    let mut counter_queue: Vec<(ObjectId, CounterId)> = Vec::new();
    for _ in 0..count {
        for &id in &cont.proliferate_selected_perms {
            for &(cid, _) in state.counters(id) {
                counter_queue.push((id, cid));
            }
        }
    }
    run_proliferate_counter_queue(state, chooser, &counter_queue)
}

pub(crate) fn finish_proliferate_choices(
    state: &mut GameState,
    chooser: PlayerId,
    chosen_players: &[PlayerId],
) -> Result<StepExecOutcome, EngineError> {
    let mut cont = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pending_continuation.take())
        .unwrap_or_default();
    cont.proliferate_chosen_players = chosen_players.iter().copied().collect();
    run_proliferate_player_counters_from(state, chooser, *cont, 0)
}

pub(crate) fn resume_proliferate_players_choice(
    state: &mut GameState,
    chooser: PlayerId,
    picked: &[u16],
) -> Result<StepExecOutcome, EngineError> {
    let candidates = state
        .resolution_vms()
        .last()
        .and_then(|vm| vm.pending_continuation.as_ref())
        .map(|c| c.remaining_players.clone())
        .unwrap_or_default();
    let chosen: SmallVec<[PlayerId; 4]> = picked
        .iter()
        .filter_map(|&idx| candidates.get(usize::from(idx)).copied())
        .collect();
    finish_proliferate_choices(state, chooser, &chosen)
}

pub(crate) fn after_proliferate_perm_selection(
    state: &mut GameState,
    chooser: PlayerId,
    selected_perms: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let player_cands: SmallVec<[PlayerId; 4]> = state
        .players()
        .iter()
        .filter(|p| !p.lost && (p.poison > 0 || p.rad > 0 || p.experience > 0 || p.energy > 0))
        .map(|p| p.id)
        .collect();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        let cont = vm
            .pending_continuation
            .get_or_insert_with(|| Box::new(PendingStepContinuation::default()));
        cont.proliferate_selected_perms = selected_perms;
        cont.remaining_players = player_cands.clone();
    }
    if player_cands.is_empty() {
        return finish_proliferate_choices(state, chooser, &[]);
    }

    let options: Vec<String> = player_cands
        .iter()
        .map(|pid| format!("Player {}", pid.0))
        .collect();
    let opt_cnt = options.len() as u16;
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::ChooseMode {
            chooser,
            purpose: ModePausePurpose::ProliferatePlayers,
            options,
            picked: SmallVec::new(),
            allow_duplicates: false,
            min: 0,
            max: opt_cnt,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseMode {
        prompt_id,
        option_count: opt_cnt,
        min: 0,
        max: opt_cnt,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn start_proliferate_selection(
    state: &mut GameState,
    chooser: PlayerId,
    count: u32,
) -> Result<StepExecOutcome, EngineError> {
    if count == 0 {
        return Ok(StepExecOutcome::Done);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pending_continuation = Some(Box::new(PendingStepContinuation {
            proliferate_count: count,
            ..Default::default()
        }));
    }
    let cands: Vec<ObjectId> = state
        .players()
        .iter()
        .flat_map(|p| p.battlefield.iter().copied())
        .filter(|&id| !state.counters(id).is_empty())
        .collect();
    if cands.is_empty() {
        return after_proliferate_perm_selection(state, chooser, Vec::new());
    }

    let max = cands.len() as u16;
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::SelectionPick {
            chooser,
            candidates: cands.clone(),
            selected: Vec::new(),
            min: 0,
            max,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseObjects {
        prompt_id,
        candidates: cands,
        min: 0,
        max,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn exec_proliferate(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    match replace::resolve_proliferate(state, ctrl, 1, src)? {
        ReplacementResult::Paused { prompt_id, chooser } => {
            set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_PROLIFERATE);
            Ok(StepExecOutcome::Paused { prompt_id, chooser })
        }
        ReplacementResult::Resolved(out) => {
            if out.mods.skipped {
                return Ok(StepExecOutcome::Done);
            }
            let (player, count) = match out.event {
                crate::state::ReplaceableEventKind::Proliferate { player, count, .. } => {
                    (player, count)
                }
                _ => (ctrl, 1),
            };
            start_proliferate_selection(state, player, count)
        }
    }
}

pub(crate) fn exec_pump(
    state: &mut GameState,
    step: &EffectStep,
    toughness: Option<&Amount>,
    duration: Option<Duration>,
    pump_choices: &[(i32, i32)],
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let objs = resolve_selector_objects(state, &step.subject);
    let targets: SmallVec<[ObjectId; 4]> = objs
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let (pw, tg) = if let Some(&(cp, ct)) = pump_choices.first() {
        (Amount::Const { n: cp }, Amount::Const { n: ct })
    } else {
        let p = resolve_step_amount(state, step, 0);
        let t = toughness
            .map(|ta| with_vm_eval(state, |ctx, _| evaluate_amount(state, ta, ctx)))
            .unwrap_or(p);
        (Amount::Const { n: p }, Amount::Const { n: t })
    };
    let dur = duration.unwrap_or(Duration::EndOfTurn);
    register_timed_effect(
        state,
        ctrl,
        src,
        Layer::Layer7c,
        ContinuousTarget::Objects(targets.clone()),
        LayerMod::Pt {
            power: Box::new(pw),
            toughness: Box::new(tg),
        },
        dur,
    );
    let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_switch_pt(
    state: &mut GameState,
    step: &EffectStep,
    duration: Option<Duration>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    register_timed_effect(
        state,
        ctrl,
        src,
        Layer::Layer7d,
        ContinuousTarget::Objects(targets.clone()),
        LayerMod::SwitchPt,
        duration.unwrap_or(Duration::EndOfTurn),
    );
    let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_grant_modifiers(
    state: &mut GameState,
    step: &EffectStep,
    targets: &SmallVec<[ObjectId; 4]>,
    keywords: &[KeywordId],
    remove_keywords: &[KeywordId],
    cant_have_or_gain: bool,
    dur: Duration,
    remove_all_abilities: bool,
) {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    if remove_all_abilities {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer6,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            dur,
        );
    }
    if !keywords.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer6,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::AddKeywords {
                keywords: keywords.iter().copied().collect(),
                protection_attachment_exemption: None,
            },
            dur,
        );
    }
    if !remove_keywords.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer6,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::RemoveKeywords {
                keywords: remove_keywords.iter().copied().collect(),
                cant_have_or_gain,
            },
            dur,
        );
    }
    let acted: Vec<_> = targets.iter().copied().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
}

pub(crate) fn resume_grant_keyword_choice(
    state: &mut GameState,
    step: &EffectStep,
    options: &[String],
    idx: u16,
) -> Result<StepExecOutcome, EngineError> {
    let EffectOp::Grant {
        keywords,
        remove_keywords,
        cant_have_or_gain,
        duration,
        choose_keyword_options,
        choose_lose_keyword_options,
        choose_protection_color,
        remove_all_abilities,
        ..
    } = &step.op
    else {
        return Ok(StepExecOutcome::Done);
    };

    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let dur = duration.unwrap_or(Duration::EndOfTurn);

    if *choose_protection_color {
        let mask = options
            .get(usize::from(idx))
            .map(|s| match s.to_ascii_lowercase().as_str() {
                "white" => ColorMask::W,
                "blue" => ColorMask::U,
                "black" => ColorMask::B,
                "red" => ColorMask::R,
                "green" => ColorMask::G,
                _ => ColorMask::empty(),
            })
            .unwrap_or(ColorMask::empty());
        if !mask.is_empty() {
            register_timed_effect_with_color(
                state,
                ctrl,
                src,
                Layer::Layer6,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::ProtectionFromChosenColor {
                    protection_attachment_exemption: None,
                },
                dur,
                Some(mask),
            );
        }
    } else if !choose_keyword_options.is_empty() {
        let kid = choose_keyword_options
            .get(usize::from(idx))
            .copied()
            .or_else(|| {
                options
                    .get(usize::from(idx))
                    .and_then(|s| crate::keywords::keyword_id_ci(state.db(), s))
            });
        if let Some(k) = kid {
            register_timed_effect(
                state,
                ctrl,
                src,
                Layer::Layer6,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::AddKeywords {
                    keywords: smallvec::smallvec![k],
                    protection_attachment_exemption: None,
                },
                dur,
            );
        }
    } else if !choose_lose_keyword_options.is_empty() {
        let kid = choose_lose_keyword_options
            .get(usize::from(idx))
            .copied()
            .or_else(|| {
                options
                    .get(usize::from(idx))
                    .and_then(|s| crate::keywords::keyword_id_ci(state.db(), s))
            });
        if let Some(k) = kid {
            register_timed_effect(
                state,
                ctrl,
                src,
                Layer::Layer6,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::RemoveKeywords {
                    keywords: smallvec::smallvec![k],
                    cant_have_or_gain: *cant_have_or_gain,
                },
                dur,
            );
        }
    }

    apply_grant_modifiers(
        state,
        step,
        &targets,
        keywords,
        remove_keywords,
        *cant_have_or_gain,
        dur,
        *remove_all_abilities,
    );
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_grant(
    state: &mut GameState,
    step: &EffectStep,
    keywords: &[KeywordId],
    remove_keywords: &[KeywordId],
    cant_have_or_gain: bool,
    duration: Option<Duration>,
    choose_keyword_options: &[KeywordId],
    choose_lose_keyword_options: &[KeywordId],
    choose_protection_color: bool,
    remove_all_abilities: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let dur = duration.unwrap_or(Duration::EndOfTurn);

    if choose_protection_color
        || !choose_keyword_options.is_empty()
        || !choose_lose_keyword_options.is_empty()
    {
        let options: Vec<String> = if choose_protection_color {
            vec![
                "white".into(),
                "blue".into(),
                "black".into(),
                "red".into(),
                "green".into(),
            ]
        } else {
            let list = if !choose_keyword_options.is_empty() {
                choose_keyword_options
            } else {
                choose_lose_keyword_options
            };
            list.iter()
                .map(|&kid| {
                    state
                        .db()
                        .keyword_name(kid)
                        .unwrap_or("keyword")
                        .to_string()
                })
                .collect()
        };
        let prompt_id = state.allocate_prompt_id();
        let opt_cnt = options.len() as u16;
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::ChooseMode {
                chooser: ctrl,
                purpose: ModePausePurpose::GrantKeywordChoice,
                options,
                picked: SmallVec::new(),
                allow_duplicates: false,
                min: 1,
                max: 1,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: opt_cnt,
            min: 1,
            max: 1,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: ctrl,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(ctrl);
        return Ok(StepExecOutcome::Paused {
            prompt_id,
            chooser: ctrl,
        });
    }

    apply_grant_modifiers(
        state,
        step,
        &targets,
        keywords,
        remove_keywords,
        cant_have_or_gain,
        dur,
        remove_all_abilities,
    );
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_animate(
    state: &mut GameState,
    step: &EffectStep,
    duration: Option<Duration>,
    add_types: CardTypeMask,
    set_types: Option<CardTypeMask>,
    remove_types: CardTypeMask,
    add_subtypes: &[mtg_ir::SubtypeId],
    set_subtypes: Option<&[mtg_ir::SubtypeId]>,
    set_colors: Option<ColorMask>,
    add_colors: ColorMask,
    add_keywords: &[KeywordId],
    set_power: Option<&Amount>,
    set_toughness: Option<&Amount>,
    remove_all_abilities: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let dur = duration.unwrap_or(Duration::EndOfTurn);

    if let Some(types) = set_types {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer4,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::SetTypes {
                types,
                subtypes: set_subtypes.map_or_else(SmallVec::new, |s| s.iter().copied().collect()),
            },
            dur,
        );
    } else if !add_types.is_empty() || !add_subtypes.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer4,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::AddTypes {
                types: add_types,
                subtypes: add_subtypes.iter().copied().collect(),
            },
            dur,
        );
    }
    if !remove_types.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer4,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::RemoveTypes {
                types: remove_types,
            },
            dur,
        );
    }
    if set_types.is_none()
        && let Some(sts) = set_subtypes
    {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer4,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::SetSubtypes {
                subtypes: sts.iter().copied().collect(),
            },
            dur,
        );
    }
    if let Some(c) = set_colors {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer5,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::SetColors { colors: c },
            dur,
        );
    } else if !add_colors.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer5,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::AddColors { colors: add_colors },
            dur,
        );
    }
    if remove_all_abilities {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer6,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            dur,
        );
    }
    if !add_keywords.is_empty() {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer6,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::AddKeywords {
                keywords: add_keywords.iter().copied().collect(),
                protection_attachment_exemption: None,
            },
            dur,
        );
    }
    match (set_power, set_toughness) {
        (Some(p), Some(t)) => {
            register_timed_effect(
                state,
                ctrl,
                src,
                Layer::Layer7b,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::SetBasePt {
                    power: Box::new(p.clone()),
                    toughness: Box::new(t.clone()),
                },
                dur,
            );
        }
        (Some(p), None) => {
            register_timed_effect(
                state,
                ctrl,
                src,
                Layer::Layer7b,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::SetBasePower {
                    power: Box::new(p.clone()),
                },
                dur,
            );
        }
        (None, Some(t)) => {
            register_timed_effect(
                state,
                ctrl,
                src,
                Layer::Layer7b,
                ContinuousTarget::Objects(targets.clone()),
                LayerMod::SetBaseToughness {
                    toughness: Box::new(t.clone()),
                },
                dur,
            );
        }
        (None, None) => {}
    }
    let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_tap_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let mut acted = Vec::new();
    for id in targets {
        if let Some(o) = state.object_mut(id)
            && o.zone == Zone::Battlefield
            && !o.flags.contains(ObjectFlags::TAPPED)
        {
            o.flags.insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: id,
                    now_tapped: true,
                    actor: Some(actor),
                },
            );
            acted.push((id, None));
        }
    }
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_tap(
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
    run_tap_targets(state, step, targets)
}

pub(crate) fn run_untap_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: &[ObjectId],
    start_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let bind_on_untap = !matches!(step.op, EffectOp::GainControl { .. });
    for (offset, &id) in targets[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        if !state
            .object(id)
            .is_some_and(|o| o.zone == Zone::Battlefield && o.flags.contains(ObjectFlags::TAPPED))
        {
            continue;
        }
        match replace::resolve_untap(state, id)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                set_waiting_on_replacement(state, prompt_id, targets.to_vec(), idx, OP_TAG_UNTAP);
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_untap(state, &out, actor);
                if bind_on_untap && !out.mods.skipped {
                    append_step_binding(state, step.bind_as.as_deref(), id, None);
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_untap(
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
    run_untap_targets(state, step, &targets, 0)
}

pub(crate) fn run_tap_or_untap_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: &[ObjectId],
    start_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    for (offset, &id) in targets[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        let is_tapped = state
            .object(id)
            .is_some_and(|o| o.zone == Zone::Battlefield && o.flags.contains(ObjectFlags::TAPPED));
        if is_tapped {
            match replace::resolve_untap(state, id)? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    set_waiting_on_replacement(
                        state,
                        prompt_id,
                        targets.to_vec(),
                        idx,
                        OP_TAG_TAP_OR_UNTAP,
                    );
                    return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                }
                ReplacementResult::Resolved(out) => {
                    apply_resolved_untap(state, &out, actor);
                }
            }
        } else if let Some(o) = state.object_mut(id)
            && o.zone == Zone::Battlefield
        {
            o.flags.insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: id,
                    now_tapped: true,
                    actor: Some(actor),
                },
            );
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_tap_or_untap(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    run_tap_or_untap_targets(state, step, &targets, 0)
}

pub(crate) fn exec_freeze_untap(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    for id in targets {
        if let Some(r) = state.rare_or_default(id) {
            r.doesnt_untap_next_untap = true;
        }
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_goad(
    state: &mut GameState,
    step: &EffectStep,
    duration: Option<Duration>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    let src = vm_source(state);
    let dur = duration.unwrap_or(Duration::UntilYourNextTurn);
    let targets = resolve_selector_objects(state, &step.subject);
    let affected: SmallVec<[ObjectId; 4]> = targets
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if !affected.is_empty() {
        register_layer6_static_effect(
            state,
            ctrl,
            src,
            ContinuousTarget::Objects(affected),
            StaticBody::CombatRule {
                affected: Affected::SelfObj,
                rule: CombatRuleSpec::IsGoaded,
            },
            dur,
        );
        state.mark_dirty(DirtyMask::COMBAT | DirtyMask::DESIGNATIONS);
    }
    Ok(StepExecOutcome::Done)
}

fn register_layer6_static_effect(
    state: &mut GameState,
    ctrl: PlayerId,
    src: Option<ObjectId>,
    affected: ContinuousTarget,
    body: StaticBody,
    duration: Duration,
) {
    register_timed_effect(
        state,
        ctrl,
        src,
        Layer::Layer6,
        affected,
        LayerMod::GrantAbility {
            ability: Box::new(AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body,
                    condition: None,
                },
                span: Span::default(),
            })),
        },
        duration,
    );
}

pub(crate) fn register_odd_or_even_cant_block(
    state: &mut GameState,
    step: &EffectStep,
    parity: ParityFilter,
    duration: Duration,
) {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let filter = ObjectFilter {
        zones: ZoneMask::BATTLEFIELD,
        types_any: CardTypeMask::CREATURE,
        extra: vec![ObjectPredicate::ManaValueParity { parity }],
        ..Default::default()
    };
    register_layer6_static_effect(
        state,
        ctrl,
        src,
        ContinuousTarget::Filter(filter),
        StaticBody::Restriction {
            affected: Affected::SelfObj,
            restriction: RestrictionKind::CantBlock,
        },
        duration,
    );
    state.mark_dirty(DirtyMask::COMBAT);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_combat_restriction(
    state: &mut GameState,
    step: &EffectStep,
    spec: Option<&TimedCombatRestrictionSpec>,
    duration: Option<Duration>,
    choose_odd_or_even_cant_block: bool,
    must_attack_next_combat: bool,
    must_attack_player: Option<&PlayerRef>,
    until_your_next_turn_directive: Option<(bool, bool)>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    if choose_odd_or_even_cant_block {
        let prompt_id = state.allocate_prompt_id();
        let options = vec!["Odd".into(), "Even".into()];
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::ChooseMode {
                chooser: ctrl,
                purpose: ModePausePurpose::OddOrEvenChoice,
                options,
                picked: SmallVec::new(),
                allow_duplicates: false,
                min: 1,
                max: 1,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: 2,
            min: 1,
            max: 1,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: ctrl,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(ctrl);
        return Ok(StepExecOutcome::Paused {
            prompt_id,
            chooser: ctrl,
        });
    }

    if let Some((must_attack, avoid_you)) = until_your_next_turn_directive {
        let opp_creatures = ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_any: CardTypeMask::CREATURE,
            controller: Some(RelativeControl::Opponent),
            ..Default::default()
        };
        if avoid_you {
            register_layer6_static_effect(
                state,
                ctrl,
                src,
                ContinuousTarget::Filter(opp_creatures),
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::IsGoaded,
                },
                Duration::UntilYourNextTurn,
            );
        } else if must_attack {
            register_layer6_static_effect(
                state,
                ctrl,
                src,
                ContinuousTarget::Filter(opp_creatures),
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::AttacksEachCombat,
                },
                Duration::UntilYourNextTurn,
            );
        }
        state.mark_dirty(DirtyMask::COMBAT);
    }

    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    if must_attack_next_combat {
        let rts = rare_turn_state_mut(state);
        for &id in &targets {
            if !rts.must_attack_next_combat.contains(&id) {
                rts.must_attack_next_combat.push(id);
            }
        }
        state.mark_dirty(DirtyMask::COMBAT);
    }

    if let Some(pr) = must_attack_player
        && let Some(req_pid) = with_vm_eval(state, |ctx, _| {
            crate::eval::resolve_player_ref(state, pr, ctx)
        })
    {
        let rts = rare_turn_state_mut(state);
        for &id in &targets {
            if !rts.must_attack_player_this_turn.contains(&(id, req_pid)) {
                rts.must_attack_player_this_turn.push((id, req_pid));
            }
        }
        state.mark_dirty(DirtyMask::COMBAT);
    }

    if let Some(sp) = spec {
        let dur = duration.unwrap_or(Duration::EndOfTurn);
        let affected = ContinuousTarget::Objects(targets.clone());
        match sp.kind {
            TimedCombatRestrictionKind::CantBlock => {
                register_layer6_static_effect(
                    state,
                    ctrl,
                    src,
                    affected,
                    StaticBody::Restriction {
                        affected: Affected::SelfObj,
                        restriction: RestrictionKind::CantBlock,
                    },
                    dur,
                );
            }
            TimedCombatRestrictionKind::CantAttack => {
                register_layer6_static_effect(
                    state,
                    ctrl,
                    src,
                    affected,
                    StaticBody::Restriction {
                        affected: Affected::SelfObj,
                        restriction: RestrictionKind::CantAttack,
                    },
                    dur,
                );
            }
            TimedCombatRestrictionKind::CantAttackOrBlock => {
                register_layer6_static_effect(
                    state,
                    ctrl,
                    src,
                    affected,
                    StaticBody::Restriction {
                        affected: Affected::SelfObj,
                        restriction: RestrictionKind::CantAttackOrBlock,
                    },
                    dur,
                );
            }
            TimedCombatRestrictionKind::CantBeBlocked => match &sp.except_by {
                None => {
                    register_layer6_static_effect(
                        state,
                        ctrl,
                        src,
                        affected,
                        StaticBody::Restriction {
                            affected: Affected::SelfObj,
                            restriction: RestrictionKind::CantBeBlocked,
                        },
                        dur,
                    );
                }
                Some(eb) => {
                    let mut registered_any = false;
                    if let Some(min) = eb.min_blockers {
                        register_layer6_static_effect(
                            state,
                            ctrl,
                            src,
                            affected.clone(),
                            StaticBody::CombatRule {
                                affected: Affected::SelfObj,
                                rule: CombatRuleSpec::MinBlockers { min },
                            },
                            dur,
                        );
                        registered_any = true;
                    }
                    let has_filter_constraint = !eb.types.is_empty()
                        || !eb.subtypes.is_empty()
                        || !eb.colors.is_empty()
                        || !eb.not_colors.is_empty()
                        || !eb.keywords.is_empty()
                        || eb.max_power.is_some();
                    let base = ObjectFilter {
                        types_any: eb.types,
                        subtypes_any: eb.subtypes.iter().copied().collect(),
                        colors_any: eb.colors,
                        colors_none: eb.not_colors,
                        power: eb.max_power.map(|n| Compare {
                            op: CompareOp::Le,
                            value: Box::new(Amount::Const { n }),
                        }),
                        extra: if eb.keywords.is_empty() {
                            Vec::new()
                        } else {
                            vec![ObjectPredicate::HasKeywords {
                                keywords: eb.keywords.clone(),
                            }]
                        },
                        ..Default::default()
                    };
                    if !eb.artifact_or_colors.is_empty() {
                        let mut f1 = base.clone();
                        f1.types_any |= CardTypeMask::ARTIFACT;
                        let mut f2 = base;
                        f2.colors_any |= eb.artifact_or_colors;
                        register_layer6_static_effect(
                            state,
                            ctrl,
                            src,
                            affected.clone(),
                            StaticBody::CombatRule {
                                affected: Affected::SelfObj,
                                rule: CombatRuleSpec::CantBeBlockedExceptBy {
                                    filter: Box::new(f1),
                                    or_filter: Some(Box::new(f2)),
                                },
                            },
                            dur,
                        );
                        registered_any = true;
                    } else if has_filter_constraint {
                        register_layer6_static_effect(
                            state,
                            ctrl,
                            src,
                            affected.clone(),
                            StaticBody::CombatRule {
                                affected: Affected::SelfObj,
                                rule: CombatRuleSpec::CantBeBlockedExceptBy {
                                    filter: Box::new(base),
                                    or_filter: None,
                                },
                            },
                            dur,
                        );
                        registered_any = true;
                    }
                    if !registered_any {
                        register_layer6_static_effect(
                            state,
                            ctrl,
                            src,
                            affected,
                            StaticBody::Restriction {
                                affected: Affected::SelfObj,
                                restriction: RestrictionKind::CantBeBlocked,
                            },
                            dur,
                        );
                    }
                }
            },
        }
        state.mark_dirty(DirtyMask::COMBAT);
    }

    let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_remove_from_combat(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    for id in targets {
        crate::combat::remove_from_combat(state, id);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_gain_control(
    state: &mut GameState,
    step: &EffectStep,
    duration: Option<Duration>,
    untap: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let targets: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .filter(|&id| {
            state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield)
        })
        .collect();
    if targets.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let dur = duration.unwrap_or(Duration::Permanent);
    if dur == Duration::Permanent {
        for &id in &targets {
            zones::transfer_control_raw(state, id, ctrl)?;
        }
    } else {
        register_timed_effect(
            state,
            ctrl,
            src,
            Layer::Layer2,
            ContinuousTarget::Objects(targets.clone()),
            LayerMod::SetController {
                controller: PlayerRef::You,
            },
            dur,
        );
    }
    let acted: Vec<_> = targets.iter().copied().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    if untap {
        return run_untap_targets(state, step, &targets, 0);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_attach_targets(
    state: &mut GameState,
    step: &EffectStep,
    to_attach: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let hosts = resolve_selector_objects(state, &step.subject);
    let Some(host) = hosts.first().copied() else {
        return Ok(StepExecOutcome::Done);
    };
    let mut acted = Vec::new();
    for att in to_attach {
        if state
            .object(att)
            .is_some_and(|o| o.zone == Zone::Battlefield)
            && state
                .object(host)
                .is_some_and(|o| o.zone == Zone::Battlefield)
            && att != host
        {
            zones::attach_raw(state, att, host)?;
            emit_event(
                state,
                AtomicEventKind::AttachmentChanged {
                    attachment: att,
                    host,
                    attached: true,
                },
            );
            acted.push((att, None));
        }
    }
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_attach(
    state: &mut GameState,
    step: &EffectStep,
    attach_object: Option<&Selector>,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let hosts = resolve_selector_objects(state, &step.subject);
    if hosts.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let att_cands = match attach_object {
        Some(sel) => resolve_selector_objects(state, sel),
        None => vm_source(state).into_iter().collect(),
    };
    let to_attach =
        match resolve_selection_mod(state, att_cands, select, ctrl, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    run_attach_targets(state, step, to_attach)
}

pub(crate) fn exec_transform(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    for id in targets {
        if faces::can_transform_permanent(state, id) {
            let fi = faces::transform_permanent(state, id)?;
            emit_event(
                state,
                AtomicEventKind::Transformed {
                    object: id,
                    to_face_index: fi,
                },
            );
            append_step_binding(state, step.bind_as.as_deref(), id, None);
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_flip(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    for id in targets {
        if faces::can_flip_permanent(state, id) {
            faces::flip_permanent(state, id)?;
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_turn_face_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    face_up: bool,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let mut acted = Vec::new();
    for id in targets {
        if face_up && faces::can_turn_face_up(state, id) {
            faces::turn_face_up(state, id)?;
            emit_event(
                state,
                AtomicEventKind::TurnedFaceUp {
                    object: id,
                    player: actor,
                    x_value: None,
                },
            );
            acted.push((id, None));
        } else if !face_up
            && let Some(o) = state.object_mut(id)
            && o.zone == Zone::Battlefield
            && !o.is_face_down()
        {
            o.flags.insert(ObjectFlags::FACE_DOWN);
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
            acted.push((id, None));
        }
    }
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_turn_face(
    state: &mut GameState,
    step: &EffectStep,
    face_up: bool,
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
    run_turn_face_targets(state, step, targets, face_up)
}

pub(crate) fn exec_double_or_triple(
    state: &mut GameState,
    step: &EffectStep,
    spec: &DoubleOrTripleSpec,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let factor = i32::from(spec.factor.max(1));
    let repeat_n = spec.repeat_factor_count.as_deref().map_or(1, |a| {
        with_vm_eval(state, |ctx, _| evaluate_amount(state, a, ctx).max(0))
    });
    let mult = factor.saturating_pow(repeat_n as u32);
    let dur = spec.duration.unwrap_or(Duration::EndOfTurn);

    match spec.target {
        DoubleTarget::Power | DoubleTarget::Toughness | DoubleTarget::Pt => {
            let targets = resolve_selector_objects(state, &step.subject);
            for tid in targets {
                let Some((p, t)) = state
                    .evaluate_board()
                    .get(tid)
                    .and_then(|e| e.pt)
                    .map(|pt| (pt.power, pt.toughness))
                else {
                    continue;
                };
                let dp = if matches!(spec.target, DoubleTarget::Power | DoubleTarget::Pt) {
                    p.saturating_mul(mult - 1)
                } else {
                    0
                };
                let dt = if matches!(spec.target, DoubleTarget::Toughness | DoubleTarget::Pt) {
                    t.saturating_mul(mult - 1)
                } else {
                    0
                };
                register_timed_effect(
                    state,
                    ctrl,
                    src,
                    Layer::Layer7c,
                    ContinuousTarget::Objects(smallvec::smallvec![tid]),
                    LayerMod::Pt {
                        power: Box::new(Amount::Const { n: dp }),
                        toughness: Box::new(Amount::Const { n: dt }),
                    },
                    dur,
                );
            }
        }
        DoubleTarget::Counters => {
            let targets = resolve_selector_objects(state, &step.subject);
            for tid in targets {
                let counters = state.counters(tid).to_vec();
                for (cid, cur) in counters {
                    let matches_ct = match spec.counter {
                        Some(CounterTarget::Specific(req)) => req == cid,
                        _ => true,
                    };
                    if matches_ct && cur > 0 {
                        let add = cur.saturating_mul((mult - 1).max(0) as u32);
                        if add > 0
                            && let ReplacementResult::Resolved(out) = replace::resolve_put_counters(
                                state,
                                tid,
                                cid,
                                add,
                                Some(ctrl),
                                src,
                            )?
                        {
                            apply_resolved_put_counters(state, &out, ctrl);
                        }
                    }
                }
            }
        }
        DoubleTarget::Life => {
            let players = resolve_target_players(state, step);
            for pid in players {
                let cur = state.player(pid).map_or(0, |p| p.life);
                let delta = cur.saturating_mul(mult - 1);
                if delta > 0 {
                    if let ReplacementResult::Resolved(out) =
                        replace::resolve_life_gain(state, pid, delta as u32, src)?
                    {
                        super::damage_life::apply_resolved_life_gain(state, pid, &out);
                    }
                } else if delta < 0 {
                    let loss = (-delta) as u32;
                    if let Some(p) = state.player_mut(pid) {
                        p.life = p.life.saturating_sub(loss as i32);
                    }
                    state.mark_dirty(DirtyMask::LIFE);
                    emit_event(
                        state,
                        AtomicEventKind::LifeChange {
                            player: pid,
                            delta: -(loss as i32),
                        },
                    );
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_choose_designation(
    state: &mut GameState,
    step: &EffectStep,
    kind: DesignationKind,
) -> Result<StepExecOutcome, EngineError> {
    let chooser = resolve_step_actor(state, step);
    let options: Vec<String> = match kind {
        DesignationKind::Color => vec![
            "white".into(),
            "blue".into(),
            "black".into(),
            "red".into(),
            "green".into(),
        ],
        DesignationKind::CreatureType => {
            let mut v = Vec::new();
            for st in &state.db().tables.subtypes {
                if !crate::faces::is_non_creature_subtype(st) {
                    v.push(st.clone());
                }
                if v.len() >= 16 {
                    break;
                }
            }
            if v.is_empty() {
                v.push("Human".into());
            }
            v
        }
        DesignationKind::CardName | DesignationKind::NonlandCardName => {
            let mut v: Vec<String> = state
                .db()
                .cards
                .iter()
                .take(16)
                .map(|c| c.name.clone())
                .collect();
            if v.is_empty() {
                v.push("Synth Card".into());
            }
            v
        }
    };
    let prompt_id = state.allocate_prompt_id();
    let opt_cnt = options.len() as u16;
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::ChooseMode {
            chooser,
            purpose: ModePausePurpose::DesignationChoice,
            options,
            picked: SmallVec::new(),
            allow_duplicates: false,
            min: 1,
            max: 1,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseMode {
        prompt_id,
        option_count: opt_cnt,
        min: 1,
        max: 1,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn apply_designation_choice(state: &mut GameState, kind: DesignationKind, chosen: &str) {
    let Some(src) = vm_source(state) else {
        return;
    };
    match kind {
        DesignationKind::Color => {
            let mask = match chosen.to_ascii_lowercase().as_str() {
                "white" => ColorMask::W,
                "blue" => ColorMask::U,
                "black" => ColorMask::B,
                "red" => ColorMask::R,
                "green" => ColorMask::G,
                _ => ColorMask::empty(),
            };
            if let Some(r) = state.rare_or_default(src) {
                r.chosen_color = Some(mask);
            }
        }
        DesignationKind::CreatureType => {
            if let Some(sid) = state.db().subtype_id(chosen)
                && let Some(r) = state.rare_or_default(src)
            {
                r.chosen_subtype = Some(sid);
            }
        }
        DesignationKind::CardName | DesignationKind::NonlandCardName => {
            if let Some(r) = state.rare_or_default(src) {
                r.chosen_card_name = Some(chosen.to_string());
            }
        }
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
}

pub(crate) fn exec_suspect(
    state: &mut GameState,
    step: &EffectStep,
    unsuspect: bool,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    for id in targets {
        if let Some(o) = state.object_mut(id)
            && o.zone == Zone::Battlefield
        {
            if unsuspect {
                o.flags.remove(ObjectFlags::SUSPECTED);
            } else {
                o.flags.insert(ObjectFlags::SUSPECTED);
            }
        }
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn finish_monstrosity_target(
    state: &mut GameState,
    id: ObjectId,
    ctrl: PlayerId,
    amt: u32,
) {
    if let Some(o) = state.object_mut(id) {
        o.flags.insert(ObjectFlags::MONSTROUS);
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    emit_event(
        state,
        AtomicEventKind::BecameMonstrous {
            player: ctrl,
            object: id,
            amount: amt as u16,
        },
    );
}

pub(crate) fn run_monstrosity_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: &[ObjectId],
    start_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;
    for (offset, &id) in targets[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        let already = state
            .object(id)
            .is_some_and(|o| o.flags.contains(ObjectFlags::MONSTROUS));
        if already {
            continue;
        }
        if let Some(cid) = state.db().counter_id("+1/+1")
            && amt > 0
        {
            match replace::resolve_put_counters(state, id, cid, amt, Some(ctrl), src)? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    set_waiting_on_replacement(
                        state,
                        prompt_id,
                        targets.to_vec(),
                        idx,
                        OP_TAG_MONSTROUS,
                    );
                    return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                }
                ReplacementResult::Resolved(out) => {
                    apply_resolved_put_counters(state, &out, ctrl);
                }
            }
        }
        finish_monstrosity_target(state, id, ctrl, amt);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_monstrosity(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    run_monstrosity_targets(state, step, &targets, 0)
}

pub(crate) fn exec_bolster(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;
    let least_id: Option<ObjectId> = {
        let cache = state.evaluate_board();
        state.player(ctrl).and_then(|p| {
            p.battlefield
                .iter()
                .copied()
                .filter_map(|id| {
                    let ev = cache.get(id)?;
                    if ev.types.contains(CardTypeMask::CREATURE) {
                        Some((id, ev.pt.map_or(0, |pt| pt.toughness)))
                    } else {
                        None
                    }
                })
                .min_by_key(|&(_, t)| t)
                .map(|(id, _)| id)
        })
    };
    if let Some(tid) = least_id
        && let Some(cid) = state.db().counter_id("+1/+1")
        && amt > 0
    {
        match replace::resolve_put_counters(state, tid, cid, amt, Some(ctrl), src)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                set_waiting_on_replacement(state, prompt_id, vec![tid], 0, OP_TAG_PUT_COUNTERS);
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_put_counters(state, &out, ctrl);
                if !out.mods.skipped {
                    append_step_binding(state, step.bind_as.as_deref(), tid, None);
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_adapt_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: &[ObjectId],
    start_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let amt = resolve_step_amount(state, step, 1).max(0) as u32;
    let Some(plus_cid) = state.db().counter_id("+1/+1") else {
        return Ok(StepExecOutcome::Done);
    };
    for (offset, &id) in targets[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        if state.counter_count(id, plus_cid) == 0 && amt > 0 {
            match replace::resolve_put_counters(state, id, plus_cid, amt, Some(ctrl), src)? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    set_waiting_on_replacement(
                        state,
                        prompt_id,
                        targets.to_vec(),
                        idx,
                        OP_TAG_PUT_COUNTERS,
                    );
                    return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                }
                ReplacementResult::Resolved(out) => {
                    apply_resolved_put_counters(state, &out, ctrl);
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_adapt(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_objects(state, &step.subject);
    run_adapt_targets(state, step, &targets, 0)
}

pub(crate) fn exec_exert(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    for id in resolve_selector_objects(state, &step.subject) {
        if let Some(o) = state.object_mut(id) {
            o.flags.insert(ObjectFlags::EXERTED);
        }
        if let Some(r) = state.rare_or_default(id) {
            r.doesnt_untap_next_untap = true;
        }
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        emit_event(
            state,
            AtomicEventKind::Exerted {
                player: ctrl,
                object: id,
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_saddle(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    for id in resolve_selector_objects(state, &step.subject) {
        if let Some(o) = state.object_mut(id) {
            o.flags.insert(ObjectFlags::SADDLED);
        }
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        emit_event(
            state,
            AtomicEventKind::BecomesSaddled {
                player: ctrl,
                mount: id,
                saddled_by: Vec::new(),
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

#[allow(dead_code)]
fn _keep_used(_: ManaColor) {}
