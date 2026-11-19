use mtg_ir::{CardDefId, CardTypeMask, ColorMask, CounterId, GamePhase, Zone};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::action::{Action, DecisionState};
use crate::id::{ObjectId, PlayerId};

/// Signed unclamped power/toughness (CR 107.1b, 208.5); `None` on non-creatures (CR 208.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerToughness {
    pub power: i32,
    pub toughness: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousBreakdown {
    pub object: ObjectId,
    pub base_pt: Option<PowerToughness>,
    pub evaluated_pt: Option<PowerToughness>,
    pub types: CardTypeMask,
    pub colors: ColorMask,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectView {
    pub id: ObjectId,
    pub card_def: CardDefId,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub zone: Zone,
    pub tapped: bool,
    pub summoning_sick: bool,
    pub pt: Option<PowerToughness>,
    pub types: CardTypeMask,
    pub colors: ColorMask,
    pub counters: SmallVec<[(CounterId, u32); 2]>,
    pub damage_marked: i32,
    pub attached_to: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerView {
    pub id: PlayerId,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub library_count: u16,
    pub hand_count: u16,
    /// Populated only for `viewer` (or when hands are publicly revealed).
    pub hand: Vec<ObjectView>,
    pub battlefield: Vec<ObjectView>,
    pub graveyard: Vec<ObjectView>,
    pub exile: Vec<ObjectView>,
    pub command: Vec<ObjectView>,
}

/// Redacted view pushed by `magic-web` over WebSockets or returned by `mtg-wasm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientViewModel {
    pub viewer: Option<PlayerId>,
    pub turn_number: u32,
    pub active_player: PlayerId,
    pub priority_player: PlayerId,
    pub phase: GamePhase,
    pub decision: DecisionState,
    pub players: Vec<PlayerView>,
    pub legal_actions: Vec<Action>,
}

/// Structured numeric/entity observation designed for `mtg-ai` and `mtg-py` ML training.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub viewer: PlayerId,
    pub turn_number: u32,
    pub phase: GamePhase,
    pub players: SmallVec<[PlayerObs; 4]>,
    pub entities: Vec<EntityObs>,
    pub legal_actions: Vec<Action>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerObs {
    pub relative_seat: u8,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub hand_count: u16,
    pub library_count: u16,
    pub graveyard_count: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityObs {
    pub id: ObjectId,
    pub card_def: CardDefId,
    pub zone: Zone,
    pub owner_rel: u8,
    pub controller_rel: u8,
    pub pt: Option<PowerToughness>,
    pub types: CardTypeMask,
    pub colors: ColorMask,
    pub tapped: bool,
    pub summoning_sick: bool,
}
