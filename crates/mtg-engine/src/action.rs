use mtg_ir::{CardDefId, ManaColor, Zone};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::id::{ObjectId, PlayerId, StackItemId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckSpec {
    pub commanders: Vec<CardDefId>,
    pub mainboard: Vec<CardDefId>,
    #[serde(default)]
    pub sideboard: Vec<CardDefId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckValidationReport {
    pub is_legal: bool,
    pub errors: Vec<String>,
    pub unsupported_card_ids: Vec<CardDefId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetRef {
    Object { id: ObjectId },
    Player { id: PlayerId },
    Stack { id: StackItemId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttackTarget {
    Player { id: PlayerId },
    Planeswalker { id: ObjectId },
    Battle { id: ObjectId },
}

/// Six canonical choice shapes (preserving `ENGINE_INVARIANTS.md` §6), plus optional guided hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChoiceRequest {
    YesNo {
        prompt_id: u32,
        label: String,
    },
    ChooseObjects {
        prompt_id: u32,
        candidates: Vec<ObjectId>,
        min: u16,
        max: u16,
    },
    ChooseMode {
        prompt_id: u32,
        option_count: u16,
        min: u16,
        max: u16,
    },
    ChooseNumber {
        prompt_id: u32,
        min: i32,
        max: i32,
    },
    OrderObjects {
        prompt_id: u32,
        objects: Vec<ObjectId>,
    },
    ChooseReplacement {
        prompt_id: u32,
        option_count: u16,
    },
    GuidedResolution {
        prompt_id: u32,
        source: ObjectId,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChoiceAnswer {
    YesNo { value: bool },
    PickObject { id: ObjectId },
    DonePicking,
    PickMode { index: u16 },
    PickNumber { value: i32 },
    OrderNext { id: ObjectId },
    PickReplacement { index: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DecisionState {
    Mulligan,
    Priority,
    CastingSequence,
    DeclareAttackers,
    DeclareBlockers,
    AssignCombatDamage,
    Prompt { request: ChoiceRequest },
    GameOver { winner: Option<PlayerId> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpecialActionKind {
    Suspend { object: ObjectId, x_value: u16 },
    TurnFaceUp { object: ObjectId },
    UnlockRoomDoor { object: ObjectId, face_index: u8 },
    CompanionToHand,
    HandSpecialAction { object: ObjectId, action_index: u16 },
}

/// Low-branching canonical sequential action (consumed by both `GameState` and `mtg-ai`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    KeepHand,
    TakeMulligan,
    BottomCard {
        object: ObjectId,
    },
    PassPriority,
    PlayLand {
        object: ObjectId,
        from_zone: Zone,
        face_index: u8,
    },
    TapMana {
        object: ObjectId,
        ability_index: u16,
        preferred_color: Option<ManaColor>,
    },
    BeginCast {
        object: ObjectId,
        option_id: u16,
        from_zone: Zone,
    },
    BeginActivate {
        object: ObjectId,
        ability_index: u16,
    },
    SpecialAction {
        kind: SpecialActionKind,
    },
    AssignAttacker {
        attacker: ObjectId,
        target: AttackTarget,
    },
    RemoveAttacker {
        attacker: ObjectId,
    },
    ConfirmAttackers,
    AssignBlocker {
        blocker: ObjectId,
        attacker: ObjectId,
    },
    ConfirmBlockers,
    Answer {
        prompt_id: u32,
        answer: ChoiceAnswer,
    },
    Concede,
}

/// Convenience batch actions submitted by human UIs and lowered atomically into `Vec<Action>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MacroAction {
    Single(Action),
    LondonBottom {
        objects: Vec<ObjectId>,
    },
    DeclareAttackers {
        assignments: Vec<(ObjectId, AttackTarget)>,
    },
    DeclareBlockers {
        assignments: Vec<(ObjectId, ObjectId)>,
    },
    CastSpell {
        object: ObjectId,
        option_id: u16,
        from_zone: Zone,
        #[serde(default)]
        modes: Vec<u16>,
        #[serde(default)]
        x_value: Option<u16>,
        #[serde(default)]
        targets: Vec<TargetRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastRequest {
    pub object: ObjectId,
    pub option_id: u16,
    pub x_value: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AutoTapPreview {
    pub can_pay: bool,
    pub sources_to_tap: SmallVec<[ObjectId; 8]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub event_seq: u64,
    pub next_actor: Option<PlayerId>,
    pub decision: DecisionState,
}
