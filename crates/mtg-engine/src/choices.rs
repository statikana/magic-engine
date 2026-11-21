use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState};
use crate::casting::{
    advance_casting_state_machine, on_effect_replacement_resumed_during_casting,
    on_replacement_resumed_during_casting, resume_casting_prompt, resume_cost_payment_prompt,
};
use crate::events::{AtomicEventKind, emit_event};
use crate::id::PlayerId;
use crate::mana::{ManaActivationOutcome, resume_mana_color_choice};
pub use crate::replace::ReplacementResumeOutcome;
use crate::replace::{legal_replacement_actions, resume_and_finalize_replacement};
use crate::state::{
    CastingPromptKind, CostPromptKind, EngineError, GameState, PromptOwner, ReplacementResumeHook,
    StepPauseState,
};
use crate::triggers::{flush_triggers, resume_trigger_order};
use crate::zones::MoveObjectOutcome;

/// Returns `true` if any replacement, trigger-ordering, or general prompt is currently awaiting an answer.
#[inline]
pub fn is_prompt_active(state: &GameState) -> bool {
    state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
        || state.pending_prompt().is_some()
}

/// Computes the canonical [`DecisionState`] from active state-machine/prompt contexts.
pub fn current_decision_state(state: &GameState) -> DecisionState {
    if matches!(state.decision(), DecisionState::GameOver { .. }) {
        return state.decision().clone();
    }
    if state.pending_replacement().is_some() || state.pending_trigger_order().is_some() {
        return state.decision().clone();
    }
    if let Some(prompt) = state.pending_prompt() {
        return DecisionState::Prompt {
            request: prompt.request.clone(),
        };
    }
    if state.casting().is_some() {
        return DecisionState::CastingSequence;
    }
    state.decision().clone()
}

/// Computes the canonical acting [`PlayerId`] for the current decision state.
pub fn current_actor(state: &GameState) -> Option<PlayerId> {
    if matches!(state.decision(), DecisionState::GameOver { .. }) {
        return None;
    }
    if let Some(pr) = state.pending_replacement() {
        return Some(pr.chooser);
    }
    if let Some(to) = state.pending_trigger_order() {
        return Some(to.controller);
    }
    if let Some(pp) = state.pending_prompt() {
        return Some(pp.chooser);
    }
    if let Some(cs) = state.casting() {
        return Some(cs.caster);
    }
    Some(state.priority_player())
}

/// Returns all legal [`Action`]s for `player` when a prompt is active.
pub fn legal_choice_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    if state.pending_replacement().is_some() {
        return legal_replacement_actions(state, player);
    }

    if let Some(to) = state.pending_trigger_order() {
        if to.controller != player {
            return Vec::new();
        }
        let mut out: Vec<Action> = to
            .object_ids
            .iter()
            .map(|&id| Action::Answer {
                prompt_id: to.prompt_id,
                answer: ChoiceAnswer::OrderNext { id },
            })
            .collect();
        out.push(Action::Concede);
        return out;
    }

    let Some(pp) = state.pending_prompt() else {
        return Vec::new();
    };
    if pp.chooser != player {
        return Vec::new();
    }

    let mut actions = Vec::new();
    match &pp.request {
        ChoiceRequest::YesNo { prompt_id, .. }
        | ChoiceRequest::GuidedResolution { prompt_id, .. } => {
            let allow_yes = !matches!(
                &pp.owner,
                PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index, .. })
                    if !crate::casting::can_pay_casting_optional_cost(state, *cost_index)
            );
            if allow_yes {
                actions.push(Action::Answer {
                    prompt_id: *prompt_id,
                    answer: ChoiceAnswer::YesNo { value: true },
                });
            }
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
            let picked: &[crate::id::ObjectId] = match &pp.owner {
                PromptOwner::Casting(CastingPromptKind::CostObjectSelection { picked, .. }) => {
                    picked.as_slice()
                }
                PromptOwner::CostPayment(cp) => match &cp.kind {
                    CostPromptKind::ObjectSelection { picked, .. } => picked.as_slice(),
                    _ => &[],
                },
                PromptOwner::ResolutionFrame => crate::effects::active_selection_picked(state),
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
            if let PromptOwner::Casting(CastingPromptKind::CastModes {
                min: c_min,
                max: c_max,
                allow_duplicates,
                option_count: c_opt_count,
                picked,
            }) = &pp.owner
            {
                let (legal_indices, can_done) = crate::casting::legal_casting_mode_choices(
                    state,
                    *c_min,
                    *c_max,
                    *allow_duplicates,
                    *c_opt_count,
                    picked,
                );
                for idx in legal_indices {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::PickMode { index: idx },
                    });
                }
                if can_done {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::DonePicking,
                    });
                }
            } else if let PromptOwner::Casting(CastingPromptKind::CostBranchChoice {
                cost_index,
                branches,
            }) = &pp.owner
            {
                for idx in
                    crate::casting::legal_casting_cost_branch_indices(state, *cost_index, branches)
                {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::PickMode { index: idx },
                    });
                }
            } else {
                let (picked, allow_dup): (&[u16], bool) = match &pp.owner {
                    PromptOwner::ResolutionFrame => state
                        .resolution_vms()
                        .last()
                        .and_then(|vm| match &vm.pause_state {
                            Some(StepPauseState::ChooseMode {
                                picked,
                                allow_duplicates,
                                ..
                            }) => Some((picked.as_slice(), *allow_duplicates)),
                            _ => None,
                        })
                        .unwrap_or((&[], false)),
                    _ => (&[], false),
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
        }
        ChoiceRequest::ChooseNumber {
            prompt_id,
            min,
            max,
        } => {
            let is_cast_x = matches!(
                &pp.owner,
                PromptOwner::Casting(CastingPromptKind::CastXValue { .. })
            );
            if min <= max {
                let count = (i64::from(*max) - i64::from(*min) + 1).min(64);
                for offset in 0..count {
                    let val = (*min as i64 + offset) as i32;
                    if !is_cast_x || crate::casting::is_casting_x_value_valid(state, val) {
                        actions.push(Action::Answer {
                            prompt_id: *prompt_id,
                            answer: ChoiceAnswer::PickNumber { value: val },
                        });
                    }
                }
                if i64::from(*max) - i64::from(*min) + 1 > 64
                    && (!is_cast_x || crate::casting::is_casting_x_value_valid(state, *max))
                {
                    actions.push(Action::Answer {
                        prompt_id: *prompt_id,
                        answer: ChoiceAnswer::PickNumber { value: *max },
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

/// Dispatches `Action::Answer { prompt_id, answer }` to the active prompt owner.
pub fn apply_choice_answer(
    state: &mut GameState,
    player: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<(), EngineError> {
    if state.pending_replacement().is_some() {
        let Some(pr) = state.pending_replacement() else {
            unreachable!();
        };
        if pr.chooser != player || pr.prompt_id != prompt_id {
            return Err(EngineError::IllegalAction(
                "prompt actor or prompt_id mismatch for replacement".into(),
            ));
        }
        return match resume_and_finalize_replacement(state, player, prompt_id, answer)? {
            ReplacementResumeOutcome::Paused { .. } => Ok(()),
            ReplacementResumeOutcome::ZoneMove(mv) => {
                if let Some((land_player, land_zone)) = state.take_pending_land_play() {
                    if let MoveObjectOutcome::Moved(raw) = mv {
                        let land_id = raw.new_id.unwrap_or(raw.old_id);
                        emit_event(
                            state,
                            AtomicEventKind::LandPlayed {
                                player: land_player,
                                object: land_id,
                                from_zone: land_zone,
                            },
                        );
                    }
                    flush_triggers(state);
                    let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
                    let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                    return Ok(());
                }
                if state.casting().is_some() {
                    return on_replacement_resumed_during_casting(state, mv);
                }
                match state.take_pending_replacement_resume() {
                    Some(ReplacementResumeHook::ResolutionFrame) => {
                        let res = crate::effects::resume_after_replacement(
                            state,
                            ReplacementResumeOutcome::ZoneMove(mv),
                        )?;
                        if matches!(res, crate::effects::FrameOutcome::Completed) {
                            crate::stack::finish_or_resume_stack_resolution(state)?;
                            let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        }
                        Ok(())
                    }
                    Some(ReplacementResumeHook::StackResolutionFinish {
                        attach_target,
                        is_adventure,
                    }) => {
                        crate::stack::complete_stack_resolution_after_replacement(
                            state,
                            mv,
                            attach_target,
                            is_adventure,
                        )?;
                        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        Ok(())
                    }
                    Some(ReplacementResumeHook::CombatDamageStep) => {
                        let _ = crate::combat::resume_combat_damage_after_replacement(
                            state,
                            ReplacementResumeOutcome::ZoneMove(mv),
                        )?;
                        Ok(())
                    }
                    Some(ReplacementResumeHook::SbaLoop { in_cleanup }) => {
                        let outcome = crate::sba::resume_sba_after_replacement(
                            state,
                            ReplacementResumeOutcome::ZoneMove(mv),
                            in_cleanup,
                        )?;
                        if matches!(outcome, crate::sba::SbaOutcome::Done) {
                            finish_after_sba_done(state, in_cleanup)?;
                        }
                        Ok(())
                    }
                    Some(ReplacementResumeHook::TurnStep(ctx)) => {
                        let _ = crate::turn::resume_turn_after_replacement(
                            state,
                            ctx,
                            ReplacementResumeOutcome::ZoneMove(mv),
                        )?;
                        Ok(())
                    }
                    None => {
                        flush_triggers(state);
                        let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
                        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        Ok(())
                    }
                }
            }
            ReplacementResumeOutcome::EffectResolved(outcome) => {
                if state.casting().is_some() {
                    return on_effect_replacement_resumed_during_casting(state, *outcome);
                }
                match state.take_pending_replacement_resume() {
                    Some(ReplacementResumeHook::ResolutionFrame) => {
                        let res = crate::effects::resume_after_replacement(
                            state,
                            ReplacementResumeOutcome::EffectResolved(outcome),
                        )?;
                        if matches!(res, crate::effects::FrameOutcome::Completed) {
                            crate::stack::finish_or_resume_stack_resolution(state)?;
                            let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        }
                        Ok(())
                    }
                    Some(ReplacementResumeHook::StackResolutionFinish { .. }) => {
                        let _ = crate::sba::check_sbas(state)?;
                        flush_triggers(state);
                        let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
                        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        Ok(())
                    }
                    Some(ReplacementResumeHook::CombatDamageStep) => {
                        let _ = crate::combat::resume_combat_damage_after_replacement(
                            state,
                            ReplacementResumeOutcome::EffectResolved(outcome),
                        )?;
                        Ok(())
                    }
                    Some(ReplacementResumeHook::SbaLoop { in_cleanup }) => {
                        let outcome = crate::sba::resume_sba_after_replacement(
                            state,
                            ReplacementResumeOutcome::EffectResolved(outcome),
                            in_cleanup,
                        )?;
                        if matches!(outcome, crate::sba::SbaOutcome::Done) {
                            finish_after_sba_done(state, in_cleanup)?;
                        }
                        Ok(())
                    }
                    Some(ReplacementResumeHook::TurnStep(ctx)) => {
                        let _ = crate::turn::resume_turn_after_replacement(
                            state,
                            ctx,
                            ReplacementResumeOutcome::EffectResolved(outcome),
                        )?;
                        Ok(())
                    }
                    None => {
                        flush_triggers(state);
                        let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
                        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                        Ok(())
                    }
                }
            }
        };
    }

    if state.pending_trigger_order().is_some() {
        resume_trigger_order(state, player, prompt_id, answer)?;
        let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
        return Ok(());
    }

    let Some(pp) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no active prompt to answer".into(),
        ));
    };
    if pp.chooser != player || pp.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "prompt actor or prompt_id mismatch".into(),
        ));
    }

    match &pp.owner {
        PromptOwner::ManaAbility(_) => {
            match resume_mana_color_choice(state, player, prompt_id, answer)? {
                ManaActivationOutcome::Paused { .. } => Ok(()),
                ManaActivationOutcome::Produced => {
                    if let Some(cs) = state.casting() {
                        let caster = cs.caster;
                        state.set_decision(DecisionState::CastingSequence);
                        state.set_priority_player(caster);
                        advance_casting_state_machine(state)?;
                    }
                    Ok(())
                }
            }
        }
        PromptOwner::Casting(_) => resume_casting_prompt(state, player, prompt_id, answer),
        PromptOwner::CostPayment(_) => resume_cost_payment_prompt(state, player, prompt_id, answer),
        PromptOwner::ResolutionFrame => {
            match crate::effects::resume_resolution_frame(state, player, prompt_id, answer)? {
                crate::effects::FrameOutcome::Completed => {
                    crate::stack::finish_or_resume_stack_resolution(state)?;
                    let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
                    Ok(())
                }
                crate::effects::FrameOutcome::Paused { .. }
                | crate::effects::FrameOutcome::GameOver { .. } => Ok(()),
            }
        }
        PromptOwner::Sba(_) => {
            let in_cleanup = state.pending_sba_in_cleanup();
            let outcome = crate::sba::resume_sba_prompt(state, player, prompt_id, answer)?;
            if matches!(outcome, crate::sba::SbaOutcome::Done) {
                finish_after_sba_done(state, in_cleanup)?;
            }
            Ok(())
        }
        PromptOwner::Turn(_) => {
            crate::turn::resume_turn_prompt(state, player, prompt_id, answer).map(|_| ())
        }
        PromptOwner::Combat(_) => {
            crate::combat::resume_combat_prompt(state, player, prompt_id, answer).map(|_| ())
        }
        PromptOwner::Replacement | PromptOwner::TriggerOrder => Err(EngineError::IllegalAction(
            "unexpected direct PendingPrompt for replacement/trigger order".into(),
        )),
    }
}

fn finish_after_sba_done(state: &mut GameState, in_cleanup: bool) -> Result<(), EngineError> {
    if matches!(state.decision(), DecisionState::GameOver { .. })
        || is_prompt_active(state)
        || state.casting().is_some()
    {
        return Ok(());
    }
    if in_cleanup {
        let _ = crate::turn::finish_cleanup_after_sba(state)?;
    } else {
        let _ = crate::turn::grant_priority_with_sba_and_triggers(state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::PendingPromptContext;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, SupertypeMask,
    };
    use smallvec::SmallVec;
    use std::sync::Arc;

    fn minimal_state() -> GameState {
        let card = CardDef {
            oracle_id: "ch_w".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Wastes".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Wastes".into(),
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
        GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 23).unwrap()
    }

    #[test]
    fn wrong_prompt_id_and_actor_reject_cleanly() {
        let mut state = minimal_state();
        let pid = state.allocate_prompt_id();
        let req = ChoiceRequest::ChooseNumber {
            prompt_id: pid,
            min: 0,
            max: 5,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id: pid,
            chooser: PlayerId(0),
            request: req.clone(),
            owner: PromptOwner::Casting(CastingPromptKind::CastXValue { min: 0, max: 5 }),
        }));
        state.set_decision(DecisionState::Prompt { request: req });

        assert_eq!(current_actor(&state), Some(PlayerId(0)));
        let legal = legal_choice_actions(&state, PlayerId(0));
        assert!(legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickNumber { value: 3 },
        }));
        assert!(legal_choice_actions(&state, PlayerId(1)).is_empty());

        let err_id = apply_choice_answer(
            &mut state,
            PlayerId(0),
            999_999,
            ChoiceAnswer::PickNumber { value: 3 },
        );
        assert!(matches!(err_id, Err(EngineError::IllegalAction(_))));
        assert!(state.pending_prompt().is_some());

        let err_actor = apply_choice_answer(
            &mut state,
            PlayerId(1),
            pid,
            ChoiceAnswer::PickNumber { value: 3 },
        );
        assert!(matches!(err_actor, Err(EngineError::IllegalAction(_))));
        assert!(state.pending_prompt().is_some());
    }

    #[test]
    fn synth_choices_cast_x_value_filters_unpayable_numbers() {
        use mtg_ir::{AbilityDef, Amount, CostDef, SpellAbility, SubtypeId, Zone};

        let mut tables = InternTables::default();
        tables.subtypes.push("Swamp".into());
        let sub_swamp = SubtypeId(0);
        let swamp = CardDef {
            oracle_id: "ch_swamp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Swamp".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_swamp]),
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
        let x_spell = CardDef {
            oracle_id: "ch_x_gy".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Graveyard X".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Graveyard X".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{X}{B}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::ExileFrom {
                        zone: Zone::Graveyard,
                        count: Box::new(Amount::X),
                        target: None,
                        from_top: false,
                        any_graveyard: false,
                        single_graveyard: false,
                        min_total_mana_value: None,
                        min_total_mana_value_amount: None,
                        min_colored_mana_symbols: None,
                        collect_evidence: false,
                        one_or_more: false,
                        total_mana_value_equals_x: false,
                        and_other_same_name_count: None,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
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
                cards: vec![swamp, x_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1), CardDefId(0), CardDefId(0), CardDefId(0)],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 41).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        for _ in 0..4 {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(oid);
        }
        let gy_ts = state.allocate_timestamp();
        let gy_oid = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Graveyard,
                gy_ts,
                crate::state::ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .graveyard
            .push(gy_oid);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let sp_id = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        crate::casting::begin_cast_spell(&mut state, PlayerId(0), sp_id, 0, Zone::Hand).unwrap();
        let pid = state.pending_prompt().unwrap().prompt_id;
        let legal = legal_choice_actions(&state, PlayerId(0));
        assert!(legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickNumber { value: 0 },
        }));
        assert!(legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickNumber { value: 1 },
        }));
        assert!(!legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickNumber { value: 2 },
        }));
        assert!(!legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickNumber { value: 3 },
        }));
    }

    #[test]
    fn synth_choices_cost_branch_choice_filters_unpayable_branches() {
        use mtg_ir::{AbilityDef, Amount, CostDef, ObjectFilter, SpellAbility, SubtypeId, Zone};

        let mut tables = InternTables::default();
        tables.subtypes.push("Plains".into());
        let sub_plains = SubtypeId(0);
        let plains = CardDef {
            oracle_id: "ch_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_plains]),
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
        let choice_spell = CardDef {
            oracle_id: "ch_choice_sp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Choice Aspirant".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Choice Aspirant".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{W}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 1,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Choice {
                        options: vec![
                            vec![CostDef::Behold {
                                count: Box::new(Amount::Const { n: 1 }),
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                                exile: false,
                            }],
                            vec![CostDef::Mana {
                                cost: ManaCost::parse("{2}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        ],
                        forage: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
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
                cards: vec![plains, choice_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1), CardDefId(0), CardDefId(0), CardDefId(0)],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 42).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        for _ in 0..3 {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                crate::state::GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(oid);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let sp_id = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        crate::casting::begin_cast_spell(&mut state, PlayerId(0), sp_id, 0, Zone::Hand).unwrap();
        crate::casting::apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana)
            .unwrap();

        let pid = state.pending_prompt().unwrap().prompt_id;
        let legal = legal_choice_actions(&state, PlayerId(0));
        assert!(!legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickMode { index: 0 },
        }));
        assert!(legal.contains(&Action::Answer {
            prompt_id: pid,
            answer: ChoiceAnswer::PickMode { index: 1 },
        }));
    }
}
