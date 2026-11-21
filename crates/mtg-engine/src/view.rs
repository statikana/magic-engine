use mtg_ir::{
    CardDefId, CardTypeMask, ColorMask, CounterId, GamePhase, KeywordId, ManaCost, SubtypeId,
    SupertypeMask, Zone,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::action::{Action, AttackTarget, ChoiceRequest, DecisionState, TargetRef};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::state::FaceDownKind;

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
    pub base_pt: Option<PowerToughness>,
    pub subtypes: SmallVec<[SubtypeId; 4]>,
    pub supertypes: SupertypeMask,
    pub keywords: SmallVec<[KeywordId; 4]>,
    pub loyalty: Option<i32>,
    pub defense: Option<i32>,
    pub mana_value: u32,
    pub mana_cost: ManaCost,
    pub deathtouch_damage_marked: bool,
    pub attachments: SmallVec<[ObjectId; 4]>,
    pub is_token: bool,
    pub is_commander: bool,
    pub is_emblem: bool,
    pub is_dungeon: bool,
    pub has_lost_all_abilities: bool,
    pub face_down: bool,
    pub face_down_kind: Option<FaceDownKind>,
    pub known_to_viewer: bool,
    pub active_face_index: u8,
    pub flipped: bool,
    pub transformed: bool,
    pub entered_this_turn: bool,
    pub monstrous: bool,
    pub renowned: bool,
    pub suspected: bool,
    pub saddled: bool,
    pub harnessed: bool,
    pub prepared: bool,
    pub prototyped: bool,
    pub goaded_by: SmallVec<[PlayerId; 2]>,
    pub class_level: Option<u16>,
    pub unlocked_faces: SmallVec<[u8; 2]>,
    pub playable_until_turn: Option<u32>,
    pub playable_by: Option<PlayerId>,
    pub plotted_turn: Option<u32>,
    pub entered_with_x: Option<u32>,
    pub chosen_color: Option<ColorMask>,
    pub chosen_subtype: Option<SubtypeId>,
    pub chosen_player: Option<PlayerId>,
    pub chosen_card_type: Option<CardTypeMask>,
    pub chosen_number: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TopLibraryVisibility {
    Hidden,
    LookByController,
    RevealedToAll,
    KnownToViewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopOfLibraryView {
    pub visibility: TopLibraryVisibility,
    pub top_card: Option<ObjectView>,
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
    /// Populated strictly when `viewer == Some(p.id)`; always empty for opponents/spectators.
    pub hand: Vec<ObjectView>,
    pub battlefield: Vec<ObjectView>,
    pub graveyard: Vec<ObjectView>,
    pub exile: Vec<ObjectView>,
    pub command: Vec<ObjectView>,
    pub experience: u16,
    pub rad: u16,
    pub mana_pool: [u32; 6],
    pub snow_mana_pool: [u32; 6],
    pub restricted_mana_count: u16,
    pub max_hand_size: Option<u32>,
    pub lands_played_this_turn: u8,
    pub max_lands_per_turn: u16,
    pub mulligan_count: u8,
    pub has_kept_hand: bool,
    pub mulligan_bottom_remaining: u8,
    pub hand_revealed_to_viewer: bool,
    pub revealed_hand: Vec<ObjectView>,
    pub top_of_library: Option<TopOfLibraryView>,
    pub commander_color_identity: ColorMask,
    pub commander_cast_counts: SmallVec<[(u32, u16); 2]>,
    pub commander_damage_taken: SmallVec<[(u32, i32); 4]>,
    pub cant_lose: bool,
    pub dont_lose_at_zero_life: bool,
    pub life_cant_change: bool,
    pub hexproof: bool,
    pub shroud: bool,
    pub prevent_all_combat_damage_this_turn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackItemKindView {
    Spell,
    ActivatedAbility,
    TriggeredAbility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackItemView {
    pub stack_id: StackItemId,
    pub kind: StackItemKindView,
    pub source_object: Option<ObjectId>,
    pub card_def: CardDefId,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub face_index: u8,
    pub face_down: bool,
    pub known_to_viewer: bool,
    pub is_copy: bool,
    pub x_value: Option<u32>,
    pub chosen_modes: SmallVec<[u16; 2]>,
    pub targets: Vec<TargetRef>,
    pub paid_mana_colors: ColorMask,
    pub cant_be_countered: bool,
    pub object_view: Option<ObjectView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombatAttackerView {
    pub attacker: ObjectId,
    pub controller: PlayerId,
    pub target: AttackTarget,
    pub blockers: Vec<ObjectId>,
    pub was_blocked: bool,
    pub damage_assignment_order: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombatView {
    pub active: bool,
    pub attackers_declared: bool,
    pub declared_blockers_players: Vec<PlayerId>,
    pub first_strike_dealt: bool,
    pub combat_damage_dealt: bool,
    pub attackers: Vec<CombatAttackerView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptPurposeView {
    Mulligan,
    LondonBottom,
    CleanupDiscard,
    CommanderZoneChoice,
    LegendRule,
    SearchLibrary,
    ReorderLibrary,
    DigPick,
    ChooseReplacement,
    TriggerOrder,
    CostPayment,
    ManaChoice,
    CastingChoice,
    ResolutionChoice,
    GuidedResolution,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptView {
    pub prompt_id: u32,
    pub chooser: PlayerId,
    pub purpose: PromptPurposeView,
    pub is_redacted: bool,
    pub request: ChoiceRequest,
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
    pub stack: Vec<StackItemView>,
    pub combat: Option<CombatView>,
    pub prompt: Option<PromptView>,
    pub monarch: Option<PlayerId>,
    pub initiative: Option<PlayerId>,
    pub day_night: Option<bool>,
    pub passed_priority_mask: u8,
    pub turn_serial: u32,
    pub winner: Option<PlayerId>,
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
