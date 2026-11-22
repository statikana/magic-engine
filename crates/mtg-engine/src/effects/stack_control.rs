use mtg_ir::{
    CardTypeMask, CastFreeSpec, ColorMask, Condition, CounterReplacementZone,
    DelayedInlineDuration, DelayedInlineTriggerSpec, DelayedTriggerPayload, Duration, EffectStep,
    GraveyardCastGrantSpec, LoopPlayersKind, ManaColor, ObjectFilter, PlayFromExileDuration,
    PlayFromExileSpec, PlayerFilter, PlayerRelation, PlayerRuleKind, PunisherPlayersKind,
    PunisherSpec, RedirectTargetsSpec, RuleMod, SelectionMod, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{ChoiceRequest, DecisionState, TargetRef};
use crate::eval::{evaluate_amount, evaluate_condition, resolve_player_ref};
use crate::events::{AtomicEventKind, emit_event};
use crate::filter::find_objects;
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::layers::predicates::PlayerContinuousEffect;
use crate::state::{
    ControlFlowFrame, DelayedSpellCopyGrant, DirtyMask, EngineError, GameState, ModePausePurpose,
    PendingPromptContext, PlayableFromExileState, PromptOwner, StackEntryKind, StepPauseState,
    TOKEN_CARD_DEF_ID, YesNoPausePurpose,
};
use crate::triggers::{self, DelayedExpiry, DelayedTriggerEntry};
use crate::zones;

use super::vm::{
    SelectionResult, StepExecOutcome, allocate_continuous_effect_id, can_pay_cost_list,
    pay_cost_list_sync, rare_turn_state_mut, resolve_selection_mod, resolve_selector_objects,
    resolve_selector_targets, resolve_step_actor, resolve_target_players, snapshot_for_binding,
    vm_controller, vm_source, with_vm_eval,
};

fn resolve_stack_item_for_target(state: &GameState, tref: TargetRef) -> Option<StackItemId> {
    match tref {
        TargetRef::Stack { id } => Some(id),
        TargetRef::Object { id } => state.stack_entries().iter().find_map(|se| match se.kind {
            StackEntryKind::Spell { object } if object == id => Some(se.id),
            _ => None,
        }),
        TargetRef::Player { .. } => None,
    }
}

pub(crate) fn exec_copy_spell(
    state: &mut GameState,
    step: &EffectStep,
    count: Option<&mtg_ir::Amount>,
    _may_choose_new_targets: bool,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let n = count
        .map(|a| with_vm_eval(state, |ctx, _| evaluate_amount(state, a, ctx).max(0) as u32))
        .unwrap_or(1);
    if n == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let targets = resolve_selector_targets(state, &step.subject);
    let stack_id = targets
        .iter()
        .find_map(|&t| resolve_stack_item_for_target(state, t))
        .or_else(|| {
            let self_id = state.resolution_vms().last().map(|v| v.stack_item);
            state
                .stack_entries()
                .iter()
                .rev()
                .find(|se| Some(se.id) != self_id)
                .map(|se| se.id)
        });
    let Some(sid) = stack_id else {
        return Ok(StepExecOutcome::Done);
    };
    for _ in 0..n {
        let _new_sid = crate::stack::copy_stack_item(state, sid, ctrl, None)?;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_counter_spell(
    state: &mut GameState,
    step: &EffectStep,
    replacement_zone: Option<CounterReplacementZone>,
    unless_pays_generic: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let targets = resolve_selector_targets(state, &step.subject);
    let stack_ids: Vec<StackItemId> = targets
        .into_iter()
        .filter_map(|t| resolve_stack_item_for_target(state, t))
        .collect();
    if stack_ids.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    if let Some(amt) = unless_pays_generic {
        let generic = with_vm_eval(state, |ctx, _| {
            evaluate_amount(state, amt, ctx).max(0) as u32
        });
        let target_ctrl = state
            .stack_entries()
            .iter()
            .find(|se| se.id == stack_ids[0])
            .map_or(ctrl, |se| se.controller);
        let cost = mtg_ir::ManaCost {
            generic,
            ..Default::default()
        };
        let spend_ctx = crate::mana::ManaSpendContext::for_generic_cost(target_ctrl);
        if generic > 0 && crate::mana::can_pay_mana_from_pool(state, target_ctrl, &cost, &spend_ctx)
        {
            let prompt_id = state.allocate_prompt_id();
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pause_state = Some(StepPauseState::ChooseNumber {
                    chooser: target_ctrl,
                    purpose: crate::state::NumberPausePurpose::CounterSpellUnlessPays,
                });
            }
            if let Some(fr) = state.resolution_frames_mut().last_mut() {
                fr.pending_prompt_id = Some(prompt_id);
            }
            let req = ChoiceRequest::ChooseNumber {
                prompt_id,
                min: 0,
                max: 1,
            };
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: target_ctrl,
                request: req.clone(),
                owner: PromptOwner::ResolutionFrame,
            }));
            state.set_decision(DecisionState::Prompt { request: req });
            state.set_priority_player(target_ctrl);
            return Ok(StepExecOutcome::Paused {
                prompt_id,
                chooser: target_ctrl,
            });
        }
    }

    for sid in stack_ids {
        crate::stack::counter_stack_item(state, sid, replacement_zone, ctrl, src)?;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn resume_counter_unless_pays(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    choice_val: i32,
    replacement_zone: Option<CounterReplacementZone>,
    unless_pays_generic: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    if choice_val > 0
        && let Some(amt) = unless_pays_generic
    {
        let generic = with_vm_eval(state, |ctx, _| {
            evaluate_amount(state, amt, ctx).max(0) as u32
        });
        let cost = mtg_ir::ManaCost {
            generic,
            ..Default::default()
        };
        let spend_ctx = crate::mana::ManaSpendContext::for_generic_cost(chooser);
        if crate::mana::can_pay_mana_from_pool(state, chooser, &cost, &spend_ctx) {
            crate::mana::deduct_mana_for_context(state, chooser, &cost, &spend_ctx, &[])?;
            return Ok(StepExecOutcome::Done);
        }
    }
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    for t in resolve_selector_targets(state, &step.subject) {
        if let Some(sid) = resolve_stack_item_for_target(state, t) {
            crate::stack::counter_stack_item(state, sid, replacement_zone, ctrl, src)?;
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_redirect_targets(
    state: &mut GameState,
    step: &EffectStep,
    spec: &RedirectTargetsSpec,
) -> Result<StepExecOutcome, EngineError> {
    let targets = resolve_selector_targets(state, &step.subject);
    let Some(sid) = targets
        .into_iter()
        .find_map(|t| resolve_stack_item_for_target(state, t))
    else {
        return Ok(StepExecOutcome::Done);
    };
    if spec.force_target_self
        && let Some(src) = vm_source(state)
    {
        let new_t = TargetRef::Object { id: src };
        if let Some(se) = state.stack_entries_mut().iter_mut().find(|e| e.id == sid) {
            se.targets.fill(new_t);
            for vec_t in &mut se.choices.targets_by_spec {
                vec_t.fill(new_t);
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_cast_name_lock(
    state: &mut GameState,
    step: &EffectStep,
    _name_ref: &mtg_ir::Ref,
    duration: Option<Duration>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let chosen_name = src.and_then(|s| state.rare(s).and_then(|r| r.chosen_card_name.clone()));
    let id = allocate_continuous_effect_id(state);
    let ts = state.allocate_timestamp();
    let dur = duration.unwrap_or(Duration::EndOfTurn);
    let (expires_turn, expires_on_player) = crate::state::compute_timed_expiry(state, dur, ctrl);
    state.add_player_continuous_effect(PlayerContinuousEffect {
        id,
        source: src,
        controller: ctrl,
        who: PlayerFilter {
            relation: PlayerRelation::Any,
        },
        rule: RuleMod::CantCastChosenName,
        duration: dur,
        timestamp: ts,
        expires_turn,
        expires_on_player,
        chosen_card_name: chosen_name,
        chosen_card_type: None,
        linked_exile_names: SmallVec::new(),
    });
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_for_each_player_loop(
    state: &mut GameState,
    step: &EffectStep,
    kind: LoopPlayersKind,
    steps: &[EffectStep],
    choose_random_opponent: bool,
    exclude_event_player: bool,
) -> Result<StepExecOutcome, EngineError> {
    if steps.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let ctrl = resolve_step_actor(state, step);
    let ev_player = super::vm::active_event_player(state);
    let total = state.players().len();
    if total == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let start_seat = usize::from(ctrl.0) % total;
    let mut ordered_alive: Vec<PlayerId> = Vec::with_capacity(total);
    for k in 0..total {
        let p = &state.players()[(start_seat + k) % total];
        if !p.lost {
            ordered_alive.push(p.id);
        }
    }

    let mut players: Vec<PlayerId> = match kind {
        LoopPlayersKind::EachPlayer | LoopPlayersKind::TurnOrderStartingWithYou => ordered_alive,
        LoopPlayersKind::EachOpponent | LoopPlayersKind::EachOtherPlayer => ordered_alive
            .into_iter()
            .filter(|&pid| pid != ctrl)
            .collect(),
        LoopPlayersKind::TurnOrderStartingWithNextOpponent => {
            let mut v: Vec<PlayerId> = ordered_alive
                .into_iter()
                .filter(|&pid| pid != ctrl)
                .collect();
            v.push(ctrl);
            v
        }
        LoopPlayersKind::ChooseOpponent | LoopPlayersKind::ChooseRandomOpponent => {
            let opps: Vec<PlayerId> = ordered_alive
                .into_iter()
                .filter(|&pid| pid != ctrl)
                .collect();
            if opps.is_empty() {
                Vec::new()
            } else if choose_random_opponent
                || matches!(kind, LoopPlayersKind::ChooseRandomOpponent)
            {
                let idx = state.rng_mut().next_index(opps.len());
                vec![opps[idx]]
            } else {
                vec![opps[0]]
            }
        }
    };

    if exclude_event_player && let Some(ep) = ev_player {
        players.retain(|&pid| pid != ep);
    }
    if players.is_empty() {
        return Ok(StepExecOutcome::Done);
    }

    let first = players[0];
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.each_loop_player = Some(first);
        if matches!(
            kind,
            LoopPlayersKind::ChooseOpponent | LoopPlayersKind::ChooseRandomOpponent
        ) {
            vm.chosen_opponent = Some(first);
        }
        vm.subframes.push(ControlFlowFrame::ForEachPlayer {
            players,
            next_player_idx: 0,
            steps: steps.to_vec(),
            step_pc: 0,
        });
    }
    Ok(StepExecOutcome::EnterSubframe)
}

pub(crate) fn exec_conditional_sequence(
    state: &mut GameState,
    condition: &Condition,
    then_steps: &[EffectStep],
    else_steps: &[EffectStep],
) -> Result<StepExecOutcome, EngineError> {
    let cond_ok = with_vm_eval(state, |ctx, _| evaluate_condition(state, condition, ctx));
    let chosen = if cond_ok { then_steps } else { else_steps };
    if chosen.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.subframes.push(ControlFlowFrame::SubSteps {
            steps: chosen.to_vec(),
            step_pc: 0,
            event_object: None,
            event_player: None,
            event_amount: None,
        });
    }
    Ok(StepExecOutcome::EnterSubframe)
}

pub(crate) fn exec_repeat_sequence(
    state: &mut GameState,
    steps: &[EffectStep],
    condition: &Condition,
    max_iterations: Option<u16>,
) -> Result<StepExecOutcome, EngineError> {
    if steps.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.subframes.push(ControlFlowFrame::RepeatLoop {
            steps: steps.to_vec(),
            step_pc: 0,
            iterations: 0,
            max_iterations: max_iterations.unwrap_or(64),
            condition: condition.clone(),
        });
    }
    Ok(StepExecOutcome::EnterSubframe)
}

pub(crate) fn exec_punisher_choice(
    state: &mut GameState,
    step: &EffectStep,
    spec: &PunisherSpec,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let chooser = match spec.players {
        Some(PunisherPlayersKind::EachOpponent) => state
            .players()
            .iter()
            .find(|p| !p.lost && p.id != ctrl)
            .map_or(ctrl, |p| p.id),
        Some(PunisherPlayersKind::SubjectPlayer) => resolve_target_players(state, step)
            .first()
            .copied()
            .unwrap_or(ctrl),
        _ => with_vm_eval(state, |ctx, _| {
            resolve_player_ref(state, &spec.unless_who, ctx).unwrap_or(ctrl)
        }),
    };
    let src = vm_source(state).unwrap_or(crate::state::NONE_OBJECT_ID);
    let x_val = state.resolution_vms().last().map_or(0, |v| v.x_value);
    let can_pay = spec.unless_cost.is_empty()
        || can_pay_cost_list(state, chooser, src, &spec.unless_cost, x_val);
    if !can_pay {
        if spec.default_steps.is_empty() {
            return Ok(StepExecOutcome::Done);
        }
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.subframes.push(ControlFlowFrame::SubSteps {
                steps: spec.default_steps.clone(),
                step_pc: 0,
                event_object: None,
                event_player: Some(chooser),
                event_amount: None,
            });
        }
        return Ok(StepExecOutcome::EnterSubframe);
    }

    let options = vec![
        "Decline cost (default effect occurs)".into(),
        "Pay cost to prevent / choose alternative".into(),
    ];
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::ChooseMode {
            chooser,
            purpose: ModePausePurpose::PunisherChoice,
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
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn resume_punisher_choice(
    state: &mut GameState,
    spec: &PunisherSpec,
    chooser: PlayerId,
    picked_index: u16,
) -> Result<StepExecOutcome, EngineError> {
    let sub_steps = if picked_index == 1 {
        let src = vm_source(state).unwrap_or(crate::state::NONE_OBJECT_ID);
        let x_val = state.resolution_vms().last().map_or(0, |v| v.x_value);
        if !spec.unless_cost.is_empty() {
            pay_cost_list_sync(state, chooser, src, &spec.unless_cost, x_val)?;
        }
        spec.unless_effect.clone()
    } else {
        spec.default_steps.clone()
    };
    if sub_steps.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.subframes.push(ControlFlowFrame::SubSteps {
            steps: sub_steps,
            step_pc: 0,
            event_object: None,
            event_player: Some(chooser),
            event_amount: None,
        });
    }
    Ok(StepExecOutcome::EnterSubframe)
}

pub(crate) fn exec_partition_keep_by_types(
    state: &mut GameState,
    step: &EffectStep,
    types: CardTypeMask,
    keep_count_per_player: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    let keep_n = keep_count_per_player
        .map(|a| {
            with_vm_eval(state, |ctx, _| {
                evaluate_amount(state, a, ctx).max(0) as usize
            })
        })
        .unwrap_or(1);
    let players = resolve_target_players(state, step);
    let mut to_sac = Vec::new();
    {
        let cache = state.evaluate_board();
        for pid in players {
            let Some(p) = state.player(pid) else { continue };
            let matching: Vec<ObjectId> = p
                .battlefield
                .iter()
                .copied()
                .filter(|&id| {
                    cache.get(id).is_some_and(|e| {
                        types.is_empty() || !e.types.intersection(types).is_empty()
                    })
                })
                .collect();
            if matching.len() > keep_n {
                to_sac.extend(matching.into_iter().skip(keep_n));
            }
        }
    }
    for id in to_sac {
        let _ = zones::move_object(state, id, Zone::Graveyard, Default::default(), None)?;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_delayed_trigger(
    state: &mut GameState,
    step: &EffectStep,
    delayed: Option<&DelayedTriggerPayload>,
    inline_delayed: Option<&DelayedInlineTriggerSpec>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let src_lki = src.and_then(|s| snapshot_for_binding(state, s));
    let card_def = src
        .and_then(|s| state.object(s).map(|o| o.card_def))
        .unwrap_or(TOKEN_CARD_DEF_ID);
    let watched: SmallVec<[ObjectId; 4]> = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .collect();
    let x_value = state.resolution_vms().last().map_or(0, |v| v.x_value);
    let bindings = state
        .resolution_frames()
        .last()
        .map(|f| f.bindings.clone())
        .unwrap_or_default();

    if let Some(payload) = delayed {
        let expiry = if payload.next_turn {
            DelayedExpiry::NextUpkeep {
                target_player: ctrl,
                min_turn: state.turn_number() + 1,
            }
        } else {
            match payload.trigger.spell_delayed {
                Some(mtg_ir::SpellDelayedDuration::NextThisTurn) => DelayedExpiry::OneShot,
                Some(mtg_ir::SpellDelayedDuration::ThisTurn) => DelayedExpiry::ThisTurn,
                Some(mtg_ir::SpellDelayedDuration::NextUpkeep) => DelayedExpiry::NextUpkeep {
                    target_player: ctrl,
                    min_turn: state.turn_number() + 1,
                },
                Some(mtg_ir::SpellDelayedDuration::UntilYourNextTurn) => {
                    DelayedExpiry::UntilYourNextTurn {
                        creator: ctrl,
                        created_turn: state.turn_number(),
                    }
                }
                None => DelayedExpiry::ThisTurn,
            }
        };
        triggers::register_delayed_trigger(
            state,
            DelayedTriggerEntry {
                id: 0,
                controller: ctrl,
                source_card: card_def,
                source_object: src,
                source_lki: src_lki.clone(),
                trigger: (*payload.trigger).clone(),
                condition: None,
                targets: payload.targets.clone(),
                effect: payload.effect.clone(),
                expiry,
                watched_objects: watched.clone(),
                x_value,
                bindings: bindings.clone(),
            },
        );
    } else if let Some(inline) = inline_delayed {
        let extracted = src
            .and_then(|sid| {
                state
                    .object(sid)
                    .map(|obj| (obj.card_def, usize::from(obj.active_face_index())))
            })
            .and_then(|(card_def, face_idx)| {
                let card = state.db().get(card_def)?;
                let face = card.faces.get(face_idx)?;
                let idx = usize::from(inline.sub_trigger_index.unwrap_or(0));
                let mtg_ir::AbilityDef::Triggered(tr) = face.abilities.get(idx)? else {
                    return None;
                };
                Some((
                    card_def,
                    tr.trigger.clone(),
                    tr.condition.as_deref().cloned(),
                    tr.targets.clone(),
                    tr.effect.clone(),
                ))
            });
        if let Some((card_def, trigger, condition, targets, effect)) = extracted {
            let expiry = match inline.duration {
                DelayedInlineDuration::ThisTurn => DelayedExpiry::ThisTurn,
                DelayedInlineDuration::UntilYourNextTurn => DelayedExpiry::UntilYourNextTurn {
                    creator: ctrl,
                    created_turn: state.turn_number(),
                },
                DelayedInlineDuration::NextCombat => DelayedExpiry::NextCombat { creator: ctrl },
            };
            triggers::register_delayed_trigger(
                state,
                DelayedTriggerEntry {
                    id: 0,
                    controller: ctrl,
                    source_card: card_def,
                    source_object: src,
                    source_lki: src_lki,
                    trigger,
                    condition,
                    targets,
                    effect,
                    expiry,
                    watched_objects: watched,
                    x_value,
                    bindings,
                },
            );
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_delayed_tap_mana_trigger(
    state: &mut GameState,
    step: &EffectStep,
    all_players: bool,
    land_subtype: Option<mtg_ir::SubtypeId>,
    mana: &[ManaColor],
    to_tapping_player: bool,
) -> Result<StepExecOutcome, EngineError> {
    if mana.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let src_lki = src.and_then(|s| snapshot_for_binding(state, s));
    let card_def = src
        .and_then(|s| state.object(s).map(|o| o.card_def))
        .unwrap_or(TOKEN_CARD_DEF_ID);

    let mut subtypes_any = SmallVec::new();
    if let Some(st) = land_subtype {
        subtypes_any.push(st);
    }
    let land_filter = ObjectFilter {
        zones: ZoneMask::BATTLEFIELD,
        types_any: CardTypeMask::LAND,
        subtypes_any,
        ..Default::default()
    };

    let trigger = mtg_ir::TriggerDef {
        event: mtg_ir::EventKind::TappedForMana,
        filter: mtg_ir::EventFilter {
            object: Some(mtg_ir::TriggerSubject::Filter {
                filter: Box::new(land_filter),
            }),
            player: (!all_players).then_some(PlayerFilter {
                relation: PlayerRelation::You,
            }),
            ..Default::default()
        },
        scope: None,
        from_zones: None,
        only_once_per_turn: false,
        do_this_once_per_turn: false,
        is_state_trigger: false,
        or: Vec::new(),
        chapter_numbers: SmallVec::new(),
        is_chapter_ability: false,
        spell_delayed: None,
    };

    let effect = vec![EffectStep {
        op: mtg_ir::EffectOp::AddMana {
            production: mtg_ir::ManaProduction::Fixed {
                symbols: mana.iter().copied().collect(),
            },
            restriction: None,
            restriction_spec: None,
            retain_until_eot: false,
            delayed_rider: None,
        },
        subject: mtg_ir::Selector::None,
        actor: to_tapping_player.then_some(mtg_ir::PlayerRef::EventPlayer),
        amount: None,
        optional: None,
        gate: None,
        bind_as: None,
        do_this_once_per_turn: false,
        reflexive_trigger: None,
        span: mtg_ir::Span::default(),
    }];

    triggers::register_delayed_trigger(
        state,
        DelayedTriggerEntry {
            id: 0,
            controller: ctrl,
            source_card: card_def,
            source_object: src,
            source_lki: src_lki,
            trigger,
            condition: None,
            targets: Vec::new(),
            effect,
            expiry: DelayedExpiry::ThisTurn,
            watched_objects: SmallVec::new(),
            x_value: 0,
            bindings: Vec::new(),
        },
    );
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_delayed_next_spell_copy(
    state: &mut GameState,
    step: &EffectStep,
    types: CardTypeMask,
    not_types: CardTypeMask,
    colors: ColorMask,
    copy_count: u16,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let filter = ObjectFilter {
        types_any: types,
        types_none: not_types,
        colors_any: colors,
        ..Default::default()
    };
    let expires_turn = state.turn_number();
    rare_turn_state_mut(state)
        .pending_delayed_next_spell_copies
        .push(DelayedSpellCopyGrant {
            controller: ctrl,
            source: src,
            count: copy_count,
            filter: Some(filter),
            expires_turn,
        });
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_grant_player(
    state: &mut GameState,
    step: &EffectStep,
    rules: &[PlayerRuleKind],
    duration: Option<Duration>,
    extra_land_plays: Option<u16>,
    next_main_phase_add_mana: Option<&(ManaColor, Box<mtg_ir::Amount>)>,
) -> Result<StepExecOutcome, EngineError> {
    let src = vm_source(state);
    let players = resolve_target_players(state, step);
    let dur = duration.unwrap_or(Duration::EndOfTurn);

    for pid in &players {
        if let Some(extra) = extra_land_plays
            && let Some(p) = state.player_mut(*pid)
        {
            p.max_lands_per_turn = p.max_lands_per_turn.saturating_add(extra as u8);
            state.mark_dirty(DirtyMask::PLAYER_STATE);
        }
        if let Some((mc, amt)) = next_main_phase_add_mana {
            let n = with_vm_eval(state, |ctx, _| {
                evaluate_amount(state, amt, ctx).max(0) as u32
            });
            if n > 0 {
                rare_turn_state_mut(state)
                    .pending_next_main_phase_mana
                    .push((*pid, *mc, n));
            }
        }
    }

    for &rule in rules {
        match rule {
            PlayerRuleKind::NoMaxHandSizeRestOfGame => {
                for &pid in &players {
                    if let Some(p) = state.player_mut(pid) {
                        p.no_max_hand_size_rest_of_game = true;
                    }
                }
                state.mark_dirty(DirtyMask::PLAYER_STATE);
            }
            PlayerRuleKind::ExtraLandPlay => {
                for &pid in &players {
                    if let Some(p) = state.player_mut(pid) {
                        p.max_lands_per_turn = p.max_lands_per_turn.saturating_add(1);
                    }
                }
                state.mark_dirty(DirtyMask::PLAYER_STATE);
            }
            PlayerRuleKind::DamageCantBePrevented => {
                state.set_damage_cant_be_prevented_this_turn(false);
            }
            PlayerRuleKind::CombatDamageCantBePrevented => {
                state.set_damage_cant_be_prevented_this_turn(true);
            }
            PlayerRuleKind::ProtectionFromEverything => {
                for &pid in &players {
                    if let Some(p) = state.player_mut(pid) {
                        p.protection_from_everything = Some(dur);
                    }
                }
                state.mark_dirty(DirtyMask::PLAYER_STATE);
            }
            _ => {
                let rm = match rule {
                    PlayerRuleKind::LifeTotalCantChange => Some(RuleMod::LifeTotalCantChange),
                    PlayerRuleKind::CannotLoseGame => Some(RuleMod::CantLose),
                    PlayerRuleKind::Hexproof => Some(RuleMod::PlayerHexproof),
                    PlayerRuleKind::OpponentsCantCastThisTurn => {
                        Some(RuleMod::OpponentsCantCastDuringYourTurn { spells_only: true })
                    }
                    PlayerRuleKind::CastAsThoughFlashThisTurn => Some(RuleMod::CastAsThoughFlash {
                        spells: None,
                        self_only: false,
                        first_matching_this_turn: false,
                        sacrifice_at_next_cleanup_if_non_sorcery: false,
                    }),
                    _ => None,
                };
                if let Some(rule_mod) = rm {
                    for &pid in &players {
                        let id = allocate_continuous_effect_id(state);
                        let ts = state.allocate_timestamp();
                        let (expires_turn, expires_on_player) =
                            crate::state::compute_timed_expiry(state, dur, pid);
                        state.add_player_continuous_effect(PlayerContinuousEffect {
                            id,
                            source: src,
                            controller: pid,
                            who: PlayerFilter {
                                relation: PlayerRelation::You,
                            },
                            rule: rule_mod.clone(),
                            duration: dur,
                            timestamp: ts,
                            expires_turn,
                            expires_on_player,
                            chosen_card_name: None,
                            chosen_card_type: None,
                            linked_exile_names: SmallVec::new(),
                        });
                    }
                }
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_grant_play_from_exile_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    spec: &PlayFromExileSpec,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let expires_turn = match spec.duration {
        PlayFromExileDuration::ThisTurn => Some(state.turn_number()),
        PlayFromExileDuration::UntilEndOfYourNextTurn
        | PlayFromExileDuration::UntilYourNextEndStep => Some(state.turn_number() + 1),
        PlayFromExileDuration::AsLongAsExiled => None,
    };
    let alt_cost = if spec.without_paying_mana_cost {
        Some(mtg_ir::ManaCost::default())
    } else {
        spec.alternative_mana_cost.clone()
    };
    for id in targets {
        if state.object(id).is_some_and(|o| o.zone == Zone::Exile)
            && let Some(r) = state.rare_or_default(id)
        {
            r.playable_from_exile_by = Some(PlayableFromExileState {
                player: actor,
                expires_turn,
                cast_with_mana_cost: alt_cost.clone(),
            });
        }
    }
    state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_grant_play_from_exile(
    state: &mut GameState,
    step: &EffectStep,
    spec: &PlayFromExileSpec,
    select: &SelectionMod,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let targets = match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::EXILE)? {
        SelectionResult::Paused { prompt_id, chooser } => {
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        SelectionResult::Ready(v) => v,
    };
    run_grant_play_from_exile_targets(state, step, targets, spec)
}

pub(crate) fn exec_grant_graveyard_cast(
    state: &mut GameState,
    step: &EffectStep,
    spec: Option<&GraveyardCastGrantSpec>,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let targets = resolve_selector_objects(state, &step.subject);
    let exp = if spec.is_some_and(|s| s.until_end_of_next_turn) {
        Some(state.turn_number() + 1)
    } else {
        Some(state.turn_number())
    };
    let alt = spec.and_then(|s| {
        if s.without_paying_mana_cost {
            Some(mtg_ir::ManaCost::default())
        } else {
            s.alternative_mana_cost.clone()
        }
    });
    for id in targets {
        if state.object(id).is_some_and(|o| o.zone == Zone::Graveyard)
            && let Some(r) = state.rare_or_default(id)
        {
            r.playable_from_exile_by = Some(PlayableFromExileState {
                player: actor,
                expires_turn: exp,
                cast_with_mana_cost: alt.clone(),
            });
        }
    }
    state.mark_dirty(DirtyMask::GRAVEYARDS | DirtyMask::PLAYER_STATE);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_cast_free(
    state: &mut GameState,
    step: &EffectStep,
    spec: Option<&CastFreeSpec>,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let mut cands = resolve_selector_objects(state, &step.subject);
    if cands.is_empty()
        && let Some(sp) = spec
        && let Some(f) = sp.filter.as_deref()
    {
        cands = with_vm_eval(state, |ctx, _| find_objects(state, f, ctx));
    }
    let cur_turn = state.turn_number();
    for id in cands {
        if let Some(r) = state.rare_or_default(id) {
            r.playable_from_exile_by = Some(PlayableFromExileState {
                player: actor,
                expires_turn: Some(cur_turn),
                cast_with_mana_cost: Some(mtg_ir::ManaCost::default()),
            });
        }
    }
    state.mark_dirty(DirtyMask::PLAYER_STATE);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_extra_turn(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    for pid in players {
        let skipped = state
            .evaluate_board()
            .player(pid)
            .is_some_and(|ep| ep.skip_extra_turns);
        if !skipped {
            rare_turn_state_mut(state).extra_turn_queue.push(pid);
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_skip_turn(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let rts = rare_turn_state_mut(state);
    for pid in players {
        if let Some((_, c)) = rts.skip_next_turns.iter_mut().find(|(p, _)| *p == pid) {
            *c = c.saturating_add(1);
        } else {
            rts.skip_next_turns.push((pid, 1));
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_end_turn(
    state: &mut GameState,
    step: &EffectStep,
    active_player_may: bool,
) -> Result<StepExecOutcome, EngineError> {
    if active_player_may {
        let chooser = state.active_player();
        let prompt_id = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::OptionalChoice {
                chooser,
                purpose: YesNoPausePurpose::EndTurnActiveMay,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::YesNo {
            prompt_id,
            label: "End the turn?".into(),
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(chooser);
        return Ok(StepExecOutcome::Paused { prompt_id, chooser });
    }
    let actor = resolve_step_actor(state, step);
    match crate::turn::end_the_turn_immediately(state, actor)? {
        crate::turn::TurnAdvanceOutcome::Continued => Ok(StepExecOutcome::Done),
        crate::turn::TurnAdvanceOutcome::Paused { prompt_id, chooser } => {
            Ok(StepExecOutcome::Paused { prompt_id, chooser })
        }
        crate::turn::TurnAdvanceOutcome::GameOver { winner } => {
            Ok(StepExecOutcome::GameOver { winner })
        }
    }
}

pub(crate) fn exec_extra_combat_phase(
    state: &mut GameState,
) -> Result<StepExecOutcome, EngineError> {
    let next = state.additional_combat_phases().saturating_add(1);
    state.set_additional_combat_phases(next);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_skip_combat_phase(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let rts = rare_turn_state_mut(state);
    for pid in players {
        if let Some((_, c)) = rts
            .skip_next_combat_phases
            .iter_mut()
            .find(|(p, _)| *p == pid)
        {
            *c = c.saturating_add(1);
        } else {
            rts.skip_next_combat_phases.push((pid, 1));
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_end_combat_phase(state: &mut GameState) -> Result<StepExecOutcome, EngineError> {
    let _ = crate::combat::end_combat_phase_immediately(state)?;
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_win(
    state: &mut GameState,
    step: &EffectStep,
    condition: Option<&Condition>,
) -> Result<StepExecOutcome, EngineError> {
    if let Some(cond) = condition
        && !with_vm_eval(state, |ctx, _| evaluate_condition(state, cond, ctx))
    {
        return Ok(StepExecOutcome::Done);
    }
    let winner = resolve_target_players(state, step)
        .into_iter()
        .next()
        .unwrap_or_else(|| vm_controller(state));
    let blocked = {
        let cache = state.evaluate_board();
        state.players().iter().any(|p| {
            !p.lost && p.id != winner && cache.player(p.id).is_some_and(|ep| ep.opponents_cant_win)
        })
    };
    if blocked {
        return Ok(StepExecOutcome::Done);
    }
    state.set_decision(DecisionState::GameOver {
        winner: Some(winner),
    });
    Ok(StepExecOutcome::GameOver {
        winner: Some(winner),
    })
}

pub(crate) fn exec_lose(
    state: &mut GameState,
    step: &EffectStep,
    condition: Option<&Condition>,
) -> Result<StepExecOutcome, EngineError> {
    if let Some(cond) = condition
        && !with_vm_eval(state, |ctx, _| evaluate_condition(state, cond, ctx))
    {
        return Ok(StepExecOutcome::Done);
    }
    let players = resolve_target_players(state, step);
    for pid in players {
        let cant_lose = state
            .evaluate_board()
            .player(pid)
            .is_some_and(|ep| ep.cant_lose);
        if cant_lose {
            continue;
        }
        if let Some(p) = state.player_mut(pid)
            && !p.lost
        {
            p.lost = true;
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
            emit_event(state, AtomicEventKind::PlayerLostGame { player: pid });
            crate::sba::cleanup_eliminated_player(state, pid);
        }
    }
    let alive: Vec<PlayerId> = state
        .players()
        .iter()
        .filter(|p| !p.lost)
        .map(|p| p.id)
        .collect();
    if alive.len() <= 1 {
        let winner = alive.first().copied();
        state.set_decision(DecisionState::GameOver { winner });
        return Ok(StepExecOutcome::GameOver { winner });
    }
    Ok(StepExecOutcome::Done)
}
