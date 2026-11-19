use std::sync::Arc;

use bitflags::bitflags;
use mtg_ir::{CardDatabase, CardDefId, CounterId, GamePhase, Zone};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use thiserror::Error;

use crate::action::{DecisionState, DeckSpec};
use crate::config::{GameConfig, RngMode};
use crate::id::{ObjectArena, ObjectId, PlayerId};
use crate::rng::RngState;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("player count mismatch: config expects {expected}, got {actual}")]
    PlayerCountMismatch { expected: u8, actual: usize },
    #[error("unknown card definition id: {0:?}")]
    UnknownCardDef(CardDefId),
    #[error("deck validation failed: {0}")]
    InvalidDeck(String),
    #[error("action rejected: {0}")]
    IllegalAction(String),
    #[error("engine mechanic not yet implemented: {0}")]
    NotImplemented(&'static str),
}

bitflags! {
    /// Coarse dirty flags tracking which state domains mutated since the last layer evaluation.
    ///
    /// Replaces TS's O(N) `dynamicFingerprintMatches` scan with O(1) revision/mask validation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct DirtyMask: u32 {
        const TOPOLOGY     = 1 << 0;
        const COUNTERS     = 1 << 1;
        const COMBAT       = 1 << 2;
        const LIFE         = 1 << 3;
        const GRAVEYARDS   = 1 << 4;
        const EXILE        = 1 << 5;
        const TURN_HISTORY = 1 << 6;
        const TAPPED       = 1 << 7;
        const DESIGNATIONS = 1 << 8;
        const ALL          = 0x01FF;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ObjectFlags: u32 {
        const TAPPED         = 1 << 0;
        const SUMMONING_SICK = 1 << 1;
        const PHASED_OUT     = 1 << 2;
        const IS_TOKEN       = 1 << 3;
        const IS_COMMANDER   = 1 << 4;
        const FACE_DOWN      = 1 << 5;
        const TRANSFORMED    = 1 << 6;
        const FLIPPED        = 1 << 7;
        const SADDLED        = 1 << 8;
        const MONSTROUS      = 1 << 9;
        const SUSPECTED      = 1 << 10;
        const PREPARED       = 1 << 11;
        const RENOWNED       = 1 << 12;
        const SOLVED         = 1 << 13;
        const EXERTED        = 1 << 14;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameObject {
    pub card_def: CardDefId,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub zone: Zone,
    pub entered_timestamp: u64,
    pub flags: ObjectFlags,
    pub damage_marked: i32,
    pub counters: SmallVec<[(CounterId, u32); 2]>,
    pub attached_to: Option<ObjectId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub id: PlayerId,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub library: Vec<ObjectId>,
    pub hand: Vec<ObjectId>,
    pub battlefield: Vec<ObjectId>,
    pub graveyard: Vec<ObjectId>,
    pub exile: Vec<ObjectId>,
    pub command: Vec<ObjectId>,
}

impl PlayerState {
    fn new(id: PlayerId, starting_life: i32) -> Self {
        Self {
            id,
            life: starting_life,
            lost: false,
            poison: 0,
            energy: 0,
            library: Vec::with_capacity(100),
            hand: Vec::with_capacity(10),
            battlefield: Vec::with_capacity(24),
            graveyard: Vec::with_capacity(16),
            exile: Vec::with_capacity(8),
            command: Vec::with_capacity(2),
        }
    }
}

/// Pure, compact, deterministically cloneable MTG game state.
///
/// Fields are intentionally private so every mutation passes through a mutator that updates
/// `layer_rev` and `dirty_since_eval`.
#[derive(Debug, Clone)]
pub struct GameState {
    db: Arc<CardDatabase>,
    config: GameConfig,
    rng: RngState,
    objects: ObjectArena<GameObject>,
    players: SmallVec<[PlayerState; 4]>,
    stack: Vec<ObjectId>,
    turn_number: u32,
    active_player: PlayerId,
    priority_player: PlayerId,
    phase: GamePhase,
    decision: DecisionState,
    next_timestamp: u64,
    event_seq: u64,
    layer_rev: u64,
    dirty_since_eval: DirtyMask,
}

impl GameState {
    pub fn new(
        db: Arc<CardDatabase>,
        config: GameConfig,
        decks: &[DeckSpec],
        seed: u64,
    ) -> Result<Self, EngineError> {
        if decks.len() != usize::from(config.player_count) {
            return Err(EngineError::PlayerCountMismatch {
                expected: config.player_count,
                actual: decks.len(),
            });
        }
        let mut rng = match config.rng_mode {
            RngMode::Mulberry32Compat => RngState::mulberry32(seed as u32),
            RngMode::Fast => RngState::splitmix64(seed),
        };
        let mut objects = ObjectArena::with_capacity(decks.len() * 100);
        let mut players = SmallVec::with_capacity(decks.len());
        let mut next_ts: u64 = 1;

        for (seat_idx, deck) in decks.iter().enumerate() {
            let pid = PlayerId(seat_idx as u8);
            let mut pstate = PlayerState::new(pid, config.starting_life);

            for &card_def in &deck.commanders {
                if db.get(card_def).is_none() {
                    return Err(EngineError::UnknownCardDef(card_def));
                }
                let oid = objects.insert(GameObject {
                    card_def,
                    owner: pid,
                    controller: pid,
                    zone: Zone::Command,
                    entered_timestamp: next_ts,
                    flags: ObjectFlags::IS_COMMANDER,
                    damage_marked: 0,
                    counters: SmallVec::new(),
                    attached_to: None,
                });
                next_ts += 1;
                pstate.command.push(oid);
            }

            for &card_def in &deck.mainboard {
                if db.get(card_def).is_none() {
                    return Err(EngineError::UnknownCardDef(card_def));
                }
                let oid = objects.insert(GameObject {
                    card_def,
                    owner: pid,
                    controller: pid,
                    zone: Zone::Library,
                    entered_timestamp: next_ts,
                    flags: ObjectFlags::empty(),
                    damage_marked: 0,
                    counters: SmallVec::new(),
                    attached_to: None,
                });
                next_ts += 1;
                pstate.library.push(oid);
            }

            rng.shuffle(&mut pstate.library);
            players.push(pstate);
        }

        Ok(Self {
            db,
            config,
            rng,
            objects,
            players,
            stack: Vec::new(),
            turn_number: 1,
            active_player: PlayerId(0),
            priority_player: PlayerId(0),
            phase: GamePhase::Main1,
            decision: DecisionState::Mulligan,
            next_timestamp: next_ts,
            event_seq: 0,
            layer_rev: 1,
            dirty_since_eval: DirtyMask::ALL,
        })
    }

    #[inline]
    pub fn db(&self) -> &CardDatabase {
        &self.db
    }

    #[inline]
    pub fn config(&self) -> &GameConfig {
        &self.config
    }

    #[inline]
    pub fn players(&self) -> &[PlayerState] {
        &self.players
    }

    #[inline]
    pub fn stack(&self) -> &[ObjectId] {
        &self.stack
    }

    #[inline]
    pub fn object(&self, id: ObjectId) -> Option<&GameObject> {
        self.objects.get(id)
    }

    #[inline]
    pub fn turn_number(&self) -> u32 {
        self.turn_number
    }

    #[inline]
    pub fn active_player(&self) -> PlayerId {
        self.active_player
    }

    #[inline]
    pub fn priority_player(&self) -> PlayerId {
        self.priority_player
    }

    #[inline]
    pub fn phase(&self) -> GamePhase {
        self.phase
    }

    #[inline]
    pub fn decision(&self) -> &DecisionState {
        &self.decision
    }

    #[inline]
    pub fn event_seq(&self) -> u64 {
        self.event_seq
    }

    #[inline]
    pub fn layer_rev(&self) -> u64 {
        self.layer_rev
    }

    #[inline]
    pub fn dirty_since_eval(&self) -> DirtyMask {
        self.dirty_since_eval
    }

    #[inline]
    pub(crate) fn mark_dirty(&mut self, mask: DirtyMask) {
        self.layer_rev = self.layer_rev.wrapping_add(1);
        self.dirty_since_eval |= mask;
    }

    #[inline]
    pub(crate) fn bump_event_seq(&mut self) -> u64 {
        self.event_seq += 1;
        self.event_seq
    }

    #[inline]
    pub fn next_timestamp_counter(&self) -> u64 {
        self.next_timestamp
    }

    #[inline]
    pub(crate) fn set_decision(&mut self, decision: DecisionState) {
        self.decision = decision;
    }

    #[inline]
    pub(crate) fn player_mut(&mut self, id: PlayerId) -> Option<&mut PlayerState> {
        self.players.get_mut(id.0 as usize)
    }

    #[inline]
    pub(crate) fn rng_mut(&mut self) -> &mut RngState {
        &mut self.rng
    }
}
