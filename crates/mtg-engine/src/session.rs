use std::sync::Arc;

use mtg_ir::{CardDatabase, CardTypeMask, ColorMask, ManaCost, Zone};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::action::{
    Action, AutoTapPreview, CastRequest, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec,
    DeckValidationReport, MacroAction, StepOutcome,
};
use crate::auto_tap::preview_auto_tap_for_request;
use crate::casting::{
    apply_casting_action, apply_play_land, apply_special_action, begin_activate_ability,
    begin_cast_spell, legal_casting_actions,
};
use crate::choices::{
    apply_choice_answer, current_actor as state_current_actor, current_decision_state,
    is_prompt_active, legal_choice_actions,
};
use crate::combat::{apply_combat_action, legal_combat_actions};
use crate::commander::{validate_commander_deck, validate_standard_deck};
use crate::config::GameConfig;
use crate::determinize::determinize_state;
use crate::events::{begin_action, end_action};
use crate::id::{ObjectId, PlayerId};
use crate::legality::collect_priority_actions;
use crate::mana::activate_mana_ability;
use crate::observe::{
    is_hand_revealed_to_cached, is_object_visible_to, is_top_of_library_visible_to,
    is_top_of_library_visible_to_cached, observe_state,
};
use crate::replay::{ReplayEntry, ReplayLog};
use crate::rng::RngState;
use crate::sba::check_sbas;
use crate::state::{
    CastStep, CastingPromptKind, CostPromptKind, CostSelectionKind, EngineError,
    EtbInteractiveStep, FaceDownKind, GameObject, GameState, GameStateSnapshot, NONE_OBJECT_ID,
    ObjectFlags, PendingReplacementPrompt, PromptOwner, SbaPromptKind, StackEntryKind,
    StepPauseState, TOKEN_CARD_DEF_ID, TurnPromptKind,
};
use crate::turn::{
    apply_concede, apply_mulligan_action, begin_opening_mulligans, legal_turn_actions,
    pass_priority,
};
use crate::view::{
    ClientViewModel, CombatAttackerView, CombatView, ContinuousBreakdown, ObjectView, Observation,
    PlayerView, PromptPurposeView, PromptView, StackItemKindView, StackItemView,
    TopLibraryVisibility, TopOfLibraryView,
};
use crate::zones::is_known_to;

const SNAPSHOT_MAGIC: &[u8; 4] = b"MTGS";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatchSessionSnapshot {
    state: GameStateSnapshot,
    log: ReplayLog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    Mulligan,
    Priority,
    CastingSequence,
    DeclareAttackers,
    DeclareBlockers,
    AssignCombatDamage,
    Prompt,
    GameOver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionInfo {
    pub actor: Option<PlayerId>,
    pub state_kind: DecisionKind,
    pub legal_count: usize,
}

/// Narrow message-oriented façade shared by `mtg-wasm`, `mtg-py`, `mtg-cli`, and `magic-web`.
#[derive(Debug, Clone)]
pub struct MatchSession {
    state: GameState,
    log: ReplayLog,
}

impl MatchSession {
    pub fn new(
        db: Arc<CardDatabase>,
        config: GameConfig,
        decks: Vec<DeckSpec>,
        seed: u64,
    ) -> Result<Self, EngineError> {
        let mut state = GameState::new(db, config.clone(), &decks, seed)?;
        begin_opening_mulligans(&mut state)?;
        let log = ReplayLog {
            config,
            decks,
            seed,
            entries: Vec::new(),
        };
        Ok(Self { state, log })
    }

    #[inline]
    pub fn state(&self) -> &GameState {
        &self.state
    }

    #[inline]
    pub fn config(&self) -> &GameConfig {
        self.state.config()
    }

    #[inline]
    pub fn replay_log(&self) -> &ReplayLog {
        &self.log
    }

    pub fn current_actor(&self) -> Option<PlayerId> {
        state_current_actor(&self.state)
    }

    pub fn decision_info(&self) -> DecisionInfo {
        let actor = self.current_actor();
        let state_kind = match current_decision_state(&self.state) {
            DecisionState::Mulligan => DecisionKind::Mulligan,
            DecisionState::Priority => DecisionKind::Priority,
            DecisionState::CastingSequence => DecisionKind::CastingSequence,
            DecisionState::DeclareAttackers => DecisionKind::DeclareAttackers,
            DecisionState::DeclareBlockers => DecisionKind::DeclareBlockers,
            DecisionState::AssignCombatDamage => DecisionKind::AssignCombatDamage,
            DecisionState::Prompt { .. } => DecisionKind::Prompt,
            DecisionState::GameOver { .. } => DecisionKind::GameOver,
        };
        let legal_count = actor.map_or(0, |p| self.legal_actions(p).len());
        DecisionInfo {
            actor,
            state_kind,
            legal_count,
        }
    }

    pub fn legal_actions(&self, player: PlayerId) -> Vec<Action> {
        if self.current_actor() != Some(player) {
            return Vec::new();
        }
        let mut actions = if is_prompt_active(&self.state) {
            legal_choice_actions(&self.state, player)
        } else {
            match current_decision_state(&self.state) {
                DecisionState::Mulligan => legal_turn_actions(&self.state, player),
                DecisionState::Priority => collect_priority_actions(&self.state, player),
                DecisionState::CastingSequence => legal_casting_actions(&self.state, player),
                DecisionState::DeclareAttackers
                | DecisionState::DeclareBlockers
                | DecisionState::AssignCombatDamage => legal_combat_actions(&self.state, player),
                DecisionState::Prompt { .. } => legal_choice_actions(&self.state, player),
                DecisionState::GameOver { .. } => return Vec::new(),
            }
        };
        if !actions.contains(&Action::Concede) {
            actions.push(Action::Concede);
        }
        actions
    }

    pub fn apply_action(
        &mut self,
        player: PlayerId,
        action: Action,
    ) -> Result<StepOutcome, EngineError> {
        self.apply_action_guarded(player, action, true)
    }

    fn apply_action_guarded(
        &mut self,
        player: PlayerId,
        action: Action,
        with_rollback: bool,
    ) -> Result<StepOutcome, EngineError> {
        if self.current_actor() != Some(player) {
            return Err(EngineError::IllegalAction(format!(
                "player {player:?} does not currently have the decision"
            )));
        }

        let need_clone =
            with_rollback && !is_infallible_nonfinal_pass(&self.state, player, &action);
        let saved_state = need_clone.then(|| self.state.clone());

        begin_action(&mut self.state);
        let res = self.apply_action_inner(player, &action);
        end_action(&mut self.state);
        if let Err(err) = res {
            if let Some(saved) = saved_state {
                self.state = saved;
            }
            return Err(err);
        }
        self.state.prune_stale_lki();

        let seq = self.state.bump_event_seq();
        self.log.entries.push(ReplayEntry {
            seq,
            actor: player,
            action,
        });
        Ok(StepOutcome {
            event_seq: seq,
            next_actor: self.current_actor(),
            decision: current_decision_state(&self.state),
        })
    }

    fn apply_action_inner(&mut self, player: PlayerId, action: &Action) -> Result<(), EngineError> {
        if matches!(action, Action::Concede) {
            let _ = apply_concede(&mut self.state, player)?;
            if !is_prompt_active(&self.state)
                && self.state.casting().is_none()
                && matches!(self.state.decision(), DecisionState::Priority)
            {
                let _ = check_sbas(&mut self.state)?;
            }
            return Ok(());
        }

        if is_prompt_active(&self.state) {
            return match action {
                Action::Answer { prompt_id, answer } => {
                    apply_choice_answer(&mut self.state, player, *prompt_id, answer.clone())
                }
                _ => Err(EngineError::IllegalAction(
                    "expected Action::Answer for active prompt".into(),
                )),
            };
        }

        match current_decision_state(&self.state) {
            DecisionState::Mulligan => match action {
                Action::KeepHand | Action::TakeMulligan | Action::BottomCard { .. } => {
                    apply_mulligan_action(&mut self.state, player, action).map(|_| ())
                }
                _ => Err(EngineError::IllegalAction(
                    "expected mulligan action in Mulligan decision state".into(),
                )),
            },
            DecisionState::Priority => match action {
                Action::PassPriority => pass_priority(&mut self.state, player).map(|_| ()),
                Action::PlayLand {
                    object,
                    from_zone,
                    face_index,
                } => apply_play_land(&mut self.state, player, *object, *from_zone, *face_index),
                Action::TapMana {
                    object,
                    ability_index,
                    preferred_color,
                } => activate_mana_ability(
                    &mut self.state,
                    player,
                    *object,
                    *ability_index,
                    *preferred_color,
                )
                .map(|_| ()),
                Action::BeginCast {
                    object,
                    option_id,
                    from_zone,
                } => begin_cast_spell(&mut self.state, player, *object, *option_id, *from_zone),
                Action::BeginActivate {
                    object,
                    ability_index,
                } => begin_activate_ability(&mut self.state, player, *object, *ability_index),
                Action::SpecialAction { kind } => {
                    apply_special_action(&mut self.state, player, *kind)
                }
                _ => Err(EngineError::IllegalAction(
                    "illegal action in Priority decision state".into(),
                )),
            },
            DecisionState::CastingSequence => apply_casting_action(&mut self.state, player, action),
            DecisionState::DeclareAttackers
            | DecisionState::DeclareBlockers
            | DecisionState::AssignCombatDamage => {
                apply_combat_action(&mut self.state, player, action).map(|_| ())
            }
            DecisionState::Prompt { .. } => match action {
                Action::Answer { prompt_id, answer } => {
                    apply_choice_answer(&mut self.state, player, *prompt_id, answer.clone())
                }
                _ => Err(EngineError::IllegalAction(
                    "expected Action::Answer for active prompt".into(),
                )),
            },
            DecisionState::GameOver { .. } => {
                Err(EngineError::IllegalAction("game is already over".into()))
            }
        }
    }

    pub fn apply_macro_action(
        &mut self,
        player: PlayerId,
        macro_act: MacroAction,
    ) -> Result<StepOutcome, EngineError> {
        match macro_act {
            MacroAction::Single(act) => self.apply_action(player, act),
            batch => {
                if self.current_actor() != Some(player) {
                    return Err(EngineError::IllegalAction(format!(
                        "player {player:?} does not currently have the decision"
                    )));
                }
                let saved_state = self.state.clone();
                let saved_log_len = self.log.entries.len();
                let res = self.apply_macro_batch(player, batch);
                if let Err(err) = res {
                    self.state = saved_state;
                    self.log.entries.truncate(saved_log_len);
                    return Err(err);
                }
                res
            }
        }
    }

    fn apply_macro_batch(
        &mut self,
        player: PlayerId,
        macro_act: MacroAction,
    ) -> Result<StepOutcome, EngineError> {
        match macro_act {
            MacroAction::Single(_) => unreachable!(),
            MacroAction::LondonBottom { objects } => {
                if !matches!(current_decision_state(&self.state), DecisionState::Mulligan) {
                    return Err(EngineError::IllegalAction(
                        "LondonBottom requires Mulligan decision state".into(),
                    ));
                }
                let already_kept = self.state.player(player).is_some_and(|p| p.has_kept_hand);
                if !already_kept {
                    self.apply_action_guarded(player, Action::KeepHand, false)?;
                }
                let remaining = usize::from(self.state.mulligan_bottom_remaining(player));
                if objects.len() != remaining {
                    return Err(EngineError::IllegalAction(format!(
                        "LondonBottom expected {remaining} cards, got {}",
                        objects.len()
                    )));
                }
                for obj in objects {
                    self.apply_action_guarded(player, Action::BottomCard { object: obj }, false)?;
                }
                Ok(StepOutcome {
                    event_seq: self.state.event_seq(),
                    next_actor: self.current_actor(),
                    decision: current_decision_state(&self.state),
                })
            }
            MacroAction::DeclareAttackers { assignments } => {
                if !matches!(
                    current_decision_state(&self.state),
                    DecisionState::DeclareAttackers
                ) {
                    return Err(EngineError::IllegalAction(
                        "DeclareAttackers requires DeclareAttackers decision state".into(),
                    ));
                }
                for (attacker, target) in assignments {
                    self.apply_action_guarded(
                        player,
                        Action::AssignAttacker { attacker, target },
                        false,
                    )?;
                }
                self.apply_action_guarded(player, Action::ConfirmAttackers, false)
            }
            MacroAction::DeclareBlockers { assignments } => {
                if !matches!(
                    current_decision_state(&self.state),
                    DecisionState::DeclareBlockers
                ) {
                    return Err(EngineError::IllegalAction(
                        "DeclareBlockers requires DeclareBlockers decision state".into(),
                    ));
                }
                for (blocker, attacker) in assignments {
                    self.apply_action_guarded(
                        player,
                        Action::AssignBlocker { blocker, attacker },
                        false,
                    )?;
                }
                self.apply_action_guarded(player, Action::ConfirmBlockers, false)
            }
            MacroAction::CastSpell {
                object,
                option_id,
                from_zone,
                modes,
                x_value,
                targets,
            } => {
                if !matches!(current_decision_state(&self.state), DecisionState::Priority)
                    || is_prompt_active(&self.state)
                {
                    return Err(EngineError::IllegalAction(
                        "CastSpell requires Priority decision state".into(),
                    ));
                }
                self.apply_action_guarded(
                    player,
                    Action::BeginCast {
                        object,
                        option_id,
                        from_zone,
                    },
                    false,
                )?;

                let mut mode_cursor = 0usize;
                let mut target_cursor = 0usize;
                let mut x_applied = false;
                let mut guard = 0usize;

                while matches!(
                    current_decision_state(&self.state),
                    DecisionState::CastingSequence | DecisionState::Prompt { .. }
                ) {
                    guard += 1;
                    if guard > 64 {
                        return Err(EngineError::IllegalAction(
                            "MacroAction::CastSpell exceeded step limit".into(),
                        ));
                    }

                    if let Some(prompt) = self.state.pending_prompt().cloned() {
                        if prompt.chooser != player {
                            break;
                        }
                        match &prompt.owner {
                            PromptOwner::Casting(CastingPromptKind::CastModes { min, .. }) => {
                                if let Some(&m_idx) = modes.get(mode_cursor) {
                                    mode_cursor += 1;
                                    self.apply_action_guarded(
                                        player,
                                        Action::Answer {
                                            prompt_id: prompt.prompt_id,
                                            answer: ChoiceAnswer::PickMode { index: m_idx },
                                        },
                                        false,
                                    )?;
                                } else if mode_cursor >= usize::from(*min) {
                                    self.apply_action_guarded(
                                        player,
                                        Action::Answer {
                                            prompt_id: prompt.prompt_id,
                                            answer: ChoiceAnswer::DonePicking,
                                        },
                                        false,
                                    )?;
                                } else {
                                    return Err(EngineError::IllegalAction(
                                        "MacroAction::CastSpell missing required mode selection"
                                            .into(),
                                    ));
                                }
                            }
                            PromptOwner::Casting(CastingPromptKind::CastXValue { .. }) => {
                                if x_applied {
                                    return Err(EngineError::IllegalAction(
                                        "unexpected duplicate X value prompt".into(),
                                    ));
                                }
                                x_applied = true;
                                let xv = i32::from(x_value.unwrap_or(0));
                                self.apply_action_guarded(
                                    player,
                                    Action::Answer {
                                        prompt_id: prompt.prompt_id,
                                        answer: ChoiceAnswer::PickNumber { value: xv },
                                    },
                                    false,
                                )?;
                            }
                            _ => {
                                // Interactive prompt (e.g. Sacrifice/Discard/Optional/Guided):
                                // return cleanly so caller/driver answers the prompt.
                                break;
                            }
                        }
                    } else if self.state.pending_replacement().is_some()
                        || self.state.pending_trigger_order().is_some()
                    {
                        break;
                    } else if let Some(cs) = self.state.casting() {
                        match cs.step {
                            CastStep::ChooseTargets => {
                                if let Some(&t) = targets.get(target_cursor) {
                                    target_cursor += 1;
                                    self.apply_action_guarded(
                                        player,
                                        Action::SelectTarget { target: t },
                                        false,
                                    )?;
                                } else {
                                    self.apply_action_guarded(player, Action::DoneTargets, false)?;
                                }
                            }
                            CastStep::ChooseDivisions => {
                                break;
                            }
                            CastStep::PayCosts => {
                                self.apply_action_guarded(player, Action::AutoPayMana, false)?;
                            }
                            _ => {
                                return Err(EngineError::IllegalAction(
                                    "unexpected unprompted casting step in MacroAction::CastSpell"
                                        .into(),
                                ));
                            }
                        }
                    } else {
                        break;
                    }
                }

                let (past_modes, past_targets) = match self.state.casting() {
                    None => (true, true),
                    Some(cs) => (
                        !matches!(cs.step, CastStep::ChooseModes)
                            || self.state.pending_prompt().is_some_and(|p| {
                                p.chooser != player
                                    && matches!(
                                        p.owner,
                                        PromptOwner::Casting(CastingPromptKind::CastModes { .. })
                                    )
                            }),
                        matches!(
                            cs.step,
                            CastStep::ChooseDivisions | CastStep::PayCosts | CastStep::Complete
                        ),
                    ),
                };
                if (past_modes && mode_cursor < modes.len())
                    || (past_targets && target_cursor < targets.len())
                {
                    return Err(EngineError::IllegalAction(
                        "extraneous modes or targets in MacroAction::CastSpell".into(),
                    ));
                }

                Ok(StepOutcome {
                    event_seq: self.state.event_seq(),
                    next_actor: self.current_actor(),
                    decision: current_decision_state(&self.state),
                })
            }
        }
    }

    pub fn view_model(&self, viewer: Option<PlayerId>) -> ClientViewModel {
        let eval_cache = self.state.evaluate_board();
        let config = self.state.config();

        let mut bf_attachments: SmallVec<[(ObjectId, ObjectId); 8]> = SmallVec::new();
        for p in self.state.players() {
            for &cand_id in &p.battlefield {
                if let Some(cand) = self.state.object(cand_id)
                    && !cand.is_phased_out()
                    && let Some(host) = cand.attached_to
                {
                    bf_attachments.push((host, cand_id));
                }
            }
        }

        let to_view_with_known =
            |oid: ObjectId, obj: &GameObject, force_known: bool| -> ObjectView {
                let eval = eval_cache.get(oid);
                let rare = self.state.rare(oid);
                let is_face_down = obj.is_face_down();
                let known = force_known
                    || viewer.is_some_and(|v| {
                        if obj.zone == Zone::Exile && is_face_down {
                            crate::observe::is_object_visible_to_cached(
                                &self.state,
                                oid,
                                Some(v),
                                eval_cache,
                            )
                        } else {
                            is_known_to(&self.state, oid, v)
                        }
                    });
                let bf_or_stack = matches!(obj.zone, Zone::Battlefield | Zone::Stack);
                let hide_identity = is_face_down && !known;

                let attachments: SmallVec<[ObjectId; 4]> = if !bf_attachments.is_empty()
                    && obj.zone == Zone::Battlefield
                    && !obj.is_phased_out()
                {
                    bf_attachments
                        .iter()
                        .filter(|&&(host, _)| host == oid)
                        .map(|&(_, att)| att)
                        .collect()
                } else {
                    SmallVec::new()
                };

                let (
                    card_def,
                    controller,
                    pt,
                    base_pt,
                    types,
                    colors,
                    subtypes,
                    supertypes,
                    keywords,
                    loyalty,
                    defense,
                    mana_value,
                    mana_cost,
                    is_commander,
                    has_lost_all_abilities,
                    face_down_kind,
                    active_face_index,
                    flipped,
                    transformed,
                    entered_this_turn,
                    monstrous,
                    renowned,
                    suspected,
                    saddled,
                    harnessed,
                    prepared,
                    prototyped,
                    goaded_by,
                    class_level,
                    unlocked_faces,
                    playable_until_turn,
                    playable_by,
                    plotted_turn,
                    entered_with_x,
                    chosen_color,
                    chosen_subtype,
                    chosen_player,
                    chosen_card_type,
                    chosen_number,
                ) = if hide_identity {
                    let fd_kind = if bf_or_stack {
                        rare.and_then(|r| r.face_down.as_deref())
                            .map(|fd| fd.kind)
                            .or(Some(FaceDownKind::Other))
                    } else {
                        Some(FaceDownKind::Other)
                    };
                    if bf_or_stack {
                        let on_bf = obj.zone == Zone::Battlefield;
                        (
                            TOKEN_CARD_DEF_ID,
                            eval.map_or(obj.controller, |e| e.controller),
                            eval.and_then(|e| e.pt),
                            eval.and_then(|e| e.base_pt),
                            eval.map_or(CardTypeMask::CREATURE, |e| e.types),
                            eval.map_or(ColorMask::empty(), |e| e.colors),
                            eval.map(|e| e.subtypes.clone()).unwrap_or_default(),
                            eval.map_or(mtg_ir::SupertypeMask::empty(), |e| e.supertypes),
                            eval.map(|e| e.keywords.clone()).unwrap_or_default(),
                            None,
                            None,
                            0,
                            ManaCost::default(),
                            false,
                            eval.is_some_and(|e| e.lost_all_abilities),
                            fd_kind,
                            0,
                            false,
                            false,
                            obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN),
                            on_bf && obj.flags.contains(ObjectFlags::MONSTROUS),
                            on_bf && obj.flags.contains(ObjectFlags::RENOWNED),
                            on_bf && obj.flags.contains(ObjectFlags::SUSPECTED),
                            on_bf && obj.flags.contains(ObjectFlags::SADDLED),
                            on_bf && obj.flags.contains(ObjectFlags::HARNESSED),
                            on_bf && obj.flags.contains(ObjectFlags::PREPARED),
                            false,
                            if on_bf {
                                rare.map(|r| r.goaded_by.clone()).unwrap_or_default()
                            } else {
                                SmallVec::new()
                            },
                            None,
                            SmallVec::new(),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                    } else {
                        (
                            TOKEN_CARD_DEF_ID,
                            obj.controller,
                            None,
                            None,
                            CardTypeMask::empty(),
                            ColorMask::empty(),
                            SmallVec::new(),
                            mtg_ir::SupertypeMask::empty(),
                            SmallVec::new(),
                            None,
                            None,
                            0,
                            ManaCost::default(),
                            false,
                            false,
                            fd_kind,
                            0,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            false,
                            SmallVec::new(),
                            None,
                            SmallVec::new(),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                    }
                } else {
                    let fd_kind = if is_face_down {
                        rare.and_then(|r| r.face_down.as_deref())
                            .map(|fd| fd.kind)
                            .or(Some(FaceDownKind::Other))
                    } else {
                        None
                    };
                    (
                        obj.card_def,
                        eval.map_or(obj.controller, |e| e.controller),
                        eval.and_then(|e| e.pt),
                        eval.and_then(|e| e.base_pt),
                        eval.map_or(CardTypeMask::empty(), |e| e.types),
                        eval.map_or(ColorMask::empty(), |e| e.colors),
                        eval.map(|e| e.subtypes.clone()).unwrap_or_default(),
                        eval.map_or(mtg_ir::SupertypeMask::empty(), |e| e.supertypes),
                        eval.map(|e| e.keywords.clone()).unwrap_or_default(),
                        eval.and_then(|e| e.loyalty),
                        eval.and_then(|e| e.defense),
                        eval.map_or(0, |e| e.mana_value),
                        eval.map(|e| e.mana_cost.clone()).unwrap_or_default(),
                        obj.is_commander(),
                        eval.is_some_and(|e| e.lost_all_abilities),
                        fd_kind,
                        obj.active_face_index,
                        obj.flags.contains(ObjectFlags::FLIPPED),
                        obj.flags.contains(ObjectFlags::TRANSFORMED),
                        obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN),
                        obj.flags.contains(ObjectFlags::MONSTROUS),
                        obj.flags.contains(ObjectFlags::RENOWNED),
                        obj.flags.contains(ObjectFlags::SUSPECTED),
                        obj.flags.contains(ObjectFlags::SADDLED),
                        obj.flags.contains(ObjectFlags::HARNESSED),
                        obj.flags.contains(ObjectFlags::PREPARED),
                        obj.flags.contains(ObjectFlags::PROTOTYPED),
                        rare.map(|r| r.goaded_by.clone()).unwrap_or_default(),
                        rare.and_then(|r| r.class_level),
                        rare.map(|r| r.unlocked_faces.clone()).unwrap_or_default(),
                        rare.and_then(|r| {
                            r.playable_from_exile_by
                                .as_ref()
                                .and_then(|p| p.expires_turn)
                        }),
                        rare.and_then(|r| r.playable_from_exile_by.as_ref().map(|p| p.player)),
                        rare.and_then(|r| r.plotted_turn),
                        rare.and_then(|r| r.entered_with_x),
                        rare.and_then(|r| r.chosen_color),
                        rare.and_then(|r| r.chosen_subtype),
                        rare.and_then(|r| r.chosen_player),
                        rare.and_then(|r| r.chosen_card_type),
                        rare.and_then(|r| r.chosen_number),
                    )
                };

                ObjectView {
                    id: oid,
                    card_def,
                    owner: obj.owner,
                    controller,
                    zone: obj.zone,
                    tapped: obj.flags.contains(ObjectFlags::TAPPED),
                    summoning_sick: obj.flags.contains(ObjectFlags::SUMMONING_SICK),
                    pt,
                    types,
                    colors,
                    counters: self.state.counters(oid).iter().copied().collect(),
                    damage_marked: obj.damage_marked,
                    attached_to: obj.attached_to,
                    base_pt,
                    subtypes,
                    supertypes,
                    keywords,
                    loyalty,
                    defense,
                    mana_value,
                    mana_cost,
                    deathtouch_damage_marked: obj.deathtouch_damage_marked,
                    attachments,
                    is_token: obj.is_token(),
                    is_commander,
                    is_emblem: obj.flags.contains(ObjectFlags::IS_EMBLEM),
                    is_dungeon: obj.flags.contains(ObjectFlags::IS_DUNGEON),
                    has_lost_all_abilities,
                    face_down: is_face_down,
                    face_down_kind,
                    known_to_viewer: known,
                    active_face_index,
                    flipped,
                    transformed,
                    entered_this_turn,
                    monstrous,
                    renowned,
                    suspected,
                    saddled,
                    harnessed,
                    prepared,
                    prototyped,
                    goaded_by,
                    class_level,
                    unlocked_faces,
                    playable_until_turn,
                    playable_by,
                    plotted_turn,
                    entered_with_x,
                    chosen_color,
                    chosen_subtype,
                    chosen_player,
                    chosen_card_type,
                    chosen_number,
                }
            };

        let mut players = Vec::with_capacity(self.state.players().len());
        for p in self.state.players() {
            let ep = eval_cache.player(p.id);
            let hand_revealed = is_hand_revealed_to_cached(&self.state, p.id, viewer, eval_cache);
            let revealed_hand = if viewer == Some(p.id) {
                Vec::new()
            } else if hand_revealed {
                let mut out = Vec::with_capacity(p.hand.len());
                for &id in &p.hand {
                    if let Some(obj) = self.state.object(id) {
                        out.push(to_view_with_known(id, obj, true));
                    }
                }
                out
            } else if let Some(v) = viewer {
                let mut out = Vec::new();
                for &id in &p.hand {
                    if is_known_to(&self.state, id, v)
                        && let Some(obj) = self.state.object(id)
                    {
                        out.push(to_view_with_known(id, obj, true));
                    }
                }
                out
            } else {
                Vec::new()
            };

            let top_of_library = p.library.last().copied().map(|top_oid| {
                if is_top_of_library_visible_to_cached(&self.state, p.id, viewer, eval_cache) {
                    let visibility =
                        if is_top_of_library_visible_to_cached(&self.state, p.id, None, eval_cache)
                        {
                            TopLibraryVisibility::RevealedToAll
                        } else if viewer.is_some_and(|v| is_known_to(&self.state, top_oid, v)) {
                            TopLibraryVisibility::KnownToViewer
                        } else {
                            TopLibraryVisibility::LookByController
                        };
                    TopOfLibraryView {
                        visibility,
                        top_card: self
                            .state
                            .object(top_oid)
                            .map(|obj| to_view_with_known(top_oid, obj, true)),
                    }
                } else {
                    TopOfLibraryView {
                        visibility: TopLibraryVisibility::Hidden,
                        top_card: None,
                    }
                }
            });

            let max_hand_size = if p.no_max_hand_size_rest_of_game {
                None
            } else {
                ep.and_then(|e| e.effective_max_hand_size(u16::from(config.max_hand_size)))
            };

            let restricted_mana_count: u16 = p
                .restricted_mana()
                .iter()
                .flat_map(|b| b.colors.iter())
                .fold(0u16, |acc, &c| {
                    acc.saturating_add(c.min(u32::from(u16::MAX)) as u16)
                });

            let hand = if viewer == Some(p.id) {
                let mut out = Vec::with_capacity(p.hand.len());
                for &id in &p.hand {
                    if let Some(obj) = self.state.object(id) {
                        out.push(to_view_with_known(id, obj, true));
                    }
                }
                out
            } else {
                Vec::new()
            };

            let mut battlefield = Vec::with_capacity(p.battlefield.len());
            for &id in &p.battlefield {
                if let Some(obj) = self.state.object(id)
                    && !obj.is_phased_out()
                {
                    battlefield.push(to_view_with_known(id, obj, false));
                }
            }

            let mut graveyard = Vec::with_capacity(p.graveyard.len());
            for &id in &p.graveyard {
                if let Some(obj) = self.state.object(id) {
                    graveyard.push(to_view_with_known(id, obj, false));
                }
            }

            let mut exile = Vec::with_capacity(p.exile.len());
            for &id in &p.exile {
                if let Some(obj) = self.state.object(id) {
                    exile.push(to_view_with_known(id, obj, false));
                }
            }

            let mut command = Vec::with_capacity(p.command.len());
            for &id in &p.command {
                if let Some(obj) = self.state.object(id) {
                    command.push(to_view_with_known(id, obj, false));
                }
            }

            players.push(PlayerView {
                id: p.id,
                life: p.life,
                lost: p.lost,
                poison: p.poison,
                energy: p.energy,
                library_count: p.library.len() as u16,
                hand_count: p.hand.len() as u16,
                hand,
                battlefield,
                graveyard,
                exile,
                command,
                experience: p.experience,
                rad: p.rad,
                mana_pool: p.mana_pool,
                snow_mana_pool: p.snow_mana_pool(),
                restricted_mana_count,
                max_hand_size,
                lands_played_this_turn: p.lands_played_this_turn,
                max_lands_per_turn: u16::from(p.max_lands_per_turn)
                    .saturating_add(ep.map_or(0, |e| e.extra_land_plays)),
                mulligan_count: p.mulligan_count,
                has_kept_hand: p.has_kept_hand,
                mulligan_bottom_remaining: self.state.mulligan_bottom_remaining(p.id),
                hand_revealed_to_viewer: viewer == Some(p.id) || hand_revealed,
                revealed_hand,
                top_of_library,
                commander_color_identity: p.commander_color_identity,
                commander_cast_counts: p.commander_cast_counts.clone(),
                commander_damage_taken: p.commander_damage_taken.clone(),
                cant_lose: ep.is_some_and(|e| e.cant_lose),
                dont_lose_at_zero_life: ep.is_some_and(|e| e.dont_lose_at_zero_life),
                life_cant_change: ep.is_some_and(|e| e.life_cant_change),
                hexproof: ep.is_some_and(|e| e.hexproof),
                shroud: ep.is_some_and(|e| e.shroud),
                prevent_all_combat_damage_this_turn: p.prevent_all_combat_damage_this_turn,
            });
        }

        let mut stack = Vec::with_capacity(self.state.stack_entries().len());
        for entry in self.state.stack_entries() {
            stack.push(match &entry.kind {
                StackEntryKind::Spell { object } => {
                    let obj = self.state.object(*object);
                    let face_down = obj.is_some_and(|o| o.is_face_down());
                    let known = viewer.is_some_and(|v| is_known_to(&self.state, *object, v));
                    let hide = face_down && !known;
                    StackItemView {
                        stack_id: entry.id,
                        kind: StackItemKindView::Spell,
                        source_object: Some(*object),
                        card_def: if hide {
                            TOKEN_CARD_DEF_ID
                        } else {
                            obj.map_or(TOKEN_CARD_DEF_ID, |o| o.card_def)
                        },
                        owner: obj.map_or(entry.controller, |o| o.owner),
                        controller: entry.controller,
                        face_index: if hide { 0 } else { entry.choices.face_index },
                        face_down,
                        known_to_viewer: !hide,
                        is_copy: entry.choices.is_copy,
                        x_value: if hide { None } else { Some(entry.x_value) },
                        chosen_modes: if hide {
                            SmallVec::new()
                        } else {
                            entry.choices.modes.clone()
                        },
                        targets: entry.targets.clone(),
                        paid_mana_colors: entry.paid_mana_colors,
                        cant_be_countered: entry.choices.cant_be_countered,
                        object_view: obj.map(|o| to_view_with_known(*object, o, false)),
                    }
                }
                StackEntryKind::Activated { source, lki, .. }
                | StackEntryKind::Triggered { source, lki, .. } => {
                    let kind = if matches!(entry.kind, StackEntryKind::Activated { .. }) {
                        StackItemKindView::ActivatedAbility
                    } else {
                        StackItemKindView::TriggeredAbility
                    };
                    let face_down = lki.face_down;
                    let known = viewer.is_some_and(|v| is_known_to(&self.state, *source, v));
                    let hide = face_down && !known;
                    StackItemView {
                        stack_id: entry.id,
                        kind,
                        source_object: Some(*source),
                        card_def: if hide {
                            TOKEN_CARD_DEF_ID
                        } else {
                            lki.card_def
                        },
                        owner: lki.owner,
                        controller: entry.controller,
                        face_index: if hide { 0 } else { lki.active_face_index },
                        face_down,
                        known_to_viewer: !hide,
                        is_copy: entry.choices.is_copy,
                        x_value: if hide { None } else { Some(entry.x_value) },
                        chosen_modes: if hide {
                            SmallVec::new()
                        } else {
                            entry.choices.modes.clone()
                        },
                        targets: entry.targets.clone(),
                        paid_mana_colors: entry.paid_mana_colors,
                        cant_be_countered: entry.choices.cant_be_countered,
                        object_view: None,
                    }
                }
            });
        }

        let combat_state = self.state.combat();
        let combat = if combat_state.active {
            let attackers = combat_state
                .attackers
                .iter()
                .filter(|a| {
                    self.state
                        .object(a.attacker)
                        .is_some_and(|o| !o.is_phased_out())
                })
                .map(|a| {
                    let controller = eval_cache.get(a.attacker).map_or_else(
                        || {
                            self.state
                                .object(a.attacker)
                                .map_or(self.state.active_player(), |o| o.controller)
                        },
                        |e| e.controller,
                    );
                    let blockers: Vec<ObjectId> = combat_state
                        .blockers
                        .iter()
                        .filter(|b| {
                            b.attacker == a.attacker
                                && self
                                    .state
                                    .object(b.blocker)
                                    .is_some_and(|o| !o.is_phased_out())
                        })
                        .map(|b| b.blocker)
                        .collect();
                    let was_blocked = !blockers.is_empty()
                        || combat_state.blocked_attackers.contains(&a.attacker);
                    let damage_assignment_order = combat_state
                        .damage_assignment_order
                        .iter()
                        .find(|(atk, _)| *atk == a.attacker)
                        .map(|(_, ord)| {
                            ord.iter()
                                .copied()
                                .filter(|&id| {
                                    self.state.object(id).is_some_and(|o| !o.is_phased_out())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    CombatAttackerView {
                        attacker: a.attacker,
                        controller,
                        target: a.target,
                        blockers,
                        was_blocked,
                        damage_assignment_order,
                    }
                })
                .collect();
            Some(CombatView {
                active: combat_state.active,
                attackers_declared: combat_state.attackers_declared,
                declared_blockers_players: combat_state.declared_blockers_players.clone(),
                first_strike_dealt: combat_state.first_strike_dealt,
                combat_damage_dealt: combat_state.combat_damage_dealt,
                attackers,
            })
        } else {
            None
        };

        let decision = current_decision_state(&self.state);
        let winner = match decision {
            DecisionState::GameOver { winner } => winner,
            _ => None,
        };

        ClientViewModel {
            viewer,
            turn_number: self.state.turn_number(),
            active_player: self.state.active_player(),
            priority_player: self.state.priority_player(),
            phase: self.state.phase(),
            decision,
            players,
            legal_actions: viewer.map(|v| self.legal_actions(v)).unwrap_or_default(),
            stack,
            combat,
            prompt: self.build_prompt_view(viewer),
            monarch: self.state.monarch(),
            initiative: self.state.initiative(),
            day_night: self.state.day_night(),
            passed_priority_mask: self.state.passed_priority_mask(),
            turn_serial: self.state.turn_serial(),
            winner,
        }
    }

    fn build_prompt_view(&self, viewer: Option<PlayerId>) -> Option<PromptView> {
        if let Some(pr) = self.state.pending_replacement() {
            let (purpose, is_public) = match &pr.prompt_kind {
                PendingReplacementPrompt::CommanderRedirect { .. } => {
                    (PromptPurposeView::CommanderZoneChoice, false)
                }
                PendingReplacementPrompt::ChooseReplacement => {
                    (PromptPurposeView::ChooseReplacement, true)
                }
                PendingReplacementPrompt::EtbCopy { .. } => {
                    (PromptPurposeView::ChooseReplacement, true)
                }
                PendingReplacementPrompt::EtbChoice { step, .. } => {
                    let public =
                        !matches!(step, EtbInteractiveStep::RevealFromHandOrEnterTapped { .. });
                    (PromptPurposeView::ChooseReplacement, public)
                }
                PendingReplacementPrompt::AbundanceChoice { .. } => {
                    (PromptPurposeView::ChooseReplacement, false)
                }
            };
            let raw_req = match self.state.decision() {
                DecisionState::Prompt { request } => request.clone(),
                _ => ChoiceRequest::ChooseReplacement {
                    prompt_id: pr.prompt_id,
                    option_count: pr.candidates.len() as u16,
                },
            };
            let is_redacted = !is_public && viewer != Some(pr.chooser);
            let request = if is_redacted {
                redact_choice_request(&raw_req)
            } else {
                raw_req
            };
            return Some(PromptView {
                prompt_id: pr.prompt_id,
                chooser: pr.chooser,
                purpose,
                is_redacted,
                request,
            });
        }

        if let Some(to) = self.state.pending_trigger_order() {
            let request = match self.state.decision() {
                DecisionState::Prompt { request } => request.clone(),
                _ => ChoiceRequest::OrderObjects {
                    prompt_id: to.prompt_id,
                    objects: to.object_ids.clone(),
                },
            };
            return Some(PromptView {
                prompt_id: to.prompt_id,
                chooser: to.controller,
                purpose: PromptPurposeView::TriggerOrder,
                is_redacted: false,
                request,
            });
        }

        let pp = self.state.pending_prompt()?;
        let (purpose, is_public) = match &pp.owner {
            PromptOwner::Turn(TurnPromptKind::CleanupDiscard { .. }) => {
                (PromptPurposeView::CleanupDiscard, false)
            }
            PromptOwner::Turn(_) => (PromptPurposeView::Generic, false),
            PromptOwner::Sba(SbaPromptKind::LegendRule { .. }) => {
                (PromptPurposeView::LegendRule, true)
            }
            PromptOwner::Sba(SbaPromptKind::CommanderReturn { .. }) => {
                (PromptPurposeView::CommanderZoneChoice, true)
            }
            PromptOwner::Casting(CastingPromptKind::CastModes { .. }) => {
                (PromptPurposeView::CastingChoice, true)
            }
            PromptOwner::Casting(CastingPromptKind::CastXValue { .. }) => {
                (PromptPurposeView::CastingChoice, true)
            }
            PromptOwner::Casting(CastingPromptKind::CostObjectSelection { kind, .. }) => (
                PromptPurposeView::CostPayment,
                is_public_cost_selection(*kind),
            ),
            PromptOwner::Casting(CastingPromptKind::GuidedFallback { .. }) => {
                (PromptPurposeView::GuidedResolution, false)
            }
            PromptOwner::Casting(_) => (PromptPurposeView::CastingChoice, false),
            PromptOwner::ManaAbility(_) => (PromptPurposeView::ManaChoice, false),
            PromptOwner::CostPayment(cp) => match &cp.kind {
                CostPromptKind::ObjectSelection { kind, .. } => (
                    PromptPurposeView::CostPayment,
                    is_public_cost_selection(*kind),
                ),
                _ => (PromptPurposeView::CostPayment, false),
            },
            PromptOwner::ResolutionFrame => {
                match self
                    .state
                    .resolution_vms()
                    .last()
                    .and_then(|vm| vm.pause_state.as_ref())
                {
                    Some(StepPauseState::SearchPick { .. }) => {
                        (PromptPurposeView::SearchLibrary, false)
                    }
                    Some(StepPauseState::ReorderLibrary { .. }) => {
                        (PromptPurposeView::ReorderLibrary, false)
                    }
                    Some(StepPauseState::DigPick { .. }) => (PromptPurposeView::DigPick, false),
                    Some(StepPauseState::GuidedResolution { .. }) => {
                        (PromptPurposeView::GuidedResolution, false)
                    }
                    Some(StepPauseState::SelectionPick { candidates, .. }) => {
                        let public = candidates
                            .iter()
                            .all(|&oid| is_object_visible_to(&self.state, oid, None));
                        (PromptPurposeView::ResolutionChoice, public)
                    }
                    _ => (PromptPurposeView::ResolutionChoice, false),
                }
            }
            PromptOwner::Combat(_) => (PromptPurposeView::Generic, true),
            PromptOwner::Replacement => (PromptPurposeView::ChooseReplacement, true),
            PromptOwner::TriggerOrder => (PromptPurposeView::TriggerOrder, true),
        };
        let is_redacted = !is_public && viewer != Some(pp.chooser);
        let request = if is_redacted {
            redact_choice_request(&pp.request)
        } else {
            pp.request.clone()
        };
        Some(PromptView {
            prompt_id: pp.prompt_id,
            chooser: pp.chooser,
            purpose,
            is_redacted,
            request,
        })
    }

    pub fn hover_breakdown(
        &self,
        viewer: Option<PlayerId>,
        object: ObjectId,
    ) -> Option<ContinuousBreakdown> {
        let obj = self.state.object(object)?;
        if !is_object_visible_to(&self.state, object, viewer) {
            return None;
        }
        let known = viewer.is_some_and(|v| is_known_to(&self.state, object, v));
        let redact_face_down =
            obj.is_face_down() && !known && matches!(obj.zone, Zone::Battlefield | Zone::Stack);
        crate::layers::inspect_continuous_breakdown(&self.state, object, redact_face_down)
    }

    pub fn preview_auto_tap(&self, player: PlayerId, req: &CastRequest) -> AutoTapPreview {
        let Some(obj) = self.state.object(req.object) else {
            return AutoTapPreview::default();
        };
        if (obj.zone == Zone::Hand
            && obj.owner != player
            && !is_known_to(&self.state, req.object, player))
            || (obj.zone == Zone::Library
                && !is_top_of_library_visible_to(&self.state, obj.owner, Some(player)))
        {
            return AutoTapPreview::default();
        }
        preview_auto_tap_for_request(&self.state, player, req)
    }

    pub fn observe(&self, viewer: PlayerId) -> Observation {
        observe_state(&self.state, viewer, self.legal_actions(viewer))
    }

    /// Determinizes hidden zones invisible to `viewer` for ISMCTS search.
    pub fn determinize(&self, viewer: PlayerId, rng: &mut RngState) -> GameState {
        determinize_state(&self.state, viewer, rng)
    }

    pub fn validate_deck(
        db: &CardDatabase,
        config: &GameConfig,
        deck: &DeckSpec,
    ) -> DeckValidationReport {
        if let Some(rules) = &config.commander {
            validate_commander_deck(db, rules, deck, config.allow_guided_fallback)
        } else {
            validate_standard_deck(db, deck, config.allow_guided_fallback)
        }
    }

    pub fn from_replay(db: Arc<CardDatabase>, log: ReplayLog) -> Result<Self, EngineError> {
        let mut session = Self::new(db, log.config.clone(), log.decks.clone(), log.seed)?;
        for entry in &log.entries {
            let out = session.apply_action(entry.actor, entry.action.clone())?;
            if out.event_seq != entry.seq {
                return Err(EngineError::Serialization(format!(
                    "replay event_seq mismatch: expected {}, got {}",
                    entry.seq, out.event_seq
                )));
            }
        }
        Ok(session)
    }

    pub fn snapshot_bytes(&self) -> Result<Vec<u8>, EngineError> {
        let snap = MatchSessionSnapshot {
            state: self.state.to_snapshot(),
            log: self.log.clone(),
        };
        let mut out = Vec::with_capacity(SNAPSHOT_MAGIC.len() + 256);
        out.extend_from_slice(SNAPSHOT_MAGIC);
        let payload =
            postcard::to_allocvec(&snap).map_err(|e| EngineError::Serialization(e.to_string()))?;
        out.extend_from_slice(&payload);
        Ok(out)
    }

    pub fn from_snapshot(db: Arc<CardDatabase>, bytes: &[u8]) -> Result<Self, EngineError> {
        if bytes.len() < SNAPSHOT_MAGIC.len() || &bytes[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
            return Err(EngineError::Serialization(
                "invalid snapshot magic header".into(),
            ));
        }
        let payload = &bytes[SNAPSHOT_MAGIC.len()..];
        let (snap, rest): (MatchSessionSnapshot, &[u8]) = postcard::take_from_bytes(payload)
            .map_err(|e| EngineError::Serialization(e.to_string()))?;
        if !rest.is_empty() {
            return Err(EngineError::Serialization(format!(
                "trailing {} bytes after snapshot payload",
                rest.len()
            )));
        }
        let state = GameState::from_snapshot(db, snap.state);
        Ok(Self {
            state,
            log: snap.log,
        })
    }
}

#[inline]
fn is_infallible_nonfinal_pass(state: &GameState, player: PlayerId, action: &Action) -> bool {
    if !matches!(action, Action::PassPriority)
        || is_prompt_active(state)
        || state.casting().is_some()
        || !matches!(state.decision(), DecisionState::Priority)
        || state.priority_player() != player
        || player.0 >= 8
    {
        return false;
    }
    let mut alive = 0u8;
    for p in state.players() {
        if !p.lost && p.id.0 < 8 {
            alive |= 1u8 << p.id.0;
        }
    }
    let next_passed = state.passed_priority_mask() | (1u8 << player.0);
    (next_passed & alive) != alive
}

#[inline]
fn is_public_cost_selection(kind: CostSelectionKind) -> bool {
    matches!(
        kind,
        CostSelectionKind::Sacrifice
            | CostSelectionKind::ExileFromGraveyard
            | CostSelectionKind::TapUntapped
            | CostSelectionKind::RemoveCounters
            | CostSelectionKind::Convoke
            | CostSelectionKind::Improvise
            | CostSelectionKind::Delve
            | CostSelectionKind::Emerge
            | CostSelectionKind::Waterbend
    )
}

fn redact_choice_request(req: &ChoiceRequest) -> ChoiceRequest {
    match req {
        ChoiceRequest::YesNo { prompt_id, .. } => ChoiceRequest::YesNo {
            prompt_id: *prompt_id,
            label: String::new(),
        },
        ChoiceRequest::ChooseObjects {
            prompt_id,
            min,
            max,
            ..
        } => ChoiceRequest::ChooseObjects {
            prompt_id: *prompt_id,
            candidates: Vec::new(),
            min: *min,
            max: *max,
        },
        ChoiceRequest::OrderObjects { prompt_id, .. } => ChoiceRequest::OrderObjects {
            prompt_id: *prompt_id,
            objects: Vec::new(),
        },
        ChoiceRequest::GuidedResolution { prompt_id, .. } => ChoiceRequest::GuidedResolution {
            prompt_id: *prompt_id,
            source: NONE_OBJECT_ID,
            text: String::new(),
        },
        ChoiceRequest::ChooseMode { .. }
        | ChoiceRequest::ChooseNumber { .. }
        | ChoiceRequest::ChooseReplacement { .. } => req.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{AttackTarget, TargetRef};
    use crate::state::{FaceDownKind, FaceDownSpec};
    use crate::zones::{
        CreateTokenOptions, MoveRawOptions, create_token_raw, exile_face_down, move_object_raw,
        phase_out_permanent,
    };
    use mtg_ir::{
        AbilityDef, ActivatedAbility, CardDef, CardDefId, CardLayout, Characteristics, CostDef,
        EffectOp, EffectStep, FaceDef, GamePhase, IR_V2_VERSION, InternTables, IrV2Bundle,
        ManaColor, ManaCost, ManaProduction, ObjectFilter, PlayerRef, PrintedPt, RelativeControl,
        SearchDestination, SearchSpec, Selector, Span, SpellAbility, StaticAbility, StaticBody,
        StaticDef, SubtypeId, SupertypeMask, Timing, TokenSpec, ZoneMask,
    };

    fn session_test_db() -> Arc<CardDatabase> {
        let cards = vec![
            CardDef {
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
            },
            CardDef {
                oracle_id: "synth_unsup".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Synth Unsup".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Unsup".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{0}"),
                        types: CardTypeMask::SORCERY,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
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
                fully_supported: false,
            },
        ];
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Soldier".into()],
                    counters: vec![],
                    keywords: vec![],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn synth_gameplay_db() -> Arc<CardDatabase> {
        let synth_forest = CardDef {
            oracle_id: "synth_forest".into(),
            compiler_version: 46,
            text_hash: "f".into(),
            name: "Synth Forest".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Forest".into(),
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
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "mana_tap".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        op: EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: smallvec::smallvec![ManaColor::G],
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        subject: Selector::None,
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Span::default(),
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
                    intrinsic: true,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let synth_cub = CardDef {
            oracle_id: "synth_cub".into(),
            compiler_version: 46,
            text_hash: "c".into(),
            name: "Synth Cub".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Cub".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
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
        let synth_hand_spy = CardDef {
            oracle_id: "synth_hand_spy".into(),
            compiler_version: 46,
            text_hash: "h".into(),
            name: "Synth Hand Spy".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Hand Spy".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::HandVisibility {
                            players: RelativeControl::Opponents,
                        },
                        condition: None,
                    },
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let synth_lens = CardDef {
            oracle_id: "synth_lens".into(),
            compiler_version: 46,
            text_hash: "l".into(),
            name: "Synth Lens".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lens".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::TopOfLibraryVisibility {
                            reveal_to_all: false,
                            all_players: false,
                        },
                        condition: None,
                    },
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let synth_tutor = CardDef {
            oracle_id: "synth_tutor".into(),
            compiler_version: 46,
            text_hash: "t".into(),
            name: "Synth Tutor".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Tutor".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![EffectStep {
                        op: EffectOp::Search {
                            spec: Box::new(SearchSpec {
                                filter: Box::new(ObjectFilter::default()),
                                count: 1,
                                up_to: false,
                                stated_quality: false,
                                destinations: vec![SearchDestination {
                                    zone: Zone::Hand,
                                    tapped: false,
                                    on_top: false,
                                }],
                                reveal: false,
                                description: None,
                                zones: ZoneMask::LIBRARY,
                                different_names: false,
                                shuffle_only_if_library: false,
                                searched_player: None,
                                cast_free: false,
                                target_controller_as_actor: false,
                            }),
                            tapped: false,
                        },
                        subject: Selector::PlayerRef {
                            player: PlayerRef::You,
                        },
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Span::default(),
                    }],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
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
                tables: InternTables::default(),
                cards: vec![
                    synth_forest,
                    synth_cub,
                    synth_hand_spy,
                    synth_lens,
                    synth_tutor,
                ],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn decision_info_snapshot_and_replay_match_contract() {
        let db = session_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 60],
            sideboard: vec![],
        };
        let mut session = MatchSession::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            1,
        )
        .unwrap();

        let info = session.decision_info();
        assert_eq!(info.actor, Some(PlayerId(0)));
        assert_eq!(info.state_kind, DecisionKind::Mulligan);
        assert_eq!(info.legal_count, 3);

        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        assert_eq!(session.decision_info().actor, Some(PlayerId(1)));
        assert_eq!(session.decision_info().state_kind, DecisionKind::Mulligan);

        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();
        let info2 = session.decision_info();
        assert_eq!(info2.actor, Some(PlayerId(0)));
        assert_eq!(info2.state_kind, DecisionKind::Priority);
        assert_eq!(info2.legal_count, 2);

        let snap = session.snapshot_bytes().unwrap();
        assert!(snap.starts_with(b"MTGS"));
        let mut from_snap = MatchSession::from_snapshot(Arc::clone(&db), &snap).unwrap();
        let mut from_rep =
            MatchSession::from_replay(Arc::clone(&db), session.replay_log().clone()).unwrap();
        assert_eq!(session.view_model(None), from_snap.view_model(None));
        assert_eq!(session.view_model(None), from_rep.view_model(None));

        let out_orig = session
            .apply_action(PlayerId(0), Action::PassPriority)
            .unwrap();
        let out_snap = from_snap
            .apply_action(PlayerId(0), Action::PassPriority)
            .unwrap();
        let out_rep = from_rep
            .apply_action(PlayerId(0), Action::PassPriority)
            .unwrap();
        assert_eq!(out_orig, out_snap);
        assert_eq!(out_orig, out_rep);
        assert_eq!(
            session.snapshot_bytes().unwrap(),
            from_snap.snapshot_bytes().unwrap()
        );

        assert!(matches!(
            MatchSession::from_snapshot(Arc::clone(&db), b"corrupt_snapshot_bytes"),
            Err(EngineError::Serialization(_))
        ));
        let mut bad_magic = snap.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            MatchSession::from_snapshot(Arc::clone(&db), &bad_magic),
            Err(EngineError::Serialization(_))
        ));
        let mut trailing = snap;
        trailing.push(0xAA);
        assert!(matches!(
            MatchSession::from_snapshot(db, &trailing),
            Err(EngineError::Serialization(_))
        ));
    }

    #[test]
    fn validate_deck_deduplicates_unsupported_and_respects_guided_fallback() {
        let db = session_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 60],
            sideboard: vec![],
        };

        let strict_rep = MatchSession::validate_deck(&db, &GameConfig::standard_1v1(), &deck);
        assert!(!strict_rep.is_legal);
        assert_eq!(strict_rep.unsupported_card_ids, vec![CardDefId(1)]);

        let mut guided_cfg = GameConfig::standard_1v1();
        guided_cfg.allow_guided_fallback = true;
        let guided_rep = MatchSession::validate_deck(&db, &guided_cfg, &deck);
        assert!(guided_rep.is_legal);
        assert_eq!(guided_rep.unsupported_card_ids, vec![CardDefId(1)]);
    }

    #[test]
    fn view_model_shows_tokens_hides_phased_out_and_redacts_face_down_objects() {
        let db = session_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut session =
            MatchSession::new(db, GameConfig::standard_1v1(), vec![deck.clone(), deck], 12)
                .unwrap();

        let p0_hand_0 = session.state().players()[0].hand[0];
        let p0_hand_1 = session.state().players()[0].hand[1];
        let p0_hand_2 = session.state().players()[0].hand[2];

        // 1) Token on battlefield shows TokenSpec stats.
        let token_id = create_token_raw(
            &mut session.state,
            PlayerId(0),
            TokenSpec {
                name: "Synth Soldier".into(),
                power: 1,
                toughness: 1,
                colors: ColorMask::W,
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: smallvec::smallvec![SubtypeId(0)],
                type_line: "Creature — Soldier".into(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: vec![],
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();

        // 2) Face-down permanent on battlefield is visible to controller, redacted for opponent.
        let fd_bf = move_object_raw(
            &mut session.state,
            p0_hand_0,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    opaque_seq: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // 3) Face-down exiled card is redacted from unauthorized opponent.
        let fd_exile = exile_face_down(
            &mut session.state,
            p0_hand_1,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                opaque_seq: 2,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // 4) Phased-out creature is hidden from battlefield view_model and hover_breakdown.
        let phased_bf = move_object_raw(
            &mut session.state,
            p0_hand_2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(phase_out_permanent(&mut session.state, phased_bf));

        let vm_p0 = session.view_model(Some(PlayerId(0)));
        let vm_p1 = session.view_model(Some(PlayerId(1)));

        assert!(
            !vm_p0.players[0]
                .battlefield
                .iter()
                .any(|v| v.id == phased_bf)
        );
        assert!(
            !vm_p1.players[0]
                .battlefield
                .iter()
                .any(|v| v.id == phased_bf)
        );
        assert!(
            session
                .hover_breakdown(Some(PlayerId(0)), phased_bf)
                .is_none()
        );

        let tok_view = vm_p1.players[0]
            .battlefield
            .iter()
            .find(|v| v.id == token_id)
            .unwrap();
        assert_eq!(tok_view.card_def, TOKEN_CARD_DEF_ID);
        assert_eq!(tok_view.colors, ColorMask::W);
        assert_eq!(tok_view.types, CardTypeMask::CREATURE);
        assert_eq!(
            tok_view.pt,
            Some(crate::view::PowerToughness {
                power: 1,
                toughness: 1
            })
        );

        let fd_p0 = vm_p0.players[0]
            .battlefield
            .iter()
            .find(|v| v.id == fd_bf)
            .unwrap();
        let fd_p1 = vm_p1.players[0]
            .battlefield
            .iter()
            .find(|v| v.id == fd_bf)
            .unwrap();
        assert_eq!(fd_p0.card_def, CardDefId(0));
        assert_eq!(fd_p1.card_def, TOKEN_CARD_DEF_ID);
        assert_eq!(fd_p1.colors, ColorMask::empty());
        assert_eq!(fd_p1.types, CardTypeMask::CREATURE);
        assert_eq!(
            fd_p1.pt,
            Some(crate::view::PowerToughness {
                power: 2,
                toughness: 2
            })
        );

        // Hover breakdown on hidden zones & face-down objects.
        let p1_hand_card = session.state().players()[1].hand[0];
        let p0_lib_card = session.state().players()[0].library[0];
        assert!(
            session
                .hover_breakdown(Some(PlayerId(0)), p1_hand_card)
                .is_none()
        );
        assert!(session.hover_breakdown(None, p0_lib_card).is_none());
        assert!(
            session
                .hover_breakdown(Some(PlayerId(1)), fd_exile)
                .is_none()
        );
        let fd_bd = session.hover_breakdown(Some(PlayerId(1)), fd_bf).unwrap();
        assert!(fd_bd.notes.iter().all(|n| !n.starts_with("Layer 3:")));

        let ex_p1 = vm_p1.players[0]
            .exile
            .iter()
            .find(|v| v.id == fd_exile)
            .unwrap();
        assert_eq!(ex_p1.card_def, TOKEN_CARD_DEF_ID);
        assert_eq!(ex_p1.types, CardTypeMask::empty());
        assert_eq!(ex_p1.colors, ColorMask::empty());
        assert_eq!(ex_p1.pt, None);
    }

    #[test]
    fn view_model_unredacts_exiled_face_down_card_moved_to_graveyard() {
        let db = session_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut session =
            MatchSession::new(db, GameConfig::standard_1v1(), vec![deck.clone(), deck], 21)
                .unwrap();

        let p0_hand_0 = session.state().players()[0].hand[0];
        let ex_id = exile_face_down(
            &mut session.state,
            p0_hand_0,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                opaque_seq: 1,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let vm_exile = session.view_model(Some(PlayerId(1)));
        let ex_view = vm_exile.players[0]
            .exile
            .iter()
            .find(|v| v.id == ex_id)
            .unwrap();
        assert_eq!(ex_view.card_def, TOKEN_CARD_DEF_ID);

        let gy_id = move_object_raw(
            &mut session.state,
            ex_id,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let vm_gy = session.view_model(Some(PlayerId(1)));
        let gy_view = vm_gy.players[0]
            .graveyard
            .iter()
            .find(|v| v.id == gy_id)
            .unwrap();
        assert_eq!(gy_view.card_def, CardDefId(0));
        assert_eq!(gy_view.colors, ColorMask::G);
        assert_eq!(gy_view.types, CardTypeMask::CREATURE);
        assert_eq!(
            gy_view.pt,
            Some(crate::view::PowerToughness {
                power: 2,
                toughness: 2
            })
        );
    }

    #[test]
    fn macro_action_london_bottom_cast_combat_and_atomic_rollback() {
        let db = synth_gameplay_db();
        // Deck of 30 Forests + 30 Cubs (CardDefId(0) & CardDefId(1))
        let mut main = vec![CardDefId(0); 30];
        main.extend(vec![CardDefId(1); 30]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: main,
            sideboard: vec![],
        };
        let mut session = MatchSession::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            77,
        )
        .unwrap();

        // P0 takes 1 mulligan, then both P0 and P1 keep hands before CR 103.5 Stage 2 bottoming.
        session
            .apply_action(PlayerId(0), Action::TakeMulligan)
            .unwrap();
        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        assert_eq!(session.current_actor(), Some(PlayerId(1)));
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();
        assert_eq!(session.current_actor(), Some(PlayerId(0)));
        assert_eq!(session.state().mulligan_bottom_remaining(PlayerId(0)), 1);

        let wrong_bottom_rev = session.state().layer_rev();
        let wrong_log_len = session.replay_log().entries.len();
        let err_bottom = session.apply_macro_action(
            PlayerId(0),
            MacroAction::LondonBottom {
                objects: vec![], // needs 1 card!
            },
        );
        assert!(matches!(err_bottom, Err(EngineError::IllegalAction(_))));
        assert_eq!(session.state().layer_rev(), wrong_rev_or(wrong_bottom_rev));
        assert_eq!(session.replay_log().entries.len(), wrong_log_len);
        assert_eq!(session.state().mulligan_bottom_remaining(PlayerId(0)), 1);
        assert_eq!(session.state().players()[0].hand.len(), 7);

        let bottom_card = session.state().players()[0].hand[0];
        session
            .apply_macro_action(
                PlayerId(0),
                MacroAction::LondonBottom {
                    objects: vec![bottom_card],
                },
            )
            .unwrap();
        assert_eq!(session.state().players()[0].hand.len(), 6);
        assert_eq!(session.current_actor(), Some(PlayerId(0)));

        // Find a Forest and a Cub in P0's hand.
        let forest_h = session.state().players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let cub_h = session.state().players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(1))
            .unwrap();

        // Casting Cub with 0 mana fails and rolls back completely.
        let rev0 = session.state().layer_rev();
        let seq0 = session.state().event_seq();
        let log0 = session.replay_log().entries.len();
        assert!(matches!(
            session.apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: cub_h,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![],
                }
            ),
            Err(EngineError::IllegalAction(_))
        ));
        assert_eq!(session.state().layer_rev(), rev0);
        assert_eq!(session.state().event_seq(), seq0);
        assert_eq!(session.replay_log().entries.len(), log0);

        // Preview auto-tap before playing land -> cannot pay; after playing land -> can pay with Forest.
        let prev_no_land = session.preview_auto_tap(
            PlayerId(0),
            &CastRequest {
                object: cub_h,
                option_id: 0,
                x_value: None,
            },
        );
        assert!(!prev_no_land.can_pay);

        session
            .apply_action(
                PlayerId(0),
                Action::PlayLand {
                    object: forest_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        let forest_bf = session.state().players()[0].battlefield[0];
        let prev_with_land = session.preview_auto_tap(
            PlayerId(0),
            &CastRequest {
                object: cub_h,
                option_id: 0,
                x_value: None,
            },
        );
        assert!(prev_with_land.can_pay);
        assert_eq!(prev_with_land.sources_to_tap.as_slice(), &[forest_bf]);

        // Opponent cannot preview P0's hidden hand card.
        let opp_prev = session.preview_auto_tap(
            PlayerId(1),
            &CastRequest {
                object: cub_h,
                option_id: 0,
                x_value: None,
            },
        );
        assert!(!opp_prev.can_pay);

        // Cast Cub via MacroAction::CastSpell -> auto-taps Forest, puts Cub on stack.
        session
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: cub_h,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap();
        assert_eq!(session.state().stack().len(), 1);
        let vm_stack = session.view_model(Some(PlayerId(1)));
        assert_eq!(vm_stack.stack.len(), 1);
        assert_eq!(vm_stack.stack[0].card_def, CardDefId(1));

        // Resolve Cub, advance to P0's next turn so Cub loses summoning sickness, then attack via MacroAction.
        while !(session.state().turn_number() == 3
            && session.state().phase() == GamePhase::DeclareAttackers)
        {
            let actor = session.current_actor().unwrap();
            let dec = current_decision_state(session.state());
            let act = match dec {
                DecisionState::DeclareAttackers => Action::ConfirmAttackers,
                DecisionState::DeclareBlockers => Action::ConfirmBlockers,
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } => {
                    let answer = if let Some(&id) = candidates.first() {
                        ChoiceAnswer::PickObject { id }
                    } else {
                        ChoiceAnswer::DonePicking
                    };
                    Action::Answer { prompt_id, answer }
                }
                _ => Action::PassPriority,
            };
            session.apply_action(actor, act).unwrap();
        }

        let cub_bf = session.state().players()[0]
            .battlefield
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        session
            .apply_macro_action(
                PlayerId(0),
                MacroAction::DeclareAttackers {
                    assignments: vec![(cub_bf, AttackTarget::Player { id: PlayerId(1) })],
                },
            )
            .unwrap();
        assert_eq!(session.decision_info().state_kind, DecisionKind::Priority);
        let vm_combat = session.view_model(None);
        assert!(
            vm_combat
                .combat
                .as_ref()
                .is_some_and(|c| c.active && c.attackers.len() == 1)
        );
        session
            .apply_action(PlayerId(0), Action::PassPriority)
            .unwrap();
        session
            .apply_action(PlayerId(1), Action::PassPriority)
            .unwrap();
        assert_eq!(
            session.decision_info().state_kind,
            DecisionKind::DeclareBlockers
        );
        session
            .apply_macro_action(
                PlayerId(1),
                MacroAction::DeclareBlockers {
                    assignments: vec![],
                },
            )
            .unwrap();

        // Replay log must reproduce the exact final state.
        let replayed = MatchSession::from_replay(db, session.replay_log().clone()).unwrap();
        assert_eq!(session.view_model(None), replayed.view_model(None));
    }

    #[inline]
    fn wrong_rev_or(r: u64) -> u64 {
        r
    }

    #[test]
    fn view_model_visibility_statics_and_prompt_redaction_and_4p_concede() {
        let db = synth_gameplay_db();
        // Put HandSpy (2), Lens (3), Tutor (4), Cub (1), Forest (0) in deck.
        let mut main = vec![CardDefId(0); 20];
        main.extend(vec![CardDefId(1); 10]);
        main.extend(vec![CardDefId(2); 10]);
        main.extend(vec![CardDefId(3); 10]);
        main.extend(vec![CardDefId(4); 10]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: main,
            sideboard: vec![],
        };
        let mut session = MatchSession::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck.clone()],
            99,
        )
        .unwrap();
        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        // Put Synth Hand Spy, Synth Lens, and a Forest onto P0's battlefield (find in hand or library).
        let find_p0 = |s: &GameState, def: CardDefId| -> ObjectId {
            s.players()[0]
                .hand
                .iter()
                .chain(s.players()[0].library.iter())
                .copied()
                .find(|&id| s.object(id).unwrap().card_def == def)
                .unwrap()
        };
        let spy_id = find_p0(session.state(), CardDefId(2));
        move_object_raw(
            &mut session.state,
            spy_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let lens_id = find_p0(session.state(), CardDefId(3));
        move_object_raw(
            &mut session.state,
            lens_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let forest_id = find_p0(session.state(), CardDefId(0));
        move_object_raw(
            &mut session.state,
            forest_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let vm_p0 = session.view_model(Some(PlayerId(0)));
        let vm_spec = session.view_model(None);
        // E2E redaction: `PlayerView.hand` is ALWAYS empty for opponents & spectators,
        // while `revealed_hand` exposes P1's hand to P0 because of HandVisibility.
        assert!(vm_p0.players[1].hand.is_empty());
        assert_eq!(
            vm_p0.players[1].revealed_hand.len(),
            vm_p0.players[1].hand_count as usize
        );
        assert!(vm_spec.players[0].hand.is_empty());
        assert!(vm_spec.players[0].revealed_hand.is_empty());
        assert!(vm_spec.players[1].hand.is_empty());
        assert_eq!(
            vm_spec.players[1].revealed_hand.len(),
            vm_spec.players[1].hand_count as usize
        );

        // TopOfLibraryVisibility { reveal_to_all: false }: P0 sees P0's top card, spectator & P1 do not.
        let p0_top_view = vm_p0.players[0].top_of_library.as_ref().unwrap();
        assert_eq!(
            p0_top_view.visibility,
            TopLibraryVisibility::LookByController
        );
        assert!(p0_top_view.top_card.is_some());
        let p0_top_id = session.state().players()[0]
            .library
            .last()
            .copied()
            .unwrap();
        assert!(
            session
                .hover_breakdown(Some(PlayerId(0)), p0_top_id)
                .is_some()
        );
        assert!(
            session
                .hover_breakdown(Some(PlayerId(1)), p0_top_id)
                .is_none()
        );
        let spec_top_view = vm_spec.players[0].top_of_library.as_ref().unwrap();
        assert_eq!(spec_top_view.visibility, TopLibraryVisibility::Hidden);
        assert!(spec_top_view.top_card.is_none());

        // CR 401.5: begin casting {G} Synth Cub (pauses in CastingSequence::PayCosts),
        // verify top of library is hidden while casting, then CancelCast.
        let cub_h = if let Some(id) = session.state().players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(1))
        {
            id
        } else {
            let lib_cub = find_p0(session.state(), CardDefId(1));
            move_object_raw(
                &mut session.state,
                lib_cub,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap()
        };
        session
            .apply_action(
                PlayerId(0),
                Action::BeginCast {
                    object: cub_h,
                    option_id: 0,
                    from_zone: Zone::Hand,
                },
            )
            .unwrap();
        let vm_while_casting = session.view_model(Some(PlayerId(0)));
        assert_eq!(
            vm_while_casting.players[0]
                .top_of_library
                .as_ref()
                .unwrap()
                .visibility,
            TopLibraryVisibility::Hidden
        );
        assert!(
            session
                .hover_breakdown(Some(PlayerId(0)), p0_top_id)
                .is_none()
        );
        session
            .apply_action(PlayerId(0), Action::CancelCast)
            .unwrap();

        // Cast Synth Tutor ({0}) -> SearchLibrary prompt must be unredacted for P0 and redacted for P1/spectator.
        let tutor_h = if let Some(id) = session.state().players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| session.state().object(id).unwrap().card_def == CardDefId(4))
        {
            id
        } else {
            let lib_tutor = find_p0(session.state(), CardDefId(4));
            move_object_raw(
                &mut session.state,
                lib_tutor,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap()
        };
        session
            .apply_action(
                PlayerId(0),
                Action::BeginCast {
                    object: tutor_h,
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

        let vm_prompt_p0 = session.view_model(Some(PlayerId(0)));
        let vm_prompt_p1 = session.view_model(Some(PlayerId(1)));
        let prompt_p0 = vm_prompt_p0.prompt.as_ref().unwrap();
        let prompt_p1 = vm_prompt_p1.prompt.as_ref().unwrap();
        assert_eq!(prompt_p0.purpose, PromptPurposeView::SearchLibrary);
        assert!(!prompt_p0.is_redacted);
        assert!(prompt_p1.is_redacted);
        if let ChoiceRequest::ChooseObjects { candidates, .. } = &prompt_p1.request {
            assert!(candidates.is_empty());
        } else {
            panic!("expected redacted ChooseObjects prompt");
        }

        // 4-player Commander Concede clears conceded player's battlefield & advances priority.
        let mut cmd_cfg = GameConfig::commander(4);
        cmd_cfg.commander = None; // free-for-all 4p with 60-card synth deck
        let mut s4 = MatchSession::new(
            db,
            cmd_cfg,
            vec![deck.clone(), deck.clone(), deck.clone(), deck],
            55,
        )
        .unwrap();
        for seat in 0..4u8 {
            s4.apply_action(PlayerId(seat), Action::KeepHand).unwrap();
        }
        let p2_card = s4.state().players()[2].hand[0];
        let _ = move_object_raw(
            &mut s4.state,
            p2_card,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        s4.apply_action(PlayerId(0), Action::PassPriority).unwrap();
        s4.apply_action(PlayerId(1), Action::PassPriority).unwrap();
        assert_eq!(s4.current_actor(), Some(PlayerId(2)));
        s4.apply_action(PlayerId(2), Action::Concede).unwrap();
        let vm4 = s4.view_model(None);
        assert!(vm4.players[2].lost);
        assert!(vm4.players[2].battlefield.is_empty());
        assert_eq!(s4.current_actor(), Some(PlayerId(3)));
    }

    fn synth_remediation_db() -> Arc<CardDatabase> {
        let synth_step = |op: EffectOp, subject: Selector| EffectStep {
            op,
            subject,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        };
        let simple_player_target = |rel: mtg_ir::PlayerRelation| mtg_ir::TargetSpec {
            kind: mtg_ir::TargetKind::Player,
            filter: ObjectFilter::default(),
            player: Some(mtg_ir::PlayerFilter { relation: rel }),
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
            zones: ZoneMask::empty(),
        };

        // CardDefId(0): Synth Forest
        let synth_forest = CardDef {
            oracle_id: "synth_r_forest".into(),
            compiler_version: 46,
            text_hash: "rf".into(),
            name: "Synth Forest".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Forest".into(),
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
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "mana_tap".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![synth_step(
                        EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: smallvec::smallvec![ManaColor::G],
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        Selector::None,
                    )],
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
                    intrinsic: true,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(1): Synth Cub ({G}, 2/2)
        let synth_cub = CardDef {
            oracle_id: "synth_r_cub".into(),
            compiler_version: 46,
            text_hash: "rc".into(),
            name: "Synth Cub".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Cub".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
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

        // CardDefId(2): Synth Multi-Blocker (3/3, blocks 1 additional creature)
        let synth_guard = CardDef {
            oracle_id: "synth_r_guard".into(),
            compiler_version: 46,
            text_hash: "rg".into(),
            name: "Synth Multi Guard".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Multi Guard".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 3,
                        toughness: 3,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::CombatRule {
                            affected: mtg_ir::Affected::SelfObj,
                            rule: mtg_ir::CombatRuleSpec::AdditionalBlockCount { count: Some(1) },
                        },
                        condition: None,
                    },
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(3): Synth Discard ({0}, target opponent discards 1)
        let mut discard_step = synth_step(
            EffectOp::Discard {
                select: mtg_ir::SelectionMod {
                    choice_count: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
                    ..Default::default()
                },
            },
            Selector::Target { index: 0 },
        );
        discard_step.actor = Some(PlayerRef::Target { index: 0 });
        let synth_discard = CardDef {
            oracle_id: "synth_r_discard".into(),
            compiler_version: 46,
            text_hash: "rd".into(),
            name: "Synth Discard".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Discard".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![simple_player_target(mtg_ir::PlayerRelation::Opponent)],
                    effect: vec![discard_step],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(4): Synth Unless Draw ({0}, draw 1 unless target opponent pays {2})
        let mut unless_step = synth_step(
            EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
            Selector::PlayerRef {
                player: PlayerRef::You,
            },
        );
        unless_step.amount = Some(Box::new(mtg_ir::Amount::Const { n: 1 }));
        unless_step.optional = Some(mtg_ir::OptionalSpec {
            chooser: PlayerRef::Target { index: 0 },
            cost: vec![CostDef::Mana {
                cost: ManaCost::parse("{2}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            mode: mtg_ir::OptionalMode::DoUnlessPaid,
        });
        let synth_unless = CardDef {
            oracle_id: "synth_r_unless".into(),
            compiler_version: 46,
            text_hash: "ru".into(),
            name: "Synth Unless Draw".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Unless Draw".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![simple_player_target(mtg_ir::PlayerRelation::Opponent)],
                    effect: vec![unless_step],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(5): Synth UpToDraw EachOpponent ({0}, each opponent may draw up to 2)
        let mut up_to_draw_step = synth_step(
            EffectOp::Draw {
                up_to: true,
                reveal_drawn: false,
            },
            Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::EachOpponent,
                },
            },
        );
        up_to_draw_step.amount = Some(Box::new(mtg_ir::Amount::Const { n: 2 }));
        let synth_up_to_draw = CardDef {
            oracle_id: "synth_r_uptodraw".into(),
            compiler_version: 46,
            text_hash: "rud".into(),
            name: "Synth Up To Draw".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Up To Draw".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![up_to_draw_step],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(6): Synth ForEachLoop Discard ({0}, each opponent discards 1)
        let mut inner_disc = synth_step(
            EffectOp::Discard {
                select: mtg_ir::SelectionMod {
                    choice_count: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
                    ..Default::default()
                },
            },
            Selector::PlayerRef {
                player: PlayerRef::EachLoopPlayer,
            },
        );
        inner_disc.actor = Some(PlayerRef::EachLoopPlayer);
        let loop_step = synth_step(
            EffectOp::ForEachPlayerLoop {
                players: mtg_ir::LoopPlayersKind::EachOpponent,
                steps: vec![inner_disc],
                choose_random_opponent: false,
                exclude_event_player: false,
                exclude_attacked_last_combat: false,
                choose_object_to_bind: None,
            },
            Selector::None,
        );
        let synth_foreach = CardDef {
            oracle_id: "synth_r_foreach".into(),
            compiler_version: 46,
            text_hash: "rfe".into(),
            name: "Synth ForEach Discard".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth ForEach Discard".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![loop_step],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(7): Synth Phase2 Spell ({0}, Phase 2 NotImplemented op)
        let synth_phase2 = CardDef {
            oracle_id: "synth_r_phase2".into(),
            compiler_version: 46,
            text_hash: "rp2".into(),
            name: "Synth Phase2".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Phase2".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![synth_step(EffectOp::SolveCase, Selector::None)],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(8): Synth Future Sight ({0}, look at & play spells from top of library)
        let synth_future = CardDef {
            oracle_id: "synth_r_future".into(),
            compiler_version: 46,
            text_hash: "rfu".into(),
            name: "Synth Top Sight".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Top Sight".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
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
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::TopOfLibraryVisibility {
                                reveal_to_all: false,
                                all_players: false,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::ZonePlayPermission {
                                zone: Zone::Library,
                                allow_lands: false,
                                allow_spells: true,
                                spell_filter: None,
                                self_only: false,
                                during_your_turn_only: false,
                                slot_mode: None,
                                exile_if_put_into_graveyard: false,
                                exiled_with_self: false,
                                require_counter: None,
                                card_owner: None,
                                spend_mana_as_any_color: false,
                                enters_tapped: false,
                                grant_haste_until_eot: false,
                                put_into_graveyard_from_non_battlefield_this_turn: false,
                                alt_cost_defs: vec![],
                                grant_flash: false,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }),
                ],
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
                tables: InternTables::default(),
                cards: vec![
                    synth_forest,
                    synth_cub,
                    synth_guard,
                    synth_discard,
                    synth_unless,
                    synth_up_to_draw,
                    synth_foreach,
                    synth_phase2,
                    synth_future,
                ],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn start_4p_session(db: Arc<CardDatabase>, seed: u64) -> MatchSession {
        let mut cfg = GameConfig::commander(4);
        cfg.commander = None;
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 60],
            sideboard: vec![],
        };
        let mut s = MatchSession::new(
            db,
            cfg,
            vec![deck.clone(), deck.clone(), deck.clone(), deck],
            seed,
        )
        .unwrap();
        for seat in 0..4u8 {
            s.apply_action(PlayerId(seat), Action::KeepHand).unwrap();
        }
        s
    }

    fn pass_all_alive(s: &mut MatchSession) {
        let mut guard = 0usize;
        let start_phase = s.state().phase();
        while matches!(s.state().decision(), DecisionState::Priority)
            && s.state().phase() == start_phase
        {
            guard += 1;
            assert!(guard < 32, "stuck passing priority");
            let actor = s.current_actor().unwrap();
            s.apply_action(actor, Action::PassPriority).unwrap();
        }
    }

    fn spawn_synth_bf(s: &mut MatchSession, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = s.state.allocate_timestamp();
        let obj = crate::state::GameObject::new(
            def,
            owner,
            Zone::Battlefield,
            ts,
            crate::state::ObjectFlags::empty(),
        );
        let id = s.state.insert_object(obj, None);
        s.state.player_mut(owner).unwrap().battlefield.push(id);
        s.state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        id
    }

    fn spawn_synth_hand(s: &mut MatchSession, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = s.state.allocate_timestamp();
        let obj = crate::state::GameObject::new(
            def,
            owner,
            Zone::Hand,
            ts,
            crate::state::ObjectFlags::empty(),
        );
        let id = s.state.insert_object(obj, None);
        s.state.player_mut(owner).unwrap().hand.push(id);
        s.state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        id
    }

    #[test]
    fn synth_concede_declare_attackers_and_main1_4p() {
        let db = synth_remediation_db();
        // Part A: Active player concedes during DeclareAttackers.
        let mut s = start_4p_session(Arc::clone(&db), 101);
        let _bear = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(1));
        pass_all_alive(&mut s); // Main1 -> BeginCombat
        assert_eq!(s.state().phase(), GamePhase::BeginCombat);
        pass_all_alive(&mut s); // BeginCombat -> DeclareAttackers
        assert_eq!(s.state().decision(), &DecisionState::DeclareAttackers);
        assert_eq!(s.current_actor(), Some(PlayerId(0)));

        s.apply_action(PlayerId(0), Action::Concede).unwrap();
        assert_eq!(s.state().decision(), &DecisionState::Priority);
        assert_eq!(s.current_actor(), Some(PlayerId(1)));
        let legal_p1 = s.legal_actions(PlayerId(1));
        assert!(legal_p1.contains(&Action::PassPriority));

        // Play remaining players cleanly through combat/turns to GameOver via PassPriority + ConfirmAttackers + Concede.
        for _ in 0..24 {
            let actor = s.current_actor().unwrap();
            if matches!(s.state().decision(), DecisionState::DeclareAttackers) {
                assert!(s.legal_actions(actor).contains(&Action::ConfirmAttackers));
                s.apply_action(actor, Action::ConfirmAttackers).unwrap();
            } else {
                assert!(s.legal_actions(actor).contains(&Action::PassPriority));
                s.apply_action(actor, Action::PassPriority).unwrap();
            }
        }
        while !matches!(s.state().decision(), DecisionState::GameOver { .. }) {
            let actor = s.current_actor().unwrap();
            s.apply_action(actor, Action::Concede).unwrap();
        }

        // Part B: Active player concedes in Main1 -> advances past DeclareAttackers without soft-lock.
        let mut s2 = start_4p_session(db, 102);
        s2.apply_action(PlayerId(0), Action::Concede).unwrap();
        assert_eq!(s2.current_actor(), Some(PlayerId(1)));
        let mut saw_main2 = false;
        for _ in 0..32 {
            if s2.state().phase() == GamePhase::Main2 {
                saw_main2 = true;
                break;
            }
            let actor = s2.current_actor().unwrap();
            let legal = s2.legal_actions(actor);
            assert!(legal.contains(&Action::PassPriority));
            s2.apply_action(actor, Action::PassPriority).unwrap();
        }
        assert!(
            saw_main2,
            "expected Main2 without DeclareAttackers soft-lock"
        );
    }

    #[test]
    fn synth_concede_declare_blockers_sole_and_multi_defender_4p() {
        let db = synth_remediation_db();
        // Case 1: Sole defender (P1) concedes during DeclareBlockers -> Priority, unattacked P2 not forced.
        let mut s1 = start_4p_session(Arc::clone(&db), 201);
        let atk = spawn_synth_bf(&mut s1, PlayerId(0), CardDefId(1));
        pass_all_alive(&mut s1); // Main1 -> BeginCombat
        pass_all_alive(&mut s1); // BeginCombat -> DeclareAttackers
        s1.apply_action(
            PlayerId(0),
            Action::AssignAttacker {
                attacker: atk,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        s1.apply_action(PlayerId(0), Action::ConfirmAttackers)
            .unwrap();
        pass_all_alive(&mut s1); // DeclareAttackers -> DeclareBlockers
        assert_eq!(s1.state().decision(), &DecisionState::DeclareBlockers);
        assert_eq!(s1.current_actor(), Some(PlayerId(1)));

        s1.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert!(s1.state().combat().attackers.is_empty());
        assert_eq!(s1.state().decision(), &DecisionState::Priority);
        assert_eq!(s1.current_actor(), Some(PlayerId(0)));

        // Case 2a: 2 defenders (P1 & P2), P1 concedes -> P2 gets DeclareBlockers.
        let mut s2 = start_4p_session(Arc::clone(&db), 202);
        let a1 = spawn_synth_bf(&mut s2, PlayerId(0), CardDefId(1));
        let a2 = spawn_synth_bf(&mut s2, PlayerId(0), CardDefId(1));
        pass_all_alive(&mut s2);
        pass_all_alive(&mut s2);
        s2.apply_action(
            PlayerId(0),
            Action::AssignAttacker {
                attacker: a1,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        s2.apply_action(
            PlayerId(0),
            Action::AssignAttacker {
                attacker: a2,
                target: AttackTarget::Player { id: PlayerId(2) },
            },
        )
        .unwrap();
        s2.apply_action(PlayerId(0), Action::ConfirmAttackers)
            .unwrap();
        pass_all_alive(&mut s2);
        assert_eq!(s2.state().decision(), &DecisionState::DeclareBlockers);
        assert_eq!(s2.current_actor(), Some(PlayerId(1)));

        s2.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert_eq!(s2.state().decision(), &DecisionState::DeclareBlockers);
        assert_eq!(s2.current_actor(), Some(PlayerId(2)));
        s2.apply_action(PlayerId(2), Action::ConfirmBlockers)
            .unwrap();
        assert_eq!(s2.state().decision(), &DecisionState::Priority);

        // Case 2b: P1 confirms blockers, then P2 concedes -> finalizes blockers cleanly.
        let mut s3 = start_4p_session(db, 203);
        let b1 = spawn_synth_bf(&mut s3, PlayerId(0), CardDefId(1));
        let b2 = spawn_synth_bf(&mut s3, PlayerId(0), CardDefId(1));
        pass_all_alive(&mut s3);
        pass_all_alive(&mut s3);
        s3.apply_action(
            PlayerId(0),
            Action::AssignAttacker {
                attacker: b1,
                target: AttackTarget::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        s3.apply_action(
            PlayerId(0),
            Action::AssignAttacker {
                attacker: b2,
                target: AttackTarget::Player { id: PlayerId(2) },
            },
        )
        .unwrap();
        s3.apply_action(PlayerId(0), Action::ConfirmAttackers)
            .unwrap();
        pass_all_alive(&mut s3);
        s3.apply_action(PlayerId(1), Action::ConfirmBlockers)
            .unwrap();
        assert_eq!(s3.state().decision(), &DecisionState::DeclareBlockers);
        assert_eq!(s3.current_actor(), Some(PlayerId(2)));

        s3.apply_action(PlayerId(2), Action::Concede).unwrap();
        assert_eq!(s3.state().decision(), &DecisionState::Priority);
        assert_eq!(s3.current_actor(), Some(PlayerId(0)));
    }

    #[test]
    fn synth_concede_blocker_damage_divide_4p() {
        let db = synth_remediation_db();
        let mut s = start_4p_session(db, 301);
        let a1 = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(1));
        let a2 = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(1));
        let a3 = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(1));
        let multi_blk = spawn_synth_bf(&mut s, PlayerId(1), CardDefId(2));

        pass_all_alive(&mut s); // Main1 -> BeginCombat
        pass_all_alive(&mut s); // BeginCombat -> DeclareAttackers
        for (atk, def) in [(a1, PlayerId(1)), (a2, PlayerId(1)), (a3, PlayerId(2))] {
            s.apply_action(
                PlayerId(0),
                Action::AssignAttacker {
                    attacker: atk,
                    target: AttackTarget::Player { id: def },
                },
            )
            .unwrap();
        }
        s.apply_action(PlayerId(0), Action::ConfirmAttackers)
            .unwrap();
        pass_all_alive(&mut s); // DeclareAttackers -> DeclareBlockers
        assert_eq!(s.current_actor(), Some(PlayerId(1)));
        s.apply_action(
            PlayerId(1),
            Action::AssignBlocker {
                blocker: multi_blk,
                attacker: a1,
            },
        )
        .unwrap();
        s.apply_action(
            PlayerId(1),
            Action::AssignBlocker {
                blocker: multi_blk,
                attacker: a2,
            },
        )
        .unwrap();
        s.apply_action(PlayerId(1), Action::ConfirmBlockers)
            .unwrap();
        assert_eq!(s.current_actor(), Some(PlayerId(2)));
        s.apply_action(PlayerId(2), Action::ConfirmBlockers)
            .unwrap();

        let p2_life_before = s.state().players()[2].life;
        pass_all_alive(&mut s); // DeclareBlockers -> CombatDamage (pauses on BlockerDamageDivide for P1)
        assert!(matches!(
            s.state().pending_prompt().map(|p| &p.owner),
            Some(PromptOwner::Combat(
                crate::state::CombatPromptKind::BlockerDamageDivide { .. }
            ))
        ));
        assert_eq!(s.current_actor(), Some(PlayerId(1)));

        s.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert!(s.state().combat().pending_damage.is_none());
        assert_eq!(s.state().decision(), &DecisionState::Priority);
        assert_eq!(s.state().players()[2].life, p2_life_before - 2);
    }

    #[test]
    fn synth_concede_mid_resolution_frame_4p() {
        let db = synth_remediation_db();

        // 1) SelectionPick (Synth Discard targeting P1) -> P1 concedes mid-prompt
        let mut s = start_4p_session(Arc::clone(&db), 401);
        let disc1 = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(3));
        let disc2 = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(3));
        s.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: disc1,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![TargetRef::Player { id: PlayerId(1) }],
            },
        )
        .unwrap();
        pass_all_alive(&mut s); // resolves onto SelectionPick for P1
        assert_eq!(s.current_actor(), Some(PlayerId(1)));
        assert_eq!(s.state().resolution_vms().len(), 1);
        assert_eq!(s.state().resolution_frames().len(), 1);

        s.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert!(s.state().resolution_vms().is_empty());
        assert!(s.state().resolution_frames().is_empty());
        assert!(s.state().stack().is_empty());
        assert_eq!(s.state().decision(), &DecisionState::Priority);
        assert_eq!(s.current_actor(), Some(PlayerId(0)));

        // Second Synth Discard targeting P2 still resolves cleanly.
        s.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: disc2,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![TargetRef::Player { id: PlayerId(2) }],
            },
        )
        .unwrap();
        pass_all_alive(&mut s);
        assert_eq!(s.current_actor(), Some(PlayerId(2)));
        let pid = s.state().pending_prompt().unwrap().prompt_id;
        let p2_card = s.state().players()[2].hand[0];
        s.apply_action(
            PlayerId(2),
            Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickObject { id: p2_card },
            },
        )
        .unwrap();
        assert!(s.state().resolution_vms().is_empty());
        assert!(s.state().resolution_frames().is_empty());
        assert!(s.state().stack().is_empty());

        // 2) OptionalChoice { is_unless: true } -> P1 concedes -> P0 still draws 1
        let mut s_unl = start_4p_session(Arc::clone(&db), 402);
        let spell_u = spawn_synth_hand(&mut s_unl, PlayerId(0), CardDefId(4));
        s_unl.state.player_mut(PlayerId(1)).unwrap().mana_pool[5] = 2;
        s_unl
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: spell_u,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![TargetRef::Player { id: PlayerId(1) }],
                },
            )
            .unwrap();
        let p0_hand_before = s_unl.state().players()[0].hand.len();
        pass_all_alive(&mut s_unl);
        assert_eq!(s_unl.current_actor(), Some(PlayerId(1)));
        s_unl.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert!(s_unl.state().resolution_vms().is_empty());
        assert!(s_unl.state().resolution_frames().is_empty());
        assert!(s_unl.state().stack().is_empty());
        assert_eq!(s_unl.state().players()[0].hand.len(), p0_hand_before + 1);

        // 3) UpToDraw multi-player -> P1 concedes -> P2 & P3 still prompted & draw
        let mut s_up = start_4p_session(Arc::clone(&db), 403);
        let spell_up = spawn_synth_hand(&mut s_up, PlayerId(0), CardDefId(5));
        s_up.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: spell_up,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        let p2_hand_before = s_up.state().players()[2].hand.len();
        let p3_hand_before = s_up.state().players()[3].hand.len();
        pass_all_alive(&mut s_up);
        assert_eq!(s_up.current_actor(), Some(PlayerId(1)));
        s_up.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert_eq!(s_up.state().players()[2].hand.len(), p2_hand_before + 2);
        assert_eq!(s_up.state().players()[3].hand.len(), p3_hand_before + 2);
        assert_eq!(s_up.state().decision(), &DecisionState::Priority);
        assert_eq!(s_up.current_actor(), Some(PlayerId(0)));
        assert!(s_up.state().resolution_vms().is_empty());
        assert!(s_up.state().resolution_frames().is_empty());
        assert!(s_up.state().stack().is_empty());

        // 4) ForEachPlayerLoop -> P1 concedes on P1's iteration -> P2 & P3 still iterate, P1 skipped
        let mut s_fe = start_4p_session(db, 404);
        let spell_fe = spawn_synth_hand(&mut s_fe, PlayerId(0), CardDefId(6));
        s_fe.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: spell_fe,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        pass_all_alive(&mut s_fe);
        assert_eq!(s_fe.current_actor(), Some(PlayerId(1)));
        s_fe.apply_action(PlayerId(1), Action::Concede).unwrap();
        assert_eq!(s_fe.current_actor(), Some(PlayerId(2)));
        let pid_p2 = s_fe.state().pending_prompt().unwrap().prompt_id;
        let p2_c = s_fe.state().players()[2].hand[0];
        s_fe.apply_action(
            PlayerId(2),
            Action::Answer {
                prompt_id: pid_p2,
                answer: ChoiceAnswer::PickObject { id: p2_c },
            },
        )
        .unwrap();
        assert_eq!(s_fe.current_actor(), Some(PlayerId(3)));
        let pid_p3 = s_fe.state().pending_prompt().unwrap().prompt_id;
        let p3_c = s_fe.state().players()[3].hand[0];
        s_fe.apply_action(
            PlayerId(3),
            Action::Answer {
                prompt_id: pid_p3,
                answer: ChoiceAnswer::PickObject { id: p3_c },
            },
        )
        .unwrap();
        assert!(s_fe.state().resolution_vms().is_empty());
        assert!(s_fe.state().resolution_frames().is_empty());
        assert!(s_fe.state().stack().is_empty());
    }

    #[test]
    fn synth_apply_action_error_atomicity_and_lki_cache_preservation() {
        let db = synth_remediation_db();
        let mut s = start_4p_session(Arc::clone(&db), 501);

        // Verify valid non-final PassPriority preserves OnceLock<LayerCache> pointer and layer_rev.
        let ptr_before: *const crate::state::LayerCache = s.state().evaluate_board();
        let rev_before = s.state().layer_rev();
        s.apply_action(PlayerId(0), Action::PassPriority).unwrap();
        let ptr_after: *const crate::state::LayerCache = s.state().evaluate_board();
        assert_eq!(s.state().layer_rev(), rev_before);
        assert_eq!(ptr_before, ptr_after);

        // Reset to clean P0 priority with a Phase 2 NotImplemented spell from initial DeckSpec (for from_replay).
        let mut cfg2 = GameConfig::commander(4);
        cfg2.commander = None;
        let p2_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(7); 60],
            sideboard: vec![],
        };
        let mut s2 = MatchSession::new(
            Arc::clone(&db),
            cfg2,
            vec![p2_deck.clone(), p2_deck.clone(), p2_deck.clone(), p2_deck],
            502,
        )
        .unwrap();
        for seat in 0..4u8 {
            s2.apply_action(PlayerId(seat), Action::KeepHand).unwrap();
        }
        let p2_spell = s2.state().players()[0].hand[0];
        s2.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: p2_spell,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        s2.apply_action(PlayerId(0), Action::PassPriority).unwrap();
        s2.apply_action(PlayerId(1), Action::PassPriority).unwrap();
        s2.apply_action(PlayerId(2), Action::PassPriority).unwrap();
        assert_eq!(s2.current_actor(), Some(PlayerId(3)));

        // Final PassPriority resolves top of stack -> hits EngineError::NotImplemented after mutating
        // passed_priority_mask & resolution_started; rollback must restore EVERYTHING identically.
        let snap_before = s2.snapshot_bytes().unwrap();
        let rev_before2 = s2.state().layer_rev();
        let seq_before = s2.state().event_seq();
        let log_len_before = s2.replay_log().entries.len();
        let err = s2
            .apply_action(PlayerId(3), Action::PassPriority)
            .unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)));
        assert_eq!(s2.state().layer_rev(), rev_before2);
        assert_eq!(s2.state().event_seq(), seq_before);
        assert_eq!(s2.replay_log().entries.len(), log_len_before);
        assert_eq!(s2.snapshot_bytes().unwrap(), snap_before);
        assert!(!s2.state().stack_entries()[0].choices.resolution_started);

        // Replay must reconstruct identical state cleanly.
        let replayed = MatchSession::from_replay(db, s2.replay_log().clone()).unwrap();
        assert_eq!(replayed.snapshot_bytes().unwrap(), snap_before);
    }

    #[test]
    fn synth_preview_auto_tap_cr401_5_and_macro_cast_trailing_modes() {
        let db = synth_remediation_db();
        let mut s = start_4p_session(db, 601);
        let _forest = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(0));
        let _future = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(8));
        let cub_hand = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(1));

        // Put a Synth Cub on top of P0's library.
        let ts = s.state.allocate_timestamp();
        let cub_lib_obj = crate::state::GameObject::new(
            CardDefId(1),
            PlayerId(0),
            Zone::Library,
            ts,
            crate::state::ObjectFlags::empty(),
        );
        let cub_top = s.state.insert_object(cub_lib_obj, None);
        s.state
            .player_mut(PlayerId(0))
            .unwrap()
            .library
            .push(cub_top);
        s.state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let top_req = CastRequest {
            object: cub_top,
            option_id: 0,
            x_value: None,
        };
        // Idle in Priority: controller can see and preview auto-tap for top-of-library spell.
        let prev_idle = s.preview_auto_tap(PlayerId(0), &top_req);
        assert!(prev_idle.can_pay);

        // While casting another spell (CR 401.5), top of library is hidden and preview rejects.
        s.apply_action(
            PlayerId(0),
            Action::BeginCast {
                object: cub_hand,
                option_id: 0,
                from_zone: Zone::Hand,
            },
        )
        .unwrap();
        let prev_casting = s.preview_auto_tap(PlayerId(0), &top_req);
        assert!(!prev_casting.can_pay);
        s.apply_action(PlayerId(0), Action::CancelCast).unwrap();

        // Trailing modes on non-modal MacroAction::CastSpell must be rejected and rolled back.
        let snap_before = s.snapshot_bytes().unwrap();
        let err = s
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: cub_hand,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![0],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::IllegalAction(_)));
        assert_eq!(s.snapshot_bytes().unwrap(), snap_before);
    }

    #[test]
    fn synth_macro_cast_divide_total_and_leftover_modes_on_pause() {
        let synth_step = |op: EffectOp, subject: Selector| EffectStep {
            op,
            subject,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        };
        let make_div_spell = |oracle_id: &str, name: &str, total: i32| {
            let mut spec = mtg_ir::TargetSpec {
                kind: mtg_ir::TargetKind::Object,
                filter: ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                },
                player: None,
                min: 0,
                max: 99,
                same_owner: false,
                condition: None,
                other_target: false,
                divide_total: Some(Box::new(mtg_ir::Amount::Const { n: total })),
                with_single_target: false,
                dynamic_count: None,
                dynamic_up_to: false,
                max_total_mana_value: None,
                also_spells: false,
                per_opponent: false,
                different_controllers: false,
                chosen_by_event_player: false,
                shares_card_type_with_target: None,
                zones: ZoneMask::BATTLEFIELD,
            };
            spec.filter.zones = ZoneMask::BATTLEFIELD;
            let mut dmg = synth_step(
                EffectOp::Damage {
                    source: None,
                    also_players: None,
                    divided: true,
                    excess_to_controller: false,
                    excess_to_controller_if_trample: false,
                    or_planeswalker_controlled_by: None,
                    damage_attacked_target_by_self: false,
                    soulfire_exile_damage_per_target: false,
                    each_creature_deals: None,
                },
                Selector::Target { index: 0 },
            );
            dmg.amount = Some(Box::new(mtg_ir::Amount::Const { n: total }));
            CardDef {
                oracle_id: oracle_id.into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: name.into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: name.into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{0}"),
                        types: CardTypeMask::INSTANT,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::R,
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![spec],
                        effect: vec![dmg],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Span::default(),
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            }
        };

        let land = CardDef {
            oracle_id: "synth_m_div_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Div Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Div Land".into(),
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
            oracle_id: "synth_m_div_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Div Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Div Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 4,
                        toughness: 4,
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
        let split3 = make_div_spell("synth_m_split3", "Synth Split 3", 3);
        let split4 = make_div_spell("synth_m_split4", "Synth Split 4", 4);

        let opt_cost = CostDef::Optional {
            cost: vec![CostDef::PayLife {
                amount: Box::new(mtg_ir::Amount::Const { n: 1 }),
                half_life_rounded_up: false,
            }],
        };
        let gain_life_mode = |amt: i32| {
            let mut st = synth_step(
                EffectOp::GainLife {
                    set_life_total: false,
                },
                Selector::PlayerRef {
                    player: PlayerRef::You,
                },
            );
            st.amount = Some(Box::new(mtg_ir::Amount::Const { n: amt }));
            mtg_ir::ModalMode {
                targets: vec![],
                effect: vec![st],
                span: Span::default(),
            }
        };
        let modal_opt = CardDef {
            oracle_id: "synth_m_modal_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal Opt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Modal Opt".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![opt_cost.clone()],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(mtg_ir::ModalDef {
                        min: 1,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![gain_life_mode(1), gain_life_mode(2)],
                        per_mode_costs: vec![],
                        unchosen_permanent: false,
                        opponent_chooses: None,
                        pawprint_budget: None,
                        pawprint_costs: vec![],
                        dynamic_max: None,
                        random_mode: false,
                        prefix_effect: vec![],
                        suffix_effect: vec![],
                        conditional_choose_range: None,
                        conditional_not_random: None,
                        different_player_targets_per_mode: false,
                    }),
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let nonmodal_opt = CardDef {
            oracle_id: "synth_m_nonmodal_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth NonModal Opt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth NonModal Opt".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![opt_cost],
                    targets: vec![],
                    effect: vec![gain_life_mode(1).effect[0].clone()],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
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
                tables: InternTables::default(),
                cards: vec![land, bear, split3, split4, modal_opt, nonmodal_opt],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let new_session = |seed: u64| {
            let mut s = MatchSession::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                vec![deck.clone(), deck.clone()],
                seed,
            )
            .unwrap();
            s.apply_action(PlayerId(0), Action::KeepHand).unwrap();
            s.apply_action(PlayerId(1), Action::KeepHand).unwrap();
            s
        };

        // 1. Synth Split 3: 1 target completes with 3; 3 targets auto-assigns (1,1,1); 4 targets rejected + rollback.
        let mut s = new_session(801);
        let c0 = spawn_synth_bf(&mut s, PlayerId(1), CardDefId(1));
        let c1 = spawn_synth_bf(&mut s, PlayerId(1), CardDefId(1));
        let c2 = spawn_synth_bf(&mut s, PlayerId(1), CardDefId(1));
        let c3 = spawn_synth_bf(&mut s, PlayerId(1), CardDefId(1));

        let sp_one = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(2));
        let out_one = s
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sp_one,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![TargetRef::Object { id: c0 }],
                },
            )
            .unwrap();
        assert_eq!(out_one.decision, DecisionState::Priority);
        assert_eq!(
            s.state()
                .stack_entries()
                .last()
                .unwrap()
                .choices
                .divisions
                .as_slice(),
            &[(TargetRef::Object { id: c0 }, 3)]
        );

        let sp_three = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(2));
        let out_three = s
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sp_three,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![
                        TargetRef::Object { id: c0 },
                        TargetRef::Object { id: c1 },
                        TargetRef::Object { id: c2 },
                    ],
                },
            )
            .unwrap();
        assert_eq!(out_three.decision, DecisionState::Priority);
        assert_eq!(
            s.state()
                .stack_entries()
                .last()
                .unwrap()
                .choices
                .divisions
                .as_slice(),
            &[
                (TargetRef::Object { id: c0 }, 1),
                (TargetRef::Object { id: c1 }, 1),
                (TargetRef::Object { id: c2 }, 1),
            ]
        );

        let sp_four = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(2));
        let snap_before_four = s.snapshot_bytes().unwrap();
        let log_before_four = s.replay_log().entries.len();
        let err_four = s
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sp_four,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![
                        TargetRef::Object { id: c0 },
                        TargetRef::Object { id: c1 },
                        TargetRef::Object { id: c2 },
                        TargetRef::Object { id: c3 },
                    ],
                },
            )
            .unwrap_err();
        assert!(matches!(err_four, EngineError::IllegalAction(_)));
        assert_eq!(s.snapshot_bytes().unwrap(), snap_before_four);
        assert_eq!(s.replay_log().entries.len(), log_before_four);

        // 2. Synth Split 4 with 2 targets pauses cleanly in ChooseDivisions; extraneous modes rejected on pause.
        let mut s_div = new_session(802);
        let d0 = spawn_synth_bf(&mut s_div, PlayerId(1), CardDefId(1));
        let d1 = spawn_synth_bf(&mut s_div, PlayerId(1), CardDefId(1));
        let sp4 = spawn_synth_hand(&mut s_div, PlayerId(0), CardDefId(3));

        let snap_before_ext = s_div.snapshot_bytes().unwrap();
        let err_ext_mode = s_div
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sp4,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![0],
                    x_value: None,
                    targets: vec![TargetRef::Object { id: d0 }, TargetRef::Object { id: d1 }],
                },
            )
            .unwrap_err();
        assert!(matches!(err_ext_mode, EngineError::IllegalAction(_)));
        assert_eq!(s_div.snapshot_bytes().unwrap(), snap_before_ext);

        let out_div = s_div
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sp4,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![TargetRef::Object { id: d0 }, TargetRef::Object { id: d1 }],
                },
            )
            .unwrap();
        assert_eq!(out_div.decision, DecisionState::CastingSequence);
        assert_eq!(
            s_div.state().casting().unwrap().step,
            crate::state::CastStep::ChooseDivisions
        );

        // 3. Synth Modal Opt: extraneous 3rd mode rejected + rolled back even when pausing on CastOptionalCost.
        let mut s_opt = new_session(803);
        let m_opt = spawn_synth_hand(&mut s_opt, PlayerId(0), CardDefId(4));
        let snap_opt = s_opt.snapshot_bytes().unwrap();
        let log_opt = s_opt.replay_log().entries.len();
        let err_opt = s_opt
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: m_opt,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![0, 1, 0],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap_err();
        assert!(matches!(err_opt, EngineError::IllegalAction(_)));
        assert_eq!(s_opt.snapshot_bytes().unwrap(), snap_opt);
        assert_eq!(s_opt.replay_log().entries.len(), log_opt);

        // Valid modes: [0, 1] pauses cleanly at CastOptionalCost and completes interactively.
        let out_opt = s_opt
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: m_opt,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![0, 1],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap();
        let DecisionState::Prompt { request } = out_opt.decision else {
            panic!("expected CastOptionalCost prompt");
        };
        let pid = match request {
            ChoiceRequest::YesNo { prompt_id, .. } => prompt_id,
            other => panic!("expected YesNo prompt, got {other:?}"),
        };
        s_opt
            .apply_action(
                PlayerId(0),
                Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::YesNo { value: false },
                },
            )
            .unwrap();
        if matches!(s_opt.state().decision(), DecisionState::CastingSequence) {
            s_opt
                .apply_action(PlayerId(0), Action::AutoPayMana)
                .unwrap();
        }
        assert_eq!(s_opt.state().stack().len(), 1);

        // 4. Non-modal spell with CastOptionalCost + modes: [0] rejected + rolled back.
        let mut s_nm = new_session(804);
        let nm_opt = spawn_synth_hand(&mut s_nm, PlayerId(0), CardDefId(5));
        let snap_nm = s_nm.snapshot_bytes().unwrap();
        let err_nm = s_nm
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: nm_opt,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![0],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap_err();
        assert!(matches!(err_nm, EngineError::IllegalAction(_)));
        assert_eq!(s_nm.snapshot_bytes().unwrap(), snap_nm);
    }

    #[test]
    fn synth_bug_m_commander_return_prompt_with_pending_sacrifice_copy_trigger() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Elemental".into());
        let st_elemental = mtg_ir::SubtypeId(0);

        let synth_step = |op: EffectOp, subject: Selector| EffectStep {
            op,
            subject,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        };

        // CardDefId(0): Synth Mountain
        let land = CardDef {
            oracle_id: "synth_bug_m_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bug M Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bug M Land".into(),
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

        // CardDefId(1): Synth Elemental Commander (Legendary 3/3 Elemental)
        let cmd_elemental = CardDef {
            oracle_id: "synth_bug_m_cmd".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Elemental Commander".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Elemental Commander".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: smallvec::smallvec![st_elemental],
                    supertypes: SupertypeMask::LEGENDARY,
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
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

        // CardDefId(2): Synth Sacrifice Copier (0/4, "Whenever you sacrifice a nontoken Elemental, create a token copy of it")
        let mut copy_step = synth_step(
            EffectOp::CopyPermanent {
                select: mtg_ir::SelectionMod::default(),
                not_legendary: false,
                tapped: false,
                tapped_and_attacking: false,
                per_opponent_tapped_attacking: None,
                copy_mod: None,
                except_pt: None,
            },
            Selector::EventObject,
        );
        copy_step.bind_as = Some("b0".into());

        let sac_copier = CardDef {
            oracle_id: "synth_bug_m_copier".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Sacrifice Copier".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Sacrifice Copier".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 0,
                        toughness: 4,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Triggered(mtg_ir::TriggeredAbility {
                    trigger: mtg_ir::TriggerDef {
                        event: mtg_ir::EventKind::Sacrificed,
                        filter: mtg_ir::EventFilter {
                            object: Some(mtg_ir::TriggerSubject::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    subtypes_any: smallvec::smallvec![st_elemental],
                                    flags: mtg_ir::FilterFlags::NONTOKEN,
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
                        or: Vec::new(),
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: Vec::new(),
                    effect: vec![copy_step],
                    modal: None,
                    text: None,
                    span: Span::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // CardDefId(3): Synth Sac Spell ({0}, target creature you control is sacrificed)
        let sac_spell = CardDef {
            oracle_id: "synth_bug_m_sac_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Sac Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Sac Spell".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{0}"),
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
                    additional_costs: vec![],
                    targets: vec![mtg_ir::TargetSpec {
                        kind: mtg_ir::TargetKind::Object,
                        filter: ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            controller: Some(mtg_ir::RelativeControl::You),
                            ..Default::default()
                        },
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
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![synth_step(
                        EffectOp::Sacrifice {
                            select: mtg_ir::SelectionMod::default(),
                        },
                        Selector::Target { index: 0 },
                    )],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Span::default(),
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
                cards: vec![land, cmd_elemental, sac_copier, sac_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck0 = DeckSpec {
            commanders: vec![CardDefId(1)],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![CardDefId(1)],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut s = MatchSession::new(
            Arc::clone(&db),
            GameConfig::commander(2),
            vec![deck0, deck1],
            910,
        )
        .unwrap();
        s.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        s.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        // Move P0's commander from Command zone to Battlefield, spawn Synth Sacrifice Copier on BF, and Synth Sac Spell in hand.
        let cmd_in_cmd = s.state().player(PlayerId(0)).unwrap().command[0];
        let cmd_bf_id = match crate::zones::move_object(
            &mut s.state,
            cmd_in_cmd,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected commander moved to BF"),
        };
        assert!(s.state().object(cmd_bf_id).unwrap().is_commander());
        let _copier = spawn_synth_bf(&mut s, PlayerId(0), CardDefId(2));
        let sac_spell_hand = spawn_synth_hand(&mut s, PlayerId(0), CardDefId(3));

        // 1. Cast Synth Sac Spell targeting Synth Elemental Commander.
        s.apply_macro_action(
            PlayerId(0),
            MacroAction::CastSpell {
                object: sac_spell_hand,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![TargetRef::Object { id: cmd_bf_id }],
            },
        )
        .unwrap();
        assert_eq!(s.state().stack().len(), 1);

        // 2. Both players pass priority: spell resolves, sacrificing commander -> queues PendingTrigger -> pauses on CommanderReturn.
        s.apply_action(PlayerId(0), Action::PassPriority).unwrap();
        let res = s.apply_action(PlayerId(1), Action::PassPriority).unwrap();

        let prompt_id = match res.decision {
            DecisionState::Prompt {
                request: ChoiceRequest::YesNo { prompt_id, .. },
            } => prompt_id,
            other => panic!("expected CommanderReturn YesNo prompt, got {other:?}"),
        };
        assert!(matches!(
            s.state().pending_prompt().map(|p| &p.owner),
            Some(PromptOwner::Sba(
                crate::state::SbaPromptKind::CommanderReturn { .. }
            ))
        ));
        assert!(s.state().stack().is_empty());
        assert!(!s.state().pending_triggers().is_empty());
        assert!(
            s.state().lki(cmd_bf_id).is_some(),
            "Bug M: LKI for sacrificed commander must be retained across CommanderReturn prompt while pending trigger references it"
        );

        // 3. Answer YesNo(false) (leave commander in graveyard): trigger flushes onto stack.
        s.apply_action(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::YesNo { value: false },
            },
        )
        .unwrap();
        assert_eq!(s.state().stack().len(), 1);

        // 4. Snapshot round-trip and resolve CopyPermanent trigger on both original and restored sessions.
        let snap_bytes = s.snapshot_bytes().unwrap();
        let mut s_restored = MatchSession::from_snapshot(db, &snap_bytes).unwrap();

        for sess in [&mut s, &mut s_restored] {
            let p0_before = sess.state().player(PlayerId(0)).unwrap().battlefield.len();
            let actor1 = sess.current_actor().unwrap();
            sess.apply_action(actor1, Action::PassPriority).unwrap();
            let actor2 = sess.current_actor().unwrap();
            sess.apply_action(actor2, Action::PassPriority).unwrap();

            assert!(sess.state().stack().is_empty());
            assert!(sess.state().pending_triggers().is_empty());
            assert!(
                sess.state().lki(cmd_bf_id).is_none(),
                "post-resolution idle prune_stale_lki must clear LKI records"
            );
            let bf = &sess.state().player(PlayerId(0)).unwrap().battlefield;
            assert_eq!(bf.len(), p0_before + 1);
            let tok_id = *bf.last().unwrap();
            assert!(sess.state().object(tok_id).unwrap().is_token());
            let ev = sess.state().evaluate_board().get(tok_id).unwrap();
            assert_eq!(
                ev.pt,
                Some(crate::view::PowerToughness {
                    power: 3,
                    toughness: 3
                })
            );
            assert!(ev.subtypes.contains(&st_elemental));
        }
    }

    #[test]
    fn synth_view_model_cached_visibility_attachments_and_cow_snapshot_parity() {
        let db = synth_gameplay_db();
        let mut main = vec![CardDefId(2), CardDefId(3)];
        main.extend(vec![CardDefId(0); 30]);
        main.extend(vec![CardDefId(1); 28]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: main,
            sideboard: vec![],
        };
        let mut session = MatchSession::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            515,
        )
        .unwrap();
        assert!(session.state().objects().slot_capacity() >= 120);

        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        let vm_empty_none = session.view_model(None);
        let vm_empty_p0 = session.view_model(Some(PlayerId(0)));
        assert!(!vm_empty_none.players[1].hand_revealed_to_viewer);
        assert!(vm_empty_none.players[1].revealed_hand.is_empty());
        assert_eq!(
            vm_empty_p0.players[0]
                .top_of_library
                .as_ref()
                .map(|t| t.visibility),
            Some(TopLibraryVisibility::Hidden)
        );

        let find_p0 = |s: &GameState, def: CardDefId| -> ObjectId {
            s.players()[0]
                .hand
                .iter()
                .chain(s.players()[0].library.iter())
                .copied()
                .find(|&id| s.object(id).unwrap().card_def == def)
                .unwrap()
        };
        let spy_id = find_p0(session.state(), CardDefId(2));
        let spy_bf = move_object_raw(
            &mut session.state,
            spy_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let cub_id = find_p0(session.state(), CardDefId(1));
        let cub_bf = move_object_raw(
            &mut session.state,
            cub_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        crate::zones::attach_raw(&mut session.state, spy_bf, cub_bf).unwrap();

        let vm_before_clone_none = session.view_model(None);
        let vm_before_clone_p0 = session.view_model(Some(PlayerId(0)));
        assert!(!vm_before_clone_none.players[0].hand_revealed_to_viewer);
        assert!(vm_before_clone_none.players[1].hand_revealed_to_viewer);
        assert!(vm_before_clone_p0.players[1].hand_revealed_to_viewer);
        assert_eq!(
            vm_before_clone_p0.players[1].revealed_hand.len(),
            session.state().players()[1].hand.len()
        );
        let cub_view = vm_before_clone_none.players[0]
            .battlefield
            .iter()
            .find(|v| v.id == cub_bf)
            .unwrap();
        assert_eq!(cub_view.attachments.as_slice(), &[spy_bf]);

        // Snapshot round-trip preserves exact ClientViewModel across spectator and player views.
        let snap_bytes = session.snapshot_bytes().unwrap();
        let restored = MatchSession::from_snapshot(Arc::clone(&db), &snap_bytes).unwrap();
        assert_eq!(restored.view_model(None), vm_before_clone_none);
        assert_eq!(restored.view_model(Some(PlayerId(0))), vm_before_clone_p0);

        // CoW state isolation: cloning GameState and mutating one chunk must not alter the original.
        let mut forked_state = session.state().clone();
        let lens_id = find_p0(&forked_state, CardDefId(3));
        move_object_raw(
            &mut forked_state,
            lens_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            forked_state
                .evaluate_board()
                .static_presence
                .contains(crate::state::StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY)
        );
        assert!(
            !session
                .state()
                .evaluate_board()
                .static_presence
                .contains(crate::state::StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY)
        );
        assert_eq!(session.view_model(None), vm_before_clone_none);
        assert_eq!(session.view_model(Some(PlayerId(0))), vm_before_clone_p0);
    }
}
