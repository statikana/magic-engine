//! Opaque-box `SessionDriver` wrapping `MatchSession` with invariant-checked execution.

use std::sync::Arc;

use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, EngineError, GameConfig,
    MacroAction, MatchSession, ObjectId, ObjectView, PlayerId, RngState, StepOutcome,
};
use mtg_ir::{CardDatabase, CardDefId, GamePhase};

use crate::assertions::{
    assert_active_has_legal_actions, assert_observation_consistency, assert_view_model_redaction,
};

#[derive(Debug, Clone)]
pub struct SessionDriver {
    pub session: MatchSession,
}

impl SessionDriver {
    pub fn new(
        db: Arc<CardDatabase>,
        config: GameConfig,
        decks: Vec<DeckSpec>,
        seed: u64,
    ) -> Result<Self, EngineError> {
        let session = MatchSession::new(db, config, decks, seed)?;
        Ok(Self { session })
    }

    pub fn from_session(session: MatchSession) -> Self {
        Self { session }
    }

    pub fn actor(&self) -> Option<PlayerId> {
        self.session.current_actor()
    }

    pub fn decision(&self) -> DecisionState {
        self.session.view_model(None).decision
    }

    /// Submits a verified-legal action for `actor` and checks post-step opaque-box invariants.
    pub fn step(&mut self, actor: PlayerId, action: Action) -> Result<StepOutcome, EngineError> {
        assert_eq!(
            self.session.current_actor(),
            Some(actor),
            "SessionDriver::step called with wrong actor {actor:?}"
        );
        let legal = self.session.legal_actions(actor);
        assert!(
            legal.contains(&action),
            "action {action:?} not present in legal_actions({actor:?}): {legal:?}"
        );
        let prev_len = self.session.replay_log().entries.len();
        let out = self.session.apply_action(actor, action)?;
        assert_eq!(
            self.session.replay_log().entries.len(),
            prev_len + 1,
            "replay_log must append 1 entry per successful action"
        );
        self.check_invariants();
        Ok(out)
    }

    /// Submits an action directly to `MatchSession::apply_action` (for negative/illegal tests).
    pub fn step_unchecked(
        &mut self,
        actor: PlayerId,
        action: Action,
    ) -> Result<StepOutcome, EngineError> {
        self.session.apply_action(actor, action)
    }

    /// Submits a `MacroAction` and checks post-step invariants.
    pub fn macro_step(
        &mut self,
        actor: PlayerId,
        macro_act: MacroAction,
    ) -> Result<StepOutcome, EngineError> {
        let out = self.session.apply_macro_action(actor, macro_act)?;
        self.check_invariants();
        Ok(out)
    }

    pub fn check_invariants(&self) {
        assert_view_model_redaction(&self.session);
        assert_active_has_legal_actions(&self.session);
        let count = self.session.view_model(None).players.len();
        for seat in 0..count {
            assert_observation_consistency(&self.session, PlayerId(seat as u8));
        }
    }

    /// Keeps hands for all players during opening mulligans (fails if stuck in a loop).
    pub fn keep_all_hands(&mut self) {
        let mut guard = 0usize;
        while matches!(self.decision(), DecisionState::Mulligan) {
            let actor = self.actor().expect("mulligan requires actor");
            self.step(actor, Action::KeepHand)
                .expect("KeepHand must succeed during Mulligan");
            guard += 1;
            assert!(
                guard <= 16,
                "mulligan did not finish within 16 KeepHand steps"
            );
        }
    }

    /// Passes priority once for the current actor (or resolves simple priority passes).
    pub fn pass_priority(&mut self) -> StepOutcome {
        let actor = self.actor().expect("pass_priority requires actor");
        self.step(actor, Action::PassPriority)
            .expect("PassPriority must succeed")
    }

    /// Advances priority passes until `target_turn` & `target_phase`, failing if `max_steps` is
    /// exceeded without reaching the target. Automatically answers `CR 514.1` Cleanup discard
    /// `ChooseObjects` prompts.
    pub fn advance_to(
        &mut self,
        target_turn: u32,
        target_phase: GamePhase,
        max_steps: usize,
    ) -> Result<(), String> {
        self.advance_to_preserving(target_turn, target_phase, max_steps, &[])
    }

    /// Like [`Self::advance_to`], but preserves up to `req` copies of each `(CardDefId, req)`
    /// pair in the discarding player's hand when answering `CR 514.1` Cleanup `ChooseObjects`.
    pub fn advance_to_preserving(
        &mut self,
        target_turn: u32,
        target_phase: GamePhase,
        max_steps: usize,
        keep: &[(CardDefId, usize)],
    ) -> Result<(), String> {
        for _ in 0..max_steps {
            let vm = self.session.view_model(None);
            if vm.turn_number == target_turn && vm.phase == target_phase {
                return Ok(());
            }
            if matches!(vm.decision, DecisionState::GameOver { .. }) {
                return Err(format!(
                    "game ended before reaching turn {target_turn} phase {target_phase:?}"
                ));
            }
            let actor = self.actor().ok_or_else(|| "no actor".to_string())?;
            let legal = self.session.legal_actions(actor);
            let act = if legal.contains(&Action::PassPriority) {
                Action::PassPriority
            } else if legal.contains(&Action::ConfirmAttackers) {
                Action::ConfirmAttackers
            } else if legal.contains(&Action::ConfirmBlockers) {
                Action::ConfirmBlockers
            } else if legal.contains(&Action::KeepHand) {
                Action::KeepHand
            } else if vm.phase == GamePhase::Cleanup
                && let DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            ref candidates,
                            ..
                        },
                } = vm.decision
            {
                let answer = if candidates.is_empty() {
                    ChoiceAnswer::DonePicking
                } else {
                    ChoiceAnswer::PickObject {
                        id: self.pick_cleanup_discard(actor, candidates, keep),
                    }
                };
                Action::Answer { prompt_id, answer }
            } else {
                return Err(format!(
                    "advance_to stuck at turn {} {:?} decision {:?} legal {:?}",
                    vm.turn_number, vm.phase, vm.decision, legal
                ));
            };
            self.step(actor, act).map_err(|e| e.to_string())?;
        }
        let vm = self.session.view_model(None);
        Err(format!(
            "advance_to({target_turn}, {target_phase:?}) timed out after {max_steps} steps; currently at turn {} {:?}",
            vm.turn_number, vm.phase
        ))
    }

    fn pick_cleanup_discard(
        &self,
        actor: PlayerId,
        candidates: &[ObjectId],
        keep: &[(CardDefId, usize)],
    ) -> ObjectId {
        if keep.is_empty() {
            return candidates[0];
        }
        let hand = self
            .session
            .view_model(Some(actor))
            .players
            .into_iter()
            .find(|p| p.id == actor)
            .map(|p| p.hand)
            .unwrap_or_default();
        candidates
            .iter()
            .copied()
            .max_by_key(|&cand_id| {
                let Some(def) = hand.iter().find(|o| o.id == cand_id).map(|o| o.card_def) else {
                    return isize::MAX;
                };
                let in_hand = hand.iter().filter(|o| o.card_def == def).count() as isize;
                let required: isize = keep
                    .iter()
                    .filter(|&&(kdef, _)| kdef == def)
                    .map(|&(_, req)| req as isize)
                    .sum();
                in_hand - required
            })
            .unwrap_or(candidates[0])
    }

    pub fn find_hand_object(&self, player: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        let vm = self.session.view_model(Some(player));
        vm.players
            .iter()
            .find(|p| p.id == player)?
            .hand
            .iter()
            .find(|o| o.card_def == card_def)
            .map(|o| o.id)
    }

    pub fn find_battlefield_object(
        &self,
        controller: PlayerId,
        card_def: CardDefId,
    ) -> Option<ObjectId> {
        let vm = self.session.view_model(None);
        vm.players
            .iter()
            .find(|p| p.id == controller)?
            .battlefield
            .iter()
            .find(|o| o.card_def == card_def)
            .map(|o| o.id)
    }

    pub fn find_graveyard_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        let vm = self.session.view_model(None);
        vm.players
            .iter()
            .find(|p| p.id == owner)?
            .graveyard
            .iter()
            .find(|o| o.card_def == card_def)
            .map(|o| o.id)
    }

    pub fn find_exile_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        let vm = self.session.view_model(None);
        vm.players
            .iter()
            .find(|p| p.id == owner)?
            .exile
            .iter()
            .find(|o| o.card_def == card_def)
            .map(|o| o.id)
    }

    pub fn find_command_object(&self, owner: PlayerId, card_def: CardDefId) -> Option<ObjectId> {
        let vm = self.session.view_model(None);
        vm.players
            .iter()
            .find(|p| p.id == owner)?
            .command
            .iter()
            .find(|o| o.card_def == card_def)
            .map(|o| o.id)
    }

    pub fn battlefield_view(&self, controller: PlayerId, id: ObjectId) -> Option<ObjectView> {
        let vm = self.session.view_model(None);
        vm.players
            .iter()
            .find(|p| p.id == controller)?
            .battlefield
            .iter()
            .find(|o| o.id == id)
            .cloned()
    }
}

/// Plays a deterministic random legal-action rollout (excluding Concede unless it is the only
/// legal action) and checks redaction/observation invariants after every step.
pub fn play_deterministic_random_game(
    db: Arc<CardDatabase>,
    config: GameConfig,
    decks: Vec<DeckSpec>,
    seed: u64,
    max_steps: usize,
) -> DecisionState {
    play_deterministic_random_game_driver(db, config, decks, seed, max_steps).0
}

/// Same as [`play_deterministic_random_game`], also returning the final [`SessionDriver`]
/// so callers can assert turn advancement and non-trivial action history.
pub fn play_deterministic_random_game_driver(
    db: Arc<CardDatabase>,
    config: GameConfig,
    decks: Vec<DeckSpec>,
    seed: u64,
    max_steps: usize,
) -> (DecisionState, SessionDriver) {
    let mut driver = SessionDriver::new(db, config, decks, seed).expect("session init");
    let mut rng = RngState::splitmix64(seed ^ 0xA5A5_5A5A_1234_5678);

    for _ in 0..max_steps {
        let dec = driver.decision();
        if matches!(dec, DecisionState::GameOver { .. }) {
            return (dec, driver);
        }
        let actor = driver.actor().expect("non-gameover must have actor");
        let legal = driver.session.legal_actions(actor);
        assert!(!legal.is_empty(), "actor {actor:?} has 0 legal actions");

        let non_concede: Vec<&Action> = legal
            .iter()
            .filter(|a| !matches!(a, Action::Concede))
            .collect();
        let chosen = if non_concede.is_empty() {
            legal[0].clone()
        } else {
            let idx = rng.next_index(non_concede.len());
            non_concede[idx].clone()
        };
        driver
            .step(actor, chosen)
            .expect("legal action must succeed");
    }
    let dec = driver.decision();
    (dec, driver)
}
