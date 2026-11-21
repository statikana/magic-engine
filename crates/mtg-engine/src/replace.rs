use std::sync::Arc;

use mtg_ir::{
    AbilityDef, Affected, CardDefId, CardTypeMask, ColorMask, CopyModificationSpec, CounterId,
    DamageModMode, DamageModTarget, DamagePreventionSpec, DealtDamageExileScope, EtbChoiceSpec,
    EventDetail, EventKind, FilterFlags, GamePhase, KeywordId, ManaColor, ObjectFilter,
    ObjectPredicate, PreventMode, PreventRedirectTo, PreventScope, ReplacementDef, ReplacementWith,
    RoundMode, SourceKindFilter, StaticBody, StaticDamageAmount, StaticDamageSourceFilter,
    StaticDamageTarget, SubTypeChoiceScope, SubtypeId, SupertypeMask, TokenSpec, TriggerSubject,
    Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::commander::{CommanderZoneHook, zone_change_hook};
use crate::eval::{BoardView, EvalContext, compare_i32, evaluate_amount, evaluate_condition};
use crate::faces::{
    active_abilities_where, face_opts_for_object, for_each_active_ability_with_card_on_faces,
    is_non_creature_subtype, resolve_object_faces,
};
use crate::filter::{
    find_objects, matches_object_filter, matches_player_filter, matches_relative_control,
};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::layers::predicates::{GrantedAbilityEntry, StaticGrantOrigin};
use crate::state::{
    CopiableState, DamagePreventionShield, DirtyMask, EngineError, EtbInteractiveStep, GameState,
    LayerCache, NONE_OBJECT_ID, ObjectFlags, PendingReplacementPrompt, PendingReplacementState,
    ReplaceableEventKind, ReplacementAccumulatedMods, ReplacementContinuation,
    ReplacementGrantOrigin, ReplacementSourceId, ReplacementSourceKind, ShieldRemaining,
    ShieldTarget, TOKEN_CARD_DEF_ID, evaluate_token_spec_base,
};
use crate::zones::{CreateTokenOptions, MoveObjectOutcome, finalize_zone_move};

const MAX_REPLACEMENT_ITERS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct ReplacementOutcome {
    pub event: ReplaceableEventKind,
    pub mods: ReplacementAccumulatedMods,
    pub applied: SmallVec<[ReplacementSourceId; 4]>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReplacementResult {
    /// Pipeline completed; caller applies the modified event (`mods.skipped == true` means skipped).
    Resolved(Box<ReplacementOutcome>),
    /// Pipeline paused on a `ChoiceRequest` stored in `state.pending_replacement()`.
    Paused { prompt_id: u32, chooser: PlayerId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReplacementResumeOutcome {
    ZoneMove(MoveObjectOutcome),
    EffectResolved(Box<ReplacementOutcome>),
    Paused { prompt_id: u32, chooser: PlayerId },
}

#[derive(Clone)]
struct Candidate {
    source_id: ReplacementSourceId,
    tier: u8,
    kind: CandidateKind,
}

#[derive(Clone)]
enum CandidateKind {
    ReplacementDef {
        def: ReplacementDef,
        source_controller: PlayerId,
    },
    Multiplier {
        source_controller: PlayerId,
        factor: f32,
        delta: i16,
        round: Option<RoundMode>,
        token_filter: Option<Box<ObjectFilter>>,
        extra_token: Option<Box<TokenSpec>>,
        extra_token_matching_count: bool,
    },
    StaticPrevention {
        amount: StaticDamageAmount,
        on_prevented: Option<mtg_ir::StaticOnPrevented>,
        phantom_counter_removal: Option<mtg_ir::PhantomCounterRemoval>,
        unpreventable_only_phantom: bool,
    },
    StaticRedirection {
        redirect_target: TargetRef,
    },
    PreventionShield {
        shield_id: u32,
    },
    TokenReplacement {
        one_of_each: bool,
        match_subtypes: SmallVec<[SubtypeId; 2]>,
        replacement_token: Option<Box<TokenSpec>>,
    },
    FinalityCounter,
    ExileInsteadOfDyingEffect,
    CommanderRedirect,
    KeywordAbsorb {
        n: u32,
    },
}

/// Resolves all applicable `CR 614–616` replacement and `CR 615` prevention effects for `event`.
pub fn resolve_replacements(
    state: &mut GameState,
    event: ReplaceableEventKind,
    mods: ReplacementAccumulatedMods,
    continuation: ReplacementContinuation,
) -> Result<ReplacementResult, EngineError> {
    let resume_decision = state.decision().clone();
    let resume_priority_player = state.priority_player();
    run_replacement_loop(
        state,
        event,
        mods,
        SmallVec::new(),
        continuation,
        resume_decision,
        resume_priority_player,
    )
}

pub fn resolve_damage(
    state: &mut GameState,
    source: Option<ObjectId>,
    target: TargetRef,
    amount: u32,
    is_combat: bool,
    unpreventable_by_effect: bool,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::Damage {
            source,
            target,
            amount,
            is_combat,
            unpreventable_by_effect,
        },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::DamageEffect,
    )
}

pub fn resolve_draw(
    state: &mut GameState,
    player: PlayerId,
    count: u32,
    source: Option<ObjectId>,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::Draw {
            player,
            count,
            source,
        },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::DrawEffect,
    )
}

pub fn resolve_life_gain(
    state: &mut GameState,
    player: PlayerId,
    amount: u32,
    source: Option<ObjectId>,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::LifeGain {
            player,
            amount,
            source,
        },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::LifeGainEffect,
    )
}

pub fn resolve_put_counters(
    state: &mut GameState,
    object: ObjectId,
    counter: CounterId,
    count: u32,
    placer: Option<PlayerId>,
    source: Option<ObjectId>,
) -> Result<ReplacementResult, EngineError> {
    resolve_put_counters_with_continuation(
        state,
        object,
        counter,
        count,
        placer,
        source,
        ReplacementContinuation::PutCountersEffect,
    )
}

pub(crate) fn resolve_put_counters_with_continuation(
    state: &mut GameState,
    object: ObjectId,
    counter: CounterId,
    count: u32,
    placer: Option<PlayerId>,
    source: Option<ObjectId>,
    continuation: ReplacementContinuation,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::PutCounters {
            object,
            counter,
            count,
            placer,
            source,
        },
        ReplacementAccumulatedMods::default(),
        continuation,
    )
}

pub fn resolve_put_player_counters(
    state: &mut GameState,
    player: PlayerId,
    counter: CounterId,
    count: u32,
    placer: Option<PlayerId>,
    source: Option<ObjectId>,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::PutPlayerCounters {
            player,
            counter,
            count,
            placer,
            source,
        },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::PutPlayerCountersEffect,
    )
}

pub fn resolve_create_token(
    state: &mut GameState,
    controller: PlayerId,
    spec: Option<TokenSpec>,
    count: u32,
    source: Option<ObjectId>,
    opts: CreateTokenOptions,
) -> Result<ReplacementResult, EngineError> {
    let mods = ReplacementAccumulatedMods {
        token_opts: Some(opts),
        ..Default::default()
    };
    resolve_replacements(
        state,
        ReplaceableEventKind::CreateToken {
            controller,
            spec: spec.map(Box::new),
            count,
            source,
        },
        mods,
        ReplacementContinuation::CreateTokenEffect,
    )
}

pub fn resolve_untap(
    state: &mut GameState,
    object: ObjectId,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::Untap { object },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::UntapEffect,
    )
}

pub fn resolve_proliferate(
    state: &mut GameState,
    player: PlayerId,
    count: u32,
    source: Option<ObjectId>,
) -> Result<ReplacementResult, EngineError> {
    resolve_replacements(
        state,
        ReplaceableEventKind::Proliferate {
            player,
            count,
            source,
        },
        ReplacementAccumulatedMods::default(),
        ReplacementContinuation::ProliferateEffect,
    )
}

/// Resumes a paused replacement prompt (`ChoiceRequest::ChooseReplacement`, `CommanderRedirect`,
/// `EtbCopy`, `EtbChoice`, or `AbundanceChoice`), preserving `CR 614.5` applied-once history.
pub fn resume_pending_replacement(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<ReplacementResult, EngineError> {
    let pending = state.pending_replacement().cloned().ok_or_else(|| {
        EngineError::IllegalAction("no pending replacement prompt is active".into())
    })?;
    if actor != pending.chooser {
        return Err(EngineError::IllegalAction(format!(
            "replacement prompt {} belongs to {:?}, not {:?}",
            pending.prompt_id, pending.chooser, actor
        )));
    }
    if prompt_id != pending.prompt_id {
        return Err(EngineError::IllegalAction(format!(
            "prompt id mismatch: expected {}, got {prompt_id}",
            pending.prompt_id
        )));
    }

    let mut event = pending.event.clone();
    let mut mods = pending.mods.clone();
    let mut applied = pending.applied.clone();

    match &pending.prompt_kind {
        PendingReplacementPrompt::ChooseReplacement => {
            let ChoiceAnswer::PickReplacement { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickReplacement".into(),
                ));
            };
            let idx = usize::from(index);
            if idx >= pending.candidates.len() {
                return Err(EngineError::IllegalAction(format!(
                    "replacement index {idx} out of bounds (option_count = {})",
                    pending.candidates.len()
                )));
            }
            let chosen_id = pending.candidates[idx];
            state.take_pending_replacement();
            state.set_decision(pending.resume_decision.clone());
            state.set_priority_player(pending.resume_priority_player);

            let candidates = collect_applicable_candidates(state, &event, &mods, &applied);
            if let Some(cand) = candidates.into_iter().find(|c| c.source_id == chosen_id) {
                match apply_candidate(
                    state,
                    cand,
                    &mut event,
                    &mut mods,
                    &mut applied,
                    pending.continuation.clone(),
                    pending.resume_decision.clone(),
                    pending.resume_priority_player,
                )? {
                    ApplyStep::Continue => {}
                    ApplyStep::Paused { prompt_id, chooser } => {
                        return Ok(ReplacementResult::Paused { prompt_id, chooser });
                    }
                }
            } else {
                // Source vanished before answer; record applied so loop advances safely.
                applied.push(chosen_id);
            }
        }
        PendingReplacementPrompt::CommanderRedirect { intended_zone } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::YesNo for commander redirect".into(),
                ));
            };
            state.take_pending_replacement();
            state.set_decision(pending.resume_decision.clone());
            state.set_priority_player(pending.resume_priority_player);
            if !mods.commander_answered_zones.contains(intended_zone) {
                mods.commander_answered_zones.push(*intended_zone);
            }
            if value && let ReplaceableEventKind::ZoneChange { to, .. } = &mut event {
                *to = Zone::Command;
                mods.shuffle_into_library = false;
                mods.enter_tapped = None;
            }
        }
        PendingReplacementPrompt::EtbCopy {
            source_id,
            source_controller,
            mod_spec,
            candidates,
            optional,
            tapped_if_copied,
            modifications,
        } => {
            let picked = match answer {
                ChoiceAnswer::PickObject { id } => {
                    if !candidates.contains(&id) {
                        return Err(EngineError::IllegalAction(format!(
                            "object {id:?} is not a valid ETB copy candidate"
                        )));
                    }
                    Some(id)
                }
                ChoiceAnswer::DonePicking if *optional => None,
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected ChoiceAnswer::PickObject for ETB copy".into(),
                    ));
                }
            };
            state.take_pending_replacement();
            state.set_decision(pending.resume_decision.clone());
            state.set_priority_player(pending.resume_priority_player);
            if let Some(copy_target) = picked
                && let ReplaceableEventKind::ZoneChange {
                    object, controller, ..
                } = &event
            {
                apply_etb_copy_to_object(
                    state,
                    *object,
                    *controller,
                    copy_target,
                    *tapped_if_copied,
                    modifications.as_deref(),
                    &mut mods,
                );
            }
            mods.etb_copy_done = true;
            match apply_zone_change_mod(
                state,
                *source_id,
                *source_controller,
                mod_spec,
                &mut event,
                &mut mods,
                &applied,
                pending.continuation.clone(),
                pending.resume_decision.clone(),
                pending.resume_priority_player,
            )? {
                ZoneModStep::Paused { prompt_id, chooser } => {
                    return Ok(ReplacementResult::Paused { prompt_id, chooser });
                }
                ZoneModStep::Done => {
                    if !applied.contains(source_id) {
                        applied.push(*source_id);
                    }
                    mods.reset_zone_mod_progress();
                }
            }
        }
        PendingReplacementPrompt::EtbChoice {
            source_id,
            source_controller,
            mod_spec,
            step,
            step_idx,
        } => {
            match step {
                EtbInteractiveStep::DiscardForKeywordCounters {
                    candidates,
                    tracked_keywords,
                    picked,
                } => match answer {
                    ChoiceAnswer::PickObject { id } => {
                        if !candidates.contains(&id) || picked.contains(&id) {
                            return Err(EngineError::IllegalAction(format!(
                                "object {id:?} is not a valid discard candidate"
                            )));
                        }
                        state.take_pending_replacement();
                        state.set_decision(pending.resume_decision.clone());
                        state.set_priority_player(pending.resume_priority_player);
                        let mut next_picked = picked.clone();
                        next_picked.push(id);
                        if next_picked.len() < candidates.len() {
                            return pause_etb_choice(
                                state,
                                pending.chooser,
                                event,
                                applied,
                                mods,
                                pending.continuation.clone(),
                                pending.resume_decision,
                                pending.resume_priority_player,
                                *source_id,
                                *source_controller,
                                mod_spec.clone(),
                                EtbInteractiveStep::DiscardForKeywordCounters {
                                    candidates: candidates.clone(),
                                    tracked_keywords: tracked_keywords.clone(),
                                    picked: next_picked,
                                },
                                *step_idx,
                            );
                        }
                        finalize_etb_discard_for_keywords(
                            state,
                            pending.chooser,
                            tracked_keywords,
                            &next_picked,
                            &mut mods,
                        )?;
                        if !mods.etb_answered_steps.contains(step_idx) {
                            mods.etb_answered_steps.push(*step_idx);
                        }
                    }
                    ChoiceAnswer::DonePicking => {
                        state.take_pending_replacement();
                        state.set_decision(pending.resume_decision.clone());
                        state.set_priority_player(pending.resume_priority_player);
                        if !picked.is_empty() {
                            finalize_etb_discard_for_keywords(
                                state,
                                pending.chooser,
                                tracked_keywords,
                                picked,
                                &mut mods,
                            )?;
                        }
                        if !mods.etb_answered_steps.contains(step_idx) {
                            mods.etb_answered_steps.push(*step_idx);
                        }
                    }
                    _ => {
                        return Err(EngineError::IllegalAction(
                            "expected ChoiceAnswer::PickObject or DonePicking".into(),
                        ));
                    }
                },
                EtbInteractiveStep::RemoveCountersForPlusCounters {
                    candidates,
                    multiplier,
                    picked,
                } => match answer {
                    ChoiceAnswer::PickObject { id } => {
                        if !candidates.contains(&id) || picked.contains(&id) {
                            return Err(EngineError::IllegalAction(format!(
                                "object {id:?} is not a valid counter-removal candidate"
                            )));
                        }
                        state.take_pending_replacement();
                        state.set_decision(pending.resume_decision.clone());
                        state.set_priority_player(pending.resume_priority_player);
                        let mut next_picked = picked.clone();
                        next_picked.push(id);
                        if next_picked.len() < candidates.len() {
                            return pause_etb_choice(
                                state,
                                pending.chooser,
                                event,
                                applied,
                                mods,
                                pending.continuation.clone(),
                                pending.resume_decision,
                                pending.resume_priority_player,
                                *source_id,
                                *source_controller,
                                mod_spec.clone(),
                                EtbInteractiveStep::RemoveCountersForPlusCounters {
                                    candidates: candidates.clone(),
                                    multiplier: *multiplier,
                                    picked: next_picked,
                                },
                                *step_idx,
                            );
                        }
                        finalize_etb_remove_counters_for_plus_counters(
                            state,
                            pending.chooser,
                            *multiplier,
                            &next_picked,
                            &mut mods,
                        );
                        if !mods.etb_answered_steps.contains(step_idx) {
                            mods.etb_answered_steps.push(*step_idx);
                        }
                    }
                    ChoiceAnswer::DonePicking => {
                        state.take_pending_replacement();
                        state.set_decision(pending.resume_decision.clone());
                        state.set_priority_player(pending.resume_priority_player);
                        if !picked.is_empty() {
                            finalize_etb_remove_counters_for_plus_counters(
                                state,
                                pending.chooser,
                                *multiplier,
                                picked,
                                &mut mods,
                            );
                        }
                        if !mods.etb_answered_steps.contains(step_idx) {
                            mods.etb_answered_steps.push(*step_idx);
                        }
                    }
                    _ => {
                        return Err(EngineError::IllegalAction(
                            "expected ChoiceAnswer::PickObject or DonePicking".into(),
                        ));
                    }
                },
                _ => {
                    apply_etb_interactive_answer(state, &mut event, step, answer, &mut mods)?;
                    state.take_pending_replacement();
                    state.set_decision(pending.resume_decision.clone());
                    state.set_priority_player(pending.resume_priority_player);

                    if let EtbInteractiveStep::ChooseTwoColors {
                        first: None,
                        colors,
                    } = step
                    {
                        // Second step of ChooseTwoColors needs a second color pick if > 1 left.
                        let picked_first = state
                            .rare(event_moving_object(&event).unwrap_or(NONE_OBJECT_ID))
                            .and_then(|r| r.chosen_colors.first().copied());
                        if let Some(c1) = picked_first {
                            let rem: Vec<ManaColor> =
                                colors.iter().copied().filter(|&c| c != c1).collect();
                            if rem.len() > 1 {
                                return pause_etb_choice(
                                    state,
                                    pending.chooser,
                                    event,
                                    applied,
                                    mods,
                                    pending.continuation.clone(),
                                    pending.resume_decision,
                                    pending.resume_priority_player,
                                    *source_id,
                                    *source_controller,
                                    mod_spec.clone(),
                                    EtbInteractiveStep::ChooseTwoColors {
                                        first: Some(c1),
                                        colors: rem,
                                    },
                                    *step_idx,
                                );
                            } else if let Some(&c2) = rem.first()
                                && let Some(obj_id) = event_moving_object(&event)
                                && let Some(rare) = state.rare_or_default(obj_id)
                            {
                                rare.chosen_colors.push(c2);
                                rare.chosen_color = Some(color_mask_for(c1) | color_mask_for(c2));
                                state.mark_dirty(DirtyMask::DESIGNATIONS);
                            }
                        }
                    } else if let EtbInteractiveStep::ChooseTwoPlayers {
                        first: None,
                        candidates,
                    } = step
                    {
                        let picked_first = event_moving_object(&event)
                            .and_then(|id| state.rare(id))
                            .and_then(|r| r.chosen_players.first().copied());
                        if let Some(p1) = picked_first {
                            let rem: Vec<PlayerId> =
                                candidates.iter().copied().filter(|&p| p != p1).collect();
                            if rem.len() > 1 {
                                return pause_etb_choice(
                                    state,
                                    pending.chooser,
                                    event,
                                    applied,
                                    mods,
                                    pending.continuation.clone(),
                                    pending.resume_decision,
                                    pending.resume_priority_player,
                                    *source_id,
                                    *source_controller,
                                    mod_spec.clone(),
                                    EtbInteractiveStep::ChooseTwoPlayers {
                                        first: Some(p1),
                                        candidates: rem,
                                    },
                                    *step_idx,
                                );
                            } else if let Some(&p2) = rem.first()
                                && let Some(obj_id) = event_moving_object(&event)
                                && let Some(rare) = state.rare_or_default(obj_id)
                            {
                                rare.chosen_players.push(p2);
                                state.mark_dirty(DirtyMask::DESIGNATIONS);
                            }
                        }
                    }

                    if matches!(step, EtbInteractiveStep::ChooseOpponentForControl { .. }) {
                        mods.etb_steal_done = true;
                    }
                    if !mods.etb_answered_steps.contains(step_idx) {
                        mods.etb_answered_steps.push(*step_idx);
                    }
                }
            }

            match apply_zone_change_mod(
                state,
                *source_id,
                *source_controller,
                mod_spec,
                &mut event,
                &mut mods,
                &applied,
                pending.continuation.clone(),
                pending.resume_decision.clone(),
                pending.resume_priority_player,
            )? {
                ZoneModStep::Paused { prompt_id, chooser } => {
                    return Ok(ReplacementResult::Paused { prompt_id, chooser });
                }
                ZoneModStep::Done => {
                    if !applied.contains(source_id) {
                        applied.push(*source_id);
                    }
                    mods.reset_zone_mod_progress();
                }
            }
        }
        PendingReplacementPrompt::AbundanceChoice { source_id } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::YesNo for Abundance choice".into(),
                ));
            };
            state.take_pending_replacement();
            state.set_decision(pending.resume_decision.clone());
            state.set_priority_player(pending.resume_priority_player);
            mods.abundance_land = Some(value);
            if !applied.contains(source_id) {
                applied.push(*source_id);
            }
        }
    }

    run_replacement_loop(
        state,
        event,
        mods,
        applied,
        pending.continuation,
        pending.resume_decision,
        pending.resume_priority_player,
    )
}

/// Resumes a pending replacement and, if resolved for a `ZoneMove` or `ZoneMoveEtbCounters`,
/// finalizes the zone move.
pub fn resume_and_finalize_replacement(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<ReplacementResumeOutcome, EngineError> {
    let cont = state
        .pending_replacement()
        .map(|p| p.continuation.clone())
        .ok_or_else(|| {
            EngineError::IllegalAction("no pending replacement prompt is active".into())
        })?;
    match resume_pending_replacement(state, actor, prompt_id, answer)? {
        ReplacementResult::Paused { prompt_id, chooser } => {
            Ok(ReplacementResumeOutcome::Paused { prompt_id, chooser })
        }
        ReplacementResult::Resolved(outcome) => match cont {
            ReplacementContinuation::ZoneMove => match finalize_zone_move(state, *outcome)? {
                MoveObjectOutcome::Paused { prompt_id, chooser } => {
                    Ok(ReplacementResumeOutcome::Paused { prompt_id, chooser })
                }
                mv => Ok(ReplacementResumeOutcome::ZoneMove(mv)),
            },
            ReplacementContinuation::ZoneMoveEtbCounters(etb_ctx) => {
                crate::zones::apply_resolved_put_counters_outcome(state, &outcome);
                match crate::zones::process_etb_counters_and_finish_move(
                    state,
                    etb_ctx.raw,
                    etb_ctx.from_player,
                    etb_ctx.to_player,
                    etb_ctx.controller,
                    etb_ctx.remaining_counters,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        Ok(ReplacementResumeOutcome::Paused { prompt_id, chooser })
                    }
                    mv => Ok(ReplacementResumeOutcome::ZoneMove(mv)),
                }
            }
            _ => Ok(ReplacementResumeOutcome::EffectResolved(outcome)),
        },
    }
}

/// Returns all legal [`Action`]s for `player` when a `PendingReplacementState` prompt is active.
pub fn legal_replacement_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let Some(pending) = state.pending_replacement() else {
        return Vec::new();
    };
    if pending.chooser != player {
        return Vec::new();
    }
    let mut actions = Vec::new();
    let pid = pending.prompt_id;
    match &pending.prompt_kind {
        PendingReplacementPrompt::ChooseReplacement => {
            for i in 0..pending.candidates.len() {
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickReplacement { index: i as u16 },
                });
            }
        }
        PendingReplacementPrompt::CommanderRedirect { .. }
        | PendingReplacementPrompt::AbundanceChoice { .. } => {
            actions.push(Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::YesNo { value: true },
            });
            actions.push(Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::YesNo { value: false },
            });
        }
        PendingReplacementPrompt::EtbCopy {
            candidates,
            optional,
            ..
        } => {
            for &id in candidates {
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickObject { id },
                });
            }
            if *optional || candidates.is_empty() {
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::DonePicking,
                });
            }
        }
        PendingReplacementPrompt::EtbChoice { step, .. } => match step {
            EtbInteractiveStep::PayLifeOrEnterTapped { .. } => {
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::YesNo { value: true },
                });
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::YesNo { value: false },
                });
            }
            EtbInteractiveStep::RevealFromHandOrEnterTapped { candidates } => {
                for &id in candidates {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickObject { id },
                    });
                }
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::DonePicking,
                });
            }
            EtbInteractiveStep::ChooseObjectOnBattlefield {
                candidates,
                optional,
            } => {
                for &id in candidates {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickObject { id },
                    });
                }
                if *optional || candidates.is_empty() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::DonePicking,
                    });
                }
            }
            EtbInteractiveStep::DiscardForKeywordCounters {
                candidates, picked, ..
            }
            | EtbInteractiveStep::RemoveCountersForPlusCounters {
                candidates, picked, ..
            } => {
                for &id in candidates {
                    if !picked.contains(&id) {
                        actions.push(Action::Answer {
                            prompt_id: pid,
                            answer: ChoiceAnswer::PickObject { id },
                        });
                    }
                }
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::DonePicking,
                });
            }
            EtbInteractiveStep::ChooseNumber { min, max } => {
                let lo = *min;
                let hi = (*max).min(lo.saturating_add(100));
                for v in lo..=hi {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickNumber { value: v },
                    });
                }
            }
            EtbInteractiveStep::ChooseSubType { options } => {
                for i in 0..options.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
            EtbInteractiveStep::ChooseColor { colors }
            | EtbInteractiveStep::ChooseTwoColors { colors, .. } => {
                for i in 0..colors.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
            EtbInteractiveStep::ChooseCardName { candidates } => {
                for i in 0..candidates.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
            EtbInteractiveStep::ChooseCounter { options } => {
                for i in 0..options.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
            EtbInteractiveStep::ChooseAnchorWord { .. } | EtbInteractiveStep::ChooseParity => {
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickMode { index: 0 },
                });
                actions.push(Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickMode { index: 1 },
                });
            }
            EtbInteractiveStep::ChoosePlayer { candidates }
            | EtbInteractiveStep::ChooseTwoPlayers { candidates, .. }
            | EtbInteractiveStep::ChooseOpponentForControl { candidates } => {
                for i in 0..candidates.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
            EtbInteractiveStep::ChooseCardType { types } => {
                for i in 0..types.len() {
                    actions.push(Action::Answer {
                        prompt_id: pid,
                        answer: ChoiceAnswer::PickMode { index: i as u16 },
                    });
                }
            }
        },
    }
    actions.push(Action::Concede);
    actions
}

/// Returns `true` if damage from `source` to `target` cannot be prevented (`CR 615.12`).
pub fn is_damage_unpreventable(
    state: &GameState,
    source: Option<ObjectId>,
    target: TargetRef,
    is_combat: bool,
    effect_unpreventable: bool,
) -> bool {
    if effect_unpreventable || state.damage_cant_be_prevented_this_turn() {
        return true;
    }
    if is_combat && state.combat_damage_cant_be_prevented_this_turn() {
        return true;
    }

    let cache = state.evaluate_board();
    let affected_player = match target {
        TargetRef::Player { id } => Some(id),
        TargetRef::Object { id } => cache
            .get(id)
            .map(|e| e.controller)
            .or_else(|| state.object(id).map(|o| o.controller)),
        TargetRef::Stack { .. } => None,
    };
    if let Some(pid) = affected_player
        && cache
            .player(pid)
            .is_some_and(|p| p.damage_cant_be_prevented)
    {
        return true;
    }
    let source_ctrl = source.and_then(|s| {
        cache
            .get(s)
            .map(|e| e.controller)
            .or_else(|| state.object(s).map(|o| o.controller))
    });
    if let Some(pid) = source_ctrl
        && cache
            .player(pid)
            .is_some_and(|p| p.damage_cant_be_prevented)
    {
        return true;
    }

    // Check active battlefield `StaticBody::UnpreventableDamage`.
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
            if eval.is_some_and(|e| e.lost_all_abilities) {
                continue;
            }
            let ctrl = eval.map_or(obj.controller, |e| e.controller);
            let ctx = EvalContext::for_source(ctrl, perm_id).with_board(BoardView::Cached(cache));
            for ab in active_abilities_for_perm_or_emblem(state, perm_id, obj, &ctx) {
                if let AbilityDef::Static(st) = ab.ability {
                    if let Some(cond) = &st.static_def.condition
                        && !evaluate_condition(state, cond, &ctx)
                    {
                        continue;
                    }
                    if let StaticBody::UnpreventableDamage {
                        combat_only,
                        source_filter,
                    } = &st.static_def.body
                    {
                        if *combat_only && !is_combat {
                            continue;
                        }
                        if let Some(aff) = source_filter {
                            let Some(src_id) = source else {
                                continue;
                            };
                            if !matches_affected(state, src_id, aff, &ctx) {
                                continue;
                            }
                        }
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Creates a `DamagePreventionShield` from `spec` and pushes it onto `GameState`, returning its id.
pub fn create_prevention_shield(
    state: &mut GameState,
    controller: PlayerId,
    spec: &DamagePreventionSpec,
    targets: &[TargetRef],
    amount: u32,
    source_stack_item: Option<StackItemId>,
) -> Option<u32> {
    let remaining = match spec.mode {
        PreventMode::All => ShieldRemaining::Infinite,
        PreventMode::Next => {
            if amount == 0 {
                return None;
            }
            ShieldRemaining::Finite(amount)
        }
    };

    let target = if spec.you_and_planeswalkers_you_control {
        ShieldTarget::YouAndPlaneswalkersYouControl(controller)
    } else if spec.all_damage_to_you_and_perms {
        ShieldTarget::PlayerAndPermanentsControlledBy {
            player: controller,
            types: CardTypeMask::PERMANENT,
        }
    } else {
        match spec.scope {
            PreventScope::Any => ShieldTarget::Any,
            PreventScope::YouAndCreaturesYouControl => {
                ShieldTarget::PlayerAndPermanentsControlledBy {
                    player: controller,
                    types: CardTypeMask::CREATURE,
                }
            }
            PreventScope::Subject => match targets.first().copied() {
                Some(TargetRef::Player { id }) => ShieldTarget::Player(id),
                Some(TargetRef::Object { id }) => ShieldTarget::Permanent(id),
                Some(TargetRef::Stack { .. }) | None => ShieldTarget::Player(controller),
            },
        }
    };

    let from_source_object = match spec.from_source.as_deref() {
        Some(mtg_ir::Selector::Target { .. }) => targets.iter().find_map(|t| match t {
            TargetRef::Object { id } => Some(*id),
            _ => None,
        }),
        _ => None,
    };

    let redirect_to = match spec.redirect_to {
        Some(PreventRedirectTo::SourceController) => Some(TargetRef::Player { id: controller }),
        Some(PreventRedirectTo::ChosenPermanent) => targets.iter().find_map(|t| match t {
            TargetRef::Object { id } => Some(TargetRef::Object { id: *id }),
            _ => None,
        }),
        None => None,
    };

    let id = state.allocate_shield_id();
    let shield = DamagePreventionShield {
        id,
        controller,
        target,
        remaining,
        combat_only: spec.combat_only,
        next_event_only: spec.next_damage_event_only,
        also_prevent_dealt_by_target: spec.also_prevent_dealt_by_subject,
        from_source_object,
        from_source_stack_item: source_stack_item,
        from_attacking_only: false,
        from_source_types: match spec.redirect_source_kind {
            Some(SourceKindFilter::Creature) => CardTypeMask::CREATURE,
            _ => CardTypeMask::empty(),
        },
        from_source_colors: spec.from_source_colors | spec.redirect_source_color_filter,
        from_source_not_subtypes: spec.from_source_not_subtypes.clone(),
        from_sources_you_dont_control: spec.from_sources_you_dont_control,
        redirect_to,
        min_life_floor: spec.min_life_floor,
        on_prevented: spec.on_prevented.clone(),
        on_prevented_creature: spec.on_prevented_creature,
        on_prevented_noncreature: spec.on_prevented_noncreature,
        on_prevented_to_damage_source_controller: spec.on_prevented_to_damage_source_controller,
    };
    state.prevention_shields_mut().push(shield);
    state.mark_dirty(DirtyMask::COMBAT | DirtyMask::PLAYER_STATE);
    Some(id)
}

/// Removes depleted shields or shields targeting objects that left the battlefield (`CR 400.7`).
pub fn prune_stale_prevention_shields(state: &mut GameState) {
    let mut shields = std::mem::take(state.prevention_shields_mut());
    shields.retain(|s| {
        if matches!(s.remaining, ShieldRemaining::Finite(0)) {
            return false;
        }
        if let ShieldTarget::Permanent(id) = s.target
            && state.object(id).is_none_or(|o| o.zone != Zone::Battlefield)
        {
            return false;
        }
        if let Some(src) = s.from_source_object
            && state.object(src).is_none()
        {
            return false;
        }
        true
    });
    *state.prevention_shields_mut() = shields;
}

/// Clears end-of-turn prevention shields and turn-wide unpreventable flags (cleanup step).
pub fn clear_eot_prevention_shields(state: &mut GameState) {
    state.clear_eot_prevention_state();
}

// ---- Core CR 614–616 Resolution Loop -----------------------------------------------------------

fn is_damage_prevented_by_protection(
    state: &GameState,
    source: Option<ObjectId>,
    target: TargetRef,
) -> bool {
    match target {
        TargetRef::Object { id } => {
            source.is_some_and(|src| crate::keywords::has_protection_from(state, id, src, false))
        }
        TargetRef::Player { id } => crate::keywords::player_has_protection_from(state, id, source),
        TargetRef::Stack { .. } => false,
    }
}

fn run_replacement_loop(
    state: &mut GameState,
    mut event: ReplaceableEventKind,
    mut mods: ReplacementAccumulatedMods,
    mut applied: SmallVec<[ReplacementSourceId; 4]>,
    continuation: ReplacementContinuation,
    resume_decision: DecisionState,
    resume_priority_player: PlayerId,
) -> Result<ReplacementResult, EngineError> {
    if is_counter_placement_blocked(state, &event) {
        mods.skipped = true;
        match &mut event {
            ReplaceableEventKind::PutCounters { count, .. }
            | ReplaceableEventKind::PutPlayerCounters { count, .. } => *count = 0,
            _ => {}
        }
        return Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
            event,
            mods,
            applied,
        })));
    }
    if let ReplaceableEventKind::LifeGain { player, amount, .. } = &mut event
        && state
            .evaluate_board()
            .player(*player)
            .is_some_and(|p| p.cant_gain_life || p.life_cant_change)
    {
        mods.skipped = true;
        *amount = 0;
        return Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
            event,
            mods,
            applied,
        })));
    }
    if let ReplaceableEventKind::Draw { player, count, .. } = &mut event {
        let cant_draw = {
            let cache = state.evaluate_board();
            let drawn = state.player(*player).map_or(0, |p| p.cards_drawn_this_turn);
            cache
                .player(*player)
                .is_some_and(|p| p.cant_draw_cards || (p.cant_draw_extra_cards && drawn >= 1))
        };
        if cant_draw {
            mods.skipped = true;
            *count = 0;
            return Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
                event,
                mods,
                applied,
            })));
        }
    }

    for _iter in 0..MAX_REPLACEMENT_ITERS {
        if let ReplaceableEventKind::Damage {
            source,
            target,
            amount,
            is_combat,
            unpreventable_by_effect,
        } = &mut event
            && *amount > 0
            && !is_damage_unpreventable(
                state,
                *source,
                *target,
                *is_combat,
                *unpreventable_by_effect,
            )
            && is_damage_prevented_by_protection(state, *source, *target)
        {
            mods.prevented_damage = mods.prevented_damage.saturating_add(*amount);
            *amount = 0;
            mods.skipped = true;
            return Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
                event,
                mods,
                applied,
            })));
        }

        let candidates = collect_applicable_candidates(state, &event, &mods, &applied);
        if candidates.is_empty() {
            finalize_draw_rule_clamp(state, &mut event, &mods);
            return Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
                event,
                mods,
                applied,
            })));
        }

        let min_tier = candidates.iter().map(|c| c.tier).min().unwrap_or(5);
        let mut tier_candidates: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| c.tier == min_tier)
            .collect();

        let chooser = determine_chooser(state, &event);
        if tier_candidates.len() == 1 {
            let cand = tier_candidates.remove(0);
            match apply_candidate(
                state,
                cand,
                &mut event,
                &mut mods,
                &mut applied,
                continuation.clone(),
                resume_decision.clone(),
                resume_priority_player,
            )? {
                ApplyStep::Continue => continue,
                ApplyStep::Paused { prompt_id, chooser } => {
                    return Ok(ReplacementResult::Paused { prompt_id, chooser });
                }
            }
        }

        // CR 616.1: Affected player / controller chooses which replacement effect applies next.
        let prompt_id = state.allocate_prompt_id();
        let cand_ids: Vec<ReplacementSourceId> =
            tier_candidates.iter().map(|c| c.source_id).collect();
        let option_count = cand_ids.len() as u16;
        state.set_pending_replacement(Some(PendingReplacementState {
            prompt_id,
            chooser,
            event,
            applied,
            candidates: cand_ids,
            prompt_kind: PendingReplacementPrompt::ChooseReplacement,
            mods,
            continuation,
            resume_decision,
            resume_priority_player,
        }));
        state.set_decision(DecisionState::Prompt {
            request: ChoiceRequest::ChooseReplacement {
                prompt_id,
                option_count,
            },
        });
        state.set_priority_player(chooser);
        return Ok(ReplacementResult::Paused { prompt_id, chooser });
    }

    finalize_draw_rule_clamp(state, &mut event, &mods);
    Ok(ReplacementResult::Resolved(Box::new(ReplacementOutcome {
        event,
        mods,
        applied,
    })))
}

fn determine_chooser(state: &GameState, event: &ReplaceableEventKind) -> PlayerId {
    match event {
        ReplaceableEventKind::ZoneChange {
            object,
            to,
            controller,
            ..
        } => {
            if matches!(to, Zone::Battlefield | Zone::Stack) {
                *controller
            } else {
                state.object(*object).map_or(*controller, |o| o.owner)
            }
        }
        ReplaceableEventKind::Damage { target, .. } => match target {
            TargetRef::Player { id } => *id,
            TargetRef::Object { id } => state
                .evaluate_board()
                .get(*id)
                .map(|e| e.controller)
                .or_else(|| state.object(*id).map(|o| o.controller))
                .unwrap_or(state.active_player()),
            TargetRef::Stack { .. } => state.active_player(),
        },
        ReplaceableEventKind::Draw { player, .. }
        | ReplaceableEventKind::LifeGain { player, .. }
        | ReplaceableEventKind::PutPlayerCounters { player, .. }
        | ReplaceableEventKind::Proliferate { player, .. } => *player,
        ReplaceableEventKind::PutCounters { object, .. }
        | ReplaceableEventKind::Untap { object } => state
            .evaluate_board()
            .get(*object)
            .map(|e| e.controller)
            .or_else(|| state.object(*object).map(|o| o.controller))
            .unwrap_or(state.active_player()),
        ReplaceableEventKind::CreateToken { controller, .. } => *controller,
    }
}

// ---- Candidate Discovery -----------------------------------------------------------------------

fn collect_applicable_candidates(
    state: &mut GameState,
    event: &ReplaceableEventKind,
    mods: &ReplacementAccumulatedMods,
    applied: &[ReplacementSourceId],
) -> Vec<Candidate> {
    // CR 616.1f / 614.7a: Once skipped/executed/terminal-replaced or reduced to 0, no further replacements apply.
    if mods.skipped
        || mods.execute_instead.is_some()
        || mods.regenerate_instead
        || mods.damage_as_minus_counters
        || mods.damage_to_self_as_counter.is_some()
        || mods.abundance_land.is_some()
    {
        return Vec::new();
    }
    match event {
        ReplaceableEventKind::Damage { amount: 0, .. }
        | ReplaceableEventKind::LifeGain { amount: 0, .. }
        | ReplaceableEventKind::PutCounters { count: 0, .. }
        | ReplaceableEventKind::PutPlayerCounters { count: 0, .. }
        | ReplaceableEventKind::CreateToken { count: 0, .. }
        | ReplaceableEventKind::Draw { count: 0, .. }
        | ReplaceableEventKind::Proliferate { count: 0, .. } => return Vec::new(),
        _ => {}
    }

    if let ReplaceableEventKind::Damage { .. } = event {
        prune_stale_prevention_shields(state);
    }

    // CR 614.12: When entering the battlefield from another zone, evaluate continuous effects
    // under hypothetical battlefield presence so Blood Moon / Humility / Grumgully apply accurately.
    if let ReplaceableEventKind::ZoneChange {
        object,
        from,
        to: Zone::Battlefield,
        controller,
    } = event
        && *from != Zone::Battlefield
    {
        return with_hypothetical_battlefield_presence(
            state,
            *object,
            *controller,
            mods,
            |state| collect_candidates_on_current_board(state, event, mods, applied),
        );
    }

    collect_candidates_on_current_board(state, event, mods, applied)
}

fn with_hypothetical_battlefield_presence<R>(
    state: &mut GameState,
    id: ObjectId,
    controller: PlayerId,
    mods: &ReplacementAccumulatedMods,
    f: impl FnOnce(&GameState) -> R,
) -> R {
    if usize::from(controller.0) >= state.players().len() {
        return f(state);
    }
    let Some(orig_obj) = state.object(id).copied() else {
        return f(state);
    };
    let orig_rare = state.rare(id).cloned();
    let orig_bf_len = state.player(controller).map_or(0, |p| p.battlefield.len());

    let mut did_push = false;
    if let Some(obj_mut) = state.object_mut(id) {
        obj_mut.zone = Zone::Battlefield;
        obj_mut.controller = controller;
        if let Some(opts) = &mods.move_opts {
            if opts.entered_transformed {
                obj_mut.flags.insert(ObjectFlags::TRANSFORMED);
                obj_mut.active_face_index = 1;
            }
            if opts.entered_flipped {
                obj_mut.flags.insert(ObjectFlags::FLIPPED);
                obj_mut.active_face_index = 1;
            }
            if opts.enter_face_down.is_some() {
                obj_mut.flags.insert(ObjectFlags::FACE_DOWN);
            }
        }
        if mods
            .enter_tapped
            .unwrap_or_else(|| mods.move_opts.as_ref().is_some_and(|o| o.enter_tapped))
        {
            obj_mut.flags.insert(ObjectFlags::TAPPED);
        }
    }
    if let Some(fd) = mods
        .move_opts
        .as_ref()
        .and_then(|o| o.enter_face_down.clone())
        && let Some(rare) = state.rare_or_default(id)
    {
        rare.face_down = Some(Arc::new(fd));
    }
    if let Some(p) = state.player_mut(controller) {
        p.battlefield.push(id);
        did_push = true;
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);

    let result = f(state);

    // Restore exact pre-lookahead state cleanly.
    if did_push && let Some(p) = state.player_mut(controller) {
        p.battlefield.truncate(orig_bf_len);
    }
    if let Some(obj_mut) = state.object_mut(id) {
        *obj_mut = orig_obj;
    }
    if let Some(rare_orig) = orig_rare {
        if let Some(rare_mut) = state.rare_or_default(id) {
            *rare_mut = rare_orig;
        }
    } else if let Some(rare_mut) = state.rare_mut(id) {
        *rare_mut = Default::default();
        state.remove_rare_if_default(id);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    result
}

fn collect_candidates_on_current_board(
    state: &GameState,
    event: &ReplaceableEventKind,
    mods: &ReplacementAccumulatedMods,
    applied: &[ReplacementSourceId],
) -> Vec<Candidate> {
    let cache = state.evaluate_board();
    let mut out: Vec<Candidate> = Vec::new();

    // 1. Scan active battlefield permanents & Command emblems across non-lost players.
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &src_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(src_obj) = state.object(src_id) else {
                continue;
            };
            if src_obj.zone == Zone::Battlefield && src_obj.is_phased_out() {
                continue;
            }
            if src_obj.zone == Zone::Command && !src_obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            let eval_obj = cache.get(src_id);
            let src_ctrl = eval_obj.map_or(src_obj.controller, |e| e.controller);
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));

            if !eval_obj.is_some_and(|e| e.lost_all_abilities) {
                for ab in active_abilities_with_card_def(state, src_id, src_obj, |cond| {
                    src_obj.zone == Zone::Battlefield && evaluate_condition(state, cond, &ctx)
                }) {
                    collect_ability_candidate(
                        state,
                        cache,
                        src_id,
                        src_ctrl,
                        ReplacementSourceKind::PrintedReplacement,
                        ReplacementGrantOrigin::Innate,
                        None,
                        Some(ab.card_def),
                        ab.face_index,
                        ab.component_index,
                        ab.ability_index as u32,
                        0,
                        ab.ability,
                        event,
                        mods,
                        applied,
                        &mut out,
                    );
                }
            }

            if let Some(extras) = cache.extra(src_id) {
                for entry in &extras.granted_abilities {
                    if let Some((ab, g_ctrl)) = resolve_granted_ability(state, entry) {
                        let g_info = granted_ability_source_info(entry);
                        collect_ability_candidate(
                            state,
                            cache,
                            src_id,
                            g_ctrl,
                            ReplacementSourceKind::GrantedReplacement,
                            g_info.grant_origin,
                            g_info.grant_source,
                            g_info.card_def,
                            g_info.face_index,
                            g_info.component_index,
                            g_info.index,
                            g_info.grant_effect_id,
                            ab,
                            event,
                            mods,
                            applied,
                            &mut out,
                        );
                    }
                }
            }
        }
    }

    // 2. CR 614.15: Self-replacements on an off-battlefield event source (e.g., Sorcery on Stack).
    if let Some(ev_src) = event_source(event)
        && let Some(src_obj) = state.object(ev_src)
        && src_obj.zone != Zone::Battlefield
    {
        for ab in active_abilities_with_card_def(state, ev_src, src_obj, |_| true) {
            if let AbilityDef::Replacement(rep_ab) = ab.ability
                && rep_ab.replacement.self_replacement
            {
                let source_id = ReplacementSourceId::printed_component(
                    ReplacementSourceKind::PrintedReplacement,
                    ev_src,
                    ab.card_def,
                    ab.face_index,
                    ab.component_index,
                    ab.ability_index as u32,
                );
                if !applied.contains(&source_id)
                    && !out.iter().any(|c| c.source_id == source_id)
                    && matches_replacement_def(
                        state,
                        cache,
                        ev_src,
                        src_obj.controller,
                        &rep_ab.replacement,
                        event,
                        mods,
                    )
                {
                    let tier = classify_replacement_tier(&rep_ab.replacement, ev_src, event);
                    out.push(Candidate {
                        source_id,
                        tier,
                        kind: CandidateKind::ReplacementDef {
                            def: rep_ab.replacement.clone(),
                            source_controller: src_obj.controller,
                        },
                    });
                }
            }
        }
    }

    // 3. Moving object off-battlefield self-replacements (e.g. GY-shuffle replacements) +
    //    Finality counter + ExileInsteadOfDying + CR 903.9b CommanderRedirect.
    if let ReplaceableEventKind::ZoneChange {
        object,
        from,
        to,
        controller: _,
    } = event
        && let Some(mov_obj) = state.object(*object)
    {
        if *from != Zone::Battlefield && *to != Zone::Battlefield {
            for ab in active_abilities_with_card_def(state, *object, mov_obj, |_| true) {
                if let AbilityDef::Replacement(rep_ab) = ab.ability {
                    let source_id = ReplacementSourceId::printed_component(
                        ReplacementSourceKind::PrintedReplacement,
                        *object,
                        ab.card_def,
                        ab.face_index,
                        ab.component_index,
                        ab.ability_index as u32,
                    );
                    if !applied.contains(&source_id)
                        && !out.iter().any(|c| c.source_id == source_id)
                        && matches_replacement_def(
                            state,
                            cache,
                            *object,
                            mov_obj.controller,
                            &rep_ab.replacement,
                            event,
                            mods,
                        )
                    {
                        let tier = classify_replacement_tier(&rep_ab.replacement, *object, event);
                        out.push(Candidate {
                            source_id,
                            tier,
                            kind: CandidateKind::ReplacementDef {
                                def: rep_ab.replacement.clone(),
                                source_controller: mov_obj.controller,
                            },
                        });
                    }
                }
            }
        }

        if *from == Zone::Battlefield && *to == Zone::Graveyard {
            if let Some(fin_cid) = state.db().counter_id("finality")
                && state.counter_count(*object, fin_cid) > 0
            {
                let sid = ReplacementSourceId::non_card(
                    ReplacementSourceKind::FinalityCounter,
                    *object,
                    0,
                );
                if !applied.contains(&sid) {
                    out.push(Candidate {
                        source_id: sid,
                        tier: 5,
                        kind: CandidateKind::FinalityCounter,
                    });
                }
            }

            if state
                .rare(*object)
                .is_some_and(|r| r.exile_instead_of_dying_this_turn)
            {
                let sid = ReplacementSourceId::non_card(
                    ReplacementSourceKind::ExileInsteadOfDyingEffect,
                    *object,
                    0,
                );
                if !applied.contains(&sid) {
                    out.push(Candidate {
                        source_id: sid,
                        tier: 5,
                        kind: CandidateKind::ExileInsteadOfDyingEffect,
                    });
                }
            }
        }

        if matches!(
            zone_change_hook(state, *object, *to),
            CommanderZoneHook::ReplacementRedirect { .. }
        ) && !mods.commander_answered_zones.contains(to)
        {
            let sid =
                ReplacementSourceId::non_card(ReplacementSourceKind::CommanderRedirect, *object, 0);
            out.push(Candidate {
                source_id: sid,
                tier: 5,
                kind: CandidateKind::CommanderRedirect,
            });
        }
    }

    // 4. CR 615 Damage Prevention Shields & Keyword Absorb (unified in Tier 5 with Damage replacements).
    if let ReplaceableEventKind::Damage {
        source,
        target,
        amount,
        is_combat,
        unpreventable_by_effect,
    } = event
        && *amount > 0
    {
        let unpreventable = is_damage_unpreventable(
            state,
            *source,
            *target,
            *is_combat,
            *unpreventable_by_effect,
        );
        if !unpreventable {
            for shield in state.prevention_shields() {
                let sid = ReplacementSourceId::non_card(
                    ReplacementSourceKind::PreventionShield,
                    NONE_OBJECT_ID,
                    shield.id,
                );
                if applied.contains(&sid) {
                    continue;
                }
                if shield_matches_damage(state, cache, shield, *source, *target, *is_combat) {
                    out.push(Candidate {
                        source_id: sid,
                        tier: 5,
                        kind: CandidateKind::PreventionShield {
                            shield_id: shield.id,
                        },
                    });
                }
            }

            if let TargetRef::Object { id: tgt_obj_id } = target
                && let Some(tgt_obj) = state.object(*tgt_obj_id)
                && tgt_obj.zone == Zone::Battlefield
                && !tgt_obj.is_phased_out()
                && !cache.get(*tgt_obj_id).is_some_and(|e| e.lost_all_abilities)
            {
                for ab in active_abilities_with_card_def(state, *tgt_obj_id, tgt_obj, |_| true) {
                    if let AbilityDef::Keyword(kw) = ab.ability
                        && state
                            .db()
                            .keyword_name(kw.keyword.keyword)
                            .is_some_and(|n| n.eq_ignore_ascii_case("Absorb"))
                    {
                        let n = kw.keyword.n.unwrap_or(1).max(0) as u32;
                        if n > 0 {
                            let sid = ReplacementSourceId::printed_component(
                                ReplacementSourceKind::KeywordAbsorb,
                                *tgt_obj_id,
                                ab.card_def,
                                ab.face_index,
                                ab.component_index,
                                ab.ability_index as u32,
                            );
                            if !applied.contains(&sid) {
                                out.push(Candidate {
                                    source_id: sid,
                                    tier: 5,
                                    kind: CandidateKind::KeywordAbsorb { n },
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    out
}

#[allow(clippy::too_many_arguments)]
fn collect_ability_candidate(
    state: &GameState,
    cache: &LayerCache,
    src_id: ObjectId,
    src_ctrl: PlayerId,
    base_kind: ReplacementSourceKind,
    grant_origin: ReplacementGrantOrigin,
    grant_source: Option<ObjectId>,
    src_card_def: Option<CardDefId>,
    face_index: u8,
    component_index: u8,
    index: u32,
    grant_effect_id: u64,
    ab: &AbilityDef,
    event: &ReplaceableEventKind,
    mods: &ReplacementAccumulatedMods,
    applied: &[ReplacementSourceId],
    out: &mut Vec<Candidate>,
) {
    match ab {
        AbilityDef::Replacement(rep_ab) => {
            let sid = ReplacementSourceId::granted(
                base_kind,
                grant_origin,
                src_id,
                grant_source,
                src_card_def,
                face_index,
                component_index,
                index,
                grant_effect_id,
            );
            if applied.contains(&sid) || out.iter().any(|c| c.source_id == sid) {
                return;
            }
            if matches_replacement_def(
                state,
                cache,
                src_id,
                src_ctrl,
                &rep_ab.replacement,
                event,
                mods,
            ) {
                let tier = classify_replacement_tier(&rep_ab.replacement, src_id, event);
                out.push(Candidate {
                    source_id: sid,
                    tier,
                    kind: CandidateKind::ReplacementDef {
                        def: rep_ab.replacement.clone(),
                        source_controller: src_ctrl,
                    },
                });
            }
        }
        AbilityDef::Static(st_ab) => {
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            if let Some(cond) = &st_ab.static_def.condition
                && !evaluate_condition(state, cond, &ctx)
            {
                return;
            }
            match &st_ab.static_def.body {
                StaticBody::Multiplier {
                    what,
                    factor,
                    delta,
                    filter,
                    counter,
                    cause,
                    cause_or,
                    round,
                    applies_to_players,
                    who_places,
                    who_creates,
                    extra_token,
                    extra_token_matching_count,
                    trigger_source_attached_to: _,
                } => {
                    if cause.is_some() || !cause_or.is_empty() {
                        return;
                    }
                    let kind = if base_kind == ReplacementSourceKind::GrantedReplacement {
                        ReplacementSourceKind::GrantedReplacement
                    } else {
                        ReplacementSourceKind::MultiplierStatic
                    };
                    let sid = ReplacementSourceId::granted(
                        kind,
                        grant_origin,
                        src_id,
                        grant_source,
                        src_card_def,
                        face_index,
                        component_index,
                        index,
                        grant_effect_id,
                    );
                    if applied.contains(&sid) || out.iter().any(|c| c.source_id == sid) {
                        return;
                    }
                    let matches = match (what, event) {
                        (
                            mtg_ir::MultiplierTarget::Counters,
                            ReplaceableEventKind::PutCounters {
                                object,
                                counter: ev_cid,
                                placer,
                                ..
                            },
                        ) => {
                            let counter_ok = counter.is_none_or(|c| c == *ev_cid);
                            let filter_ok = filter
                                .as_deref()
                                .is_none_or(|f| matches_object_filter(state, *object, f, &ctx));
                            let placer_ok = who_places.is_none_or(|rel| {
                                placer.is_some_and(|p| matches_relative_control(p, rel, &ctx))
                            });
                            counter_ok && filter_ok && placer_ok
                        }
                        (
                            mtg_ir::MultiplierTarget::Counters,
                            ReplaceableEventKind::PutPlayerCounters {
                                counter: ev_cid,
                                placer,
                                ..
                            },
                        ) => {
                            *applies_to_players
                                && counter.is_none_or(|c| c == *ev_cid)
                                && who_places.is_none_or(|rel| {
                                    placer.is_some_and(|p| matches_relative_control(p, rel, &ctx))
                                })
                        }
                        (
                            mtg_ir::MultiplierTarget::Tokens,
                            ReplaceableEventKind::CreateToken {
                                controller: tok_ctrl,
                                spec,
                                ..
                            },
                        ) => {
                            who_creates
                                .is_none_or(|rel| matches_relative_control(*tok_ctrl, rel, &ctx))
                                && filter.as_deref().is_none_or(|f| {
                                    matches_token_creation_filter(
                                        state,
                                        *tok_ctrl,
                                        spec.as_deref(),
                                        mods,
                                        f,
                                        &ctx,
                                    )
                                })
                        }
                        _ => false,
                    };
                    if matches {
                        out.push(Candidate {
                            source_id: sid,
                            tier: 5,
                            kind: CandidateKind::Multiplier {
                                source_controller: src_ctrl,
                                factor: *factor,
                                delta: *delta,
                                round: *round,
                                token_filter: filter.clone(),
                                extra_token: extra_token.clone(),
                                extra_token_matching_count: *extra_token_matching_count,
                            },
                        });
                    }
                }
                StaticBody::TokenReplacement {
                    one_of_each,
                    match_subtypes,
                    replacement_token,
                } => {
                    if let ReplaceableEventKind::CreateToken {
                        controller: tok_ctrl,
                        spec,
                        ..
                    } = event
                        && *tok_ctrl == src_ctrl
                    {
                        let kind = if base_kind == ReplacementSourceKind::GrantedReplacement {
                            ReplacementSourceKind::GrantedReplacement
                        } else {
                            ReplacementSourceKind::TokenReplacementStatic
                        };
                        let sid = ReplacementSourceId::granted(
                            kind,
                            grant_origin,
                            src_id,
                            grant_source,
                            src_card_def,
                            face_index,
                            component_index,
                            index,
                            grant_effect_id,
                        );
                        if applied.contains(&sid) || out.iter().any(|c| c.source_id == sid) {
                            return;
                        }
                        let matches = if !mods.replace_tokens.is_empty() {
                            mods.replace_tokens.iter().any(|(s, _)| {
                                token_spec_matches_replacement(s, *one_of_each, match_subtypes)
                            })
                        } else if let Some(tok_spec) = spec.as_deref() {
                            token_spec_matches_replacement(tok_spec, *one_of_each, match_subtypes)
                        } else {
                            false
                        };
                        if matches {
                            out.push(Candidate {
                                source_id: sid,
                                tier: 5,
                                kind: CandidateKind::TokenReplacement {
                                    one_of_each: *one_of_each,
                                    match_subtypes: match_subtypes.clone(),
                                    replacement_token: replacement_token.clone(),
                                },
                            });
                        }
                    }
                }
                StaticBody::StaticDamagePrevention {
                    combat_only,
                    noncombat_only,
                    amount,
                    target: prev_target,
                    also_prevent_dealt_by,
                    dealt_by_only,
                    dealt_by_target_filter,
                    source_filter,
                    phantom_counter_removal,
                    on_prevented,
                    prevent_opponent_only,
                } => {
                    if let ReplaceableEventKind::Damage {
                        source: dmg_src,
                        target: dmg_tgt,
                        amount: dmg_amt,
                        is_combat,
                        unpreventable_by_effect,
                    } = event
                        && *dmg_amt > 0
                    {
                        let kind = if base_kind == ReplacementSourceKind::GrantedReplacement {
                            ReplacementSourceKind::GrantedReplacement
                        } else {
                            ReplacementSourceKind::StaticPrevention
                        };
                        let sid = ReplacementSourceId::granted(
                            kind,
                            grant_origin,
                            src_id,
                            grant_source,
                            src_card_def,
                            face_index,
                            component_index,
                            index,
                            grant_effect_id,
                        );
                        if applied.contains(&sid) || out.iter().any(|c| c.source_id == sid) {
                            return;
                        }
                        let unpreventable = is_damage_unpreventable(
                            state,
                            *dmg_src,
                            *dmg_tgt,
                            *is_combat,
                            *unpreventable_by_effect,
                        );
                        if unpreventable && phantom_counter_removal.is_none() {
                            return;
                        }
                        if *combat_only && !*is_combat {
                            return;
                        }
                        if *noncombat_only && *is_combat {
                            return;
                        }
                        if *prevent_opponent_only
                            && dmg_src.is_some_and(|s| {
                                cache.get(s).map_or_else(
                                    || state.object(s).is_some_and(|o| o.controller == src_ctrl),
                                    |e| e.controller == src_ctrl,
                                )
                            })
                        {
                            return;
                        }
                        if !static_prevention_matches(
                            state,
                            cache,
                            src_id,
                            src_ctrl,
                            prev_target,
                            *also_prevent_dealt_by,
                            dealt_by_only.as_ref(),
                            dealt_by_target_filter.as_deref(),
                            source_filter.as_ref(),
                            *dmg_src,
                            *dmg_tgt,
                            &ctx,
                        ) {
                            return;
                        }
                        if let Some(phantom) = phantom_counter_removal
                            && phantom.require_counter_present
                            && !matches!(
                                dmg_tgt,
                                TargetRef::Object { id } if state.counter_count(*id, phantom.counter) > 0
                            )
                        {
                            return;
                        }
                        out.push(Candidate {
                            source_id: sid,
                            tier: 5,
                            kind: CandidateKind::StaticPrevention {
                                amount: amount.clone(),
                                on_prevented: on_prevented.clone(),
                                phantom_counter_removal: phantom_counter_removal.clone(),
                                unpreventable_only_phantom: unpreventable,
                            },
                        });
                    }
                }
                StaticBody::StaticDamageRedirection {
                    include_other_controlled,
                    to_attached,
                } => {
                    if let ReplaceableEventKind::Damage {
                        target: dmg_tgt,
                        amount: dmg_amt,
                        ..
                    } = event
                        && *dmg_amt > 0
                    {
                        let kind = if base_kind == ReplacementSourceKind::GrantedReplacement {
                            ReplacementSourceKind::GrantedReplacement
                        } else {
                            ReplacementSourceKind::StaticPrevention
                        };
                        let sid = ReplacementSourceId::granted(
                            kind,
                            grant_origin,
                            src_id,
                            grant_source,
                            src_card_def,
                            face_index,
                            component_index,
                            index,
                            grant_effect_id,
                        );
                        if applied.contains(&sid) || out.iter().any(|c| c.source_id == sid) {
                            return;
                        }
                        let redirect_target = if *to_attached {
                            state
                                .object(src_id)
                                .and_then(|o| o.attached_to)
                                .map(|id| TargetRef::Object { id })
                        } else {
                            Some(TargetRef::Object { id: src_id })
                        };
                        let Some(redir_tgt) = redirect_target else {
                            return;
                        };
                        if *dmg_tgt == redir_tgt {
                            return;
                        }
                        let applies = match dmg_tgt {
                            TargetRef::Player { id } => *id == src_ctrl,
                            TargetRef::Object { id } => {
                                *include_other_controlled
                                    && cache.get(*id).map_or_else(
                                        || {
                                            state
                                                .object(*id)
                                                .is_some_and(|o| o.controller == src_ctrl)
                                        },
                                        |e| e.controller == src_ctrl,
                                    )
                            }
                            TargetRef::Stack { .. } => false,
                        };
                        if applies {
                            out.push(Candidate {
                                source_id: sid,
                                tier: 5,
                                kind: CandidateKind::StaticRedirection {
                                    redirect_target: redir_tgt,
                                },
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

struct SourcedAbility<'a> {
    card_def: CardDefId,
    component_index: u8,
    face_index: u8,
    ability_index: usize,
    ability: &'a AbilityDef,
}

fn active_abilities_with_card_def<'a>(
    state: &'a GameState,
    id: ObjectId,
    obj: &crate::state::GameObject,
    allow_grant_condition: impl FnMut(&mtg_ir::Condition) -> bool,
) -> Vec<SourcedAbility<'a>> {
    let Some(opts) = face_opts_for_object(state, id) else {
        return Vec::new();
    };
    if opts.is_face_down {
        return Vec::new();
    }
    let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for_each_active_ability_with_card_on_faces(
        state.db(),
        obj.card_def,
        &opts,
        &view.active_face_indices,
        allow_grant_condition,
        |ab| {
            out.push(SourcedAbility {
                card_def: ab.card_def,
                component_index: ab.component_index,
                face_index: ab.face_index,
                ability_index: ab.ability_index,
                ability: ab.ability,
            });
        },
    );
    out
}

struct GrantedSourceInfo {
    grant_origin: ReplacementGrantOrigin,
    grant_source: Option<ObjectId>,
    card_def: Option<CardDefId>,
    face_index: u8,
    component_index: u8,
    index: u32,
    grant_effect_id: u64,
}

#[inline]
const fn mix_u64_step(acc: u64, val: u64) -> u64 {
    let mut x = acc ^ val;
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline]
fn mix_card_ability_grant_effect_id(
    donor_object: ObjectId,
    granter_origin: StaticGrantOrigin,
    granter_card_def: Option<CardDefId>,
    granter_face_index: u8,
    granter_component_index: u8,
    granter_ability_index: u32,
    source_effect_id: u64,
) -> u64 {
    let donor_tag = (u64::from(donor_object.index) << 32) | u64::from(donor_object.generation);
    let granter_origin_tag = match granter_origin {
        StaticGrantOrigin::Innate => 1u64,
        StaticGrantOrigin::TimedEffect => 2u64,
        StaticGrantOrigin::Nested => 3u64,
    };
    let granter_card_tag = match granter_card_def {
        Some(cd) => u64::from(cd.0) + 1,
        None => 0u64,
    };
    let granter_loc_tag = (u64::from(granter_face_index) << 40)
        | (u64::from(granter_component_index) << 32)
        | u64::from(granter_ability_index);
    let mut h = 0x517C_C1B7_2722_0A95u64;
    h = mix_u64_step(h, donor_tag);
    h = mix_u64_step(h, granter_origin_tag);
    h = mix_u64_step(h, granter_card_tag);
    h = mix_u64_step(h, granter_loc_tag);
    mix_u64_step(h, source_effect_id)
}

fn granted_ability_source_info(entry: &GrantedAbilityEntry) -> GrantedSourceInfo {
    match entry {
        GrantedAbilityEntry::CardAbility {
            card_def,
            face_index,
            ability_index,
            grant_source,
            donor_object,
            donor_component_index,
            granter_origin,
            granter_card_def,
            granter_face_index,
            granter_component_index,
            granter_ability_index,
            source_effect_id,
            ..
        } => GrantedSourceInfo {
            grant_origin: ReplacementGrantOrigin::GrantedFromObject,
            grant_source: *grant_source,
            card_def: Some(*card_def),
            face_index: *face_index,
            component_index: *donor_component_index,
            index: *ability_index as u32,
            grant_effect_id: mix_card_ability_grant_effect_id(
                *donor_object,
                *granter_origin,
                *granter_card_def,
                *granter_face_index,
                *granter_component_index,
                *granter_ability_index,
                *source_effect_id,
            ),
        },
        GrantedAbilityEntry::Inline {
            grant_source,
            grant_origin,
            grant_card_def,
            source_face_index,
            source_component_index,
            source_ability_index,
            source_effect_id,
            ..
        } => GrantedSourceInfo {
            grant_origin: *grant_origin,
            grant_source: *grant_source,
            card_def: *grant_card_def,
            face_index: *source_face_index,
            component_index: *source_component_index,
            index: *source_ability_index,
            grant_effect_id: *source_effect_id,
        },
        GrantedAbilityEntry::IntrinsicBasicMana { .. }
        | GrantedAbilityEntry::DynamicWard { .. } => GrantedSourceInfo {
            grant_origin: ReplacementGrantOrigin::Innate,
            grant_source: None,
            card_def: None,
            face_index: 0,
            component_index: 0,
            index: 0,
            grant_effect_id: 0,
        },
    }
}

fn active_abilities_for_perm_or_emblem<'a>(
    state: &'a GameState,
    id: ObjectId,
    obj: &crate::state::GameObject,
    ctx: &EvalContext<'_>,
) -> Vec<crate::faces::ActiveAbilityRef<'a>> {
    let Some(opts) = face_opts_for_object(state, id) else {
        return Vec::new();
    };
    active_abilities_where(state.db(), obj.card_def, &opts, |cond| {
        obj.zone == Zone::Battlefield && evaluate_condition(state, cond, ctx)
    })
}

fn resolve_granted_ability<'a>(
    state: &'a GameState,
    entry: &'a GrantedAbilityEntry,
) -> Option<(&'a AbilityDef, PlayerId)> {
    match entry {
        GrantedAbilityEntry::CardAbility {
            card_def,
            face_index,
            ability_index,
            grant_controller,
            ..
        } => {
            let ab = state
                .db()
                .get(*card_def)?
                .faces
                .get(usize::from(*face_index))?
                .abilities
                .get(*ability_index)?;
            Some((ab, *grant_controller))
        }
        GrantedAbilityEntry::Inline {
            ability,
            grant_controller,
            ..
        } => Some((ability.as_ref(), *grant_controller)),
        GrantedAbilityEntry::IntrinsicBasicMana { .. }
        | GrantedAbilityEntry::DynamicWard { .. } => None,
    }
}

fn classify_replacement_tier(
    def: &ReplacementDef,
    source_obj: ObjectId,
    event: &ReplaceableEventKind,
) -> u8 {
    if let ReplacementWith::ZoneChange { mod_spec } = &def.with {
        if mod_spec.enter_under_opponent_control {
            return 2; // CR 616.1b
        }
        if mod_spec.copy.is_some() {
            return 3; // CR 616.1c
        }
        if def.self_replacement {
            return 1; // CR 616.1a
        }
        if let ReplaceableEventKind::ZoneChange {
            object,
            to: Zone::Battlefield,
            ..
        } = event
            && *object == source_obj
            && (matches!(def.replaces.filter.object, Some(TriggerSubject::SelfObj))
                || (def.replaces.filter.object.is_none()
                    && def.replaces.filter.or_self
                    && !def.replaces.filter.or_attached_to_self
                    && !def.replaces.filter.or_commander))
        {
            return 1; // CR 616.1a
        }
        return 5;
    }
    if def.self_replacement {
        return 1;
    }
    5
}

fn matches_replacement_def(
    state: &GameState,
    cache: &LayerCache,
    src_id: ObjectId,
    src_ctrl: PlayerId,
    def: &ReplacementDef,
    event: &ReplaceableEventKind,
    mods: &ReplacementAccumulatedMods,
) -> bool {
    let mut ctx = EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
    if event_moving_object(event) == Some(src_id) {
        ctx.x_value = state.rare(src_id).and_then(|r| r.entered_with_x);
    }
    ctx.event_object = event_object(event);
    ctx.event_player = Some(event_player(state, event));
    ctx.event_amount = event_amount(event).map(|v| v as i32);

    if let Some(cond) = &def.condition
        && !evaluate_condition(state, cond, &ctx)
    {
        return false;
    }
    if let Some(who) = &def.who {
        let ep = event_player(state, event);
        if !matches_player_filter(state, ep, who, &ctx) {
            return false;
        }
    }

    match (def.replaces.event, event) {
        (
            EventKind::ZoneChange | EventKind::Discard,
            ReplaceableEventKind::ZoneChange {
                object,
                from,
                to,
                controller,
            },
        ) => {
            if def.replaces.event == EventKind::Discard
                && !matches!(
                    &def.with,
                    ReplacementWith::ZoneChange { mod_spec }
                        if mod_spec.opponent_discard_to_battlefield.is_some()
                )
            {
                return false;
            }
            if !matches_event_filter_common(
                state,
                src_id,
                Some(*object),
                *controller,
                &def.replaces.filter,
                &ctx,
            ) {
                return false;
            }
            if let Some(EventDetail::ZoneChange {
                from: det_from,
                to: det_to,
                from_anywhere,
                not_from_zone,
                not_to_zone,
                enters_untapped,
                dealt_damage_by_self_this_turn,
            }) = &def.replaces.filter.detail
            {
                if !*from_anywhere
                    && !det_from.is_empty()
                    && !det_from.contains(ZoneMask::from_zone(*from))
                {
                    return false;
                }
                if let Some(req_to) = det_to
                    && *req_to != *to
                {
                    return false;
                }
                if let Some(nf) = not_from_zone
                    && *nf == *from
                {
                    return false;
                }
                if let Some(nt) = not_to_zone
                    && *nt == *to
                {
                    return false;
                }
                if *enters_untapped && mods.enter_tapped == Some(true) {
                    return false;
                }
                if *dealt_damage_by_self_this_turn
                    && !state
                        .rare(*object)
                        .is_some_and(|r| r.dealt_damage_exile_sources.contains(&src_id))
                {
                    return false;
                }
            }
            if let ReplacementWith::ZoneChange { mod_spec } = &def.with {
                if let Some(scope) = mod_spec.dealt_damage_exile_on_die {
                    if *from != Zone::Battlefield || *to != Zone::Graveyard {
                        return false;
                    }
                    let dealt_ok = state.rare(*object).is_some_and(|r| match scope {
                        DealtDamageExileScope::SelfSource => {
                            r.dealt_damage_exile_sources.contains(&src_id)
                        }
                        DealtDamageExileScope::ControlledSource => {
                            r.dealt_damage_exile_sources.iter().any(|&s| {
                                cache.get(s).map_or_else(
                                    || state.object(s).is_some_and(|o| o.controller == src_ctrl),
                                    |e| e.controller == src_ctrl,
                                )
                            })
                        }
                        DealtDamageExileScope::ControlledSourceOpponentCreature => {
                            *controller != src_ctrl
                                && r.dealt_damage_exile_sources.iter().any(|&s| {
                                    cache.get(s).map_or_else(
                                        || {
                                            state
                                                .object(s)
                                                .is_some_and(|o| o.controller == src_ctrl)
                                        },
                                        |e| e.controller == src_ctrl,
                                    )
                                })
                        }
                    });
                    if !dealt_ok {
                        return false;
                    }
                }
                if mod_spec.dies_exile_and_reflexive.is_some()
                    && (*from != Zone::Battlefield
                        || *to != Zone::Graveyard
                        || state.object(*object).is_none_or(|o| o.is_token()))
                {
                    return false;
                }
                if mod_spec.exile_and_extra_turn_on_bf_to_gy
                    && (*from != Zone::Battlefield || *to != Zone::Graveyard)
                {
                    return false;
                }
                if mod_spec.opponent_discard_to_battlefield.is_some() {
                    if *from != Zone::Hand || *to != Zone::Graveyard {
                        return false;
                    }
                    let Some(move_opts) = &mods.move_opts else {
                        return false;
                    };
                    if !move_opts.is_discard {
                        return false;
                    }
                    let owner = state.object(*object).map_or(*controller, |o| o.owner);
                    let caused_by_opp = move_opts
                        .discard_caused_by
                        .or_else(|| {
                            state.resolution_frames().last().and_then(|rf| {
                                state
                                    .stack_entries()
                                    .iter()
                                    .find(|se| se.id == rf.stack_item)
                                    .map(|se| se.controller)
                            })
                        })
                        .is_some_and(|causer| causer != owner);
                    if !caused_by_opp {
                        return false;
                    }
                }
                if mod_spec.to == Some(*to)
                    && !mod_spec.shuffle_into_library
                    && mod_spec.exile_with_counter.is_none()
                    && mod_spec.tapped.is_none()
                    && mod_spec.counters.is_none()
                    && mod_spec.additional_counters.is_none()
                    && mod_spec.conditional_counters.is_none()
                    && mod_spec.conditional_or_fallback_counters.is_none()
                    && mod_spec.counters_per_artifact_mana.is_none()
                    && mod_spec.copy.is_none()
                    && mod_spec.etb_choices.is_empty()
                    && mod_spec.pay_life_or_enter_tapped.is_none()
                    && mod_spec.reveal_from_hand_or_enter_tapped.is_none()
                    && mod_spec.reveal_unless_controls.is_none()
                    && !mod_spec.enters_prepared
                    && !mod_spec.becomes_day_if_neither
                    && !mod_spec.enter_under_opponent_control
                    && mod_spec.dies_exile_and_reflexive.is_none()
                    && !mod_spec.exile_and_extra_turn_on_bf_to_gy
                    && mod_spec.etb_discard_for_keyword_counters.is_empty()
                    && mod_spec.etb_remove_counters_for_plus_counters.is_none()
                {
                    return false;
                }
            }
            !(matches!(def.with, ReplacementWith::RegenerateSelfOnDestroy)
                && (*from != Zone::Battlefield || *to != Zone::Graveyard))
        }
        (
            EventKind::DrawReplacement | EventKind::DrawCard,
            ReplaceableEventKind::Draw { player, count, .. },
        ) => {
            if *count == 0 {
                return false;
            }
            if !matches_event_filter_common(
                state,
                src_id,
                None,
                *player,
                &def.replaces.filter,
                &ctx,
            ) {
                return false;
            }
            if let Some(EventDetail::Draw {
                except_first_in_draw_step,
            }) = &def.replaces.filter.detail
                && *except_first_in_draw_step
                && is_first_draw_in_own_draw_step(state, *player)
            {
                return false;
            }
            match &def.with {
                ReplacementWith::DrawScale { spec } => {
                    !(spec.skip_first_in_your_draw_step
                        && !is_first_draw_in_own_draw_step(state, *player))
                }
                ReplacementWith::StealOpponentExtraDraws => {
                    *player != src_ctrl && !is_first_draw_in_own_draw_step(state, *player)
                }
                ReplacementWith::EmptyLibraryGraveyardReanimate { .. } => {
                    state.player(*player).is_some_and(|p| p.library.is_empty())
                }
                _ => true,
            }
        }
        (
            EventKind::DamageReplacement | EventKind::DealsDamage | EventKind::DealtDamage,
            ReplaceableEventKind::Damage {
                source: dmg_src,
                target,
                amount,
                is_combat,
                ..
            },
        ) => {
            if *amount == 0 {
                return false;
            }
            if !matches_event_filter_common(
                state,
                src_id,
                *dmg_src,
                event_player(state, event),
                &def.replaces.filter,
                &ctx,
            ) {
                return false;
            }
            if let Some(EventDetail::Damage {
                combat,
                noncombat_only,
                damage_to,
                ..
            }) = &def.replaces.filter.detail
            {
                if let Some(req_combat) = combat
                    && *req_combat != *is_combat
                {
                    return false;
                }
                if *noncombat_only && *is_combat {
                    return false;
                }
                if let Some(rcp) = damage_to {
                    let ok = match (rcp, target) {
                        (mtg_ir::DamageRecipient::Any, _) => true,
                        (mtg_ir::DamageRecipient::Player, TargetRef::Player { .. }) => true,
                        (
                            mtg_ir::DamageRecipient::PlayerOrPlaneswalker,
                            TargetRef::Player { .. },
                        ) => true,
                        (
                            mtg_ir::DamageRecipient::PlayerOrPlaneswalker,
                            TargetRef::Object { id },
                        ) => cache
                            .get(*id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::PLANESWALKER)),
                        (mtg_ir::DamageRecipient::Creature, TargetRef::Object { id }) => cache
                            .get(*id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE)),
                        _ => false,
                    };
                    if !ok {
                        return false;
                    }
                }
            }
            if let ReplacementWith::DamageScale { spec } = &def.with {
                if spec.noncombat_only && *is_combat {
                    return false;
                }
                if spec.combat_only && !*is_combat {
                    return false;
                }
                if let Some(aff) = &spec.source_filter {
                    let Some(s) = dmg_src else {
                        return false;
                    };
                    if !matches_affected(state, *s, aff, &ctx) {
                        return false;
                    }
                }
                let tgt_ok = match spec.target_filter {
                    DamageModTarget::Any => true,
                    DamageModTarget::Opponent => {
                        matches!(target, TargetRef::Player { id } if *id != src_ctrl)
                    }
                    DamageModTarget::OpponentOrOpponentPerm => match target {
                        TargetRef::Player { id } => *id != src_ctrl,
                        TargetRef::Object { id } => cache.get(*id).map_or_else(
                            || state.object(*id).is_some_and(|o| o.controller != src_ctrl),
                            |e| e.controller != src_ctrl,
                        ),
                        TargetRef::Stack { .. } => false,
                    },
                    DamageModTarget::ChosenPlayer => {
                        let chosen = state.rare(src_id).and_then(|r| r.chosen_player);
                        match (chosen, target) {
                            (Some(cp), TargetRef::Player { id }) => *id == cp,
                            (Some(cp), TargetRef::Object { id }) => cache.get(*id).map_or_else(
                                || state.object(*id).is_some_and(|o| o.controller == cp),
                                |e| e.controller == cp,
                            ),
                            _ => false,
                        }
                    }
                };
                if !tgt_ok {
                    return false;
                }
            }
            if let ReplacementWith::DamageToMinusCounters = &def.with {
                if *is_combat {
                    return false;
                }
                let TargetRef::Object { id } = target else {
                    return false;
                };
                let is_opp_creature = cache.get(*id).is_some_and(|e| {
                    e.types.contains(CardTypeMask::CREATURE) && e.controller != src_ctrl
                });
                if !is_opp_creature {
                    return false;
                }
            }
            !(matches!(def.with, ReplacementWith::DamageToSelfAsCounters { .. })
                && !matches!(target, TargetRef::Object { id } if *id == src_id))
        }
        (
            EventKind::LifeGainReplacement | EventKind::GainLife,
            ReplaceableEventKind::LifeGain { player, amount, .. },
        ) => {
            *amount > 0
                && matches_event_filter_common(
                    state,
                    src_id,
                    None,
                    *player,
                    &def.replaces.filter,
                    &ctx,
                )
        }
        (
            EventKind::ProliferateReplacement | EventKind::Proliferated,
            ReplaceableEventKind::Proliferate { player, count, .. },
        ) => {
            *count > 0
                && matches_event_filter_common(
                    state,
                    src_id,
                    None,
                    *player,
                    &def.replaces.filter,
                    &ctx,
                )
        }
        (EventKind::BecomesUntapped, ReplaceableEventKind::Untap { object }) => {
            matches_event_filter_common(
                state,
                src_id,
                Some(*object),
                event_player(state, event),
                &def.replaces.filter,
                &ctx,
            )
        }
        (
            EventKind::CounterPlaced,
            ReplaceableEventKind::PutCounters {
                object,
                counter,
                count,
                ..
            },
        ) => {
            if *count == 0 {
                return false;
            }
            if let Some(EventDetail::Counter { counter: req, .. }) = &def.replaces.filter.detail
                && let Some(req_cid) = req
                && *req_cid != *counter
            {
                return false;
            }
            matches_event_filter_common(
                state,
                src_id,
                Some(*object),
                event_player(state, event),
                &def.replaces.filter,
                &ctx,
            )
        }
        (
            EventKind::CounterPlaced,
            ReplaceableEventKind::PutPlayerCounters {
                player,
                counter,
                count,
                ..
            },
        ) => {
            if *count == 0 {
                return false;
            }
            if let Some(EventDetail::Counter { counter: req, .. }) = &def.replaces.filter.detail
                && let Some(req_cid) = req
                && *req_cid != *counter
            {
                return false;
            }
            matches_event_filter_common(state, src_id, None, *player, &def.replaces.filter, &ctx)
        }
        _ => false,
    }
}

fn matches_event_filter_common(
    state: &GameState,
    src_id: ObjectId,
    ev_obj: Option<ObjectId>,
    ev_player: PlayerId,
    filter: &mtg_ir::EventFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    if filter.object.is_some()
        || filter.or_self
        || filter.or_attached_to_self
        || filter.or_commander
    {
        let Some(oid) = ev_obj else {
            return false;
        };
        let matched = match &filter.object {
            Some(TriggerSubject::SelfObj) => oid == src_id,
            Some(TriggerSubject::AttachedTo) => state
                .object(src_id)
                .is_some_and(|s| s.attached_to == Some(oid)),
            Some(TriggerSubject::Filter { filter: obj_f }) => {
                matches_object_filter(state, oid, obj_f, ctx)
            }
            None => false,
        };
        let or_self = filter.or_self && oid == src_id;
        let or_att = filter.or_attached_to_self
            && state
                .object(src_id)
                .is_some_and(|s| s.attached_to == Some(oid));
        let or_cmd = filter.or_commander && state.object(oid).is_some_and(|o| o.is_commander());
        if !(matched || or_self || or_att || or_cmd) {
            return false;
        }
    }
    if let Some(pf) = &filter.player
        && !matches_player_filter(state, ev_player, pf, ctx)
    {
        return false;
    }
    if let Some(dt) = filter.during_turn {
        let active = state.active_player();
        let ok = match dt {
            mtg_ir::DuringTurnKind::Your => active == ctx.controller,
            mtg_ir::DuringTurnKind::Opponent => active != ctx.controller,
            mtg_ir::DuringTurnKind::EventPlayer => active == ev_player,
        };
        if !ok {
            return false;
        }
    }
    if let Some(step) = filter.during_step
        && state.phase() != step
    {
        return false;
    }
    if filter.during_combat
        && !matches!(
            state.phase(),
            GamePhase::BeginCombat
                | GamePhase::DeclareAttackers
                | GamePhase::DeclareBlockers
                | GamePhase::FirstStrikeDamage
                | GamePhase::CombatDamage
                | GamePhase::EndCombat
        )
    {
        return false;
    }
    if filter.during_main_phase && !matches!(state.phase(), GamePhase::Main1 | GamePhase::Main2) {
        return false;
    }
    true
}

fn shield_matches_damage(
    state: &GameState,
    cache: &LayerCache,
    shield: &DamagePreventionShield,
    source: Option<ObjectId>,
    target: TargetRef,
    is_combat: bool,
) -> bool {
    if matches!(shield.remaining, ShieldRemaining::Finite(0)) {
        return false;
    }
    if shield.combat_only && !is_combat {
        return false;
    }
    let target_ok = match shield.target {
        ShieldTarget::Any => true,
        ShieldTarget::Player(p) => target == TargetRef::Player { id: p },
        ShieldTarget::Permanent(oid) => {
            target == TargetRef::Object { id: oid }
                || (shield.also_prevent_dealt_by_target && source == Some(oid))
        }
        ShieldTarget::PermanentsControlledBy { controller, types } => {
            if let TargetRef::Object { id } = target {
                let eval = cache.get(id);
                let obj_ctrl = eval
                    .map(|e| e.controller)
                    .or_else(|| state.object(id).map(|o| o.controller));
                let obj_types = eval.map_or(CardTypeMask::empty(), |e| e.types);
                obj_ctrl == Some(controller) && (types.is_empty() || obj_types.intersects(types))
            } else {
                false
            }
        }
        ShieldTarget::PlayerAndPermanentsControlledBy { player, types } => match target {
            TargetRef::Player { id } => id == player,
            TargetRef::Object { id } => {
                let eval = cache.get(id);
                let obj_ctrl = eval
                    .map(|e| e.controller)
                    .or_else(|| state.object(id).map(|o| o.controller));
                let obj_types = eval.map_or(CardTypeMask::empty(), |e| e.types);
                obj_ctrl == Some(player) && (types.is_empty() || obj_types.intersects(types))
            }
            TargetRef::Stack { .. } => false,
        },
        ShieldTarget::YouAndPlaneswalkersYouControl(player) => match target {
            TargetRef::Player { id } => id == player,
            TargetRef::Object { id } => {
                let eval = cache.get(id);
                let obj_ctrl = eval
                    .map(|e| e.controller)
                    .or_else(|| state.object(id).map(|o| o.controller));
                let obj_types = eval.map_or(CardTypeMask::empty(), |e| e.types);
                obj_ctrl == Some(player) && obj_types.contains(CardTypeMask::PLANESWALKER)
            }
            TargetRef::Stack { .. } => false,
        },
    };
    if !target_ok {
        return false;
    }

    if let Some(req_src) = shield.from_source_object
        && source != Some(req_src)
    {
        return false;
    }
    if shield.from_attacking_only {
        let is_att =
            source.is_some_and(|s| state.combat().attackers.iter().any(|a| a.attacker == s));
        if !is_att {
            return false;
        }
    }
    if !shield.from_source_types.is_empty() {
        let Some(src_id) = source else {
            return false;
        };
        let src_types = cache
            .get(src_id)
            .map(|e| e.types)
            .unwrap_or_else(|| base_types_quick(state, src_id));
        if !src_types.intersects(shield.from_source_types) {
            return false;
        }
    }
    if !shield.from_source_colors.is_empty() {
        let Some(src_id) = source else {
            return false;
        };
        let src_colors = cache
            .get(src_id)
            .map(|e| e.colors)
            .unwrap_or(ColorMask::empty());
        if !src_colors.intersects(shield.from_source_colors) {
            return false;
        }
    }
    if !shield.from_source_not_subtypes.is_empty()
        && let Some(src_id) = source
        && let Some(eval) = cache.get(src_id)
    {
        for st in &shield.from_source_not_subtypes {
            if eval.subtypes.contains(st) {
                return false;
            }
        }
    }
    !(shield.from_sources_you_dont_control
        && source.is_some_and(|s| {
            cache.get(s).map_or_else(
                || {
                    state
                        .object(s)
                        .is_some_and(|o| o.controller == shield.controller)
                },
                |e| e.controller == shield.controller,
            )
        }))
}

#[allow(clippy::too_many_arguments)]
fn static_prevention_matches(
    state: &GameState,
    cache: &LayerCache,
    src_id: ObjectId,
    src_ctrl: PlayerId,
    target_spec: &StaticDamageTarget,
    also_dealt_by: bool,
    dealt_by_only: Option<&Affected>,
    dealt_by_target_filter: Option<&mtg_ir::ObjectFilter>,
    source_filter: Option<&StaticDamageSourceFilter>,
    dmg_src: Option<ObjectId>,
    dmg_tgt: TargetRef,
    ctx: &EvalContext<'_>,
) -> bool {
    let tgt_ok = match target_spec {
        StaticDamageTarget::SelfObj => {
            dmg_tgt == TargetRef::Object { id: src_id }
                || (also_dealt_by && dmg_src == Some(src_id))
        }
        StaticDamageTarget::AttachedTo => {
            let host = state.object(src_id).and_then(|o| o.attached_to);
            host.is_some_and(|h| {
                dmg_tgt == TargetRef::Object { id: h } || (also_dealt_by && dmg_src == Some(h))
            })
        }
        StaticDamageTarget::You => dmg_tgt == TargetRef::Player { id: src_ctrl },
        StaticDamageTarget::YouAndControlled => match dmg_tgt {
            TargetRef::Player { id } => id == src_ctrl,
            TargetRef::Object { id } => cache.get(id).map_or_else(
                || state.object(id).is_some_and(|o| o.controller == src_ctrl),
                |e| e.controller == src_ctrl,
            ),
            TargetRef::Stack { .. } => false,
        },
        StaticDamageTarget::Any => true,
        StaticDamageTarget::Filter { filter } => match dmg_tgt {
            TargetRef::Object { id } => {
                matches_object_filter(state, id, filter, ctx)
                    || (also_dealt_by
                        && dmg_src.is_some_and(|s| matches_object_filter(state, s, filter, ctx)))
            }
            _ => false,
        },
    };
    if !tgt_ok {
        return false;
    }
    if let Some(aff) = dealt_by_only {
        let Some(s) = dmg_src else {
            return false;
        };
        if !matches_affected(state, s, aff, ctx) {
            return false;
        }
    }
    if let Some(tf) = dealt_by_target_filter {
        match dmg_tgt {
            TargetRef::Object { id } => {
                if !matches_object_filter(state, id, tf, ctx) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    if let Some(sf) = source_filter {
        let Some(s) = dmg_src else {
            return false;
        };
        let sf_ok = match sf {
            StaticDamageSourceFilter::Filter { filter } => {
                matches_object_filter(state, s, filter, ctx)
            }
            StaticDamageSourceFilter::BlockingOrBlockedBySelf => {
                state.combat().blockers.iter().any(|b| {
                    (b.attacker == src_id && b.blocker == s)
                        || (b.blocker == src_id && b.attacker == s)
                })
            }
            StaticDamageSourceFilter::BlockingSelf => state
                .combat()
                .blockers
                .iter()
                .any(|b| b.attacker == src_id && b.blocker == s),
            StaticDamageSourceFilter::BlockedBySelf => state
                .combat()
                .blockers
                .iter()
                .any(|b| b.blocker == src_id && b.attacker == s),
        };
        if !sf_ok {
            return false;
        }
    }
    true
}

// ---- Candidate Application ---------------------------------------------------------------------

enum ApplyStep {
    Continue,
    Paused { prompt_id: u32, chooser: PlayerId },
}

#[allow(clippy::too_many_arguments)]
fn apply_candidate(
    state: &mut GameState,
    cand: Candidate,
    event: &mut ReplaceableEventKind,
    mods: &mut ReplacementAccumulatedMods,
    applied: &mut SmallVec<[ReplacementSourceId; 4]>,
    continuation: ReplacementContinuation,
    resume_decision: DecisionState,
    resume_priority_player: PlayerId,
) -> Result<ApplyStep, EngineError> {
    let sid = cand.source_id;
    match cand.kind {
        CandidateKind::ReplacementDef {
            def,
            source_controller,
        } => {
            match &def.with {
                ReplacementWith::Skip => {
                    mods.skipped = true;
                }
                ReplacementWith::Execute { steps } => {
                    mods.execute_instead = Some(steps.clone());
                }
                ReplacementWith::RegenerateSelfOnDestroy => {
                    mods.regenerate_instead = true;
                }
                ReplacementWith::ZoneChange { mod_spec } => {
                    let step = apply_zone_change_mod(
                        state,
                        sid,
                        source_controller,
                        mod_spec,
                        event,
                        mods,
                        applied,
                        continuation,
                        resume_decision,
                        resume_priority_player,
                    )?;
                    match step {
                        ZoneModStep::Done => {
                            mods.reset_zone_mod_progress();
                        }
                        ZoneModStep::Paused { prompt_id, chooser } => {
                            return Ok(ApplyStep::Paused { prompt_id, chooser });
                        }
                    }
                }
                ReplacementWith::DamageScale { spec } => {
                    if let ReplaceableEventKind::Damage { amount, .. } = event {
                        let cache = state.evaluate_board();
                        let ctx = EvalContext::for_source(source_controller, sid.object)
                            .with_board(BoardView::Cached(cache));
                        match spec.mode {
                            DamageModMode::Double => *amount = amount.saturating_mul(2),
                            DamageModMode::Triple => *amount = amount.saturating_mul(3),
                            DamageModMode::Add => {
                                let delta = spec
                                    .add
                                    .as_deref()
                                    .map_or(0, |a| evaluate_amount(state, a, &ctx))
                                    .max(0) as u32;
                                *amount = amount.saturating_add(delta);
                            }
                        }
                    }
                }
                ReplacementWith::DamageToMinusCounters => {
                    mods.damage_as_minus_counters = true;
                }
                ReplacementWith::DamageToSelfAsCounters { counter } => {
                    mods.damage_to_self_as_counter = Some(*counter);
                }
                ReplacementWith::LifeGainScale { spec } => {
                    if let ReplaceableEventKind::LifeGain { amount, .. } = event {
                        if spec.double {
                            *amount = amount.saturating_mul(2);
                        }
                        if let Some(add_amt) = &spec.add {
                            let cache = state.evaluate_board();
                            let ctx = EvalContext::for_source(source_controller, sid.object)
                                .with_board(BoardView::Cached(cache));
                            let delta = evaluate_amount(state, add_amt, &ctx).max(0) as u32;
                            *amount = amount.saturating_add(delta);
                        }
                    }
                }
                ReplacementWith::DrawScale { spec } => {
                    if let ReplaceableEventKind::Draw { count, .. } = event {
                        if spec.skip_first_in_your_draw_step {
                            mods.skipped = true;
                            *count = 0;
                        } else {
                            if let Some(delta) = spec.delta {
                                *count = (*count as i32 + i32::from(delta)).max(0) as u32;
                            }
                            if let Some(factor) = spec.factor {
                                *count = count.saturating_mul(u32::from(factor));
                            }
                        }
                    }
                }
                ReplacementWith::StealOpponentExtraDraws => {
                    mods.draw_stealer = Some(source_controller);
                    mods.skipped = true;
                }
                ReplacementWith::AbundanceDrawChoice => {
                    if mods.abundance_land.is_none() {
                        let prompt_id = state.allocate_prompt_id();
                        let chooser = match event {
                            ReplaceableEventKind::Draw { player, .. } => *player,
                            _ => source_controller,
                        };
                        state.set_pending_replacement(Some(PendingReplacementState {
                            prompt_id,
                            chooser,
                            event: event.clone(),
                            applied: applied.clone(),
                            candidates: vec![sid],
                            prompt_kind: PendingReplacementPrompt::AbundanceChoice {
                                source_id: sid,
                            },
                            mods: mods.clone(),
                            continuation,
                            resume_decision,
                            resume_priority_player,
                        }));
                        state.set_decision(DecisionState::Prompt {
                            request: ChoiceRequest::YesNo {
                                prompt_id,
                                label: "Abundance: choose Land (Yes) or Nonland (No)".into(),
                            },
                        });
                        state.set_priority_player(chooser);
                        return Ok(ApplyStep::Paused { prompt_id, chooser });
                    }
                }
                ReplacementWith::EmptyLibraryGraveyardReanimate {
                    filter,
                    return_other_cards_to_hand,
                } => {
                    mods.reanimate_on_empty_draw =
                        Some((filter.clone(), *return_other_cards_to_hand));
                    mods.skipped = true;
                }
                ReplacementWith::ProliferateMultiplier { factor } => {
                    if let ReplaceableEventKind::Proliferate { count, .. } = event {
                        let mult = u32::from((*factor).clamp(1, 16));
                        *count = count.saturating_mul(mult);
                    }
                }
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::Multiplier {
            source_controller,
            factor,
            delta,
            round,
            token_filter,
            extra_token,
            extra_token_matching_count,
        } => {
            match event {
                ReplaceableEventKind::PutCounters { count, .. }
                | ReplaceableEventKind::PutPlayerCounters { count, .. } => {
                    *count = scale_count(*count, factor, delta, round);
                }
                ReplaceableEventKind::CreateToken {
                    controller: tok_ctrl,
                    count,
                    ..
                } => {
                    *count = scale_count(*count, factor, delta, round);
                    if !mods.replace_tokens.is_empty() {
                        let cache = state.evaluate_board();
                        let ctx = EvalContext::for_source(source_controller, sid.object)
                            .with_board(BoardView::Cached(cache));
                        let tok_owner = mods
                            .token_opts
                            .as_ref()
                            .and_then(|o| o.owner)
                            .unwrap_or(*tok_ctrl);
                        let tapped_opt = mods.token_opts.as_ref().is_some_and(|o| o.tapped);
                        for (rep_spec, c) in &mut mods.replace_tokens {
                            if token_filter.as_deref().is_none_or(|f| {
                                matches_token_spec_filter(
                                    state, *tok_ctrl, tok_owner, tapped_opt, rep_spec, f, &ctx,
                                )
                            }) {
                                *c = scale_count(*c, factor, delta, round);
                            }
                        }
                    }
                    if let Some(tok) = extra_token {
                        let extra_cnt = if extra_token_matching_count {
                            if !mods.replace_tokens.is_empty() {
                                mods.replace_tokens.iter().map(|(_, c)| *c).sum()
                            } else {
                                *count
                            }
                        } else {
                            1
                        };
                        if extra_cnt > 0 {
                            mods.extra_tokens.push((*tok, extra_cnt));
                        }
                    }
                }
                _ => {}
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::TokenReplacement {
            one_of_each,
            match_subtypes,
            replacement_token,
        } => {
            if let ReplaceableEventKind::CreateToken { spec, count, .. } = event {
                if one_of_each {
                    // Academy Manufactor: 1 Clue, 1 Food, 1 Treasure per matching token.
                    if mods.replace_tokens.is_empty() {
                        let n = *count;
                        let tapped = spec.as_deref().is_some_and(|s| s.tapped);
                        if n > 0 {
                            mods.replace_tokens = academy_manufactor_tokens(state.db())
                                .into_iter()
                                .map(|mut t| {
                                    t.tapped = tapped;
                                    (t, n)
                                })
                                .collect();
                        }
                    } else {
                        let base_set = academy_manufactor_tokens(state.db());
                        let mut next: Vec<(TokenSpec, u32)> =
                            Vec::with_capacity(mods.replace_tokens.len() * 3);
                        for (entry_spec, entry_cnt) in mods.replace_tokens.drain(..) {
                            if token_spec_matches_replacement(&entry_spec, true, &match_subtypes) {
                                for mut t in base_set.clone() {
                                    t.tapped = entry_spec.tapped;
                                    if let Some(existing) = next.iter_mut().find(|(s, _)| *s == t) {
                                        existing.1 = existing.1.saturating_add(entry_cnt);
                                    } else {
                                        next.push((t, entry_cnt));
                                    }
                                }
                            } else if let Some(existing) =
                                next.iter_mut().find(|(s, _)| *s == entry_spec)
                            {
                                existing.1 = existing.1.saturating_add(entry_cnt);
                            } else {
                                next.push((entry_spec, entry_cnt));
                            }
                        }
                        mods.replace_tokens = next;
                    }
                } else if let Some(rep_tok) = replacement_token {
                    if !mods.replace_tokens.is_empty() {
                        for (entry_spec, _) in &mut mods.replace_tokens {
                            if token_spec_matches_replacement(entry_spec, false, &match_subtypes) {
                                let tapped = entry_spec.tapped;
                                let mut new_spec = (*rep_tok).clone();
                                new_spec.tapped = tapped || new_spec.tapped;
                                *entry_spec = new_spec;
                            }
                        }
                    } else if let Some(orig) = spec.as_deref_mut() {
                        let tapped = orig.tapped;
                        let mut new_spec = (*rep_tok).clone();
                        new_spec.tapped = tapped || new_spec.tapped;
                        *orig = new_spec;
                    } else {
                        *spec = Some(rep_tok);
                    }
                }
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::StaticPrevention {
            amount: prev_amt,
            on_prevented,
            phantom_counter_removal,
            unpreventable_only_phantom,
        } => {
            if let ReplaceableEventKind::Damage { amount, target, .. } = event {
                let would_prevent = match prev_amt {
                    StaticDamageAmount::Fixed { n } => (*amount).min(u32::from(n)),
                    StaticDamageAmount::All => *amount,
                    StaticDamageAmount::HalfRoundedUp => (*amount).div_ceil(2),
                };
                if let Some(phantom) = &phantom_counter_removal
                    && phantom.require_counter_present
                    && !matches!(
                        target,
                        TargetRef::Object { id } if state.counter_count(*id, phantom.counter) > 0
                    )
                {
                    if !applied.contains(&sid) {
                        applied.push(sid);
                    }
                    return Ok(ApplyStep::Continue);
                }
                let mut prevented = 0u32;
                if !unpreventable_only_phantom {
                    prevented = would_prevent;
                    *amount -= prevented;
                    mods.prevented_damage = mods.prevented_damage.saturating_add(prevented);
                }
                if let Some(phantom) = phantom_counter_removal
                    && let TargetRef::Object { id: tgt_id } = target
                {
                    let to_remove = if phantom.remove_that_many {
                        would_prevent
                    } else {
                        1
                    };
                    let prev = state.counter_count(*tgt_id, phantom.counter);
                    let rem = state.remove_counter(*tgt_id, phantom.counter, to_remove);
                    if rem > 0
                        && state
                            .object(*tgt_id)
                            .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
                    {
                        crate::events::emit_event(
                            state,
                            crate::events::AtomicEventKind::CounterChanged {
                                object: *tgt_id,
                                counter: phantom.counter,
                                previous_count: prev,
                                new_count: prev - rem,
                                actor: None,
                            },
                        );
                    }
                }
                if prevented > 0
                    && let Some(rider) = on_prevented
                {
                    // Record synthetic rider if needed; counters handled directly on permanent if applicable.
                    if matches!(rider, mtg_ir::StaticOnPrevented::PutPlusCountersPerDamage)
                        && let Some(plus_cid) = state.db().counter_id("+1/+1")
                    {
                        state.add_counter(sid.object, plus_cid, prevented);
                    }
                }
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::StaticRedirection { redirect_target } => {
            if let ReplaceableEventKind::Damage { target, .. } = event {
                *target = redirect_target;
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::PreventionShield { shield_id } => {
            if let ReplaceableEventKind::Damage { amount, target, .. } = event
                && let Some(pos) = state
                    .prevention_shields()
                    .iter()
                    .position(|s| s.id == shield_id)
            {
                let shield = state.prevention_shields()[pos].clone();
                let prevented = if let Some(floor) = shield.min_life_floor {
                    if let TargetRef::Player { id } = target
                        && let Some(p) = state.player(*id)
                    {
                        let allowed_loss = (p.life - floor).max(0) as u32;
                        (*amount).saturating_sub(allowed_loss)
                    } else {
                        0
                    }
                } else {
                    match shield.remaining {
                        ShieldRemaining::Infinite => *amount,
                        ShieldRemaining::Finite(rem) => (*amount).min(rem),
                    }
                };
                *amount -= prevented;
                mods.prevented_damage = mods.prevented_damage.saturating_add(prevented);
                if let Some(redir) = shield.redirect_to
                    && prevented > 0
                {
                    mods.redirected_damage.push((redir, prevented));
                }
                if prevented > 0
                    && (shield.on_prevented.is_some()
                        || shield.on_prevented_creature.is_some()
                        || shield.on_prevented_noncreature.is_some())
                {
                    mods.prevented_riders.push((shield.clone(), prevented));
                }

                let remove_shield = shield.next_event_only
                    || matches!(shield.remaining, ShieldRemaining::Finite(rem) if rem <= prevented);
                if remove_shield {
                    state.prevention_shields_mut().remove(pos);
                } else if let ShieldRemaining::Finite(rem) =
                    &mut state.prevention_shields_mut()[pos].remaining
                {
                    *rem -= prevented;
                }
                state.mark_dirty(DirtyMask::COMBAT | DirtyMask::PLAYER_STATE);
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::KeywordAbsorb { n } => {
            if let ReplaceableEventKind::Damage { amount, .. } = event {
                let prevented = (*amount).min(n);
                *amount -= prevented;
                mods.prevented_damage = mods.prevented_damage.saturating_add(prevented);
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::FinalityCounter | CandidateKind::ExileInsteadOfDyingEffect => {
            if let ReplaceableEventKind::ZoneChange { to, .. } = event {
                *to = Zone::Exile;
                mods.enter_tapped = None;
            }
            if !applied.contains(&sid) {
                applied.push(sid);
            }
        }
        CandidateKind::CommanderRedirect => {
            if let ReplaceableEventKind::ZoneChange { object, to, .. } = event {
                let intended_zone = *to;
                let owner = state.object(*object).map_or(PlayerId(0), |o| o.owner);
                let prompt_id = state.allocate_prompt_id();
                state.set_pending_replacement(Some(PendingReplacementState {
                    prompt_id,
                    chooser: owner,
                    event: event.clone(),
                    applied: applied.clone(),
                    candidates: vec![sid],
                    prompt_kind: PendingReplacementPrompt::CommanderRedirect { intended_zone },
                    mods: mods.clone(),
                    continuation,
                    resume_decision,
                    resume_priority_player,
                }));
                state.set_decision(DecisionState::Prompt {
                    request: ChoiceRequest::YesNo {
                        prompt_id,
                        label: "Put commander into command zone instead?".into(),
                    },
                });
                state.set_priority_player(owner);
                return Ok(ApplyStep::Paused {
                    prompt_id,
                    chooser: owner,
                });
            }
        }
    }
    Ok(ApplyStep::Continue)
}

enum ZoneModStep {
    Done,
    Paused { prompt_id: u32, chooser: PlayerId },
}

#[allow(clippy::too_many_arguments)]
fn apply_zone_change_mod(
    state: &mut GameState,
    sid: ReplacementSourceId,
    source_controller: PlayerId,
    mod_spec: &mtg_ir::ZoneChangeMod,
    event: &mut ReplaceableEventKind,
    mods: &mut ReplacementAccumulatedMods,
    applied: &SmallVec<[ReplacementSourceId; 4]>,
    continuation: ReplacementContinuation,
    resume_decision: DecisionState,
    resume_priority_player: PlayerId,
) -> Result<ZoneModStep, EngineError> {
    let ReplaceableEventKind::ZoneChange {
        object,
        from,
        to,
        controller,
    } = event
    else {
        return Ok(ZoneModStep::Done);
    };
    let moving_id = *object;

    if !mods.etb_pre_done {
        if let Some(new_to) = mod_spec.to {
            *to = new_to;
            if new_to != Zone::Battlefield {
                mods.enter_tapped = None;
            }
        }
        if mod_spec.shuffle_into_library {
            mods.shuffle_into_library = true;
        }
        if let Some(ex_cid) = mod_spec.exile_with_counter {
            mods.exile_with_counter = Some(ex_cid);
        }
        if mod_spec.enters_prepared {
            mods.enters_prepared = true;
        }
        if mod_spec.becomes_day_if_neither {
            mods.becomes_day_if_neither = true;
        }
        if mod_spec.exile_and_extra_turn_on_bf_to_gy {
            *to = Zone::Exile;
            mods.exile_and_extra_turn_on_bf_to_gy = true;
        }
        if let Some(dies_spec) = &mod_spec.dies_exile_and_reflexive {
            *to = Zone::Exile;
            mods.dies_exile_and_reflexive = Some(dies_spec.clone());
        }
        if mod_spec.opponent_discard_to_battlefield.is_some() {
            *to = Zone::Battlefield;
            *controller = source_controller;
            if let Some(mtg_ir::OpponentDiscardToBattlefield::WithCounters { counter, count }) =
                mod_spec.opponent_discard_to_battlefield
            {
                mods.extra_counters.push((counter, u32::from(count)));
            }
        }
        mods.etb_pre_done = true;
    }

    // CR 616.1b: enter under opponent's control (changes chooser for Tier 3+!).
    if mod_spec.enter_under_opponent_control && !mods.etb_steal_done {
        let cur_ctrl = *controller;
        let opps: Vec<PlayerId> = state
            .players()
            .iter()
            .filter(|p| !p.lost && p.id != cur_ctrl)
            .map(|p| p.id)
            .collect();
        if opps.len() == 1 {
            *controller = opps[0];
            if let Some(opts) = &mut mods.move_opts {
                opts.controller = Some(opps[0]);
            }
            mods.etb_steal_done = true;
        } else if opps.len() > 1 {
            let prompt_id = state.allocate_prompt_id();
            state.set_pending_replacement(Some(PendingReplacementState {
                prompt_id,
                chooser: cur_ctrl,
                event: event.clone(),
                applied: applied.clone(),
                candidates: vec![sid],
                prompt_kind: PendingReplacementPrompt::EtbChoice {
                    source_id: sid,
                    source_controller,
                    mod_spec: Box::new(mod_spec.clone()),
                    step: EtbInteractiveStep::ChooseOpponentForControl {
                        candidates: opps.clone(),
                    },
                    step_idx: 1000,
                },
                mods: mods.clone(),
                continuation,
                resume_decision,
                resume_priority_player,
            }));
            state.set_decision(DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode {
                    prompt_id,
                    option_count: opps.len() as u16,
                    min: 1,
                    max: 1,
                },
            });
            state.set_priority_player(cur_ctrl);
            return Ok(ZoneModStep::Paused {
                prompt_id,
                chooser: cur_ctrl,
            });
        } else {
            mods.etb_steal_done = true;
        }
    }

    // CR 616.1c: ETB Copy replacement.
    if !mods.etb_copy_done
        && let Some(copy_spec) = &mod_spec.copy
    {
        let mut copy_candidates = {
            let cache = state.evaluate_board();
            let ctx = etb_eval_context(state, *controller, moving_id, cache);
            let mut found = find_objects(state, &copy_spec.filter, &ctx);
            if copy_spec.from_graveyards {
                let mut gy_filter = (*copy_spec.filter).clone();
                gy_filter.zones = ZoneMask::GRAVEYARD;
                found.extend(find_objects(state, &gy_filter, &ctx));
            }
            found.retain(|&id| id != moving_id);
            found
        };
        copy_candidates.dedup();
        if !copy_candidates.is_empty() {
            let prompt_id = state.allocate_prompt_id();
            let chooser = *controller;
            let min = if copy_spec.optional { 0 } else { 1 };
            state.set_pending_replacement(Some(PendingReplacementState {
                prompt_id,
                chooser,
                event: event.clone(),
                applied: applied.clone(),
                candidates: vec![sid],
                prompt_kind: PendingReplacementPrompt::EtbCopy {
                    source_id: sid,
                    source_controller,
                    mod_spec: Box::new(mod_spec.clone()),
                    candidates: copy_candidates.clone(),
                    optional: copy_spec.optional,
                    tapped_if_copied: copy_spec.tapped_if_copied,
                    modifications: copy_spec.modifications.clone(),
                },
                mods: mods.clone(),
                continuation,
                resume_decision,
                resume_priority_player,
            }));
            state.set_decision(DecisionState::Prompt {
                request: ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates: copy_candidates,
                    min,
                    max: 1,
                },
            });
            state.set_priority_player(chooser);
            return Ok(ZoneModStep::Paused { prompt_id, chooser });
        }
        mods.etb_copy_done = true;
    }

    // Interactive ETB choices (`etb_choices`, discard-for-keyword-counters, remove-counters-for-+1/+1,
    // `pay_life_or_enter_tapped`, `reveal_from_hand_or_enter_tapped`).
    for (idx, choice_spec) in mod_spec.etb_choices.iter().enumerate() {
        let step_idx = idx as u16;
        if mods.etb_answered_steps.contains(&step_idx) {
            continue;
        }
        if let Some(step) = build_etb_interactive_step(state, *controller, choice_spec) {
            return pause_etb_choice_step(
                state,
                *controller,
                event.clone(),
                applied.clone(),
                mods.clone(),
                continuation.clone(),
                resume_decision,
                resume_priority_player,
                sid,
                source_controller,
                Box::new(mod_spec.clone()),
                step,
                step_idx,
            );
        }
        mods.etb_answered_steps.push(step_idx);
    }

    if !mod_spec.etb_discard_for_keyword_counters.is_empty()
        && !mods.etb_answered_steps.contains(&502)
    {
        let hand_cands: Vec<ObjectId> = {
            let cache = state.evaluate_board();
            state
                .player(*controller)
                .map(|p| {
                    p.hand
                        .iter()
                        .copied()
                        .filter(|&id| {
                            id != moving_id
                                && cache
                                    .get(id)
                                    .map_or_else(|| base_types_quick(state, id), |e| e.types)
                                    .contains(CardTypeMask::CREATURE)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        if !hand_cands.is_empty() {
            return pause_etb_choice_step(
                state,
                *controller,
                event.clone(),
                applied.clone(),
                mods.clone(),
                continuation.clone(),
                resume_decision,
                resume_priority_player,
                sid,
                source_controller,
                Box::new(mod_spec.clone()),
                EtbInteractiveStep::DiscardForKeywordCounters {
                    candidates: hand_cands,
                    tracked_keywords: mod_spec.etb_discard_for_keyword_counters.clone(),
                    picked: Vec::new(),
                },
                502,
            );
        } else {
            mods.etb_answered_steps.push(502);
        }
    }

    if let Some((multiplier, ref filter)) = mod_spec.etb_remove_counters_for_plus_counters
        && !mods.etb_answered_steps.contains(&503)
    {
        let cands: Vec<ObjectId> =
            with_eval_on_bf_context(state, moving_id, *from, *controller, mods, |state| {
                let cache = state.evaluate_board();
                let ctx = etb_eval_context(state, *controller, moving_id, cache);
                let mut out_ids = Vec::new();
                for p in state.players() {
                    if p.lost {
                        continue;
                    }
                    for &perm_id in &p.battlefield {
                        if perm_id == moving_id {
                            continue;
                        }
                        if state.object(perm_id).is_some_and(|o| o.is_phased_out()) {
                            continue;
                        }
                        if !state.counters(perm_id).iter().any(|&(_, c)| c > 0) {
                            continue;
                        }
                        if filter
                            .as_deref()
                            .is_none_or(|f| matches_object_filter(state, perm_id, f, &ctx))
                        {
                            out_ids.push(perm_id);
                        }
                    }
                }
                out_ids
            });
        if !cands.is_empty() {
            return pause_etb_choice_step(
                state,
                *controller,
                event.clone(),
                applied.clone(),
                mods.clone(),
                continuation.clone(),
                resume_decision,
                resume_priority_player,
                sid,
                source_controller,
                Box::new(mod_spec.clone()),
                EtbInteractiveStep::RemoveCountersForPlusCounters {
                    candidates: cands,
                    multiplier,
                    picked: Vec::new(),
                },
                503,
            );
        } else {
            mods.etb_answered_steps.push(503);
        }
    }

    if let Some(life) = mod_spec.pay_life_or_enter_tapped {
        let step_idx = 500u16;
        if !mods.etb_answered_steps.contains(&step_idx) {
            let can_pay = can_player_pay_life(state, *controller, life);
            if can_pay {
                return pause_etb_choice_step(
                    state,
                    *controller,
                    event.clone(),
                    applied.clone(),
                    mods.clone(),
                    continuation.clone(),
                    resume_decision,
                    resume_priority_player,
                    sid,
                    source_controller,
                    Box::new(mod_spec.clone()),
                    EtbInteractiveStep::PayLifeOrEnterTapped { life },
                    step_idx,
                );
            } else {
                mods.enter_tapped = Some(true);
                mods.etb_answered_steps.push(step_idx);
            }
        }
    }

    if let Some(reveal_filter) = &mod_spec.reveal_from_hand_or_enter_tapped {
        let step_idx = 501u16;
        if !mods.etb_answered_steps.contains(&step_idx) {
            let hand_cands: Vec<ObjectId> = {
                let cache = state.evaluate_board();
                let ctx = etb_eval_context(state, *controller, moving_id, cache);
                state
                    .player(*controller)
                    .map(|p| {
                        p.hand
                            .iter()
                            .copied()
                            .filter(|&id| {
                                id != moving_id
                                    && matches_object_filter(state, id, reveal_filter, &ctx)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if !hand_cands.is_empty() {
                return pause_etb_choice_step(
                    state,
                    *controller,
                    event.clone(),
                    applied.clone(),
                    mods.clone(),
                    continuation,
                    resume_decision,
                    resume_priority_player,
                    sid,
                    source_controller,
                    Box::new(mod_spec.clone()),
                    EtbInteractiveStep::RevealFromHandOrEnterTapped {
                        candidates: hand_cands,
                    },
                    step_idx,
                );
            } else {
                mods.enter_tapped = Some(true);
                mods.etb_answered_steps.push(step_idx);
            }
        }
    }

    if let Some(ctrl_filter) = &mod_spec.reveal_unless_controls {
        let controls =
            with_eval_on_bf_context(state, moving_id, *from, *controller, mods, |state| {
                let cache = state.evaluate_board();
                let ctx = etb_eval_context(state, *controller, moving_id, cache);
                state.player(*controller).is_some_and(|p| {
                    p.battlefield.iter().any(|&id| {
                        id != moving_id && matches_object_filter(state, id, ctrl_filter, &ctx)
                    })
                })
            });
        if !controls {
            mods.enter_tapped = Some(true);
        }
    }

    if let Some(tapped) = mod_spec.tapped {
        mods.enter_tapped = Some(tapped);
    }

    // Evaluate ETB counter placements under CR 614.12 lookahead.
    if mod_spec.counters.is_some()
        || mod_spec.additional_counters.is_some()
        || mod_spec.conditional_counters.is_some()
        || mod_spec.conditional_or_fallback_counters.is_some()
        || mod_spec.counters_per_artifact_mana.is_some()
    {
        let evaluated_counters =
            with_eval_on_bf_context(state, moving_id, *from, *controller, mods, |state| {
                let mut added: SmallVec<[(CounterId, u32); 2]> = SmallVec::new();
                let cache = state.evaluate_board();
                let ctx = etb_eval_context(state, *controller, moving_id, cache);
                if let Some((cid, amt)) = &mod_spec.counters {
                    let n = evaluate_amount(state, amt, &ctx).max(0) as u32;
                    if n > 0 {
                        added.push((*cid, n));
                    }
                }
                if let Some((cid, amt)) = &mod_spec.additional_counters {
                    let n = evaluate_amount(state, amt, &ctx).max(0) as u32;
                    if n > 0 {
                        added.push((*cid, n));
                    }
                }
                if let Some((cond, cid, amt)) = &mod_spec.conditional_counters
                    && evaluate_condition(state, cond, &ctx)
                {
                    let n = evaluate_amount(state, amt, &ctx).max(0) as u32;
                    if n > 0 {
                        added.push((*cid, n));
                    }
                }
                if let Some((cond, cid, then_n, else_n)) =
                    &mod_spec.conditional_or_fallback_counters
                {
                    let n = if evaluate_condition(state, cond, &ctx) {
                        u32::from(*then_n)
                    } else {
                        u32::from(*else_n)
                    };
                    if n > 0 {
                        added.push((*cid, n));
                    }
                }
                if let Some(cid) = mod_spec.counters_per_artifact_mana {
                    let n = state.rare(moving_id).map_or(0, |r| r.mana_spent_count);
                    if n > 0 {
                        added.push((cid, n));
                    }
                }
                added
            });
        mods.extra_counters.extend(evaluated_counters);
    }

    Ok(ZoneModStep::Done)
}

fn etb_eval_context<'a>(
    state: &GameState,
    controller: PlayerId,
    object: ObjectId,
    cache: &'a LayerCache,
) -> EvalContext<'a> {
    let mut ctx = EvalContext::for_source(controller, object).with_board(BoardView::Cached(cache));
    ctx.x_value = state.rare(object).and_then(|r| r.entered_with_x);
    ctx
}

fn with_eval_on_bf_context<R>(
    state: &mut GameState,
    id: ObjectId,
    from: Zone,
    controller: PlayerId,
    mods: &ReplacementAccumulatedMods,
    f: impl FnOnce(&GameState) -> R,
) -> R {
    if from != Zone::Battlefield {
        with_hypothetical_battlefield_presence(state, id, controller, mods, f)
    } else {
        f(state)
    }
}

#[allow(clippy::too_many_arguments)]
fn pause_etb_choice(
    state: &mut GameState,
    chooser: PlayerId,
    event: ReplaceableEventKind,
    applied: SmallVec<[ReplacementSourceId; 4]>,
    mods: ReplacementAccumulatedMods,
    continuation: ReplacementContinuation,
    resume_decision: DecisionState,
    resume_priority_player: PlayerId,
    source_id: ReplacementSourceId,
    source_controller: PlayerId,
    mod_spec: Box<mtg_ir::ZoneChangeMod>,
    step: EtbInteractiveStep,
    step_idx: u16,
) -> Result<ReplacementResult, EngineError> {
    let prompt_id = state.allocate_prompt_id();
    let req = choice_request_for_etb_step(prompt_id, &step);
    state.set_pending_replacement(Some(PendingReplacementState {
        prompt_id,
        chooser,
        event,
        applied,
        candidates: vec![source_id],
        prompt_kind: PendingReplacementPrompt::EtbChoice {
            source_id,
            source_controller,
            mod_spec,
            step,
            step_idx,
        },
        mods,
        continuation,
        resume_decision,
        resume_priority_player,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(ReplacementResult::Paused { prompt_id, chooser })
}

#[allow(clippy::too_many_arguments)]
fn pause_etb_choice_step(
    state: &mut GameState,
    chooser: PlayerId,
    event: ReplaceableEventKind,
    applied: SmallVec<[ReplacementSourceId; 4]>,
    mods: ReplacementAccumulatedMods,
    continuation: ReplacementContinuation,
    resume_decision: DecisionState,
    resume_priority_player: PlayerId,
    source_id: ReplacementSourceId,
    source_controller: PlayerId,
    mod_spec: Box<mtg_ir::ZoneChangeMod>,
    step: EtbInteractiveStep,
    step_idx: u16,
) -> Result<ZoneModStep, EngineError> {
    let prompt_id = state.allocate_prompt_id();
    let req = choice_request_for_etb_step(prompt_id, &step);
    state.set_pending_replacement(Some(PendingReplacementState {
        prompt_id,
        chooser,
        event,
        applied,
        candidates: vec![source_id],
        prompt_kind: PendingReplacementPrompt::EtbChoice {
            source_id,
            source_controller,
            mod_spec,
            step,
            step_idx,
        },
        mods,
        continuation,
        resume_decision,
        resume_priority_player,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(ZoneModStep::Paused { prompt_id, chooser })
}

fn choice_request_for_etb_step(prompt_id: u32, step: &EtbInteractiveStep) -> ChoiceRequest {
    match step {
        EtbInteractiveStep::PayLifeOrEnterTapped { life } => ChoiceRequest::YesNo {
            prompt_id,
            label: format!("Pay {life} life to enter untapped?"),
        },
        EtbInteractiveStep::RevealFromHandOrEnterTapped { candidates } => {
            ChoiceRequest::ChooseObjects {
                prompt_id,
                candidates: candidates.clone(),
                min: 0,
                max: 1,
            }
        }
        EtbInteractiveStep::ChooseObjectOnBattlefield {
            candidates,
            optional,
        } => ChoiceRequest::ChooseObjects {
            prompt_id,
            candidates: candidates.clone(),
            min: if *optional { 0 } else { 1 },
            max: 1,
        },
        EtbInteractiveStep::DiscardForKeywordCounters {
            candidates, picked, ..
        }
        | EtbInteractiveStep::RemoveCountersForPlusCounters {
            candidates, picked, ..
        } => {
            let rem: Vec<ObjectId> = candidates
                .iter()
                .copied()
                .filter(|id| !picked.contains(id))
                .collect();
            let max = rem.len() as u16;
            ChoiceRequest::ChooseObjects {
                prompt_id,
                candidates: rem,
                min: 0,
                max,
            }
        }
        EtbInteractiveStep::ChooseNumber { min, max } => ChoiceRequest::ChooseNumber {
            prompt_id,
            min: *min,
            max: *max,
        },
        EtbInteractiveStep::ChooseSubType { options } => ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: options.len() as u16,
            min: 1,
            max: 1,
        },
        EtbInteractiveStep::ChooseColor { colors }
        | EtbInteractiveStep::ChooseTwoColors { colors, .. } => ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: colors.len() as u16,
            min: 1,
            max: 1,
        },
        EtbInteractiveStep::ChooseCardName { candidates } => ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: candidates.len() as u16,
            min: 1,
            max: 1,
        },
        EtbInteractiveStep::ChooseCounter { options } => ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: options.len() as u16,
            min: 1,
            max: 1,
        },
        EtbInteractiveStep::ChooseAnchorWord { .. } | EtbInteractiveStep::ChooseParity => {
            ChoiceRequest::ChooseMode {
                prompt_id,
                option_count: 2,
                min: 1,
                max: 1,
            }
        }
        EtbInteractiveStep::ChoosePlayer { candidates }
        | EtbInteractiveStep::ChooseTwoPlayers { candidates, .. }
        | EtbInteractiveStep::ChooseOpponentForControl { candidates } => {
            ChoiceRequest::ChooseMode {
                prompt_id,
                option_count: candidates.len() as u16,
                min: 1,
                max: 1,
            }
        }
        EtbInteractiveStep::ChooseCardType { types } => ChoiceRequest::ChooseMode {
            prompt_id,
            option_count: types.len() as u16,
            min: 1,
            max: 1,
        },
    }
}

fn reveal_opponent_hand_to_chooser(state: &mut GameState, chooser: PlayerId) {
    let opp_hand: Vec<ObjectId> = state
        .players()
        .iter()
        .find(|p| !p.lost && p.id != chooser && !p.hand.is_empty())
        .map(|p| p.hand.clone())
        .unwrap_or_default();
    for hid in opp_hand {
        crate::zones::reveal_to_player(state, hid, chooser);
    }
}

fn build_etb_interactive_step(
    state: &mut GameState,
    chooser: PlayerId,
    spec: &EtbChoiceSpec,
) -> Option<EtbInteractiveStep> {
    match spec {
        EtbChoiceSpec::SubType { scope } => {
            let opts = collect_subtype_options_for_scope(state, *scope);
            if opts.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseSubType { options: opts })
            }
        }
        EtbChoiceSpec::SubTypeOptions { subtypes } => {
            if subtypes.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseSubType {
                    options: subtypes.to_vec(),
                })
            }
        }
        EtbChoiceSpec::Color { exclude } => {
            let colors: Vec<ManaColor> = [
                ManaColor::W,
                ManaColor::U,
                ManaColor::B,
                ManaColor::R,
                ManaColor::G,
            ]
            .into_iter()
            .filter(|c| Some(*c) != *exclude)
            .collect();
            Some(EtbInteractiveStep::ChooseColor { colors })
        }
        EtbChoiceSpec::TwoColors => Some(EtbInteractiveStep::ChooseTwoColors {
            first: None,
            colors: vec![
                ManaColor::W,
                ManaColor::U,
                ManaColor::B,
                ManaColor::R,
                ManaColor::G,
            ],
        }),
        EtbChoiceSpec::CardName {
            nonland_only,
            look_at_opponent_hand,
        } => {
            if *look_at_opponent_hand {
                reveal_opponent_hand_to_chooser(state, chooser);
            }
            let mut names: Vec<String> = state
                .db()
                .cards
                .iter()
                .filter(|c| {
                    !*nonland_only
                        || !c
                            .faces
                            .first()
                            .is_some_and(|f| f.characteristics.types.contains(CardTypeMask::LAND))
                })
                .map(|c| c.name.clone())
                .collect();
            names.sort();
            names.dedup();
            names.truncate(usize::from(u16::MAX));
            if names.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseCardName { candidates: names })
            }
        }
        EtbChoiceSpec::Counter { options } => {
            if options.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseCounter {
                    options: options.clone(),
                })
            }
        }
        EtbChoiceSpec::AnchorWord { words } => Some(EtbInteractiveStep::ChooseAnchorWord {
            words: words.clone(),
        }),
        EtbChoiceSpec::Player { opponent_only } => {
            let cands: Vec<PlayerId> = state
                .players()
                .iter()
                .filter(|p| !p.lost && (!*opponent_only || p.id != chooser))
                .map(|p| p.id)
                .collect();
            if cands.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChoosePlayer { candidates: cands })
            }
        }
        EtbChoiceSpec::TwoPlayers => {
            let cands: Vec<PlayerId> = state
                .players()
                .iter()
                .filter(|p| !p.lost)
                .map(|p| p.id)
                .collect();
            if cands.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseTwoPlayers {
                    first: None,
                    candidates: cands,
                })
            }
        }
        EtbChoiceSpec::Parity => Some(EtbInteractiveStep::ChooseParity),
        EtbChoiceSpec::CardType {
            exclude,
            options,
            look_at_opponent_hand,
        } => {
            if *look_at_opponent_hand {
                reveal_opponent_hand_to_chooser(state, chooser);
            }
            let all = [
                CardTypeMask::ARTIFACT,
                CardTypeMask::CREATURE,
                CardTypeMask::ENCHANTMENT,
                CardTypeMask::INSTANT,
                CardTypeMask::LAND,
                CardTypeMask::PLANESWALKER,
                CardTypeMask::SORCERY,
                CardTypeMask::BATTLE,
            ];
            let pool = if options.is_empty() {
                CardTypeMask::ARTIFACT
                    | CardTypeMask::CREATURE
                    | CardTypeMask::ENCHANTMENT
                    | CardTypeMask::INSTANT
                    | CardTypeMask::LAND
                    | CardTypeMask::PLANESWALKER
                    | CardTypeMask::SORCERY
                    | CardTypeMask::BATTLE
            } else {
                *options
            };
            let types: Vec<CardTypeMask> = all
                .into_iter()
                .filter(|t| pool.contains(*t) && !exclude.contains(*t))
                .collect();
            if types.is_empty() {
                None
            } else {
                Some(EtbInteractiveStep::ChooseCardType { types })
            }
        }
        EtbChoiceSpec::Number { min, max } => Some(EtbInteractiveStep::ChooseNumber {
            min: min.unwrap_or(0),
            max: max.unwrap_or(20),
        }),
        EtbChoiceSpec::ObjectOnBattlefield { filter, optional } => {
            let cands = {
                let cache = state.evaluate_board();
                let ctx = EvalContext::new(chooser).with_board(BoardView::Cached(cache));
                find_objects(state, filter, &ctx)
            };
            if cands.is_empty() && !*optional {
                None
            } else {
                Some(EtbInteractiveStep::ChooseObjectOnBattlefield {
                    candidates: cands,
                    optional: *optional,
                })
            }
        }
    }
}

fn apply_etb_interactive_answer(
    state: &mut GameState,
    event: &mut ReplaceableEventKind,
    step: &EtbInteractiveStep,
    answer: ChoiceAnswer,
    mods: &mut ReplacementAccumulatedMods,
) -> Result<(), EngineError> {
    let moving_id = event_moving_object(event);
    match step {
        EtbInteractiveStep::ChooseOpponentForControl { candidates } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&pid) = candidates.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "opponent index out of range".into(),
                ));
            };
            if let ReplaceableEventKind::ZoneChange { controller, .. } = event {
                *controller = pid;
            }
            if let Some(opts) = &mut mods.move_opts {
                opts.controller = Some(pid);
            }
            return Ok(());
        }
        EtbInteractiveStep::PayLifeOrEnterTapped { life } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::YesNo".into(),
                ));
            };
            if value {
                let chooser = match event {
                    ReplaceableEventKind::ZoneChange { controller, .. } => *controller,
                    _ => state.active_player(),
                };
                if !can_player_pay_life(state, chooser, *life) {
                    return Err(EngineError::IllegalAction(
                        "cannot pay life to enter untapped".into(),
                    ));
                }
                if let Some(p) = state.player_mut(chooser) {
                    p.life -= i32::from(*life);
                }
                state.mark_dirty(DirtyMask::LIFE);
                mods.enter_tapped = Some(false);
            } else {
                mods.enter_tapped = Some(true);
            }
        }
        EtbInteractiveStep::RevealFromHandOrEnterTapped { candidates } => match answer {
            ChoiceAnswer::PickObject { id } => {
                if !candidates.contains(&id) {
                    return Err(EngineError::IllegalAction(
                        "chosen card is not a valid reveal candidate".into(),
                    ));
                }
                crate::zones::reveal_to_all(state, id);
                mods.enter_tapped = Some(false);
            }
            ChoiceAnswer::DonePicking => {
                mods.enter_tapped = Some(true);
            }
            _ => {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickObject or DonePicking".into(),
                ));
            }
        },
        EtbInteractiveStep::ChooseSubType { options } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&st) = options.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "subtype index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_subtype = Some(st);
            }
        }
        EtbInteractiveStep::ChooseColor { colors } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&color) = colors.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "color index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_color = Some(color_mask_for(color));
                rare.chosen_colors = SmallVec::from_slice(&[color]);
            }
        }
        EtbInteractiveStep::ChooseTwoColors { first, colors } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&color) = colors.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "color index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                if let Some(c1) = first {
                    rare.chosen_colors = SmallVec::from_slice(&[*c1, color]);
                    rare.chosen_color = Some(color_mask_for(*c1) | color_mask_for(color));
                } else {
                    rare.chosen_colors = SmallVec::from_slice(&[color]);
                    rare.chosen_color = Some(color_mask_for(color));
                }
            }
        }
        EtbInteractiveStep::ChooseCardName { candidates } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(name) = candidates.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "card name index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_card_name = Some(name.clone());
            }
        }
        EtbInteractiveStep::ChooseCounter { options } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&cid) = options.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "counter index out of range".into(),
                ));
            };
            mods.extra_counters.push((cid, 1));
        }
        EtbInteractiveStep::ChooseAnchorWord { words } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(word) = words.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "anchor word index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_anchor_word = Some(word.clone());
            }
        }
        EtbInteractiveStep::ChoosePlayer { candidates } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&pid) = candidates.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "player index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_player = Some(pid);
            }
        }
        EtbInteractiveStep::ChooseTwoPlayers { first, candidates } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&pid) = candidates.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "player index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                if let Some(p1) = first {
                    rare.chosen_players = SmallVec::from_slice(&[*p1, pid]);
                    rare.chosen_player = Some(*p1);
                } else {
                    rare.chosen_players = SmallVec::from_slice(&[pid]);
                    rare.chosen_player = Some(pid);
                }
            }
        }
        EtbInteractiveStep::ChooseParity => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            if index > 1 {
                return Err(EngineError::IllegalAction(
                    "parity index must be 0 (Odd) or 1 (Even)".into(),
                ));
            }
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_odd_parity = Some(index == 0);
            }
        }
        EtbInteractiveStep::ChooseCardType { types } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickMode".into(),
                ));
            };
            let Some(&ct) = types.get(usize::from(index)) else {
                return Err(EngineError::IllegalAction(
                    "card type index out of range".into(),
                ));
            };
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_card_type = Some(ct);
            }
        }
        EtbInteractiveStep::ChooseNumber { min, max } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickNumber".into(),
                ));
            };
            if value < *min || value > *max {
                return Err(EngineError::IllegalAction(format!(
                    "chosen number {value} outside range {min}..={max}"
                )));
            }
            if let Some(id) = moving_id
                && let Some(rare) = state.rare_or_default(id)
            {
                rare.chosen_number = Some(value);
            }
        }
        EtbInteractiveStep::ChooseObjectOnBattlefield {
            candidates,
            optional,
        } => match answer {
            ChoiceAnswer::PickObject { id } => {
                if !candidates.contains(&id) {
                    return Err(EngineError::IllegalAction(
                        "chosen object is not a valid candidate".into(),
                    ));
                }
                if let Some(mov) = moving_id
                    && let Some(rare) = state.rare_or_default(mov)
                {
                    rare.chosen_object = Some(id);
                }
            }
            ChoiceAnswer::DonePicking if *optional || candidates.is_empty() => {}
            _ => {
                return Err(EngineError::IllegalAction(
                    "expected ChoiceAnswer::PickObject".into(),
                ));
            }
        },
        EtbInteractiveStep::DiscardForKeywordCounters { .. }
        | EtbInteractiveStep::RemoveCountersForPlusCounters { .. } => {}
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    Ok(())
}

fn finalize_etb_discard_for_keywords(
    state: &mut GameState,
    chooser: PlayerId,
    tracked_keywords: &[KeywordId],
    picked: &[ObjectId],
    mods: &mut ReplacementAccumulatedMods,
) -> Result<(), EngineError> {
    let mut granted: SmallVec<[KeywordId; 8]> = SmallVec::new();
    {
        let cache = state.evaluate_board();
        for &cid in picked {
            if let Some(eval) = cache.get(cid) {
                for &kw in &eval.keywords {
                    if !granted.contains(&kw) {
                        granted.push(kw);
                    }
                }
            }
            if let Some(obj) = state.object(cid)
                && let Some(opts) = face_opts_for_object(state, cid)
            {
                let ctx = EvalContext::for_source(obj.controller, cid)
                    .with_board(BoardView::Cached(cache));
                for ab in active_abilities_where(state.db(), obj.card_def, &opts, |cond| {
                    evaluate_condition(state, cond, &ctx)
                }) {
                    if let AbilityDef::Keyword(kw_ab) = ab.ability
                        && !granted.contains(&kw_ab.keyword.keyword)
                    {
                        granted.push(kw_ab.keyword.keyword);
                    }
                }
            }
        }
    }

    for &cid in picked {
        let raw = crate::zones::move_object_raw(
            state,
            cid,
            Zone::Graveyard,
            crate::zones::MoveRawOptions::default(),
            None,
        )?;
        crate::events::emit_zone_change(
            state,
            raw.old_id,
            raw.new_id,
            raw.from_zone,
            raw.to_zone,
            Some(chooser),
            Some(chooser),
            raw.lki.as_ref(),
            raw.ceased_to_exist,
        );
        crate::events::emit_event(
            state,
            crate::events::AtomicEventKind::Discard {
                player: chooser,
                object: raw.new_id.unwrap_or(cid),
                dest_zone: raw.to_zone,
                is_cycle: false,
                cause_controller: Some(chooser),
            },
        );
    }

    for &kw_id in tracked_keywords {
        if granted.contains(&kw_id)
            && let Some(kw_name) = state.db().keyword_name(kw_id)
        {
            let ctr_id = state.db().counter_id(kw_name).or_else(|| {
                state
                    .db()
                    .tables
                    .counters
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(kw_name))
                    .map(|i| CounterId(i as u16))
            });
            if let Some(cid) = ctr_id {
                mods.extra_counters.push((cid, 1));
            }
        }
    }
    Ok(())
}

fn finalize_etb_remove_counters_for_plus_counters(
    state: &mut GameState,
    chooser: PlayerId,
    multiplier: u16,
    picked: &[ObjectId],
    mods: &mut ReplacementAccumulatedMods,
) {
    let mut total_removed = 0u32;
    for &target_id in picked {
        let snapshot: SmallVec<[(CounterId, u32); 4]> =
            state.counters(target_id).iter().copied().collect();
        for (cid, prev) in snapshot {
            if prev == 0 {
                continue;
            }
            let rem = state.remove_counter(target_id, cid, prev);
            total_removed = total_removed.saturating_add(rem);
            if rem > 0 {
                crate::events::emit_event(
                    state,
                    crate::events::AtomicEventKind::CounterChanged {
                        object: target_id,
                        counter: cid,
                        previous_count: prev,
                        new_count: prev - rem,
                        actor: Some(chooser),
                    },
                );
            }
        }
    }
    let added = total_removed.saturating_mul(u32::from(multiplier));
    if added > 0
        && let Some(plus_cid) = state.db().counter_id("+1/+1")
    {
        mods.extra_counters.push((plus_cid, added));
    }
}

fn finalize_draw_rule_clamp(
    state: &GameState,
    event: &mut ReplaceableEventKind,
    mods: &ReplacementAccumulatedMods,
) {
    if !mods.skipped
        && mods.abundance_land.is_none()
        && mods.draw_stealer.is_none()
        && mods.reanimate_on_empty_draw.is_none()
        && let ReplaceableEventKind::Draw { player, count, .. } = event
        && *count > 1
    {
        let drawn = state.player(*player).map_or(0, |p| p.cards_drawn_this_turn);
        if drawn == 0
            && state
                .evaluate_board()
                .player(*player)
                .is_some_and(|p| p.cant_draw_extra_cards)
        {
            *count = 1;
        }
    }
}

fn token_spec_matches_replacement(
    spec: &TokenSpec,
    one_of_each: bool,
    match_subtypes: &[SubtypeId],
) -> bool {
    if !match_subtypes.is_empty() {
        match_subtypes.iter().any(|st| spec.subtypes.contains(st))
    } else if !one_of_each {
        spec.types.contains(CardTypeMask::CREATURE)
    } else {
        false
    }
}

fn matches_token_creation_filter(
    state: &GameState,
    tok_ctrl: PlayerId,
    spec: Option<&TokenSpec>,
    mods: &ReplacementAccumulatedMods,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let tok_owner = mods
        .token_opts
        .as_ref()
        .and_then(|o| o.owner)
        .unwrap_or(tok_ctrl);
    let tapped_opt = mods.token_opts.as_ref().is_some_and(|o| o.tapped);
    if !mods.replace_tokens.is_empty() {
        return mods.replace_tokens.iter().any(|(rep_spec, _)| {
            matches_token_spec_filter(
                state, tok_ctrl, tok_owner, tapped_opt, rep_spec, filter, ctx,
            )
        });
    }
    if let Some(s) = spec {
        return matches_token_spec_filter(state, tok_ctrl, tok_owner, tapped_opt, s, filter, ctx);
    }
    if filter.flags.contains(FilterFlags::NONTOKEN) {
        return false;
    }
    if let Some(rel) = filter.controller
        && !matches_relative_control(tok_ctrl, rel, ctx)
    {
        return false;
    }
    if let Some(rel) = filter.owner
        && !matches_relative_control(tok_owner, rel, ctx)
    {
        return false;
    }
    filter.types_any.is_empty()
        && filter.types_all.is_empty()
        && filter.supertypes_any.is_empty()
        && filter.colors_any.is_empty()
        && filter.colors_all.is_empty()
        && filter.subtypes_any.is_empty()
        && filter.subtypes_all.is_empty()
        && filter.power.is_none()
        && filter.toughness.is_none()
}

fn matches_token_spec_filter(
    state: &GameState,
    tok_ctrl: PlayerId,
    tok_owner: PlayerId,
    tapped_opt: bool,
    spec: &TokenSpec,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let flags = filter.flags;
    if flags.contains(FilterFlags::NONTOKEN) {
        return false;
    }
    if let Some(rel) = filter.controller
        && !matches_relative_control(tok_ctrl, rel, ctx)
    {
        return false;
    }
    if let Some(rel) = filter.owner
        && !matches_relative_control(tok_owner, rel, ctx)
    {
        return false;
    }
    let is_tapped = spec.tapped || tapped_opt;
    if flags.contains(FilterFlags::TAPPED) && !is_tapped {
        return false;
    }
    if flags.contains(FilterFlags::UNTAPPED) && is_tapped {
        return false;
    }

    let (name, ev) = evaluate_token_spec_base(spec, None);
    if !filter.types_any.is_empty() && !ev.types.intersects(filter.types_any) {
        return false;
    }
    if !filter.types_all.is_empty() && !ev.types.contains(filter.types_all) {
        return false;
    }
    if !filter.types_none.is_empty() && ev.types.intersects(filter.types_none) {
        return false;
    }
    if !filter.supertypes_any.is_empty() && !ev.supertypes.intersects(filter.supertypes_any) {
        return false;
    }
    if !filter.supertypes_none.is_empty() && ev.supertypes.intersects(filter.supertypes_none) {
        return false;
    }
    if !filter.colors_any.is_empty() && !ev.colors.intersects(filter.colors_any) {
        return false;
    }
    if !filter.colors_all.is_empty() && !ev.colors.contains(filter.colors_all) {
        return false;
    }
    if !filter.colors_none.is_empty() && ev.colors.intersects(filter.colors_none) {
        return false;
    }

    let color_count = ev.colors.bits().count_ones();
    if flags.contains(FilterFlags::IS_COLORED) && color_count == 0 {
        return false;
    }
    if flags.contains(FilterFlags::IS_COLORLESS) && color_count > 0 {
        return false;
    }
    if flags.contains(FilterFlags::MULTICOLORED) && color_count < 2 {
        return false;
    }
    if flags.contains(FilterFlags::NOT_MULTICOLORED) && color_count >= 2 {
        return false;
    }
    if flags.contains(FilterFlags::MONOCOLORED) && color_count != 1 {
        return false;
    }
    if flags.contains(FilterFlags::NOT_MONOCOLORED) && color_count == 1 {
        return false;
    }

    let db = state.db();
    let has_changeling = db
        .keyword_id("Changeling")
        .is_some_and(|kw| ev.keywords.contains(&kw));
    let has_subtype = |sid: SubtypeId| -> bool {
        ev.subtypes.contains(&sid)
            || (has_changeling
                && db
                    .subtype_name(sid)
                    .is_some_and(|n| !is_non_creature_subtype(n)))
    };
    if !filter.subtypes_any.is_empty() && !filter.subtypes_any.iter().copied().any(has_subtype) {
        return false;
    }
    if !filter.subtypes_all.is_empty() && !filter.subtypes_all.iter().copied().all(has_subtype) {
        return false;
    }
    if !filter.subtypes_none.is_empty() && filter.subtypes_none.iter().copied().any(has_subtype) {
        return false;
    }

    if flags.contains(FilterFlags::HISTORIC)
        && !ev.supertypes.contains(SupertypeMask::LEGENDARY)
        && !ev.types.contains(CardTypeMask::ARTIFACT)
        && !db
            .subtype_id("Saga")
            .is_some_and(|s| ev.subtypes.contains(&s))
    {
        return false;
    }

    if filter.power.is_some() || filter.toughness.is_some() {
        let Some(pt) = ev.pt else {
            return false;
        };
        if let Some(cmp) = &filter.power {
            let rhs = evaluate_amount(state, &cmp.value, ctx);
            if !compare_i32(pt.power, cmp.op, rhs) {
                return false;
            }
        }
        if let Some(cmp) = &filter.toughness {
            let rhs = evaluate_amount(state, &cmp.value, ctx);
            if !compare_i32(pt.toughness, cmp.op, rhs) {
                return false;
            }
        }
    }

    for pred in &filter.extra {
        match pred {
            ObjectPredicate::AnyOf { filters } => {
                if !filters.iter().any(|sub| {
                    matches_token_spec_filter(
                        state, tok_ctrl, tok_owner, tapped_opt, spec, sub, ctx,
                    )
                }) {
                    return false;
                }
            }
            ObjectPredicate::Named { name: req } => {
                if !name.eq_ignore_ascii_case(req) {
                    return false;
                }
            }
            ObjectPredicate::HasKeywords { keywords } => {
                if !keywords.iter().all(|kw| ev.keywords.contains(kw)) {
                    return false;
                }
            }
            ObjectPredicate::LacksKeywords { keywords }
                if keywords.iter().any(|kw| ev.keywords.contains(kw)) =>
            {
                return false;
            }
            _ => {}
        }
    }

    true
}

fn apply_etb_copy_to_object(
    state: &mut GameState,
    entering_id: ObjectId,
    controller: PlayerId,
    copy_target: ObjectId,
    tapped_if_copied: bool,
    modifications: Option<&CopyModificationSpec>,
    mods: &mut ReplacementAccumulatedMods,
) {
    let Some(target_obj) = state.object(copy_target).copied() else {
        return;
    };
    let target_rare = state.rare(copy_target).cloned();
    let copiable =
        if let Some(existing_arc) = target_rare.as_ref().and_then(|r| r.copiable.as_ref()) {
            let mut cloned = (**existing_arc).clone();
            if let Some(next_mods) = modifications {
                cloned.modifications = Some(next_mods.clone());
            }
            cloned
        } else {
            CopiableState {
                card_def: target_obj.card_def,
                face_index: Some(target_obj.active_face_index()),
                fused: target_rare.as_ref().is_some_and(|r| r.fused),
                is_single_face_copy: target_obj.flags.contains(ObjectFlags::TRANSFORMED)
                    || target_rare.as_ref().is_some_and(|r| r.is_single_face_copy),
                prototyped: target_obj.flags.contains(ObjectFlags::PROTOTYPED),
                merged_components: target_rare
                    .as_ref()
                    .map(|r| r.merged_components.iter().map(|m| m.card_def).collect())
                    .unwrap_or_default(),
                except: None,
                modifications: modifications.cloned(),
            }
        };
    let tok_spec = (copiable.card_def == TOKEN_CARD_DEF_ID)
        .then(|| target_rare.and_then(|r| r.token_spec))
        .flatten();

    if let Some(rare) = state.rare_or_default(entering_id) {
        rare.copiable = Some(Arc::new(copiable));
        if tok_spec.is_some() {
            rare.token_spec = tok_spec;
        }
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    if tapped_if_copied {
        mods.enter_tapped = Some(true);
    }
    if let Some(cmod) = modifications {
        let ctrl = controller;
        let cache = state.evaluate_board();
        let ctx = etb_eval_context(state, ctrl, entering_id, cache);
        if let Some((cid, amt)) = &cmod.additional_counters {
            let n = evaluate_amount(state, amt, &ctx).max(0) as u32;
            if n > 0 {
                mods.extra_counters.push((*cid, n));
            }
        }
        if let Some(cc) = &cmod.conditional_counters
            && cc.count > 0
            && evaluate_condition(state, &cc.condition, &ctx)
        {
            mods.extra_counters.push((cc.counter, u32::from(cc.count)));
        }
        if !cmod.conditional_type_counters.is_empty() {
            let target_types = cache
                .get(copy_target)
                .map(|e| e.types)
                .unwrap_or_else(|| base_types_quick(state, copy_target));
            for ctc in &cmod.conditional_type_counters {
                if ctc.count > 0 && target_types.intersects(ctc.card_type) {
                    mods.extra_counters
                        .push((ctc.counter, u32::from(ctc.count)));
                }
            }
        }
    }
}

// ---- Small Helpers -----------------------------------------------------------------------------

fn can_player_pay_life(state: &GameState, player: PlayerId, life: u16) -> bool {
    let cache = state.evaluate_board();
    if cache.player(player).is_some_and(|p| p.life_cant_change) {
        return false;
    }
    state
        .player(player)
        .is_some_and(|p| p.life >= i32::from(life))
}

fn is_first_draw_in_own_draw_step(state: &GameState, player: PlayerId) -> bool {
    state.active_player() == player
        && state.phase() == GamePhase::Draw
        && state
            .player(player)
            .is_some_and(|p| p.draw_step_draws_this_turn == 0)
}

fn event_source(event: &ReplaceableEventKind) -> Option<ObjectId> {
    match event {
        ReplaceableEventKind::Draw { source, .. }
        | ReplaceableEventKind::Damage { source, .. }
        | ReplaceableEventKind::LifeGain { source, .. }
        | ReplaceableEventKind::PutCounters { source, .. }
        | ReplaceableEventKind::PutPlayerCounters { source, .. }
        | ReplaceableEventKind::CreateToken { source, .. }
        | ReplaceableEventKind::Proliferate { source, .. } => *source,
        ReplaceableEventKind::ZoneChange { .. } | ReplaceableEventKind::Untap { .. } => None,
    }
}

fn event_moving_object(event: &ReplaceableEventKind) -> Option<ObjectId> {
    match event {
        ReplaceableEventKind::ZoneChange { object, .. } => Some(*object),
        _ => None,
    }
}

fn event_object(event: &ReplaceableEventKind) -> Option<ObjectId> {
    match event {
        ReplaceableEventKind::ZoneChange { object, .. }
        | ReplaceableEventKind::PutCounters { object, .. }
        | ReplaceableEventKind::Untap { object } => Some(*object),
        ReplaceableEventKind::Damage { source, target, .. } => match target {
            TargetRef::Object { id } => Some(*id),
            _ => *source,
        },
        _ => event_source(event),
    }
}

fn event_player(state: &GameState, event: &ReplaceableEventKind) -> PlayerId {
    match event {
        ReplaceableEventKind::ZoneChange { controller, .. }
        | ReplaceableEventKind::CreateToken { controller, .. } => *controller,
        ReplaceableEventKind::Draw { player, .. }
        | ReplaceableEventKind::LifeGain { player, .. }
        | ReplaceableEventKind::PutPlayerCounters { player, .. }
        | ReplaceableEventKind::Proliferate { player, .. } => *player,
        ReplaceableEventKind::Damage { target, .. } => match target {
            TargetRef::Player { id } => *id,
            TargetRef::Object { id } => state
                .object(*id)
                .map_or(state.active_player(), |o| o.controller),
            TargetRef::Stack { .. } => state.active_player(),
        },
        ReplaceableEventKind::PutCounters { object, .. }
        | ReplaceableEventKind::Untap { object } => state
            .object(*object)
            .map_or(state.active_player(), |o| o.controller),
    }
}

fn event_amount(event: &ReplaceableEventKind) -> Option<u32> {
    match event {
        ReplaceableEventKind::Draw { count, .. }
        | ReplaceableEventKind::PutCounters { count, .. }
        | ReplaceableEventKind::PutPlayerCounters { count, .. }
        | ReplaceableEventKind::CreateToken { count, .. }
        | ReplaceableEventKind::Proliferate { count, .. } => Some(*count),
        ReplaceableEventKind::Damage { amount, .. }
        | ReplaceableEventKind::LifeGain { amount, .. } => Some(*amount),
        ReplaceableEventKind::ZoneChange { .. } | ReplaceableEventKind::Untap { .. } => None,
    }
}

fn matches_affected(
    state: &GameState,
    id: ObjectId,
    aff: &Affected,
    ctx: &EvalContext<'_>,
) -> bool {
    match aff {
        Affected::SelfObj => ctx.source == Some(id),
        Affected::AttachedTo => ctx
            .source
            .and_then(|s| state.object(s))
            .is_some_and(|s| s.attached_to == Some(id)),
        Affected::Filter { filter } => matches_object_filter(state, id, filter, ctx),
    }
}

fn base_types_quick(state: &GameState, id: ObjectId) -> CardTypeMask {
    let Some(obj) = state.object(id) else {
        return CardTypeMask::empty();
    };
    if let Some(spec) = state.rare(id).and_then(|r| r.token_spec.as_deref()) {
        return evaluate_token_spec_base(spec, None).1.types;
    }
    let Some(opts) = face_opts_for_object(state, id) else {
        return CardTypeMask::empty();
    };
    crate::faces::resolve_object_faces(state.db(), obj.card_def, &opts)
        .map_or(CardTypeMask::empty(), |v| v.types)
}

fn scale_count(count: u32, factor: f32, delta: i16, round: Option<RoundMode>) -> u32 {
    let raw = ((count as f32) + f32::from(delta)) * factor;
    let rounded = match round {
        Some(RoundMode::Up) => raw.ceil(),
        Some(RoundMode::Down) | None => raw.floor(),
    };
    rounded.max(0.0) as u32
}

fn color_mask_for(c: ManaColor) -> ColorMask {
    match c {
        ManaColor::W => ColorMask::W,
        ManaColor::U => ColorMask::U,
        ManaColor::B => ColorMask::B,
        ManaColor::R => ColorMask::R,
        ManaColor::G => ColorMask::G,
        ManaColor::C => ColorMask::empty(),
    }
}

fn collect_subtype_options_for_scope(
    state: &GameState,
    scope: SubTypeChoiceScope,
) -> Vec<SubtypeId> {
    let mut out = Vec::new();
    for (idx, name) in state.db().tables.subtypes.iter().enumerate() {
        let is_non_cre = crate::faces::is_non_creature_subtype(name);
        let include = match scope {
            SubTypeChoiceScope::Creature => !is_non_cre,
            SubTypeChoiceScope::BasicLand => {
                matches!(
                    name.as_str(),
                    "Plains" | "Island" | "Swamp" | "Mountain" | "Forest"
                )
            }
            SubTypeChoiceScope::Land => {
                matches!(
                    name.as_str(),
                    "Plains"
                        | "Island"
                        | "Swamp"
                        | "Mountain"
                        | "Forest"
                        | "Gate"
                        | "Locus"
                        | "Cave"
                        | "Desert"
                        | "Lair"
                        | "Sphere"
                        | "Tower"
                        | "Mine"
                        | "Power-Plant"
                        | "Town"
                        | "Urza's"
                        | "Plan"
                        | "Planet"
                )
            }
        };
        if include {
            out.push(SubtypeId(idx as u16));
        }
    }
    out
}

fn academy_manufactor_tokens(db: &mtg_ir::CardDatabase) -> Vec<TokenSpec> {
    let mut specs = Vec::with_capacity(3);
    for name in ["Clue", "Food", "Treasure"] {
        let mut subtypes = SmallVec::new();
        if let Some(sid) = db.subtype_id(name) {
            subtypes.push(sid);
        }
        specs.push(TokenSpec {
            name: name.into(),
            power: 0,
            toughness: 0,
            colors: ColorMask::empty(),
            types: CardTypeMask::ARTIFACT,
            supertypes: mtg_ir::SupertypeMask::empty(),
            subtypes,
            type_line: format!("Artifact — {name}"),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: Vec::new(),
            legendary: false,
        });
    }
    specs
}

fn strip_filter_zones_for_bf_lookahead(filter: &mut mtg_ir::ObjectFilter, from: Zone) -> bool {
    if !filter.zones.is_empty() && !filter.zones.contains(ZoneMask::from_zone(from)) {
        return false;
    }
    filter.zones = ZoneMask::empty();
    for pred in &mut filter.extra {
        if let mtg_ir::ObjectPredicate::AnyOf { filters } = pred {
            filters.retain_mut(|sub| strip_filter_zones_for_bf_lookahead(sub, from));
            if filters.is_empty() {
                return false;
            }
        }
    }
    true
}

/// Checks active battlefield statics (`LandsCantEnterBattlefield`, `CantEnterFromZones`) for
/// `ZoneChange { object, from, to: Zone::Battlefield, controller }`.
pub(crate) fn is_battlefield_entry_blocked_by_static(
    state: &mut GameState,
    object: ObjectId,
    from: Zone,
    controller: PlayerId,
    mods: &ReplacementAccumulatedMods,
) -> bool {
    with_eval_on_bf_context(state, object, from, controller, mods, |state| {
        let cache = state.evaluate_board();
        let obj_types = cache
            .get(object)
            .map(|e| e.types)
            .unwrap_or_else(|| base_types_quick(state, object));

        for p in state.players() {
            if p.lost {
                continue;
            }
            for &src_id in p.battlefield.iter().chain(p.command.iter()) {
                let Some(src_obj) = state.object(src_id) else {
                    continue;
                };
                if src_obj.zone == Zone::Battlefield && src_obj.is_phased_out() {
                    continue;
                }
                if src_obj.zone == Zone::Command && !src_obj.flags.contains(ObjectFlags::IS_EMBLEM)
                {
                    continue;
                }
                let eval = cache.get(src_id);
                if eval.is_some_and(|e| e.lost_all_abilities) {
                    continue;
                }
                let src_ctrl = eval.map_or(src_obj.controller, |e| e.controller);
                let ctx =
                    EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
                for ab in active_abilities_for_perm_or_emblem(state, src_id, src_obj, &ctx) {
                    if let AbilityDef::Static(st) = ab.ability {
                        if let Some(cond) = &st.static_def.condition
                            && !evaluate_condition(state, cond, &ctx)
                        {
                            continue;
                        }
                        match &st.static_def.body {
                            StaticBody::LandsCantEnterBattlefield
                                if obj_types.contains(CardTypeMask::LAND) =>
                            {
                                return true;
                            }
                            StaticBody::CantEnterFromZones { filter, zones, who }
                                if zones.contains(ZoneMask::from_zone(from))
                                    && who.is_none_or(|rel| {
                                        matches_relative_control(controller, rel, &ctx)
                                    }) =>
                            {
                                let mut bf_filter = (**filter).clone();
                                if strip_filter_zones_for_bf_lookahead(&mut bf_filter, from)
                                    && matches_object_filter(state, object, &bf_filter, &ctx)
                                {
                                    return true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        false
    })
}

/// Returns `true` when no active replacement can copy, change controller, skip/execute, or
/// redirect `object` away from `Zone::Battlefield`, making pre-replacement entry blocking safe.
pub(crate) fn can_pre_block_battlefield_entry(
    state: &mut GameState,
    object: ObjectId,
    from: Zone,
    controller: PlayerId,
    mods: &ReplacementAccumulatedMods,
) -> bool {
    let event = ReplaceableEventKind::ZoneChange {
        object,
        from,
        to: Zone::Battlefield,
        controller,
    };
    let candidates = collect_applicable_candidates(state, &event, mods, &[]);
    for cand in candidates {
        if let CandidateKind::ReplacementDef { def, .. } = cand.kind {
            match &def.with {
                ReplacementWith::Skip | ReplacementWith::Execute { .. } => return false,
                ReplacementWith::ZoneChange { mod_spec }
                    if mod_spec.copy.is_some()
                        || mod_spec.enter_under_opponent_control
                        || mod_spec.to.is_some_and(|dest| dest != Zone::Battlefield) =>
                {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

fn is_counter_placement_blocked(state: &GameState, event: &ReplaceableEventKind) -> bool {
    match event {
        ReplaceableEventKind::PutPlayerCounters {
            player, counter, ..
        } => state.evaluate_board().player(*player).is_some_and(|p| {
            p.cant_get_any_counters || p.active_rule_mods.iter().any(|m| {
                matches!(
                    m.rule,
                    mtg_ir::RuleMod::PlayerCantGetCounters { counter: Some(cid) } if cid == *counter
                )
            })
        }),
        ReplaceableEventKind::PutCounters {
            object, counter, ..
        } => {
            let cache = state.evaluate_board();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &src_id in p.battlefield.iter().chain(p.command.iter()) {
                    let Some(src_obj) = state.object(src_id) else {
                        continue;
                    };
                    if src_obj.zone == Zone::Battlefield && src_obj.is_phased_out() {
                        continue;
                    }
                    if src_obj.zone == Zone::Command
                        && !src_obj.flags.contains(ObjectFlags::IS_EMBLEM)
                    {
                        continue;
                    }
                    let eval = cache.get(src_id);
                    if eval.is_some_and(|e| e.lost_all_abilities) {
                        continue;
                    }
                    let src_ctrl = eval.map_or(src_obj.controller, |e| e.controller);
                    let ctx = EvalContext::for_source(src_ctrl, src_id)
                        .with_board(BoardView::Cached(cache));
                    for ab in active_abilities_for_perm_or_emblem(state, src_id, src_obj, &ctx) {
                        if let AbilityDef::Static(st) = ab.ability {
                            if let Some(cond) = &st.static_def.condition
                                && !evaluate_condition(state, cond, &ctx)
                            {
                                continue;
                            }
                            if let StaticBody::CantHaveCounterType {
                                affected,
                                counter: req_c,
                            } = &st.static_def.body
                                && req_c.is_none_or(|c| c == *counter)
                                && matches_affected(state, *object, affected, &ctx)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::{GameConfig, RngMode};
    use crate::zones::{MoveRawOptions, move_object, move_object_raw};
    use mtg_ir::{
        Amount, CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, DrawScaleSpec,
        EventDetail, EventFilter, EventKind, EventPattern, IR_V2_VERSION, InternTables, IrV2Bundle,
        Layer, LayerMod, LifeGainScaleSpec, ManaCost, ObjectFilter, PlayerFilter, PlayerRelation,
        PrintedPt, RelativeControl, ReplacementAbility, StaticAbility, StaticDef, SupertypeMask,
        ZoneChangeMod, ZoneMask,
    };

    fn make_card(name: &str, types: CardTypeMask, abilities: Vec<AbilityDef>) -> CardDef {
        make_card_with_subtypes(name, types, &[], SupertypeMask::empty(), abilities)
    }

    fn make_card_with_subtypes(
        name: &str,
        types: CardTypeMask,
        subtypes: &[SubtypeId],
        supertypes: SupertypeMask,
        abilities: Vec<AbilityDef>,
    ) -> CardDef {
        let pt = if types.contains(CardTypeMask::CREATURE) {
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            })
        } else {
            None
        };
        CardDef {
            oracle_id: name.to_lowercase().replace(' ', "_"),
            compiler_version: 46,
            text_hash: name.to_string(),
            name: name.to_string(),
            layout: CardLayout::Normal,
            faces: vec![mtg_ir::FaceDef {
                name: name.to_string(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types,
                    subtypes: subtypes.iter().copied().collect(),
                    supertypes,
                    colors: ColorMask::empty(),
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

    fn build_db(cards: Vec<CardDef>) -> Arc<CardDatabase> {
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Mountain".into(), "Human".into(), "Goblin".into()],
                    counters: vec!["+1/+1".into(), "-1/-1".into(), "lore".into()],
                    keywords: vec!["Absorb".into()],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn boot_2p(db: Arc<CardDatabase>) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.rng_mode = RngMode::Mulberry32Compat;
        GameState::new(db, cfg, &[deck.clone(), deck], 42).unwrap()
    }

    fn put_on_battlefield(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let obj =
            crate::state::GameObject::new(def, player, Zone::Library, ts, ObjectFlags::empty());
        let id = state.insert_object(obj, None);
        state.player_mut(player).unwrap().library.push(id);
        move_object_raw(
            state,
            id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap()
    }

    fn put_in_hand(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let obj =
            crate::state::GameObject::new(def, player, Zone::Library, ts, ObjectFlags::empty());
        let id = state.insert_object(obj, None);
        state.player_mut(player).unwrap().library.push(id);
        move_object_raw(state, id, Zone::Hand, MoveRawOptions::default(), None)
            .unwrap()
            .new_id
            .unwrap()
    }

    fn rep_ability(replacement: ReplacementDef) -> AbilityDef {
        AbilityDef::Replacement(ReplacementAbility {
            replacement,
            span: Default::default(),
        })
    }

    fn zc_to(to: Zone, or_self: bool) -> EventPattern {
        EventPattern {
            event: EventKind::ZoneChange,
            filter: EventFilter {
                or_self,
                detail: Some(EventDetail::ZoneChange {
                    from: ZoneMask::empty(),
                    to: Some(to),
                    from_anywhere: false,
                    not_from_zone: None,
                    not_to_zone: None,
                    enters_untapped: false,
                    dealt_damage_by_self_this_turn: false,
                }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn cr_614_5_applied_once_dedup_on_zone_change_and_life_gain() {
        let rip = make_card(
            "GyExiler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Graveyard, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Exile),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let doubler = make_card(
            "LifeDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::GainLife,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: true,
                        add: None,
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            rip,
            doubler,
        ]);
        let mut state = boot_2p(db);
        let bear = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

        let moved = move_object(
            &mut state,
            bear,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let MoveObjectOutcome::Moved(raw) = moved else {
            panic!("expected immediate redirect to Exile");
        };
        let exiled = raw.new_id.unwrap();
        assert_eq!(state.object(exiled).unwrap().zone, Zone::Exile);

        // Two LifeDoublers -> gain 1 becomes 1 -> 2 -> 4 (applied once each).
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        let mut res = resolve_life_gain(&mut state, PlayerId(0), 1, None).unwrap();
        while let ReplacementResult::Paused { prompt_id, chooser } = res {
            res = resume_pending_replacement(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap();
        }
        let ReplacementResult::Resolved(out) = res else {
            unreachable!();
        };
        let ReplaceableEventKind::LifeGain { amount, .. } = out.event else {
            panic!("expected LifeGain event");
        };
        assert_eq!(amount, 4);
    }

    #[test]
    fn cr_614_5_pause_resume_across_snapshot_and_out_of_range_pick() {
        let doubler = make_card(
            "LifeDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::GainLife,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: true,
                        add: None,
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        let adder = make_card(
            "LifeAdder",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::GainLife,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: false,
                        add: Some(Box::new(Amount::Const { n: 3 })),
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            doubler,
            adder,
        ]);
        let mut state = boot_2p(db.clone());
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));

        let res = resolve_life_gain(&mut state, PlayerId(0), 2, None).unwrap();
        let ReplacementResult::Paused { prompt_id, chooser } = res else {
            panic!("expected ChooseReplacement pause");
        };
        assert_eq!(chooser, PlayerId(0));

        // Roundtrip GameStateSnapshot via JSON cleanly.
        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let snap_from_json: crate::state::GameStateSnapshot = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&snap_from_json).unwrap();
        assert_eq!(json, json2);
        let mut restored = GameState::from_snapshot(db, snap_from_json);
        assert!(restored.pending_replacement().is_some());

        // Out-of-range index returns Err(IllegalAction) and MUST NOT clear pending_replacement (t2_f09_03).
        let err = resume_pending_replacement(
            &mut restored,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 99 },
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::IllegalAction(_)));
        assert!(restored.pending_replacement().is_some());

        // Pick index 1 (LifeAdder first: 2 -> 5, then LifeDoubler: 5 -> 10).
        let res2 = resume_pending_replacement(
            &mut restored,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 1 },
        )
        .unwrap();
        let ReplacementResult::Resolved(out) = res2 else {
            panic!("expected final resolved result after single remaining replacement");
        };
        let ReplaceableEventKind::LifeGain { amount, .. } = out.event else {
            panic!("expected LifeGain");
        };
        assert_eq!(amount, 10);
        assert!(restored.pending_replacement().is_none());
    }

    #[test]
    fn cr_616_1_tiers_self_stack_draw_and_steal_before_clone_control() {
        // Card 1: Stack card with CR 614.15 self-replacement Draw (+1 draw).
        let stack_draw_rep = make_card(
            "SelfDrawStackCard",
            CardTypeMask::SORCERY,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: None,
                        delta: Some(1),
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: true,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        // Card 2: Doubler enchantment (Tier 5).
        let draw_doubler = make_card(
            "DrawDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: Some(2),
                        delta: None,
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        // Card 3: Opponent steal ETB (Tier 2).
        let steal_etb = make_card(
            "SynthOppEnter",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enter_under_opponent_control: true,
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                condition: None,
            })],
        );
        // Card 4: Clone (Tier 3 even though self_replacement=true!).
        let clone_card = make_card(
            "SynthClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: true,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        // Card 5: All creatures enter tapped (Tier 5).
        let all_tapped = make_card(
            "SynthAllTapped",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            stack_draw_rep,
            draw_doubler,
            steal_etb,
            clone_card,
            all_tapped,
        ]);

        // Part A: CR 614.15 stack self-replacement (+1 before *2 => (2 + 1) * 2 = 6).
        let mut state = boot_2p(db.clone());
        let sorc_in_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let sorc_on_stack = move_object_raw(
            &mut state,
            sorc_in_hand,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        let ReplacementResult::Resolved(draw_out) =
            resolve_draw(&mut state, PlayerId(0), 2, Some(sorc_on_stack)).unwrap()
        else {
            panic!("expected immediate resolved draw without ChooseReplacement");
        };
        let ReplaceableEventKind::Draw { count, .. } = draw_out.event else {
            panic!("expected Draw");
        };
        assert_eq!(count, 6);

        // Part B: Steal ETB (Tier 2) -> Clone (Tier 3, chooser = Player 1!) -> AllTapped (Tier 5).
        let mut state2 = boot_2p(db);
        let bear = put_on_battlefield(&mut state2, PlayerId(1), CardDefId(0));
        put_on_battlefield(&mut state2, PlayerId(1), CardDefId(3)); // Player 1 has Steal ETB
        put_on_battlefield(&mut state2, PlayerId(1), CardDefId(5)); // AllTapped
        let clone_hand = put_in_hand(&mut state2, PlayerId(0), CardDefId(4));

        let res = move_object(
            &mut state2,
            clone_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let MoveObjectOutcome::Paused { prompt_id, chooser } = res else {
            panic!("expected Clone ETB prompt after Steal");
        };
        // Tier 2 changed enters_controller to Player 1, so Tier 3 Clone prompts Player 1!
        assert_eq!(chooser, PlayerId(1));
        assert!(!matches!(
            state2.pending_replacement().as_ref().unwrap().prompt_kind,
            PendingReplacementPrompt::ChooseReplacement
        ));

        let finalized = resume_and_finalize_replacement(
            &mut state2,
            chooser,
            prompt_id,
            ChoiceAnswer::PickObject { id: bear },
        )
        .unwrap();
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) = finalized else {
            panic!("expected ZoneMove after Clone pick");
        };
        let cloned_id = raw.new_id.unwrap();
        let cloned_obj = state2.object(cloned_id).unwrap();
        assert_eq!(cloned_obj.controller, PlayerId(1));
        assert!(cloned_obj.flags.contains(ObjectFlags::TAPPED));
    }

    #[test]
    fn cr_614_12_hypothetical_battlefield_lookahead() {
        // Card 1: Blood Moon style — nonbasic lands are Mountains & lose all abilities.
        let nonbasic_land_filter = ObjectFilter {
            types_any: CardTypeMask::LAND,
            supertypes_none: SupertypeMask::BASIC,
            ..Default::default()
        };
        let moon = make_card(
            "MoonStatic",
            CardTypeMask::ENCHANTMENT,
            vec![
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer4,
                            affected: Affected::Filter {
                                filter: Box::new(nonbasic_land_filter.clone()),
                            },
                            modifier: LayerMod::SetSubtypes {
                                subtypes: smallvec::smallvec![SubtypeId(0)], // Mountain
                            },
                            active_in_zone: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                }),
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(nonbasic_land_filter),
                            },
                            modifier: LayerMod::RemoveAllAbilities {
                                keep_mana_abilities: false,
                            },
                            active_in_zone: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                }),
            ],
        );
        // Card 2: Shockland — self-replacement pay 2 life or enter tapped.
        let shockland = make_card(
            "SynthShock",
            CardTypeMask::LAND,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        pay_life_or_enter_tapped: Some(2),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        // Card 3: Grumgully-style static: non-Human creatures you control enter with a +1/+1 counter.
        let grum = make_card(
            "NonHumanMentor",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                subtypes_none: smallvec::smallvec![SubtypeId(1)], // Human
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        // Card 4: Human creature + Layer 4 turning all creatures into Goblin (not Human anymore).
        let human_creature = make_card_with_subtypes(
            "HumanBear",
            CardTypeMask::CREATURE,
            &[SubtypeId(1)], // Human
            SupertypeMask::empty(),
            vec![],
        );
        let goblin_mask = make_card(
            "MaskGoblins",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetSubtypes {
                            subtypes: smallvec::smallvec![SubtypeId(2)], // Goblin
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            moon,
            shockland,
            grum,
            human_creature,
            goblin_mask,
        ]);

        // Part A: Blood Moon suppresses SynthShock's own ETB replacement (no prompt, untapped).
        let mut state = boot_2p(db.clone());
        put_on_battlefield(&mut state, PlayerId(1), CardDefId(1)); // MoonStatic
        let shock = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
        let ts_before = state.next_timestamp_counter();
        let moved = move_object(
            &mut state,
            shock,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let MoveObjectOutcome::Moved(raw) = moved else {
            panic!("Blood Moon should make Shockland enter untapped with zero prompt");
        };
        assert!(
            !state
                .object(raw.new_id.unwrap())
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert_eq!(state.players()[0].life, 20);
        assert!(state.next_timestamp_counter() <= ts_before + 2);

        // Part B: Layer 4 turns entering Human into Goblin -> Grumgully lookahead gives +1/+1 counter!
        let mut state2 = boot_2p(db);
        put_on_battlefield(&mut state2, PlayerId(0), CardDefId(3)); // Grumgully
        put_on_battlefield(&mut state2, PlayerId(0), CardDefId(5)); // MaskGoblins
        let h_hand = put_in_hand(&mut state2, PlayerId(0), CardDefId(4)); // printed Human
        let MoveObjectOutcome::Moved(h_raw) = move_object(
            &mut state2,
            h_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected immediate ETB with counter");
        };
        assert_eq!(state2.counter_count(h_raw.new_id.unwrap(), CounterId(0)), 1);
    }

    #[test]
    fn cr_615_damage_prevention_double_zero_and_cant_prevent() {
        let dmg_doubler = make_card(
            "FurnaceDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DealsDamage,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: mtg_ir::DamageScaleSpec {
                        mode: DamageModMode::Double,
                        add: None,
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            dmg_doubler,
        ]);
        let mut state = boot_2p(db);
        let src = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1)); // FurnaceDoubler

        let prev_spec = DamagePreventionSpec {
            mode: PreventMode::Next,
            combat_only: false,
            scope: PreventScope::Subject,
            from_source: None,
            also_prevent_dealt_by_subject: false,
            choose_source: false,
            next_damage_event_only: false,
            on_prevented: None,
            redirect_to: None,
            redirect_source_color_filter: ColorMask::empty(),
            redirect_source_kind: None,
            min_life_floor: None,
            all_damage_to_you_and_perms: false,
            you_and_planeswalkers_you_control: false,
            from_sources_you_dont_control: false,
            from_source_colors: ColorMask::empty(),
            from_source_not_subtypes: SmallVec::new(),
            on_prevented_creature: None,
            on_prevented_noncreature: None,
            on_prevented_to_damage_source_controller: false,
        };
        // Shield prevents 2 damage to Player 1.
        let s_id = create_prevention_shield(
            &mut state,
            PlayerId(1),
            &prev_spec,
            &[TargetRef::Player { id: PlayerId(1) }],
            2,
            None,
        )
        .unwrap();

        // 0-damage short-circuit leaves shield untouched (t2_f09_02).
        let zero_res = resolve_damage(
            &mut state,
            Some(src),
            TargetRef::Player { id: PlayerId(1) },
            0,
            false,
            false,
        )
        .unwrap();
        let ReplacementResult::Resolved(zero_out) = zero_res else {
            panic!("0 damage must short-circuit without prompt");
        };
        let ReplaceableEventKind::Damage { amount: z_amt, .. } = zero_out.event else {
            unreachable!();
        };
        assert_eq!(z_amt, 0);
        assert_eq!(
            state
                .prevention_shields()
                .iter()
                .find(|s| s.id == s_id)
                .unwrap()
                .remaining,
            ShieldRemaining::Finite(2)
        );

        // Unpreventable skips shield without depleting remaining (t2_f09_04).
        let mut state_unprev = state.clone();
        state_unprev.set_damage_cant_be_prevented_this_turn(false);
        let unprev = resolve_damage(
            &mut state_unprev,
            Some(src),
            TargetRef::Player { id: PlayerId(1) },
            3,
            false,
            false,
        )
        .unwrap();
        let ReplacementResult::Resolved(unprev_out) = unprev else {
            panic!("only doubler applies when damage can't be prevented");
        };
        let ReplaceableEventKind::Damage { amount: u_amt, .. } = unprev_out.event else {
            unreachable!();
        };
        assert_eq!(u_amt, 6);
        assert_eq!(
            state_unprev
                .prevention_shields()
                .iter()
                .find(|s| s.id == s_id)
                .unwrap()
                .remaining,
            ShieldRemaining::Finite(2)
        );

        // With prevention enabled: Shield(2) + Doubler on Bolt(3) prompts affected player Player 1 (CR 616.1e, t3_16).
        let mut branch_double_first = state.clone();
        let res = resolve_damage(
            &mut branch_double_first,
            Some(src),
            TargetRef::Player { id: PlayerId(1) },
            3,
            false,
            false,
        )
        .unwrap();
        let ReplacementResult::Paused { prompt_id, chooser } = res else {
            panic!("expected affected player Player 1 to order Doubler vs Shield");
        };
        assert_eq!(chooser, PlayerId(1));
        let mut branch_prevent_first = branch_double_first.clone();

        let ReplacementResult::Resolved(out_a) = resume_pending_replacement(
            &mut branch_double_first,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap() else {
            panic!("expected single remaining replacement after pick 0");
        };
        let ReplacementResult::Resolved(out_b) = resume_pending_replacement(
            &mut branch_prevent_first,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 1 },
        )
        .unwrap() else {
            panic!("expected single remaining replacement after pick 1");
        };
        let (
            ReplaceableEventKind::Damage { amount: a, .. },
            ReplaceableEventKind::Damage { amount: b, .. },
        ) = (out_a.event, out_b.event)
        else {
            unreachable!();
        };
        let mut vals = [a, b];
        vals.sort();
        assert_eq!(vals, [2, 4]);
    }

    #[test]
    fn cr_616_1f_two_skip_replacements_terminate_immediately() {
        let skip_draw = make_card(
            "SkipDraw",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::Skip,
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            skip_draw,
        ]);
        let mut state = boot_2p(db);
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

        let res = resolve_draw(&mut state, PlayerId(0), 1, None).unwrap();
        let ReplacementResult::Paused { prompt_id, chooser } = res else {
            panic!("expected single ChooseReplacement between two Skip effects");
        };
        let res2 = resume_pending_replacement(
            &mut state,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let ReplacementResult::Resolved(out) = res2 else {
            panic!("CR 616.1f must terminate immediately without prompting for second Skip");
        };
        assert!(out.mods.skipped);
    }

    #[test]
    fn cr_903_9_commander_graveyard_vs_hand_library() {
        let gy_to_lib = make_card(
            "WheelSunMoonStyle",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Graveyard, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Library),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            gy_to_lib,
        ]);

        let deck = DeckSpec {
            commanders: vec![CardDefId(0)],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::commander(2);
        cfg.rng_mode = RngMode::Mulberry32Compat;
        let mut state = GameState::new(db, cfg, &[deck.clone(), deck], 7).unwrap();

        let cmd_id = state.players()[0].command[0];
        let cmd_bf = move_object_raw(
            &mut state,
            cmd_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(state.object(cmd_bf).unwrap().is_commander());

        // CR 903.9a: Moving to Graveyard (with NO Gy->Library replacement) moves directly (SBA handles command redirect).
        let mut s_gy = state.clone();
        let MoveObjectOutcome::Moved(gy_raw) = move_object(
            &mut s_gy,
            cmd_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("CR 903.9a Commander -> Graveyard must not prompt replacement");
        };
        assert_eq!(
            s_gy.object(gy_raw.new_id.unwrap()).unwrap().zone,
            Zone::Graveyard
        );

        // CR 903.9b: Moving directly to Hand prompts YesNo -> Yes goes to Command, No goes to Hand.
        let mut s_yes = state.clone();
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s_yes,
            cmd_bf,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("CR 903.9b Commander -> Hand must prompt owner");
        };
        let mut s_no = s_yes.clone();
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(yes_raw)) =
            resume_and_finalize_replacement(
                &mut s_yes,
                chooser,
                prompt_id,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap()
        else {
            panic!("expected ZoneMove");
        };
        assert_eq!(
            s_yes.object(yes_raw.new_id.unwrap()).unwrap().zone,
            Zone::Command
        );
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(no_raw)) =
            resume_and_finalize_replacement(
                &mut s_no,
                chooser,
                prompt_id,
                ChoiceAnswer::YesNo { value: false },
            )
            .unwrap()
        else {
            panic!("expected ZoneMove");
        };
        assert_eq!(
            s_no.object(no_raw.new_id.unwrap()).unwrap().zone,
            Zone::Hand
        );

        // Combined: Graveyard replaced to Library -> CR 903.9b fires dynamically on replaced destination!
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            cmd_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected CR 903.9b prompt after Gy -> Library replacement");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(redir)) =
            resume_and_finalize_replacement(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap()
        else {
            panic!("expected ZoneMove");
        };
        assert_eq!(
            state.object(redir.new_id.unwrap()).unwrap().zone,
            Zone::Command
        );
    }

    #[test]
    fn opponent_discard_to_battlefield_requires_opponent_caused_discard() {
        let discard_reanim = make_card(
            "ObstinateDiscarder",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::Discard,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        opponent_discard_to_battlefield: Some(
                            mtg_ir::OpponentDiscardToBattlefield::WithCounters {
                                counter: CounterId(0),
                                count: 2,
                            },
                        ),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            discard_reanim,
        ]);
        let mut state = boot_2p(db);

        // 1. MoveRawOptions::default() (is_discard: false) Hand -> Graveyard goes to Graveyard.
        let c1 = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Moved(r1) = move_object(
            &mut state,
            c1,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(r1.to_zone, Zone::Graveyard);

        // 2. is_discard: true, discard_caused_by: Some(PlayerId(0)) (self-discard) -> Graveyard.
        let c2 = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Moved(r2) = move_object(
            &mut state,
            c2,
            Zone::Graveyard,
            MoveRawOptions {
                is_discard: true,
                discard_caused_by: Some(PlayerId(0)),
                ..Default::default()
            },
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(r2.to_zone, Zone::Graveyard);

        // 3. is_discard: true, discard_caused_by: None (no resolution frame) -> Graveyard.
        let c3 = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Moved(r3) = move_object(
            &mut state,
            c3,
            Zone::Graveyard,
            MoveRawOptions {
                is_discard: true,
                discard_caused_by: None,
                ..Default::default()
            },
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(r3.to_zone, Zone::Graveyard);

        // 4. is_discard: true, discard_caused_by: Some(PlayerId(1)) -> Battlefield under P0 with 2 +1/+1 counters.
        let c4 = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Moved(r4) = move_object(
            &mut state,
            c4,
            Zone::Graveyard,
            MoveRawOptions {
                is_discard: true,
                discard_caused_by: Some(PlayerId(1)),
                ..Default::default()
            },
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(r4.to_zone, Zone::Battlefield);
        let bf4 = r4.new_id.unwrap();
        assert_eq!(state.object(bf4).unwrap().controller, PlayerId(0));
        assert_eq!(state.counter_count(bf4, CounterId(0)), 2);

        // 5. is_discard: true, discard_caused_by: None with active ResolutionFrame controlled by PlayerId(1).
        let c5 = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let spell_id = put_in_hand(&mut state, PlayerId(1), CardDefId(0));
        let stack_id = state.allocate_stack_id();
        state.stack_entries_mut().push(crate::state::StackEntry {
            id: stack_id,
            kind: crate::state::StackEntryKind::Spell { object: spell_id },
            controller: PlayerId(1),
            choices: Default::default(),
            targets: vec![],
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });
        state
            .resolution_frames_mut()
            .push(crate::state::ResolutionFrame {
                stack_item: stack_id,
                steps_remaining: vec![],
                pc: 0,
                bindings: vec![],
                pending_prompt_id: None,
            });
        let MoveObjectOutcome::Moved(r5) = move_object(
            &mut state,
            c5,
            Zone::Graveyard,
            MoveRawOptions {
                is_discard: true,
                discard_caused_by: None,
                ..Default::default()
            },
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(r5.to_zone, Zone::Battlefield);
        assert_eq!(state.counter_count(r5.new_id.unwrap(), CounterId(0)), 2);
    }

    #[test]
    fn matches_event_filter_none_object_respects_or_self_attached_commander() {
        let synth_shock = make_card(
            "SynthShock",
            CardTypeMask::LAND,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        pay_life_or_enter_tapped: Some(2),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let attach_exiler = make_card(
            "AttachExiler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: None,
                        or_attached_to_self: true,
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
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
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Exile),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let cmd_tapper = make_card(
            "CmdTapper",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: None,
                        or_commander: true,
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            synth_shock,
            attach_exiler,
            cmd_tapper,
        ]);

        // 1. SynthShock (object: None, or_self: true) on BF does NOT fire when Bear enters BF.
        let mut state = boot_2p(db.clone());
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        let bear_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
        let MoveObjectOutcome::Moved(_) = move_object(
            &mut state,
            bear_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("SynthShock must not intercept Bear entering BF");
        };
        assert_eq!(state.players()[0].life, 20);

        // 2. or_attached_to_self with object: None only matches attached host.
        let cre_a = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let cre_b = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let aura = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        crate::zones::attach_raw(&mut state, aura, cre_a).unwrap();

        let MoveObjectOutcome::Moved(raw_b) = move_object(
            &mut state,
            cre_b,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(raw_b.to_zone, Zone::Graveyard);

        let MoveObjectOutcome::Moved(raw_a) = move_object(
            &mut state,
            cre_a,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert_eq!(raw_a.to_zone, Zone::Exile);

        // 3. or_commander with object: None only matches commander.
        let deck = DeckSpec {
            commanders: vec![CardDefId(0)],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::commander(2);
        cfg.rng_mode = RngMode::Mulberry32Compat;
        let mut cmd_state = GameState::new(db, cfg, &[deck.clone(), deck], 11).unwrap();
        put_on_battlefield(&mut cmd_state, PlayerId(1), CardDefId(3));

        let normal_hand = put_in_hand(&mut cmd_state, PlayerId(0), CardDefId(0));
        let MoveObjectOutcome::Moved(norm_raw) = move_object(
            &mut cmd_state,
            normal_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert!(
            !cmd_state
                .object(norm_raw.new_id.unwrap())
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );

        let cmd_id = cmd_state.players()[0].command[0];
        let MoveObjectOutcome::Moved(cmd_raw) = move_object(
            &mut cmd_state,
            cmd_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Moved");
        };
        assert!(
            cmd_state
                .object(cmd_raw.new_id.unwrap())
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
    }

    #[test]
    fn etb_copy_and_etb_choice_preserve_trailing_tapped_counters_and_granted() {
        let clone_with_trailing = make_card(
            "CloneWithTrailing",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        tapped: Some(true),
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 2 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let choice_with_trailing = make_card(
            "ChoiceWithTrailing",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![EtbChoiceSpec::Color { exclude: None }],
                        reveal_unless_controls: Some(Box::new(ObjectFilter {
                            subtypes_any: smallvec::smallvec![SubtypeId(0)], // Mountain
                            ..Default::default()
                        })),
                        tapped: Some(false),
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let competing_self_etb = make_card(
            "CompetingSelfEtb",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 3 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let granted_rep_ab = rep_ability(ReplacementDef {
            replaces: zc_to(Zone::Battlefield, true),
            with: ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    etb_choices: vec![EtbChoiceSpec::Color { exclude: None }],
                    counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                    ..Default::default()
                }),
            },
            self_replacement: false,
            who: None,
            condition: None,
        });
        let rep_granter = make_card(
            "RepGranter",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(granted_rep_ab),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            clone_with_trailing,
            choice_with_trailing,
            competing_self_etb,
            rep_granter,
        ]);

        // (a) Clone with trailing tapped: Some(true) + counters: 2 +1/+1
        let mut s1 = boot_2p(db.clone());
        let bear_bf = put_on_battlefield(&mut s1, PlayerId(0), CardDefId(0));
        let clone_hand = put_in_hand(&mut s1, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s1,
            clone_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw1)) =
            resume_and_finalize_replacement(
                &mut s1,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after EtbCopy");
        };
        let bf1 = raw1.new_id.unwrap();
        assert_eq!(
            s1.rare(bf1).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(0)
        );
        assert!(s1.object(bf1).unwrap().flags.contains(ObjectFlags::TAPPED));
        assert_eq!(s1.counter_count(bf1, CounterId(0)), 2);

        // (b) ChoiceWithTrailing + CompetingSelfEtb (both Tier 1) -> pick Choice A -> Color ->
        // must NOT re-prompt ChooseReplacement mid-A, trailing counters + tapped: false + Choice B run cleanly.
        let mut s2 = boot_2p(db.clone());
        put_on_battlefield(&mut s2, PlayerId(0), CardDefId(3));
        let ch_hand = put_in_hand(&mut s2, PlayerId(0), CardDefId(2));
        let MoveObjectOutcome::Paused {
            prompt_id: p_ord,
            chooser: c_ord,
        } = move_object(
            &mut s2,
            ch_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        else {
            panic!("expected Tier 1 ChooseReplacement pause");
        };
        assert!(matches!(
            s2.pending_replacement().unwrap().prompt_kind,
            PendingReplacementPrompt::ChooseReplacement
        ));
        let ReplacementResumeOutcome::Paused {
            prompt_id: p_col,
            chooser: c_col,
        } = resume_and_finalize_replacement(
            &mut s2,
            c_ord,
            p_ord,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap()
        else {
            panic!("expected EtbChoice Color pause");
        };
        assert!(matches!(
            s2.pending_replacement().unwrap().prompt_kind,
            PendingReplacementPrompt::EtbChoice { .. }
        ));
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw2)) =
            resume_and_finalize_replacement(
                &mut s2,
                c_col,
                p_col,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved without re-prompting ChooseReplacement mid-A");
        };
        let bf2 = raw2.new_id.unwrap();
        assert_eq!(s2.rare(bf2).unwrap().chosen_color, Some(ColorMask::W));
        assert!(!s2.object(bf2).unwrap().flags.contains(ObjectFlags::TAPPED));
        assert_eq!(s2.counter_count(bf2, CounterId(0)), 4); // 1 from A + 3 from B

        // (c) GrantedReplacement with etb_choices + trailing counters
        let mut s3 = boot_2p(db);
        put_on_battlefield(&mut s3, PlayerId(0), CardDefId(4));
        let bear_hand = put_in_hand(&mut s3, PlayerId(0), CardDefId(0));
        let MoveObjectOutcome::Paused {
            prompt_id: p_g,
            chooser: c_g,
        } = move_object(
            &mut s3,
            bear_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        else {
            panic!("expected EtbChoice from GrantedReplacement");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw3)) =
            resume_and_finalize_replacement(&mut s3, c_g, p_g, ChoiceAnswer::PickMode { index: 1 })
                .unwrap()
        else {
            panic!("expected Moved after GrantedReplacement choice");
        };
        let bf3 = raw3.new_id.unwrap();
        assert_eq!(s3.rare(bf3).unwrap().chosen_color, Some(ColorMask::U));
        assert_eq!(s3.counter_count(bf3, CounterId(0)), 1);
    }

    #[test]
    fn cr_616_1b_4p_enter_under_opponent_control_updates_clone_chooser_and_copy_mod_controller() {
        let clone_card = make_card(
            "ArtifactCheckingClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: Some(Box::new(mtg_ir::CopyModificationSpec {
                                conditional_counters: Some(mtg_ir::ConditionalCopyCounter {
                                    condition: Box::new(mtg_ir::Condition::Compare {
                                        left: Box::new(Amount::Count {
                                            of: Box::new(ObjectFilter {
                                                types_any: CardTypeMask::ARTIFACT,
                                                controller: Some(RelativeControl::You),
                                                ..Default::default()
                                            }),
                                        }),
                                        op: mtg_ir::CompareOp::Ge,
                                        right: Box::new(Amount::Const { n: 1 }),
                                    }),
                                    counter: CounterId(0),
                                    count: 2,
                                }),
                                ..Default::default()
                            })),
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let stealer = make_card(
            "OppStealer",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enter_under_opponent_control: true,
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let two_colors_card = make_card(
            "TwoColorTest",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![EtbChoiceSpec::TwoPlayers],
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]),
            clone_card,
            stealer,
            make_card("Artifact", CardTypeMask::ARTIFACT, vec![]),
            two_colors_card,
        ]);

        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::commander(4);
        cfg.rng_mode = RngMode::Mulberry32Compat;
        let mut state = GameState::new(
            db.clone(),
            cfg,
            &[deck.clone(), deck.clone(), deck.clone(), deck],
            5,
        )
        .unwrap();

        let bear_bf = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        put_on_battlefield(&mut state, PlayerId(1), CardDefId(2)); // OppStealer
        put_on_battlefield(&mut state, PlayerId(2), CardDefId(3)); // P2 controls Artifact (P0 does not)
        let clone_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(1));

        // Warm layer cache pre-copy (m4_chal_2 regression).
        let _ = state.evaluate_board();

        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            clone_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Tier 2 ChooseOpponentForControl pause");
        };
        assert_eq!(chooser, PlayerId(0));
        assert!(matches!(
            state.pending_replacement().unwrap().prompt_kind,
            PendingReplacementPrompt::EtbChoice {
                step: EtbInteractiveStep::ChooseOpponentForControl { .. },
                ..
            }
        ));

        // Opponents of P0 are [P1, P2, P3]; index 1 picks P2!
        let ReplacementResumeOutcome::Paused {
            prompt_id: clone_pid,
            chooser: clone_chooser,
        } = resume_and_finalize_replacement(
            &mut state,
            chooser,
            prompt_id,
            ChoiceAnswer::PickMode { index: 1 },
        )
        .unwrap()
        else {
            panic!("expected Tier 3 EtbCopy pause");
        };
        assert_eq!(clone_chooser, PlayerId(2));
        assert!(
            state
                .rare(clone_hand)
                .is_none_or(|r| r.chosen_player.is_none())
        );

        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
            resume_and_finalize_replacement(
                &mut state,
                clone_chooser,
                clone_pid,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap()
        else {
            panic!("expected final Moved");
        };
        let final_id = raw.new_id.unwrap();
        assert_eq!(state.object(final_id).unwrap().controller, PlayerId(2));
        assert_eq!(state.counter_count(final_id, CounterId(0)), 2);
        let _ = state.evaluate_board();

        // Also test ChooseTwoColors auto-pick with 2-color pool marks DirtyMask::DESIGNATIONS.
        let mut s2 = boot_2p(db);
        let c_id = put_in_hand(&mut s2, PlayerId(0), CardDefId(0));
        let pid = s2.allocate_prompt_id();
        let rev_before = s2.layer_rev();
        s2.set_pending_replacement(Some(PendingReplacementState {
            prompt_id: pid,
            chooser: PlayerId(0),
            event: ReplaceableEventKind::ZoneChange {
                object: c_id,
                from: Zone::Hand,
                to: Zone::Battlefield,
                controller: PlayerId(0),
            },
            applied: SmallVec::new(),
            candidates: vec![],
            prompt_kind: PendingReplacementPrompt::EtbChoice {
                source_id: ReplacementSourceId::printed(
                    ReplacementSourceKind::PrintedReplacement,
                    c_id,
                    CardDefId(0),
                    0,
                    0,
                ),
                source_controller: PlayerId(0),
                mod_spec: Box::new(ZoneChangeMod::default()),
                step: EtbInteractiveStep::ChooseTwoColors {
                    first: None,
                    colors: vec![ManaColor::W, ManaColor::U],
                },
                step_idx: 0,
            },
            mods: ReplacementAccumulatedMods::default(),
            continuation: ReplacementContinuation::ZoneMove,
            resume_decision: DecisionState::Priority,
            resume_priority_player: PlayerId(0),
        }));
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw2)) =
            resume_and_finalize_replacement(
                &mut s2,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved");
        };
        let c_bf = raw2.new_id.unwrap();
        assert!(s2.layer_rev() > rev_before);
        assert_eq!(
            s2.rare(c_bf).unwrap().chosen_colors.as_slice(),
            &[ManaColor::W, ManaColor::U]
        );
    }

    #[test]
    fn rule_mods_block_life_gain_draws_player_counters_and_scoped_unpreventable() {
        let rule_card = |name: &str, rule: mtg_ir::RuleMod, relation: PlayerRelation| {
            make_card(
                name,
                CardTypeMask::ENCHANTMENT,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::RuleModifier {
                            rule,
                            who: PlayerFilter { relation },
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )
        };
        let life_doubler = make_card(
            "LifeDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::GainLife,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: true,
                        add: None,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let draw_doubler = make_card(
            "DrawDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: Some(2),
                        delta: None,
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![],
                    counters: vec!["poison".into(), "energy".into()],
                    keywords: vec![],
                },
                cards: vec![
                    make_card("Bear", CardTypeMask::CREATURE, vec![]),
                    rule_card(
                        "CantGain",
                        mtg_ir::RuleMod::CantGainLife,
                        PlayerRelation::You,
                    ),
                    rule_card(
                        "LifeLock",
                        mtg_ir::RuleMod::LifeTotalCantChange,
                        PlayerRelation::You,
                    ),
                    rule_card(
                        "CantDraw",
                        mtg_ir::RuleMod::CantDrawCards,
                        PlayerRelation::You,
                    ),
                    rule_card(
                        "OneDraw",
                        mtg_ir::RuleMod::CantDrawExtraCards,
                        PlayerRelation::You,
                    ),
                    rule_card(
                        "NoPoison",
                        mtg_ir::RuleMod::PlayerCantGetCounters {
                            counter: Some(CounterId(0)),
                        },
                        PlayerRelation::You,
                    ),
                    rule_card(
                        "SrcUnprev",
                        mtg_ir::RuleMod::DamageCantBePrevented,
                        PlayerRelation::You,
                    ),
                    life_doubler,
                    draw_doubler,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // 1. CantGainLife & LifeTotalCantChange block life gain with 0 ChooseReplacement prompts.
        for def_id in [CardDefId(1), CardDefId(2)] {
            let mut s = boot_2p(db.clone());
            put_on_battlefield(&mut s, PlayerId(0), def_id);
            put_on_battlefield(&mut s, PlayerId(0), CardDefId(7));
            put_on_battlefield(&mut s, PlayerId(0), CardDefId(7));
            let ReplacementResult::Resolved(out) =
                resolve_life_gain(&mut s, PlayerId(0), 5, None).unwrap()
            else {
                panic!("CantGainLife/LifeTotalCantChange must short-circuit without prompt");
            };
            assert!(out.mods.skipped);
            assert!(matches!(
                out.event,
                ReplaceableEventKind::LifeGain { amount: 0, .. }
            ));
        }

        // 2. CantDrawCards blocks draw; CantDrawExtraCards allows 1st draw (clamped to 1) & blocks 2nd.
        let mut s_cd = boot_2p(db.clone());
        put_on_battlefield(&mut s_cd, PlayerId(0), CardDefId(3));
        let ReplacementResult::Resolved(cd_out) =
            resolve_draw(&mut s_cd, PlayerId(0), 2, None).unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(cd_out.mods.skipped);
        assert!(matches!(
            cd_out.event,
            ReplaceableEventKind::Draw { count: 0, .. }
        ));

        let mut s_one = boot_2p(db.clone());
        put_on_battlefield(&mut s_one, PlayerId(0), CardDefId(4));
        put_on_battlefield(&mut s_one, PlayerId(0), CardDefId(8));
        let ReplacementResult::Resolved(d1) =
            resolve_draw(&mut s_one, PlayerId(0), 3, None).unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(!d1.mods.skipped);
        assert!(matches!(
            d1.event,
            ReplaceableEventKind::Draw { count: 1, .. }
        ));

        s_one.player_mut(PlayerId(0)).unwrap().cards_drawn_this_turn = 1;
        let ReplacementResult::Resolved(d2) =
            resolve_draw(&mut s_one, PlayerId(0), 1, None).unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(d2.mods.skipped);
        assert!(matches!(
            d2.event,
            ReplaceableEventKind::Draw { count: 0, .. }
        ));

        // 3. PlayerCantGetCounters { counter: Some(poison) } blocks poison, allows energy.
        let mut s_pc = boot_2p(db.clone());
        put_on_battlefield(&mut s_pc, PlayerId(0), CardDefId(5));
        let ReplacementResult::Resolved(poison_out) =
            resolve_put_player_counters(&mut s_pc, PlayerId(0), CounterId(0), 2, None, None)
                .unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(poison_out.mods.skipped);
        assert!(matches!(
            poison_out.event,
            ReplaceableEventKind::PutPlayerCounters { count: 0, .. }
        ));
        let ReplacementResult::Resolved(energy_out) =
            resolve_put_player_counters(&mut s_pc, PlayerId(0), CounterId(1), 2, None, None)
                .unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(!energy_out.mods.skipped);
        assert!(matches!(
            energy_out.event,
            ReplaceableEventKind::PutPlayerCounters { count: 2, .. }
        ));

        // 4. DamageCantBePrevented on source's controller makes damage unpreventable.
        let mut s_unp = boot_2p(db);
        put_on_battlefield(&mut s_unp, PlayerId(0), CardDefId(6));
        let src_obj = put_on_battlefield(&mut s_unp, PlayerId(0), CardDefId(0));
        assert!(is_damage_unpreventable(
            &s_unp,
            Some(src_obj),
            TargetRef::Player { id: PlayerId(1) },
            false,
            false
        ));
    }

    #[test]
    fn etb_discard_for_keyword_counters_and_remove_counters_for_plus_counters() {
        let kw_ab = |kw: KeywordId| {
            AbilityDef::Keyword(mtg_ir::KeywordAbility {
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
            })
        };
        let rex = make_card(
            "RexDiscard",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Battlefield),
                        etb_discard_for_keyword_counters: smallvec::smallvec![
                            KeywordId(0),
                            KeywordId(1)
                        ],
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let remover = make_card(
            "CounterEater",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Battlefield),
                        etb_remove_counters_for_plus_counters: Some((2, None)),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![],
                    counters: vec!["+1/+1".into(), "flying".into(), "deathtouch".into()],
                    keywords: vec!["Flying".into(), "Deathtouch".into(), "Trample".into()],
                },
                cards: vec![
                    make_card("Filler", CardTypeMask::CREATURE, vec![]),
                    make_card("Flyer", CardTypeMask::CREATURE, vec![kw_ab(KeywordId(0))]),
                    make_card(
                        "Trampler",
                        CardTypeMask::CREATURE,
                        vec![kw_ab(KeywordId(2))],
                    ),
                    make_card("NonCreatureSpell", CardTypeMask::INSTANT, vec![]),
                    rex,
                    remover,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = boot_2p(db.clone());

        // (a) etb_discard_for_keyword_counters
        let flyer_h = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let _trampler_h = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
        let noncre_h = put_in_hand(&mut state, PlayerId(0), CardDefId(3));
        let rex_h = put_in_hand(&mut state, PlayerId(0), CardDefId(4));

        crate::events::begin_action(&mut state);
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            rex_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected DiscardForKeywordCounters pause");
        };
        let legal = legal_replacement_actions(&state, chooser);
        assert!(legal.iter().any(|a| matches!(
            a,
            Action::Answer {
                answer: ChoiceAnswer::PickObject { id },
                ..
            } if *id == flyer_h
        )));
        assert!(!legal.iter().any(|a| matches!(
            a,
            Action::Answer {
                answer: ChoiceAnswer::PickObject { id },
                ..
            } if *id == noncre_h
        )));
        let ReplacementResumeOutcome::Paused {
            prompt_id: pid2,
            chooser: c2,
        } = resume_and_finalize_replacement(
            &mut state,
            chooser,
            prompt_id,
            ChoiceAnswer::PickObject { id: flyer_h },
        )
        .unwrap()
        else {
            panic!("expected second pick/DonePicking prompt");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(rex_raw)) =
            resume_and_finalize_replacement(&mut state, c2, pid2, ChoiceAnswer::DonePicking)
                .unwrap()
        else {
            panic!("expected Moved");
        };
        let rex_bf = rex_raw.new_id.unwrap();
        assert_eq!(state.counter_count(rex_bf, CounterId(1)), 1); // flying counter
        assert_eq!(state.counter_count(rex_bf, CounterId(2)), 0); // no deathtouch
        assert!(
            crate::events::peek_events(&state)
                .iter()
                .any(|e| { matches!(e.kind, crate::events::AtomicEventKind::Discard { .. }) })
        );
        assert!(
            state.players()[0]
                .graveyard
                .iter()
                .any(|&id| { state.object(id).is_some_and(|o| o.card_def == CardDefId(1)) })
        );
        crate::events::end_action(&mut state);

        // (b) etb_remove_counters_for_plus_counters
        let mut state = boot_2p(db);
        let host = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        state.add_counter(host, CounterId(0), 3);
        let eater_h = put_in_hand(&mut state, PlayerId(0), CardDefId(5));
        crate::events::begin_action(&mut state);
        let MoveObjectOutcome::Paused {
            prompt_id: ep1,
            chooser: ec1,
        } = move_object(
            &mut state,
            eater_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        else {
            panic!("expected RemoveCountersForPlusCounters pause");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(eater_raw)) =
            resume_and_finalize_replacement(
                &mut state,
                ec1,
                ep1,
                ChoiceAnswer::PickObject { id: host },
            )
            .unwrap()
        else {
            panic!("expected Moved (only 1 candidate so auto-finalized)");
        };
        let eater_bf = eater_raw.new_id.unwrap();
        assert_eq!(state.counter_count(host, CounterId(0)), 0);
        assert_eq!(state.counter_count(eater_bf, CounterId(0)), 6);
        assert!(crate::events::peek_events(&state).iter().any(|e| {
            matches!(
                e.kind,
                crate::events::AtomicEventKind::CounterChanged {
                    object,
                    previous_count: 3,
                    new_count: 0,
                    ..
                } if object == host
            )
        }));
        crate::events::end_action(&mut state);
    }

    #[test]
    fn cr_615_12_phantom_counter_removal_and_minors() {
        let phantom_card = make_card(
            "PhantomDude",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::StaticDamagePrevention {
                        combat_only: false,
                        noncombat_only: false,
                        amount: StaticDamageAmount::All,
                        target: StaticDamageTarget::SelfObj,
                        also_prevent_dealt_by: false,
                        dealt_by_only: None,
                        dealt_by_target_filter: None,
                        source_filter: None,
                        phantom_counter_removal: Some(mtg_ir::PhantomCounterRemoval {
                            counter: CounterId(0),
                            remove_that_many: true,
                            require_counter_present: true,
                        }),
                        on_prevented: None,
                        prevent_opponent_only: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let your_token_doubler = make_card(
            "YourTokenDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: mtg_ir::MultiplierTarget::Tokens,
                        factor: 2.0,
                        delta: 0,
                        filter: Some(Box::new(ObjectFilter {
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        })),
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let manufactor = make_card(
            "Manufactor",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::TokenReplacement {
                        one_of_each: true,
                        match_subtypes: smallvec::smallvec![
                            SubtypeId(0),
                            SubtypeId(1),
                            SubtypeId(2)
                        ],
                        replacement_token: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let abundance = make_card(
            "DrawOrLand",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::AbundanceDrawChoice,
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let draw_doubler = make_card(
            "DrawDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: Some(2),
                        delta: None,
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let dmg_to_minus = make_card(
            "DmgToMinus",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DealtDamage,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageToMinusCounters,
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let dmg_doubler = make_card(
            "DmgDoubler",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DealtDamage,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: mtg_ir::DamageScaleSpec {
                        mode: DamageModMode::Double,
                        add: None,
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let player_counter_doubler = make_card(
            "PlayerCtrRep",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::CounterPlaced,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::ProliferateMultiplier { factor: 2 },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let look_and_name = make_card(
            "PithingLook",
            CardTypeMask::ARTIFACT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![EtbChoiceSpec::CardName {
                            nonland_only: false,
                            look_at_opponent_hand: true,
                        }],
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Clue".into(), "Food".into(), "Treasure".into()],
                    counters: vec!["+1/+1".into(), "poison".into()],
                    keywords: vec![],
                },
                cards: vec![
                    make_card("Zebra", CardTypeMask::CREATURE, vec![]), // 0
                    phantom_card,                                       // 1
                    your_token_doubler,                                 // 2
                    manufactor,                                         // 3
                    abundance,                                          // 4
                    draw_doubler,                                       // 5
                    dmg_to_minus,                                       // 6
                    dmg_doubler,                                        // 7
                    player_counter_doubler,                             // 8
                    look_and_name,                                      // 9
                    make_card("Alpha", CardTypeMask::CREATURE, vec![]), // 10
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // 1. PhantomCounterRemoval: 0 counters -> 0 prevented; 2 counters + unpreventable -> 5 dmg + 2 removed + CounterChanged
        let mut s1 = boot_2p(db.clone());
        let ph = put_on_battlefield(&mut s1, PlayerId(0), CardDefId(1));
        let ReplacementResult::Resolved(r_zero) =
            resolve_damage(&mut s1, None, TargetRef::Object { id: ph }, 5, false, false).unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(matches!(
            r_zero.event,
            ReplaceableEventKind::Damage { amount: 5, .. }
        ));
        assert_eq!(r_zero.mods.prevented_damage, 0);

        s1.add_counter(ph, CounterId(0), 2);
        crate::events::begin_action(&mut s1);
        let ReplacementResult::Resolved(r_unp) =
            resolve_damage(&mut s1, None, TargetRef::Object { id: ph }, 5, false, true).unwrap()
        else {
            panic!("expected Resolved");
        };
        assert!(matches!(
            r_unp.event,
            ReplaceableEventKind::Damage { amount: 5, .. }
        ));
        assert_eq!(r_unp.mods.prevented_damage, 0);
        assert_eq!(s1.counter_count(ph, CounterId(0)), 0);
        assert!(crate::events::peek_events(&s1).iter().any(|e| matches!(
            e.kind,
            crate::events::AtomicEventKind::CounterChanged {
                object,
                previous_count: 2,
                new_count: 0,
                ..
            } if object == ph
        )));
        crate::events::end_action(&mut s1);

        // 2. MultiplierTarget::Tokens filter (You vs Opponent) + 2x Academy Manufactor + opponent Manufactor ignored
        let mut s2 = boot_2p(db.clone());
        put_on_battlefield(&mut s2, PlayerId(0), CardDefId(2)); // YourTokenDoubler for P0
        put_on_battlefield(&mut s2, PlayerId(0), CardDefId(3)); // Manufactor #1 for P0
        put_on_battlefield(&mut s2, PlayerId(0), CardDefId(3)); // Manufactor #2 for P0
        put_on_battlefield(&mut s2, PlayerId(1), CardDefId(3)); // Manufactor for P1 (must NOT apply to P0!)

        let clue_spec = TokenSpec {
            name: "Clue".into(),
            power: 0,
            toughness: 0,
            colors: ColorMask::empty(),
            types: CardTypeMask::ARTIFACT,
            supertypes: SupertypeMask::empty(),
            subtypes: smallvec::smallvec![SubtypeId(0)],
            type_line: "Artifact — Clue".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };
        // P0 creates 1 Clue; P0 picks Manufactor #1 -> Manufactor #2 -> YourTokenDoubler (or vice versa).
        // Let's test 2 Manufactors alone first for exact 3 each, and YourTokenDoubler separately on P0 vs P1!
        let mut s2_man = boot_2p(db.clone());
        put_on_battlefield(&mut s2_man, PlayerId(0), CardDefId(3));
        put_on_battlefield(&mut s2_man, PlayerId(0), CardDefId(3));
        put_on_battlefield(&mut s2_man, PlayerId(1), CardDefId(3)); // opponent's ignored
        let ReplacementResult::Paused { prompt_id, chooser } = resolve_create_token(
            &mut s2_man,
            PlayerId(0),
            Some(clue_spec.clone()),
            1,
            None,
            CreateTokenOptions::default(),
        )
        .unwrap() else {
            panic!("expected ChooseReplacement between two P0 Manufactors");
        };
        let ReplacementResult::Resolved(man_out) = resume_pending_replacement(
            &mut s2_man,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap() else {
            panic!("expected Resolved after 2nd Manufactor");
        };
        for tok_name in ["Clue", "Food", "Treasure"] {
            let sum: u32 = man_out
                .mods
                .replace_tokens
                .iter()
                .filter(|(s, _)| s.name == tok_name)
                .map(|(_, c)| *c)
                .sum();
            assert_eq!(sum, 3, "expected 3 {tok_name} tokens with 2 Manufactors");
        }

        // YourTokenDoubler doubles P0's tokens, not P1's.
        let mut s2_filt = boot_2p(db.clone());
        put_on_battlefield(&mut s2_filt, PlayerId(0), CardDefId(2));
        let ReplacementResult::Resolved(p0_tok) = resolve_create_token(
            &mut s2_filt,
            PlayerId(0),
            Some(clue_spec.clone()),
            1,
            None,
            CreateTokenOptions::default(),
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(matches!(
            p0_tok.event,
            ReplaceableEventKind::CreateToken { count: 2, .. }
        ));
        let ReplacementResult::Resolved(p1_tok) = resolve_create_token(
            &mut s2_filt,
            PlayerId(1),
            Some(clue_spec),
            1,
            None,
            CreateTokenOptions::default(),
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(matches!(
            p1_tok.event,
            ReplaceableEventKind::CreateToken { count: 1, .. }
        ));

        // 3. AbundanceDrawChoice & DamageToMinusCounters terminate Tier-5 loop (CR 616.1f).
        let mut s3 = boot_2p(db.clone());
        put_on_battlefield(&mut s3, PlayerId(0), CardDefId(4)); // Abundance
        put_on_battlefield(&mut s3, PlayerId(0), CardDefId(5)); // DrawDoubler
        let ReplacementResult::Paused {
            prompt_id: ab_pid,
            chooser: ab_c,
        } = resolve_draw(&mut s3, PlayerId(0), 1, None).unwrap()
        else {
            panic!("expected ChooseReplacement");
        };
        // Pick Abundance (index 0) -> prompts YesNo -> answer Yes -> DrawDoubler must NOT run afterward!
        let ReplacementResult::Paused {
            prompt_id: ab_yn,
            chooser: ab_c2,
        } = resume_pending_replacement(
            &mut s3,
            ab_c,
            ab_pid,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap()
        else {
            panic!("expected Abundance YesNo");
        };
        let ReplacementResult::Resolved(ab_done) =
            resume_pending_replacement(&mut s3, ab_c2, ab_yn, ChoiceAnswer::YesNo { value: true })
                .unwrap()
        else {
            panic!("Abundance must terminate CR 616.1f loop without running DrawDoubler");
        };
        assert_eq!(ab_done.mods.abundance_land, Some(true));

        let mut s4 = boot_2p(db.clone());
        put_on_battlefield(&mut s4, PlayerId(0), CardDefId(6)); // DmgToMinus
        put_on_battlefield(&mut s4, PlayerId(0), CardDefId(7)); // DmgDoubler
        let opp_cre = put_on_battlefield(&mut s4, PlayerId(1), CardDefId(0));
        let ReplacementResult::Paused {
            prompt_id: d_pid,
            chooser: d_c,
        } = resolve_damage(
            &mut s4,
            None,
            TargetRef::Object { id: opp_cre },
            2,
            false,
            false,
        )
        .unwrap()
        else {
            panic!("expected ChooseReplacement");
        };
        let ReplacementResult::Resolved(d_out) = resume_pending_replacement(
            &mut s4,
            d_c,
            d_pid,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap() else {
            panic!("DamageToMinusCounters must terminate CR 616.1f loop");
        };
        assert!(d_out.mods.damage_as_minus_counters);
        assert!(matches!(
            d_out.event,
            ReplaceableEventKind::Damage { amount: 2, .. }
        ));

        // 4. PutPlayerCounters matches EventKind::CounterPlaced replacement; CardName sorts & reveals opp hand.
        let mut s5 = boot_2p(db);
        put_on_battlefield(&mut s5, PlayerId(0), CardDefId(8));
        let cand_list = collect_applicable_candidates(
            &mut s5,
            &ReplaceableEventKind::PutPlayerCounters {
                player: PlayerId(0),
                counter: CounterId(1),
                count: 1,
                placer: None,
                source: None,
            },
            &ReplacementAccumulatedMods::default(),
            &[],
        );
        assert_eq!(cand_list.len(), 1);

        let opp_hand_card = put_in_hand(&mut s5, PlayerId(1), CardDefId(0));
        let pith_hand = put_in_hand(&mut s5, PlayerId(0), CardDefId(9));
        let MoveObjectOutcome::Paused { .. } = move_object(
            &mut s5,
            pith_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected CardName ETB pause");
        };
        assert!(crate::zones::is_known_to(&s5, opp_hand_card, PlayerId(0)));
        if let PendingReplacementPrompt::EtbChoice {
            step: EtbInteractiveStep::ChooseCardName { candidates },
            ..
        } = &s5.pending_replacement().unwrap().prompt_kind
        {
            assert_eq!(candidates.first().map(String::as_str), Some("Alpha"));
            assert_eq!(candidates.last().map(String::as_str), Some("Zebra"));
        } else {
            panic!("expected ChooseCardName step");
        }
    }

    #[test]
    fn multi_zone_change_mod_synchronous_reset() {
        let zc_creature_to = |to: Zone| EventPattern {
            event: EventKind::ZoneChange,
            filter: EventFilter {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                }),
                detail: Some(EventDetail::ZoneChange {
                    from: ZoneMask::empty(),
                    to: Some(to),
                    from_anywhere: false,
                    not_from_zone: None,
                    not_to_zone: None,
                    enters_untapped: false,
                    dealt_damage_by_self_this_turn: false,
                }),
                ..Default::default()
            },
        };

        // (a) etb_pre_done + etb_answered_steps[0] across Tier 2 steal (2p) -> Tier 5 ZoneChangeMod
        let stealer_with_zero_cand_choice = make_card(
            "StealerWithZeroCandChoice",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enter_under_opponent_control: true,
                        etb_choices: vec![EtbChoiceSpec::ObjectOnBattlefield {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                        }],
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let prepared_day_color_ench = make_card(
            "PreparedDayColorEnch",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enters_prepared: true,
                        becomes_day_if_neither: true,
                        etb_choices: vec![EtbChoiceSpec::Color { exclude: None }],
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        // (b) etb_copy_done across two Tier 3 copy replacements via ChooseReplacement
        let artifact_only_clone = make_card(
            "ArtifactOnlyClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let creature_copier_ench = make_card(
            "CreatureCopierEnch",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        // (c1) etb_pre_done for `to` + `exile_with_counter` (Tier 1 self-ETB -> Tier 5)
        let self_tapped_cre = make_card(
            "SelfTappedCre",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let exile_with_counter_ench = make_card(
            "ExileWithCounterEnch",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Exile),
                        exile_with_counter: Some(CounterId(0)),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        // (c2) etb_pre_done for `to` + `shuffle_into_library` (Tier 1 self-GY -> Tier 5)
        let self_gy_etb_zero = make_card(
            "SelfGyEtbZero",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Graveyard, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![EtbChoiceSpec::ObjectOnBattlefield {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                        }],
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let gy_to_library_shuffle_ench = make_card(
            "GyToLibraryShuffleEnch",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Graveyard),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Library),
                        shuffle_into_library: true,
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]), // CardDefId(0)
            stealer_with_zero_cand_choice,                     // CardDefId(1)
            prepared_day_color_ench,                           // CardDefId(2)
            artifact_only_clone,                               // CardDefId(3)
            creature_copier_ench,                              // CardDefId(4)
            self_tapped_cre,                                   // CardDefId(5)
            exile_with_counter_ench,                           // CardDefId(6)
            self_gy_etb_zero,                                  // CardDefId(7)
            gy_to_library_shuffle_ench,                        // CardDefId(8)
        ]);

        // Execute (a)
        let mut s1 = boot_2p(db.clone());
        put_on_battlefield(&mut s1, PlayerId(0), CardDefId(1));
        put_on_battlefield(&mut s1, PlayerId(0), CardDefId(2));
        assert_eq!(s1.day_night(), None);
        let bear_h = put_in_hand(&mut s1, PlayerId(0), CardDefId(0));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s1,
            bear_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbChoice Color pause from Tier 5 PreparedDayColorEnch");
        };
        assert_eq!(chooser, PlayerId(1));
        assert!(matches!(
            s1.pending_replacement().unwrap().prompt_kind,
            PendingReplacementPrompt::EtbChoice {
                step: EtbInteractiveStep::ChooseColor { .. },
                ..
            }
        ));
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw1)) =
            resume_and_finalize_replacement(
                &mut s1,
                chooser,
                prompt_id,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved after ChooseColor");
        };
        let bf1 = raw1.new_id.unwrap();
        assert_eq!(s1.object(bf1).unwrap().controller, PlayerId(1));
        assert!(
            s1.object(bf1)
                .unwrap()
                .flags
                .contains(ObjectFlags::PREPARED)
        );
        assert_eq!(s1.day_night(), Some(false));
        assert_eq!(s1.rare(bf1).unwrap().chosen_color, Some(ColorMask::W));
        let _ = s1.evaluate_board();

        // Execute (b)
        let mut s2 = boot_2p(db.clone());
        let bear_bf = put_on_battlefield(&mut s2, PlayerId(0), CardDefId(0));
        put_on_battlefield(&mut s2, PlayerId(0), CardDefId(4));
        let clone_h = put_in_hand(&mut s2, PlayerId(0), CardDefId(3));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s2,
            clone_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected ChooseReplacement between ArtifactOnlyClone and CreatureCopierEnch");
        };
        let clone_cand_idx = s2
            .pending_replacement()
            .unwrap()
            .candidates
            .iter()
            .position(|sid| sid.object == clone_h)
            .expect("ArtifactOnlyClone candidate missing");
        let ReplacementResumeOutcome::Paused {
            prompt_id: copy_pid,
            chooser: copy_c,
        } = resume_and_finalize_replacement(
            &mut s2,
            chooser,
            prompt_id,
            ChoiceAnswer::PickReplacement {
                index: clone_cand_idx as u16,
            },
        )
        .unwrap()
        else {
            panic!("expected EtbCopy pause from CreatureCopierEnch after ArtifactOnlyClone");
        };
        if let PendingReplacementPrompt::EtbCopy { candidates, .. } =
            &s2.pending_replacement().unwrap().prompt_kind
        {
            assert!(candidates.contains(&bear_bf));
        } else {
            panic!("expected EtbCopy prompt");
        }
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw2)) =
            resume_and_finalize_replacement(
                &mut s2,
                copy_c,
                copy_pid,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after CreatureCopierEnch copied Bear");
        };
        let bf2 = raw2.new_id.unwrap();
        assert_eq!(
            s2.rare(bf2).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(0)
        );
        let _ = s2.evaluate_board();

        // Execute (c1)
        let mut s3 = boot_2p(db.clone());
        put_on_battlefield(&mut s3, PlayerId(0), CardDefId(6));
        let tapped_cre_h = put_in_hand(&mut s3, PlayerId(0), CardDefId(5));
        let MoveObjectOutcome::Moved(raw3) = move_object(
            &mut s3,
            tapped_cre_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected synchronous MoveObjectOutcome::Moved for (c1)");
        };
        assert_eq!(raw3.to_zone, Zone::Exile);
        let ex3 = raw3.new_id.unwrap();
        assert_eq!(s3.counter_count(ex3, CounterId(0)), 1);
        assert!(!s3.object(ex3).unwrap().flags.contains(ObjectFlags::TAPPED));
        let _ = s3.evaluate_board();

        // Execute (c2)
        let mut s4 = boot_2p(db);
        put_on_battlefield(&mut s4, PlayerId(0), CardDefId(8));
        let gy_cre_bf = put_on_battlefield(&mut s4, PlayerId(0), CardDefId(7));
        let MoveObjectOutcome::Moved(raw4) = move_object(
            &mut s4,
            gy_cre_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected synchronous MoveObjectOutcome::Moved for (c2)");
        };
        assert_eq!(raw4.to_zone, Zone::Library);
        let lib_id = raw4.new_id.unwrap();
        assert!(s4.players()[0].library.contains(&lib_id));
        assert!(!s4.players()[0].graveyard.contains(&lib_id));
        let _ = s4.evaluate_board();

        // (d) etb_steal_done leak + etb_answered_steps[502] across synchronous ZoneChangeMods
        let kw_ab = |kw: KeywordId| {
            AbilityDef::Keyword(mtg_ir::KeywordAbility {
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
            })
        };
        let self_discard_502 = make_card(
            "SelfDiscardStep502",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_discard_for_keyword_counters: smallvec::smallvec![KeywordId(0)],
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let stealer_plain = make_card(
            "StealerPlain",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enter_under_opponent_control: true,
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let grant_discard_ench = make_card(
            "GrantDiscardEnch",
            CardTypeMask::ENCHANTMENT,
            vec![rep_ability(ReplacementDef {
                replaces: zc_creature_to(Zone::Battlefield),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_discard_for_keyword_counters: smallvec::smallvec![KeywordId(0)],
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );
        let db_d = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![],
                    counters: vec!["+1/+1".into(), "flying".into()],
                    keywords: vec!["Flying".into()],
                },
                cards: vec![
                    make_card("FillerLand", CardTypeMask::LAND, vec![]), // CardDefId(0)
                    make_card("BearD", CardTypeMask::CREATURE, vec![]),  // CardDefId(1)
                    make_card("FlyerD", CardTypeMask::CREATURE, vec![kw_ab(KeywordId(0))]), // CardDefId(2)
                    self_discard_502,   // CardDefId(3)
                    stealer_plain,      // CardDefId(4)
                    grant_discard_ench, // CardDefId(5)
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // Part (d1): two StealerPlain enchantments (P0 -> P1 -> P0)
        let mut s5 = boot_2p(db_d.clone());
        put_on_battlefield(&mut s5, PlayerId(0), CardDefId(4));
        put_on_battlefield(&mut s5, PlayerId(1), CardDefId(4));
        let bear5_h = put_in_hand(&mut s5, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s5,
            bear5_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected ChooseReplacement for two StealerPlain enchantments");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw5)) =
            resume_and_finalize_replacement(
                &mut s5,
                chooser,
                prompt_id,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved after both StealerPlain enchantments applied");
        };
        assert_eq!(
            s5.object(raw5.new_id.unwrap()).unwrap().controller,
            PlayerId(0)
        );
        let _ = s5.evaluate_board();

        // Part (d2): step_idx = 502 reset across Tier 1 -> Tier 2 -> Tier 5
        let mut s6 = boot_2p(db_d);
        put_on_battlefield(&mut s6, PlayerId(0), CardDefId(4)); // StealerPlain
        put_on_battlefield(&mut s6, PlayerId(0), CardDefId(5)); // GrantDiscardEnch
        let flyer_h = put_in_hand(&mut s6, PlayerId(1), CardDefId(2));
        let self_disc_h = put_in_hand(&mut s6, PlayerId(0), CardDefId(3));
        crate::events::begin_action(&mut s6);
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s6,
            self_disc_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected DiscardForKeywordCounters pause from Tier 5 GrantDiscardEnch");
        };
        assert_eq!(chooser, PlayerId(1));
        assert!(matches!(
            s6.pending_replacement().unwrap().prompt_kind,
            PendingReplacementPrompt::EtbChoice {
                step: EtbInteractiveStep::DiscardForKeywordCounters { .. },
                ..
            }
        ));
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw6)) =
            resume_and_finalize_replacement(
                &mut s6,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: flyer_h },
            )
            .unwrap()
        else {
            panic!("expected Moved after P1 discarded FlyerD");
        };
        let bf6 = raw6.new_id.unwrap();
        assert_eq!(s6.object(bf6).unwrap().controller, PlayerId(1));
        assert_eq!(s6.counter_count(bf6, CounterId(1)), 1); // flying counter
        crate::events::end_action(&mut s6);
        let _ = s6.evaluate_board();
    }

    #[test]
    fn clone_at_index_zero_copies_etb_replacement_at_index_zero() {
        let clone_card = make_card(
            "Idx0Clone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let second_clone_card = make_card(
            "Idx0SecondClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let hydra_idx0 = make_card(
            "HydraAtIdx0",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 3 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let tap_idx0 = make_card(
            "TapCreatureAtIdx0",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let choice_idx0 = make_card(
            "ChoiceCreatureAtIdx0",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![EtbChoiceSpec::Color { exclude: None }],
                        tapped: Some(true),
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 2 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let mutate_top = make_card(
            "MutateTopAtIdx0",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let mutate_bottom = make_card(
            "MutateBottomAtIdx0",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 2 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let split_ench_pattern = EventPattern {
            event: EventKind::ZoneChange,
            filter: EventFilter {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                }),
                detail: Some(EventDetail::ZoneChange {
                    from: ZoneMask::empty(),
                    to: Some(Zone::Battlefield),
                    from_anywhere: false,
                    not_from_zone: None,
                    not_to_zone: None,
                    enters_untapped: false,
                    dealt_damage_by_self_this_turn: false,
                }),
                ..Default::default()
            },
        };
        let dual_face_ench = CardDef {
            oracle_id: "dual_face_idx0_ench".into(),
            compiler_version: 46,
            text_hash: "dual_face_idx0_ench".into(),
            name: "LeftDoor // RightDoor".into(),
            layout: CardLayout::Split,
            faces: vec![
                mtg_ir::FaceDef {
                    name: "LeftDoor".into(),
                    characteristics: mtg_ir::Characteristics {
                        mana_cost: ManaCost::parse("{1}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: Default::default(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![rep_ability(ReplacementDef {
                        replaces: split_ench_pattern.clone(),
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
                mtg_ir::FaceDef {
                    name: "RightDoor".into(),
                    characteristics: mtg_ir::Characteristics {
                        mana_cost: ManaCost::parse("{1}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: Default::default(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![rep_ability(ReplacementDef {
                        replaces: split_ench_pattern,
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                tapped: Some(true),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
            ],
            fully_supported: true,
        };
        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]), // CardDefId(0)
            clone_card,                                        // CardDefId(1)
            hydra_idx0,                                        // CardDefId(2)
            tap_idx0,                                          // CardDefId(3)
            choice_idx0,                                       // CardDefId(4)
            second_clone_card,                                 // CardDefId(5)
            mutate_top,                                        // CardDefId(6)
            mutate_bottom,                                     // CardDefId(7)
            dual_face_ench,                                    // CardDefId(8)
        ]);

        // (a) counters at abilities[0]
        let mut s1 = boot_2p(db.clone());
        let hydra_bf = put_on_battlefield(&mut s1, PlayerId(0), CardDefId(2));
        let clone_hand1 = put_in_hand(&mut s1, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s1,
            clone_hand1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for hydra");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw1)) =
            resume_and_finalize_replacement(
                &mut s1,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: hydra_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after copying HydraAtIdx0");
        };
        let bf1 = raw1.new_id.unwrap();
        assert_eq!(
            s1.rare(bf1).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(2)
        );
        assert_eq!(s1.counter_count(bf1, CounterId(0)), 3);
        let _ = s1.evaluate_board();

        // (b) tapped at abilities[0]
        let mut s2 = boot_2p(db.clone());
        let tap_bf = put_on_battlefield(&mut s2, PlayerId(0), CardDefId(3));
        let clone_hand2 = put_in_hand(&mut s2, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s2,
            clone_hand2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for tap creature");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw2)) =
            resume_and_finalize_replacement(
                &mut s2,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: tap_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after copying TapCreatureAtIdx0");
        };
        let bf2 = raw2.new_id.unwrap();
        assert!(s2.object(bf2).unwrap().flags.contains(ObjectFlags::TAPPED));
        let _ = s2.evaluate_board();

        // (c) etb_choices (+ tapped + counters) at abilities[0]
        let mut s3 = boot_2p(db.clone());
        let choice_bf = put_on_battlefield(&mut s3, PlayerId(0), CardDefId(4));
        let clone_hand3 = put_in_hand(&mut s3, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s3,
            clone_hand3,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for choice creature");
        };
        let ReplacementResumeOutcome::Paused {
            prompt_id: p_col,
            chooser: c_col,
        } = resume_and_finalize_replacement(
            &mut s3,
            chooser,
            prompt_id,
            ChoiceAnswer::PickObject { id: choice_bf },
        )
        .unwrap()
        else {
            panic!("expected EtbChoice Color pause from copied ChoiceCreatureAtIdx0");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw3)) =
            resume_and_finalize_replacement(
                &mut s3,
                c_col,
                p_col,
                ChoiceAnswer::PickMode { index: 1 }, // Blue
            )
            .unwrap()
        else {
            panic!("expected Moved after copied EtbChoice");
        };
        let bf3 = raw3.new_id.unwrap();
        assert_eq!(s3.rare(bf3).unwrap().chosen_color, Some(ColorMask::U));
        assert!(s3.object(bf3).unwrap().flags.contains(ObjectFlags::TAPPED));
        assert_eq!(s3.counter_count(bf3, CounterId(0)), 2);
        let _ = s3.evaluate_board();

        // (d) Clone copying an uncopied Clone of the SAME CardDefId terminates (CR 614.5)
        let mut s4 = boot_2p(db.clone());
        let uncopied_same_clone = put_on_battlefield(&mut s4, PlayerId(0), CardDefId(1));
        let clone_hand4 = put_in_hand(&mut s4, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s4,
            clone_hand4,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for same-def uncopied clone");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw4)) =
            resume_and_finalize_replacement(
                &mut s4,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject {
                    id: uncopied_same_clone,
                },
            )
            .unwrap()
        else {
            panic!("expected immediate Moved without infinite same-CardDefId EtbCopy loop");
        };
        let bf4 = raw4.new_id.unwrap();
        assert_eq!(
            s4.rare(bf4).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(1)
        );
        let _ = s4.evaluate_board();

        // (e) CloneA copying an uncopied CloneB (different CardDefId) chains to HydraAtIdx0
        let mut s5 = boot_2p(db.clone());
        let uncopied_other_clone = put_on_battlefield(&mut s5, PlayerId(0), CardDefId(5));
        let hydra5_bf = put_on_battlefield(&mut s5, PlayerId(0), CardDefId(2));
        let clone_hand5 = put_in_hand(&mut s5, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s5,
            clone_hand5,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected first EtbCopy pause");
        };
        let ReplacementResumeOutcome::Paused {
            prompt_id: p_chain,
            chooser: c_chain,
        } = resume_and_finalize_replacement(
            &mut s5,
            chooser,
            prompt_id,
            ChoiceAnswer::PickObject {
                id: uncopied_other_clone,
            },
        )
        .unwrap()
        else {
            panic!("expected chained EtbCopy pause from CloneB");
        };
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw5)) =
            resume_and_finalize_replacement(
                &mut s5,
                c_chain,
                p_chain,
                ChoiceAnswer::PickObject { id: hydra5_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after chained EtbCopy to HydraAtIdx0");
        };
        let bf5 = raw5.new_id.unwrap();
        assert_eq!(
            s5.rare(bf5).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(2)
        );
        assert_eq!(s5.counter_count(bf5, CounterId(0)), 3);
        let _ = s5.evaluate_board();

        // (f) Mutate merged_components: Clone copies a Mutate pile where both top and bottom
        // components have ETB replacements at abilities[0]; both must apply (1 + 2 = 3 counters).
        let mut s6 = boot_2p(db.clone());
        let mutate_bf = put_on_battlefield(&mut s6, PlayerId(0), CardDefId(6));
        s6.rare_or_default(mutate_bf).unwrap().merged_components = vec![
            crate::state::MergedComponent {
                card_def: CardDefId(6),
                is_token: false,
            },
            crate::state::MergedComponent {
                card_def: CardDefId(7),
                is_token: false,
            },
        ];
        s6.mark_dirty(DirtyMask::TOPOLOGY);
        let clone_hand6 = put_in_hand(&mut s6, PlayerId(0), CardDefId(1));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s6,
            clone_hand6,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for Mutate pile");
        };
        let ReplacementResumeOutcome::Paused {
            prompt_id: p_mut,
            chooser: c_mut,
        } = resume_and_finalize_replacement(
            &mut s6,
            chooser,
            prompt_id,
            ChoiceAnswer::PickObject { id: mutate_bf },
        )
        .unwrap()
        else {
            panic!("expected ChooseReplacement across MutateTop and MutateBottom abilities[0]");
        };
        assert_eq!(s6.pending_replacement().unwrap().candidates.len(), 2);
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw6)) =
            resume_and_finalize_replacement(
                &mut s6,
                c_mut,
                p_mut,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved after both Mutate component replacements applied");
        };
        let bf6 = raw6.new_id.unwrap();
        assert_eq!(s6.counter_count(bf6, CounterId(0)), 3);
        let _ = s6.evaluate_board();

        // (g) Multi-face (Split/Room) permanent with both faces unlocked and abilities[0] on each
        // face: both face_index 0 and face_index 1 replacements must be distinct and both apply.
        let mut s7 = boot_2p(db);
        let dual_bf = put_on_battlefield(&mut s7, PlayerId(0), CardDefId(8));
        s7.rare_or_default(dual_bf).unwrap().unlocked_faces = smallvec::smallvec![0, 1];
        s7.mark_dirty(DirtyMask::TOPOLOGY);
        let bear_hand7 = put_in_hand(&mut s7, PlayerId(0), CardDefId(0));
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut s7,
            bear_hand7,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected ChooseReplacement across face 0 and face 1 abilities[0]");
        };
        let cands7 = &s7.pending_replacement().unwrap().candidates;
        assert_eq!(cands7.len(), 2);
        assert_ne!(cands7[0].face_index, cands7[1].face_index);
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw7)) =
            resume_and_finalize_replacement(
                &mut s7,
                chooser,
                prompt_id,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected Moved after both unlocked faces applied");
        };
        let bf7 = raw7.new_id.unwrap();
        assert_eq!(s7.counter_count(bf7, CounterId(0)), 1);
        assert!(s7.object(bf7).unwrap().flags.contains(ObjectFlags::TAPPED));
        let _ = s7.evaluate_board();
    }

    #[test]
    fn granted_replacement_g_idx_shift_across_etb_copy_does_not_drop_tier5_counter() {
        let dual_static_granter = make_card(
            "DualStaticGranter",
            CardTypeMask::ENCHANTMENT,
            vec![
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(rep_ability(ReplacementDef {
                                    replaces: zc_to(Zone::Battlefield, true),
                                    with: ReplacementWith::ZoneChange {
                                        mod_spec: Box::new(ZoneChangeMod {
                                            tapped: Some(true),
                                            ..Default::default()
                                        }),
                                    },
                                    self_replacement: true,
                                    who: None,
                                    condition: None,
                                })),
                            },
                            active_in_zone: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                }),
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE | CardTypeMask::ARTIFACT,
                                    controller: Some(RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(rep_ability(ReplacementDef {
                                    replaces: zc_to(Zone::Battlefield, false),
                                    with: ReplacementWith::ZoneChange {
                                        mod_spec: Box::new(ZoneChangeMod {
                                            counters: Some((
                                                CounterId(0),
                                                Box::new(Amount::Const { n: 1 }),
                                            )),
                                            ..Default::default()
                                        }),
                                    },
                                    self_replacement: false,
                                    who: None,
                                    condition: None,
                                })),
                            },
                            active_in_zone: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                }),
            ],
        );
        let synth_stone = make_card("SynthStone", CardTypeMask::ARTIFACT, vec![]);
        let perm_clone = make_card(
            "PermClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]), // CardDefId(0)
            dual_static_granter,                               // CardDefId(1)
            synth_stone,                                       // CardDefId(2)
            perm_clone,                                        // CardDefId(3)
        ]);
        let mut state = boot_2p(db);
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        // SynthStone controlled by PlayerId(1) so DualStaticGranter's `You` filter does not grant to it.
        let synth_stone_bf = put_on_battlefield(&mut state, PlayerId(1), CardDefId(2));
        let clone_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(3));

        // Tier-1 enters-tapped (abilities[0]) applies synchronously, then Tier-3 EtbCopy pauses.
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            clone_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected EtbCopy pause for PermClone");
        };

        // Copying non-creature Artifact SynthStone drops Creature-only Tier-1 grant, shifting
        // Tier-5 +1/+1 counter grant from g_idx 1 -> 0; CR 614.5 must NOT drop it.
        let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
            resume_and_finalize_replacement(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: synth_stone_bf },
            )
            .unwrap()
        else {
            panic!("expected Moved after PermClone copied SynthStone");
        };
        let bf = raw.new_id.unwrap();
        assert_eq!(
            state.rare(bf).unwrap().copiable.as_ref().unwrap().card_def,
            CardDefId(2)
        );
        assert!(
            state
                .object(bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert_eq!(state.counter_count(bf, CounterId(0)), 1);
        let cache = state.evaluate_board();
        let ev = cache.get(bf).unwrap();
        assert!(!ev.types.contains(CardTypeMask::CREATURE));
        assert!(ev.types.contains(CardTypeMask::ARTIFACT));
    }

    #[test]
    fn granted_replacement_same_card_def_distinct_grant_sources_do_not_collide() {
        let cond_granter = make_card(
            "CondGranter",
            CardTypeMask::ENCHANTMENT,
            vec![
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(rep_ability(ReplacementDef {
                                    replaces: zc_to(Zone::Battlefield, true),
                                    with: ReplacementWith::ZoneChange {
                                        mod_spec: Box::new(ZoneChangeMod {
                                            tapped: Some(true),
                                            ..Default::default()
                                        }),
                                    },
                                    self_replacement: true,
                                    who: None,
                                    condition: None,
                                })),
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(mtg_ir::Condition::Not {
                            of: Box::new(mtg_ir::Condition::SelfPrepared),
                        })),
                    },
                    span: Default::default(),
                }),
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::ARTIFACT,
                                    controller: Some(RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(rep_ability(ReplacementDef {
                                    replaces: zc_to(Zone::Battlefield, false),
                                    with: ReplacementWith::ZoneChange {
                                        mod_spec: Box::new(ZoneChangeMod {
                                            counters: Some((
                                                CounterId(0),
                                                Box::new(Amount::Const { n: 2 }),
                                            )),
                                            ..Default::default()
                                        }),
                                    },
                                    self_replacement: false,
                                    who: None,
                                    condition: None,
                                })),
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(mtg_ir::Condition::SelfPrepared)),
                    },
                    span: Default::default(),
                }),
            ],
        );
        let synth_stone_b = make_card("SynthStoneB", CardTypeMask::ARTIFACT, vec![]);
        let perm_clone_b = make_card(
            "PermCloneB",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]), // CardDefId(0)
            cond_granter,                                      // CardDefId(1)
            synth_stone_b,                                     // CardDefId(2)
            perm_clone_b,                                      // CardDefId(3)
        ]);

        // (a) Unprepared CondGranter (tapped) + Prepared CondGranter (+2 counters) across EtbCopy.
        {
            let mut s1 = boot_2p(db.clone());
            let _g_unprep = put_on_battlefield(&mut s1, PlayerId(0), CardDefId(1));
            let g_prep = put_on_battlefield(&mut s1, PlayerId(0), CardDefId(1));
            s1.object_mut(g_prep)
                .unwrap()
                .flags
                .insert(ObjectFlags::PREPARED);
            s1.mark_dirty(DirtyMask::DESIGNATIONS);

            let sol_bf = put_on_battlefield(&mut s1, PlayerId(1), CardDefId(2));
            let clone_hand = put_in_hand(&mut s1, PlayerId(0), CardDefId(3));

            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut s1,
                clone_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected EtbCopy pause for PermCloneB");
            };
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut s1,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickObject { id: sol_bf },
                )
                .unwrap()
            else {
                panic!("expected Moved after PermCloneB copied SynthStoneB");
            };
            let bf = raw.new_id.unwrap();
            assert!(s1.object(bf).unwrap().flags.contains(ObjectFlags::TAPPED));
            assert_eq!(s1.counter_count(bf, CounterId(0)), 2);
            let cache = s1.evaluate_board();
            let ev = cache.get(bf).unwrap();
            assert!(!ev.types.contains(CardTypeMask::CREATURE));
            assert!(ev.types.contains(CardTypeMask::ARTIFACT));
        }

        // (b) Two prepared CondGranters of the same CardDefId granting the identical ability_index
        //     must remain distinct by grant_source ObjectId -> +2 + +2 = 4 counters.
        {
            let mut s2 = boot_2p(db);
            for _ in 0..2 {
                let g = put_on_battlefield(&mut s2, PlayerId(0), CardDefId(1));
                s2.object_mut(g)
                    .unwrap()
                    .flags
                    .insert(ObjectFlags::PREPARED);
            }
            s2.mark_dirty(DirtyMask::DESIGNATIONS);

            let sol_bf = put_on_battlefield(&mut s2, PlayerId(1), CardDefId(2));
            let clone_hand = put_in_hand(&mut s2, PlayerId(0), CardDefId(3));

            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut s2,
                clone_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected EtbCopy pause");
            };
            let ReplacementResumeOutcome::Paused {
                prompt_id: p_rep,
                chooser: c_rep,
            } = resume_and_finalize_replacement(
                &mut s2,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: sol_bf },
            )
            .unwrap()
            else {
                panic!("expected ChooseReplacement across two distinct prepared CondGranters");
            };
            assert_eq!(s2.pending_replacement().unwrap().candidates.len(), 2);
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut s2,
                    c_rep,
                    p_rep,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected Moved after both CondGranter +2 counter grants applied");
            };
            let bf = raw.new_id.unwrap();
            assert_eq!(s2.counter_count(bf, CounterId(0)), 4);
            let _ = s2.evaluate_board();
        }
    }

    #[test]
    fn clone_mutate_pile_with_duplicate_card_def_components_applies_both() {
        let mut_top = make_card(
            "MutTopDup",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        let clone_card = make_card(
            "MutDupClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );

        let db = build_db(vec![
            make_card("Bear", CardTypeMask::CREATURE, vec![]), // CardDefId(0)
            mut_top,                                           // CardDefId(1)
            clone_card,                                        // CardDefId(2)
        ]);

        // (a) Cloning a Mutate pile [Bear, MutTop, MutTop] applies both duplicate MutTop components.
        {
            let mut state = boot_2p(db.clone());
            let mutate_bf = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            state.rare_or_default(mutate_bf).unwrap().merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(0),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
            ];
            state.mark_dirty(DirtyMask::TOPOLOGY);

            let clone_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                clone_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected EtbCopy pause for Mutate pile [Bear, MutTop, MutTop]");
            };

            let ReplacementResumeOutcome::Paused {
                prompt_id: p_mut,
                chooser: c_mut,
            } = resume_and_finalize_replacement(
                &mut state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickObject { id: mutate_bf },
            )
            .unwrap()
            else {
                panic!("expected ChooseReplacement across duplicate MutTop components");
            };
            assert_eq!(state.pending_replacement().unwrap().candidates.len(), 2);

            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    c_mut,
                    p_mut,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected clean termination after both MutTop replacements applied");
            };
            let bf = raw.new_id.unwrap();
            assert_eq!(state.counter_count(bf, CounterId(0)), 2);
            let _ = state.evaluate_board();
        }

        // (b) Uncopied re-entry of [Bear, MutTop, MutTop] applies each MutTop component once and stops.
        {
            let mut state = boot_2p(db.clone());
            let mutate_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
            state
                .rare_or_default(mutate_hand)
                .unwrap()
                .merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(0),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
            ];
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                mutate_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected ChooseReplacement for uncopied [Bear, MutTop, MutTop]");
            };
            assert_eq!(state.pending_replacement().unwrap().candidates.len(), 2);
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected clean Moved for uncopied [Bear, MutTop, MutTop]");
            };
            let bf = raw.new_id.unwrap();
            assert_eq!(state.counter_count(bf, CounterId(0)), 2);
            let _ = state.evaluate_board();
        }

        // (c) Clone copying another Clone of the same CardDefId terminates via CR 614.5 on comp 0.
        {
            let mut state = boot_2p(db);
            let other_clone = put_on_battlefield(&mut state, PlayerId(1), CardDefId(2));
            let clone_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                clone_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected EtbCopy pause for Clone -> Clone");
            };
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickObject { id: other_clone },
                )
                .unwrap()
            else {
                panic!("expected CR 614.5 termination when Clone copies Clone");
            };
            assert!(raw.new_id.is_some());
            let _ = state.evaluate_board();
        }
    }

    #[test]
    fn provenance_disambiguates_timed_effects_emblems_nested_statics_and_card_ability_donors() {
        use crate::layers::predicates::{ContinuousTarget, TimedContinuousEffect};

        let bear = make_card("Bear", CardTypeMask::CREATURE, vec![]); // CardDefId(0)
        let synth_stone = make_card("SynthStone", CardTypeMask::ARTIFACT, vec![]); // CardDefId(1)
        let perm_clone = make_card(
            "PermClone",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        ); // CardDefId(2)
        let emblem_card = make_card(
            "GranterEmblem",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(rep_ability(ReplacementDef {
                                replaces: zc_to(Zone::Battlefield, false),
                                with: ReplacementWith::ZoneChange {
                                    mod_spec: Box::new(ZoneChangeMod {
                                        counters: Some((
                                            CounterId(0),
                                            Box::new(Amount::Const { n: 1 }),
                                        )),
                                        ..Default::default()
                                    }),
                                },
                                self_replacement: false,
                                who: None,
                                condition: None,
                            })),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        ); // CardDefId(3)
        let donor_card = make_card(
            "DonorCreature",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        ); // CardDefId(4)
        let exile_granter = make_card(
            "ExileGranter",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbilitiesFromObjects {
                            from: mtg_ir::AbilityGrantSource::ExiledCardsWithCounter {
                                counter: CounterId(0),
                                owner: None,
                                filter: None,
                            },
                            mana_as_any_color: false,
                            once_each_turn: false,
                            loyalty_only: false,
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        ); // CardDefId(5)
        let l6_nested_granter = make_card(
            "L6NestedGranter",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(AbilityDef::Static(StaticAbility {
                                static_def: StaticDef {
                                    body: StaticBody::Continuous {
                                        layer: Layer::Layer6,
                                        affected: Affected::Filter {
                                            filter: Box::new(ObjectFilter {
                                                zones: ZoneMask::BATTLEFIELD,
                                                types_any: CardTypeMask::ARTIFACT,
                                                controller: Some(RelativeControl::You),
                                                ..Default::default()
                                            }),
                                        },
                                        modifier: LayerMod::GrantAbility {
                                            ability: Box::new(rep_ability(ReplacementDef {
                                                replaces: zc_to(Zone::Battlefield, false),
                                                with: ReplacementWith::ZoneChange {
                                                    mod_spec: Box::new(ZoneChangeMod {
                                                        counters: Some((
                                                            CounterId(0),
                                                            Box::new(Amount::Const { n: 1 }),
                                                        )),
                                                        ..Default::default()
                                                    }),
                                                },
                                                self_replacement: false,
                                                who: None,
                                                condition: None,
                                            })),
                                        },
                                        active_in_zone: None,
                                    },
                                    condition: None,
                                },
                                span: Default::default(),
                            })),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        ); // CardDefId(6)

        let db = build_db(vec![
            bear,
            synth_stone,
            perm_clone,
            emblem_card,
            donor_card,
            exile_granter,
            l6_nested_granter,
        ]);

        // (a) Two TimedContinuousEffects with source: None, identical timestamp (100), distinct id (1 vs 2)
        //     across EtbCopy + PendingReplacementState JSON round-trip.
        {
            let mut state = boot_2p(db.clone());
            state.add_continuous_effect(TimedContinuousEffect {
                id: 1,
                source: None,
                controller: PlayerId(0),
                layer: Layer::Layer6,
                affected: ContinuousTarget::Filter(ObjectFilter {
                    zones: ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE,
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                }),
                modifier: LayerMod::GrantAbility {
                    ability: Box::new(rep_ability(ReplacementDef {
                        replaces: zc_to(Zone::Battlefield, true),
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                tapped: Some(true),
                                ..Default::default()
                            }),
                        },
                        self_replacement: true,
                        who: None,
                        condition: None,
                    })),
                },
                duration: mtg_ir::Duration::EndOfTurn,
                timestamp: 100,
                expires_turn: None,
                expires_on_player: None,
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });
            state.add_continuous_effect(TimedContinuousEffect {
                id: 2,
                source: None,
                controller: PlayerId(0),
                layer: Layer::Layer6,
                affected: ContinuousTarget::Filter(ObjectFilter {
                    zones: ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE | CardTypeMask::ARTIFACT,
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                }),
                modifier: LayerMod::GrantAbility {
                    ability: Box::new(rep_ability(ReplacementDef {
                        replaces: zc_to(Zone::Battlefield, false),
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    })),
                },
                duration: mtg_ir::Duration::EndOfTurn,
                timestamp: 100,
                expires_turn: None,
                expires_on_player: None,
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });

            let sol_bf = put_on_battlefield(&mut state, PlayerId(1), CardDefId(1));
            let clone_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                clone_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected EtbCopy pause under TimedContinuousEffect");
            };
            // Verify PendingReplacementState with expanded ReplacementSourceId serializes/deserializes cleanly.
            let pending_json = serde_json::to_string(state.pending_replacement().unwrap()).unwrap();
            let pending_back: PendingReplacementState =
                serde_json::from_str(&pending_json).unwrap();
            assert_eq!(
                &pending_back,
                state.pending_replacement().unwrap(),
                "PendingReplacementState must round-trip cleanly"
            );

            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickObject { id: sol_bf },
                )
                .unwrap()
            else {
                panic!("expected Moved with both TimedContinuousEffects applied");
            };
            let bf = raw.new_id.unwrap();
            assert!(
                state
                    .object(bf)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.counter_count(bf, CounterId(0)), 1);
            let _ = state.evaluate_board();
        }

        // (b) Two Emblems of the same CardDefId in Zone::Command remain distinct by grant_source.
        {
            let mut state = boot_2p(db.clone());
            for _ in 0..2 {
                let ts = state.allocate_timestamp();
                let mut emb = crate::state::GameObject::new(
                    CardDefId(3),
                    PlayerId(0),
                    Zone::Command,
                    ts,
                    ObjectFlags::IS_EMBLEM,
                );
                emb.entered_timestamp = ts;
                let id = state.insert_object(emb, None);
                state.player_mut(PlayerId(0)).unwrap().command.push(id);
            }
            state.mark_dirty(DirtyMask::TOPOLOGY);

            let bear_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                bear_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected ChooseReplacement across two Emblems of same CardDefId");
            };
            assert_eq!(state.pending_replacement().unwrap().candidates.len(), 2);
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected Moved after both Emblem grants applied");
            };
            assert_eq!(state.counter_count(raw.new_id.unwrap(), CounterId(0)), 2);
            let _ = state.evaluate_board();
        }

        // (c) GrantAbilitiesFromObjects (CardAbility): two granters of same CardDefId + two exiled
        //     donors of same CardDefId -> 2 * 2 = 4 distinct candidates -> 4 counters.
        {
            let mut state = boot_2p(db.clone());
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            for _ in 0..2 {
                let ts = state.allocate_timestamp();
                let obj = crate::state::GameObject::new(
                    CardDefId(4),
                    PlayerId(0),
                    Zone::Exile,
                    ts,
                    ObjectFlags::empty(),
                );
                let eid = state.insert_object(obj, None);
                state.player_mut(PlayerId(0)).unwrap().exile.push(eid);
                state.set_counter(eid, CounterId(0), 1);
            }
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE | DirtyMask::COUNTERS);

            let bear_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
            let mut outcome = move_object(
                &mut state,
                bear_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            let mut first_cand_count = 0usize;
            let bf = loop {
                match outcome {
                    MoveObjectOutcome::Moved(raw) => break raw.new_id.unwrap(),
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        if first_cand_count == 0 {
                            first_cand_count =
                                state.pending_replacement().unwrap().candidates.len();
                        }
                        match resume_and_finalize_replacement(
                            &mut state,
                            chooser,
                            prompt_id,
                            ChoiceAnswer::PickReplacement { index: 0 },
                        )
                        .unwrap()
                        {
                            ReplacementResumeOutcome::ZoneMove(m) => outcome = m,
                            ReplacementResumeOutcome::Paused { prompt_id, chooser } => {
                                outcome = MoveObjectOutcome::Paused { prompt_id, chooser };
                            }
                            other => panic!("unexpected ReplacementResumeOutcome: {other:?}"),
                        }
                    }
                    other => panic!("unexpected MoveObjectOutcome: {other:?}"),
                }
            };
            assert_eq!(first_cand_count, 4);
            assert_eq!(state.counter_count(bf, CounterId(0)), 4);
            let _ = state.evaluate_board();
        }

        // (d) NestedStatic (A -> B -> C): two Layer-5 TimedContinuousEffects (id 10 vs 11) grant a
        //     Layer-6 GrantAbility(Rep) static onto host Bear B, which grants Rep onto entering C,
        //     with NO dummy Layer-6 permanent on the battlefield (4c-1).
        {
            let mut state = boot_2p(db.clone());
            let host_b = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            for eff_id in [10u64, 11u64] {
                state.add_continuous_effect(TimedContinuousEffect {
                    id: eff_id,
                    source: None,
                    controller: PlayerId(0),
                    layer: Layer::Layer5,
                    affected: ContinuousTarget::Objects(smallvec::smallvec![host_b]),
                    modifier: LayerMod::GrantAbility {
                        ability: Box::new(AbilityDef::Static(StaticAbility {
                            static_def: StaticDef {
                                body: StaticBody::Continuous {
                                    layer: Layer::Layer6,
                                    affected: Affected::Filter {
                                        filter: Box::new(ObjectFilter {
                                            zones: ZoneMask::BATTLEFIELD,
                                            types_any: CardTypeMask::ARTIFACT,
                                            controller: Some(RelativeControl::You),
                                            ..Default::default()
                                        }),
                                    },
                                    modifier: LayerMod::GrantAbility {
                                        ability: Box::new(rep_ability(ReplacementDef {
                                            replaces: zc_to(Zone::Battlefield, false),
                                            with: ReplacementWith::ZoneChange {
                                                mod_spec: Box::new(ZoneChangeMod {
                                                    counters: Some((
                                                        CounterId(0),
                                                        Box::new(Amount::Const { n: 1 }),
                                                    )),
                                                    ..Default::default()
                                                }),
                                            },
                                            self_replacement: false,
                                            who: None,
                                            condition: None,
                                        })),
                                    },
                                    active_in_zone: None,
                                },
                                condition: None,
                            },
                            span: Default::default(),
                        })),
                    },
                    duration: mtg_ir::Duration::EndOfTurn,
                    timestamp: 200,
                    expires_turn: None,
                    expires_on_player: None,
                    chosen_color: None,
                    chosen_subtype: None,
                    chosen_player: None,
                    chosen_card_type: None,
                    locked_pt: None,
                    is_cda: false,
                });
            }

            let rock_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                rock_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected ChooseReplacement across two NestedStatic grants on host B");
            };
            assert_eq!(state.pending_replacement().unwrap().candidates.len(), 2);
            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected Moved after both NestedStatic grants applied");
            };
            assert_eq!(state.counter_count(raw.new_id.unwrap(), CounterId(0)), 2);
            let _ = state.evaluate_board();
        }

        // (e) L6 -> L6 -> Rep (4c-3): innate Layer-6 enchantment grants a Layer-6 GrantAbility(Rep)
        //     static onto host Bear B, which applies +1/+1 counter onto entering Artifact C.
        {
            let mut state = boot_2p(db);
            let _granter_a = put_on_battlefield(&mut state, PlayerId(0), CardDefId(6));
            let _host_b = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));

            let rock_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
            let MoveObjectOutcome::Moved(raw) = move_object(
                &mut state,
                rock_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected single L6->L6->Rep replacement to apply directly");
            };
            assert_eq!(state.counter_count(raw.new_id.unwrap(), CounterId(0)), 1);
            let _ = state.evaluate_board();
        }
    }

    #[test]
    fn single_granter_multiple_grant_abilities_from_objects_statics_do_not_collide() {
        let bear = make_card("Bear", CardTypeMask::CREATURE, vec![]); // CardDefId(0)
        let donor = make_card(
            "DonorCreature",
            CardTypeMask::CREATURE,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((CounterId(0), Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        ); // CardDefId(1)

        let grant_exiled_with_self = || {
            AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                extra: vec![ObjectPredicate::NotSelf],
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbilitiesFromObjects {
                            from: mtg_ir::AbilityGrantSource::ExiledWithSelf {
                                filter: Box::new(ObjectFilter::default()),
                            },
                            mana_as_any_color: false,
                            once_each_turn: false,
                            loyalty_only: false,
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })
        };

        // Two innate GrantAbilitiesFromObjects statics (abilities[0] and abilities[1]) on one card.
        let double_granter = make_card(
            "DoubleExileGranter",
            CardTypeMask::ENCHANTMENT,
            vec![grant_exiled_with_self(), grant_exiled_with_self()],
        ); // CardDefId(2)

        // Single GrantAbilitiesFromObjects static used as duplicate Mutate components (comp 1 vs 2).
        let single_mut_granter = make_card(
            "MutExileGranter",
            CardTypeMask::CREATURE,
            vec![grant_exiled_with_self()],
        ); // CardDefId(3)

        let db = build_db(vec![bear, donor, double_granter, single_mut_granter]);

        // (a) Bug 4d-2: One DoubleExileGranter (abilities[0] + abilities[1]) x one exiled Donor
        //     must emit 2 distinct ReplacementSourceIds and apply +1/+1 twice (counters == 2).
        {
            let mut state = boot_2p(db.clone());
            let granter_bf = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let ts = state.allocate_timestamp();
            let obj = crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Exile,
                ts,
                ObjectFlags::empty(),
            );
            let donor_exile = state.insert_object(obj, None);
            state
                .player_mut(PlayerId(0))
                .unwrap()
                .exile
                .push(donor_exile);
            state.rare_or_default(donor_exile).unwrap().exiled_by = Some(granter_bf);
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE);

            let bear_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                bear_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected ChooseReplacement across DoubleExileGranter abilities[0] and [1]");
            };
            let cands = &state.pending_replacement().unwrap().candidates;
            assert_eq!(cands.len(), 2);
            assert_ne!(cands[0], cands[1]);

            let pending_json = serde_json::to_string(state.pending_replacement().unwrap()).unwrap();
            let pending_back: PendingReplacementState =
                serde_json::from_str(&pending_json).unwrap();
            assert_eq!(&pending_back, state.pending_replacement().unwrap());

            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected Moved after both granted abilities applied");
            };
            assert_eq!(state.counter_count(raw.new_id.unwrap(), CounterId(0)), 2);
            let _ = state.evaluate_board();
        }

        // (b) Duplicate Mutate components [Bear, MutExileGranter, MutExileGranter] on one granter
        //     must also remain distinct by granter_component_index (1 vs 2 -> counters == 2).
        {
            let mut state = boot_2p(db);
            let mut_granter_bf = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            state
                .rare_or_default(mut_granter_bf)
                .unwrap()
                .merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(0),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(3),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(3),
                    is_token: false,
                },
            ];
            let ts = state.allocate_timestamp();
            let obj = crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Exile,
                ts,
                ObjectFlags::empty(),
            );
            let donor_exile = state.insert_object(obj, None);
            state
                .player_mut(PlayerId(0))
                .unwrap()
                .exile
                .push(donor_exile);
            state.rare_or_default(donor_exile).unwrap().exiled_by = Some(mut_granter_bf);
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE);

            let bear_hand = put_in_hand(&mut state, PlayerId(0), CardDefId(0));
            let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
                &mut state,
                bear_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected ChooseReplacement across Mutate granter components 1 and 2");
            };
            let cands = &state.pending_replacement().unwrap().candidates;
            assert_eq!(cands.len(), 2);
            assert_ne!(cands[0], cands[1]);

            let ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
                resume_and_finalize_replacement(
                    &mut state,
                    chooser,
                    prompt_id,
                    ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap()
            else {
                panic!("expected Moved after both Mutate granter components applied");
            };
            assert_eq!(state.counter_count(raw.new_id.unwrap(), CounterId(0)), 2);
            let _ = state.evaluate_board();
        }
    }

    #[test]
    fn synth_cr702_16e_non_combat_player_protection_and_unpreventable_bypass() {
        let mut tables = InternTables::default();
        tables.keywords.push("protection".into());
        let prot_kw = KeywordId(0);

        let prot_red_bear = CardDef {
            oracle_id: "synth_prot_red_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Prot Red Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![mtg_ir::FaceDef {
                name: "Synth Prot Red Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{W}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: mtg_ir::KeywordDef {
                        keyword: prot_kw,
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
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let red_pumper = CardDef {
            oracle_id: "synth_red_source".into(),
            compiler_version: 46,
            text_hash: "1".into(),
            name: "Synth Red Source".into(),
            layout: CardLayout::Normal,
            faces: vec![mtg_ir::FaceDef {
                name: "Synth Red Source".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
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
        let player_prot_creatures = make_card(
            "Synth Emissary",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::RuleModifier {
                        who: PlayerFilter {
                            relation: PlayerRelation::You,
                        },
                        rule: mtg_ir::RuleMod::PlayerProtectionFromChosenCardType,
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
                cards: vec![prot_red_bear, red_pumper, player_prot_creatures],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = boot_2p(db);
        let prot_bear = put_on_battlefield(&mut state, PlayerId(1), CardDefId(0));
        let red_src = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

        // 1. Non-combat damage from red source to protection-from-red creature is prevented (amount = 0, skipped = true).
        let ReplacementResult::Resolved(out1) = resolve_damage(
            &mut state,
            Some(red_src),
            TargetRef::Object { id: prot_bear },
            3,
            false,
            false,
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(out1.mods.skipped);
        assert_eq!(out1.mods.prevented_damage, 3);
        assert!(matches!(
            out1.event,
            ReplaceableEventKind::Damage { amount: 0, .. }
        ));

        // 2. Unpreventable bypass (effect_unpreventable = true or damage_cant_be_prevented_this_turn): deals 3!
        let ReplacementResult::Resolved(out2) = resolve_damage(
            &mut state,
            Some(red_src),
            TargetRef::Object { id: prot_bear },
            3,
            false,
            true,
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(!out2.mods.skipped);
        assert_eq!(out2.mods.prevented_damage, 0);
        assert!(matches!(
            out2.event,
            ReplaceableEventKind::Damage { amount: 3, .. }
        ));

        // 3. Player protection from chosen card type (Creature) prevents non-combat creature damage.
        let emissary = put_on_battlefield(&mut state, PlayerId(1), CardDefId(2));
        state.rare_or_default(emissary).unwrap().chosen_card_type = Some(CardTypeMask::CREATURE);
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        let ReplacementResult::Resolved(out3) = resolve_damage(
            &mut state,
            Some(red_src),
            TargetRef::Player { id: PlayerId(1) },
            4,
            false,
            false,
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(out3.mods.skipped);
        assert_eq!(out3.mods.prevented_damage, 4);

        // 4. Player protection_from_everything prevents non-combat damage unless unpreventable.
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .protection_from_everything = Some(mtg_ir::Duration::UntilYourNextTurn);
        state.mark_dirty(DirtyMask::PLAYER_STATE);
        let ReplacementResult::Resolved(out4) = resolve_damage(
            &mut state,
            Some(red_src),
            TargetRef::Player { id: PlayerId(0) },
            5,
            false,
            false,
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(out4.mods.skipped);
        assert_eq!(out4.mods.prevented_damage, 5);

        state.set_damage_cant_be_prevented_this_turn(false);
        let ReplacementResult::Resolved(out5) = resolve_damage(
            &mut state,
            Some(red_src),
            TargetRef::Player { id: PlayerId(0) },
            5,
            false,
            false,
        )
        .unwrap() else {
            panic!("expected Resolved");
        };
        assert!(!out5.mods.skipped);
        assert_eq!(out5.mods.prevented_damage, 0);
        assert!(matches!(
            out5.event,
            ReplaceableEventKind::Damage { amount: 5, .. }
        ));
    }

    #[test]
    fn synth_x_etb_counters_cast_vs_uncast_and_copy_cr_107_3m() {
        use crate::action::{ChoiceAnswer, CostAidKind};
        use crate::casting::{apply_casting_action, begin_cast_spell};
        use crate::choices::apply_choice_answer;
        use crate::sba::check_sbas;
        use crate::session::MatchSession;
        use crate::stack::resolve_top_of_stack;
        use mtg_ir::{
            ActivatedAbility, CostDef, EffectOp, EffectStep, KeywordAbility, KeywordDef,
            ManaProduction, MultiplierTarget, Timing,
        };
        use smallvec::smallvec;

        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let cid_p1 = CounterId(0);
        tables.keywords.push("convoke".into());
        let kw_convoke = KeywordId(0);

        let make_synth_card = |oracle_id: &str,
                               name: &str,
                               mana_cost: &str,
                               types: CardTypeMask,
                               colors: ColorMask,
                               pt: Option<(i32, i32)>,
                               abilities: Vec<AbilityDef>| CardDef {
            oracle_id: oracle_id.into(),
            compiler_version: 46,
            text_hash: "1".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![mtg_ir::FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse(mana_cost),
                    types,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors,
                    color_indicator: None,
                    pt: pt.map(|(power, toughness)| PrintedPt::Fixed { power, toughness }),
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
        };

        let make_convoke_kw = || {
            AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: kw_convoke,
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
            })
        };

        // 0: Synth Vanilla Bear ({1}{G}, 2/2)
        let synth_bear = make_synth_card(
            "synth_r_bear",
            "Synth Bear",
            "{1}{G}",
            CardTypeMask::CREATURE,
            ColorMask::G,
            Some((2, 2)),
            vec![],
        );
        // 1: Synth Plains Land ({T}: Add {W})
        let synth_plains = make_synth_card(
            "synth_r_plains",
            "Synth Plains",
            "",
            CardTypeMask::LAND,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Activated(ActivatedAbility {
                id: "mana_w".into(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![EffectStep {
                    op: EffectOp::AddMana {
                        production: ManaProduction::Fixed {
                            symbols: smallvec![ManaColor::W],
                        },
                        restriction: None,
                        restriction_spec: None,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    subject: mtg_ir::Selector::None,
                    actor: None,
                    amount: None,
                    optional: None,
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span: Default::default(),
                }],
                modal: None,
                timing: Timing::Mana,
                is_mana: true,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: ZoneMask::BATTLEFIELD,
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Default::default(),
            })],
        );
        // 2: Synth Green Sprite ({0}, 1/1 Green Creature)
        let synth_green = make_synth_card(
            "synth_r_green",
            "Synth Green Sprite",
            "",
            CardTypeMask::CREATURE,
            ColorMask::G,
            Some((1, 1)),
            vec![],
        );
        // 3: Synth Red Sprite ({0}, 1/1 Red Creature)
        let synth_red = make_synth_card(
            "synth_r_red",
            "Synth Red Sprite",
            "",
            CardTypeMask::CREATURE,
            ColorMask::R,
            Some((1, 1)),
            vec![],
        );
        // 4: Synth X Colossus ({X}{G}{W}, 0/0 Convoke + enters with X +1/+1 counters)
        let synth_x_colossus = make_synth_card(
            "synth_r_colossus",
            "Synth X Colossus",
            "{X}{G}{W}",
            CardTypeMask::CREATURE,
            ColorMask::G | ColorMask::W,
            Some((0, 0)),
            vec![
                make_convoke_kw(),
                rep_ability(ReplacementDef {
                    replaces: zc_to(Zone::Battlefield, true),
                    with: ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            counters: Some((cid_p1, Box::new(Amount::X))),
                            ..Default::default()
                        }),
                    },
                    self_replacement: true,
                    who: None,
                    condition: None,
                }),
            ],
        );
        // 5: Synth X2 Hydra ({X}{G}, 0/0 enters with 2*X +1/+1 counters)
        let synth_x2_hydra = make_synth_card(
            "synth_r_hydra",
            "Synth X2 Hydra",
            "{X}{G}",
            CardTypeMask::CREATURE,
            ColorMask::G,
            Some((0, 0)),
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((
                            cid_p1,
                            Box::new(Amount::Mul {
                                factor: 2,
                                of: Box::new(Amount::X),
                            }),
                        )),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        // 6: Synth Counter Doubler (Enchantment, doubles counters placed on your permanents)
        let synth_doubler = make_synth_card(
            "synth_r_doubler",
            "Synth Counter Doubler",
            "{2}{G}",
            CardTypeMask::ENCHANTMENT,
            ColorMask::G,
            None,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: MultiplierTarget::Counters,
                        factor: 2.0,
                        delta: 0,
                        filter: Some(Box::new(ObjectFilter {
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        })),
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // 7: Synth Ego Clone ({X}{2}{G}{U}, 0/0 EtbCopy with additional X +1/+1 counters)
        let synth_ego_clone = make_synth_card(
            "synth_r_ego",
            "Synth Ego Clone",
            "{X}{2}{G}{U}",
            CardTypeMask::CREATURE,
            ColorMask::G | ColorMask::U,
            Some((0, 0)),
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: true,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: Some(Box::new(CopyModificationSpec {
                                additional_counters: Some((cid_p1, Box::new(Amount::X))),
                                ..Default::default()
                            })),
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        // 8: Synth Plain Clone ({3}{U}, 0/0 non-{X} EtbCopy)
        let synth_plain_clone = make_synth_card(
            "synth_r_plain_clone",
            "Synth Plain Clone",
            "{3}{U}",
            CardTypeMask::CREATURE,
            ColorMask::U,
            Some((0, 0)),
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, true),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(mtg_ir::EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: true,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            })],
        );
        // 9: Synth BF Exiler (redirects entering Battlefield from Stack to Exile)
        let synth_bf_exiler = make_synth_card(
            "synth_r_bf_exiler",
            "Synth BF Exiler",
            "{2}{W}",
            CardTypeMask::ENCHANTMENT,
            ColorMask::W,
            None,
            vec![rep_ability(ReplacementDef {
                replaces: zc_to(Zone::Battlefield, false),
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        to: Some(Zone::Exile),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    synth_bear,
                    synth_plains,
                    synth_green,
                    synth_red,
                    synth_x_colossus,
                    synth_x2_hydra,
                    synth_doubler,
                    synth_ego_clone,
                    synth_plain_clone,
                    synth_bf_exiler,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // Case A: Full MatchSession Convoke cast of Synth X Colossus with X = 1, resolving
        // Stack -> Battlefield, surviving SBA as a 1/1, and matching snapshot/replay.
        {
            let p0_deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![
                    CardDefId(1), // Synth Plains
                    CardDefId(2), // Synth Green Sprite
                    CardDefId(3), // Synth Red Sprite
                    CardDefId(4), // Synth X Colossus
                    CardDefId(0),
                    CardDefId(0),
                    CardDefId(0),
                ],
                sideboard: vec![],
            };
            let p1_deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 10],
                sideboard: vec![],
            };
            let mut session = MatchSession::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                vec![p0_deck, p1_deck],
                99,
            )
            .unwrap();
            session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
            session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

            let find_hand = |s: &MatchSession, def: CardDefId| -> ObjectId {
                s.state().players()[0]
                    .hand
                    .iter()
                    .copied()
                    .find(|&id| s.state().object(id).unwrap().card_def == def)
                    .unwrap()
            };
            let plains_h = find_hand(&session, CardDefId(1));
            session
                .apply_action(
                    PlayerId(0),
                    Action::PlayLand {
                        object: plains_h,
                        from_zone: Zone::Hand,
                        face_index: 0,
                    },
                )
                .unwrap();

            let green_h = find_hand(&session, CardDefId(2));
            session
                .apply_action(
                    PlayerId(0),
                    Action::BeginCast {
                        object: green_h,
                        option_id: 0,
                        from_zone: Zone::Hand,
                    },
                )
                .unwrap();
            session
                .apply_action(PlayerId(0), Action::PassPriority)
                .unwrap();
            session
                .apply_action(PlayerId(1), Action::PassPriority)
                .unwrap();

            let red_h = find_hand(&session, CardDefId(3));
            session
                .apply_action(
                    PlayerId(0),
                    Action::BeginCast {
                        object: red_h,
                        option_id: 0,
                        from_zone: Zone::Hand,
                    },
                )
                .unwrap();
            session
                .apply_action(PlayerId(0), Action::PassPriority)
                .unwrap();
            session
                .apply_action(PlayerId(1), Action::PassPriority)
                .unwrap();

            let plains_bf = session.state().players()[0].battlefield[0];
            let green_bf = session.state().players()[0]
                .battlefield
                .iter()
                .copied()
                .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(2))
                .unwrap();
            let red_bf = session.state().players()[0]
                .battlefield
                .iter()
                .copied()
                .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(3))
                .unwrap();

            session
                .apply_action(
                    PlayerId(0),
                    Action::TapMana {
                        object: plains_bf,
                        ability_index: 0,
                        preferred_color: None,
                    },
                )
                .unwrap();

            let colossus_h = find_hand(&session, CardDefId(4));
            session
                .apply_action(
                    PlayerId(0),
                    Action::BeginCast {
                        object: colossus_h,
                        option_id: 0,
                        from_zone: Zone::Hand,
                    },
                )
                .unwrap();
            let x_pid = match session.state().decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            session
                .apply_action(
                    PlayerId(0),
                    Action::Answer {
                        prompt_id: x_pid,
                        answer: ChoiceAnswer::PickNumber { value: 1 },
                    },
                )
                .unwrap();
            session
                .apply_action(
                    PlayerId(0),
                    Action::SelectCostAid {
                        aid_kind: CostAidKind::Convoke,
                        object: green_bf,
                    },
                )
                .unwrap();
            session
                .apply_action(
                    PlayerId(0),
                    Action::SelectCostAid {
                        aid_kind: CostAidKind::Convoke,
                        object: red_bf,
                    },
                )
                .unwrap();
            session
                .apply_action(PlayerId(0), Action::PayFromPool)
                .unwrap();
            assert_eq!(session.state().stack().len(), 1);
            let colossus_stack = session.state().stack()[0];
            assert_eq!(
                session
                    .state()
                    .rare(colossus_stack)
                    .and_then(|r| r.entered_with_x),
                Some(1)
            );

            // Both players pass priority -> resolves Stack -> Battlefield and checks SBAs.
            session
                .apply_action(PlayerId(0), Action::PassPriority)
                .unwrap();
            session
                .apply_action(PlayerId(1), Action::PassPriority)
                .unwrap();

            let colossus_bf = session.state().players()[0]
                .battlefield
                .iter()
                .copied()
                .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(4))
                .expect("Synth X Colossus with X=1 should survive SBA on BF");
            assert_eq!(session.state().counter_count(colossus_bf, cid_p1), 1);
            assert_eq!(
                session
                    .state()
                    .rare(colossus_bf)
                    .and_then(|r| r.entered_with_x),
                Some(1)
            );
            assert_eq!(
                session
                    .state()
                    .evaluate_board()
                    .get(colossus_bf)
                    .unwrap()
                    .pt,
                Some(crate::PowerToughness {
                    power: 1,
                    toughness: 1,
                })
            );
            assert!(session.state().players()[0].graveyard.is_empty());

            let snap = session.snapshot_bytes().unwrap();
            let from_snap = MatchSession::from_snapshot(Arc::clone(&db), &snap).unwrap();
            let from_rep =
                MatchSession::from_replay(Arc::clone(&db), session.replay_log().clone()).unwrap();
            assert_eq!(from_snap.snapshot_bytes().unwrap(), snap);
            assert_eq!(from_rep.snapshot_bytes().unwrap(), snap);
        }

        // Helper for Cases B–E on raw GameState
        let setup_main1 = || {
            let mut s = boot_2p(Arc::clone(&db));
            s.set_active_player(PlayerId(0));
            s.set_phase(GamePhase::Main1);
            s.set_priority_player(PlayerId(0));
            s.set_decision(DecisionState::Priority);
            s
        };
        let add_mana = |s: &mut GameState, color: ManaColor, n: u32| {
            s.player_mut(PlayerId(0)).unwrap().mana_pool[crate::mana::color_to_index(color)] += n;
            s.mark_dirty(DirtyMask::PLAYER_STATE);
        };

        // Case B: Cast Synth X Colossus with X = 0 -> enters BF with 0 counters -> SBA moves to GY.
        {
            let mut state = setup_main1();
            add_mana(&mut state, ManaColor::G, 1);
            add_mana(&mut state, ManaColor::W, 1);
            let col_h = put_in_hand(&mut state, PlayerId(0), CardDefId(4));
            begin_cast_spell(&mut state, PlayerId(0), col_h, 0, Zone::Hand).unwrap();
            let pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 0 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let col_stack = state.stack()[0];
            assert_eq!(
                state.rare(col_stack).and_then(|r| r.entered_with_x),
                Some(0)
            );
            assert!(resolve_top_of_stack(&mut state).unwrap());
            let sba = check_sbas(&mut state).unwrap();
            assert_eq!(sba, crate::sba::SbaOutcome::Done);
            assert!(state.players()[0].battlefield.is_empty());
            assert_eq!(state.players()[0].graveyard.len(), 1);
            let col_gy = state.players()[0].graveyard[0];
            assert_eq!(state.counter_count(col_gy, cid_p1), 0);
            assert_eq!(state.rare(col_gy).and_then(|r| r.entered_with_x), None);
        }

        // Case C: Non-cast reanimation (Graveyard -> Battlefield) and blink (BF -> Exile -> BF)
        // evaluate X = 0 (CR 107.3e / CR 107.3h) even if previously cast with X = 3.
        {
            let mut state = setup_main1();
            add_mana(&mut state, ManaColor::G, 4);
            add_mana(&mut state, ManaColor::W, 1);
            let col_h = put_in_hand(&mut state, PlayerId(0), CardDefId(4));
            begin_cast_spell(&mut state, PlayerId(0), col_h, 0, Zone::Hand).unwrap();
            let pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 3 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let col_bf = state.stack()[0];
            assert!(resolve_top_of_stack(&mut state).unwrap());
            assert!(state.players()[0].battlefield.contains(&col_bf));
            assert_eq!(state.counter_count(col_bf, cid_p1), 3);

            // Blink BF -> Exile -> BF: LTB clears entered_with_x, so re-entry gets 0 counters.
            let MoveObjectOutcome::Moved(ex_mv) = move_object(
                &mut state,
                col_bf,
                Zone::Exile,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected Exile move");
            };
            let exiled_id = ex_mv.new_id.unwrap();
            assert_eq!(state.rare(exiled_id).and_then(|r| r.entered_with_x), None);
            let MoveObjectOutcome::Moved(re_mv) = move_object(
                &mut state,
                exiled_id,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected BF re-entry");
            };
            let re_bf = re_mv.new_id.unwrap();
            assert_eq!(state.counter_count(re_bf, cid_p1), 0);
            let _ = check_sbas(&mut state).unwrap();
            assert!(state.players()[0].battlefield.is_empty());
            assert_eq!(state.players()[0].graveyard.len(), 1);

            // Reanimate from GY -> BF: still 0 counters.
            let gy_id = state.players()[0].graveyard[0];
            let MoveObjectOutcome::Moved(rean_mv) = move_object(
                &mut state,
                gy_id,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap() else {
                panic!("expected GY -> BF move");
            };
            let rean_bf = rean_mv.new_id.unwrap();
            assert_eq!(state.counter_count(rean_bf, cid_p1), 0);
            let _ = check_sbas(&mut state).unwrap();
            assert!(state.players()[0].battlefield.is_empty());
        }

        // Case D: Synth X2 Hydra (Amount::Mul(2, X)) + Synth Counter Doubler (Multiplier x2)
        // cast with X = 2 -> enters with 2 * 2 * 2 = 8 +1/+1 counters.
        {
            let mut state = setup_main1();
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(6));
            add_mana(&mut state, ManaColor::G, 3);
            let hydra_h = put_in_hand(&mut state, PlayerId(0), CardDefId(5));
            begin_cast_spell(&mut state, PlayerId(0), hydra_h, 0, Zone::Hand).unwrap();
            let pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 2 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let hydra_bf = state.stack()[0];
            assert!(resolve_top_of_stack(&mut state).unwrap());
            assert!(state.players()[0].battlefield.contains(&hydra_bf));
            assert_eq!(state.counter_count(hydra_bf, cid_p1), 8);
            assert_eq!(
                check_sbas(&mut state).unwrap(),
                crate::sba::SbaOutcome::Done
            );
            assert_eq!(state.players()[0].battlefield.len(), 2);
            assert!(state.players()[0].graveyard.is_empty());
        }

        // Case E: Synth Ego Clone ({X}{2}{G}{U}, X=3 copying Synth Bear -> 5/5) AND non-{X}
        // Synth Plain Clone copying X-Colossus on BF (entered_with_x == Some(3) on Colossus)
        // -> Plain Clone has entered_with_x == None -> 0 counters -> SBA -> GY (CR 107.3e).
        {
            let mut state = setup_main1();
            let bear_bf = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));

            // Cast Synth Ego Clone with X = 3.
            add_mana(&mut state, ManaColor::G, 1);
            add_mana(&mut state, ManaColor::U, 1);
            add_mana(&mut state, ManaColor::C, 5);
            let ego_h = put_in_hand(&mut state, PlayerId(0), CardDefId(7));
            begin_cast_spell(&mut state, PlayerId(0), ego_h, 0, Zone::Hand).unwrap();
            let pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 3 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let ego_bf = state.stack()[0];
            // Resolves and pauses on EtbCopy choice.
            assert!(!resolve_top_of_stack(&mut state).unwrap());
            let copy_pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseObjects { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected EtbCopy ChooseObjects prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                copy_pid,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap();
            assert!(state.players()[0].battlefield.contains(&ego_bf));
            assert_eq!(state.counter_count(ego_bf, cid_p1), 3);
            assert_eq!(
                state.evaluate_board().get(ego_bf).unwrap().pt,
                Some(crate::PowerToughness {
                    power: 5,
                    toughness: 5,
                })
            );

            // Also put a Synth X Colossus on BF by casting it with X = 3.
            add_mana(&mut state, ManaColor::G, 4);
            add_mana(&mut state, ManaColor::W, 1);
            let col_h = put_in_hand(&mut state, PlayerId(0), CardDefId(4));
            begin_cast_spell(&mut state, PlayerId(0), col_h, 0, Zone::Hand).unwrap();
            let pid_c = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid_c,
                ChoiceAnswer::PickNumber { value: 3 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let col_bf = state.stack()[0];
            assert!(resolve_top_of_stack(&mut state).unwrap());
            assert!(state.players()[0].battlefield.contains(&col_bf));
            assert_eq!(state.counter_count(col_bf, cid_p1), 3);

            // Cast non-{X} Synth Plain Clone copying Synth X Colossus -> 0 counters -> dies to SBA.
            add_mana(&mut state, ManaColor::U, 1);
            add_mana(&mut state, ManaColor::C, 3);
            let plain_h = put_in_hand(&mut state, PlayerId(0), CardDefId(8));
            begin_cast_spell(&mut state, PlayerId(0), plain_h, 0, Zone::Hand).unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            let plain_stack = state.stack()[0];
            assert_eq!(state.rare(plain_stack).and_then(|r| r.entered_with_x), None);
            assert!(!resolve_top_of_stack(&mut state).unwrap());
            let copy_pid2 = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseObjects { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected EtbCopy prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                copy_pid2,
                ChoiceAnswer::PickObject { id: col_bf },
            )
            .unwrap();
            let sba = check_sbas(&mut state).unwrap();
            assert_eq!(sba, crate::sba::SbaOutcome::Done);
            assert!(
                !state.players()[0].battlefield.iter().any(|&id| state
                    .object(id)
                    .unwrap()
                    .card_def
                    == CardDefId(8))
            );
            let plain_gy = state.players()[0]
                .graveyard
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(8))
                .expect("Synth Plain Clone copying 0/0 Colossus should die to SBA");
            assert_eq!(state.counter_count(plain_gy, cid_p1), 0);
            assert_eq!(state.rare(plain_gy).and_then(|r| r.entered_with_x), None);
        }

        // Case F: stack.rs:441 hardening — if Stack -> Battlefield resolution is redirected
        // to Exile by a replacement effect, entered_with_x is cleared (not leaked into Exile).
        {
            let mut state = setup_main1();
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(9)); // Synth BF Exiler
            add_mana(&mut state, ManaColor::G, 3);
            add_mana(&mut state, ManaColor::W, 1);
            let col_h = put_in_hand(&mut state, PlayerId(0), CardDefId(4));
            begin_cast_spell(&mut state, PlayerId(0), col_h, 0, Zone::Hand).unwrap();
            let pid = match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { prompt_id, .. },
                } => *prompt_id,
                other => panic!("expected X prompt, got {other:?}"),
            };
            apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 2 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            assert!(resolve_top_of_stack(&mut state).unwrap());
            assert_eq!(state.players()[0].exile.len(), 1);
            let exiled_col = state.players()[0].exile[0];
            assert_eq!(state.rare(exiled_col).and_then(|r| r.entered_with_x), None);
        }
    }
}
