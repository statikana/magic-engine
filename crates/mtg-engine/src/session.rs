use std::sync::Arc;

use mtg_ir::{CardDatabase, CardTypeMask, ColorMask};
use smallvec::SmallVec;

use crate::action::{
    Action, AutoTapPreview, CastRequest, DecisionState, DeckSpec, DeckValidationReport,
    MacroAction, StepOutcome,
};
use crate::config::GameConfig;
use crate::id::{ObjectId, PlayerId};
use crate::replay::{ReplayEntry, ReplayLog};
use crate::rng::RngState;
use crate::state::{DirtyMask, EngineError, GameState};
use crate::view::{
    ClientViewModel, ContinuousBreakdown, EntityObs, ObjectView, Observation, PlayerObs, PlayerView,
};

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
        let state = GameState::new(db, config.clone(), &decks, seed)?;
        let log = ReplayLog {
            config,
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
    pub fn replay_log(&self) -> &ReplayLog {
        &self.log
    }

    pub fn current_actor(&self) -> Option<PlayerId> {
        match self.state.decision() {
            DecisionState::GameOver { .. } => None,
            _ => Some(self.state.priority_player()),
        }
    }

    pub fn legal_actions(&self, player: PlayerId) -> Vec<Action> {
        if self.current_actor() != Some(player) {
            return Vec::new();
        }
        match self.state.decision() {
            DecisionState::Mulligan => {
                vec![Action::KeepHand, Action::TakeMulligan, Action::Concede]
            }
            DecisionState::Priority => vec![Action::PassPriority, Action::Concede],
            DecisionState::GameOver { .. } => Vec::new(),
            _ => vec![Action::Concede],
        }
    }

    pub fn apply_action(
        &mut self,
        player: PlayerId,
        action: Action,
    ) -> Result<StepOutcome, EngineError> {
        if self.current_actor() != Some(player) {
            return Err(EngineError::IllegalAction(format!(
                "player {player:?} does not currently have the decision"
            )));
        }
        match &action {
            Action::Concede => {
                if let Some(p) = self.state.player_mut(player) {
                    p.lost = true;
                }
                self.state.mark_dirty(DirtyMask::TOPOLOGY);
                let alive: Vec<PlayerId> = self
                    .state
                    .players()
                    .iter()
                    .filter(|p| !p.lost)
                    .map(|p| p.id)
                    .collect();
                if alive.len() <= 1 {
                    self.state.set_decision(DecisionState::GameOver {
                        winner: alive.first().copied(),
                    });
                }
            }
            Action::KeepHand | Action::PassPriority => {
                // Phase 1 will wire full mulligan, turn structure, casting, combat, and SBA loops.
                if matches!(self.state.decision(), DecisionState::Mulligan) {
                    self.state.set_decision(DecisionState::Priority);
                }
            }
            _ => {
                return Err(EngineError::NotImplemented(
                    "action handler lands in Phase 1",
                ));
            }
        }

        let seq = self.state.bump_event_seq();
        self.log.entries.push(ReplayEntry {
            seq,
            actor: player,
            action,
        });
        Ok(StepOutcome {
            event_seq: seq,
            next_actor: self.current_actor(),
            decision: self.state.decision().clone(),
        })
    }

    pub fn apply_macro_action(
        &mut self,
        player: PlayerId,
        macro_act: MacroAction,
    ) -> Result<StepOutcome, EngineError> {
        match macro_act {
            MacroAction::Single(act) => self.apply_action(player, act),
            _ => Err(EngineError::NotImplemented(
                "macro action lowering lands in Phase 1",
            )),
        }
    }

    pub fn view_model(&self, viewer: Option<PlayerId>) -> ClientViewModel {
        let to_view = |oid: ObjectId| -> Option<ObjectView> {
            let obj = self.state.object(oid)?;
            let face = self
                .state
                .db()
                .get(obj.card_def)
                .and_then(|c| c.faces.first());
            Some(ObjectView {
                id: oid,
                card_def: obj.card_def,
                owner: obj.owner,
                controller: obj.controller,
                zone: obj.zone,
                tapped: obj.flags.contains(crate::state::ObjectFlags::TAPPED),
                summoning_sick: obj
                    .flags
                    .contains(crate::state::ObjectFlags::SUMMONING_SICK),
                pt: None,
                types: face
                    .map(|f| f.characteristics.types)
                    .unwrap_or(CardTypeMask::empty()),
                colors: face
                    .map(|f| f.characteristics.colors)
                    .unwrap_or(ColorMask::empty()),
                counters: obj.counters.clone(),
                damage_marked: obj.damage_marked,
                attached_to: obj.attached_to,
            })
        };

        let players = self
            .state
            .players()
            .iter()
            .map(|p| PlayerView {
                id: p.id,
                life: p.life,
                lost: p.lost,
                poison: p.poison,
                energy: p.energy,
                library_count: p.library.len() as u16,
                hand_count: p.hand.len() as u16,
                hand: if viewer == Some(p.id) {
                    p.hand.iter().filter_map(|&id| to_view(id)).collect()
                } else {
                    Vec::new()
                },
                battlefield: p.battlefield.iter().filter_map(|&id| to_view(id)).collect(),
                graveyard: p.graveyard.iter().filter_map(|&id| to_view(id)).collect(),
                exile: p.exile.iter().filter_map(|&id| to_view(id)).collect(),
                command: p.command.iter().filter_map(|&id| to_view(id)).collect(),
            })
            .collect();

        ClientViewModel {
            viewer,
            turn_number: self.state.turn_number(),
            active_player: self.state.active_player(),
            priority_player: self.state.priority_player(),
            phase: self.state.phase(),
            decision: self.state.decision().clone(),
            players,
            legal_actions: viewer.map(|v| self.legal_actions(v)).unwrap_or_default(),
        }
    }

    pub fn hover_breakdown(
        &self,
        _viewer: Option<PlayerId>,
        object: ObjectId,
    ) -> Option<ContinuousBreakdown> {
        let obj = self.state.object(object)?;
        let face = self.state.db().get(obj.card_def)?.faces.first()?;
        Some(ContinuousBreakdown {
            object,
            base_pt: None,
            evaluated_pt: None,
            types: face.characteristics.types,
            colors: face.characteristics.colors,
            notes: Vec::new(),
        })
    }

    pub fn preview_auto_tap(&self, _player: PlayerId, _req: &CastRequest) -> AutoTapPreview {
        AutoTapPreview::default()
    }

    pub fn observe(&self, viewer: PlayerId) -> Observation {
        let n = self.state.players().len();
        let mut players = SmallVec::with_capacity(n);
        for offset in 0..n {
            let idx = (usize::from(viewer.0) + offset) % n;
            let p = &self.state.players()[idx];
            players.push(PlayerObs {
                relative_seat: offset as u8,
                life: p.life,
                lost: p.lost,
                poison: p.poison,
                energy: p.energy,
                hand_count: p.hand.len() as u16,
                library_count: p.library.len() as u16,
                graveyard_count: p.graveyard.len() as u16,
            });
        }
        let entities: Vec<EntityObs> = Vec::new();
        Observation {
            viewer,
            turn_number: self.state.turn_number(),
            phase: self.state.phase(),
            players,
            entities,
            legal_actions: self.legal_actions(viewer),
        }
    }

    /// Determinizes hidden zones invisible to `viewer` for ISMCTS search.
    pub fn determinize(&self, viewer: PlayerId, rng: &mut RngState) -> GameState {
        let mut cloned = self.state.clone();
        // Phase 1/3: pool and resample unrevealed opponent hand + library + face-down cards.
        let _ = (viewer, rng, cloned.rng_mut());
        cloned
    }

    pub fn validate_deck(
        db: &CardDatabase,
        config: &GameConfig,
        deck: &DeckSpec,
    ) -> DeckValidationReport {
        let mut errors = Vec::new();
        let mut unsupported = Vec::new();
        for &cid in deck.commanders.iter().chain(deck.mainboard.iter()) {
            match db.get(cid) {
                None => errors.push(format!("unknown card id {cid:?}")),
                Some(def) if !def.fully_supported => {
                    unsupported.push(cid);
                    if !config.allow_guided_fallback {
                        errors.push(format!("unsupported card: {}", def.name));
                    }
                }
                Some(_) => {}
            }
        }
        DeckValidationReport {
            is_legal: errors.is_empty(),
            errors,
            unsupported_card_ids: unsupported,
        }
    }
}
