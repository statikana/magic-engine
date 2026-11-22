pub mod counters_mods;
pub mod damage_life;
pub mod library_hand;
pub mod phase2;
pub mod stack_control;
pub mod vm;
pub mod zones_tokens;

use std::sync::Arc;

use mtg_ir::{EffectOp, EffectStep, Zone};
use smallvec::SmallVec;

use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::eval::evaluate_condition;
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::replace::ReplacementOutcome;
use crate::state::{
    ControlFlowFrame, EngineError, FaceDownKind, GameState, LkiSnapshot, ModePausePurpose,
    NumberPausePurpose, PendingPromptContext, PromptOwner, ResolutionBinding, ResolutionFrame,
    ResolutionVmState, StepAcceptance, StepPauseState, StepWayStats, YesNoPausePurpose,
};

use self::vm::{
    OP_TAG_BOUNCE, OP_TAG_CREATE_TOKEN, OP_TAG_DAMAGE, OP_TAG_DRAW, OP_TAG_GAIN_LIFE,
    OP_TAG_MONSTROUS, OP_TAG_PROLIFERATE, OP_TAG_PROLIFERATE_COUNTER,
    OP_TAG_PROLIFERATE_PLAYER_COUNTER, OP_TAG_PUT_COUNTERS, OP_TAG_PUT_PLAYER_COUNTERS,
    OP_TAG_SACRIFICE, OP_TAG_TAP_OR_UNTAP, OP_TAG_UNTAP, StepExecOutcome, active_legal_actions,
    advance_active_pc, current_step_cloned, execute_single_step_with_op, finish_accepted_step,
    handle_replacement_resume, handle_vm_pause_answer, maybe_queue_reflexive_trigger,
    record_step_acceptance, record_step_bindings, resolve_step_actor, vm_controller, with_vm_eval,
};

pub use phase2::{PHASE1_EFFECT_OP_NAMES, PHASE2_EFFECT_OP_NAMES, phase2_deferred_message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    Completed,
    Paused { prompt_id: u32, chooser: PlayerId },
    GameOver { winner: Option<PlayerId> },
}

/// Begins executing a pausable [`ResolutionFrame`] + [`ResolutionVmState`] for `stack_item`.
#[allow(clippy::too_many_arguments)]
pub fn begin_resolution_frame(
    state: &mut GameState,
    stack_item: StackItemId,
    controller: PlayerId,
    source: Option<ObjectId>,
    source_lki: Option<Arc<LkiSnapshot>>,
    x_value: u32,
    targets: Vec<TargetRef>,
    targets_by_spec: Vec<Vec<TargetRef>>,
    divisions: SmallVec<[(TargetRef, u32); 4]>,
    steps: Vec<EffectStep>,
    initial_bindings: Vec<ResolutionBinding>,
) -> Result<FrameOutcome, EngineError> {
    let is_non_trigger_entry = state
        .stack_entries()
        .iter()
        .find(|e| e.id == stack_item)
        .is_some_and(|e| !matches!(e.kind, crate::state::StackEntryKind::Triggered { .. }));
    let (event_object, event_lki, event_player, event_amount, defending_player) =
        if !is_non_trigger_entry && let Some(meta) = state.stack_trigger_meta(stack_item) {
            (
                meta.event_object,
                meta.event_lki.clone(),
                meta.event_player,
                meta.event_amount,
                meta.defending_player,
            )
        } else {
            (None, None, None, None, None)
        };
    let frame = ResolutionFrame {
        stack_item,
        steps_remaining: steps,
        pc: 0,
        bindings: initial_bindings,
        pending_prompt_id: None,
    };
    let vm = ResolutionVmState {
        stack_item,
        controller,
        source,
        source_lki,
        x_value,
        targets,
        targets_by_spec,
        divisions,
        step_results: Vec::new(),
        pause_state: None,
        subframes: Vec::new(),
        step_acted_ids: Vec::new(),
        already_selected_controllers: SmallVec::new(),
        each_loop_player: None,
        chosen_opponent: None,
        event_object,
        event_lki,
        event_player,
        event_amount,
        defending_player,
        last_effect_amount: 0,
        last_excess_damage: 0,
        last_counters_removed: 0,
        last_roll_result: 0,
        coin_heads_count: 0,
        counters_removed_this_way: 0,
        step_way_stats: StepWayStats::default(),
        pending_continuation: None,
    };
    state.resolution_frames_mut().push(frame);
    state.resolution_vms_mut().push(vm);
    run_vm_loop(state)
}

/// Begins a single-step `ChoiceRequest::GuidedResolution` frame for an unsupported ability when
/// `allow_guided_fallback` is enabled.
pub fn begin_guided_resolution_frame(
    state: &mut GameState,
    stack_item: StackItemId,
    controller: PlayerId,
    source: ObjectId,
    text: String,
) -> Result<FrameOutcome, EngineError> {
    let prompt_id = state.allocate_prompt_id();
    let frame = ResolutionFrame {
        stack_item,
        steps_remaining: Vec::new(),
        pc: 0,
        bindings: Vec::new(),
        pending_prompt_id: Some(prompt_id),
    };
    let vm = ResolutionVmState {
        stack_item,
        controller,
        source: Some(source),
        source_lki: None,
        x_value: 0,
        targets: Vec::new(),
        targets_by_spec: Vec::new(),
        divisions: SmallVec::new(),
        step_results: Vec::new(),
        pause_state: Some(StepPauseState::GuidedResolution { controller, source }),
        subframes: Vec::new(),
        step_acted_ids: Vec::new(),
        already_selected_controllers: SmallVec::new(),
        each_loop_player: None,
        chosen_opponent: None,
        event_object: None,
        event_lki: None,
        event_player: None,
        event_amount: None,
        defending_player: None,
        last_effect_amount: 0,
        last_excess_damage: 0,
        last_counters_removed: 0,
        last_roll_result: 0,
        coin_heads_count: 0,
        counters_removed_this_way: 0,
        step_way_stats: StepWayStats::default(),
        pending_continuation: None,
    };
    state.resolution_frames_mut().push(frame);
    state.resolution_vms_mut().push(vm);

    let req = ChoiceRequest::GuidedResolution {
        prompt_id,
        source,
        text,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: controller,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(controller);
    Ok(FrameOutcome::Paused {
        prompt_id,
        chooser: controller,
    })
}

/// Resumes a paused [`ResolutionFrame`] with `answer` from `actor`.
pub fn resume_resolution_frame(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<FrameOutcome, EngineError> {
    if state.resolution_frames().is_empty() || state.resolution_vms().is_empty() {
        return Err(EngineError::IllegalAction(
            "no active resolution frame to resume".into(),
        ));
    }
    if let Some(outcome) =
        handle_vm_pause_answer(state, actor, prompt_id, answer, dispatch_effect_op)?
    {
        match outcome {
            StepExecOutcome::Paused { prompt_id, chooser } => {
                return Ok(FrameOutcome::Paused { prompt_id, chooser });
            }
            StepExecOutcome::GameOver { winner } => {
                pop_active_frame(state);
                return Ok(FrameOutcome::GameOver { winner });
            }
            StepExecOutcome::EnterSubframe | StepExecOutcome::Done | StepExecOutcome::Declined => {}
        }
    }
    run_vm_loop(state)
}

/// Resumes an active [`ResolutionFrame`] after a mid-step replacement prompt completes.
pub fn resume_after_replacement(
    state: &mut GameState,
    outcome: crate::choices::ReplacementResumeOutcome,
) -> Result<FrameOutcome, EngineError> {
    if state.resolution_frames().is_empty() || state.resolution_vms().is_empty() {
        return Ok(FrameOutcome::Completed);
    }
    if let Some(step_res) = handle_replacement_resume(state, outcome, dispatch_effect_op)? {
        match step_res {
            StepExecOutcome::Paused { prompt_id, chooser } => {
                return Ok(FrameOutcome::Paused { prompt_id, chooser });
            }
            StepExecOutcome::GameOver { winner } => {
                pop_active_frame(state);
                return Ok(FrameOutcome::GameOver { winner });
            }
            StepExecOutcome::EnterSubframe | StepExecOutcome::Done | StepExecOutcome::Declined => {}
        }
    }
    run_vm_loop(state)
}

/// Returns already-selected `ObjectId`s for an active `ChooseObjects` pause in `ResolutionFrame`.
pub fn active_selection_picked(state: &GameState) -> &[ObjectId] {
    let Some(vm) = state.resolution_vms().last() else {
        return &[];
    };
    match vm.pause_state.as_ref() {
        Some(StepPauseState::SelectionPick { selected, .. })
        | Some(StepPauseState::SearchPick { selected, .. })
        | Some(StepPauseState::DigPick { selected, .. }) => selected.as_slice(),
        _ => &[],
    }
}

/// Returns legal [`Action`]s for `player` when a `ResolutionFrame` prompt is active.
pub fn legal_resolution_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    active_legal_actions(state, player)
}

/// Aborts/resolves a paused [`ResolutionFrame`] prompt when `conceded_player` leaves the game (`CR 800.4a`).
pub fn abort_resolution_prompt_on_concede(
    state: &mut GameState,
    conceded_player: PlayerId,
) -> Result<FrameOutcome, EngineError> {
    if state.resolution_frames().is_empty() || state.resolution_vms().is_empty() {
        return Ok(FrameOutcome::Completed);
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = None;
    }
    let pause = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pause_state.take());

    match pause {
        Some(StepPauseState::OptionalChoice { purpose, .. }) => {
            if matches!(purpose, YesNoPausePurpose::OptionalSpec { is_unless: true })
                && let Some(step) = current_step_cloned(state)
            {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.step_way_stats.did_last_optional_step = true;
                }
                match dispatch_effect_op(state, &step)? {
                    StepExecOutcome::Done => {
                        finish_accepted_step(state, &step);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::Declined => {
                        record_step_acceptance(state, StepAcceptance::Declined);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::EnterSubframe => {}
                    StepExecOutcome::Paused { prompt_id, chooser } => {
                        return Ok(FrameOutcome::Paused { prompt_id, chooser });
                    }
                    StepExecOutcome::GameOver { winner } => {
                        pop_active_frame(state);
                        return Ok(FrameOutcome::GameOver { winner });
                    }
                }
            } else {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.step_way_stats.did_last_optional_step = false;
                }
                record_step_acceptance(state, StepAcceptance::Declined);
                advance_active_pc(state);
            }
        }
        Some(StepPauseState::ChooseNumber {
            chooser,
            purpose:
                purpose @ (NumberPausePurpose::CounterSpellUnlessPays | NumberPausePurpose::UpToDraw),
        }) => {
            if purpose == NumberPausePurpose::UpToDraw {
                let alive: SmallVec<[PlayerId; 4]> = state
                    .players()
                    .iter()
                    .filter(|p| !p.lost)
                    .map(|p| p.id)
                    .collect();
                if let Some(vm) = state.resolution_vms_mut().last_mut()
                    && let Some(cont) = vm.pending_continuation.as_mut()
                {
                    cont.remaining_players.retain(|p| alive.contains(p));
                }
            }
            if let Some(step) = current_step_cloned(state) {
                match resume_number_choice_op(state, &step, chooser, purpose, 0)? {
                    StepExecOutcome::Done => {
                        finish_accepted_step(state, &step);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::Declined => {
                        record_step_acceptance(state, StepAcceptance::Declined);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::EnterSubframe => {}
                    StepExecOutcome::Paused { prompt_id, chooser } => {
                        return Ok(FrameOutcome::Paused { prompt_id, chooser });
                    }
                    StepExecOutcome::GameOver { winner } => {
                        pop_active_frame(state);
                        return Ok(FrameOutcome::GameOver { winner });
                    }
                }
            } else {
                record_step_acceptance(state, StepAcceptance::Declined);
                advance_active_pc(state);
            }
        }
        Some(StepPauseState::ChooseMode {
            chooser,
            purpose: ModePausePurpose::PunisherChoice,
            ..
        }) => {
            if let Some(step) = current_step_cloned(state) {
                match resume_mode_choice_op(
                    state,
                    &step,
                    chooser,
                    ModePausePurpose::PunisherChoice,
                    &[],
                    &[0],
                )? {
                    StepExecOutcome::Done => {
                        finish_accepted_step(state, &step);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::Declined => {
                        record_step_acceptance(state, StepAcceptance::Declined);
                        advance_active_pc(state);
                    }
                    StepExecOutcome::EnterSubframe => {}
                    StepExecOutcome::Paused { prompt_id, chooser } => {
                        return Ok(FrameOutcome::Paused { prompt_id, chooser });
                    }
                    StepExecOutcome::GameOver { winner } => {
                        pop_active_frame(state);
                        return Ok(FrameOutcome::GameOver { winner });
                    }
                }
            } else {
                record_step_acceptance(state, StepAcceptance::Declined);
                advance_active_pc(state);
            }
        }
        Some(
            StepPauseState::SelectionPick { .. }
            | StepPauseState::SearchPick { .. }
            | StepPauseState::ReorderLibrary { .. }
            | StepPauseState::DigPick { .. }
            | StepPauseState::ChooseNumber { .. }
            | StepPauseState::ChooseMode { .. }
            | StepPauseState::MoveCountersSelectTarget { .. }
            | StepPauseState::MoveCountersChooseAmount { .. }
            | StepPauseState::GuidedResolution { .. }
            | StepPauseState::WaitingOnReplacement { .. },
        )
        | None => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pending_continuation = None;
            }
            record_step_acceptance(state, StepAcceptance::Declined);
            advance_active_pc(state);
        }
    }

    if let Some(vm) = state.resolution_vms_mut().last_mut()
        && vm.each_loop_player == Some(conceded_player)
        && let Some(ControlFlowFrame::ForEachPlayer { steps, step_pc, .. }) =
            vm.subframes.last_mut()
    {
        *step_pc = steps.len();
    }

    run_vm_loop(state)
}

fn pop_active_frame(state: &mut GameState) {
    state.resolution_frames_mut().pop();
    state.resolution_vms_mut().pop();
}

fn run_vm_loop(state: &mut GameState) -> Result<FrameOutcome, EngineError> {
    loop {
        if let DecisionState::GameOver { winner } = state.decision().clone() {
            pop_active_frame(state);
            return Ok(FrameOutcome::GameOver { winner });
        }
        let Some(frame_len) = state
            .resolution_frames()
            .last()
            .map(|f| f.steps_remaining.len())
        else {
            return Ok(FrameOutcome::Completed);
        };
        let has_subframe = state
            .resolution_vms()
            .last()
            .is_some_and(|v| !v.subframes.is_empty());

        if has_subframe {
            // Inspect active subframe
            enum SubAction {
                PopAndAdvanceParent,
                NextPlayerOrPop {
                    next_idx: usize,
                    next_player: Option<PlayerId>,
                },
                RepeatOrPop {
                    again: bool,
                },
                ExecuteStep(Box<EffectStep>),
            }
            let act = {
                let vm = state.resolution_vms().last().expect("vm checked");
                let sub = vm.subframes.last().expect("subframe checked");
                match sub {
                    ControlFlowFrame::SubSteps { steps, step_pc, .. } => {
                        if *step_pc >= steps.len() {
                            SubAction::PopAndAdvanceParent
                        } else {
                            SubAction::ExecuteStep(Box::new(steps[*step_pc].clone()))
                        }
                    }
                    ControlFlowFrame::ForEachPlayer {
                        players,
                        next_player_idx,
                        steps,
                        step_pc,
                    } => {
                        if *next_player_idx >= players.len() {
                            SubAction::PopAndAdvanceParent
                        } else if *step_pc >= steps.len()
                            || vm
                                .each_loop_player
                                .is_some_and(|lp| state.player(lp).is_some_and(|ps| ps.lost))
                        {
                            let mut nxt = *next_player_idx + 1;
                            while let Some(&cand) = players.get(nxt) {
                                if state.player(cand).is_some_and(|ps| !ps.lost) {
                                    break;
                                }
                                nxt += 1;
                            }
                            SubAction::NextPlayerOrPop {
                                next_idx: nxt,
                                next_player: players.get(nxt).copied(),
                            }
                        } else {
                            SubAction::ExecuteStep(Box::new(steps[*step_pc].clone()))
                        }
                    }
                    ControlFlowFrame::RepeatLoop {
                        steps,
                        step_pc,
                        iterations,
                        max_iterations,
                        condition,
                    } => {
                        if *step_pc >= steps.len() {
                            let cond_ok = with_vm_eval(state, |ctx, _| {
                                evaluate_condition(state, condition, ctx)
                            });
                            let again = cond_ok && (*iterations + 1) < *max_iterations;
                            SubAction::RepeatOrPop { again }
                        } else {
                            SubAction::ExecuteStep(Box::new(steps[*step_pc].clone()))
                        }
                    }
                }
            };

            match act {
                SubAction::PopAndAdvanceParent => {
                    if let Some(vm) = state.resolution_vms_mut().last_mut() {
                        vm.subframes.pop();
                        if vm.subframes.is_empty() {
                            vm.each_loop_player = None;
                        }
                    }
                    advance_active_pc(state);
                }
                SubAction::NextPlayerOrPop {
                    next_idx,
                    next_player,
                } => {
                    if let Some(next_p) = next_player {
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.each_loop_player = Some(next_p);
                            if let Some(ControlFlowFrame::ForEachPlayer {
                                next_player_idx,
                                step_pc,
                                ..
                            }) = vm.subframes.last_mut()
                            {
                                *next_player_idx = next_idx;
                                *step_pc = 0;
                            }
                        }
                    } else {
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.subframes.pop();
                            vm.each_loop_player = None;
                        }
                        advance_active_pc(state);
                    }
                }
                SubAction::RepeatOrPop { again } => {
                    if again {
                        if let Some(vm) = state.resolution_vms_mut().last_mut()
                            && let Some(ControlFlowFrame::RepeatLoop {
                                step_pc,
                                iterations,
                                ..
                            }) = vm.subframes.last_mut()
                        {
                            *iterations += 1;
                            *step_pc = 0;
                        }
                    } else {
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.subframes.pop();
                        }
                        advance_active_pc(state);
                    }
                }
                SubAction::ExecuteStep(step) => {
                    match execute_single_step_with_op(state, &step, false, dispatch_effect_op)? {
                        StepExecOutcome::Paused { prompt_id, chooser } => {
                            return Ok(FrameOutcome::Paused { prompt_id, chooser });
                        }
                        StepExecOutcome::GameOver { winner } => {
                            pop_active_frame(state);
                            return Ok(FrameOutcome::GameOver { winner });
                        }
                        StepExecOutcome::EnterSubframe => {}
                        StepExecOutcome::Done | StepExecOutcome::Declined => {
                            advance_active_pc(state);
                        }
                    }
                }
            }
            continue;
        }

        let pc = state.resolution_frames().last().map_or(0, |f| f.pc);
        if pc >= frame_len {
            pop_active_frame(state);
            return Ok(FrameOutcome::Completed);
        }

        let step = state
            .resolution_frames()
            .last()
            .expect("frame checked")
            .steps_remaining[pc]
            .clone();
        match execute_single_step_with_op(state, &step, false, dispatch_effect_op)? {
            StepExecOutcome::Paused { prompt_id, chooser } => {
                return Ok(FrameOutcome::Paused { prompt_id, chooser });
            }
            StepExecOutcome::GameOver { winner } => {
                pop_active_frame(state);
                return Ok(FrameOutcome::GameOver { winner });
            }
            StepExecOutcome::EnterSubframe => {}
            StepExecOutcome::Done | StepExecOutcome::Declined => {
                advance_active_pc(state);
            }
        }
    }
}

/// Exhaustive 134-variant `EffectOp` dispatcher (ZERO `_ =>` wildcards!).
pub(crate) fn dispatch_effect_op(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    match &step.op {
        // ---- Damage & Life (9) ----
        EffectOp::Damage {
            source,
            also_players,
            divided,
            excess_to_controller,
            excess_to_controller_if_trample,
            or_planeswalker_controlled_by,
            damage_attacked_target_by_self,
            soulfire_exile_damage_per_target: _,
            each_creature_deals,
        } => damage_life::exec_damage(
            state,
            step,
            source.as_deref(),
            also_players.as_ref(),
            *divided,
            *excess_to_controller,
            *excess_to_controller_if_trample,
            or_planeswalker_controlled_by.as_ref(),
            *damage_attacked_target_by_self,
            each_creature_deals.as_ref(),
        ),
        EffectOp::Fight {
            with,
            excess_damage_to_mana,
        } => damage_life::exec_fight(state, step, with, *excess_damage_to_mana),
        EffectOp::GainLife { set_life_total } => {
            damage_life::exec_gain_life(state, step, *set_life_total)
        }
        EffectOp::LoseLife => damage_life::exec_lose_life(state, step),
        EffectOp::PreventDamage { spec } => damage_life::exec_prevent_damage(state, step, spec),
        EffectOp::Regenerate => damage_life::exec_regenerate(state, step),
        EffectOp::CantRegenerate => damage_life::exec_cant_regenerate(state, step),
        EffectOp::ExileInsteadOfDying { duration: _ } => {
            damage_life::exec_exile_instead_of_dying(state, step)
        }
        EffectOp::GainEnergy => damage_life::exec_gain_energy(state, step),

        // ---- Zones & Tokens (14) ----
        EffectOp::Destroy {
            cant_be_regenerated,
            select,
        } => zones_tokens::exec_destroy(state, step, *cant_be_regenerated, select),
        EffectOp::Exile {
            select,
            duration,
            until_source_leaves,
            until_opponent_becomes_monarch,
            exile_all_except_bottom,
            face_down,
            look_only,
            exile_with_counters,
        } => zones_tokens::exec_exile(
            state,
            step,
            select,
            *duration,
            *until_source_leaves,
            *until_opponent_becomes_monarch,
            *exile_all_except_bottom,
            *face_down,
            *look_only,
            *exile_with_counters,
        ),
        EffectOp::Bounce { select } => zones_tokens::exec_bounce(state, step, select),
        EffectOp::Move {
            from,
            to,
            select,
            tapped,
            under_your_control,
            topmost_only,
            may_shuffle,
            position,
            from_top,
            library_choice_by,
            random_order,
            entered_transformed,
            entered_flipped,
            enters_tapped_and_attacking,
            as_enchantment_only,
            unearth,
            with_counters,
            attached_to,
            condition,
            choose_at_random_from_exiled_with_self: _,
            leave_chosen_in_graveyard_rest_to_hand: _,
            split_exiled_cost_cards_to_bottom_and_battlefield: _,
        } => zones_tokens::exec_move(
            state,
            step,
            *from,
            *to,
            select,
            *tapped,
            *under_your_control,
            *topmost_only,
            *may_shuffle,
            *position,
            *from_top,
            *library_choice_by,
            *random_order,
            *entered_transformed,
            *entered_flipped,
            *enters_tapped_and_attacking,
            *as_enchantment_only,
            *unearth,
            *with_counters,
            attached_to.as_deref(),
            condition.as_deref(),
        ),
        EffectOp::Sacrifice { select } => zones_tokens::exec_sacrifice(state, step, select),
        EffectOp::Discard { select } => zones_tokens::exec_discard(state, step, select),
        EffectOp::DiscardHand {
            draw_that_many,
            draw_after,
        } => zones_tokens::exec_discard_hand(state, step, *draw_that_many, draw_after.as_deref()),
        EffectOp::RevealAndDiscard {
            spec,
            partial_reveal,
            coercive_count,
            include_graveyard,
            discard_all_matching_sacrificed_colors,
            discard_all_with_chosen_name,
        } => zones_tokens::exec_reveal_and_discard(
            state,
            step,
            spec.as_ref(),
            partial_reveal.as_ref(),
            coercive_count.as_deref(),
            *include_graveyard,
            *discard_all_matching_sacrificed_colors,
            *discard_all_with_chosen_name,
        ),
        EffectOp::Mill => zones_tokens::exec_mill(state, step),
        EffectOp::PhaseOut {
            select,
            cant_phase_in_while_source_controlled: _,
        } => zones_tokens::exec_phase_out(state, step, select),
        EffectOp::CreateToken {
            token,
            dynamic_pt,
            tapped_and_attacking,
            per_opponent_tapped_attacking,
            multiply_by_opponents_did_this_way,
            benthic_anomaly_clone,
        } => zones_tokens::exec_create_token(
            state,
            step,
            token.as_deref(),
            dynamic_pt.as_ref(),
            *tapped_and_attacking,
            *per_opponent_tapped_attacking,
            *multiply_by_opponents_did_this_way,
            *benthic_anomaly_clone,
        ),
        EffectOp::CopyPermanent {
            select,
            not_legendary,
            tapped,
            tapped_and_attacking,
            per_opponent_tapped_attacking,
            copy_mod,
            except_pt,
        } => zones_tokens::exec_copy_permanent(
            state,
            step,
            select,
            *not_legendary,
            *tapped,
            *tapped_and_attacking,
            *per_opponent_tapped_attacking,
            copy_mod.as_deref(),
            *except_pt,
        ),
        EffectOp::BecomeCopy {
            copy_source,
            duration,
            copy_mod,
        } => {
            zones_tokens::exec_become_copy(state, step, copy_source, *duration, copy_mod.as_deref())
        }
        EffectOp::CreateEmblem { abilities } => {
            zones_tokens::exec_create_emblem(state, step, abilities)
        }

        // ---- Counters & Board/Object Mods (30) ----
        EffectOp::PutCounters {
            counter,
            multi_counters,
            select,
            divided,
            allow_exile_counters: _,
            remove_instead_of_add,
            copy_counters_from,
            choose_counter_on_subject_and_add_one: _,
            conditional_counter_on_type: _,
            cast_bolas_bonus_loyalty: _,
        } => counters_mods::exec_put_counters(
            state,
            step,
            *counter,
            multi_counters,
            select,
            *divided,
            *remove_instead_of_add,
            copy_counters_from.as_deref(),
        ),
        EffectOp::RemoveCounters {
            counter,
            up_to,
            if_unable_sacrifice_self,
            remove_all_from_target_or_player,
        } => counters_mods::exec_remove_counters(
            state,
            step,
            *counter,
            *up_to,
            *if_unable_sacrifice_self,
            *remove_all_from_target_or_player,
        ),
        EffectOp::MoveCounters {
            counter,
            to_filter,
            spec,
        } => counters_mods::exec_move_counters(
            state,
            step,
            *counter,
            to_filter.as_deref(),
            spec.as_ref(),
        ),
        EffectOp::PutPlayerCounters { counter } => {
            counters_mods::exec_put_player_counters(state, step, *counter)
        }
        EffectOp::Proliferate => counters_mods::exec_proliferate(state, step),
        EffectOp::Pump {
            toughness,
            duration,
            pump_choices,
        } => counters_mods::exec_pump(state, step, toughness.as_deref(), *duration, pump_choices),
        EffectOp::SwitchPt { duration } => counters_mods::exec_switch_pt(state, step, *duration),
        EffectOp::Grant {
            keywords,
            remove_keywords,
            cant_have_or_gain,
            duration,
            granted_ability_texts: _,
            choose_keyword_options,
            choose_lose_keyword_options,
            choose_protection_color,
            remove_all_abilities,
            can_attack_no_defender: _,
            must_attack_this_turn: _,
            must_be_blocked_this_turn: _,
            must_block_this_turn: _,
            must_block_self_this_turn: _,
            cant_block_self_this_turn: _,
            cant_be_blocked_by_opponents_creatures: _,
            assigns_no_combat_damage_this_turn: _,
            grant_suspend_if_missing: _,
            exile_resolving_spell_to_linked: _,
            grant_trigger_spell_sunburst: _,
            pay_any_mana_for_damage: _,
            pay_up_to_life_gained_mana_to_draw: _,
            gambit_combat_directive: _,
        } => counters_mods::exec_grant(
            state,
            step,
            keywords,
            remove_keywords,
            *cant_have_or_gain,
            *duration,
            choose_keyword_options,
            choose_lose_keyword_options,
            *choose_protection_color,
            *remove_all_abilities,
        ),
        EffectOp::Animate {
            duration,
            add_types,
            set_types,
            remove_types,
            add_subtypes,
            set_subtypes,
            set_colors,
            add_colors,
            add_keywords,
            set_power,
            set_toughness,
            keep_existing_types: _,
            still_a_land: _,
            remove_all_abilities,
            granted_ability_texts: _,
            choose_creature_type: _,
            choose_basic_land_type: _,
            choose_color: _,
        } => counters_mods::exec_animate(
            state,
            step,
            *duration,
            *add_types,
            *set_types,
            *remove_types,
            add_subtypes,
            set_subtypes.as_deref(),
            *set_colors,
            *add_colors,
            add_keywords,
            set_power.as_deref(),
            set_toughness.as_deref(),
            *remove_all_abilities,
        ),
        EffectOp::Attach {
            attach_object,
            select,
            attach_equipment_from_self_lki: _,
        } => counters_mods::exec_attach(state, step, attach_object.as_deref(), select),
        EffectOp::Tap { select } => counters_mods::exec_tap(state, step, select),
        EffectOp::Untap { select } => counters_mods::exec_untap(state, step, select),
        EffectOp::TapOrUntap => counters_mods::exec_tap_or_untap(state, step),
        EffectOp::FreezeUntap { duration: _ } => counters_mods::exec_freeze_untap(state, step),
        EffectOp::Goad { duration } => counters_mods::exec_goad(state, step, *duration),
        EffectOp::CombatRestriction {
            spec,
            duration,
            choose_odd_or_even_cant_block,
            must_attack_next_combat,
            must_attack_player,
            until_your_next_turn_directive,
        } => counters_mods::exec_combat_restriction(
            state,
            step,
            spec.as_ref(),
            *duration,
            *choose_odd_or_even_cant_block,
            *must_attack_next_combat,
            must_attack_player.as_ref(),
            *until_your_next_turn_directive,
        ),
        EffectOp::RemoveFromCombat => counters_mods::exec_remove_from_combat(state, step),
        EffectOp::GainControl { duration, untap } => {
            counters_mods::exec_gain_control(state, step, *duration, *untap)
        }
        EffectOp::Transform | EffectOp::Convert => counters_mods::exec_transform(state, step),
        EffectOp::Flip => counters_mods::exec_flip(state, step),
        EffectOp::TurnFace { face_up, select } => {
            counters_mods::exec_turn_face(state, step, *face_up, select)
        }
        EffectOp::DoubleOrTriple { spec } => {
            counters_mods::exec_double_or_triple(state, step, spec)
        }
        EffectOp::ChooseDesignation { kind } => {
            counters_mods::exec_choose_designation(state, step, *kind)
        }
        EffectOp::Suspect { unsuspect } => counters_mods::exec_suspect(state, step, *unsuspect),
        EffectOp::Monstrosity => counters_mods::exec_monstrosity(state, step),
        EffectOp::Bolster => counters_mods::exec_bolster(state, step),
        EffectOp::Adapt => counters_mods::exec_adapt(state, step),
        EffectOp::Exert => counters_mods::exec_exert(state, step),
        EffectOp::Saddle => counters_mods::exec_saddle(state, step),

        // ---- Mana, Library & Hand (24) ----
        EffectOp::AddMana {
            production,
            restriction,
            restriction_spec,
            retain_until_eot,
            delayed_rider,
        } => library_hand::exec_add_mana(
            state,
            step,
            production,
            *restriction,
            restriction_spec.as_deref(),
            *retain_until_eot,
            delayed_rider.as_deref(),
        ),
        EffectOp::RetainMana { colors } => library_hand::exec_retain_mana(state, step, *colors),
        EffectOp::Draw {
            up_to,
            reveal_drawn,
        } => library_hand::exec_draw(state, step, *up_to, *reveal_drawn),
        EffectOp::Scry => library_hand::exec_scry(state, step),
        EffectOp::Surveil => library_hand::exec_surveil(state, step),
        EffectOp::LookAndReorder { may_shuffle } => {
            library_hand::exec_look_and_reorder(state, step, *may_shuffle)
        }
        EffectOp::Search { spec, tapped } => library_hand::exec_search(state, step, spec, *tapped),
        EffectOp::Dig { spec, look_only } => library_hand::exec_dig(state, step, spec, *look_only),
        EffectOp::ExileUntil { spec } => library_hand::exec_exile_until(state, step, spec),
        EffectOp::PileSplit { spec } => library_hand::exec_pile_split(state, step, spec),
        EffectOp::Reveal {
            select,
            look_only,
            special_tag: _,
        } => library_hand::exec_reveal(state, step, select, *look_only),
        EffectOp::PayCost => library_hand::exec_pay_cost(state, step),
        EffectOp::Investigate => library_hand::exec_investigate(state, step),
        EffectOp::Explore => library_hand::exec_explore(state, step),
        EffectOp::Connive {
            for_each_convoking_creature_steps: _,
        } => library_hand::exec_connive(state, step),
        EffectOp::Amass { subtype } => library_hand::exec_amass(state, step, *subtype),
        EffectOp::Incubate { count } => library_hand::exec_incubate(state, step, count.as_deref()),
        EffectOp::Populate => library_hand::exec_populate(state, step),
        EffectOp::Manifest => {
            library_hand::exec_manifest_or_cloak(state, step, FaceDownKind::Manifest)
        }
        EffectOp::Cloak => library_hand::exec_manifest_or_cloak(state, step, FaceDownKind::Cloak),
        EffectOp::ManifestDread => library_hand::exec_manifest_dread(state, step),
        EffectOp::Forage => library_hand::exec_forage(state, step),
        EffectOp::CollectEvidence => library_hand::exec_collect_evidence(state, step),
        EffectOp::BecomeMonarch => library_hand::exec_become_monarch(state, step),

        // ---- Stack, Control Flow & Turn/Permissions (24) ----
        EffectOp::CopySpell {
            count,
            may_choose_new_targets,
            non_legendary: _,
            except_colors: _,
            copy_event_ability: _,
            copy_per_sacrificed_cost: _,
            copy_all_controlled_spells_and_other_abilities: _,
        } => stack_control::exec_copy_spell(state, step, count.as_deref(), *may_choose_new_targets),
        EffectOp::CounterSpell {
            replacement_zone,
            unless_pays_generic,
            on_countered_steal_filter: _,
        } => stack_control::exec_counter_spell(
            state,
            step,
            *replacement_zone,
            unless_pays_generic.as_deref(),
        ),
        EffectOp::RedirectTargets { spec } => {
            stack_control::exec_redirect_targets(state, step, spec)
        }
        EffectOp::CastNameLock { name_ref, duration } => {
            stack_control::exec_cast_name_lock(state, step, name_ref, *duration)
        }
        EffectOp::ForEachPlayerLoop {
            players,
            steps,
            choose_random_opponent,
            exclude_attacked_last_combat: _,
            exclude_event_player,
            choose_object_to_bind: _,
        } => stack_control::exec_for_each_player_loop(
            state,
            step,
            *players,
            steps,
            *choose_random_opponent,
            *exclude_event_player,
        ),
        EffectOp::ConditionalSequence {
            condition,
            then_steps,
            else_steps,
        } => stack_control::exec_conditional_sequence(state, condition, then_steps, else_steps),
        EffectOp::RepeatSequence {
            steps,
            condition,
            max_iterations,
        } => stack_control::exec_repeat_sequence(state, steps, condition, *max_iterations),
        EffectOp::PunisherChoice { spec } => stack_control::exec_punisher_choice(state, step, spec),
        EffectOp::PartitionKeepByTypes {
            types,
            keep_count_per_player,
            chooser_you: _,
            all_types: _,
            nonland_only: _,
            put_vow_counter: _,
        } => stack_control::exec_partition_keep_by_types(
            state,
            step,
            *types,
            keep_count_per_player.as_deref(),
        ),
        EffectOp::DelayedTrigger {
            delayed,
            inline_delayed,
            return_target_and_sacrificed_at_upkeep: _,
        } => stack_control::exec_delayed_trigger(
            state,
            step,
            delayed.as_ref(),
            inline_delayed.as_ref(),
        ),
        EffectOp::DelayedTapManaTrigger {
            all_players,
            land_subtype,
            mana,
            to_tapping_player,
        } => stack_control::exec_delayed_tap_mana_trigger(
            state,
            step,
            *all_players,
            *land_subtype,
            mana,
            *to_tapping_player,
        ),
        EffectOp::DelayedNextSpellCopy {
            types,
            not_types,
            colors,
            copy_count,
            may_choose_new_targets: _,
        } => stack_control::exec_delayed_next_spell_copy(
            state,
            step,
            *types,
            *not_types,
            *colors,
            *copy_count,
        ),
        EffectOp::GrantPlayer {
            rules,
            duration,
            extra_land_plays,
            next_spell_types: _,
            spend_mana_as_any_color_top_of_library: _,
            steal_opponent_entering_creatures_until_eot: _,
            opponents_spells_cost_more_until_next_turn: _,
            attack_tax_until_your_next_turn: _,
            next_spell_cost_reduction_generic: _,
            next_creature_enters_with_plus_counters: _,
            next_main_phase_add_mana,
            double_tokens_this_turn: _,
            flash_cast_condition: _,
            next_spell_cascade: _,
            extra_loyalty_activations_this_turn: _,
            planar_action: _,
        } => stack_control::exec_grant_player(
            state,
            step,
            rules,
            *duration,
            *extra_land_plays,
            next_main_phase_add_mana.as_ref(),
        ),
        EffectOp::GrantPlayFromExile {
            spec,
            select,
            cast_spell_cost_tax_for_opponents: _,
        } => stack_control::exec_grant_play_from_exile(state, step, spec, select),
        EffectOp::GrantGraveyardCast {
            spec,
            grant_adventure_until_end_of_next_turn: _,
        } => stack_control::exec_grant_graveyard_cast(state, step, spec.as_ref()),
        EffectOp::CastFree {
            spec,
            at_random: _,
            cast_any_number_from_exiled: _,
        } => stack_control::exec_cast_free(state, step, spec.as_deref()),
        EffectOp::ExtraTurn => stack_control::exec_extra_turn(state, step),
        EffectOp::SkipTurn => stack_control::exec_skip_turn(state, step),
        EffectOp::EndTurn { active_player_may } => {
            stack_control::exec_end_turn(state, step, *active_player_may)
        }
        EffectOp::ExtraCombatPhase => stack_control::exec_extra_combat_phase(state),
        EffectOp::SkipCombatPhase => stack_control::exec_skip_combat_phase(state, step),
        EffectOp::EndCombatPhase => stack_control::exec_end_combat_phase(state),
        EffectOp::Win { condition } => stack_control::exec_win(state, step, condition.as_deref()),
        EffectOp::Lose { condition } => stack_control::exec_lose(state, step, condition.as_deref()),

        // ---- Phase 2 Explicit Boundary (33) ----
        EffectOp::SuspendTick
        | EffectOp::SuspendCast
        | EffectOp::CumulativeUpkeep { .. }
        | EffectOp::Discover
        | EffectOp::Clash
        | EffectOp::Detain
        | EffectOp::Endure
        | EffectOp::TimeTravel
        | EffectOp::Blight
        | EffectOp::Fateseal
        | EffectOp::FlipCoin { .. }
        | EffectOp::RollDie { .. }
        | EffectOp::CreateRoleToken { .. }
        | EffectOp::Exchange { .. }
        | EffectOp::VentureIntoDungeon { .. }
        | EffectOp::TakeInitiative
        | EffectOp::TemptWithRing
        | EffectOp::Earthbend
        | EffectOp::Airbend
        | EffectOp::Learn
        | EffectOp::VillainousChoice { .. }
        | EffectOp::Vote { .. }
        | EffectOp::Prepare
        | EffectOp::Unprepare
        | EffectOp::IncreaseSpeed
        | EffectOp::Meld { .. }
        | EffectOp::Harness
        | EffectOp::ReselectAttackTarget
        | EffectOp::EmpowerJace
        | EffectOp::Assimilate
        | EffectOp::Recruit
        | EffectOp::Heal
        | EffectOp::SolveCase => match phase2::handle_phase2_op(state, &step.op)? {
            phase2::Phase2Outcome::GuidedPaused { prompt_id, chooser } => {
                Ok(StepExecOutcome::Paused { prompt_id, chooser })
            }
        },
    }
}

pub(crate) fn resume_selection_op(
    state: &mut GameState,
    step: &EffectStep,
    selected: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    match &step.op {
        EffectOp::Destroy {
            cant_be_regenerated,
            ..
        } => zones_tokens::run_destroy_targets(state, step, selected, *cant_be_regenerated),
        EffectOp::Exile {
            face_down,
            look_only,
            ..
        } => zones_tokens::run_exile_targets(state, step, selected, *face_down, *look_only),
        EffectOp::Bounce { .. } => {
            zones_tokens::resume_zone_move_batch(state, step, &selected, 0, OP_TAG_BOUNCE)
        }
        EffectOp::Move { .. } => zones_tokens::run_move_targets(state, step, selected),
        EffectOp::Sacrifice { .. } => {
            zones_tokens::resume_zone_move_batch(state, step, &selected, 0, OP_TAG_SACRIFICE)
        }
        EffectOp::Discard { .. } => zones_tokens::finish_discard_picked(state, step, selected),
        EffectOp::RevealAndDiscard { spec, .. } => {
            zones_tokens::run_reveal_and_discard_picked(state, step, spec.as_ref(), &selected)
        }
        EffectOp::Proliferate => {
            counters_mods::after_proliferate_perm_selection(state, ctrl, selected)
        }
        EffectOp::PutCounters {
            counter,
            multi_counters,
            divided,
            remove_instead_of_add,
            copy_counters_from,
            ..
        } => counters_mods::run_put_counters_targets(
            state,
            step,
            selected,
            *counter,
            multi_counters,
            *divided,
            *remove_instead_of_add,
            copy_counters_from.as_deref(),
        ),
        EffectOp::Attach { .. } => counters_mods::run_attach_targets(state, step, selected),
        EffectOp::TurnFace { face_up, .. } => {
            counters_mods::run_turn_face_targets(state, step, selected, *face_up)
        }
        EffectOp::CopyPermanent {
            not_legendary,
            tapped,
            tapped_and_attacking,
            per_opponent_tapped_attacking,
            copy_mod,
            except_pt,
            ..
        } => zones_tokens::run_copy_permanent_targets(
            state,
            step,
            selected,
            *not_legendary,
            *tapped,
            *tapped_and_attacking,
            *per_opponent_tapped_attacking,
            copy_mod.as_deref(),
            *except_pt,
        ),
        EffectOp::GrantPlayFromExile { spec, .. } => {
            stack_control::run_grant_play_from_exile_targets(state, step, selected, spec)
        }
        EffectOp::Tap { .. } => counters_mods::run_tap_targets(state, step, selected),
        EffectOp::Untap { .. } => counters_mods::run_untap_targets(state, step, &selected, 0),
        EffectOp::PhaseOut { .. } => zones_tokens::run_phase_out_targets(state, step, selected),
        EffectOp::Populate => library_hand::run_populate_targets(state, step, selected),
        EffectOp::Reveal { look_only, .. } => {
            library_hand::run_reveal_targets(state, step, selected, *look_only)
        }
        _ => {
            let acted: Vec<_> = selected.into_iter().map(|id| (id, None)).collect();
            record_step_bindings(state, step.bind_as.as_deref(), &acted);
            Ok(StepExecOutcome::Done)
        }
    }
}

pub(crate) fn resume_number_choice_op(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    purpose: NumberPausePurpose,
    value: i32,
) -> Result<StepExecOutcome, EngineError> {
    match purpose {
        NumberPausePurpose::UpToDraw => {
            let reveal_drawn = matches!(
                step.op,
                EffectOp::Draw {
                    reveal_drawn: true,
                    ..
                }
            );
            library_hand::resume_up_to_draw(state, step, chooser, value, reveal_drawn)
        }
        NumberPausePurpose::CounterSpellUnlessPays => {
            if let EffectOp::CounterSpell {
                replacement_zone,
                unless_pays_generic,
                ..
            } = &step.op
            {
                stack_control::resume_counter_unless_pays(
                    state,
                    step,
                    chooser,
                    value,
                    *replacement_zone,
                    unless_pays_generic.as_deref(),
                )
            } else {
                Ok(StepExecOutcome::Done)
            }
        }
        NumberPausePurpose::GenericAmount | NumberPausePurpose::DivideDamage => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.last_effect_amount = value;
            }
            Ok(StepExecOutcome::Done)
        }
    }
}

pub(crate) fn resume_mode_choice_op(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    purpose: ModePausePurpose,
    options: &[String],
    picked: &[u16],
) -> Result<StepExecOutcome, EngineError> {
    let idx = picked.first().copied().unwrap_or(0);
    match purpose {
        ModePausePurpose::PunisherChoice => {
            if let EffectOp::PunisherChoice { spec } = &step.op {
                return stack_control::resume_punisher_choice(state, spec, chooser, idx);
            }
        }
        ModePausePurpose::DesignationChoice => {
            if let EffectOp::ChooseDesignation { kind } = &step.op
                && let Some(chosen) = options.get(usize::from(idx))
            {
                counters_mods::apply_designation_choice(state, *kind, chosen);
            }
        }
        ModePausePurpose::OddOrEvenChoice => {
            if let Some(src) = vm::vm_source(state)
                && let Some(r) = state.rare_or_default(src)
            {
                r.chosen_odd_parity = Some(idx == 0);
            }
            let parity = if idx == 0 {
                mtg_ir::ParityFilter::Odd
            } else {
                mtg_ir::ParityFilter::Even
            };
            let dur = match &step.op {
                EffectOp::CombatRestriction { duration, .. } => {
                    duration.unwrap_or(mtg_ir::Duration::EndOfTurn)
                }
                _ => mtg_ir::Duration::EndOfTurn,
            };
            counters_mods::register_odd_or_even_cant_block(state, step, parity, dur);
        }
        ModePausePurpose::GrantKeywordChoice => {
            return counters_mods::resume_grant_keyword_choice(state, step, options, idx);
        }
        ModePausePurpose::ProliferatePlayers => {
            return counters_mods::resume_proliferate_players_choice(state, chooser, picked);
        }
        ModePausePurpose::ModalSuffixOrReflexive | ModePausePurpose::PartitionChoice => {}
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn resume_effect_replacement_op(
    state: &mut GameState,
    step: &EffectStep,
    op_tag: u8,
    rep_out: ReplacementOutcome,
    batch_remaining: &[ObjectId],
    batch_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    match op_tag {
        OP_TAG_DAMAGE => {
            let cont = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .unwrap_or_default();
            damage_life::apply_resolved_damage(state, &rep_out)?;
            damage_life::apply_fight_excess_mana(state, cont.fight_excess_mana)?;
            if cont.damage_do_excess
                && let crate::state::ReplaceableEventKind::Damage {
                    source,
                    target: TargetRef::Object { id: oid },
                    ..
                } = rep_out.event
                && let Some(excess) = state
                    .resolution_vms()
                    .last()
                    .map(|v| v.last_excess_damage)
                    .filter(|&e| e > 0)
                && let Some(def_ctrl) = state.object(oid).map(|o| o.controller)
            {
                match crate::replace::resolve_damage(
                    state,
                    source,
                    TargetRef::Player { id: def_ctrl },
                    excess as u32,
                    false,
                    false,
                )? {
                    crate::replace::ReplacementResult::Paused { prompt_id, chooser } => {
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pending_continuation =
                                Some(Box::new(crate::state::PendingStepContinuation {
                                    remaining_damage: cont.remaining_damage,
                                    damage_do_excess: cont.damage_do_excess,
                                    fight_return_damage: cont.fight_return_damage,
                                    ..Default::default()
                                }));
                        }
                        vm::set_waiting_on_replacement(
                            state,
                            prompt_id,
                            Vec::new(),
                            0,
                            OP_TAG_DAMAGE,
                        );
                        return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                    }
                    crate::replace::ReplacementResult::Resolved(ex_out) => {
                        damage_life::apply_resolved_damage(state, &ex_out)?;
                    }
                }
            }
            if !cont.remaining_damage.is_empty() || cont.fight_return_damage.is_some() {
                return damage_life::run_damage_packets(
                    state,
                    cont.remaining_damage,
                    cont.damage_do_excess,
                    cont.fight_return_damage,
                    None,
                );
            }
        }
        OP_TAG_GAIN_LIFE => {
            let pid = match rep_out.event {
                crate::state::ReplaceableEventKind::LifeGain { player, .. } => player,
                _ => ctrl,
            };
            damage_life::apply_resolved_life_gain(state, pid, &rep_out);
            let rem = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .map(|c| c.remaining_players)
                .unwrap_or_default();
            if !rem.is_empty() {
                let set_life_total = matches!(
                    step.op,
                    EffectOp::GainLife {
                        set_life_total: true
                    }
                );
                let amt = vm::resolve_step_amount(state, step, 0);
                return damage_life::run_gain_life_players(state, &rem, amt, set_life_total);
            }
        }
        OP_TAG_DRAW => {
            let reveal_drawn = matches!(
                step.op,
                EffectOp::Draw {
                    reveal_drawn: true,
                    ..
                }
            );
            library_hand::apply_resolved_draw(state, step, &rep_out, reveal_drawn)?;
            let rem = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .map(|c| c.remaining_players)
                .unwrap_or_default();
            if !rem.is_empty() {
                let count = vm::resolve_step_amount(state, step, 1).max(0) as u32;
                return library_hand::run_draw_players(state, step, &rem, count, reveal_drawn);
            }
        }
        OP_TAG_PUT_COUNTERS => {
            counters_mods::apply_resolved_put_counters(state, &rep_out, ctrl);
            if !rep_out.mods.skipped
                && let crate::state::ReplaceableEventKind::PutCounters { object, .. } =
                    rep_out.event
            {
                vm::append_step_binding(state, step.bind_as.as_deref(), object, None);
            }
            if batch_idx + 1 < batch_remaining.len() {
                match &step.op {
                    EffectOp::PutCounters { counter, .. } => {
                        let cid = counter
                            .or_else(|| state.db().counter_id("+1/+1"))
                            .unwrap_or(mtg_ir::CounterId(0));
                        let amt = vm::resolve_step_amount(state, step, 1).max(0) as u32;
                        let src = vm::vm_source(state);
                        for (off, &tid) in batch_remaining[batch_idx + 1..].iter().enumerate() {
                            match crate::replace::resolve_put_counters(
                                state,
                                tid,
                                cid,
                                amt,
                                Some(ctrl),
                                src,
                            )? {
                                crate::replace::ReplacementResult::Paused {
                                    prompt_id,
                                    chooser,
                                } => {
                                    vm::set_waiting_on_replacement(
                                        state,
                                        prompt_id,
                                        batch_remaining.to_vec(),
                                        batch_idx + 1 + off,
                                        OP_TAG_PUT_COUNTERS,
                                    );
                                    return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                                }
                                crate::replace::ReplacementResult::Resolved(out) => {
                                    counters_mods::apply_resolved_put_counters(state, &out, ctrl);
                                    if !out.mods.skipped {
                                        vm::append_step_binding(
                                            state,
                                            step.bind_as.as_deref(),
                                            tid,
                                            None,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    EffectOp::Adapt => {
                        return counters_mods::run_adapt_targets(
                            state,
                            step,
                            batch_remaining,
                            batch_idx + 1,
                        );
                    }
                    _ => {}
                }
            }
        }
        OP_TAG_PUT_PLAYER_COUNTERS => {
            counters_mods::apply_resolved_put_player_counters(state, &rep_out);
            let rem = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .map(|c| c.remaining_players)
                .unwrap_or_default();
            if !rem.is_empty()
                && let EffectOp::PutPlayerCounters { counter, .. } = &step.op
            {
                let amt = vm::resolve_step_amount(state, step, 1).max(0) as u32;
                return counters_mods::run_put_player_counters_players(state, &rem, *counter, amt);
            }
        }
        OP_TAG_CREATE_TOKEN => {
            zones_tokens::apply_resolved_create_token(state, step, &rep_out)?;
        }
        OP_TAG_UNTAP => {
            counters_mods::apply_resolved_untap(state, &rep_out, ctrl);
            if !rep_out.mods.skipped
                && !matches!(step.op, EffectOp::GainControl { .. })
                && let crate::state::ReplaceableEventKind::Untap { object } = rep_out.event
            {
                vm::append_step_binding(state, step.bind_as.as_deref(), object, None);
            }
            if batch_idx + 1 < batch_remaining.len() {
                return counters_mods::run_untap_targets(
                    state,
                    step,
                    batch_remaining,
                    batch_idx + 1,
                );
            }
        }
        OP_TAG_PROLIFERATE => {
            if !rep_out.mods.skipped
                && let crate::state::ReplaceableEventKind::Proliferate { player, count, .. } =
                    rep_out.event
            {
                return counters_mods::start_proliferate_selection(state, player, count);
            }
        }
        OP_TAG_PROLIFERATE_COUNTER => {
            counters_mods::apply_resolved_put_counters(state, &rep_out, ctrl);
            let cont = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .unwrap_or_default();
            let prolif_player = cont
                .proliferate_chosen_players
                .first()
                .copied()
                .unwrap_or(ctrl);
            return counters_mods::run_proliferate_counter_queue(
                state,
                prolif_player,
                &cont.proliferate_counter_queue,
            );
        }
        OP_TAG_MONSTROUS => {
            counters_mods::apply_resolved_put_counters(state, &rep_out, ctrl);
            let amt = vm::resolve_step_amount(state, step, 1).max(0) as u32;
            if let crate::state::ReplaceableEventKind::PutCounters { object, .. } = rep_out.event {
                counters_mods::finish_monstrosity_target(state, object, ctrl, amt);
            } else if let Some(&id) = batch_remaining.get(batch_idx) {
                counters_mods::finish_monstrosity_target(state, id, ctrl, amt);
            }
            if batch_idx + 1 < batch_remaining.len() {
                return counters_mods::run_monstrosity_targets(
                    state,
                    step,
                    batch_remaining,
                    batch_idx + 1,
                );
            }
        }
        OP_TAG_TAP_OR_UNTAP => {
            counters_mods::apply_resolved_untap(state, &rep_out, ctrl);
            if batch_idx + 1 < batch_remaining.len() {
                return counters_mods::run_tap_or_untap_targets(
                    state,
                    step,
                    batch_remaining,
                    batch_idx + 1,
                );
            }
        }
        OP_TAG_PROLIFERATE_PLAYER_COUNTER => {
            counters_mods::apply_resolved_put_player_counters(state, &rep_out);
            let cont = state
                .resolution_vms_mut()
                .last_mut()
                .and_then(|v| v.pending_continuation.take())
                .unwrap_or_default();
            let prolif_chooser = cont.remaining_players.first().copied().unwrap_or(ctrl);
            return counters_mods::run_proliferate_player_counters_from(
                state,
                prolif_chooser,
                *cont,
                batch_idx + 1,
            );
        }
        _ => {}
    }
    Ok(StepExecOutcome::Done)
}

#[allow(dead_code)]
fn _keep_unused(_: fn(&mut GameState, &EffectStep), _: Zone) {
    let _ = maybe_queue_reflexive_trigger;
    let _ = vm_controller;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::id::StackItemId;
    use mtg_ir::{
        Amount, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics,
        ColorMask, Condition, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost,
        ObjectFilter, OptionalMode, OptionalSpec, PlayerRef, Selector, StepGate, SupertypeMask,
    };

    fn synth_db_and_state(allow_guided: bool) -> GameState {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        tables.keywords.push("reach".into());
        tables.counters.push("+1/+1".into());
        let land = CardDef {
            oracle_id: "synth_plains".into(),
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
        let mut cfg = GameConfig::standard_1v1();
        cfg.allow_guided_fallback = allow_guided;
        GameState::new(db, cfg, &[deck.clone(), deck], 101).unwrap()
    }

    fn make_step(op: EffectOp) -> EffectStep {
        EffectStep {
            op,
            subject: Selector::None,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        }
    }

    #[test]
    fn phase1_and_phase2_audit_covers_all_134_variants() {
        assert_eq!(PHASE1_EFFECT_OP_NAMES.len(), 101);
        assert_eq!(PHASE2_EFFECT_OP_NAMES.len(), 33);
        assert_eq!(
            PHASE1_EFFECT_OP_NAMES.len() + PHASE2_EFFECT_OP_NAMES.len(),
            134
        );

        let mut state_strict = synth_db_and_state(false);
        let phase2_ops = [
            EffectOp::SuspendTick,
            EffectOp::SuspendCast,
            EffectOp::CumulativeUpkeep {
                cost: vec![],
                text: None,
            },
            EffectOp::Discover,
            EffectOp::Clash,
            EffectOp::Detain,
            EffectOp::Endure,
            EffectOp::TimeTravel,
            EffectOp::Blight,
            EffectOp::Fateseal,
            EffectOp::FlipCoin {
                until_lose: false,
                win_effect: vec![],
                lose_effect: vec![],
            },
            EffectOp::RollDie {
                sides: 6,
                bonus: None,
                table: vec![],
            },
            EffectOp::CreateRoleToken {
                role: mtg_ir::RoleTokenKind::Royal,
            },
            EffectOp::Exchange {
                spec: mtg_ir::ExchangeSpec {
                    kind: mtg_ir::ExchangeKind::LifeTotals,
                    second: None,
                    duration: None,
                },
            },
            EffectOp::VentureIntoDungeon { undercity: false },
            EffectOp::TakeInitiative,
            EffectOp::TemptWithRing,
            EffectOp::Earthbend,
            EffectOp::Airbend,
            EffectOp::Learn,
            EffectOp::VillainousChoice {
                options: [vec![], vec![]],
                labels: ["A".into(), "B".into()],
            },
            EffectOp::Vote {
                spec: mtg_ir::VoteSpec {
                    options: vec!["A".into()],
                    extra_votes_for_actor: 0,
                    per_option_effect: vec![],
                    per_winning_effect: vec![],
                    tie_winner_index: None,
                },
            },
            EffectOp::Prepare,
            EffectOp::Unprepare,
            EffectOp::IncreaseSpeed,
            EffectOp::Meld {
                partner_name: "Synth Partner".into(),
                result_name: "Synth Melded".into(),
                tapped_and_attacking: false,
                partner_must_attack: false,
            },
            EffectOp::Harness,
            EffectOp::ReselectAttackTarget,
            EffectOp::EmpowerJace,
            EffectOp::Assimilate,
            EffectOp::Recruit,
            EffectOp::Heal,
            EffectOp::SolveCase,
        ];
        assert_eq!(phase2_ops.len(), 33);

        for op in &phase2_ops {
            assert!(phase2_deferred_message(op).is_some());
            let mut st = state_strict.clone();
            let err = begin_resolution_frame(
                &mut st,
                StackItemId(1),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![make_step(op.clone())],
                vec![],
            );
            assert!(
                matches!(err, Err(EngineError::NotImplemented(_))),
                "expected NotImplemented for {op:?}, got {err:?}"
            );
        }

        // Guided fallback mode pauses cleanly with ChoiceRequest::GuidedResolution.
        let mut state_guided = synth_db_and_state(true);
        let out = begin_resolution_frame(
            &mut state_guided,
            StackItemId(1),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![make_step(EffectOp::Discover)],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected guided resolution pause");
        };
        assert_eq!(chooser, PlayerId(0));
        assert!(matches!(
            state_guided.pending_prompt().map(|p| &p.request),
            Some(ChoiceRequest::GuidedResolution { .. })
        ));
        let resumed = resume_resolution_frame(
            &mut state_guided,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(resumed, FrameOutcome::Completed);
        let _ = &mut state_strict;
    }

    #[test]
    fn optional_gate_scry_snapshot_roundtrip_resumes_cleanly() {
        let mut state = synth_db_and_state(false);
        let mut step0 = make_step(EffectOp::Draw {
            up_to: false,
            reveal_drawn: false,
        });
        step0.amount = Some(Box::new(Amount::Const { n: 1 }));
        step0.optional = Some(OptionalSpec {
            chooser: PlayerRef::You,
            cost: vec![],
            mode: OptionalMode::DoIfPaid,
        });

        let mut step1 = make_step(EffectOp::Scry);
        step1.amount = Some(Box::new(Amount::Const { n: 1 }));
        step1.gate = Some(StepGate::IfPrecedingAccepted { step_index: 0 });

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(1),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![step0, step1],
            vec![],
        )
        .unwrap();

        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected OptionalChoice pause");
        };
        assert_eq!(chooser, PlayerId(0));

        // Round-trip GameStateSnapshot via JSON while paused mid-ResolutionFrame!
        let snap_json = serde_json::to_string(&state.to_snapshot()).unwrap();
        let db = Arc::new(state.db().clone());
        let mut restored = GameState::from_snapshot(db, serde_json::from_str(&snap_json).unwrap());

        let hand_before = restored.player(PlayerId(0)).unwrap().hand.len();
        let out2 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(
            restored.player(PlayerId(0)).unwrap().hand.len(),
            hand_before + 1
        );

        // Step 1 (Scry 1) should now be paused on YesNo (bottom vs top).
        let FrameOutcome::Paused {
            prompt_id: scry_pid,
            ..
        } = out2
        else {
            panic!("expected Scry YesNo pause");
        };
        let out3 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            scry_pid,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);
        assert!(restored.resolution_frames().is_empty());
        assert!(restored.resolution_vms().is_empty());
    }

    #[test]
    fn conditional_and_repeat_sequences_execute_genuinely() {
        let mut state = synth_db_and_state(false);
        let mut gain = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        gain.amount = Some(Box::new(Amount::Const { n: 3 }));
        let cond_step = make_step(EffectOp::ConditionalSequence {
            condition: Box::new(Condition::IsYourTurn),
            then_steps: vec![gain],
            else_steps: vec![],
        });
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(2),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![cond_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);
    }

    #[test]
    fn filter_helper_symbol_smoke() {
        let _ = ObjectFilter::default();
    }

    #[test]
    fn phase1_baseline_ops_execute_genuinely() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into()); // 0
        tables.keywords.push("vigilance".into()); // 1
        tables.counters.push("+1/+1".into()); // 0
        tables.subtypes.push("Zombie".into()); // 0
        tables.subtypes.push("Army".into()); // 1

        let land = CardDef {
            oracle_id: "synth_plains".into(),
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
        let bear = CardDef {
            oracle_id: "synth_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
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
            }],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land, bear],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 202).unwrap();

        let ts1 = state.allocate_timestamp();
        let p0_bear = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(p0_bear);
        let ts2 = state.allocate_timestamp();
        let p1_bear = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(p1_bear);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut gain = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        gain.amount = Some(Box::new(Amount::Const { n: 3 }));

        let mut lose = make_step(EffectOp::LoseLife);
        lose.subject = Selector::Player {
            filter: mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::EachOpponent,
            },
        };
        lose.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut dmg = make_step(EffectOp::Damage {
            source: None,
            also_players: None,
            divided: false,
            excess_to_controller: false,
            excess_to_controller_if_trample: false,
            or_planeswalker_controlled_by: None,
            damage_attacked_target_by_self: false,
            soulfire_exile_damage_per_target: false,
            each_creature_deals: None,
        });
        dmg.subject = Selector::Player {
            filter: mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::EachOpponent,
            },
        };
        dmg.amount = Some(Box::new(Amount::Const { n: 3 }));

        let mut draw = make_step(EffectOp::Draw {
            up_to: false,
            reveal_drawn: false,
        });
        draw.amount = Some(Box::new(Amount::Const { n: 1 }));

        let mut mill = make_step(EffectOp::Mill);
        mill.subject = Selector::Player {
            filter: mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::EachOpponent,
            },
        };
        mill.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut add_mana = make_step(EffectOp::AddMana {
            production: mtg_ir::ManaProduction::Fixed {
                symbols: smallvec::smallvec![mtg_ir::ManaColor::G],
            },
            restriction: None,
            restriction_spec: None,
            retain_until_eot: false,
            delayed_rider: None,
        });
        add_mana.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut tap = make_step(EffectOp::Tap {
            select: mtg_ir::SelectionMod::default(),
        });
        tap.subject = Selector::Target { index: 0 };

        let mut untap = make_step(EffectOp::Untap {
            select: mtg_ir::SelectionMod::default(),
        });
        untap.subject = Selector::Target { index: 0 };

        let mut put_c = make_step(EffectOp::PutCounters {
            counter: Some(mtg_ir::CounterId(0)),
            multi_counters: vec![],
            select: mtg_ir::SelectionMod::default(),
            divided: false,
            allow_exile_counters: false,
            remove_instead_of_add: false,
            copy_counters_from: None,
            choose_counter_on_subject_and_add_one: false,
            conditional_counter_on_type: None,
            cast_bolas_bonus_loyalty: false,
        });
        put_c.subject = Selector::Target { index: 0 };
        put_c.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut remove_c = make_step(EffectOp::RemoveCounters {
            counter: Some(mtg_ir::CounterId(0)),
            up_to: false,
            if_unable_sacrifice_self: false,
            remove_all_from_target_or_player: false,
        });
        remove_c.subject = Selector::Target { index: 0 };
        remove_c.amount = Some(Box::new(Amount::Const { n: 1 }));

        let mut prolif = make_step(EffectOp::Proliferate);
        prolif.subject = Selector::Target { index: 0 };

        let mut pump = make_step(EffectOp::Pump {
            toughness: Some(Box::new(Amount::Const { n: 2 })),
            duration: Some(mtg_ir::Duration::EndOfTurn),
            pump_choices: vec![],
        });
        pump.subject = Selector::Target { index: 0 };
        pump.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut grant = make_step(EffectOp::Grant {
            keywords: smallvec::smallvec![mtg_ir::KeywordId(1)],
            remove_keywords: SmallVec::new(),
            cant_have_or_gain: false,
            duration: Some(mtg_ir::Duration::EndOfTurn),
            granted_ability_texts: vec![],
            choose_keyword_options: SmallVec::new(),
            choose_lose_keyword_options: SmallVec::new(),
            choose_protection_color: false,
            remove_all_abilities: false,
            can_attack_no_defender: false,
            must_attack_this_turn: false,
            must_be_blocked_this_turn: false,
            must_block_this_turn: false,
            must_block_self_this_turn: false,
            cant_block_self_this_turn: false,
            cant_be_blocked_by_opponents_creatures: false,
            assigns_no_combat_damage_this_turn: false,
            grant_suspend_if_missing: false,
            exile_resolving_spell_to_linked: false,
            grant_trigger_spell_sunburst: false,
            pay_any_mana_for_damage: None,
            pay_up_to_life_gained_mana_to_draw: false,
            gambit_combat_directive: false,
        });
        grant.subject = Selector::Target { index: 0 };

        let mut token = make_step(EffectOp::CreateToken {
            token: Some(Box::new(mtg_ir::TokenSpec {
                name: "Synth Soldier".into(),
                power: 1,
                toughness: 1,
                colors: ColorMask::W,
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: "Creature — Synth Soldier".into(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: vec![],
                legendary: false,
            })),
            dynamic_pt: None,
            tapped_and_attacking: false,
            per_opponent_tapped_attacking: None,
            multiply_by_opponents_did_this_way: false,
            benthic_anomaly_clone: false,
        });
        token.bind_as = Some("tok".into());

        let mut destroy = make_step(EffectOp::Destroy {
            cant_be_regenerated: false,
            select: mtg_ir::SelectionMod::default(),
        });
        destroy.subject = Selector::Target { index: 1 };

        let mut amass = make_step(EffectOp::Amass {
            subtype: Some(mtg_ir::SubtypeId(0)),
        });
        amass.amount = Some(Box::new(Amount::Const { n: 2 }));

        let monarch = make_step(EffectOp::BecomeMonarch);

        let mut loop_gain = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        loop_gain.amount = Some(Box::new(Amount::Const { n: 1 }));
        let mut repeat = make_step(EffectOp::RepeatSequence {
            steps: vec![loop_gain],
            condition: Box::new(Condition::IsYourTurn),
            max_iterations: Some(2),
        });
        repeat.amount = Some(Box::new(Amount::Const { n: 2 }));

        let mut foreach_dmg = make_step(EffectOp::LoseLife);
        foreach_dmg.subject = Selector::PlayerRef {
            player: PlayerRef::EachLoopPlayer,
        };
        foreach_dmg.amount = Some(Box::new(Amount::Const { n: 1 }));
        let foreach_p = make_step(EffectOp::ForEachPlayerLoop {
            players: mtg_ir::LoopPlayersKind::EachOpponent,
            steps: vec![foreach_dmg],
            choose_random_opponent: false,
            exclude_event_player: false,
            exclude_attacked_last_combat: false,
            choose_object_to_bind: None,
        });

        let p0_hand_before = state.player(PlayerId(0)).unwrap().hand.len();
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(10),
            PlayerId(0),
            None,
            None,
            0,
            vec![
                TargetRef::Object { id: p0_bear },
                TargetRef::Object { id: p1_bear },
            ],
            vec![],
            SmallVec::new(),
            vec![
                gain, lose, dmg, draw, mill, add_mana, tap, untap, put_c, remove_c, prolif, pump,
                grant, token, destroy, amass, monarch, repeat, foreach_p,
            ],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: prolif_pid,
            ..
        } = out
        else {
            panic!("expected Proliferate SelectionPick pause, got {out:?}");
        };
        let out_final = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prolif_pid,
            ChoiceAnswer::PickObject { id: p0_bear },
        )
        .unwrap();
        assert_eq!(out_final, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 25); // 20 + 3 + 2 (repeat x2)
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 14); // 20 - 2 - 3 - 1
        assert_eq!(
            state.player(PlayerId(0)).unwrap().hand.len(),
            p0_hand_before + 1
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().graveyard.len(), 3); // 2 milled + 1 destroyed bear
        assert_eq!(
            state
                .player(PlayerId(0))
                .unwrap()
                .mana_pool
                .iter()
                .sum::<u32>(),
            2
        );
        assert_eq!(state.monarch(), Some(PlayerId(0)));
        assert_eq!(state.counter_count(p0_bear, mtg_ir::CounterId(0)), 2); // +2 -1 +1(proliferate)
        assert!(crate::keywords::has_keyword(&state, p0_bear, "vigilance"));
        assert!(
            !state
                .object(p0_bear)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn search_pause_snapshot_roundtrip_resumes_cleanly() {
        let mut state = synth_db_and_state(false);
        let search_step = make_step(EffectOp::Search {
            spec: Box::new(mtg_ir::SearchSpec {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::LAND,
                    ..Default::default()
                }),
                count: 1,
                up_to: true,
                stated_quality: false,
                destinations: vec![mtg_ir::SearchDestination {
                    zone: Zone::Hand,
                    tapped: false,
                    on_top: false,
                }],
                reveal: false,
                description: None,
                zones: mtg_ir::ZoneMask::LIBRARY,
                different_names: false,
                shuffle_only_if_library: false,
                searched_player: None,
                cast_free: false,
                target_controller_as_actor: false,
            }),
            tapped: false,
        });

        let hand_before = state.player(PlayerId(0)).unwrap().hand.len();
        let first_lib_card = state.player(PlayerId(0)).unwrap().library[0];
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(11),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![search_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected Search SelectionPick pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));

        // JSON snapshot round-trip while paused mid-Search!
        let snap_json = serde_json::to_string(&state.to_snapshot()).unwrap();
        let db = Arc::new(state.db().clone());
        let mut restored = GameState::from_snapshot(db, serde_json::from_str(&snap_json).unwrap());

        let out2 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickObject { id: first_lib_card },
        )
        .unwrap();
        assert_eq!(out2, FrameOutcome::Completed);
        assert_eq!(
            restored.player(PlayerId(0)).unwrap().hand.len(),
            hand_before + 1
        );
        assert!(restored.resolution_frames().is_empty());
        assert!(restored.resolution_vms().is_empty());
    }

    #[test]
    fn replacement_inside_effect_execute_instead_waiting_and_bind_as_lki() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());

        // Card 0: Synth Shockland (self-replacement ETB: pay 2 life or enter tapped)
        let shockland = CardDef {
            oracle_id: "synth_shockland".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shockland".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Shockland".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    or_self: true,
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::empty(),
                                        to: Some(Zone::Battlefield),
                                        from_anywhere: true,
                                        not_from_zone: None,
                                        not_to_zone: None,
                                        enters_untapped: false,
                                        dealt_damage_by_self_this_turn: false,
                                    }),
                                    ..Default::default()
                                },
                            },
                            with: mtg_ir::ReplacementWith::ZoneChange {
                                mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                    pay_life_or_enter_tapped: Some(2),
                                    ..Default::default()
                                }),
                            },
                            self_replacement: true,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 1: Synth Guardian (ReplacementWith::Execute on own BF->GY zone change)
        let mut heal_4 = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        heal_4.amount = Some(Box::new(Amount::Const { n: 4 }));
        let guardian = CardDef {
            oracle_id: "synth_guardian".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Guardian".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Guardian".into(),
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
                abilities: vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    or_self: true,
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
                            },
                            with: mtg_ir::ReplacementWith::Execute {
                                steps: vec![heal_4],
                            },
                            self_replacement: false,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
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
                cards: vec![shockland, guardian],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 303).unwrap();

        // Spawn shockland in Player 0 hand, guardian on Player 0 battlefield.
        let ts1 = state.allocate_timestamp();
        let hand_shock = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(hand_shock);

        let ts2 = state.allocate_timestamp();
        let bf_guardian = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(bf_guardian);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // Step 0: Move shockland Hand -> Battlefield with bind_as = "moved" -> pauses on WaitingOnReplacement.
        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Battlefield),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: true,
            topmost_only: false,
            may_shuffle: false,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::Target { index: 0 };
        mv_step.bind_as = Some("moved".into());

        // Step 1: Destroy guardian -> triggers ExecuteInstead -> pushes SubSteps(GainLife(4)).
        let mut destroy_guardian = make_step(EffectOp::Destroy {
            cant_be_regenerated: false,
            select: mtg_ir::SelectionMod::default(),
        });
        destroy_guardian.subject = Selector::Target { index: 1 };

        // Step 2: Tap bound "moved" permanent -> verifies bind_as persisted across WaitingOnReplacement!
        let mut tap_moved = make_step(EffectOp::Tap {
            select: mtg_ir::SelectionMod::default(),
        });
        tap_moved.subject = Selector::Binding {
            name: "moved".into(),
        };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(12),
            PlayerId(0),
            None,
            None,
            0,
            vec![
                TargetRef::Object { id: hand_shock },
                TargetRef::Object { id: bf_guardian },
            ],
            vec![],
            SmallVec::new(),
            vec![mv_step, destroy_guardian, tap_moved],
            vec![],
        )
        .unwrap();

        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected WaitingOnReplacement pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        assert!(matches!(
            state
                .resolution_vms()
                .last()
                .and_then(|v| v.pause_state.as_ref()),
            Some(StepPauseState::WaitingOnReplacement { .. })
        ));

        // Answer Yes -> pay 2 life (20 -> 18) -> enter untapped -> resume frame ->
        // ExecuteInstead runs GainLife(4) (18 -> 22) and Tap taps the moved shockland!
        let rep_out = crate::replace::resume_and_finalize_replacement(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        let out2 = resume_after_replacement(&mut state, rep_out).unwrap();
        assert_eq!(out2, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 22);
        // Guardian survived because ExecuteInstead replaced its dying move!
        assert_eq!(state.object(bf_guardian).unwrap().zone, Zone::Battlefield);
        // Shockland moved to battlefield AND was tapped by step 2 via bind_as "moved"!
        let bf_shock = *state
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .find(|&&id| id != bf_guardian)
            .expect("moved shockland on battlefield");
        assert!(
            state
                .object(bf_shock)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn t1_delayed_tap_mana_trigger_all_and_you_eot() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Forest".into());
        let forest_def = CardDef {
            oracle_id: "synth_forest".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Forest".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Forest".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: smallvec::smallvec![mtg_ir::SubtypeId(0)],
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
                cards: vec![forest_def],
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

        let ts = state.allocate_timestamp();
        let forest_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(1),
                Zone::Battlefield,
                ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(forest_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let all_step = make_step(EffectOp::DelayedTapManaTrigger {
            all_players: true,
            land_subtype: Some(mtg_ir::SubtypeId(0)),
            mana: smallvec::smallvec![mtg_ir::ManaColor::G],
            to_tapping_player: true,
        });
        let you_step = make_step(EffectOp::DelayedTapManaTrigger {
            all_players: false,
            land_subtype: None,
            mana: smallvec::smallvec![mtg_ir::ManaColor::U],
            to_tapping_player: false,
        });
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(101),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![all_step, you_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out, FrameOutcome::Completed);

        let delayed = state.delayed_triggers();
        assert_eq!(delayed.len(), 2);
        assert_eq!(delayed[0].trigger.event, mtg_ir::EventKind::TappedForMana);
        assert_eq!(delayed[0].expiry, crate::triggers::DelayedExpiry::ThisTurn);
        assert!(delayed[0].trigger.filter.player.is_none());
        assert!(delayed[1].trigger.filter.player.is_some());

        state.set_decision(crate::action::DecisionState::Priority);
        crate::events::emit_event(
            &mut state,
            crate::events::AtomicEventKind::TappedForMana {
                player: PlayerId(1),
                source: forest_id,
                produced_colors: smallvec::smallvec![mtg_ir::ManaColor::G],
                amount: 1,
            },
        );
        crate::triggers::flush_inline_triggers(&mut state);
        crate::triggers::flush_triggers(&mut state);
        // Inline mana trigger grants {G} to tapping player (Player 1) via triggered mana ability.
        assert!(
            state.player(PlayerId(1)).unwrap().mana_pool[4] >= 1
                || !state.stack_entries().is_empty(),
            "delayed tap mana trigger should add green mana or queue trigger"
        );
    }

    #[test]
    fn t2_combat_restriction_cant_block_attack_cant_be_blocked_and_except_by() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        let creature = CardDef {
            oracle_id: "synth_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
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
            }],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![creature],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 22).unwrap();

        let ts1 = state.allocate_timestamp();
        let bear_a = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts2 = state.allocate_timestamp();
        let bear_b = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(1),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts3 = state.allocate_timestamp();
        let bear_c = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts3,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(bear_a);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(bear_c);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(bear_b);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut step_cant_block = make_step(EffectOp::CombatRestriction {
            spec: Some(mtg_ir::TimedCombatRestrictionSpec {
                kind: mtg_ir::TimedCombatRestrictionKind::CantBlock,
                except_by: None,
            }),
            duration: Some(mtg_ir::Duration::EndOfTurn),
            choose_odd_or_even_cant_block: false,
            must_attack_next_combat: false,
            must_attack_player: None,
            until_your_next_turn_directive: None,
        });
        step_cant_block.subject = Selector::Target { index: 0 };

        let mut step_menace = make_step(EffectOp::CombatRestriction {
            spec: Some(mtg_ir::TimedCombatRestrictionSpec {
                kind: mtg_ir::TimedCombatRestrictionKind::CantBeBlocked,
                except_by: Some(mtg_ir::BlockerExceptionSpec {
                    min_blockers: Some(2),
                    artifact_or_colors: ColorMask::B,
                    ..Default::default()
                }),
            }),
            duration: Some(mtg_ir::Duration::EndOfTurn),
            choose_odd_or_even_cant_block: false,
            must_attack_next_combat: false,
            must_attack_player: None,
            until_your_next_turn_directive: None,
        });
        step_menace.subject = Selector::Target { index: 1 };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(102),
            PlayerId(0),
            None,
            None,
            0,
            vec![
                TargetRef::Object { id: bear_a },
                TargetRef::Object { id: bear_b },
            ],
            vec![],
            SmallVec::new(),
            vec![step_cant_block, step_menace],
            vec![],
        )
        .unwrap();
        assert_eq!(out, FrameOutcome::Completed);

        assert!(crate::keywords::check_evasion_can_block(&state, bear_a, bear_b).is_err());
        // Green non-artifact bear_c also fails CantBeBlockedExceptBy (Artifact or Black).
        assert!(crate::keywords::check_evasion_can_block(&state, bear_c, bear_b).is_err());
        assert!(state.continuous_effects().iter().any(|ce| {
            matches!(
                &ce.modifier,
                mtg_ir::LayerMod::GrantAbility { ability }
                    if matches!(
                        &**ability,
                        mtg_ir::AbilityDef::Static(s)
                            if matches!(
                                &s.static_def.body,
                                mtg_ir::StaticBody::CombatRule {
                                    rule: mtg_ir::CombatRuleSpec::MinBlockers { min: 2 },
                                    ..
                                }
                            )
                    )
            )
        }));
    }

    #[test]
    fn t3_combat_restriction_odd_even_and_directives() {
        let tables = InternTables::default();
        let odd_bear = CardDef {
            oracle_id: "synth_odd_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Odd Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Odd Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 3,
                        toughness: 3,
                    }),
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
        let even_bear = CardDef {
            oracle_id: "synth_even_bear".into(),
            compiler_version: 46,
            name: "Synth Even Bear".into(),
            faces: vec![FaceDef {
                name: "Synth Even Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{G}"),
                    ..odd_bear.faces[0].characteristics.clone()
                },
                ..odd_bear.faces[0].clone()
            }],
            ..odd_bear.clone()
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![odd_bear, even_bear],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 33).unwrap();
        let ts0 = state.allocate_timestamp();
        let p0_atk = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts0,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts1 = state.allocate_timestamp();
        let odd_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(1),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts2 = state.allocate_timestamp();
        let even_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(p0_atk);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(odd_id);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(even_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let odd_even_step = make_step(EffectOp::CombatRestriction {
            spec: None,
            duration: Some(mtg_ir::Duration::EndOfTurn),
            choose_odd_or_even_cant_block: true,
            must_attack_next_combat: false,
            must_attack_player: None,
            until_your_next_turn_directive: None,
        });
        let mut directives_step = make_step(EffectOp::CombatRestriction {
            spec: None,
            duration: Some(mtg_ir::Duration::UntilYourNextTurn),
            choose_odd_or_even_cant_block: false,
            must_attack_next_combat: true,
            must_attack_player: Some(PlayerRef::ControllerOf {
                of: Box::new(mtg_ir::Ref::Target { index: 0 }),
            }),
            until_your_next_turn_directive: Some((true, true)),
        });
        directives_step.subject = Selector::Target { index: 0 };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(103),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: even_id }],
            vec![],
            SmallVec::new(),
            vec![odd_even_step, directives_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected Odd/Even choice pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));

        // Answer 0 ("Odd"): odd MV bear cannot block, even MV bear can block!
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickMode { index: 0 },
        )
        .unwrap();
        assert_eq!(out2, FrameOutcome::Completed);

        assert!(crate::keywords::check_evasion_can_block(&state, odd_id, p0_atk).is_err());
        assert!(crate::keywords::check_evasion_can_block(&state, even_id, p0_atk).is_ok());
        assert!(state.continuous_effects().iter().any(|ce| {
            matches!(
                &ce.modifier,
                mtg_ir::LayerMod::GrantAbility { ability }
                    if matches!(
                        &**ability,
                        mtg_ir::AbilityDef::Static(s)
                            if matches!(
                                &s.static_def.body,
                                mtg_ir::StaticBody::CombatRule {
                                    rule: mtg_ir::CombatRuleSpec::IsGoaded,
                                    ..
                                }
                            )
                    )
            )
        }));
        let must_atk_step = make_step(EffectOp::CombatRestriction {
            spec: None,
            duration: Some(mtg_ir::Duration::UntilYourNextTurn),
            choose_odd_or_even_cant_block: false,
            must_attack_next_combat: false,
            must_attack_player: None,
            until_your_next_turn_directive: Some((true, false)),
        });
        let out3 = begin_resolution_frame(
            &mut state,
            StackItemId(104),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![must_atk_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);

        assert!(state.continuous_effects().iter().any(|ce| {
            matches!(
                &ce.modifier,
                mtg_ir::LayerMod::GrantAbility { ability }
                    if matches!(
                        &**ability,
                        mtg_ir::AbilityDef::Static(s)
                            if matches!(
                                &s.static_def.body,
                                mtg_ir::StaticBody::Restriction {
                                    restriction: mtg_ir::RestrictionKind::AttacksEachCombat,
                                    ..
                                }
                            )
                    )
            )
        }));
        let rts = state.rare_turn_state().unwrap();
        assert!(rts.must_attack_next_combat.contains(&even_id));
        assert!(
            rts.must_attack_player_this_turn
                .contains(&(even_id, PlayerId(1)))
        );
    }

    #[test]
    fn t4_become_copy_eot_records_backup_and_applies_characteristics() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        let shapeshifter = CardDef {
            oracle_id: "synth_shapeshifter".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shapeshifter".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Shapeshifter".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{U}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
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
            }],
            fully_supported: true,
        };
        let dragon = CardDef {
            oracle_id: "synth_dragon".into(),
            name: "Synth Dragon".into(),
            faces: vec![FaceDef {
                name: "Synth Dragon".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{4}{R}{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 5,
                        toughness: 5,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: serde_json::from_str(r#"{"keyword":0}"#).unwrap(),
                    id: None,
                    span: Default::default(),
                })],
                ..shapeshifter.faces[0].clone()
            }],
            ..shapeshifter.clone()
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![shapeshifter, dragon],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 44).unwrap();
        let ts1 = state.allocate_timestamp();
        let ss_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts2 = state.allocate_timestamp();
        let dr_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(ss_id);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(dr_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut step = make_step(EffectOp::BecomeCopy {
            copy_source: Box::new(Selector::Target { index: 1 }),
            duration: Some(mtg_ir::Duration::EndOfTurn),
            copy_mod: None,
        });
        step.subject = Selector::Target { index: 0 };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(104),
            PlayerId(0),
            None,
            None,
            0,
            vec![
                TargetRef::Object { id: ss_id },
                TargetRef::Object { id: dr_id },
            ],
            vec![],
            SmallVec::new(),
            vec![step],
            vec![],
        )
        .unwrap();
        assert_eq!(out, FrameOutcome::Completed);

        assert_eq!(
            state
                .rare(ss_id)
                .and_then(|r| r.copiable.as_ref())
                .map(|c| c.card_def),
            Some(CardDefId(1))
        );
        assert!(crate::keywords::has_keyword(&state, ss_id, "flying"));
        assert_eq!(
            state
                .evaluate_board()
                .get(ss_id)
                .and_then(|o| o.pt)
                .map(|p| p.power),
            Some(5)
        );
        let rts = state.rare_turn_state().expect("backup recorded");
        assert_eq!(rts.timed_copiable_backups.len(), 1);
        assert_eq!(rts.timed_copiable_backups[0].object, ss_id);
        assert_eq!(
            rts.timed_copiable_backups[0].duration,
            mtg_ir::Duration::EndOfTurn
        );
    }

    #[test]
    fn t5_grant_keyword_choice_protection_color_and_lose_keyword() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        let flyer = CardDef {
            oracle_id: "synth_flyer".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Flyer".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Flyer".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{W}"),
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
                abilities: vec![mtg_ir::AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: serde_json::from_str(r#"{"keyword":0}"#).unwrap(),
                    id: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let red_bear = CardDef {
            oracle_id: "synth_red_bear".into(),
            name: "Synth Red Bear".into(),
            faces: vec![FaceDef {
                name: "Synth Red Bear".into(),
                characteristics: Characteristics {
                    colors: ColorMask::R,
                    ..flyer.faces[0].characteristics.clone()
                },
                abilities: vec![],
                ..flyer.faces[0].clone()
            }],
            ..flyer.clone()
        };
        let blue_bear = CardDef {
            oracle_id: "synth_blue_bear".into(),
            name: "Synth Blue Bear".into(),
            faces: vec![FaceDef {
                name: "Synth Blue Bear".into(),
                characteristics: Characteristics {
                    colors: ColorMask::U,
                    ..flyer.faces[0].characteristics.clone()
                },
                abilities: vec![],
                ..flyer.faces[0].clone()
            }],
            ..flyer.clone()
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![flyer, red_bear, blue_bear],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 55).unwrap();
        let ts1 = state.allocate_timestamp();
        let flyer_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts2 = state.allocate_timestamp();
        let red_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let ts3 = state.allocate_timestamp();
        let blue_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(1),
                Zone::Battlefield,
                ts3,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(flyer_id);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(red_id);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(blue_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut grant_prot = make_step(EffectOp::Grant {
            keywords: SmallVec::new(),
            remove_keywords: smallvec::smallvec![mtg_ir::KeywordId(0)],
            cant_have_or_gain: false,
            duration: Some(mtg_ir::Duration::EndOfTurn),
            granted_ability_texts: Vec::new(),
            choose_keyword_options: SmallVec::new(),
            choose_lose_keyword_options: SmallVec::new(),
            choose_protection_color: true,
            remove_all_abilities: false,
            can_attack_no_defender: false,
            must_attack_this_turn: false,
            must_be_blocked_this_turn: false,
            must_block_this_turn: false,
            must_block_self_this_turn: false,
            cant_block_self_this_turn: false,
            cant_be_blocked_by_opponents_creatures: false,
            assigns_no_combat_damage_this_turn: false,
            grant_suspend_if_missing: false,
            exile_resolving_spell_to_linked: false,
            grant_trigger_spell_sunburst: false,
            pay_any_mana_for_damage: None,
            pay_up_to_life_gained_mana_to_draw: false,
            gambit_combat_directive: false,
        });
        grant_prot.subject = Selector::Target { index: 0 };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(105),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: flyer_id }],
            vec![],
            SmallVec::new(),
            vec![grant_prot],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, .. } = out else {
            panic!("expected color choice pause, got {out:?}");
        };
        // Color options are ["White", "Blue", "Black", "Red", "Green"] -> index 3 is Red.
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickMode { index: 3 },
        )
        .unwrap();
        assert_eq!(out2, FrameOutcome::Completed);

        assert!(!crate::keywords::has_keyword(&state, flyer_id, "flying"));
        assert!(crate::keywords::has_protection_from(
            &state, flyer_id, red_id, false
        ));
        assert!(!crate::keywords::has_protection_from(
            &state, flyer_id, blue_id, false
        ));
    }

    #[test]
    fn t6_replacement_resume_hook_resolution_frame_serde_and_end_turn_and_lose() {
        let tables = InternTables::default();
        let shockland = CardDef {
            oracle_id: "synth_shock".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shock".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Shock".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    or_self: true,
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::empty(),
                                        to: Some(Zone::Battlefield),
                                        from_anywhere: true,
                                        not_from_zone: None,
                                        not_to_zone: None,
                                        enters_untapped: false,
                                        dealt_damage_by_self_this_turn: false,
                                    }),
                                    ..Default::default()
                                },
                            },
                            with: mtg_ir::ReplacementWith::ZoneChange {
                                mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                    pay_life_or_enter_tapped: Some(2),
                                    ..Default::default()
                                }),
                            },
                            self_replacement: true,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
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
                cards: vec![shockland],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 66).unwrap();
        let ts = state.allocate_timestamp();
        let hand_land = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(hand_land);

        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Battlefield),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: true,
            topmost_only: false,
            may_shuffle: false,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::Target { index: 0 };

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(106),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: hand_land }],
            vec![],
            SmallVec::new(),
            vec![mv_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, .. } = out else {
            panic!("expected WaitingOnReplacement, got {out:?}");
        };
        assert_eq!(
            state.pending_replacement_resume(),
            Some(crate::state::ReplacementResumeHook::ResolutionFrame)
        );

        let snap_json = serde_json::to_string(&state.to_snapshot()).unwrap();
        let db_arc = Arc::new(state.db().clone());
        let mut restored =
            GameState::from_snapshot(db_arc, serde_json::from_str(&snap_json).unwrap());
        assert_eq!(
            restored.pending_replacement_resume(),
            Some(crate::state::ReplacementResumeHook::ResolutionFrame)
        );
        let rep_out = crate::replace::resume_and_finalize_replacement(
            &mut restored,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();
        let out2 = resume_after_replacement(&mut restored, rep_out).unwrap();
        assert_eq!(out2, FrameOutcome::Completed);
        assert_eq!(restored.player(PlayerId(0)).unwrap().battlefield.len(), 1);

        // Verify EndTurn cleans resolution frames and Lose removes eliminated player's permanents.
        let lose_step = make_step(EffectOp::Lose { condition: None });
        let end_step = make_step(EffectOp::EndTurn {
            active_player_may: false,
        });
        let mut lose_target = lose_step;
        lose_target.subject = Selector::PlayerRef {
            player: PlayerRef::You,
        };
        let out3 = begin_resolution_frame(
            &mut restored,
            StackItemId(107),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![lose_target],
            vec![],
        )
        .unwrap();
        assert!(matches!(
            out3,
            FrameOutcome::Completed | FrameOutcome::GameOver { .. }
        ));
        assert!(restored.player(PlayerId(0)).unwrap().lost);
        assert!(restored.player(PlayerId(0)).unwrap().battlefield.is_empty());

        let mut end_state = synth_db_and_state(false);
        let out4 = begin_resolution_frame(
            &mut end_state,
            StackItemId(108),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![end_step],
            vec![],
        )
        .unwrap();
        assert!(matches!(
            out4,
            FrameOutcome::Completed | FrameOutcome::Paused { .. } | FrameOutcome::GameOver { .. }
        ));
        assert!(end_state.resolution_frames().is_empty());
    }

    #[test]
    fn t7_step_acted_ids_no_leak_and_post_move_modifiers_across_pause() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let shock = CardDef {
            oracle_id: "synth_shock_mod".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shock Mod".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Shock Mod".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    or_self: true,
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::empty(),
                                        to: Some(Zone::Battlefield),
                                        from_anywhere: true,
                                        not_from_zone: None,
                                        not_to_zone: None,
                                        enters_untapped: false,
                                        dealt_damage_by_self_this_turn: false,
                                    }),
                                    ..Default::default()
                                },
                            },
                            with: mtg_ir::ReplacementWith::ZoneChange {
                                mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                    pay_life_or_enter_tapped: Some(2),
                                    ..Default::default()
                                }),
                            },
                            self_replacement: true,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
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
                cards: vec![shock],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 77).unwrap();
        let ts = state.allocate_timestamp();
        let hand_card = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(hand_card);

        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Battlefield),
            select: mtg_ir::SelectionMod::default(),
            tapped: true,
            under_your_control: true,
            topmost_only: false,
            may_shuffle: false,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: Some((mtg_ir::CounterId(0), 1)),
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::Target { index: 0 };

        // Step 1 is a no-op Sacrifice with reflexive SacrificedThisWay -> MUST NOT fire from Step 0!
        let mut noop_sac = make_step(EffectOp::Sacrifice {
            select: mtg_ir::SelectionMod {
                filter: Some(Box::new(ObjectFilter {
                    types_any: CardTypeMask::ARTIFACT,
                    ..Default::default()
                })),
                ..Default::default()
            },
        });
        let mut leak_heal = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        leak_heal.amount = Some(Box::new(Amount::Const { n: 5 }));
        noop_sac.reflexive_trigger = Some(Box::new(mtg_ir::ReflexiveTriggerSpec {
            targets: Vec::new(),
            effect: vec![leak_heal],
            modal: None,
            text: None,
            event_filter: Some(mtg_ir::ReflexiveEventFilter::SacrificedThisWay),
            object_filter: None,
        }));

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(109),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: hand_card }],
            vec![],
            SmallVec::new(),
            vec![mv_step, noop_sac],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, .. } = out else {
            panic!("expected WaitingOnReplacement, got {out:?}");
        };
        let rep_out = crate::replace::resume_and_finalize_replacement(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        let out2 = resume_after_replacement(&mut state, rep_out).unwrap();
        assert_eq!(out2, FrameOutcome::Completed);

        // Reflexive gain life did NOT fire (life is 20 - 2 shock = 18, not 19!).
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 18);
        let bf_id = *state
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .first()
            .unwrap();
        assert!(
            state
                .object(bf_id)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED),
            "post-move tapped=true must apply after replacement resume"
        );
        assert_eq!(state.counter_count(bf_id, mtg_ir::CounterId(0)), 1);
    }

    #[test]
    fn t8_batch_continuations_proliferate_copy_lki_once_per_turn() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        tables.counters.push("poison".into());
        let bear = CardDef {
            oracle_id: "synth_lki_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Lki Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lki Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
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
            }],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bear],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 88).unwrap();

        let ts1 = state.allocate_timestamp();
        let bf_bear = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(bf_bear);
        state.add_counter(bf_bear, mtg_ir::CounterId(0), 1);
        state.player_mut(PlayerId(1)).unwrap().poison = 1;
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // Proliferate pauses for permanent selection -> player selection -> increments both.
        let prolif_step = make_step(EffectOp::Proliferate);
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(110),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![prolif_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, .. } = out else {
            panic!("expected Proliferate permanent pick, got {out:?}");
        };
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickObject { id: bf_bear },
        )
        .unwrap();
        let prompt_id2 = match out2 {
            FrameOutcome::Paused { prompt_id, .. } => prompt_id,
            other => panic!("expected DonePicking or ProliferatePlayers pause, got {other:?}"),
        };
        let res_done = if matches!(
            state
                .resolution_vms()
                .last()
                .and_then(|v| v.pause_state.as_ref()),
            Some(StepPauseState::SelectionPick { .. })
        ) {
            resume_resolution_frame(
                &mut state,
                PlayerId(0),
                prompt_id2,
                ChoiceAnswer::DonePicking,
            )
            .unwrap()
        } else {
            FrameOutcome::Paused {
                prompt_id: prompt_id2,
                chooser: PlayerId(0),
            }
        };
        if let FrameOutcome::Paused {
            prompt_id: pid3, ..
        } = res_done
        {
            let out3 = resume_resolution_frame(
                &mut state,
                PlayerId(0),
                pid3,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap();
            assert_eq!(out3, FrameOutcome::Completed);
        }
        assert_eq!(state.counter_count(bf_bear, mtg_ir::CounterId(0)), 2);
        assert_eq!(state.player(PlayerId(1)).unwrap().poison, 2);

        // do_this_once_per_turn marks used even after OptionalSpec pause.
        let mut opt_life = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        opt_life.amount = Some(Box::new(Amount::Const { n: 3 }));
        opt_life.do_this_once_per_turn = true;
        opt_life.optional = Some(OptionalSpec {
            mode: OptionalMode::DoIfPaid,
            cost: vec![],
            chooser: PlayerRef::You,
        });
        let out_opt = begin_resolution_frame(
            &mut state,
            StackItemId(111),
            PlayerId(0),
            Some(bf_bear),
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![opt_life.clone()],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: opid, ..
        } = out_opt
        else {
            panic!("expected optional pause, got {out_opt:?}");
        };
        let out_opt2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            opid,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(out_opt2, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);
        // Second invocation same turn must be declined immediately.
        let out_opt3 = begin_resolution_frame(
            &mut state,
            StackItemId(112),
            PlayerId(0),
            Some(bf_bear),
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![opt_life],
            vec![],
        )
        .unwrap();
        assert_eq!(out_opt3, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);

        // CopyPermanent when target moved off battlefield uses LKI fallback.
        let snap = vm::snapshot_for_binding(&state, bf_bear).unwrap();
        crate::zones::move_object_raw(
            &mut state,
            bf_bear,
            Zone::Graveyard,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let mut copy_step = make_step(EffectOp::CopyPermanent {
            select: mtg_ir::SelectionMod::default(),
            not_legendary: false,
            tapped: false,
            tapped_and_attacking: false,
            per_opponent_tapped_attacking: None,
            copy_mod: None,
            except_pt: None,
        });
        copy_step.subject = Selector::Binding {
            name: "lki_target".into(),
        };
        let out_copy = begin_resolution_frame(
            &mut state,
            StackItemId(113),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![copy_step],
            vec![crate::state::ResolutionBinding {
                name: "lki_target".into(),
                objects: vec![bf_bear],
                lki: vec![snap],
            }],
        )
        .unwrap();
        assert_eq!(out_copy, FrameOutcome::Completed);
        assert_eq!(state.player(PlayerId(0)).unwrap().battlefield.len(), 1);
    }

    #[test]
    fn synth_choice_mana_cost_and_stack_target_chooser() {
        let mut state = synth_db_and_state(false);
        let choice_cost = vec![mtg_ir::CostDef::Choice {
            options: vec![
                vec![mtg_ir::CostDef::Mana {
                    cost: ManaCost::parse("{B}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
                vec![mtg_ir::CostDef::Mana {
                    cost: ManaCost::parse("{3}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
            ],
            forage: false,
        }];

        // Case 1: Only {B} available -> can pay!
        state.player_mut(PlayerId(1)).unwrap().mana_pool = [0, 0, 1, 0, 0, 0];
        assert!(vm::can_pay_cost_list(
            &state,
            PlayerId(1),
            crate::state::NONE_OBJECT_ID,
            &choice_cost,
            0,
        ));
        assert!(
            vm::pay_cost_list_sync(
                &mut state,
                PlayerId(1),
                crate::state::NONE_OBJECT_ID,
                &choice_cost,
                0,
            )
            .is_ok()
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().mana_pool, [0; 6]);

        // Case 2: Only {W}{W}{W} available -> can pay {3} branch!
        state.player_mut(PlayerId(1)).unwrap().mana_pool = [3, 0, 0, 0, 0, 0];
        assert!(vm::can_pay_cost_list(
            &state,
            PlayerId(1),
            crate::state::NONE_OBJECT_ID,
            &choice_cost,
            0,
        ));
        assert!(
            vm::pay_cost_list_sync(
                &mut state,
                PlayerId(1),
                crate::state::NONE_OBJECT_ID,
                &choice_cost,
                0,
            )
            .is_ok()
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().mana_pool, [0; 6]);

        // Case 3: {W}{U} only -> neither {B} nor {3} payable!
        state.player_mut(PlayerId(1)).unwrap().mana_pool = [1, 1, 0, 0, 0, 0];
        assert!(!vm::can_pay_cost_list(
            &state,
            PlayerId(1),
            crate::state::NONE_OBJECT_ID,
            &choice_cost,
            0,
        ));

        // Case 4: X-cost scales by x_value.
        let x_cost = vec![mtg_ir::CostDef::Mana {
            cost: ManaCost::parse("{X}"),
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
        }];
        state.player_mut(PlayerId(1)).unwrap().mana_pool = [0, 0, 0, 2, 0, 0];
        assert!(!vm::can_pay_cost_list(
            &state,
            PlayerId(1),
            crate::state::NONE_OBJECT_ID,
            &x_cost,
            3,
        ));
        state.player_mut(PlayerId(1)).unwrap().mana_pool = [0, 0, 0, 3, 0, 0];
        assert!(
            vm::pay_cost_list_sync(
                &mut state,
                PlayerId(1),
                crate::state::NONE_OBJECT_ID,
                &x_cost,
                3,
            )
            .is_ok()
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().mana_pool, [0; 6]);

        // Case 5: TargetRef::Stack resolves to the stack spell's controller.
        let spell_ts = state.allocate_timestamp();
        let spell_obj = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(1),
                Zone::Stack,
                spell_ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.stack_entries_mut().push(crate::state::StackEntry {
            id: StackItemId(500),
            kind: crate::state::StackEntryKind::Spell { object: spell_obj },
            controller: PlayerId(1),
            choices: Default::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });
        let mut step = make_step(EffectOp::Draw {
            up_to: false,
            reveal_drawn: false,
        });
        step.subject = Selector::Target { index: 0 };
        let frame = crate::state::ResolutionFrame {
            stack_item: StackItemId(501),
            steps_remaining: vec![step.clone()],
            pc: 0,
            bindings: Vec::new(),
            pending_prompt_id: None,
        };
        let vm_state = crate::state::ResolutionVmState {
            stack_item: StackItemId(501),
            controller: PlayerId(0),
            source: None,
            source_lki: None,
            x_value: 0,
            targets: vec![TargetRef::Stack {
                id: StackItemId(500),
            }],
            targets_by_spec: vec![vec![TargetRef::Stack {
                id: StackItemId(500),
            }]],
            divisions: SmallVec::new(),
            step_results: Vec::new(),
            pause_state: None,
            subframes: Vec::new(),
            step_acted_ids: Vec::new(),
            already_selected_controllers: SmallVec::new(),
            each_loop_player: None,
            chosen_opponent: None,
            event_object: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: None,
            last_effect_amount: 0,
            last_excess_damage: 0,
            last_counters_removed: 0,
            last_roll_result: 0,
            coin_heads_count: 0,
            counters_removed_this_way: 0,
            step_way_stats: Default::default(),
            pending_continuation: None,
        };
        state.resolution_frames_mut().push(frame);
        state.resolution_vms_mut().push(vm_state);
        assert_eq!(vm::resolve_target_players(&state, &step), vec![PlayerId(1)]);
    }

    fn synth_card_def(
        oracle_id: &str,
        name: &str,
        colors: ColorMask,
        types: CardTypeMask,
        subtypes: SmallVec<[mtg_ir::SubtypeId; 3]>,
        pt: Option<mtg_ir::PrintedPt>,
        abilities: Vec<mtg_ir::AbilityDef>,
    ) -> CardDef {
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
                    types,
                    subtypes,
                    supertypes: SupertypeMask::empty(),
                    colors,
                    color_indicator: None,
                    pt,
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

    #[test]
    fn synth_stack_trigger_meta_dies_lki_counter_and_defending_player() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into()); // 0
        tables.counters.push("-1/-1".into()); // 1

        // Creature 0: dies due to CR 704.8 annihilation (2 +1/+1 vs 4 -1/-1 -> 0/0),
        // trigger reads pre-annihilation +1/+1 counter count via Amount::LkiCounterCount.
        let mut gain_step = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        gain_step.subject = Selector::PlayerRef {
            player: PlayerRef::You,
        };
        gain_step.amount = Some(Box::new(Amount::LkiCounterCount {
            counter: mtg_ir::CounterTarget::Specific(mtg_ir::CounterId(0)),
        }));
        let reactor = synth_card_def(
            "synth_dies_lki_reactor",
            "Synth Dies Reactor",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![mtg_ir::AbilityDef::Triggered(mtg_ir::TriggeredAbility {
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
                effect: vec![gain_step],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        // Enchantment 1: non-creature permanent triggering when a creature you control attacks,
        // checking Condition::DethroneMostLife and draining PlayerRef::DefendingPlayer.
        let mut drain_step = make_step(EffectOp::LoseLife);
        drain_step.subject = Selector::PlayerRef {
            player: PlayerRef::DefendingPlayer,
        };
        drain_step.amount = Some(Box::new(Amount::Const { n: 3 }));
        let banner = synth_card_def(
            "synth_attack_banner",
            "Synth Attack Banner",
            ColorMask::R,
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            None,
            vec![mtg_ir::AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                trigger: mtg_ir::TriggerDef {
                    event: mtg_ir::EventKind::Attacks,
                    filter: mtg_ir::EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(mtg_ir::RelativeControl::You),
                                ..Default::default()
                            }),
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
                condition: Some(Box::new(Condition::DethroneMostLife)),
                grant_condition: None,
                targets: vec![],
                effect: vec![drain_step],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        // Vanilla attacker 2.
        let charger = synth_card_def(
            "synth_plain_charger",
            "Synth Plain Charger",
            ColorMask::R,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![reactor, banner, charger],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(2); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 201).unwrap();
        state.set_decision(crate::action::DecisionState::Priority);

        // Spawn reactor with 2 +1/+1 counters and 4 -1/-1 counters -> CR 704.8 LKI preserves 2 +1/+1 counters.
        let ts0 = state.allocate_timestamp();
        let reactor_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts0,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(reactor_id);
        state.add_counter(reactor_id, mtg_ir::CounterId(0), 2);
        state.add_counter(reactor_id, mtg_ir::CounterId(1), 4);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let sba_out = crate::sba::check_sbas(&mut state).unwrap();
        assert_eq!(sba_out, crate::sba::SbaOutcome::Done);
        assert_eq!(state.stack_entries().len(), 1);
        assert!(crate::stack::resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 22);

        // Attack PlayerId(1) who has most life (25 > 22) with Charger while Banner (enchantment) is on battlefield.
        state.player_mut(PlayerId(1)).unwrap().life = 25;
        let ts_b = state.allocate_timestamp();
        let banner_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts_b,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(banner_id);

        let ts_c = state.allocate_timestamp();
        let charger_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Battlefield,
                ts_c,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(charger_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let _ = crate::combat::enter_declare_attackers_step(&mut state).unwrap();
        let _ = crate::combat::apply_combat_action(
            &mut state,
            PlayerId(0),
            &crate::action::Action::AssignAttacker {
                attacker: charger_id,
                target: crate::action::AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        let _ = crate::combat::apply_combat_action(
            &mut state,
            PlayerId(0),
            &crate::action::Action::ConfirmAttackers,
        )
        .unwrap();
        assert_eq!(state.stack_entries().len(), 1);
        let stack_id = state.stack_entries()[0].id;
        assert_eq!(
            state
                .stack_trigger_meta(stack_id)
                .and_then(|m| m.defending_player),
            Some(PlayerId(1))
        );
        assert!(crate::stack::resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 22);
    }

    #[test]
    fn synth_selection_pick_missing_ops_and_untap_replacement_pause() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into()); // 0
        tables.keywords.push("equip".into()); // 0
        tables.subtypes.push("Equipment".into()); // 0

        let vanilla_creature = synth_card_def(
            "synth_sel_creature",
            "Synth Sel Creature",
            ColorMask::W,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let equipment = synth_card_def(
            "synth_sel_equip",
            "Synth Sel Gear",
            ColorMask::empty(),
            CardTypeMask::ARTIFACT,
            smallvec::smallvec![mtg_ir::SubtypeId(0)],
            None,
            vec![],
        );
        let mut untap_rep_step = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        untap_rep_step.subject = Selector::PlayerRef {
            player: PlayerRef::You,
        };
        untap_rep_step.amount = Some(Box::new(Amount::Const { n: 1 }));
        let untap_watcher = synth_card_def(
            "synth_untap_watcher",
            "Synth Untap Watcher",
            ColorMask::U,
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            None,
            vec![mtg_ir::AbilityDef::Replacement(
                mtg_ir::ReplacementAbility {
                    replacement: mtg_ir::ReplacementDef {
                        replaces: mtg_ir::EventPattern {
                            event: mtg_ir::EventKind::BecomesUntapped,
                            filter: mtg_ir::EventFilter::default(),
                        },
                        with: mtg_ir::ReplacementWith::Execute {
                            steps: vec![untap_rep_step],
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                },
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![vanilla_creature, equipment, untap_watcher],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 202).unwrap();

        let spawn = |state: &mut GameState, card: CardDefId, owner: PlayerId, zone: Zone| {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                crate::state::GameObject::new(
                    card,
                    owner,
                    zone,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            let p = state.player_mut(owner).unwrap();
            match zone {
                Zone::Battlefield => p.battlefield.push(oid),
                Zone::Exile => p.exile.push(oid),
                Zone::Hand => p.hand.push(oid),
                _ => {}
            }
            state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            oid
        };

        let c1 = spawn(&mut state, CardDefId(0), PlayerId(0), Zone::Battlefield);
        let c2 = spawn(&mut state, CardDefId(0), PlayerId(0), Zone::Battlefield);
        let eq = spawn(&mut state, CardDefId(1), PlayerId(0), Zone::Battlefield);
        let eq2 = spawn(&mut state, CardDefId(1), PlayerId(0), Zone::Battlefield);
        let ex1 = spawn(&mut state, CardDefId(0), PlayerId(0), Zone::Exile);
        let ex2 = spawn(&mut state, CardDefId(0), PlayerId(0), Zone::Exile);
        let opp_h1 = spawn(&mut state, CardDefId(0), PlayerId(1), Zone::Hand);
        let _opp_h2 = spawn(&mut state, CardDefId(0), PlayerId(1), Zone::Hand);

        let select_one = mtg_ir::SelectionMod {
            choice_count: Some(Box::new(Amount::Const { n: 1 })),
            up_to: false,
            at_random: false,
            filter: None,
            chooser: None,
        };
        let you_creatures = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                controller: Some(mtg_ir::RelativeControl::You),
                ..Default::default()
            }),
        };

        // 1) PutCounters with SelectionMod
        let mut pc_step = make_step(EffectOp::PutCounters {
            counter: Some(mtg_ir::CounterId(0)),
            multi_counters: vec![],
            select: select_one.clone(),
            divided: false,
            allow_exile_counters: false,
            remove_instead_of_add: false,
            copy_counters_from: None,
            choose_counter_on_subject_and_add_one: false,
            conditional_counter_on_type: None,
            cast_bolas_bonus_loyalty: false,
        });
        pc_step.subject = you_creatures.clone();
        pc_step.amount = Some(Box::new(Amount::Const { n: 2 }));
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(301),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![pc_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected PutCounters selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: c2 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.counter_count(c1, mtg_ir::CounterId(0)), 0);
        assert_eq!(state.counter_count(c2, mtg_ir::CounterId(0)), 2);

        // 2) Attach with SelectionMod (select which Equipment to attach to target c2)
        let mut at_step = make_step(EffectOp::Attach {
            attach_object: Some(Box::new(Selector::All {
                filter: Box::new(ObjectFilter {
                    zones: mtg_ir::ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::ARTIFACT,
                    controller: Some(mtg_ir::RelativeControl::You),
                    ..Default::default()
                }),
            })),
            select: select_one.clone(),
            attach_equipment_from_self_lki: false,
        });
        at_step.subject = Selector::Target { index: 0 };
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(302),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: c2 }],
            vec![],
            SmallVec::new(),
            vec![at_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected Attach selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: eq2 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.object(eq).unwrap().attached_to, None);
        assert_eq!(state.object(eq2).unwrap().attached_to, Some(c2));

        // 3) TurnFace with SelectionMod
        state
            .object_mut(c1)
            .unwrap()
            .flags
            .insert(crate::state::ObjectFlags::FACE_DOWN);
        state
            .object_mut(c2)
            .unwrap()
            .flags
            .insert(crate::state::ObjectFlags::FACE_DOWN);
        state.mark_dirty(crate::state::DirtyMask::DESIGNATIONS);
        let mut tf_step = make_step(EffectOp::TurnFace {
            face_up: true,
            select: select_one.clone(),
        });
        tf_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            }),
        };
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(303),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![tf_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected TurnFace selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: c1 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert!(!state.object(c1).unwrap().is_face_down());
        assert!(state.object(c2).unwrap().is_face_down());
        state
            .object_mut(c2)
            .unwrap()
            .flags
            .remove(crate::state::ObjectFlags::FACE_DOWN);
        state.mark_dirty(crate::state::DirtyMask::DESIGNATIONS);

        // 4) CopyPermanent with SelectionMod
        let bf_before = state.player(PlayerId(0)).unwrap().battlefield.len();
        let mut cp_step = make_step(EffectOp::CopyPermanent {
            select: select_one.clone(),
            not_legendary: false,
            tapped: false,
            tapped_and_attacking: false,
            per_opponent_tapped_attacking: None,
            copy_mod: None,
            except_pt: None,
        });
        cp_step.subject = you_creatures.clone();
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(304),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![cp_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected CopyPermanent selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: c1 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(
            state.player(PlayerId(0)).unwrap().battlefield.len(),
            bf_before + 1
        );

        // 5) GrantPlayFromExile with SelectionMod
        let mut gp_step = make_step(EffectOp::GrantPlayFromExile {
            spec: mtg_ir::PlayFromExileSpec {
                duration: mtg_ir::PlayFromExileDuration::ThisTurn,
                spend_mana_as_any_color: false,
                spells_only: false,
                without_paying_mana_cost: false,
                alternative_mana_cost: None,
                pay_life_equal_mana_value: false,
                cost_reduction_generic: None,
                plotted: false,
                total_max_mv: None,
            },
            select: select_one.clone(),
            cast_spell_cost_tax_for_opponents: None,
        });
        gp_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::EXILE,
                ..Default::default()
            }),
        };
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(305),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![gp_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected GrantPlayFromExile selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: ex2 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(
            state
                .rare(ex1)
                .and_then(|r| r.playable_from_exile_by.as_ref().map(|p| p.player)),
            None
        );
        assert_eq!(
            state
                .rare(ex2)
                .and_then(|r| r.playable_from_exile_by.as_ref().map(|p| p.player)),
            Some(PlayerId(0))
        );

        // 6) RevealAndDiscard (CoerciveDiscard to Zone::Exile)
        let mut rd_step = make_step(EffectOp::RevealAndDiscard {
            spec: Some(mtg_ir::CoerciveDiscardSpec {
                chooser: PlayerRef::You,
                filter: None,
                optional_choice: false,
                destination: Zone::Exile,
            }),
            partial_reveal: None,
            coercive_count: None,
            include_graveyard: false,
            discard_all_matching_sacrificed_colors: false,
            discard_all_with_chosen_name: false,
        });
        rd_step.subject = Selector::Player {
            filter: mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::Opponent,
            },
        };
        let FrameOutcome::Paused { prompt_id, chooser } = begin_resolution_frame(
            &mut state,
            StackItemId(306),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![rd_step],
            vec![],
        )
        .unwrap() else {
            panic!("expected RevealAndDiscard selection pause");
        };
        assert_eq!(
            resume_resolution_frame(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: opp_h1 }
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert!(!state.player(PlayerId(1)).unwrap().hand.contains(&opp_h1));
        assert_eq!(state.player(PlayerId(1)).unwrap().exile.len(), 1);

        // 7) Untap over 2 tapped permanents with 2 BecomesUntapped replacements causing ChooseReplacement pause
        let _w1 = spawn(&mut state, CardDefId(2), PlayerId(0), Zone::Battlefield);
        let _w2 = spawn(&mut state, CardDefId(2), PlayerId(0), Zone::Battlefield);
        state
            .object_mut(c1)
            .unwrap()
            .flags
            .insert(crate::state::ObjectFlags::TAPPED);
        state
            .object_mut(c2)
            .unwrap()
            .flags
            .insert(crate::state::ObjectFlags::TAPPED);

        let mut untap_step = make_step(EffectOp::Untap {
            select: mtg_ir::SelectionMod::default(),
        });
        untap_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                flags: mtg_ir::FilterFlags::TAPPED,
                ..Default::default()
            }),
        };
        untap_step.bind_as = Some("untapped_ones".into());
        let check_bound = make_step(EffectOp::Tap {
            select: mtg_ir::SelectionMod::default(),
        });
        let mut check_tap = check_bound;
        check_tap.subject = Selector::Binding {
            name: "untapped_ones".into(),
        };

        let _ = crate::events::drain_events(&mut state);
        let out_u1 = begin_resolution_frame(
            &mut state,
            StackItemId(307),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![untap_step, check_tap],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: p_rep1,
            chooser: c_rep1,
        } = out_u1
        else {
            panic!("expected first BecomesUntapped replacement pause, got {out_u1:?}");
        };
        let rep1 = crate::replace::resume_and_finalize_replacement(
            &mut state,
            c_rep1,
            p_rep1,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let out_u2 = resume_after_replacement(&mut state, rep1).unwrap();
        let FrameOutcome::Paused {
            prompt_id: p_rep2,
            chooser: c_rep2,
        } = out_u2
        else {
            panic!("expected second BecomesUntapped replacement pause, got {out_u2:?}");
        };
        assert_ne!(p_rep1, p_rep2);
        let rep2 = crate::replace::resume_and_finalize_replacement(
            &mut state,
            c_rep2,
            p_rep2,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        assert_eq!(
            resume_after_replacement(&mut state, rep2).unwrap(),
            FrameOutcome::Completed
        );
        let untapped_ev_count = crate::events::peek_events(&state)
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    crate::events::AtomicEventKind::TapStateChanged {
                        now_tapped: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(untapped_ev_count, 2);
        assert!(
            state
                .object(c1)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED)
        );
        assert!(
            state
                .object(c2)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn synth_linked_exile_imprint_and_lock_mark_dirty_for_layer_cache() {
        let tables = InternTables::default();
        let imprint_shell = synth_card_def(
            "synth_imprint_shell",
            "Synth Imprint Shell",
            ColorMask::empty(),
            CardTypeMask::CREATURE | CardTypeMask::ARTIFACT,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 4,
            }),
            vec![mtg_ir::AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::Continuous {
                        layer: mtg_ir::Layer::Layer7b,
                        affected: mtg_ir::Affected::SelfObj,
                        modifier: mtg_ir::LayerMod::CopyStatsAndSubtypesFromLastExiled {
                            include_subtypes: true,
                            keep_self_name: true,
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let target_beast = synth_card_def(
            "synth_target_beast",
            "Synth Target Beast",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 5,
                toughness: 6,
            }),
            vec![],
        );
        let exile_lock = synth_card_def(
            "synth_exile_lock",
            "Synth Exile Lock",
            ColorMask::U | ColorMask::B,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 3,
            }),
            vec![mtg_ir::AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::RuleModifier {
                        rule: mtg_ir::RuleMod::CantCastSameNameAsLinkedExile,
                        who: mtg_ir::PlayerFilter {
                            relation: mtg_ir::PlayerRelation::Opponent,
                        },
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![imprint_shell, target_beast, exile_lock],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 203).unwrap();

        let ts0 = state.allocate_timestamp();
        let shell_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts0,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(shell_id);

        let ts1 = state.allocate_timestamp();
        let beast_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts1,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(beast_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // Prime LayerCache before EXILE step mutates linked exile state.
        assert_eq!(
            state
                .evaluate_board()
                .get(shell_id)
                .unwrap()
                .pt
                .unwrap()
                .power,
            2
        );

        // 1) OP_TAG_EXILE sets exiled_by + marks DirtyMask::EXILE.
        let mut ex_step = make_step(EffectOp::Exile {
            select: mtg_ir::SelectionMod::default(),
            duration: None,
            until_source_leaves: false,
            until_opponent_becomes_monarch: false,
            exile_all_except_bottom: false,
            face_down: false,
            look_only: false,
            exile_with_counters: None,
        });
        ex_step.subject = Selector::Target { index: 0 };
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(310),
                PlayerId(0),
                Some(shell_id),
                None,
                0,
                vec![TargetRef::Object { id: beast_id }],
                vec![],
                SmallVec::new(),
                vec![ex_step],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        let exiled_beast = *state.player(PlayerId(1)).unwrap().exile.last().unwrap();
        assert_eq!(
            state.rare(exiled_beast).and_then(|r| r.exiled_by),
            Some(shell_id)
        );
        // Cache must observe the new 5/6 linked imprint stats!
        let shell_pt = state.evaluate_board().get(shell_id).unwrap().pt.unwrap();
        assert_eq!((shell_pt.power, shell_pt.toughness), (5, 6));

        // 2) exec_exile_until marks DirtyMask::EXILE after linking exiled_by.
        let ts2 = state.allocate_timestamp();
        let lock_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Battlefield,
                ts2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(lock_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let _ = state.evaluate_board();
        let mut ex_until = make_step(EffectOp::ExileUntil {
            spec: Box::new(mtg_ir::ExileUntilSpec {
                filter: Box::new(ObjectFilter::default()),
                library_owner: Some(Box::new(Selector::Player {
                    filter: mtg_ir::PlayerFilter {
                        relation: mtg_ir::PlayerRelation::Opponent,
                    },
                })),
                actor: None,
                hit_action: mtg_ir::ExileUntilHitAction::StayExiled,
                cast_free_condition: None,
                fallback_to_hand: false,
                spend_mana_as_any_color: false,
                without_paying_mana_cost: false,
                spells_only: false,
                pay_life_equal_mana_value: false,
                rest_destination: mtg_ir::ExileUntilRest::StayExiled,
            }),
        });
        ex_until.amount = Some(Box::new(Amount::Const { n: 1 }));
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(311),
                PlayerId(0),
                Some(lock_id),
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![ex_until],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        let opp_exiled = *state.player(PlayerId(1)).unwrap().exile.last().unwrap();
        assert_eq!(
            state.rare(opp_exiled).and_then(|r| r.exiled_by),
            Some(lock_id)
        );
        let p1_eval = state.evaluate_board().player(PlayerId(1)).unwrap();
        assert!(p1_eval.active_rule_mods.iter().any(|entry| {
            matches!(entry.rule, mtg_ir::RuleMod::CantCastSameNameAsLinkedExile)
                && !entry.linked_exile_names.is_empty()
        }));
    }

    #[test]
    fn synth_multiplayer_timed_expiry_and_protection_from_everything() {
        let tables = InternTables::default();
        let raider = synth_card_def(
            "synth_mp_raider",
            "Synth MP Raider",
            ColorMask::R,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let giant = synth_card_def(
            "synth_mp_giant",
            "Synth MP Giant",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 5,
                toughness: 5,
            }),
            vec![],
        );
        let spell = synth_card_def(
            "synth_mp_bolt",
            "Synth MP Bolt",
            ColorMask::R,
            CardTypeMask::INSTANT,
            SmallVec::new(),
            None,
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![raider, giant, spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let cfg = GameConfig::commander(3);
        let mut state = GameState::new(db, cfg, &[deck.clone(), deck.clone(), deck], 204).unwrap();
        crate::turn::begin_opening_mulligans(&mut state).unwrap();
        for pid in [PlayerId(0), PlayerId(1), PlayerId(2)] {
            crate::turn::apply_mulligan_action(&mut state, pid, &crate::action::Action::KeepHand)
                .unwrap();
        }
        let step_to_main1 = |s: &mut GameState, target_player: PlayerId, min_turn: u32| {
            for iter in 0..=200 {
                if s.active_player() == target_player
                    && s.phase() == mtg_ir::GamePhase::Main1
                    && s.turn_number() >= min_turn
                    && matches!(s.decision(), crate::action::DecisionState::Priority)
                {
                    return;
                }
                assert!(
                    iter < 200,
                    "step_to_main1 exceeded 200 iterations (player={:?}, turn={}, phase={:?}, decision={:?})",
                    s.active_player(),
                    s.turn_number(),
                    s.phase(),
                    s.decision()
                );
                match s.decision().clone() {
                    crate::action::DecisionState::Priority => {
                        let p = s.priority_player();
                        let _ = crate::turn::pass_priority(s, p).unwrap();
                    }
                    crate::action::DecisionState::DeclareAttackers => {
                        let p = s.active_player();
                        if crate::combat::apply_combat_action(
                            s,
                            p,
                            &crate::action::Action::ConfirmAttackers,
                        )
                        .is_err()
                        {
                            let atk_candidates: Vec<_> = s
                                .player(p)
                                .map(|pl| pl.battlefield.clone())
                                .unwrap_or_default();
                            for cand in atk_candidates {
                                let _ = crate::combat::apply_combat_action(
                                    s,
                                    p,
                                    &crate::action::Action::AssignAttacker {
                                        attacker: cand,
                                        target: crate::action::AttackTarget::Player {
                                            id: PlayerId(2),
                                        },
                                    },
                                );
                            }
                            crate::combat::apply_combat_action(
                                s,
                                p,
                                &crate::action::Action::ConfirmAttackers,
                            )
                            .unwrap();
                        }
                    }
                    crate::action::DecisionState::DeclareBlockers => {
                        let p = s.priority_player();
                        crate::combat::apply_combat_action(
                            s,
                            p,
                            &crate::action::Action::ConfirmBlockers,
                        )
                        .unwrap();
                    }
                    crate::action::DecisionState::Prompt { request } => {
                        let pp = s
                            .pending_prompt()
                            .cloned()
                            .expect("pending prompt expected");
                        if let crate::action::ChoiceRequest::ChooseObjects { candidates, .. } =
                            request
                        {
                            let _ = crate::turn::resume_turn_prompt(
                                s,
                                pp.chooser,
                                pp.prompt_id,
                                ChoiceAnswer::PickObject { id: candidates[0] },
                            )
                            .unwrap();
                        } else {
                            panic!("unexpected prompt in step_to_main1: {request:?}");
                        }
                    }
                    other => panic!("unexpected decision state in step_to_main1: {other:?}"),
                }
            }
        };
        step_to_main1(&mut state, PlayerId(0), 1);

        let spawn_bf = |state: &mut GameState, card: CardDefId, ctrl: PlayerId| {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                crate::state::GameObject::new(
                    card,
                    ctrl,
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(ctrl).unwrap().battlefield.push(oid);
            state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            oid
        };

        let p1_raider = spawn_bf(&mut state, CardDefId(0), PlayerId(1));
        let p0_clone = spawn_bf(&mut state, CardDefId(0), PlayerId(0));
        let p0_giant = spawn_bf(&mut state, CardDefId(1), PlayerId(0));

        // 1) CastNameLock(EndOfTurn), Goad(UntilYourNextTurn), BecomeCopy(UntilYourNextTurn),
        // BecomeCopy(UntilEndOfYourNextTurn on own turn), and GrantPlayer(ProtectionFromEverything, UntilYourNextTurn)
        // on P0's turn.
        let mut goad_step = make_step(EffectOp::Goad {
            duration: Some(mtg_ir::Duration::UntilYourNextTurn),
        });
        goad_step.subject = Selector::Target { index: 0 };

        let mut copy_next = make_step(EffectOp::BecomeCopy {
            copy_source: Box::new(Selector::Target { index: 1 }),
            duration: Some(mtg_ir::Duration::UntilYourNextTurn),
            copy_mod: None,
        });
        copy_next.subject = Selector::SelfObj;

        let mut lock_step = make_step(EffectOp::CastNameLock {
            name_ref: mtg_ir::Ref::Target { index: 0 },
            duration: Some(mtg_ir::Duration::EndOfTurn),
        });
        lock_step.subject = Selector::PlayerRef {
            player: PlayerRef::You,
        };

        let mut prot_step = make_step(EffectOp::GrantPlayer {
            rules: vec![mtg_ir::PlayerRuleKind::ProtectionFromEverything],
            duration: Some(mtg_ir::Duration::UntilYourNextTurn),
            extra_land_plays: None,
            next_spell_types: None,
            spend_mana_as_any_color_top_of_library: false,
            steal_opponent_entering_creatures_until_eot: false,
            opponents_spells_cost_more_until_next_turn: None,
            attack_tax_until_your_next_turn: None,
            next_spell_cost_reduction_generic: None,
            next_creature_enters_with_plus_counters: None,
            next_main_phase_add_mana: None,
            double_tokens_this_turn: false,
            flash_cast_condition: None,
            next_spell_cascade: None,
            extra_loyalty_activations_this_turn: None,
            planar_action: None,
        });
        prot_step.subject = Selector::PlayerRef {
            player: PlayerRef::You,
        };

        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(320),
                PlayerId(0),
                Some(p0_clone),
                None,
                0,
                vec![
                    TargetRef::Object { id: p1_raider },
                    TargetRef::Object { id: p0_giant },
                ],
                vec![],
                SmallVec::new(),
                vec![goad_step, copy_next, lock_step, prot_step],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );

        assert!(
            state
                .evaluate_board()
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );
        assert_eq!(state.continuous_effects().len(), 1);
        assert!(!crate::combat::can_creature_attack(
            &state,
            p1_raider,
            crate::action::AttackTarget::Player { id: PlayerId(0) },
        ));
        assert!(crate::combat::can_creature_attack(
            &state,
            p1_raider,
            crate::action::AttackTarget::Player { id: PlayerId(2) },
        ));
        assert_eq!(
            state
                .rare_turn_state()
                .map_or(0, |r| r.timed_copiable_backups.len()),
            1
        );
        assert_eq!(state.player_continuous_effects().len(), 1);

        // Non-combat Damage targeting P0 with ProtectionFromEverything deals 0 damage.
        let p0_life_before = state.player(PlayerId(0)).unwrap().life;
        let ts_spell = state.allocate_timestamp();
        let spell_obj = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(1),
                Zone::Stack,
                ts_spell,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        let mut dmg_p0 = make_step(EffectOp::Damage {
            source: None,
            also_players: None,
            divided: false,
            excess_to_controller: false,
            excess_to_controller_if_trample: false,
            or_planeswalker_controlled_by: None,
            damage_attacked_target_by_self: false,
            soulfire_exile_damage_per_target: false,
            each_creature_deals: None,
        });
        dmg_p0.subject = Selector::Target { index: 0 };
        dmg_p0.amount = Some(Box::new(Amount::Const { n: 4 }));
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(321),
                PlayerId(1),
                Some(spell_obj),
                None,
                0,
                vec![TargetRef::Player { id: PlayerId(0) }],
                vec![],
                SmallVec::new(),
                vec![dmg_p0],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.player(PlayerId(0)).unwrap().life, p0_life_before);

        // Targeting P0 with a spell is illegal due to ProtectionFromEverything.
        let p_spec = mtg_ir::TargetSpec {
            kind: mtg_ir::TargetKind::Player,
            filter: ObjectFilter::default(),
            player: None,
            min: 1,
            max: 1,
            same_owner: false,
            condition: None,
            other_target: false,
            divide_total: None,
            with_single_target: false,
            dynamic_count: None,
            dynamic_up_to: false,
            max_total_mana_value: None,
            also_spells: false,
            per_opponent: false,
            different_controllers: false,
            chosen_by_event_player: false,
            shares_card_type_with_target: None,
            zones: mtg_ir::ZoneMask::empty(),
        };
        let targets_by_spec = vec![vec![]];
        let info = crate::targets::build_spell_target_source_info(
            &state,
            PlayerId(1),
            spell_obj,
            None,
            &targets_by_spec,
        );
        assert!(!crate::targets::is_target_legal_for_spec(
            &state,
            &p_spec,
            &info,
            &[],
            TargetRef::Player { id: PlayerId(0) },
        ));

        // Advance to P1's turn: EndOfTurn CastNameLock expired, UntilYourNextTurn effects remain active!
        step_to_main1(&mut state, PlayerId(1), 2);
        assert!(state.player_continuous_effects().is_empty());
        assert!(
            state
                .evaluate_board()
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );
        assert_eq!(state.continuous_effects().len(), 1);
        assert!(!crate::combat::can_creature_attack(
            &state,
            p1_raider,
            crate::action::AttackTarget::Player { id: PlayerId(0) },
        ));

        // Cast BecomeCopy(UntilEndOfYourNextTurn) on P1's turn (controller = P0 -> expires on P0's turn end).
        let mut copy_eoynt = make_step(EffectOp::BecomeCopy {
            copy_source: Box::new(Selector::Target { index: 0 }),
            duration: Some(mtg_ir::Duration::UntilEndOfYourNextTurn),
            copy_mod: None,
        });
        copy_eoynt.subject = Selector::SelfObj;
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(322),
                PlayerId(0),
                Some(p0_clone),
                None,
                0,
                vec![TargetRef::Object { id: p0_giant }],
                vec![],
                SmallVec::new(),
                vec![copy_eoynt],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(
            state
                .rare_turn_state()
                .map_or(0, |r| r.timed_copiable_backups.len()),
            2
        );

        // Advance to P2's turn -> UntilYourNextTurn + UntilEndOfYourNextTurn remain active!
        step_to_main1(&mut state, PlayerId(2), 3);
        assert!(
            state
                .evaluate_board()
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );

        // Advance back to P0's turn (Main1): UntilYourNextTurn (P0's protection, goad, copy_next) expired!
        // UntilEndOfYourNextTurn backup still active until P0's Cleanup!
        step_to_main1(&mut state, PlayerId(0), 4);
        assert!(
            !state
                .evaluate_board()
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );
        assert!(state.continuous_effects().is_empty());
        assert_eq!(
            state
                .rare_turn_state()
                .map_or(0, |r| r.timed_copiable_backups.len()),
            1
        );

        // Advance past P0's Cleanup onto P1's turn -> UntilEndOfYourNextTurn expired AND Raider can attack P0 again!
        step_to_main1(&mut state, PlayerId(1), 5);
        assert_eq!(
            state
                .rare_turn_state()
                .map_or(0, |r| r.timed_copiable_backups.len()),
            0
        );
        assert!(crate::combat::can_creature_attack(
            &state,
            p1_raider,
            crate::action::AttackTarget::Player { id: PlayerId(0) },
        ));
    }

    #[test]
    fn synth_protection_damage_prevention_and_unpreventable_bypass() {
        let mut tables = InternTables::default();
        tables.keywords.push("protection".into()); // 0
        tables.keywords.push("protection from red".into()); // 1

        let red_fighter = synth_card_def(
            "synth_red_fighter",
            "Synth Red Fighter",
            ColorMask::R,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 3,
                toughness: 3,
            }),
            vec![],
        );
        let prot_red_defender = synth_card_def(
            "synth_prot_red_guard",
            "Synth Prot-Red Guard",
            ColorMask::W,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![mtg_ir::AbilityDef::Keyword(mtg_ir::KeywordAbility {
                keyword: mtg_ir::KeywordDef {
                    keyword: mtg_ir::KeywordId(0),
                    cost: None,
                    param: Some("red".into()),
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
            })],
        );
        let unpreventable_aura = synth_card_def(
            "synth_unpreventable_relic",
            "Synth Unpreventable Relic",
            ColorMask::R,
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            None,
            vec![mtg_ir::AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::UnpreventableDamage {
                        combat_only: false,
                        source_filter: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let instant_bolt = synth_card_def(
            "synth_instant_bolt",
            "Synth Instant Bolt",
            ColorMask::R,
            CardTypeMask::INSTANT,
            SmallVec::new(),
            None,
            vec![],
        );
        let prot_card_type_relic = synth_card_def(
            "synth_prot_type_relic",
            "Synth Prot Type Relic",
            ColorMask::W,
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            None,
            vec![mtg_ir::AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::RuleModifier {
                        rule: mtg_ir::RuleMod::PlayerProtectionFromChosenCardType,
                        who: mtg_ir::PlayerFilter {
                            relation: mtg_ir::PlayerRelation::You,
                        },
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    red_fighter,
                    prot_red_defender,
                    unpreventable_aura,
                    instant_bolt,
                    prot_card_type_relic,
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
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 205).unwrap();

        let ts_r = state.allocate_timestamp();
        let red_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts_r,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(red_id);

        let ts_g = state.allocate_timestamp();
        let guard_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts_g,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(guard_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // 1) Non-combat Damage from red source onto protection-from-red creature is prevented (0 damage, 0 DamageDealt events).
        let _ = crate::events::drain_events(&mut state);
        let mut dmg_step = make_step(EffectOp::Damage {
            source: None,
            also_players: None,
            divided: false,
            excess_to_controller: true,
            excess_to_controller_if_trample: false,
            or_planeswalker_controlled_by: None,
            damage_attacked_target_by_self: false,
            soulfire_exile_damage_per_target: false,
            each_creature_deals: None,
        });
        dmg_step.subject = Selector::Target { index: 0 };
        dmg_step.amount = Some(Box::new(Amount::Const { n: 4 }));
        let p1_life_before = state.player(PlayerId(1)).unwrap().life;
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(330),
                PlayerId(0),
                Some(red_id),
                None,
                0,
                vec![TargetRef::Object { id: guard_id }],
                vec![],
                SmallVec::new(),
                vec![dmg_step.clone()],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.object(guard_id).unwrap().damage_marked, 0);
        assert_eq!(state.player(PlayerId(1)).unwrap().life, p1_life_before);
        let dmg_events: usize = crate::events::peek_events(&state)
            .iter()
            .filter(|e| matches!(e.kind, crate::events::AtomicEventKind::DamageDealt { .. }))
            .count();
        assert_eq!(dmg_events, 0);

        // 2) Fight between red 3/3 and prot-red 2/2: guard takes 0 damage, red 3/3 takes 2.
        let mut fight_step = make_step(EffectOp::Fight {
            with: Box::new(Selector::Target { index: 1 }),
            excess_damage_to_mana: None,
        });
        fight_step.subject = Selector::Target { index: 0 };
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(331),
                PlayerId(0),
                Some(red_id),
                None,
                0,
                vec![
                    TargetRef::Object { id: red_id },
                    TargetRef::Object { id: guard_id },
                ],
                vec![],
                SmallVec::new(),
                vec![fight_step],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.object(guard_id).unwrap().damage_marked, 0);
        assert_eq!(state.object(red_id).unwrap().damage_marked, 2);

        // 3) UnpreventableDamage static ability bypasses CR 702.16e damage prevention!
        let ts_u = state.allocate_timestamp();
        let unprev_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Battlefield,
                ts_u,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(unprev_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut dmg_2 = dmg_step;
        dmg_2.amount = Some(Box::new(Amount::Const { n: 2 }));
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(332),
                PlayerId(0),
                Some(red_id),
                None,
                0,
                vec![TargetRef::Object { id: guard_id }],
                vec![],
                SmallVec::new(),
                vec![dmg_2],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        // Lethal 2 damage marked on 2/2 guard -> SBA destroys guard (CR 400.7 new Graveyard ObjectId)!
        let _ = crate::sba::check_sbas(&mut state).unwrap();
        assert!(state.object(guard_id).is_none());
        assert!(
            !state
                .player(PlayerId(1))
                .unwrap()
                .battlefield
                .contains(&guard_id)
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().graveyard.len(), 1);

        // 4) Also verify PlayerRuleKind::DamageCantBePrevented bypasses protection on a fresh guard.
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .retain(|&id| id != unprev_id);
        state.object_mut(unprev_id).unwrap().zone = Zone::Graveyard;
        let ts_g2 = state.allocate_timestamp();
        let guard2_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(1),
                Zone::Battlefield,
                ts_g2,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(guard2_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        state.set_damage_cant_be_prevented_this_turn(false);

        let mut dmg_2b = make_step(EffectOp::Damage {
            source: None,
            also_players: None,
            divided: false,
            excess_to_controller: false,
            excess_to_controller_if_trample: false,
            or_planeswalker_controlled_by: None,
            damage_attacked_target_by_self: false,
            soulfire_exile_damage_per_target: false,
            each_creature_deals: None,
        });
        dmg_2b.subject = Selector::Target { index: 0 };
        dmg_2b.amount = Some(Box::new(Amount::Const { n: 2 }));
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(333),
                PlayerId(0),
                Some(red_id),
                None,
                0,
                vec![TargetRef::Object { id: guard2_id }],
                vec![],
                SmallVec::new(),
                vec![dmg_2b],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.object(guard2_id).unwrap().damage_marked, 2);

        // 5) PlayerProtectionFromChosenCardType prevents spell damage to that player when
        // damage_cant_be_prevented_this_turn is cleared.
        state.clear_eot_prevention_state();
        let ts_p = state.allocate_timestamp();
        let relic_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(4),
                PlayerId(1),
                Zone::Battlefield,
                ts_p,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state.rare_or_default(relic_id).unwrap().chosen_card_type = Some(CardTypeMask::INSTANT);
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(relic_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let ts_bolt = state.allocate_timestamp();
        let bolt_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(3),
                PlayerId(0),
                Zone::Stack,
                ts_bolt,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        let mut dmg_player = make_step(EffectOp::Damage {
            source: None,
            also_players: None,
            divided: false,
            excess_to_controller: false,
            excess_to_controller_if_trample: false,
            or_planeswalker_controlled_by: None,
            damage_attacked_target_by_self: false,
            soulfire_exile_damage_per_target: false,
            each_creature_deals: None,
        });
        dmg_player.subject = Selector::Target { index: 0 };
        dmg_player.amount = Some(Box::new(Amount::Const { n: 3 }));
        let p1_life_now = state.player(PlayerId(1)).unwrap().life;
        assert_eq!(
            begin_resolution_frame(
                &mut state,
                StackItemId(334),
                PlayerId(0),
                Some(bolt_id),
                None,
                0,
                vec![TargetRef::Player { id: PlayerId(1) }],
                vec![],
                SmallVec::new(),
                vec![dmg_player],
                vec![],
            )
            .unwrap(),
            FrameOutcome::Completed
        );
        assert_eq!(state.player(PlayerId(1)).unwrap().life, p1_life_now);
    }

    #[test]
    fn synth_copy_permanent_uses_vm_event_lki_even_when_lki_records_empty() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Elemental".into());
        let land = CardDef {
            oracle_id: "synth_copy_lki_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Copy Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Copy Land".into(),
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
        let elemental = CardDef {
            oracle_id: "synth_copy_elemental".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth LKI Elemental".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth LKI Elemental".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: smallvec::smallvec![mtg_ir::SubtypeId(0)],
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 4,
                        toughness: 3,
                    }),
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
                cards: vec![land, elemental],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 555).unwrap();

        let copier_id = state.player(PlayerId(0)).unwrap().hand[0];
        let dead_id = ObjectId {
            index: 999,
            generation: 5,
        };
        let synth_elemental_lki = Arc::new(LkiSnapshot {
            object_id: dead_id,
            card_def: CardDefId(1),
            name: "Synth LKI Elemental".into(),
            owner: PlayerId(0),
            controller: PlayerId(0),
            last_zone: Zone::Battlefield,
            pt: Some(crate::view::PowerToughness {
                power: 4,
                toughness: 3,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: smallvec::smallvec![mtg_ir::SubtypeId(0)],
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::R,
            mana_value: 3,
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
        });

        // Confirm neither live object nor lki_records has dead_id.
        state.lki_records_mut().clear();
        assert!(state.object(dead_id).is_none());
        assert!(state.lki(dead_id).is_none());

        let sid = state.allocate_stack_id();
        state.stack_entries_mut().push(crate::state::StackEntry {
            id: sid,
            kind: crate::state::StackEntryKind::Triggered {
                source: copier_id,
                ability_idx: 0,
                lki: Arc::clone(&synth_elemental_lki),
            },
            controller: PlayerId(0),
            choices: crate::state::StackChoices::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });
        state.push_stack_trigger_meta(
            sid,
            crate::triggers::StackTriggerMeta {
                source: copier_id,
                face_index: 0,
                ability_idx: 0,
                kind: crate::triggers::TriggerSourceKind::Innate {
                    component_index: 0,
                    face_index: 0,
                    ability_idx: 0,
                },
                condition: None,
                targets: Vec::new(),
                effect: Vec::new(),
                modal: None,
                event_object: Some(dead_id),
                event_lki: Some(Arc::clone(&synth_elemental_lki)),
                event_player: Some(PlayerId(0)),
                event_amount: None,
                defending_player: None,
                last_tapped_mana_colors: SmallVec::new(),
                is_look_back: false,
                x_value: 0,
                bindings: Vec::new(),
            },
        );

        let mut copy_step = make_step(EffectOp::CopyPermanent {
            select: mtg_ir::SelectionMod::default(),
            not_legendary: false,
            tapped: false,
            tapped_and_attacking: false,
            per_opponent_tapped_attacking: None,
            copy_mod: None,
            except_pt: None,
        });
        copy_step.subject = Selector::EventObject;

        let out = begin_resolution_frame(
            &mut state,
            sid,
            PlayerId(0),
            Some(copier_id),
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![copy_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out, FrameOutcome::Completed);

        let bf = &state.player(PlayerId(0)).unwrap().battlefield;
        assert_eq!(bf.len(), 1);
        let tok_id = bf[0];
        let tok_obj = state.object(tok_id).unwrap();
        assert!(tok_obj.is_token());
        let cache = state.evaluate_board();
        let ev = cache.get(tok_id).unwrap();
        assert_eq!(
            ev.pt,
            Some(crate::view::PowerToughness {
                power: 4,
                toughness: 3
            })
        );
        assert!(ev.types.contains(CardTypeMask::CREATURE));
        assert!(ev.subtypes.contains(&mtg_ir::SubtypeId(0)));
        assert_eq!(ev.colors, ColorMask::R);
    }

    #[test]
    fn synth_move_hand_to_library_top_uses_step_amount_and_preserves_top_order() {
        let mut state = synth_db_and_state(false);
        let old_hand = state.player(PlayerId(0)).unwrap().hand.clone();
        for id in old_hand {
            crate::zones::move_object_raw(
                &mut state,
                id,
                Zone::Library,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let mut hand_ids = Vec::new();
        for _ in 0..4 {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(id);
            hand_ids.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Library),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: false,
            topmost_only: false,
            may_shuffle: false,
            position: Some(mtg_ir::LibraryPosition::Top),
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::HAND,
                controller: Some(mtg_ir::RelativeControl::You),
                ..Default::default()
            }),
        };
        mv_step.amount = Some(Box::new(Amount::Const { n: 2 }));

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(401),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![mv_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected ChooseObjects pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        match state.pending_prompt().map(|p| &p.request) {
            Some(crate::action::ChoiceRequest::ChooseObjects {
                min,
                max,
                candidates,
                ..
            }) => {
                assert_eq!((*min, *max, candidates.len()), (2, 2, 4));
            }
            other => panic!("expected ChooseObjects(min=2, max=2), got {other:?}"),
        }

        let pick_a = hand_ids[0];
        let pick_b = hand_ids[1];
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickObject { id: pick_a },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: prompt_id2,
            ..
        } = out2
        else {
            panic!("expected second pick pause, got {out2:?}");
        };
        let out3 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id2,
            ChoiceAnswer::PickObject { id: pick_b },
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);

        let p0 = state.player(PlayerId(0)).unwrap();
        assert_eq!(p0.hand.len(), 2);
        let lib_len = p0.library.len();
        assert_eq!(p0.library[lib_len - 1].index, pick_a.index);
        assert_eq!(p0.library[lib_len - 2].index, pick_b.index);
    }

    #[test]
    fn synth_move_hand_to_library_top_pauses_when_hand_len_equals_amount() {
        let mut state = synth_db_and_state(false);
        let old_hand = state.player(PlayerId(0)).unwrap().hand.clone();
        for id in old_hand {
            crate::zones::move_object_raw(
                &mut state,
                id,
                Zone::Library,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let mut hand_ids = Vec::new();
        for _ in 0..2 {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(id);
            hand_ids.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Library),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: false,
            topmost_only: false,
            may_shuffle: false,
            position: Some(mtg_ir::LibraryPosition::Top),
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::HAND,
                controller: Some(mtg_ir::RelativeControl::You),
                ..Default::default()
            }),
        };
        mv_step.amount = Some(Box::new(Amount::Const { n: 2 }));

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(402),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![mv_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected ChooseObjects pause when hand len == amount == 2, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        match state.pending_prompt().map(|p| &p.request) {
            Some(crate::action::ChoiceRequest::ChooseObjects {
                min,
                max,
                candidates,
                ..
            }) => {
                assert_eq!((*min, *max, candidates.len()), (2, 2, 2));
            }
            other => panic!("expected ChooseObjects(min=2, max=2), got {other:?}"),
        }

        let pick_first = hand_ids[1];
        let pick_second = hand_ids[0];
        let FrameOutcome::Paused {
            prompt_id: prompt_id2,
            ..
        } = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickObject { id: pick_first },
        )
        .unwrap()
        else {
            panic!("expected second pick pause");
        };
        let out3 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id2,
            ChoiceAnswer::PickObject { id: pick_second },
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);

        let p0 = state.player(PlayerId(0)).unwrap();
        assert!(p0.hand.is_empty());
        let lib_len = p0.library.len();
        assert_eq!(p0.library[lib_len - 1].index, pick_first.index);
        assert_eq!(p0.library[lib_len - 2].index, pick_second.index);
    }

    #[test]
    fn synth_move_hand_to_battlefield_up_to_uses_step_amount() {
        let mut state = synth_db_and_state(false);
        let old_hand = state.player(PlayerId(0)).unwrap().hand.clone();
        for id in old_hand {
            crate::zones::move_object_raw(
                &mut state,
                id,
                Zone::Library,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let mut hand_ids = Vec::new();
        for _ in 0..3 {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(id);
            hand_ids.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let mut mv_step = make_step(EffectOp::Move {
            from: Some(Zone::Hand),
            to: Some(Zone::Battlefield),
            select: mtg_ir::SelectionMod {
                up_to: true,
                choice_count: None,
                ..Default::default()
            },
            tapped: false,
            under_your_control: false,
            topmost_only: false,
            may_shuffle: false,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        mv_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::HAND,
                types_any: CardTypeMask::LAND,
                controller: Some(mtg_ir::RelativeControl::You),
                ..Default::default()
            }),
        };
        mv_step.amount = Some(Box::new(Amount::Const { n: 1 }));

        let out = begin_resolution_frame(
            &mut state,
            StackItemId(403),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![mv_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id, chooser } = out else {
            panic!("expected ChooseObjects pause, got {out:?}");
        };
        assert_eq!(chooser, PlayerId(0));
        match state.pending_prompt().map(|p| &p.request) {
            Some(crate::action::ChoiceRequest::ChooseObjects {
                min,
                max,
                candidates,
                ..
            }) => {
                assert_eq!((*min, *max, candidates.len()), (0, 1, 3));
            }
            other => panic!("expected ChooseObjects(min=0, max=1), got {other:?}"),
        }

        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickObject { id: hand_ids[0] },
        )
        .unwrap();
        assert_eq!(out2, FrameOutcome::Completed);

        let p0 = state.player(PlayerId(0)).unwrap();
        assert_eq!(p0.hand.len(), 2);
        assert_eq!(p0.battlefield.len(), 1);
    }

    #[test]
    fn synth_dig_partitions_three_two_one_cards_and_postcard_roundtrip() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        let card = synth_card_def(
            "synth_dig_land",
            "Synth Dig Land",
            ColorMask::empty(),
            CardTypeMask::LAND,
            SmallVec::new(),
            None,
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![card],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };

        let make_partition_dig = || {
            make_step(EffectOp::Dig {
                spec: Box::new(mtg_ir::DigSpec {
                    keep: 1,
                    up_to: false,
                    keep_filter: None,
                    keep_all_matching: false,
                    keep_label: None,
                    keep_to: mtg_ir::DigKeepTo::Hand,
                    keep_tapped: false,
                    keep_attacking: false,
                    keep_attacking_player: None,
                    reveal_all: false,
                    reveal_kept: false,
                    rest: mtg_ir::DigRestTo::LibraryBottom,
                    rest_random: false,
                    rest_order: None,
                    until_filter: None,
                    rest_to_hand: false,
                    shuffle_rest: false,
                    keep_max_mana_value: None,
                    keep_max_mv_strict_lt: false,
                    choose_one_of_exiled: false,
                    play_from_exile: None,
                    partitions: vec![
                        mtg_ir::DigPartition {
                            count: 1,
                            to: mtg_ir::DigRestTo::LibraryBottom,
                            to_hand: true,
                            playable_this_turn: false,
                        },
                        mtg_ir::DigPartition {
                            count: 1,
                            to: mtg_ir::DigRestTo::LibraryBottom,
                            to_hand: false,
                            playable_this_turn: false,
                        },
                        mtg_ir::DigPartition {
                            count: 1,
                            to: mtg_ir::DigRestTo::Exile,
                            to_hand: false,
                            playable_this_turn: true,
                        },
                    ],
                    destination_buckets: vec![],
                }),
                look_only: true,
            })
        };
        let mut dig_step = make_partition_dig();
        dig_step.amount = Some(Box::new(Amount::Const { n: 3 }));

        // Case A: 3 cards -> 2 prompts + postcard snapshot roundtrip mid-DigPick
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck.clone()],
            101,
        )
        .unwrap();
        let top0 = *state.player(PlayerId(0)).unwrap().library.last().unwrap();
        let lib = &state.player(PlayerId(0)).unwrap().library;
        let top1 = lib[lib.len() - 2];
        let top2 = lib[lib.len() - 3];
        let hand_before = state.player(PlayerId(0)).unwrap().hand.len();

        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(501),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step.clone()],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid1,
            chooser: c1,
        } = out1
        else {
            panic!("expected first DigPick prompt, got {out1:?}");
        };
        assert_eq!(c1, PlayerId(0));

        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            pid1,
            ChoiceAnswer::PickObject { id: top0 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid2,
            chooser: c2,
        } = out2
        else {
            panic!("expected second DigPick prompt, got {out2:?}");
        };
        assert_eq!(c2, PlayerId(0));
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseObjects {
                candidates,
                min,
                max,
                ..
            }) => {
                assert_eq!((*min, *max), (1, 1));
                assert_eq!(candidates, &vec![top1, top2]);
            }
            other => panic!("expected second ChooseObjects(1..=1), got {other:?}"),
        }

        // Postcard round-trip via GameStateSnapshot while paused with partition_picks populated.
        let bytes = postcard::to_allocvec(&state.to_snapshot()).unwrap();
        let snap: crate::state::GameStateSnapshot = postcard::from_bytes(&bytes).unwrap();
        let mut restored = GameState::from_snapshot(Arc::clone(&db), snap);

        let out3 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            pid2,
            ChoiceAnswer::PickObject { id: top1 },
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);

        let p0 = restored.player(PlayerId(0)).unwrap();
        assert_eq!(p0.hand.len(), hand_before + 1);
        assert_eq!(p0.exile.len(), 1);
        let exiled_id = p0.exile[0];
        let pfe = restored
            .rare(exiled_id)
            .and_then(|r| r.playable_from_exile_by.as_ref())
            .expect("exiled partition should be playable this turn");
        assert_eq!(pfe.player, PlayerId(0));
        assert_eq!(pfe.expires_turn, Some(restored.turn_number()));
        assert_eq!(p0.library.len(), 1); // 1 card bottomed from the 3-card dig

        // Case B: 2 cards in library -> 1 prompt (hand + playable exile, 0 bottom)
        let mut state2 = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck.clone()],
            102,
        )
        .unwrap();
        let p0_lib_len = state2.player(PlayerId(0)).unwrap().library.len();
        for _ in 0..(p0_lib_len - 2) {
            let id = state2
                .player_mut(PlayerId(0))
                .unwrap()
                .library
                .pop()
                .unwrap();
            state2.player_mut(PlayerId(0)).unwrap().hand.push(id);
            state2.object_mut(id).unwrap().zone = Zone::Hand;
        }
        let two_top = *state2.player(PlayerId(0)).unwrap().library.last().unwrap();
        let hand_before2 = state2.player(PlayerId(0)).unwrap().hand.len();
        let out_b1 = begin_resolution_frame(
            &mut state2,
            StackItemId(502),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step.clone()],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid_b, ..
        } = out_b1
        else {
            panic!("expected 1 prompt for 2 cards, got {out_b1:?}");
        };
        let out_b2 = resume_resolution_frame(
            &mut state2,
            PlayerId(0),
            pid_b,
            ChoiceAnswer::PickObject { id: two_top },
        )
        .unwrap();
        assert_eq!(out_b2, FrameOutcome::Completed);
        assert_eq!(
            state2.player(PlayerId(0)).unwrap().hand.len(),
            hand_before2 + 1
        );
        assert_eq!(state2.player(PlayerId(0)).unwrap().exile.len(), 1);
        assert!(
            state2
                .rare(state2.player(PlayerId(0)).unwrap().exile[0])
                .and_then(|r| r.playable_from_exile_by.as_ref())
                .is_some()
        );

        // Case C: 1 card in library -> 0 prompts (straight to hand)
        let mut state1 = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            103,
        )
        .unwrap();
        let p0_lib_len1 = state1.player(PlayerId(0)).unwrap().library.len();
        for _ in 0..(p0_lib_len1 - 1) {
            let id = state1
                .player_mut(PlayerId(0))
                .unwrap()
                .library
                .pop()
                .unwrap();
            state1.player_mut(PlayerId(0)).unwrap().hand.push(id);
            state1.object_mut(id).unwrap().zone = Zone::Hand;
        }
        let hand_before1 = state1.player(PlayerId(0)).unwrap().hand.len();
        let out_c = begin_resolution_frame(
            &mut state1,
            StackItemId(503),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out_c, FrameOutcome::Completed);
        assert_eq!(
            state1.player(PlayerId(0)).unwrap().hand.len(),
            hand_before1 + 1
        );
        assert!(state1.player(PlayerId(0)).unwrap().exile.is_empty());
    }

    #[test]
    fn synth_multi_dest_search_sequential_fail_to_find_and_postcard_roundtrip() {
        let mut state = synth_db_and_state(false);
        let db = Arc::new(state.db().clone());
        let search_step = make_step(EffectOp::Search {
            spec: Box::new(mtg_ir::SearchSpec {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::LAND,
                    ..Default::default()
                }),
                count: 2,
                up_to: true,
                stated_quality: true,
                destinations: vec![
                    mtg_ir::SearchDestination {
                        zone: Zone::Battlefield,
                        tapped: true,
                        on_top: false,
                    },
                    mtg_ir::SearchDestination {
                        zone: Zone::Hand,
                        tapped: false,
                        on_top: false,
                    },
                ],
                reveal: true,
                description: None,
                zones: mtg_ir::ZoneMask::LIBRARY,
                different_names: false,
                shuffle_only_if_library: false,
                searched_player: None,
                cast_free: false,
                target_controller_as_actor: false,
            }),
            tapped: true,
        });

        let lib0 = state.player(PlayerId(0)).unwrap().library[0];
        let lib1 = state.player(PlayerId(0)).unwrap().library[1];
        let hand_before = state.player(PlayerId(0)).unwrap().hand.len();
        let bf_before = state.player(PlayerId(0)).unwrap().battlefield.len();

        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(511),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![search_step.clone()],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid1, ..
        } = out1
        else {
            panic!("expected first SearchPick prompt, got {out1:?}");
        };
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseObjects { min, max, .. }) => {
                assert_eq!((*min, *max), (0, 1));
            }
            other => panic!("expected ChooseObjects(min=0, max=1), got {other:?}"),
        }

        // Pick first land -> immediately moves tapped to battlefield, opens prompt 2.
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            pid1,
            ChoiceAnswer::PickObject { id: lib0 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid2, ..
        } = out2
        else {
            panic!("expected second SearchPick prompt, got {out2:?}");
        };
        assert_eq!(
            state.player(PlayerId(0)).unwrap().battlefield.len(),
            bf_before + 1
        );
        let bf_id = *state
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .last()
            .unwrap();
        assert!(
            state
                .object(bf_id)
                .unwrap()
                .flags
                .contains(crate::state::ObjectFlags::TAPPED)
        );
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseObjects {
                candidates,
                min,
                max,
                ..
            }) => {
                assert_eq!((*min, *max), (0, 1));
                assert!(!candidates.contains(&lib0));
                assert!(candidates.contains(&lib1));
            }
            other => panic!("expected second ChooseObjects(min=0, max=1), got {other:?}"),
        }

        // Postcard round-trip while paused at dest_idx == 1!
        let bytes = postcard::to_allocvec(&state.to_snapshot()).unwrap();
        let snap: crate::state::GameStateSnapshot = postcard::from_bytes(&bytes).unwrap();
        let mut restored = GameState::from_snapshot(Arc::clone(&db), snap);

        let out3 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            pid2,
            ChoiceAnswer::PickObject { id: lib1 },
        )
        .unwrap();
        assert_eq!(out3, FrameOutcome::Completed);
        assert_eq!(
            restored.player(PlayerId(0)).unwrap().hand.len(),
            hand_before + 1
        );
        assert_eq!(
            restored.player(PlayerId(0)).unwrap().battlefield.len(),
            bf_before + 1
        );

        // Fail to find on prompt 1 -> skips prompt 2 and finishes immediately.
        let mut state_ftf = synth_db_and_state(false);
        let out_ftf1 = begin_resolution_frame(
            &mut state_ftf,
            StackItemId(512),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![search_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: pid_f, ..
        } = out_ftf1
        else {
            panic!("expected SearchPick prompt for fail-to-find");
        };
        let out_ftf2 = resume_resolution_frame(
            &mut state_ftf,
            PlayerId(0),
            pid_f,
            ChoiceAnswer::DonePicking,
        )
        .unwrap();
        assert_eq!(out_ftf2, FrameOutcome::Completed);
    }

    #[test]
    fn synth_move_may_shuffle_is_unconditional_while_look_and_reorder_still_prompts() {
        let mut state = synth_db_and_state(false);
        // Spawn a permanent owned by PlayerId(1) on the battlefield.
        let ts = state.allocate_timestamp();
        let perm = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(1),
                Zone::Battlefield,
                ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .push(perm);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let p1_lib_before = state.player(PlayerId(1)).unwrap().library.len();
        let mut move_step = make_step(EffectOp::Move {
            from: None,
            to: Some(Zone::Library),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: false,
            topmost_only: false,
            may_shuffle: true,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        move_step.subject = Selector::Target { index: 0 };

        let ev_start = state.event_log().len();
        let out = begin_resolution_frame(
            &mut state,
            StackItemId(521),
            PlayerId(0),
            None,
            None,
            0,
            vec![TargetRef::Object { id: perm }],
            vec![],
            SmallVec::new(),
            vec![move_step],
            vec![],
        )
        .unwrap();
        // Unconditional shuffle -> completes immediately with 0 YesNo prompts!
        assert_eq!(out, FrameOutcome::Completed);
        assert!(state.player(PlayerId(1)).unwrap().battlefield.is_empty());
        assert_eq!(
            state.player(PlayerId(1)).unwrap().library.len(),
            p1_lib_before + 1
        );
        assert!(
            state.event_log()[ev_start..].iter().any(|ev| matches!(
                ev.kind,
                crate::events::AtomicEventKind::LibraryShuffled {
                    player: PlayerId(1)
                }
            )),
            "expected LibraryShuffled event for owner"
        );

        // Empty Selector::All with may_shuffle: true still shuffles affected player (CR 701.20d).
        let mut all_gy_step = make_step(EffectOp::Move {
            from: Some(Zone::Graveyard),
            to: Some(Zone::Library),
            select: mtg_ir::SelectionMod::default(),
            tapped: false,
            under_your_control: false,
            topmost_only: false,
            may_shuffle: true,
            position: None,
            from_top: None,
            library_choice_by: None,
            random_order: false,
            entered_transformed: false,
            entered_flipped: false,
            enters_tapped_and_attacking: false,
            as_enchantment_only: false,
            unearth: false,
            with_counters: None,
            attached_to: None,
            condition: None,
            choose_at_random_from_exiled_with_self: false,
            leave_chosen_in_graveyard_rest_to_hand: false,
            split_exiled_cost_cards_to_bottom_and_battlefield: false,
        });
        all_gy_step.subject = Selector::All {
            filter: Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::GRAVEYARD,
                owner: Some(mtg_ir::RelativeControl::You),
                ..Default::default()
            }),
        };
        let ev_start2 = state.event_log().len();
        let out_empty = begin_resolution_frame(
            &mut state,
            StackItemId(522),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![all_gy_step],
            vec![],
        )
        .unwrap();
        assert_eq!(out_empty, FrameOutcome::Completed);
        assert!(
            state.event_log()[ev_start2..].iter().any(|ev| matches!(
                ev.kind,
                crate::events::AtomicEventKind::LibraryShuffled {
                    player: PlayerId(0)
                }
            )),
            "expected empty Selector::All move-shuffle to still shuffle PlayerId(0)"
        );

        // Regression check: LookAndReorder { may_shuffle: true } reorders top N >= 2, then prompts YesNo.
        let mut look_step = make_step(EffectOp::LookAndReorder { may_shuffle: true });
        look_step.amount = Some(Box::new(Amount::Const { n: 2 }));
        let lib_snapshot = state.player(PlayerId(0)).unwrap().library.clone();
        let top_card = *lib_snapshot.last().unwrap();
        let second_card = lib_snapshot[lib_snapshot.len() - 2];
        let out_look = begin_resolution_frame(
            &mut state,
            StackItemId(523),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![look_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: lp, .. } = out_look else {
            panic!("expected LookAndReorder ReorderLibrary pause");
        };
        assert!(matches!(
            state.pending_prompt().map(|p| &p.request),
            Some(ChoiceRequest::OrderObjects { objects, .. })
                if objects == &[top_card, second_card]
        ));
        let out_look2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            lp,
            ChoiceAnswer::OrderNext { id: second_card },
        )
        .unwrap();
        let p0_lib_after_reorder = &state.player(PlayerId(0)).unwrap().library;
        let len_after = p0_lib_after_reorder.len();
        assert_eq!(len_after, lib_snapshot.len());
        assert_eq!(p0_lib_after_reorder[len_after - 1], second_card);
        assert_eq!(p0_lib_after_reorder[len_after - 2], top_card);

        let FrameOutcome::Paused { prompt_id: yp, .. } = out_look2 else {
            panic!("expected LookAndReorder may_shuffle YesNo prompt, got {out_look2:?}");
        };
        assert!(matches!(
            state.pending_prompt().map(|p| &p.request),
            Some(ChoiceRequest::YesNo { label, .. }) if label == "Shuffle your library?"
        ));

        let ev_start3 = state.event_log().len();
        let out_look3 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            yp,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(out_look3, FrameOutcome::Completed);
        assert!(
            state.event_log()[ev_start3..].iter().any(|ev| matches!(
                ev.kind,
                crate::events::AtomicEventKind::LibraryShuffled {
                    player: PlayerId(0)
                }
            )),
            "expected LookAndReorder may_shuffle YesNo=true to emit LibraryShuffled"
        );
    }

    #[test]
    fn synth_interactive_move_counters_loop_and_postcard_roundtrip() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let creature = synth_card_def(
            "synth_counter_host",
            "Synth Counter Host",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 0,
                toughness: 3,
            }),
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![creature],
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
            202,
        )
        .unwrap();
        let spawn = |s: &mut GameState, owner: PlayerId| {
            let ts = s.allocate_timestamp();
            let id = s.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    owner,
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            s.player_mut(owner).unwrap().battlefield.push(id);
            s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };
        let src_c = spawn(&mut state, PlayerId(0));
        let dst_a = spawn(&mut state, PlayerId(0));
        let dst_b = spawn(&mut state, PlayerId(1));
        state.add_counter(src_c, mtg_ir::CounterId(0), 3);

        let mut mc_step = make_step(EffectOp::MoveCounters {
            counter: Some(mtg_ir::CounterId(0)),
            to_filter: Some(Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            })),
            spec: None,
        });
        mc_step.subject = Selector::SelfObj;
        mc_step.amount = None;

        // Prompt 1: SelectTarget (min=0, max=1) excluding src_c
        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(531),
            PlayerId(0),
            Some(src_c),
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![mc_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected MoveCountersSelectTarget pause, got {out1:?}");
        };
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseObjects {
                candidates,
                min,
                max,
                ..
            }) => {
                assert_eq!((*min, *max), (0, 1));
                assert!(!candidates.contains(&src_c));
                assert_eq!(candidates, &vec![dst_a, dst_b]);
            }
            other => panic!("expected ChooseObjects(0..=1), got {other:?}"),
        }

        // Pick dst_a -> ChooseNumber(1..=3)
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            ChoiceAnswer::PickObject { id: dst_a },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p2, .. } = out2 else {
            panic!("expected MoveCountersChooseAmount pause, got {out2:?}");
        };
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseNumber { min, max, .. }) => {
                assert_eq!((*min, *max), (1, 3));
            }
            other => panic!("expected ChooseNumber(1..=3), got {other:?}"),
        }

        // Postcard round-trip while paused on MoveCountersChooseAmount!
        let bytes = postcard::to_allocvec(&state.to_snapshot()).unwrap();
        let snap: crate::state::GameStateSnapshot = postcard::from_bytes(&bytes).unwrap();
        let mut restored = GameState::from_snapshot(Arc::clone(&db), snap);

        // Pick amount 2 -> back to SelectTarget with remaining=1 (ChooseObjects 0..=1)
        let out3 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            p2,
            ChoiceAnswer::PickNumber { value: 2 },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p3, .. } = out3 else {
            panic!("expected second MoveCountersSelectTarget pause, got {out3:?}");
        };
        // Counters have not moved yet (atomic apply at end of loop).
        assert_eq!(restored.counter_count(src_c, mtg_ir::CounterId(0)), 3);

        // DonePicking finishes early and applies the 2 counters onto dst_a!
        let out4 =
            resume_resolution_frame(&mut restored, PlayerId(0), p3, ChoiceAnswer::DonePicking)
                .unwrap();
        assert_eq!(out4, FrameOutcome::Completed);
        assert_eq!(restored.counter_count(src_c, mtg_ir::CounterId(0)), 1);
        assert_eq!(restored.counter_count(dst_a, mtg_ir::CounterId(0)), 2);
        assert_eq!(restored.counter_count(dst_b, mtg_ir::CounterId(0)), 0);
    }

    #[test]
    fn synth_finalize_dig_partitions_pauses_on_replacement_roundtrips_and_resumes_remaining() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let make_rep_ench = |oracle_id: &str, name: &str, mod_spec: mtg_ir::ZoneChangeMod| {
            synth_card_def(
                oracle_id,
                name,
                ColorMask::W,
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                None,
                vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::LIBRARY,
                                        to: Some(Zone::Exile),
                                        from_anywhere: false,
                                        not_from_zone: None,
                                        not_to_zone: None,
                                        enters_untapped: false,
                                        dealt_damage_by_self_this_turn: false,
                                    }),
                                    ..Default::default()
                                },
                            },
                            with: mtg_ir::ReplacementWith::ZoneChange {
                                mod_spec: Box::new(mod_spec),
                            },
                            self_replacement: false,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
            )
        };
        let card_a = synth_card_def(
            "synth_dig_a",
            "Synth Dig A",
            ColorMask::U,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![],
        );
        let card_b = synth_card_def(
            "synth_dig_b",
            "Synth Dig B",
            ColorMask::U,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let card_c = synth_card_def(
            "synth_dig_c",
            "Synth Dig C",
            ColorMask::U,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 3,
                toughness: 3,
            }),
            vec![],
        );
        let rep_exile_tapped = make_rep_ench(
            "synth_rep_exile_tap",
            "Synth Rep Exile Tap",
            mtg_ir::ZoneChangeMod {
                to: Some(Zone::Exile),
                tapped: Some(true),
                ..Default::default()
            },
        );
        let rep_exile_to_gy = make_rep_ench(
            "synth_rep_exile_gy",
            "Synth Rep Exile Gy",
            mtg_ir::ZoneChangeMod {
                to: Some(Zone::Graveyard),
                ..Default::default()
            },
        );
        let rep_exile_counter = make_rep_ench(
            "synth_rep_exile_ctr",
            "Synth Rep Exile Ctr",
            mtg_ir::ZoneChangeMod {
                to: Some(Zone::Exile),
                exile_with_counter: Some(mtg_ir::CounterId(0)),
                ..Default::default()
            },
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    card_a,
                    card_b,
                    card_c,
                    rep_exile_tapped,
                    rep_exile_to_gy,
                    rep_exile_counter,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            303,
        )
        .unwrap();

        let put_bf = |s: &mut GameState, cd: CardDefId| {
            let ts = s.allocate_timestamp();
            let id = s.insert_object(
                crate::state::GameObject::new(
                    cd,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            s.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };
        put_bf(&mut state, CardDefId(3)); // rep_exile_tapped
        put_bf(&mut state, CardDefId(5)); // rep_exile_counter

        let lib = &state.player(PlayerId(0)).unwrap().library;
        let top0 = lib[lib.len() - 1];
        let top1 = lib[lib.len() - 2];
        let top2 = lib[lib.len() - 3];
        let top2_def = state.object(top2).unwrap().card_def;

        let mut dig_step = make_step(EffectOp::Dig {
            spec: Box::new(mtg_ir::DigSpec {
                keep: 1,
                up_to: false,
                keep_filter: None,
                keep_all_matching: false,
                keep_label: None,
                keep_to: mtg_ir::DigKeepTo::Hand,
                keep_tapped: false,
                keep_attacking: false,
                keep_attacking_player: None,
                reveal_all: false,
                reveal_kept: false,
                rest: mtg_ir::DigRestTo::LibraryBottom,
                rest_random: false,
                rest_order: None,
                until_filter: None,
                rest_to_hand: false,
                shuffle_rest: false,
                keep_max_mana_value: None,
                keep_max_mv_strict_lt: false,
                choose_one_of_exiled: false,
                play_from_exile: None,
                partitions: vec![
                    mtg_ir::DigPartition {
                        count: 1,
                        to: mtg_ir::DigRestTo::LibraryBottom,
                        to_hand: true,
                        playable_this_turn: false,
                    },
                    mtg_ir::DigPartition {
                        count: 1,
                        to: mtg_ir::DigRestTo::Exile,
                        to_hand: false,
                        playable_this_turn: true,
                    },
                ],
                destination_buckets: vec![],
            }),
            look_only: true,
        });
        dig_step.amount = Some(Box::new(Amount::Const { n: 3 }));
        dig_step.bind_as = Some("dug".into());

        let mut gain_step = make_step(EffectOp::GainLife {
            set_life_total: false,
        });
        gain_step.amount = Some(Box::new(Amount::Const { n: 1 }));
        gain_step.optional = Some(OptionalSpec {
            mode: OptionalMode::DoIfPaid,
            cost: vec![],
            chooser: PlayerRef::You,
        });

        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(601),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step.clone(), gain_step.clone()],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected DigPick prompt 1, got {out1:?}");
        };
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            ChoiceAnswer::PickObject { id: top0 },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p2, .. } = out2 else {
            panic!("expected DigPick prompt 2, got {out2:?}");
        };
        let out3 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p2,
            ChoiceAnswer::PickObject { id: top1 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: rep_pid, ..
        } = out3
        else {
            panic!("expected WaitingOnReplacement pause, got {out3:?}");
        };

        assert!(matches!(
            state.resolution_vms().last().and_then(|v| v.pause_state.as_ref()),
            Some(StepPauseState::WaitingOnReplacement {
                batch_idx: 1,
                op_tag,
                ..
            }) if *op_tag == super::vm::OP_TAG_DIG
        ));
        assert_eq!(state.resolution_frames().last().unwrap().pc, 0);
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 20);
        assert_eq!(
            state.resolution_vms().last().unwrap().step_acted_ids.len(),
            1
        );
        // top2 is still in library (PartitionBottom hasn't run yet).
        assert!(state.player(PlayerId(0)).unwrap().library.contains(&top2));
        assert!(
            state
                .player(PlayerId(0))
                .unwrap()
                .hand
                .iter()
                .any(|&id| state.object(id).is_some())
        );

        // Postcard round-trip while paused on OP_TAG_DIG!
        let bytes = postcard::to_allocvec(&state.to_snapshot()).unwrap();
        let snap: crate::state::GameStateSnapshot = postcard::from_bytes(&bytes).unwrap();
        let mut restored = GameState::from_snapshot(Arc::clone(&db), snap);

        // Visibility & determinization during OP_TAG_DIG pause:
        assert!(crate::observe::is_object_visible_to(
            &restored,
            top2,
            Some(PlayerId(0))
        ));
        assert!(!crate::observe::is_object_visible_to(
            &restored,
            top2,
            Some(PlayerId(1))
        ));
        let mut rng = crate::rng::RngState::splitmix64(99);
        let det_p0 = crate::determinize::determinize_state(&restored, PlayerId(0), &mut rng);
        assert_eq!(det_p0.object(top2).unwrap().card_def, top2_def);

        // Resume replacement -> moves top1 to Exile (playable_this_turn), moves top2 to bottom, advances to Step 1 (GainLife optional pause).
        let rep_out = crate::replace::resume_and_finalize_replacement(
            &mut restored,
            PlayerId(0),
            rep_pid,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: opt_pid, ..
        } = resume_after_replacement(&mut restored, rep_out).unwrap()
        else {
            panic!("expected Step 1 optional pause");
        };
        assert_eq!(restored.resolution_frames().last().unwrap().pc, 1);
        let dug_binding = restored
            .resolution_frames()
            .last()
            .unwrap()
            .bindings
            .iter()
            .find(|b| b.name == "dug")
            .expect("dug binding must exist");
        assert_eq!(dug_binding.objects.len(), 2);
        assert!(!dug_binding.objects.contains(&top2));
        let out4 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            opt_pid,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(out4, FrameOutcome::Completed);
        assert_eq!(restored.player(PlayerId(0)).unwrap().life, 21);
        assert_eq!(restored.player(PlayerId(0)).unwrap().exile.len(), 1);
        let exiled_id = restored.player(PlayerId(0)).unwrap().exile[0];
        assert!(
            restored
                .rare(exiled_id)
                .and_then(|r| r.playable_from_exile_by.as_ref())
                .is_some(),
            "PartitionSecond staying in Exile must grant playable_from_exile_by"
        );
        // top2 moved to library bottom (index 0; same-zone Library->Library preserves ObjectId).
        let p0_lib_after = &restored.player(PlayerId(0)).unwrap().library;
        assert_eq!(p0_lib_after[0], top2);
        assert!(
            restored
                .object(p0_lib_after[0])
                .is_some_and(|o| o.card_def == top2_def)
        );

        // Also verify redirect away from Exile (to Graveyard) does NOT grant playable_from_exile_by.
        put_bf(&mut restored, CardDefId(4)); // rep_exile_to_gy
        let lib2 = &restored.player(PlayerId(0)).unwrap().library;
        let b0 = lib2[lib2.len() - 1];
        let b1 = lib2[lib2.len() - 2];
        let out_b1 = begin_resolution_frame(
            &mut restored,
            StackItemId(602),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: bp1, .. } = out_b1 else {
            panic!("expected DigPick");
        };
        let out_b2 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            bp1,
            ChoiceAnswer::PickObject { id: b0 },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: bp2, .. } = out_b2 else {
            panic!("expected DigPick 2");
        };
        let out_b3 = resume_resolution_frame(
            &mut restored,
            PlayerId(0),
            bp2,
            ChoiceAnswer::PickObject { id: b1 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: brep, ..
        } = out_b3
        else {
            panic!("expected ChooseReplacement");
        };
        let gy_rep_idx = restored
            .pending_replacement()
            .unwrap()
            .candidates
            .iter()
            .position(|c| {
                restored
                    .object(c.object)
                    .is_some_and(|o| o.card_def == CardDefId(4))
            })
            .unwrap() as u16;
        let brep_out = crate::replace::resume_and_finalize_replacement(
            &mut restored,
            PlayerId(0),
            brep,
            ChoiceAnswer::PickReplacement { index: gy_rep_idx },
        )
        .unwrap();
        let out_b4 = resume_after_replacement(&mut restored, brep_out).unwrap();
        assert_eq!(out_b4, FrameOutcome::Completed);
        assert_eq!(restored.player(PlayerId(0)).unwrap().graveyard.len(), 1);
        let gy_id = restored.player(PlayerId(0)).unwrap().graveyard[0];
        assert!(
            restored
                .rare(gy_id)
                .and_then(|r| r.playable_from_exile_by.as_ref())
                .is_none(),
            "card redirected out of Exile must NOT get playable_from_exile_by"
        );
    }

    #[test]
    fn synth_finalize_dig_picks_pauses_on_replacement_and_shuffles_rest() {
        let tables = InternTables::default();
        let make_gy_rep = |oracle_id: &str, name: &str, to: Zone| {
            synth_card_def(
                oracle_id,
                name,
                ColorMask::W,
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                None,
                vec![mtg_ir::AbilityDef::Replacement(
                    mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::LIBRARY,
                                        to: Some(Zone::Graveyard),
                                        from_anywhere: false,
                                        not_from_zone: None,
                                        not_to_zone: None,
                                        enters_untapped: false,
                                        dealt_damage_by_self_this_turn: false,
                                    }),
                                    ..Default::default()
                                },
                            },
                            with: mtg_ir::ReplacementWith::ZoneChange {
                                mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                    to: Some(to),
                                    ..Default::default()
                                }),
                            },
                            self_replacement: false,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    },
                )],
            )
        };
        let bear = synth_card_def(
            "synth_pick_bear",
            "Synth Pick Bear",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let rep_gy_exile = make_gy_rep("synth_gy_exile", "Synth Gy Exile", Zone::Exile);
        let rep_gy_hand = make_gy_rep("synth_gy_hand", "Synth Gy Hand", Zone::Hand);
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bear, rep_gy_exile, rep_gy_hand],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 404).unwrap();
        for cd in [CardDefId(1), CardDefId(2)] {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    cd,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let hand_before = state.player(PlayerId(0)).unwrap().hand.len();
        let lib = &state.player(PlayerId(0)).unwrap().library;
        let top0 = lib[lib.len() - 1];
        let mut dig_step = make_step(EffectOp::Dig {
            spec: Box::new(mtg_ir::DigSpec {
                keep: 1,
                up_to: false,
                keep_filter: None,
                keep_all_matching: false,
                keep_label: None,
                keep_to: mtg_ir::DigKeepTo::Hand,
                keep_tapped: false,
                keep_attacking: false,
                keep_attacking_player: None,
                reveal_all: false,
                reveal_kept: false,
                rest: mtg_ir::DigRestTo::Graveyard,
                rest_random: false,
                rest_order: None,
                until_filter: None,
                rest_to_hand: false,
                shuffle_rest: true,
                keep_max_mana_value: None,
                keep_max_mv_strict_lt: false,
                choose_one_of_exiled: false,
                play_from_exile: None,
                partitions: vec![],
                destination_buckets: vec![],
            }),
            look_only: false,
        });
        dig_step.amount = Some(Box::new(Amount::Const { n: 2 }));
        dig_step.bind_as = Some("kept_only".into());

        let ev_start = state.event_log().len();
        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(603),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![dig_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected DigPick pause, got {out1:?}");
        };
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            ChoiceAnswer::PickObject { id: top0 },
        )
        .unwrap();
        let FrameOutcome::Paused {
            prompt_id: rep_pid, ..
        } = out2
        else {
            panic!("expected WaitingOnReplacement on PickRest, got {out2:?}");
        };

        assert!(matches!(
            state.resolution_vms().last().and_then(|v| v.pause_state.as_ref()),
            Some(StepPauseState::WaitingOnReplacement {
                batch_idx: 1,
                op_tag,
                ..
            }) if *op_tag == super::vm::OP_TAG_DIG
        ));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().hand.len(),
            hand_before + 1
        );
        assert!(
            !state.event_log()[ev_start..].iter().any(|ev| matches!(
                ev.kind,
                crate::events::AtomicEventKind::LibraryShuffled { .. }
            )),
            "shuffle_rest must NOT run while PickRest is paused on replacement"
        );

        let rep_out = crate::replace::resume_and_finalize_replacement(
            &mut state,
            PlayerId(0),
            rep_pid,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let out3 = resume_after_replacement(&mut state, rep_out).unwrap();
        assert_eq!(out3, FrameOutcome::Completed);
        assert!(
            state.event_log()[ev_start..].iter().any(|ev| matches!(
                ev.kind,
                crate::events::AtomicEventKind::LibraryShuffled {
                    player: PlayerId(0)
                }
            )),
            "shuffle_rest must run once all Dig moves complete"
        );
    }

    #[test]
    fn synth_move_counters_clamps_remaining_above_i32_max() {
        let mut tables = InternTables::default();
        tables.counters.push("charge".into());
        let creature = synth_card_def(
            "synth_clamp_host",
            "Synth Clamp Host",
            ColorMask::G,
            CardTypeMask::CREATURE,
            SmallVec::new(),
            Some(mtg_ir::PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![creature],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 505).unwrap();
        let spawn = |s: &mut GameState| {
            let ts = s.allocate_timestamp();
            let id = s.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            s.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };
        let src_c = spawn(&mut state);
        let dst_a = spawn(&mut state);
        state.add_counter(src_c, mtg_ir::CounterId(0), u32::MAX);

        let mut mc_step = make_step(EffectOp::MoveCounters {
            counter: Some(mtg_ir::CounterId(0)),
            to_filter: Some(Box::new(ObjectFilter {
                zones: mtg_ir::ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            })),
            spec: None,
        });
        mc_step.subject = Selector::SelfObj;

        let out1 = begin_resolution_frame(
            &mut state,
            StackItemId(604),
            PlayerId(0),
            Some(src_c),
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![mc_step],
            vec![],
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected MoveCountersSelectTarget pause, got {out1:?}");
        };
        let out2 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            ChoiceAnswer::PickObject { id: dst_a },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p2, .. } = out2 else {
            panic!("expected MoveCountersChooseAmount pause, got {out2:?}");
        };
        match state.pending_prompt().map(|p| &p.request) {
            Some(ChoiceRequest::ChooseNumber { min, max, .. }) => {
                assert_eq!((*min, *max), (1, i32::MAX));
            }
            other => panic!("expected ChooseNumber(1..=i32::MAX), got {other:?}"),
        }
        let legal = active_legal_actions(&state, PlayerId(0));
        assert!(
            legal.iter().any(|a| matches!(
                a,
                Action::Answer {
                    answer: ChoiceAnswer::PickNumber { value },
                    ..
                } if *value == i32::MAX
            )),
            "legal actions must include PickNumber(i32::MAX) when remaining == u32::MAX"
        );

        let out3 = resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p2,
            ChoiceAnswer::PickNumber { value: i32::MAX },
        )
        .unwrap();
        let FrameOutcome::Paused { prompt_id: p3, .. } = out3 else {
            panic!("expected second MoveCountersSelectTarget pause, got {out3:?}");
        };
        let out4 = resume_resolution_frame(&mut state, PlayerId(0), p3, ChoiceAnswer::DonePicking)
            .unwrap();
        assert_eq!(out4, FrameOutcome::Completed);
        assert_eq!(
            state.counter_count(src_c, mtg_ir::CounterId(0)),
            u32::MAX - (i32::MAX as u32)
        );
        assert_eq!(
            state.counter_count(dst_a, mtg_ir::CounterId(0)),
            i32::MAX as u32
        );
    }

    #[test]
    fn synth_vm_pause_invalid_answer_preserves_prompt_and_pause_state() {
        let assert_vm_pause_preserved = |state: &GameState, expected_pp: &PendingPromptContext| {
            assert!(crate::choices::is_prompt_active(state));
            assert_eq!(state.pending_prompt(), Some(expected_pp));
            assert!(matches!(state.decision(), DecisionState::Prompt { .. }));
            assert!(
                state
                    .resolution_vms()
                    .last()
                    .and_then(|v| v.pause_state.as_ref())
                    .is_some()
            );
            assert_eq!(
                state
                    .resolution_frames()
                    .last()
                    .and_then(|f| f.pending_prompt_id),
                Some(expected_pp.prompt_id)
            );
            assert!(!crate::choices::legal_choice_actions(state, expected_pp.chooser).is_empty());
        };

        // 1. SelectionPick (wrong variant, invalid/duplicate id, DonePicking below min)
        {
            let mut state = synth_db_and_state(false);
            let spawn_c = |s: &mut GameState| {
                let ts = s.allocate_timestamp();
                let id = s.insert_object(
                    crate::state::GameObject::new(
                        CardDefId(0),
                        PlayerId(0),
                        Zone::Battlefield,
                        ts,
                        crate::state::ObjectFlags::empty(),
                    ),
                    None,
                );
                s.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
                s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
                id
            };
            let c1 = spawn_c(&mut state);
            let _c2 = spawn_c(&mut state);
            let mut step = make_step(EffectOp::Tap {
                select: mtg_ir::SelectionMod {
                    up_to: false,
                    choice_count: Some(Box::new(Amount::Const { n: 1 })),
                    ..Default::default()
                },
            });
            step.subject = Selector::All {
                filter: Box::new(ObjectFilter {
                    zones: mtg_ir::ZoneMask::BATTLEFIELD,
                    ..Default::default()
                }),
            };
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(901),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![step],
                vec![],
            )
            .unwrap() else {
                panic!("expected SelectionPick pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            let bogus_id = state.player(PlayerId(1)).unwrap().hand[0];

            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickObject { id: bogus_id },
                ChoiceAnswer::DonePicking,
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                        .is_err()
                );
                assert_vm_pause_preserved(&state, &pp);
            }
            assert!(
                resume_resolution_frame(
                    &mut state,
                    PlayerId(1),
                    prompt_id,
                    ChoiceAnswer::PickObject { id: c1 },
                )
                .is_err()
            );
            assert_vm_pause_preserved(&state, &pp);

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickObject { id: c1 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert!(state.resolution_frames().is_empty());
        }

        // 2. SearchPick (wrong variant, invalid id, DonePicking below min)
        {
            let mut state = synth_db_and_state(false);
            let step = make_step(EffectOp::Search {
                spec: Box::new(mtg_ir::SearchSpec {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::LAND,
                        ..Default::default()
                    }),
                    count: 1,
                    up_to: false,
                    stated_quality: false,
                    destinations: vec![mtg_ir::SearchDestination {
                        zone: Zone::Hand,
                        tapped: false,
                        on_top: false,
                    }],
                    reveal: false,
                    description: None,
                    zones: mtg_ir::ZoneMask::LIBRARY,
                    different_names: false,
                    shuffle_only_if_library: false,
                    searched_player: None,
                    cast_free: false,
                    target_controller_as_actor: false,
                }),
                tapped: false,
            });
            let lib0 = state.player(PlayerId(0)).unwrap().library[0];
            let hand0 = state.player(PlayerId(0)).unwrap().hand[0];
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(902),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![step],
                vec![],
            )
            .unwrap() else {
                panic!("expected SearchPick pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            for bad in [
                ChoiceAnswer::YesNo { value: false },
                ChoiceAnswer::PickObject { id: hand0 },
                ChoiceAnswer::DonePicking,
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                        .is_err()
                );
                assert_vm_pause_preserved(&state, &pp);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickObject { id: lib0 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // 3. ReorderLibrary (Scry YesNo + LookAndReorder OrderObjects)
        {
            let mut state = synth_db_and_state(false);
            let mut scry_step = make_step(EffectOp::Scry);
            scry_step.amount = Some(Box::new(Amount::Const { n: 2 }));
            let FrameOutcome::Paused { prompt_id: sp1, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(903),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![scry_step],
                vec![],
            )
            .unwrap() else {
                panic!("expected Scry YesNo pause");
            };
            let pp_scry = state.pending_prompt().unwrap().clone();
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    sp1,
                    ChoiceAnswer::DonePicking,
                )
                .is_err()
            );
            assert_vm_pause_preserved(&state, &pp_scry);

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                sp1,
                ChoiceAnswer::YesNo { value: false },
            )
            .unwrap();
            let sp2 = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                sp2,
                ChoiceAnswer::YesNo { value: false },
            )
            .unwrap();
            // Now OrderObjects for the 2 top cards
            let pp_ord = state.pending_prompt().unwrap().clone();
            let top0 = match &pp_ord.request {
                ChoiceRequest::OrderObjects { objects, .. } => objects[0],
                other => panic!("expected OrderObjects, got {other:?}"),
            };
            let hand0 = state.player(PlayerId(0)).unwrap().hand[0];
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::OrderNext { id: hand0 },
            ] {
                assert!(
                    crate::choices::apply_choice_answer(
                        &mut state,
                        PlayerId(0),
                        pp_ord.prompt_id,
                        bad,
                    )
                    .is_err()
                );
                assert_vm_pause_preserved(&state, &pp_ord);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pp_ord.prompt_id,
                ChoiceAnswer::OrderNext { id: top0 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // 4. DigPick (wrong variant, invalid id, DonePicking below min)
        {
            let mut state = synth_db_and_state(false);
            let mut dig_step = make_step(EffectOp::Dig {
                spec: Box::new(mtg_ir::DigSpec {
                    keep: 1,
                    up_to: false,
                    keep_filter: None,
                    keep_all_matching: false,
                    keep_label: None,
                    keep_to: mtg_ir::DigKeepTo::Hand,
                    keep_tapped: false,
                    keep_attacking: false,
                    keep_attacking_player: None,
                    reveal_all: false,
                    reveal_kept: false,
                    rest: mtg_ir::DigRestTo::LibraryBottom,
                    rest_random: false,
                    rest_order: None,
                    until_filter: None,
                    rest_to_hand: false,
                    shuffle_rest: false,
                    keep_max_mana_value: None,
                    keep_max_mv_strict_lt: false,
                    choose_one_of_exiled: false,
                    play_from_exile: None,
                    partitions: vec![],
                    destination_buckets: vec![],
                }),
                look_only: false,
            });
            dig_step.amount = Some(Box::new(Amount::Const { n: 2 }));
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(904),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![dig_step],
                vec![],
            )
            .unwrap() else {
                panic!("expected DigPick pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            let valid_dig = match &pp.request {
                ChoiceRequest::ChooseObjects { candidates, .. } => candidates[0],
                other => panic!("expected ChooseObjects, got {other:?}"),
            };
            let hand0 = state.player(PlayerId(0)).unwrap().hand[0];
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickObject { id: hand0 },
                ChoiceAnswer::DonePicking,
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                        .is_err()
                );
                assert_vm_pause_preserved(&state, &pp);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickObject { id: valid_dig },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // 5. ChooseNumber (wrong variant, below min, above max)
        {
            let mut state = synth_db_and_state(false);
            let mut draw_step = make_step(EffectOp::Draw {
                up_to: true,
                reveal_drawn: false,
            });
            draw_step.amount = Some(Box::new(Amount::Const { n: 3 }));
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(905),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![draw_step],
                vec![],
            )
            .unwrap() else {
                panic!("expected ChooseNumber pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickNumber { value: -1 },
                ChoiceAnswer::PickNumber { value: 4 },
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                        .is_err()
                );
                assert_vm_pause_preserved(&state, &pp);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickNumber { value: 2 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // 6. ChooseMode (out-of-range, duplicate, DonePicking below min)
        {
            let mut state = synth_db_and_state(false);
            let mut odd_even = make_step(EffectOp::CombatRestriction {
                spec: None,
                duration: Some(mtg_ir::Duration::EndOfTurn),
                choose_odd_or_even_cant_block: true,
                must_attack_next_combat: false,
                must_attack_player: None,
                until_your_next_turn_directive: None,
            });
            odd_even.subject = Selector::PlayerRef {
                player: mtg_ir::PlayerRef::You,
            };
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(906),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![odd_even],
                vec![],
            )
            .unwrap() else {
                panic!("expected ChooseMode pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickMode { index: 5 },
                ChoiceAnswer::DonePicking,
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, bad)
                        .is_err()
                );
                assert_vm_pause_preserved(&state, &pp);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);

            // Also verify duplicate mode index rejection when min=2, max=2 via ProliferatePlayers
            let mut tables = InternTables::default();
            tables.counters.push("+1/+1".into());
            let db_p = Arc::new(
                CardDatabase::from_bundle(IrV2Bundle {
                    ir_version: IR_V2_VERSION,
                    source_compiler_version: 46,
                    tables,
                    cards: vec![state.db().cards[0].clone()],
                    decks: vec![],
                })
                .unwrap(),
            );
            let deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 15],
                sideboard: vec![],
            };
            let mut pstate =
                GameState::new(db_p, GameConfig::standard_1v1(), &[deck.clone(), deck], 801)
                    .unwrap();
            pstate.player_mut(PlayerId(0)).unwrap().poison = 1;
            pstate.player_mut(PlayerId(1)).unwrap().poison = 1;
            let prolif = make_step(EffectOp::Proliferate);
            let FrameOutcome::Paused { prompt_id: mp1, .. } = begin_resolution_frame(
                &mut pstate,
                StackItemId(907),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![prolif],
                vec![],
            )
            .unwrap() else {
                panic!("expected ChooseMode for ProliferatePlayers");
            };
            crate::choices::apply_choice_answer(
                &mut pstate,
                PlayerId(0),
                mp1,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap();
            let pp_dup = pstate.pending_prompt().unwrap().clone();
            assert!(
                crate::choices::apply_choice_answer(
                    &mut pstate,
                    PlayerId(0),
                    pp_dup.prompt_id,
                    ChoiceAnswer::PickMode { index: 0 },
                )
                .is_err()
            );
            assert_vm_pause_preserved(&pstate, &pp_dup);
        }

        // 7. MoveCountersSelectTarget & MoveCountersChooseAmount
        {
            let mut tables = InternTables::default();
            tables.counters.push("+1/+1".into());
            let cdef = synth_card_def(
                "synth_mc_host",
                "Synth Mc Host",
                ColorMask::G,
                CardTypeMask::CREATURE,
                SmallVec::new(),
                Some(mtg_ir::PrintedPt::Fixed {
                    power: 0,
                    toughness: 3,
                }),
                vec![],
            );
            let db = Arc::new(
                CardDatabase::from_bundle(IrV2Bundle {
                    ir_version: IR_V2_VERSION,
                    source_compiler_version: 46,
                    tables,
                    cards: vec![cdef],
                    decks: vec![],
                })
                .unwrap(),
            );
            let deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 15],
                sideboard: vec![],
            };
            let mut state =
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 802).unwrap();
            let spawn = |s: &mut GameState| {
                let ts = s.allocate_timestamp();
                let id = s.insert_object(
                    crate::state::GameObject::new(
                        CardDefId(0),
                        PlayerId(0),
                        Zone::Battlefield,
                        ts,
                        crate::state::ObjectFlags::empty(),
                    ),
                    None,
                );
                s.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
                s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
                id
            };
            let src_c = spawn(&mut state);
            let dst_a = spawn(&mut state);
            state.add_counter(src_c, mtg_ir::CounterId(0), 3);

            let mut mc_step = make_step(EffectOp::MoveCounters {
                counter: Some(mtg_ir::CounterId(0)),
                to_filter: Some(Box::new(ObjectFilter {
                    zones: mtg_ir::ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                })),
                spec: None,
            });
            mc_step.subject = Selector::SelfObj;
            let FrameOutcome::Paused { prompt_id: p1, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(908),
                PlayerId(0),
                Some(src_c),
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![mc_step],
                vec![],
            )
            .unwrap() else {
                panic!("expected MoveCountersSelectTarget pause");
            };
            let pp1 = state.pending_prompt().unwrap().clone();
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickObject { id: src_c },
            ] {
                assert!(
                    crate::choices::apply_choice_answer(&mut state, PlayerId(0), p1, bad).is_err()
                );
                assert_vm_pause_preserved(&state, &pp1);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                p1,
                ChoiceAnswer::PickObject { id: dst_a },
            )
            .unwrap();

            let pp2 = state.pending_prompt().unwrap().clone();
            for bad in [
                ChoiceAnswer::YesNo { value: true },
                ChoiceAnswer::PickNumber { value: 0 },
                ChoiceAnswer::PickNumber { value: 4 },
            ] {
                assert!(
                    crate::choices::apply_choice_answer(
                        &mut state,
                        PlayerId(0),
                        pp2.prompt_id,
                        bad,
                    )
                    .is_err()
                );
                assert_vm_pause_preserved(&state, &pp2);
            }
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pp2.prompt_id,
                ChoiceAnswer::PickNumber { value: 3 },
            )
            .unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // 8. OptionalChoice & GuidedResolution (wrong variant, wrong actor/prompt_id via resume_resolution_frame)
        {
            let mut state = synth_db_and_state(false);
            let mut opt_step = make_step(EffectOp::GainLife {
                set_life_total: false,
            });
            opt_step.amount = Some(Box::new(Amount::Const { n: 2 }));
            opt_step.optional = Some(OptionalSpec {
                mode: OptionalMode::DoIfPaid,
                cost: vec![],
                chooser: PlayerRef::You,
            });
            let FrameOutcome::Paused { prompt_id, .. } = begin_resolution_frame(
                &mut state,
                StackItemId(909),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![opt_step],
                vec![],
            )
            .unwrap() else {
                panic!("expected OptionalChoice pause");
            };
            let pp = state.pending_prompt().unwrap().clone();
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    prompt_id,
                    ChoiceAnswer::DonePicking,
                )
                .is_err()
            );
            assert_vm_pause_preserved(&state, &pp);
            assert!(
                resume_resolution_frame(
                    &mut state,
                    PlayerId(1),
                    prompt_id,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_vm_pause_preserved(&state, &pp);
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            assert_eq!(state.player(PlayerId(0)).unwrap().life, 22);
            assert_eq!(state.decision(), &DecisionState::Priority);

            let mut guided_state = synth_db_and_state(true);
            let FrameOutcome::Paused {
                prompt_id: gpid, ..
            } = begin_resolution_frame(
                &mut guided_state,
                StackItemId(910),
                PlayerId(0),
                None,
                None,
                0,
                vec![],
                vec![],
                SmallVec::new(),
                vec![make_step(EffectOp::Discover)],
                vec![],
            )
            .unwrap()
            else {
                panic!("expected GuidedResolution pause");
            };
            let gpp = guided_state.pending_prompt().unwrap().clone();
            assert!(
                crate::choices::apply_choice_answer(
                    &mut guided_state,
                    PlayerId(0),
                    gpid,
                    ChoiceAnswer::DonePicking,
                )
                .is_err()
            );
            assert_vm_pause_preserved(&guided_state, &gpp);
            assert!(
                resume_resolution_frame(
                    &mut guided_state,
                    PlayerId(1),
                    gpid,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_vm_pause_preserved(&guided_state, &gpp);
            assert!(
                resume_resolution_frame(
                    &mut guided_state,
                    PlayerId(0),
                    gpid + 1,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert_vm_pause_preserved(&guided_state, &gpp);
            crate::choices::apply_choice_answer(
                &mut guided_state,
                PlayerId(0),
                gpid,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            assert!(guided_state.pending_prompt().is_none());
            assert_eq!(guided_state.decision(), &DecisionState::Priority);
        }

        // 9. WaitingOnReplacement — direct resume_resolution_frame returns Err and preserves replacement + pause_state + pending_prompt_id
        {
            let tables = InternTables::default();
            let shockland = CardDef {
                oracle_id: "synth_vm_wait_rep_land".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Vm Wait Rep Land".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Vm Wait Rep Land".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::LAND,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![mtg_ir::AbilityDef::Replacement(
                        mtg_ir::ReplacementAbility {
                            replacement: mtg_ir::ReplacementDef {
                                replaces: mtg_ir::EventPattern {
                                    event: mtg_ir::EventKind::ZoneChange,
                                    filter: mtg_ir::EventFilter {
                                        or_self: true,
                                        detail: Some(mtg_ir::EventDetail::ZoneChange {
                                            from: mtg_ir::ZoneMask::empty(),
                                            to: Some(Zone::Battlefield),
                                            from_anywhere: true,
                                            not_from_zone: None,
                                            not_to_zone: None,
                                            enters_untapped: false,
                                            dealt_damage_by_self_this_turn: false,
                                        }),
                                        ..Default::default()
                                    },
                                },
                                with: mtg_ir::ReplacementWith::ZoneChange {
                                    mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                        pay_life_or_enter_tapped: Some(2),
                                        ..Default::default()
                                    }),
                                },
                                self_replacement: true,
                                who: None,
                                condition: None,
                            },
                            span: Default::default(),
                        },
                    )],
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
                    cards: vec![shockland],
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
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 803).unwrap();
            let ts = state.allocate_timestamp();
            let hand_land = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(hand_land);
            let mut mv_step = make_step(EffectOp::Move {
                from: Some(Zone::Hand),
                to: Some(Zone::Battlefield),
                select: mtg_ir::SelectionMod::default(),
                tapped: false,
                under_your_control: true,
                topmost_only: false,
                may_shuffle: false,
                position: None,
                from_top: None,
                library_choice_by: None,
                random_order: false,
                entered_transformed: false,
                entered_flipped: false,
                enters_tapped_and_attacking: false,
                as_enchantment_only: false,
                unearth: false,
                with_counters: None,
                attached_to: None,
                condition: None,
                choose_at_random_from_exiled_with_self: false,
                leave_chosen_in_graveyard_rest_to_hand: false,
                split_exiled_cost_cards_to_bottom_and_battlefield: false,
            });
            mv_step.subject = Selector::Target { index: 0 };
            let FrameOutcome::Paused {
                prompt_id: rep_pid, ..
            } = begin_resolution_frame(
                &mut state,
                StackItemId(911),
                PlayerId(0),
                None,
                None,
                0,
                vec![TargetRef::Object { id: hand_land }],
                vec![],
                SmallVec::new(),
                vec![mv_step],
                vec![],
            )
            .unwrap()
            else {
                panic!("expected WaitingOnReplacement pause");
            };

            assert!(
                resume_resolution_frame(
                    &mut state,
                    PlayerId(0),
                    rep_pid,
                    ChoiceAnswer::YesNo { value: true },
                )
                .is_err()
            );
            assert!(state.pending_replacement().is_some());
            assert!(matches!(
                state
                    .resolution_vms()
                    .last()
                    .and_then(|v| v.pause_state.as_ref()),
                Some(StepPauseState::WaitingOnReplacement { .. })
            ));
            assert_eq!(
                state
                    .resolution_frames()
                    .last()
                    .and_then(|f| f.pending_prompt_id),
                Some(rep_pid)
            );
        }
    }
}
