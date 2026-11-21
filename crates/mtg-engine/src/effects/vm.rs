use std::sync::Arc;

use mtg_ir::{
    CardTypeMask, CostDef, EffectStep, OptionalMode, ReflexiveEventFilter, SelectionMod, Selector,
    StepGate, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::costs::{self, CostStepOutcome};
use crate::eval::{
    BoardView, EvalContext, evaluate_amount, evaluate_condition, resolve_player_ref,
};
use crate::events::{AtomicEventKind, emit_event};
use crate::filter::{find_objects, matches_object_filter, matches_player_filter};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::mana;
use crate::replace::ReplacementOutcome;
use crate::state::{
    ControlFlowFrame, EngineError, GameState, LayerCache, LkiSnapshot, NONE_OBJECT_ID,
    PendingPromptContext, PromptOwner, RareTurnState, ResolutionBinding, StepAcceptance,
    StepPauseState, YesNoPausePurpose,
};
use crate::triggers::{self, PendingReflexiveTrigger};
use crate::zones::{self, MoveObjectOutcome, MoveRawOptions, ZonePosition};

pub(crate) const OP_TAG_DESTROY: u8 = 1;
pub(crate) const OP_TAG_EXILE: u8 = 2;
pub(crate) const OP_TAG_BOUNCE: u8 = 3;
pub(crate) const OP_TAG_MOVE: u8 = 4;
pub(crate) const OP_TAG_SACRIFICE: u8 = 5;
pub(crate) const OP_TAG_DISCARD: u8 = 6;
pub(crate) const OP_TAG_MILL: u8 = 7;
pub(crate) const OP_TAG_DRAW: u8 = 8;
pub(crate) const OP_TAG_DAMAGE: u8 = 9;
pub(crate) const OP_TAG_GAIN_LIFE: u8 = 10;
pub(crate) const OP_TAG_PUT_COUNTERS: u8 = 11;
pub(crate) const OP_TAG_PUT_PLAYER_COUNTERS: u8 = 12;
pub(crate) const OP_TAG_CREATE_TOKEN: u8 = 13;
pub(crate) const OP_TAG_UNTAP: u8 = 14;
pub(crate) const OP_TAG_PROLIFERATE: u8 = 15;
pub(crate) const OP_TAG_SEARCH: u8 = 16;
pub(crate) const OP_TAG_PROLIFERATE_COUNTER: u8 = 17;
pub(crate) const OP_TAG_MONSTROUS: u8 = 18;
pub(crate) const OP_TAG_TAP_OR_UNTAP: u8 = 19;
pub(crate) const OP_TAG_PROLIFERATE_PLAYER_COUNTER: u8 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepExecOutcome {
    Done,
    Declined,
    Paused { prompt_id: u32, chooser: PlayerId },
    EnterSubframe,
    GameOver { winner: Option<PlayerId> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectionResult {
    Ready(Vec<ObjectId>),
    Paused { prompt_id: u32, chooser: PlayerId },
}

#[inline]
pub(crate) fn allocate_continuous_effect_id(state: &mut GameState) -> u64 {
    state.allocate_continuous_effect_id()
}

#[inline]
pub(crate) fn rare_turn_state_mut(state: &mut GameState) -> &mut RareTurnState {
    state.rare_turn_state_mut()
}

/// Evaluates `f` with a freshly populated [`EvalContext`] backed by `BoardView::Cached(state.evaluate_board())`.
pub(crate) fn with_vm_eval<R>(
    state: &GameState,
    f: impl FnOnce(&EvalContext<'_>, &LayerCache) -> R,
) -> R {
    let cache = state.evaluate_board();
    let frame = state.resolution_frames().last();
    let vm = state.resolution_vms().last();
    let (ctrl, src, stack_item_id, x_val) = match vm {
        Some(v) => (v.controller, v.source, Some(v.stack_item), Some(v.x_value)),
        None => (state.priority_player(), None, None, None),
    };
    let stack_entry =
        stack_item_id.and_then(|sid| state.stack_entries().iter().find(|se| se.id == sid));
    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::Cached(cache));
    ctx.source = src;
    ctx.stack_item = stack_entry;
    ctx.x_value = x_val;
    if let Some(fr) = frame {
        ctx.bindings = &fr.bindings;
    }
    let mut spec_slices: SmallVec<[&[TargetRef]; 4]> = SmallVec::new();
    if let Some(v) = vm {
        ctx.targets = &v.targets;
        for s in &v.targets_by_spec {
            spec_slices.push(s.as_slice());
        }
        ctx.step_acted_ids = &v.step_acted_ids;
        ctx.already_selected_controllers = &v.already_selected_controllers;
        ctx.each_loop_player = v.each_loop_player;
        ctx.chosen_opponent = v.chosen_opponent;
        ctx.event_object = v.event_object;
        ctx.event_lki = v.event_lki.as_deref();
        ctx.event_player = v.event_player;
        ctx.event_amount = v.event_amount;
        ctx.defending_player = v.defending_player;
        ctx.last_effect_amount = v.last_effect_amount;
        ctx.last_excess_damage = v.last_excess_damage;
        ctx.last_counters_removed = v.last_counters_removed;
        ctx.last_roll_result = v.last_roll_result;
        ctx.coin_heads_count = v.coin_heads_count;
        ctx.counters_removed_this_way = v.counters_removed_this_way;
        ctx.step_way_stats = Some(&v.step_way_stats);
        if let Some(ControlFlowFrame::SubSteps {
            event_object,
            event_player,
            event_amount,
            ..
        }) = v.subframes.last()
        {
            if event_object.is_some() {
                ctx.event_object = *event_object;
            }
            if event_player.is_some() {
                ctx.event_player = *event_player;
            }
            if event_amount.is_some() {
                ctx.event_amount = *event_amount;
            }
        }
    }
    if !spec_slices.is_empty() {
        // Safety/lifetime: `spec_slices` lives in this stack frame while `f` executes.
        ctx.targets_by_spec = &spec_slices;
    }
    f(&ctx, cache)
}

#[inline]
pub(crate) fn vm_controller(state: &GameState) -> PlayerId {
    state
        .resolution_vms()
        .last()
        .map_or_else(|| state.priority_player(), |v| v.controller)
}

#[inline]
pub(crate) fn vm_source(state: &GameState) -> Option<ObjectId> {
    state.resolution_vms().last().and_then(|v| v.source)
}

#[inline]
pub(crate) fn vm_stack_item(state: &GameState) -> Option<StackItemId> {
    state.resolution_vms().last().map(|v| v.stack_item)
}

#[inline]
pub(crate) fn active_event_player(state: &GameState) -> Option<PlayerId> {
    let vm = state.resolution_vms().last()?;
    if let Some(ControlFlowFrame::SubSteps { event_player, .. }) = vm.subframes.last()
        && event_player.is_some()
    {
        return *event_player;
    }
    vm.event_player
}

#[inline]
pub(crate) fn active_event_object(state: &GameState) -> Option<ObjectId> {
    let vm = state.resolution_vms().last()?;
    if let Some(ControlFlowFrame::SubSteps { event_object, .. }) = vm.subframes.last()
        && event_object.is_some()
    {
        return *event_object;
    }
    vm.event_object
}

pub(crate) fn current_step_cloned(state: &GameState) -> Option<EffectStep> {
    let vm = state.resolution_vms().last()?;
    if let Some(sub) = vm.subframes.last() {
        return match sub {
            ControlFlowFrame::ForEachPlayer { steps, step_pc, .. }
            | ControlFlowFrame::RepeatLoop { steps, step_pc, .. }
            | ControlFlowFrame::SubSteps { steps, step_pc, .. } => steps.get(*step_pc).cloned(),
        };
    }
    let fr = state.resolution_frames().last()?;
    fr.steps_remaining.get(fr.pc).cloned()
}

pub(crate) fn advance_active_pc(state: &mut GameState) {
    if let Some(vm) = state.resolution_vms_mut().last_mut()
        && let Some(sub) = vm.subframes.last_mut()
    {
        match sub {
            ControlFlowFrame::ForEachPlayer { step_pc, .. }
            | ControlFlowFrame::RepeatLoop { step_pc, .. }
            | ControlFlowFrame::SubSteps { step_pc, .. } => {
                *step_pc += 1;
            }
        }
        return;
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pc += 1;
    }
}

pub(crate) fn record_step_acceptance(state: &mut GameState, acc: StepAcceptance) {
    if let Some(vm) = state.resolution_vms_mut().last_mut()
        && vm.subframes.is_empty()
    {
        vm.step_results.push(acc);
    }
}

pub(crate) fn evaluate_step_gate(state: &GameState, gate: &StepGate) -> bool {
    let vm = state.resolution_vms().last();
    let check_acc = |idx: u16, expected: StepAcceptance| -> bool {
        vm.and_then(|v| v.step_results.get(usize::from(idx)))
            .is_some_and(|&r| r == expected)
    };
    match gate {
        StepGate::IfPrecedingAccepted { step_index } => {
            check_acc(*step_index, StepAcceptance::Accepted)
        }
        StepGate::IfPrecedingDeclined { step_index } => {
            check_acc(*step_index, StepAcceptance::Declined)
        }
        StepGate::Condition { condition } => {
            with_vm_eval(state, |ctx, _| evaluate_condition(state, condition, ctx))
        }
        StepGate::Combined {
            if_preceding_accepted,
            if_preceding_declined,
            condition,
        } => {
            if let Some(idx) = if_preceding_accepted
                && !check_acc(*idx, StepAcceptance::Accepted)
            {
                return false;
            }
            if let Some(idx) = if_preceding_declined
                && !check_acc(*idx, StepAcceptance::Declined)
            {
                return false;
            }
            if let Some(cond) = condition
                && !with_vm_eval(state, |ctx, _| evaluate_condition(state, cond, ctx))
            {
                return false;
            }
            true
        }
    }
}

pub(crate) fn resolve_step_actor(state: &GameState, step: &EffectStep) -> PlayerId {
    let fallback = vm_controller(state);
    let Some(pr) = step.actor.as_ref() else {
        return fallback;
    };
    with_vm_eval(state, |ctx, _| {
        resolve_player_ref(state, pr, ctx).unwrap_or(fallback)
    })
}

pub(crate) fn resolve_step_amount(state: &GameState, step: &EffectStep, default: i32) -> i32 {
    let Some(amt) = step.amount.as_deref() else {
        return default;
    };
    with_vm_eval(state, |ctx, _| evaluate_amount(state, amt, ctx))
}

pub(crate) fn resolve_selector_objects(state: &GameState, subject: &Selector) -> Vec<ObjectId> {
    let Some(vm) = state.resolution_vms().last() else {
        return Vec::new();
    };
    let Some(frame) = state.resolution_frames().last() else {
        return Vec::new();
    };
    match subject {
        Selector::SelfObj => vm.source.into_iter().collect(),
        Selector::Binding { name } => frame
            .bindings
            .iter()
            .rev()
            .find(|b| b.name == *name)
            .map(|b| b.objects.clone())
            .unwrap_or_default(),
        Selector::Target { index } => {
            let idx = usize::from(*index);
            if !vm.targets_by_spec.is_empty() && idx < vm.targets_by_spec.len() {
                vm.targets_by_spec[idx]
                    .iter()
                    .filter_map(|t| match t {
                        TargetRef::Object { id } => Some(*id),
                        _ => None,
                    })
                    .collect()
            } else {
                vm.targets
                    .get(idx)
                    .and_then(|t| match t {
                        TargetRef::Object { id } => Some(*id),
                        _ => None,
                    })
                    .into_iter()
                    .collect()
            }
        }
        Selector::All { filter } => with_vm_eval(state, |ctx, _| find_objects(state, filter, ctx)),
        Selector::EventObject => active_event_object(state).into_iter().collect(),
        Selector::AttachedTo => vm
            .source
            .and_then(|s| state.object(s)?.attached_to)
            .into_iter()
            .collect(),
        Selector::LibraryTop { player } => {
            let pid = match player {
                Some(pr) => with_vm_eval(state, |ctx, _| {
                    resolve_player_ref(state, pr, ctx).unwrap_or(vm.controller)
                }),
                None => vm.controller,
            };
            state
                .player(pid)
                .and_then(|p| p.library.last().copied())
                .into_iter()
                .collect()
        }
        Selector::Player { .. } | Selector::PlayerRef { .. } | Selector::None => Vec::new(),
    }
}

pub(crate) fn resolve_selector_targets(state: &GameState, subject: &Selector) -> Vec<TargetRef> {
    let Some(vm) = state.resolution_vms().last() else {
        return Vec::new();
    };
    match subject {
        Selector::Target { index } => {
            let idx = usize::from(*index);
            if !vm.targets_by_spec.is_empty() && idx < vm.targets_by_spec.len() {
                vm.targets_by_spec[idx].clone()
            } else {
                vm.targets.get(idx).copied().into_iter().collect()
            }
        }
        Selector::Player { filter } => with_vm_eval(state, |ctx, _| {
            state
                .players()
                .iter()
                .filter(|p| !p.lost && matches_player_filter(state, p.id, filter, ctx))
                .map(|p| TargetRef::Player { id: p.id })
                .collect()
        }),
        Selector::PlayerRef { player } => with_vm_eval(state, |ctx, _| {
            resolve_player_ref(state, player, ctx)
                .map(|id| TargetRef::Player { id })
                .into_iter()
                .collect()
        }),
        _ => resolve_selector_objects(state, subject)
            .into_iter()
            .map(|id| TargetRef::Object { id })
            .collect(),
    }
}

pub(crate) fn resolve_target_players(state: &GameState, step: &EffectStep) -> Vec<PlayerId> {
    let ctrl = vm_controller(state);
    match &step.subject {
        Selector::Player { filter } => with_vm_eval(state, |ctx, _| {
            state
                .players()
                .iter()
                .filter(|p| !p.lost && matches_player_filter(state, p.id, filter, ctx))
                .map(|p| p.id)
                .collect()
        }),
        Selector::PlayerRef { player } => with_vm_eval(state, |ctx, _| {
            resolve_player_ref(state, player, ctx).into_iter().collect()
        }),
        Selector::Target { .. } => {
            let trefs = resolve_selector_targets(state, &step.subject);
            let mut out: Vec<PlayerId> = trefs
                .into_iter()
                .filter_map(|t| match t {
                    TargetRef::Player { id } => Some(id),
                    TargetRef::Object { id } => state.object(id).map(|o| o.controller),
                    TargetRef::Stack { id } => state
                        .stack_entries()
                        .iter()
                        .find(|se| se.id == id)
                        .map(|se| se.controller),
                })
                .collect();
            if out.is_empty()
                && let Some(pr) = step.actor.as_ref()
            {
                out = with_vm_eval(state, |ctx, _| {
                    resolve_player_ref(state, pr, ctx).into_iter().collect()
                });
            }
            out
        }
        Selector::None => vec![resolve_step_actor(state, step)],
        _ => vec![
            step.actor
                .as_ref()
                .map_or(ctrl, |_| resolve_step_actor(state, step)),
        ],
    }
}

pub(crate) fn resolve_selection_mod(
    state: &mut GameState,
    subject_candidates: Vec<ObjectId>,
    select: &SelectionMod,
    default_chooser: PlayerId,
    default_zone: ZoneMask,
) -> Result<SelectionResult, EngineError> {
    let (candidates, chooser, choice_count) = with_vm_eval(state, |ctx, _| {
        let mut cands = if subject_candidates.is_empty() {
            if let Some(f) = select.filter.as_deref() {
                let mut f_owned = f.clone();
                if f_owned.zones.is_empty() {
                    f_owned.zones = default_zone;
                }
                find_objects(state, &f_owned, ctx)
            } else {
                Vec::new()
            }
        } else if let Some(f) = select.filter.as_deref() {
            subject_candidates
                .into_iter()
                .filter(|&id| matches_object_filter(state, id, f, ctx))
                .collect()
        } else {
            subject_candidates
        };

        // Deduplicate preserving order.
        let mut dedup = Vec::with_capacity(cands.len());
        for id in cands.drain(..) {
            if !dedup.contains(&id) {
                dedup.push(id);
            }
        }

        let chooser = match select.chooser {
            Some(mtg_ir::ChooserOverride::You) => ctx.controller,
            Some(mtg_ir::ChooserOverride::EventPlayer) => {
                ctx.event_player.unwrap_or(default_chooser)
            }
            Some(mtg_ir::ChooserOverride::Opponent) => ctx
                .chosen_opponent
                .or_else(|| {
                    state
                        .players()
                        .iter()
                        .find(|p| !p.lost && p.id != ctx.controller)
                        .map(|p| p.id)
                })
                .unwrap_or(default_chooser),
            None => default_chooser,
        };

        let choice_count = select
            .choice_count
            .as_deref()
            .map(|a| evaluate_amount(state, a, ctx).max(0) as u16);
        (dedup, chooser, choice_count)
    });

    if select.is_empty() || (choice_count.is_none() && !select.up_to && !select.at_random) {
        return Ok(SelectionResult::Ready(candidates));
    }

    let max = choice_count
        .unwrap_or(if select.up_to {
            candidates.len() as u16
        } else {
            1
        })
        .min(candidates.len() as u16);

    if max == 0 {
        return Ok(SelectionResult::Ready(Vec::new()));
    }

    if select.at_random {
        let mut pool = candidates;
        let mut picked = Vec::with_capacity(usize::from(max));
        for _ in 0..max {
            if pool.is_empty() {
                break;
            }
            let idx = state.rng_mut().next_index(pool.len());
            picked.push(pool.swap_remove(idx));
        }
        return Ok(SelectionResult::Ready(picked));
    }

    let min = if select.up_to { 0 } else { max };
    if min == 0 && candidates.is_empty() {
        return Ok(SelectionResult::Ready(Vec::new()));
    }
    if !select.up_to && candidates.len() <= usize::from(min) {
        return Ok(SelectionResult::Ready(candidates));
    }

    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::SelectionPick {
            chooser,
            candidates: candidates.clone(),
            selected: Vec::new(),
            min,
            max,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseObjects {
        prompt_id,
        candidates,
        min,
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

    Ok(SelectionResult::Paused { prompt_id, chooser })
}

/// Captures an `LkiSnapshot` for `id` (from `state.lki(id)`, or synthesized from current state).
pub(crate) fn snapshot_for_binding(state: &GameState, id: ObjectId) -> Option<Arc<LkiSnapshot>> {
    if let Some(snap) = state.lki(id) {
        return Some(Arc::new(snap.clone()));
    }
    let obj = state.object(id)?;
    let cache = state.evaluate_board();
    let eval = cache.get(id).cloned();
    let rare = state.rare(id);
    let mut name = eval
        .as_ref()
        .and_then(|e| e.name_override.clone())
        .unwrap_or_default();
    if name.is_empty() {
        if let Some(ts) = rare.and_then(|r| r.token_spec.as_deref()) {
            name = ts.name.clone();
        } else if let Some(card) = state.db().get(obj.card_def) {
            name = card
                .faces
                .get(usize::from(obj.active_face_index()))
                .map_or_else(|| card.name.clone(), |f| f.name.clone());
        }
    }
    let (pt, types, subtypes, supertypes, colors, mana_value, keywords, lost_all, all_ct) =
        if let Some(e) = eval {
            (
                e.pt,
                e.types,
                e.subtypes,
                e.supertypes,
                e.colors,
                e.mana_value,
                e.keywords,
                e.lost_all_abilities,
                e.has_all_creature_types,
            )
        } else {
            let ctx = crate::eval::EvalContext::new(obj.controller)
                .with_board(crate::eval::BoardView::Cached(cache));
            let off = crate::eval::off_battlefield_characteristics(state, id, obj, obj.zone, &ctx);
            if name.is_empty() {
                name = off.name;
            }
            (
                off.pt,
                off.types,
                off.subtypes,
                off.supertypes,
                off.colors,
                off.mana_value,
                off.keywords,
                false,
                off.has_all_creature_types,
            )
        };
    let mut attached_ids = SmallVec::new();
    if obj.zone == Zone::Battlefield {
        for p in state.players() {
            for &cand in &p.battlefield {
                if state
                    .object(cand)
                    .is_some_and(|o| o.attached_to == Some(id))
                {
                    attached_ids.push(cand);
                }
            }
        }
    }
    Some(Arc::new(LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name,
        owner: obj.owner,
        controller: obj.controller,
        last_zone: obj.zone,
        pt,
        types,
        subtypes,
        supertypes,
        colors,
        mana_value,
        keywords,
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.damage_marked,
        is_token: obj.is_token(),
        is_commander: obj.is_commander(),
        tapped: obj.flags.contains(crate::state::ObjectFlags::TAPPED),
        phased_out: obj.is_phased_out(),
        face_down: obj.is_face_down(),
        lost_all_abilities: lost_all,
        has_all_creature_types: all_ct,
        attached_ids,
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.active_face_index(),
    }))
}

pub(crate) fn record_step_bindings(
    state: &mut GameState,
    bind_name: Option<&str>,
    acted: &[(ObjectId, Option<Arc<LkiSnapshot>>)],
) {
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.step_acted_ids.clear();
        for (id, _) in acted {
            vm.step_acted_ids.push(*id);
        }
    }
    let Some(name) = bind_name else {
        return;
    };
    let mut objects = Vec::with_capacity(acted.len());
    let mut lki = Vec::with_capacity(acted.len());
    for (id, maybe_snap) in acted {
        objects.push(*id);
        if let Some(s) = maybe_snap {
            lki.push(Arc::clone(s));
        } else if let Some(s) = snapshot_for_binding(state, *id) {
            lki.push(s);
        }
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        if let Some(existing) = fr.bindings.iter_mut().find(|b| b.name == name) {
            existing.objects = objects;
            existing.lki = lki;
        } else {
            fr.bindings.push(ResolutionBinding {
                name: name.to_string(),
                objects,
                lki,
            });
        }
    }
}

pub(crate) fn append_step_binding(
    state: &mut GameState,
    bind_name: Option<&str>,
    id: ObjectId,
    maybe_snap: Option<Arc<LkiSnapshot>>,
) {
    if let Some(vm) = state.resolution_vms_mut().last_mut()
        && !vm.step_acted_ids.contains(&id)
    {
        vm.step_acted_ids.push(id);
    }
    let Some(name) = bind_name else {
        return;
    };
    let snap = maybe_snap.or_else(|| snapshot_for_binding(state, id));
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        if let Some(existing) = fr.bindings.iter_mut().find(|b| b.name == name) {
            existing.objects.push(id);
            if let Some(s) = snap {
                existing.lki.push(s);
            }
        } else {
            let mut lki = Vec::new();
            if let Some(s) = snap {
                lki.push(s);
            }
            fr.bindings.push(ResolutionBinding {
                name: name.to_string(),
                objects: vec![id],
                lki,
            });
        }
    }
}

pub(crate) fn handle_zone_move_outcome(
    state: &mut GameState,
    mv: MoveObjectOutcome,
    bind_as: Option<&str>,
    batch_remaining: Vec<ObjectId>,
    batch_idx: usize,
    op_tag: u8,
) -> Result<StepExecOutcome, EngineError> {
    match mv {
        MoveObjectOutcome::Moved(raw) => {
            let final_id = raw.new_id.unwrap_or(raw.old_id);
            append_step_binding(state, bind_as, final_id, raw.lki.clone());
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                if raw.from_zone == Zone::Battlefield && raw.to_zone == Zone::Graveyard {
                    vm.step_way_stats.died_this_way += 1;
                }
                if raw.to_zone == Zone::Exile {
                    vm.step_way_stats.exiled_count_this_way += 1;
                    if Some(raw.old_id) == vm.source {
                        vm.step_way_stats.source_exiled_this_way = true;
                    }
                }
                if raw.to_zone == Zone::Hand {
                    vm.step_way_stats.returned_filter_to_hand_this_way = true;
                }
            }
            Ok(StepExecOutcome::Done)
        }
        MoveObjectOutcome::Skipped { object, zone } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut()
                && zone != Zone::Battlefield
            {
                vm.step_way_stats.didnt_put_onto_bf_this_way = true;
                vm.step_way_stats.not_put_onto_bf_by_this_ability = true;
            }
            let _ = object;
            Ok(StepExecOutcome::Done)
        }
        MoveObjectOutcome::ExecuteInstead { object, steps, .. } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.subframes.push(ControlFlowFrame::SubSteps {
                    steps,
                    step_pc: 0,
                    event_object: Some(object),
                    event_player: None,
                    event_amount: None,
                });
            }
            Ok(StepExecOutcome::EnterSubframe)
        }
        MoveObjectOutcome::Paused { prompt_id, chooser } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pause_state = Some(StepPauseState::WaitingOnReplacement {
                    batch_remaining,
                    batch_idx,
                    op_tag,
                });
            }
            if let Some(fr) = state.resolution_frames_mut().last_mut() {
                fr.pending_prompt_id = Some(prompt_id);
            }
            state.set_pending_replacement_resume(Some(
                crate::state::ReplacementResumeHook::ResolutionFrame,
            ));
            Ok(StepExecOutcome::Paused { prompt_id, chooser })
        }
    }
}

pub(crate) fn set_waiting_on_replacement(
    state: &mut GameState,
    prompt_id: u32,
    batch_remaining: Vec<ObjectId>,
    batch_idx: usize,
    op_tag: u8,
) {
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::WaitingOnReplacement {
            batch_remaining,
            batch_idx,
            op_tag,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    state
        .set_pending_replacement_resume(Some(crate::state::ReplacementResumeHook::ResolutionFrame));
}

pub(crate) fn maybe_queue_reflexive_trigger(state: &mut GameState, step: &EffectStep) {
    let Some(spec) = step.reflexive_trigger.as_deref() else {
        return;
    };
    let Some(vm) = state.resolution_vms().last().cloned() else {
        return;
    };
    let Some(frame) = state.resolution_frames().last() else {
        return;
    };
    let ok = match spec.event_filter {
        None => true,
        Some(ReflexiveEventFilter::WhenYouDo) => vm.step_way_stats.did_last_optional_step,
        Some(ReflexiveEventFilter::DiedThisWay) => vm.step_way_stats.died_this_way > 0,
        Some(ReflexiveEventFilter::SacrificedThisWay) => !vm.step_acted_ids.is_empty(),
        Some(ReflexiveEventFilter::DiscardedThisWay) => {
            vm.step_way_stats.discarded_nonland_this_way
                || vm.step_way_stats.discarded_creature_this_way
                || vm.step_way_stats.discarded_noncreature_this_way
                || !vm.step_acted_ids.is_empty()
        }
        Some(ReflexiveEventFilter::ExiledThisWay) => vm.step_way_stats.exiled_count_this_way > 0,
        Some(ReflexiveEventFilter::RemovedCounterThisWay) => vm.counters_removed_this_way > 0,
    };
    if !ok {
        return;
    }
    let source = vm.source.unwrap_or(NONE_OBJECT_ID);
    let source_lki = vm
        .source_lki
        .clone()
        .or_else(|| snapshot_for_binding(state, source))
        .unwrap_or_else(|| Arc::new(empty_lki(source, vm.controller)));
    let ev_obj = vm.step_acted_ids.first().copied().or(vm.event_object);
    let ev_lki = ev_obj.and_then(|id| snapshot_for_binding(state, id));
    triggers::queue_reflexive_trigger(
        state,
        PendingReflexiveTrigger {
            id: 0,
            controller: vm.controller,
            source,
            source_lki,
            spec: spec.clone(),
            event_object: ev_obj,
            event_lki: ev_lki.or(vm.event_lki),
            event_player: vm.event_player,
            event_amount: vm.event_amount.or(Some(vm.last_effect_amount)),
            bindings: frame.bindings.clone(),
            x_value: vm.x_value,
        },
    );
}

pub(crate) fn empty_lki(object_id: ObjectId, controller: PlayerId) -> LkiSnapshot {
    LkiSnapshot {
        object_id,
        card_def: crate::state::TOKEN_CARD_DEF_ID,
        name: String::new(),
        owner: controller,
        controller,
        last_zone: Zone::Stack,
        pt: None,
        types: CardTypeMask::empty(),
        subtypes: SmallVec::new(),
        supertypes: mtg_ir::SupertypeMask::empty(),
        colors: mtg_ir::ColorMask::empty(),
        mana_value: 0,
        keywords: SmallVec::new(),
        counters: SmallVec::new(),
        damage_marked: 0,
        is_token: false,
        is_commander: false,
        tapped: false,
        phased_out: false,
        face_down: false,
        lost_all_abilities: false,
        has_all_creature_types: false,
        attached_ids: SmallVec::new(),
        chosen_color: None,
        chosen_subtype: None,
        chosen_card_name: None,
        active_face_index: 0,
    }
}

pub(crate) fn can_pay_cost_list(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost_defs: &[CostDef],
    x_value: u32,
) -> bool {
    let mut total_mana = mtg_ir::ManaCost::default();
    let mut non_mana = Vec::new();
    for c in cost_defs {
        match c {
            CostDef::Mana { cost, .. } => {
                total_mana.generic = total_mana
                    .generic
                    .saturating_add(cost.generic)
                    .saturating_add(x_value.saturating_mul(u32::from(cost.x_count)));
                total_mana.pips.extend(cost.pips.iter().copied());
            }
            CostDef::Choice { options, forage } => {
                if *forage {
                    non_mana.push(c.clone());
                } else if !options
                    .iter()
                    .any(|branch| can_pay_cost_list(state, payer, source, branch, x_value))
                {
                    return false;
                }
            }
            CostDef::Optional { .. } => {}
            other => non_mana.push(other.clone()),
        }
    }
    if !total_mana.is_empty() {
        let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
        if !mana::can_pay_mana_from_pool(state, payer, &total_mana, &spend_ctx) {
            return false;
        }
    }
    if !non_mana.is_empty() {
        return costs::can_pay_non_mana_costs(state, payer, source, &non_mana, Some(x_value));
    }
    true
}

pub(crate) fn pay_cost_list_sync(
    state: &mut GameState,
    payer: PlayerId,
    source: ObjectId,
    cost_defs: &[CostDef],
    x_value: u32,
) -> Result<(), EngineError> {
    for c in cost_defs {
        match c {
            CostDef::Mana { cost, .. } => {
                let mut eff = cost.clone();
                if eff.x_count > 0 {
                    eff.generic = eff
                        .generic
                        .saturating_add(x_value.saturating_mul(u32::from(eff.x_count)));
                    eff.x_count = 0;
                }
                let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
                mana::deduct_mana_for_context(state, payer, &eff, &spend_ctx, &[])?;
            }
            CostDef::Choice { options, forage } if !*forage => {
                let Some(branch) = options
                    .iter()
                    .find(|b| can_pay_cost_list(state, payer, source, b, x_value))
                    .cloned()
                else {
                    return Err(EngineError::IllegalAction(
                        "no payable option in Choice cost".into(),
                    ));
                };
                pay_cost_list_sync(state, payer, source, &branch, x_value)?;
            }
            CostDef::Optional { cost: inner } => {
                if can_pay_cost_list(state, payer, source, inner, x_value) {
                    pay_cost_list_sync(state, payer, source, inner, x_value)?;
                }
            }
            other => {
                let outcome = costs::execute_non_mana_cost_step(
                    state,
                    payer,
                    source,
                    other,
                    &[],
                    Some(x_value),
                    None,
                )?;
                if let CostStepOutcome::Paused { .. } = outcome {
                    // Best-effort inline: costs during optional resolution should not dangle.
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn push_optional_prompt(
    state: &mut GameState,
    chooser: PlayerId,
    is_unless: bool,
    label: String,
) -> (u32, PlayerId) {
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::OptionalChoice {
            chooser,
            purpose: YesNoPausePurpose::OptionalSpec { is_unless },
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::YesNo { prompt_id, label };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    (prompt_id, chooser)
}

pub(crate) fn finish_accepted_step(state: &mut GameState, step: &EffectStep) {
    if step.do_this_once_per_turn {
        let src = vm_source(state).unwrap_or(NONE_OBJECT_ID);
        let z = state.object(src).map_or(Zone::Stack, |o| o.zone);
        let fi = state.object(src).map_or(0, |o| o.active_face_index());
        let pc = state.resolution_frames().last().map_or(0, |f| f.pc);
        triggers::mark_do_this_once_taken(state, src, z, fi, pc);
    }
    record_step_acceptance(state, StepAcceptance::Accepted);
    maybe_queue_reflexive_trigger(state, step);
}

pub(crate) fn execute_single_step_with_op(
    state: &mut GameState,
    step: &EffectStep,
    skip_optional_prompt: bool,
    dispatch_op: fn(&mut GameState, &EffectStep) -> Result<StepExecOutcome, EngineError>,
) -> Result<StepExecOutcome, EngineError> {
    if !skip_optional_prompt && let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.step_acted_ids.clear();
        vm.pending_continuation = None;
    }

    // 1. Check StepGate
    if !skip_optional_prompt
        && let Some(gate) = step.gate.as_ref()
        && !evaluate_step_gate(state, gate)
    {
        record_step_acceptance(state, StepAcceptance::Declined);
        return Ok(StepExecOutcome::Declined);
    }

    // 2. Check do_this_once_per_turn
    if !skip_optional_prompt && step.do_this_once_per_turn {
        let src = vm_source(state).unwrap_or(NONE_OBJECT_ID);
        let key = crate::triggers::OncePerTurnKey {
            object: src,
            zone: state.object(src).map_or(Zone::Stack, |o| o.zone),
            face_index: state.object(src).map_or(0, |o| o.active_face_index()),
            ability_idx: state.resolution_frames().last().map_or(0, |f| f.pc),
            origin: crate::triggers::TriggerAbilityOrigin::Innate { component_index: 0 },
            is_do_this_once: true,
        };
        if state.do_this_once_taken_this_turn().contains(&key) {
            record_step_acceptance(state, StepAcceptance::Declined);
            return Ok(StepExecOutcome::Declined);
        }
    }

    // 3. Check OptionalSpec
    if !skip_optional_prompt && let Some(opt) = step.optional.as_ref() {
        let chooser = with_vm_eval(state, |ctx, _| {
            resolve_player_ref(state, &opt.chooser, ctx).unwrap_or(ctx.controller)
        });
        let src = vm_source(state).unwrap_or(NONE_OBJECT_ID);
        let x_val = state.resolution_vms().last().map_or(0, |v| v.x_value);
        let can_pay = can_pay_cost_list(state, chooser, src, &opt.cost, x_val);
        match opt.mode {
            OptionalMode::DoIfPaid => {
                if !can_pay {
                    if let Some(vm) = state.resolution_vms_mut().last_mut() {
                        vm.step_way_stats.did_last_optional_step = false;
                    }
                    record_step_acceptance(state, StepAcceptance::Declined);
                    return Ok(StepExecOutcome::Declined);
                }
                let (prompt_id, ch) = push_optional_prompt(
                    state,
                    chooser,
                    false,
                    "You may perform this action".into(),
                );
                return Ok(StepExecOutcome::Paused {
                    prompt_id,
                    chooser: ch,
                });
            }
            OptionalMode::DoUnlessPaid => {
                if can_pay {
                    let (prompt_id, ch) = push_optional_prompt(
                        state,
                        chooser,
                        true,
                        "Pay cost to prevent this effect?".into(),
                    );
                    return Ok(StepExecOutcome::Paused {
                        prompt_id,
                        chooser: ch,
                    });
                }
                // Can't pay -> effect proceeds automatically.
            }
        }
    }

    // 4. Execute op
    let outcome = dispatch_op(state, step)?;
    match &outcome {
        StepExecOutcome::Done => {
            finish_accepted_step(state, step);
        }
        StepExecOutcome::Declined => {
            record_step_acceptance(state, StepAcceptance::Declined);
        }
        StepExecOutcome::EnterSubframe
        | StepExecOutcome::Paused { .. }
        | StepExecOutcome::GameOver { .. } => {}
    }
    Ok(outcome)
}

/// Completes a paused `ChoiceRequest` once an answer is received.
pub(crate) fn handle_vm_pause_answer(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
    dispatch_op: fn(&mut GameState, &EffectStep) -> Result<StepExecOutcome, EngineError>,
) -> Result<Option<StepExecOutcome>, EngineError> {
    let pause = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pause_state.take())
        .ok_or_else(|| EngineError::IllegalAction("no active resolution pause state".into()))?;
    state.take_pending_prompt();
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = None;
    }

    match pause {
        StepPauseState::GuidedResolution { .. } => {
            let ChoiceAnswer::YesNo { .. } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for GuidedResolution".into(),
                ));
            };
            advance_active_pc(state);
            Ok(None)
        }
        StepPauseState::OptionalChoice { chooser, purpose } => {
            if actor != chooser {
                return Err(EngineError::IllegalAction(
                    "optional choice actor mismatch".into(),
                ));
            }
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo for OptionalChoice".into(),
                ));
            };
            match purpose {
                YesNoPausePurpose::OptionalSpec { is_unless } => {
                    let step = current_step_cloned(state)
                        .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
                    let accepted = if is_unless { !value } else { value };
                    let paid = value;
                    if paid
                        && let Some(opt) = step.optional.as_ref()
                        && !opt.cost.is_empty()
                    {
                        let src = vm_source(state).unwrap_or(NONE_OBJECT_ID);
                        let x_val = state.resolution_vms().last().map_or(0, |v| v.x_value);
                        pay_cost_list_sync(state, chooser, src, &opt.cost, x_val)?;
                    }
                    if let Some(vm) = state.resolution_vms_mut().last_mut() {
                        vm.step_way_stats.did_last_optional_step = accepted;
                    }
                    if !accepted {
                        record_step_acceptance(state, StepAcceptance::Declined);
                        advance_active_pc(state);
                        return Ok(None);
                    }
                    let outcome = dispatch_op(state, &step)?;
                    match outcome {
                        StepExecOutcome::Done => {
                            finish_accepted_step(state, &step);
                            advance_active_pc(state);
                            Ok(None)
                        }
                        StepExecOutcome::Declined => {
                            record_step_acceptance(state, StepAcceptance::Declined);
                            advance_active_pc(state);
                            Ok(None)
                        }
                        other => Ok(Some(other)),
                    }
                }
                YesNoPausePurpose::MayShuffleAfterMoveOrLook => {
                    if value {
                        zones::shuffle_library(state, chooser, 0)?;
                        emit_event(state, AtomicEventKind::LibraryShuffled { player: chooser });
                    }
                    if let Some(step) = current_step_cloned(state) {
                        finish_accepted_step(state, &step);
                    }
                    advance_active_pc(state);
                    Ok(None)
                }
                YesNoPausePurpose::EndTurnActiveMay => {
                    if value {
                        return match crate::turn::end_the_turn_immediately(state, chooser)? {
                            crate::turn::TurnAdvanceOutcome::Continued => Ok(None),
                            crate::turn::TurnAdvanceOutcome::Paused { prompt_id, chooser } => {
                                Ok(Some(StepExecOutcome::Paused { prompt_id, chooser }))
                            }
                            crate::turn::TurnAdvanceOutcome::GameOver { winner } => {
                                Ok(Some(StepExecOutcome::GameOver { winner }))
                            }
                        };
                    }
                    advance_active_pc(state);
                    Ok(None)
                }
                YesNoPausePurpose::ScryPutBottom | YesNoPausePurpose::SurveilPutGraveyard => {
                    Err(EngineError::IllegalAction(
                        "scry/surveil uses ReorderLibrary pause state".into(),
                    ))
                }
            }
        }
        StepPauseState::SelectionPick {
            chooser,
            candidates,
            mut selected,
            min,
            max,
        } => {
            if actor != chooser {
                return Err(EngineError::IllegalAction(
                    "selection actor mismatch".into(),
                ));
            }
            match answer {
                ChoiceAnswer::PickObject { id } => {
                    if !candidates.contains(&id) || selected.contains(&id) {
                        return Err(EngineError::IllegalAction(
                            "invalid selection candidate".into(),
                        ));
                    }
                    selected.push(id);
                    if selected.len() < usize::from(max) && selected.len() < candidates.len() {
                        let next_pid = state.allocate_prompt_id();
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pause_state = Some(StepPauseState::SelectionPick {
                                chooser,
                                candidates: candidates.clone(),
                                selected,
                                min,
                                max,
                            });
                        }
                        if let Some(fr) = state.resolution_frames_mut().last_mut() {
                            fr.pending_prompt_id = Some(next_pid);
                        }
                        let req = ChoiceRequest::ChooseObjects {
                            prompt_id: next_pid,
                            candidates,
                            min,
                            max,
                        };
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id: next_pid,
                            chooser,
                            request: req.clone(),
                            owner: PromptOwner::ResolutionFrame,
                        }));
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(Some(StepExecOutcome::Paused {
                            prompt_id: next_pid,
                            chooser,
                        }));
                    }
                }
                ChoiceAnswer::DonePicking => {
                    if selected.len() < usize::from(min) {
                        return Err(EngineError::IllegalAction(
                            "fewer than minimum objects picked".into(),
                        ));
                    }
                }
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected PickObject or DonePicking".into(),
                    ));
                }
            }
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome = super::resume_selection_op(state, &step, selected)?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                StepExecOutcome::Declined => {
                    record_step_acceptance(state, StepAcceptance::Declined);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
        StepPauseState::SearchPick {
            chooser,
            searched_player,
            candidates,
            mut selected,
            min,
            max,
            destinations,
            shuffle_library,
            reveal,
            tapped,
        } => {
            if actor != chooser {
                return Err(EngineError::IllegalAction("search actor mismatch".into()));
            }
            match answer {
                ChoiceAnswer::PickObject { id } => {
                    if !candidates.contains(&id) || selected.contains(&id) {
                        return Err(EngineError::IllegalAction("invalid search pick".into()));
                    }
                    selected.push(id);
                    if selected.len() < usize::from(max) && selected.len() < candidates.len() {
                        let next_pid = state.allocate_prompt_id();
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pause_state = Some(StepPauseState::SearchPick {
                                chooser,
                                searched_player,
                                candidates: candidates.clone(),
                                selected,
                                min,
                                max,
                                destinations,
                                shuffle_library,
                                reveal,
                                tapped,
                            });
                        }
                        if let Some(fr) = state.resolution_frames_mut().last_mut() {
                            fr.pending_prompt_id = Some(next_pid);
                        }
                        let req = ChoiceRequest::ChooseObjects {
                            prompt_id: next_pid,
                            candidates,
                            min,
                            max,
                        };
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id: next_pid,
                            chooser,
                            request: req.clone(),
                            owner: PromptOwner::ResolutionFrame,
                        }));
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(Some(StepExecOutcome::Paused {
                            prompt_id: next_pid,
                            chooser,
                        }));
                    }
                }
                ChoiceAnswer::DonePicking => {
                    if selected.len() < usize::from(min) {
                        return Err(EngineError::IllegalAction(
                            "fewer than minimum search objects picked".into(),
                        ));
                    }
                }
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected PickObject or DonePicking for Search".into(),
                    ));
                }
            }
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome = super::library_hand::finalize_search_picks(
                state,
                &step,
                chooser,
                searched_player,
                selected,
                &destinations,
                shuffle_library,
                reveal,
                tapped,
            )?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
        StepPauseState::ReorderLibrary {
            player,
            mode,
            remaining,
            top_order,
            bottom_or_gy,
            deciding_card,
            may_shuffle,
        } => {
            if actor != player {
                return Err(EngineError::IllegalAction("reorder actor mismatch".into()));
            }
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome = super::library_hand::resume_reorder_library(
                state,
                &step,
                player,
                mode,
                remaining,
                top_order,
                bottom_or_gy,
                deciding_card,
                may_shuffle,
                answer,
            )?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
        StepPauseState::DigPick {
            player,
            revealed,
            candidates,
            mut selected,
            min,
            max,
        } => {
            if actor != player {
                return Err(EngineError::IllegalAction("dig actor mismatch".into()));
            }
            match answer {
                ChoiceAnswer::PickObject { id } => {
                    if !candidates.contains(&id) || selected.contains(&id) {
                        return Err(EngineError::IllegalAction("invalid dig pick".into()));
                    }
                    selected.push(id);
                    if selected.len() < usize::from(max) && selected.len() < candidates.len() {
                        let next_pid = state.allocate_prompt_id();
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pause_state = Some(StepPauseState::DigPick {
                                player,
                                revealed,
                                candidates: candidates.clone(),
                                selected,
                                min,
                                max,
                            });
                        }
                        if let Some(fr) = state.resolution_frames_mut().last_mut() {
                            fr.pending_prompt_id = Some(next_pid);
                        }
                        let req = ChoiceRequest::ChooseObjects {
                            prompt_id: next_pid,
                            candidates,
                            min,
                            max,
                        };
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id: next_pid,
                            chooser: player,
                            request: req.clone(),
                            owner: PromptOwner::ResolutionFrame,
                        }));
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(Some(StepExecOutcome::Paused {
                            prompt_id: next_pid,
                            chooser: player,
                        }));
                    }
                }
                ChoiceAnswer::DonePicking => {
                    if selected.len() < usize::from(min) {
                        return Err(EngineError::IllegalAction(
                            "fewer than minimum dig cards picked".into(),
                        ));
                    }
                }
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected PickObject or DonePicking for Dig".into(),
                    ));
                }
            }
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome =
                super::library_hand::finalize_dig_picks(state, &step, player, revealed, selected)?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
        StepPauseState::ChooseNumber { chooser, purpose } => {
            if actor != chooser {
                return Err(EngineError::IllegalAction("number chooser mismatch".into()));
            }
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction("expected PickNumber".into()));
            };
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome = super::resume_number_choice_op(state, &step, chooser, purpose, value)?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
        StepPauseState::ChooseMode {
            chooser,
            purpose,
            options,
            mut picked,
            allow_duplicates,
            min,
            max,
        } => {
            if actor != chooser {
                return Err(EngineError::IllegalAction("mode chooser mismatch".into()));
            }
            match answer {
                ChoiceAnswer::PickMode { index } => {
                    if usize::from(index) >= options.len()
                        || (!allow_duplicates && picked.contains(&index))
                    {
                        return Err(EngineError::IllegalAction("invalid mode index".into()));
                    }
                    picked.push(index);
                    if picked.len() < usize::from(max)
                        && (allow_duplicates || picked.len() < options.len())
                    {
                        let next_pid = state.allocate_prompt_id();
                        let option_count = options.len() as u16;
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pause_state = Some(StepPauseState::ChooseMode {
                                chooser,
                                purpose,
                                options,
                                picked,
                                allow_duplicates,
                                min,
                                max,
                            });
                        }
                        if let Some(fr) = state.resolution_frames_mut().last_mut() {
                            fr.pending_prompt_id = Some(next_pid);
                        }
                        let req = ChoiceRequest::ChooseMode {
                            prompt_id: next_pid,
                            option_count,
                            min,
                            max,
                        };
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id: next_pid,
                            chooser,
                            request: req.clone(),
                            owner: PromptOwner::ResolutionFrame,
                        }));
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(Some(StepExecOutcome::Paused {
                            prompt_id: next_pid,
                            chooser,
                        }));
                    }
                }
                ChoiceAnswer::DonePicking => {
                    if picked.len() < usize::from(min) {
                        return Err(EngineError::IllegalAction(
                            "fewer than minimum modes picked".into(),
                        ));
                    }
                }
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected PickMode or DonePicking".into(),
                    ));
                }
            }
            let step = current_step_cloned(state)
                .ok_or_else(|| EngineError::IllegalAction("missing step".into()))?;
            let outcome =
                super::resume_mode_choice_op(state, &step, chooser, purpose, &options, &picked)?;
            match outcome {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                StepExecOutcome::EnterSubframe => Ok(Some(StepExecOutcome::EnterSubframe)),
                other => Ok(Some(other)),
            }
        }
        StepPauseState::WaitingOnReplacement { .. } => Err(EngineError::IllegalAction(format!(
            "prompt {prompt_id} is waiting on replacement, not direct choice"
        ))),
    }
}

/// Resumes an active `ResolutionFrame` after `replace::resume_and_finalize_replacement` finishes.
pub(crate) fn handle_replacement_resume(
    state: &mut GameState,
    outcome: crate::choices::ReplacementResumeOutcome,
    dispatch_op: fn(&mut GameState, &EffectStep) -> Result<StepExecOutcome, EngineError>,
) -> Result<Option<StepExecOutcome>, EngineError> {
    let pause = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pause_state.take());
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = None;
    }

    let Some(StepPauseState::WaitingOnReplacement {
        batch_remaining,
        batch_idx,
        op_tag,
    }) = pause
    else {
        return Ok(None);
    };

    let step = current_step_cloned(state)
        .ok_or_else(|| EngineError::IllegalAction("missing step after replacement".into()))?;

    match outcome {
        crate::choices::ReplacementResumeOutcome::Paused { prompt_id, chooser } => {
            set_waiting_on_replacement(state, prompt_id, batch_remaining, batch_idx, op_tag);
            Ok(Some(StepExecOutcome::Paused { prompt_id, chooser }))
        }
        crate::choices::ReplacementResumeOutcome::ZoneMove(mv) => {
            let bind_as = step.bind_as.as_deref();
            let moved_raw = if let MoveObjectOutcome::Moved(ref r) = mv {
                Some(r.clone())
            } else {
                None
            };
            let mv_res = handle_zone_move_outcome(
                state,
                mv,
                bind_as,
                batch_remaining.clone(),
                batch_idx,
                op_tag,
            )?;
            match mv_res {
                StepExecOutcome::Paused { prompt_id, chooser } => {
                    return Ok(Some(StepExecOutcome::Paused { prompt_id, chooser }));
                }
                StepExecOutcome::EnterSubframe => {
                    return Ok(Some(StepExecOutcome::EnterSubframe));
                }
                _ => {}
            }

            if op_tag == OP_TAG_SEARCH {
                let mut cont = state
                    .resolution_vms_mut()
                    .last_mut()
                    .and_then(|vm| vm.pending_continuation.take())
                    .unwrap_or_default();
                if let Some(ref raw) = moved_raw {
                    cont.search_moved_ids.push(raw.new_id.unwrap_or(raw.old_id));
                }
                let fallback_actor = resolve_step_actor(state, &step);
                let res = super::library_hand::finalize_search_picks_from(
                    state,
                    &step,
                    cont.search_chooser.unwrap_or(fallback_actor),
                    cont.search_searched_player.unwrap_or(fallback_actor),
                    &batch_remaining,
                    batch_idx + 1,
                    &cont.search_destinations,
                    cont.search_shuffle,
                    cont.search_reveal,
                    cont.search_tapped,
                    cont.search_moved_ids,
                )?;
                if !matches!(res, StepExecOutcome::Done) {
                    return Ok(Some(res));
                }
            } else {
                if let Some(ref raw) = moved_raw {
                    super::zones_tokens::apply_post_zone_move_effects(state, &step, op_tag, raw);
                }
                let res = super::zones_tokens::resume_zone_move_batch(
                    state,
                    &step,
                    &batch_remaining,
                    batch_idx + 1,
                    op_tag,
                )?;
                if !matches!(res, StepExecOutcome::Done) {
                    return Ok(Some(res));
                }
                if op_tag == OP_TAG_DISCARD {
                    let cont_res = super::zones_tokens::continue_discard_remaining(state, &step)?;
                    if !matches!(cont_res, StepExecOutcome::Done) {
                        return Ok(Some(cont_res));
                    }
                }
            }

            finish_accepted_step(state, &step);
            advance_active_pc(state);
            let _ = dispatch_op;
            Ok(None)
        }
        crate::choices::ReplacementResumeOutcome::EffectResolved(rep_out) => {
            let res = super::resume_effect_replacement_op(
                state,
                &step,
                op_tag,
                *rep_out,
                &batch_remaining,
                batch_idx,
            )?;
            match res {
                StepExecOutcome::Done => {
                    finish_accepted_step(state, &step);
                    advance_active_pc(state);
                    Ok(None)
                }
                other => Ok(Some(other)),
            }
        }
    }
}

pub(crate) fn move_opts_for_zone(
    _dest: Zone,
    tapped: bool,
    controller: Option<PlayerId>,
    pos: ZonePosition,
) -> MoveRawOptions {
    MoveRawOptions {
        controller,
        to_player: controller,
        position: pos,
        enter_tapped: tapped,
        ..Default::default()
    }
}

pub(crate) fn active_legal_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let Some(pp) = state.pending_prompt() else {
        return Vec::new();
    };
    if pp.chooser != player || !matches!(pp.owner, PromptOwner::ResolutionFrame) {
        return Vec::new();
    }
    let Some(vm) = state.resolution_vms().last() else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    match &pp.request {
        ChoiceRequest::YesNo { prompt_id, .. }
        | ChoiceRequest::GuidedResolution { prompt_id, .. } => {
            actions.push(Action::Answer {
                prompt_id: *prompt_id,
                answer: ChoiceAnswer::YesNo { value: true },
            });
            actions.push(Action::Answer {
                prompt_id: *prompt_id,
                answer: ChoiceAnswer::YesNo { value: false },
            });
        }
        ChoiceRequest::ChooseObjects {
            prompt_id,
            candidates,
            min,
            max,
        } => {
            let picked = match vm.pause_state.as_ref() {
                Some(StepPauseState::SelectionPick { selected, .. })
                | Some(StepPauseState::SearchPick { selected, .. })
                | Some(StepPauseState::DigPick { selected, .. }) => selected.as_slice(),
                _ => &[],
            };
            if picked.len() < usize::from(*max) {
                for &id in candidates {
                    if !picked.contains(&id) {
                        actions.push(Action::Answer {
                            prompt_id: *prompt_id,
                            answer: ChoiceAnswer::PickObject { id },
                        });
                    }
                }
            }
            if picked.len() >= usize::from(*min) {
                actions.push(Action::Answer {
                    prompt_id: *prompt_id,
                    answer: ChoiceAnswer::DonePicking,
                });
            }
        }
        ChoiceRequest::ChooseMode {
            prompt_id,
            option_count,
            min,
            ..
        } => {
            let (picked, allow_dup) = match vm.pause_state.as_ref() {
                Some(StepPauseState::ChooseMode {
                    picked,
                    allow_duplicates,
                    ..
                }) => (picked.as_slice(), *allow_duplicates),
                _ => (&[][..], false),
            };
            for idx in 0..*option_count {
                if allow_dup || !picked.contains(&idx) {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::PickMode { index: idx },
                    });
                }
            }
            if picked.len() >= usize::from(*min) {
                actions.push(Action::Answer {
                    prompt_id: *prompt_id,
                    answer: ChoiceAnswer::DonePicking,
                });
            }
        }
        ChoiceRequest::ChooseNumber {
            prompt_id,
            min,
            max,
        } => {
            if min <= max {
                let count = (i64::from(*max) - i64::from(*min) + 1).min(64);
                for off in 0..count {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::PickNumber {
                            value: (*min as i64 + off) as i32,
                        },
                    });
                }
            }
        }
        ChoiceRequest::OrderObjects { prompt_id, objects } => {
            for &id in objects {
                actions.push(Action::Answer {
                    prompt_id: *prompt_id,
                    answer: ChoiceAnswer::OrderNext { id },
                });
            }
        }
        ChoiceRequest::ChooseReplacement {
            prompt_id,
            option_count,
        } => {
            for idx in 0..*option_count {
                actions.push(Action::Answer {
                    prompt_id: *prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index: idx },
                });
            }
        }
    }
    actions.push(Action::Concede);
    actions
}

#[allow(dead_code)]
fn _keep_imports_used(_: ReplacementOutcome) {}
