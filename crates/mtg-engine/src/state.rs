use std::sync::{Arc, OnceLock};

use bitflags::bitflags;
use mtg_ir::{
    CardDatabase, CardDefId, CardTypeMask, ColorMask, CopyModificationSpec, CostDef, CounterId,
    DelayedTriggerPayload, EffectStep, GamePhase, KeywordId, ManaColor, ManaCost, ManaPip,
    ManaRestrictionTag, ManaSpendingRestriction, RoundMode, SubtypeId, SupertypeMask, TokenSpec,
    Zone,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};
use smallvec::SmallVec;
use thiserror::Error;

use crate::action::{AttackTarget, ChoiceRequest, DecisionState, DeckSpec, TargetRef};
use crate::config::{GameConfig, RngMode};
use crate::id::{ObjectArena, ObjectId, PlayerId, StackItemId};
use crate::rng::RngState;
use crate::view::PowerToughness;

/// Sentinel [`CardDefId`] used for created `TokenSpec` tokens that are not backed by a `CardDef`.
pub const TOKEN_CARD_DEF_ID: CardDefId = CardDefId(u32::MAX);

/// Sentinel [`ObjectId`] used for non-object replacement sources such as player-level prevention shields.
pub const NONE_OBJECT_ID: ObjectId = ObjectId {
    index: u32::MAX,
    generation: 0,
};

pub(crate) mod arc_serde {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(val: &Arc<T>, ser: S) -> Result<S::Ok, S::Error> {
        val.as_ref().serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        de: D,
    ) -> Result<Arc<T>, D::Error> {
        T::deserialize(de).map(Arc::new)
    }
}

pub(crate) mod opt_arc_serde {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(
        val: &Option<Arc<T>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        val.as_deref().serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        de: D,
    ) -> Result<Option<Arc<T>>, D::Error> {
        Option::<T>::deserialize(de).map(|opt| opt.map(Arc::new))
    }
}

mod vec_arc_serde {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(
        val: &[Arc<T>],
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(val.len()))?;
        for item in val {
            seq.serialize_element(item.as_ref())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        de: D,
    ) -> Result<Vec<Arc<T>>, D::Error> {
        Vec::<T>::deserialize(de).map(|v| v.into_iter().map(Arc::new).collect())
    }
}

mod vec_id_arc_serde {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(
        val: &[(ObjectId, Arc<T>)],
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(val.len()))?;
        for (id, item) in val {
            seq.serialize_element(&(*id, item.as_ref()))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        de: D,
    ) -> Result<Vec<(ObjectId, Arc<T>)>, D::Error> {
        Vec::<(ObjectId, T)>::deserialize(de).map(|v| {
            v.into_iter()
                .map(|(id, item)| (id, Arc::new(item)))
                .collect()
        })
    }
}

mod vec_u32_arc_serde {
    use super::*;

    pub fn serialize<S: Serializer, T: Serialize>(
        val: &[(u32, Arc<T>)],
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(val.len()))?;
        for (k, item) in val {
            seq.serialize_element(&(*k, item.as_ref()))?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        de: D,
    ) -> Result<Vec<(u32, Arc<T>)>, D::Error> {
        Vec::<(u32, T)>::deserialize(de)
            .map(|v| v.into_iter().map(|(k, item)| (k, Arc::new(item))).collect())
    }
}

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
    #[error("serialization error: {0}")]
    Serialization(String),
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
        const TOPOLOGY           = 1 << 0;
        const COUNTERS           = 1 << 1;
        const COMBAT             = 1 << 2;
        const LIFE               = 1 << 3;
        const GRAVEYARDS         = 1 << 4;
        const EXILE              = 1 << 5;
        const TURN_HISTORY       = 1 << 6;
        const TAPPED             = 1 << 7;
        const DESIGNATIONS       = 1 << 8;
        const CONTINUOUS_EFFECTS = 1 << 9;
        const PLAYER_STATE       = 1 << 10;
        const ALL                = 0x07FF;
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ObjectFlags: u32 {
        const TAPPED                 = 1 << 0;
        const SUMMONING_SICK         = 1 << 1;
        const PHASED_OUT             = 1 << 2;
        const IS_TOKEN               = 1 << 3;
        const IS_COMMANDER           = 1 << 4;
        const FACE_DOWN              = 1 << 5;
        const TRANSFORMED            = 1 << 6;
        const FLIPPED                = 1 << 7;
        const SADDLED                = 1 << 8;
        const MONSTROUS              = 1 << 9;
        const SUSPECTED              = 1 << 10;
        const PREPARED               = 1 << 11;
        const RENOWNED               = 1 << 12;
        const SOLVED                 = 1 << 13;
        const EXERTED                = 1 << 14;
        const PHASED_OUT_INDIRECTLY  = 1 << 15;
        const BESTOWED               = 1 << 16;
        const PROTOTYPED             = 1 << 17;
        const EVOKED                 = 1 << 18;
        const ESCAPED                = 1 << 19;
        const FORETOLD               = 1 << 20;
        const BLITZED                = 1 << 21;
        const DASHED                 = 1 << 22;
        const UNEARTH_LEAVE_EXILE    = 1 << 23;
        const IS_SUSPENDED           = 1 << 24;
        const CAST_VIA_SUSPEND       = 1 << 25;
        const WAS_CAST               = 1 << 26;
        const HARNESSED              = 1 << 27;
        const IS_EMBLEM              = 1 << 28;
        const IS_DUNGEON             = 1 << 29;
        const ENTERED_THIS_TURN      = 1 << 30;
        const DEALT_DAMAGE_THIS_TURN = 1 << 31;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum FaceDownKind {
    Morph,
    Megamorph,
    Disguise,
    Manifest,
    Cloak,
    Foretell,
    ExiledFaceDown,
    Cyberman,
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaceDownSpec {
    pub kind: FaceDownKind,
    pub turn_up_mana_cost: Option<ManaCost>,
    pub turn_up_cost_defs: Vec<CostDef>,
    pub ward_cost: Option<ManaCost>,
    pub supertypes: SupertypeMask,
    pub types: CardTypeMask,
    pub subtypes: SmallVec<[SubtypeId; 2]>,
    pub pt: Option<PowerToughness>,
    pub colors: ColorMask,
    pub known_to: SmallVec<[PlayerId; 4]>,
    pub opaque_seq: u32,
}

impl Default for FaceDownSpec {
    fn default() -> Self {
        Self {
            kind: FaceDownKind::Other,
            turn_up_mana_cost: None,
            turn_up_cost_defs: Vec::new(),
            ward_cost: None,
            supertypes: SupertypeMask::empty(),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            colors: ColorMask::empty(),
            known_to: SmallVec::new(),
            opaque_seq: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyExceptions {
    pub names: Option<SmallVec<[String; 2]>>,
    pub supertypes: Option<SupertypeMask>,
    pub remove_supertypes: SupertypeMask,
    pub types: Option<CardTypeMask>,
    pub add_types: CardTypeMask,
    pub subtypes: Option<SmallVec<[SubtypeId; 3]>>,
    pub add_subtypes: SmallVec<[SubtypeId; 2]>,
    pub colors: Option<ColorMask>,
    pub add_colors: ColorMask,
    pub add_keywords: SmallVec<[KeywordId; 2]>,
    pub no_mana_cost: bool,
    pub pt: Option<PowerToughness>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopiableState {
    pub card_def: CardDefId,
    pub face_index: Option<u8>,
    pub fused: bool,
    pub is_single_face_copy: bool,
    pub prototyped: bool,
    pub merged_components: SmallVec<[CardDefId; 4]>,
    pub except: Option<CopyExceptions>,
    pub modifications: Option<CopyModificationSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MergedComponent {
    pub card_def: CardDefId,
    pub is_token: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UntilSourceLeavesReturn {
    pub source: ObjectId,
    pub return_zone: Zone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayableFromExileState {
    pub player: PlayerId,
    pub expires_turn: Option<u32>,
    pub cast_with_mana_cost: Option<ManaCost>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AltCastKind {
    #[default]
    Normal,
    AdventureHalf,
    OmenHalf,
    SplitSecondHalf,
    ModalBackFace,
    Prototype,
    MorphFaceDown,
    DisguiseFaceDown,
    Bestow,
    Flashback,
    Escape,
    Disturb,
    Encore,
    Harmonize,
    Aftermath,
    JumpStart,
    Madness,
    Miracle,
    ForetellCast,
    PlotCast,
    Warp,
    Blitz,
    Dash,
    Evoke,
    Overload,
    Cleave,
    Fuse,
    Spree,
    Emerge,
    Mutate,
    CommandZoneCast,
    ExternalPermission(u16),
}

/// Rare per-object overlay allocated only when an object has non-default/special characteristics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RareObjectState {
    pub unlocked_faces: SmallVec<[u8; 2]>,
    pub class_level: Option<u16>,
    pub fused: bool,
    pub is_single_face_copy: bool,
    pub prototyped_face: Option<u8>,
    pub read_ahead_chapter: Option<u8>,
    pub melded_components: Option<[CardDefId; 2]>,
    pub merged_components: Vec<MergedComponent>,
    #[serde(with = "opt_arc_serde")]
    pub token_spec: Option<Arc<TokenSpec>>,
    #[serde(with = "opt_arc_serde")]
    pub copiable: Option<Arc<CopiableState>>,
    #[serde(with = "opt_arc_serde")]
    pub face_down: Option<Arc<FaceDownSpec>>,
    pub known_to_players: SmallVec<[PlayerId; 4]>,
    pub entered_with_x: Option<u32>,
    pub cast_from_zone: Option<Zone>,
    pub paid_mana_colors: ColorMask,
    pub mana_spent_count: u32,
    pub kicked_count: u16,
    pub chosen_color: Option<ColorMask>,
    pub chosen_subtype: Option<SubtypeId>,
    pub chosen_card_name: Option<String>,
    pub chosen_player: Option<PlayerId>,
    pub chosen_card_type: Option<CardTypeMask>,
    pub chosen_number: Option<i32>,
    pub chosen_object: Option<ObjectId>,
    pub exiled_by: Option<ObjectId>,
    pub until_source_leaves_return: Option<UntilSourceLeavesReturn>,
    pub commander_sba_declined_gen: Option<u32>,
    pub protector: Option<PlayerId>,
    pub default_controller: Option<PlayerId>,
    pub temp_power_mod: i32,
    pub temp_toughness_mod: i32,
    pub granted_keywords: SmallVec<[KeywordId; 2]>,
    pub counter_timestamps: SmallVec<[(CounterId, u64); 2]>,
    pub came_under_control_at: Option<u64>,
    pub goaded_by: SmallVec<[PlayerId; 2]>,
    pub playable_from_exile_by: Option<PlayableFromExileState>,
    pub used_loyalty_this_turn: bool,
    pub doesnt_untap_next_untap: bool,
    pub switched_pt_count: u8,
    pub created_by_source: Option<ObjectId>,
    pub chosen_colors: SmallVec<[ManaColor; 2]>,
    pub chosen_players: SmallVec<[PlayerId; 2]>,
    pub chosen_anchor_word: Option<String>,
    pub chosen_odd_parity: Option<bool>,
    pub dealt_damage_exile_sources: SmallVec<[ObjectId; 2]>,
    pub exile_instead_of_dying_this_turn: bool,
    pub on_an_adventure: bool,
    pub bargained: bool,
    pub additional_cost_paid: bool,
    pub alt_cast_kind: AltCastKind,
    pub kicked_tags: SmallVec<[String; 2]>,
    pub plotted_by: Option<PlayerId>,
    pub plotted_turn: Option<u32>,
}

impl RareObjectState {
    #[inline]
    pub fn is_default(&self) -> bool {
        self.unlocked_faces.is_empty()
            && self.class_level.is_none()
            && !self.fused
            && !self.is_single_face_copy
            && self.prototyped_face.is_none()
            && self.read_ahead_chapter.is_none()
            && self.melded_components.is_none()
            && self.merged_components.is_empty()
            && self.token_spec.is_none()
            && self.copiable.is_none()
            && self.face_down.is_none()
            && self.known_to_players.is_empty()
            && self.entered_with_x.is_none()
            && self.cast_from_zone.is_none()
            && self.paid_mana_colors.is_empty()
            && self.mana_spent_count == 0
            && self.kicked_count == 0
            && self.chosen_color.is_none()
            && self.chosen_subtype.is_none()
            && self.chosen_card_name.is_none()
            && self.chosen_player.is_none()
            && self.chosen_card_type.is_none()
            && self.chosen_number.is_none()
            && self.chosen_object.is_none()
            && self.exiled_by.is_none()
            && self.until_source_leaves_return.is_none()
            && self.commander_sba_declined_gen.is_none()
            && self.protector.is_none()
            && self.default_controller.is_none()
            && self.temp_power_mod == 0
            && self.temp_toughness_mod == 0
            && self.granted_keywords.is_empty()
            && self.counter_timestamps.is_empty()
            && self.came_under_control_at.is_none()
            && self.goaded_by.is_empty()
            && self.playable_from_exile_by.is_none()
            && !self.used_loyalty_this_turn
            && !self.doesnt_untap_next_untap
            && self.switched_pt_count == 0
            && self.created_by_source.is_none()
            && self.chosen_colors.is_empty()
            && self.chosen_players.is_empty()
            && self.chosen_anchor_word.is_none()
            && self.chosen_odd_parity.is_none()
            && self.dealt_damage_exile_sources.is_empty()
            && !self.exile_instead_of_dying_this_turn
            && !self.on_an_adventure
            && !self.bargained
            && !self.additional_cost_paid
            && self.alt_cast_kind == AltCastKind::Normal
            && self.kicked_tags.is_empty()
            && self.plotted_by.is_none()
            && self.plotted_turn.is_none()
    }
}

pub(crate) const AUX_HAS_COUNTERS: u8 = 1 << 0;
pub(crate) const AUX_HAS_RARE: u8 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameObject {
    pub card_def: CardDefId,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub zone: Zone,
    pub deathtouch_damage_marked: bool,
    pub active_face_index: u8,
    pub(crate) aux_flags: u8,
    pub flags: ObjectFlags,
    pub damage_marked: i32,
    pub entered_timestamp: u64,
    pub attached_to: Option<ObjectId>,
}

impl GameObject {
    #[inline]
    pub fn new(
        card_def: CardDefId,
        owner: PlayerId,
        zone: Zone,
        entered_timestamp: u64,
        flags: ObjectFlags,
    ) -> Self {
        Self {
            card_def,
            owner,
            controller: owner,
            zone,
            deathtouch_damage_marked: false,
            active_face_index: 0,
            aux_flags: 0,
            flags,
            damage_marked: 0,
            entered_timestamp,
            attached_to: None,
        }
    }

    #[inline]
    pub fn active_face_index(&self) -> u8 {
        self.active_face_index
    }

    #[inline]
    pub const fn has_counters(&self) -> bool {
        (self.aux_flags & AUX_HAS_COUNTERS) != 0
    }

    #[inline]
    pub const fn has_rare(&self) -> bool {
        (self.aux_flags & AUX_HAS_RARE) != 0
    }

    #[inline]
    pub fn is_phased_out(&self) -> bool {
        self.flags
            .intersects(ObjectFlags::PHASED_OUT | ObjectFlags::PHASED_OUT_INDIRECTLY)
    }

    #[inline]
    pub fn is_phased_out_directly(&self) -> bool {
        self.flags.contains(ObjectFlags::PHASED_OUT)
            && !self.flags.contains(ObjectFlags::PHASED_OUT_INDIRECTLY)
    }

    #[inline]
    pub fn is_phased_out_indirectly(&self) -> bool {
        self.flags.contains(ObjectFlags::PHASED_OUT_INDIRECTLY)
    }

    #[inline]
    pub fn is_token(&self) -> bool {
        self.flags.contains(ObjectFlags::IS_TOKEN)
    }

    #[inline]
    pub fn is_commander(&self) -> bool {
        self.flags.contains(ObjectFlags::IS_COMMANDER)
    }

    #[inline]
    pub fn is_face_down(&self) -> bool {
        self.flags.contains(ObjectFlags::FACE_DOWN)
    }
}

/// Layer-evaluated characteristics for a live [`GameObject`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluatedObject {
    pub pt: Option<PowerToughness>,
    pub base_pt: Option<PowerToughness>,
    pub types: CardTypeMask,
    pub subtypes: SmallVec<[SubtypeId; 4]>,
    pub supertypes: SupertypeMask,
    pub colors: ColorMask,
    pub mana_value: u32,
    pub keywords: SmallVec<[KeywordId; 4]>,
    pub has_all_creature_types: bool,
    pub lost_all_abilities: bool,
    pub loyalty: Option<i32>,
    pub defense: Option<i32>,
    pub controller: PlayerId,
    pub name_override: Option<String>,
    pub mana_cost: ManaCost,
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct StaticPresenceFlags: u16 {
        const HAND_VISIBILITY              = 1 << 0;
        const TOP_OF_LIBRARY_VISIBILITY    = 1 << 1;
        const ZONE_PLAY_PERMISSION         = 1 << 2;
        const ALT_CAST_PERMISSION          = 1 << 3;
        const GRANT_HAND_FORETELL          = 1 << 4;
        const CANT_ACTIVATE_ABILITIES      = 1 << 5;
        const ACTIVATE_AS_THOUGH_HASTE     = 1 << 6;
        const MAY_LOOK_AT_EXILED_WITH_SELF = 1 << 7;
    }
}

#[inline]
pub(crate) fn static_presence_flag_for_body(body: &mtg_ir::StaticBody) -> StaticPresenceFlags {
    match body {
        mtg_ir::StaticBody::HandVisibility { .. } => StaticPresenceFlags::HAND_VISIBILITY,
        mtg_ir::StaticBody::TopOfLibraryVisibility { .. } => {
            StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY
        }
        mtg_ir::StaticBody::ZonePlayPermission { .. } => StaticPresenceFlags::ZONE_PLAY_PERMISSION,
        mtg_ir::StaticBody::AltCastPermission { .. } => StaticPresenceFlags::ALT_CAST_PERMISSION,
        mtg_ir::StaticBody::GrantHandForetell { .. } => StaticPresenceFlags::GRANT_HAND_FORETELL,
        mtg_ir::StaticBody::CantActivateAbilities { .. } => {
            StaticPresenceFlags::CANT_ACTIVATE_ABILITIES
        }
        mtg_ir::StaticBody::ActivateAsThoughHaste { .. } => {
            StaticPresenceFlags::ACTIVATE_AS_THOUGH_HASTE
        }
        mtg_ir::StaticBody::MayLookAtExiledWithSelf => {
            StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF
        }
        _ => StaticPresenceFlags::empty(),
    }
}

/// Memoized board evaluation stored inside [`GameState`]'s `OnceLock<LayerCache>`.
#[derive(Debug, Clone, Default)]
pub struct LayerCache {
    pub rev: u64,
    pub reads_mask: DirtyMask,
    pub objects: Vec<Option<(u32, EvaluatedObject)>>,
    pub extras: Vec<Option<crate::layers::predicates::EvaluatedObjectExtras>>,
    pub players: SmallVec<[crate::layers::predicates::EvaluatedPlayerState; 4]>,
    pub all_off_battlefield_colorless: bool,
    pub may_have_off_bf_char_grants: bool,
    pub off_bf_grantor_ids: Vec<ObjectId>,
    pub static_presence: StaticPresenceFlags,
}

impl LayerCache {
    #[inline]
    pub fn get(&self, id: ObjectId) -> Option<&EvaluatedObject> {
        let slot = self.objects.get(id.index as usize)?.as_ref()?;
        if slot.0 == id.generation {
            Some(&slot.1)
        } else {
            None
        }
    }

    #[inline]
    pub fn extra(&self, id: ObjectId) -> Option<&crate::layers::predicates::EvaluatedObjectExtras> {
        let slot = self.objects.get(id.index as usize)?.as_ref()?;
        if slot.0 != id.generation {
            return None;
        }
        self.extras.get(id.index as usize)?.as_ref()
    }

    #[inline]
    pub fn player(&self, id: PlayerId) -> Option<&crate::layers::predicates::EvaluatedPlayerState> {
        self.players.get(id.0 as usize)
    }
}

/// Last Known Information snapshot captured prior to zone transitions / object disappearance (CR 113.7a, 608.2h).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LkiSnapshot {
    pub object_id: ObjectId,
    pub card_def: CardDefId,
    pub name: String,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub last_zone: Zone,
    pub pt: Option<PowerToughness>,
    pub types: CardTypeMask,
    pub subtypes: SmallVec<[SubtypeId; 4]>,
    pub supertypes: SupertypeMask,
    pub colors: ColorMask,
    pub mana_value: u32,
    pub keywords: SmallVec<[KeywordId; 4]>,
    pub counters: SmallVec<[(CounterId, u32); 2]>,
    pub damage_marked: i32,
    pub is_token: bool,
    pub is_commander: bool,
    pub tapped: bool,
    pub phased_out: bool,
    pub face_down: bool,
    pub lost_all_abilities: bool,
    pub has_all_creature_types: bool,
    pub attached_ids: SmallVec<[ObjectId; 2]>,
    pub chosen_color: Option<ColorMask>,
    pub chosen_subtype: Option<SubtypeId>,
    pub chosen_card_name: Option<String>,
    pub active_face_index: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackChoices {
    pub modes: SmallVec<[u16; 2]>,
    pub optional_costs: SmallVec<[u16; 2]>,
    pub divisions: SmallVec<[(TargetRef, u32); 4]>,
    pub fused: bool,
    pub cleaved: bool,
    pub gift_promised: bool,
    pub kicked_count: u16,
    pub alt_cast_kind: AltCastKind,
    pub additional_cost_paid: bool,
    pub bargained: bool,
    pub kicked_indices: SmallVec<[u16; 2]>,
    pub kicked_tags: SmallVec<[String; 2]>,
    pub cant_be_countered: bool,
    pub commander_bonus_counters: u32,
    pub face_index: u8,
    pub from_zone: Zone,
    pub exile_on_leave_stack: bool,
    pub is_copy: bool,
    pub bestowed: bool,
    pub mutating: bool,
    pub mutate_target: Option<ObjectId>,
    pub mutate_under: bool,
    pub resolution_started: bool,
    pub targets_by_spec: Vec<Vec<TargetRef>>,
    pub modes_chosen: bool,
    pub targets_chosen: bool,
}

impl Default for StackChoices {
    fn default() -> Self {
        Self {
            modes: SmallVec::new(),
            optional_costs: SmallVec::new(),
            divisions: SmallVec::new(),
            fused: false,
            cleaved: false,
            gift_promised: false,
            kicked_count: 0,
            alt_cast_kind: AltCastKind::Normal,
            additional_cost_paid: false,
            bargained: false,
            kicked_indices: SmallVec::new(),
            kicked_tags: SmallVec::new(),
            cant_be_countered: false,
            commander_bonus_counters: 0,
            face_index: 0,
            from_zone: Zone::Hand,
            exile_on_leave_stack: false,
            is_copy: false,
            bestowed: false,
            mutating: false,
            mutate_target: None,
            mutate_under: false,
            resolution_started: false,
            targets_by_spec: Vec::new(),
            modes_chosen: false,
            targets_chosen: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackEntryKind {
    Spell {
        object: ObjectId,
    },
    Activated {
        source: ObjectId,
        ability_idx: usize,
        #[serde(with = "arc_serde")]
        lki: Arc<LkiSnapshot>,
    },
    Triggered {
        source: ObjectId,
        ability_idx: usize,
        #[serde(with = "arc_serde")]
        lki: Arc<LkiSnapshot>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackEntry {
    pub id: StackItemId,
    pub kind: StackEntryKind,
    pub controller: PlayerId,
    pub choices: StackChoices,
    pub targets: Vec<TargetRef>,
    pub x_value: u32,
    pub paid_mana_colors: ColorMask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplacementSourceKind {
    PrintedReplacement,
    GrantedReplacement,
    MultiplierStatic,
    StaticPrevention,
    PreventionShield,
    TokenReplacementStatic,
    FinalityCounter,
    CommanderRedirect,
    KeywordAbsorb,
    ExileInsteadOfDyingEffect,
}

/// Provenance category for a [`ReplacementSourceId`] when `kind == ReplacementSourceKind::GrantedReplacement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplacementGrantOrigin {
    /// Not a granted replacement (innate printed/static ability, shield, counter, or redirect).
    Innate,
    /// Direct `LayerMod::GrantAbility` from a permanent, emblem, or graveyard card's static ability.
    StaticAbility,
    /// `LayerMod::GrantAbility` from a [`crate::layers::predicates::TimedContinuousEffect`].
    TimedEffect,
    /// Nested `LayerMod::GrantAbility` whose granting static was itself granted (`grant_effect_id`
    /// identifies the root granter).
    NestedStatic,
    /// `LayerMod::GrantAbilitiesFromObjects` (`grant_effect_id` identifies the donor `ObjectId`).
    GrantedFromObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplacementSourceId {
    pub kind: ReplacementSourceKind,
    /// Provenance kind distinguishing innate vs static/timed/nested/donor grants.
    pub grant_origin: ReplacementGrantOrigin,
    /// Active face index on `card_def` (disambiguates multi-face permanents / granters with
    /// abilities at matching `ability_index` across faces).
    pub face_index: u8,
    /// Mutate component ordinal (`0` for non-Mutate or primary top component, `1..` for
    /// `merged_components.iter().skip(1)`). Disambiguates duplicate `CardDefId` Mutate components.
    pub component_index: u8,
    /// Ability index within the supplying face, `TimedContinuousEffect` timestamp low bits, or
    /// `DamagePreventionShield.id`.
    pub index: u32,
    /// Object carrying or affected by the replacement ability (`NONE_OBJECT_ID` for player shields).
    pub object: ObjectId,
    /// Effective `CardDefId` of the face/component supplying the ability (or granter `CardDefId`
    /// for granted replacements; `None` for non-card or sourceless timed grants).
    pub card_def: Option<CardDefId>,
    /// Object granting this ability for `GrantedReplacement` (`None` for innate or sourceless
    /// `TimedContinuousEffect` grants). Disambiguates distinct granters of the same `CardDefId`.
    pub grant_source: Option<ObjectId>,
    /// Stable `TimedContinuousEffect::id`, root-granter identity for nested grants, or packed
    /// donor `ObjectId` for `GrantAbilitiesFromObjects` (`0` otherwise).
    pub grant_effect_id: u64,
}

impl ReplacementSourceId {
    #[inline]
    pub const fn printed(
        kind: ReplacementSourceKind,
        object: ObjectId,
        card_def: CardDefId,
        face_index: u8,
        index: u32,
    ) -> Self {
        Self::printed_component(kind, object, card_def, face_index, 0, index)
    }

    #[inline]
    pub const fn printed_component(
        kind: ReplacementSourceKind,
        object: ObjectId,
        card_def: CardDefId,
        face_index: u8,
        component_index: u8,
        index: u32,
    ) -> Self {
        Self {
            kind,
            grant_origin: ReplacementGrantOrigin::Innate,
            face_index,
            component_index,
            index,
            object,
            card_def: Some(card_def),
            grant_source: None,
            grant_effect_id: 0,
        }
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub const fn granted(
        kind: ReplacementSourceKind,
        grant_origin: ReplacementGrantOrigin,
        object: ObjectId,
        grant_source: Option<ObjectId>,
        card_def: Option<CardDefId>,
        face_index: u8,
        component_index: u8,
        index: u32,
        grant_effect_id: u64,
    ) -> Self {
        Self {
            kind,
            grant_origin,
            face_index,
            component_index,
            index,
            object,
            card_def,
            grant_source,
            grant_effect_id,
        }
    }

    #[inline]
    pub const fn non_card(kind: ReplacementSourceKind, object: ObjectId, index: u32) -> Self {
        Self {
            kind,
            grant_origin: ReplacementGrantOrigin::Innate,
            face_index: 0,
            component_index: 0,
            index,
            object,
            card_def: None,
            grant_source: None,
            grant_effect_id: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShieldRemaining {
    Finite(u32),
    Infinite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShieldTarget {
    Any,
    Player(PlayerId),
    Permanent(ObjectId),
    PermanentsControlledBy {
        controller: PlayerId,
        types: CardTypeMask,
    },
    PlayerAndPermanentsControlledBy {
        player: PlayerId,
        types: CardTypeMask,
    },
    YouAndPlaneswalkersYouControl(PlayerId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DamagePreventionShield {
    pub id: u32,
    pub controller: PlayerId,
    pub target: ShieldTarget,
    pub remaining: ShieldRemaining,
    pub combat_only: bool,
    pub next_event_only: bool,
    pub also_prevent_dealt_by_target: bool,
    pub from_source_object: Option<ObjectId>,
    pub from_source_stack_item: Option<StackItemId>,
    pub from_attacking_only: bool,
    pub from_source_types: CardTypeMask,
    pub from_source_colors: ColorMask,
    pub from_source_not_subtypes: SmallVec<[SubtypeId; 2]>,
    pub from_sources_you_dont_control: bool,
    pub redirect_to: Option<TargetRef>,
    pub min_life_floor: Option<i32>,
    pub on_prevented: Option<mtg_ir::OnPreventedRider>,
    pub on_prevented_creature: Option<mtg_ir::PreventReflectTarget>,
    pub on_prevented_noncreature: Option<mtg_ir::PreventReflectTarget>,
    pub on_prevented_to_damage_source_controller: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplaceableEventKind {
    ZoneChange {
        object: ObjectId,
        from: Zone,
        to: Zone,
        controller: PlayerId,
    },
    Draw {
        player: PlayerId,
        count: u32,
        source: Option<ObjectId>,
    },
    Damage {
        source: Option<ObjectId>,
        target: TargetRef,
        amount: u32,
        is_combat: bool,
        unpreventable_by_effect: bool,
    },
    LifeGain {
        player: PlayerId,
        amount: u32,
        source: Option<ObjectId>,
    },
    PutCounters {
        object: ObjectId,
        counter: CounterId,
        count: u32,
        placer: Option<PlayerId>,
        source: Option<ObjectId>,
    },
    PutPlayerCounters {
        player: PlayerId,
        counter: CounterId,
        count: u32,
        placer: Option<PlayerId>,
        source: Option<ObjectId>,
    },
    CreateToken {
        controller: PlayerId,
        spec: Option<Box<TokenSpec>>,
        count: u32,
        source: Option<ObjectId>,
    },
    Untap {
        object: ObjectId,
    },
    Proliferate {
        player: PlayerId,
        count: u32,
        source: Option<ObjectId>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EtbInteractiveStep {
    PayLifeOrEnterTapped {
        life: u16,
    },
    RevealFromHandOrEnterTapped {
        candidates: Vec<ObjectId>,
    },
    ChooseSubType {
        options: Vec<SubtypeId>,
    },
    ChooseColor {
        colors: Vec<ManaColor>,
    },
    ChooseTwoColors {
        first: Option<ManaColor>,
        colors: Vec<ManaColor>,
    },
    ChooseCardName {
        candidates: Vec<String>,
    },
    ChooseCounter {
        options: SmallVec<[CounterId; 3]>,
    },
    ChooseAnchorWord {
        words: [String; 2],
    },
    ChoosePlayer {
        candidates: Vec<PlayerId>,
    },
    ChooseTwoPlayers {
        first: Option<PlayerId>,
        candidates: Vec<PlayerId>,
    },
    ChooseParity,
    ChooseCardType {
        types: Vec<CardTypeMask>,
    },
    ChooseNumber {
        min: i32,
        max: i32,
    },
    ChooseObjectOnBattlefield {
        candidates: Vec<ObjectId>,
        optional: bool,
    },
    /// CR 616.1b: `enter_under_opponent_control` opponent selection in 3+ player games.
    ChooseOpponentForControl {
        candidates: Vec<PlayerId>,
    },
    DiscardForKeywordCounters {
        candidates: Vec<ObjectId>,
        tracked_keywords: SmallVec<[KeywordId; 4]>,
        picked: Vec<ObjectId>,
    },
    RemoveCountersForPlusCounters {
        candidates: Vec<ObjectId>,
        multiplier: u16,
        picked: Vec<ObjectId>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PendingReplacementPrompt {
    /// `ChoiceRequest::ChooseReplacement { prompt_id, option_count }`
    ChooseReplacement,
    /// `CR 903.9b`: `ChoiceRequest::YesNo` for moving commander from Hand/Library to Command.
    CommanderRedirect { intended_zone: Zone },
    /// `CR 616.1c`: `ChoiceRequest::ChooseObjects` for ETB Clone copy.
    EtbCopy {
        source_id: ReplacementSourceId,
        source_controller: PlayerId,
        mod_spec: Box<mtg_ir::ZoneChangeMod>,
        candidates: Vec<ObjectId>,
        optional: bool,
        tapped_if_copied: bool,
        modifications: Option<Box<CopyModificationSpec>>,
    },
    /// `CR 614.12`: Interactive self-ETB choice (`EtbChoiceSpec`, shockland pay-life, reveal-land).
    EtbChoice {
        source_id: ReplacementSourceId,
        source_controller: PlayerId,
        mod_spec: Box<mtg_ir::ZoneChangeMod>,
        step: EtbInteractiveStep,
        step_idx: u16,
    },
    /// *Abundance* draw choice (Yes = land, No = nonland).
    AbundanceChoice { source_id: ReplacementSourceId },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplacementAccumulatedMods {
    pub skipped: bool,
    pub execute_instead: Option<Vec<EffectStep>>,
    pub shuffle_into_library: bool,
    pub enter_tapped: Option<bool>,
    pub enters_prepared: bool,
    pub becomes_day_if_neither: bool,
    pub exile_with_counter: Option<CounterId>,
    pub extra_counters: SmallVec<[(CounterId, u32); 2]>,
    pub extra_tokens: Vec<(TokenSpec, u32)>,
    pub replace_tokens: Vec<(TokenSpec, u32)>,
    pub redirected_damage: Vec<(TargetRef, u32)>,
    pub prevented_damage: u32,
    pub prevented_riders: Vec<(DamagePreventionShield, u32)>,
    pub damage_as_minus_counters: bool,
    pub damage_to_self_as_counter: Option<CounterId>,
    pub draw_stealer: Option<PlayerId>,
    pub abundance_land: Option<bool>,
    pub reanimate_on_empty_draw: Option<(Box<mtg_ir::ObjectFilter>, bool)>,
    pub regenerate_instead: bool,
    pub exile_and_extra_turn_on_bf_to_gy: bool,
    pub dies_exile_and_reflexive: Option<(ManaCost, Box<TokenSpec>, String)>,
    pub commander_answered_zones: SmallVec<[Zone; 2]>,
    pub etb_answered_steps: SmallVec<[u16; 4]>,
    pub etb_pre_done: bool,
    pub etb_steal_done: bool,
    pub etb_copy_done: bool,
    pub move_opts: Option<crate::zones::MoveRawOptions>,
    pub token_opts: Option<crate::zones::CreateTokenOptions>,
    pub lki_eval: Option<EvaluatedObject>,
}

impl ReplacementAccumulatedMods {
    pub(crate) fn reset_zone_mod_progress(&mut self) {
        self.etb_pre_done = false;
        self.etb_steal_done = false;
        self.etb_copy_done = false;
        self.etb_answered_steps.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEtbCounterMove {
    pub raw: crate::zones::MoveRawOutcome,
    pub from_player: Option<PlayerId>,
    pub to_player: Option<PlayerId>,
    pub controller: PlayerId,
    pub remaining_counters: SmallVec<[(CounterId, u32); 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplacementContinuation {
    ZoneMove,
    ZoneMoveEtbCounters(Box<PendingEtbCounterMove>),
    DamageEffect,
    DrawEffect,
    LifeGainEffect,
    PutCountersEffect,
    PutPlayerCountersEffect,
    CreateTokenEffect,
    UntapEffect,
    ProliferateEffect,
}

/// Serializable in-progress replacement state so `ChoiceRequest::ChooseReplacement` and ETB
/// choice pauses survive snapshots (`CR 614.5`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReplacementState {
    pub prompt_id: u32,
    pub chooser: PlayerId,
    pub event: ReplaceableEventKind,
    pub applied: SmallVec<[ReplacementSourceId; 4]>,
    pub candidates: Vec<ReplacementSourceId>,
    pub prompt_kind: PendingReplacementPrompt,
    pub mods: ReplacementAccumulatedMods,
    pub continuation: ReplacementContinuation,
    pub resume_decision: DecisionState,
    pub resume_priority_player: PlayerId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestrictedManaBucket {
    pub colors: [u32; 6], // W, U, B, R, G, C
    #[serde(with = "arc_serde")]
    pub restriction: Arc<ManaSpendingRestriction>,
    pub source_id: Option<ObjectId>,
    pub source_tag: Option<ManaRestrictionTag>,
    pub source_kind_flags: u8, // bit 0=Creature, 1=BasicLand, 2=Snow, 3=Treasure, 4=ChosenColor
    pub is_snow: bool,
    pub retain_until_eot: bool,
    #[serde(with = "opt_arc_serde")]
    pub delayed_rider: Option<Arc<DelayedTriggerPayload>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingManaDelayedRider {
    pub controller: PlayerId,
    pub source: Option<ObjectId>,
    #[serde(with = "arc_serde")]
    pub payload: Arc<DelayedTriggerPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingZoneCostKind {
    Sacrifice,
    Discard,
    ExileFrom {
        source: ObjectId,
        collect_evidence: bool,
        accumulated_mv: u32,
    },
    ForageExile,
    ForageFood,
    ReturnToHand,
    Mill {
        remaining_after_current: usize,
    },
    PutOnTopOfLibrary,
    PutOnBottomOfLibrary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingEffectReplaceKind {
    Untap {
        current_object: ObjectId,
        remaining_objects: Vec<ObjectId>,
    },
    OpponentGainsLife {
        current_opponent: PlayerId,
        remaining_opponents: Vec<PlayerId>,
        amount: u32,
    },
    LoyaltyPositive {
        counter: CounterId,
    },
    PutCounters,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingNonManaCostStep {
    WaitingCostPrompt,
    ZoneMove {
        kind: PendingZoneCostKind,
        current_object: ObjectId,
        remaining_objects: Vec<ObjectId>,
    },
    EffectReplace {
        kind: PendingEffectReplaceKind,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastOption {
    pub option_id: u16,
    pub face_idx: u8,
    pub from_zone: Zone,
    pub alt_kind: AltCastKind,
    pub kicker_indices: SmallVec<[u16; 2]>,
    pub is_kicked: bool,
    pub gift_promised: bool,
    pub fused: bool,
    pub cleaved: bool,
    pub spree_required: bool,
    pub optional_cost_indices: SmallVec<[u16; 2]>,
    pub mana_cost_override: Option<ManaCost>,
    pub extra_costs: Vec<CostDef>,
    pub permission_source: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CastKind {
    Spell,
    ActivatedAbility {
        ability_index: u16,
        is_loyalty: bool,
    },
    TriggeredAbility {
        stack_item: StackItemId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCostPaymentPlan {
    pub locked_mana_cost: ManaCost,
    pub remaining_mana: ManaCost,
    pub remaining_non_mana_costs: Vec<CostDef>,
    pub non_mana_cursor: usize,
    pub pending_step: Option<PendingNonManaCostStep>,
    pub chosen_optional_costs: SmallVec<[u16; 2]>,
    pub convoke_tapped: SmallVec<[ObjectId; 4]>,
    pub improvise_tapped: SmallVec<[ObjectId; 4]>,
    pub delve_exiled: SmallVec<[ObjectId; 4]>,
    pub emerge_sacrificed: Option<ObjectId>,
    pub waterbend_tapped: SmallVec<[ObjectId; 4]>,
    #[serde(with = "vec_arc_serde")]
    pub sacrificed_lki: Vec<Arc<LkiSnapshot>>,
    pub revealed_cards: Vec<ObjectId>,
    pub revealed_card_defs: Vec<CardDefId>,
    pub discarded_cards: Vec<ObjectId>,
    pub counters_removed: u32,
    pub phyrexian_pay_life: SmallVec<[bool; 4]>,
    pub cant_be_countered_from_mana: bool,
    pub mana_commander_bonus_counters: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CastStep {
    ChooseModes,
    ChooseX,
    ChooseOptionalCosts,
    ChooseTargets,
    ChooseDivisions,
    PayCosts,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreCastExileState {
    pub foretold: bool,
    pub was_face_down: bool,
    pub is_suspended: bool,
    pub unearth_leave_exile: bool,
    #[serde(with = "opt_arc_serde")]
    pub face_down: Option<Arc<FaceDownSpec>>,
    pub known_to_players: SmallVec<[PlayerId; 4]>,
    pub exiled_by: Option<ObjectId>,
    pub playable_from_exile_by: Option<PlayableFromExileState>,
    pub plotted_by: Option<PlayerId>,
    pub plotted_turn: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastingState {
    pub caster: PlayerId,
    pub object: ObjectId,
    pub face_idx: u8,
    pub from_zone: Zone,
    pub step: CastStep,
    pub chosen_modes: SmallVec<[u16; 2]>,
    pub x_value: Option<u32>,
    pub optional_costs: SmallVec<[u16; 2]>,
    pub optional_cost_cursor: usize,
    pub targets: Vec<TargetRef>,
    pub divisions: SmallVec<[(TargetRef, u32); 4]>,
    pub locked_cost: ManaCost,
    pub mana_paid: SmallVec<[ManaPip; 4]>,
    pub paid_mana_colors: ColorMask,
    pub kind: CastKind,
    pub option_id: u16,
    pub alt_kind: AltCastKind,
    pub is_kicked: bool,
    pub kicked_indices: SmallVec<[u16; 2]>,
    pub kicked_tags: SmallVec<[String; 2]>,
    pub fused: bool,
    pub cleaved: bool,
    pub gift_promised: bool,
    pub additional_cost_paid: bool,
    pub bargained: bool,
    pub target_spec_cursor: usize,
    pub targets_by_spec: Vec<Vec<TargetRef>>,
    pub division_cursor: usize,
    pub payment_plan: Option<Box<PendingCostPaymentPlan>>,
    pub paid_mana_sources: SmallVec<[ObjectId; 8]>,
    pub cant_be_countered_from_mana: bool,
    pub mana_commander_bonus_counters: u32,
    pub mana_spent_total: u32,
    pub cast_option: Option<Box<CastOption>>,
    pub pre_cast_exile: Option<PreCastExileState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CostSelectionKind {
    Sacrifice,
    Discard,
    ExileFromGraveyard,
    ExileFromHand,
    ReturnToHand,
    RevealFromHand,
    TapUntapped,
    RemoveCounters,
    Convoke,
    Improvise,
    Delve,
    Emerge,
    Waterbend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostPromptKind {
    ObjectSelection {
        cost_index: usize,
        kind: CostSelectionKind,
        candidates: Vec<ObjectId>,
        min: u16,
        max: u16,
        picked: Vec<ObjectId>,
    },
    BranchChoice {
        cost_index: usize,
        option_count: u16,
    },
    OptionalPay {
        cost_index: usize,
        label: String,
    },
    ChooseNumber {
        cost_index: usize,
        min: i32,
        max: i32,
    },
    ChooseMode {
        cost_index: usize,
        option_count: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostPaymentPromptState {
    pub controller: PlayerId,
    pub source: ObjectId,
    pub kind: CostPromptKind,
    pub resume_decision: DecisionState,
    pub resume_priority_player: PlayerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManaAbilityPromptState {
    pub source: ObjectId,
    pub ability_index: u16,
    pub controller: PlayerId,
    pub single_colors: SmallVec<[ManaColor; 6]>,
    pub bundle_options: Vec<SmallVec<[ManaColor; 3]>>,
    pub resume_decision: DecisionState,
    pub resume_priority_player: PlayerId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CastingPromptKind {
    CastModes {
        min: u16,
        max: u16,
        allow_duplicates: bool,
        option_count: u16,
        picked: SmallVec<[u16; 4]>,
    },
    CastXValue {
        min: i32,
        max: i32,
    },
    CastOptionalCost {
        cost_index: u16,
        label: String,
    },
    CostObjectSelection {
        cost_index: u16,
        kind: CostSelectionKind,
        candidates: Vec<ObjectId>,
        min: u16,
        max: u16,
        picked: Vec<ObjectId>,
    },
    CostBranchChoice {
        cost_index: u16,
        branches: Vec<Vec<CostDef>>,
    },
    CostPayLifeOrNot {
        cost_index: u16,
        life: u32,
    },
    GuidedFallback {
        source: ObjectId,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SbaPromptKind {
    LegendRule {
        owner: PlayerId,
        name: String,
        candidates: Vec<ObjectId>,
    },
    CommanderReturn {
        owner: PlayerId,
        commander: ObjectId,
        from_zone: Zone,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnPromptKind {
    CleanupDiscard {
        remaining: u16,
        picked: Vec<ObjectId>,
    },
    UntapOptional {
        candidates: Vec<ObjectId>,
        next_idx: usize,
    },
    EndTurnMay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CombatPromptKind {
    DamageAssignmentOrder {
        attacker: ObjectId,
        unordered_blockers: Vec<ObjectId>,
        ordered_blockers: Vec<ObjectId>,
    },
    AttackerDamageDivide {
        is_first_strike: bool,
        attacker: ObjectId,
        blocker_idx: usize,
        remaining_damage: u32,
    },
    BlockerDamageDivide {
        is_first_strike: bool,
        blocker: ObjectId,
        attacker_idx: usize,
        remaining_damage: u32,
    },
    SuperTrampleAsUnblocked {
        is_first_strike: bool,
        attacker: ObjectId,
    },
    AttackTaxPay,
    BlockTaxPay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PromptOwner {
    Replacement,
    Casting(CastingPromptKind),
    ManaAbility(ManaAbilityPromptState),
    CostPayment(CostPaymentPromptState),
    TriggerOrder,
    ResolutionFrame,
    Sba(SbaPromptKind),
    Turn(TurnPromptKind),
    Combat(CombatPromptKind),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingPromptContext {
    pub prompt_id: u32,
    pub chooser: PlayerId,
    pub request: ChoiceRequest,
    pub owner: PromptOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttackerEntry {
    pub attacker: ObjectId,
    pub target: AttackTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockerEntry {
    pub blocker: ObjectId,
    pub attacker: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledDamagePacket {
    pub source: Option<ObjectId>,
    pub source_controller: PlayerId,
    pub target: TargetRef,
    pub amount: u32,
    pub is_commander: bool,
    pub has_deathtouch: bool,
    pub has_lifelink: bool,
    pub has_infect: bool,
    pub has_wither: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCombatDamageState {
    pub packets: Vec<ScheduledDamagePacket>,
    pub packet_idx: usize,
    pub attacker_cursor: usize,
    pub blocker_cursor: usize,
    pub lifelink_gains: Vec<(PlayerId, u32)>,
    pub lifelink_idx: usize,
    pub combat_damaged_players: Vec<(PlayerId, Vec<ObjectId>)>,
    pub is_first_strike_step: bool,
    pub chosen_allocations: Vec<(ObjectId, ObjectId, u32)>,
    pub chosen_super_trample: Vec<(ObjectId, bool)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombatState {
    pub active: bool,
    pub attackers: Vec<AttackerEntry>,
    pub blockers: Vec<BlockerEntry>,
    pub damage_assignment_order: Vec<(ObjectId, Vec<ObjectId>)>,
    pub first_strike_dealt: bool,
    pub combat_damage_dealt: bool,
    pub attacked_this_combat: Vec<ObjectId>,
    pub attackers_declared: bool,
    pub declared_blockers_players: Vec<PlayerId>,
    pub blocked_attackers: Vec<ObjectId>,
    pub dealt_first_strike_ids: Vec<ObjectId>,
    pub pending_damage: Option<Box<PendingCombatDamageState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnReplacementContext {
    UntapStep,
    DrawStep { remaining_draws: u32 },
    CleanupDiscard { remaining_excess: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplacementResumeHook {
    ResolutionFrame,
    StackResolutionFinish {
        attach_target: Option<ObjectId>,
        is_adventure: bool,
    },
    CombatDamageStep,
    SbaLoop {
        in_cleanup: bool,
    },
    TurnStep(TurnReplacementContext),
    MegamorphTurnFaceUp {
        object: ObjectId,
        player: PlayerId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSbaBatchState {
    pub remaining_graveyard_ids: Vec<ObjectId>,
    pub in_cleanup: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayedSpellCopyGrant {
    pub controller: PlayerId,
    pub source: Option<ObjectId>,
    pub count: u16,
    pub filter: Option<mtg_ir::ObjectFilter>,
    pub expires_turn: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedCopiableBackup {
    pub object: ObjectId,
    pub source: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub prev_copiable: Option<Arc<CopiableState>>,
    #[serde(with = "opt_arc_serde")]
    pub prev_token_spec: Option<Arc<TokenSpec>>,
    pub duration: mtg_ir::Duration,
    pub expires_turn: Option<u32>,
    pub expires_on_player: Option<PlayerId>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RareTurnState {
    pub extra_turn_queue: Vec<PlayerId>,
    pub skip_next_combat_phases: Vec<(PlayerId, u8)>,
    pub skip_next_turns: Vec<(PlayerId, u8)>,
    pub cant_be_regenerated_this_turn: Vec<ObjectId>,
    pub regeneration_shields: Vec<(ObjectId, u16)>,
    pub pending_delayed_next_spell_copies: Vec<DelayedSpellCopyGrant>,
    pub pending_next_main_phase_mana: Vec<(PlayerId, ManaColor, u32)>,
    pub must_attack_next_combat: Vec<ObjectId>,
    pub must_attack_player_this_turn: Vec<(ObjectId, PlayerId)>,
    pub timed_copiable_backups: Vec<TimedCopiableBackup>,
}

impl RareTurnState {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.extra_turn_queue.is_empty()
            && self.skip_next_combat_phases.is_empty()
            && self.skip_next_turns.is_empty()
            && self.cant_be_regenerated_this_turn.is_empty()
            && self.regeneration_shields.is_empty()
            && self.pending_delayed_next_spell_copies.is_empty()
            && self.pending_next_main_phase_mana.is_empty()
            && self.must_attack_next_combat.is_empty()
            && self.must_attack_player_this_turn.is_empty()
            && self.timed_copiable_backups.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepAcceptance {
    Pending,
    Accepted,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReorderMode {
    Scry,
    Surveil,
    LookAndReorder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumberPausePurpose {
    CounterSpellUnlessPays,
    UpToDraw,
    GenericAmount,
    DivideDamage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModePausePurpose {
    PunisherChoice,
    DesignationChoice,
    GrantKeywordChoice,
    OddOrEvenChoice,
    ModalSuffixOrReflexive,
    PartitionChoice,
    ProliferatePlayers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum YesNoPausePurpose {
    OptionalSpec { is_unless: bool },
    MayShuffleAfterMoveOrLook,
    ScryPutBottom,
    SurveilPutGraveyard,
    EndTurnActiveMay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepPauseState {
    OptionalChoice {
        chooser: PlayerId,
        purpose: YesNoPausePurpose,
    },
    SelectionPick {
        chooser: PlayerId,
        candidates: Vec<ObjectId>,
        selected: Vec<ObjectId>,
        min: u16,
        max: u16,
    },
    SearchPick {
        chooser: PlayerId,
        searched_player: PlayerId,
        candidates: Vec<ObjectId>,
        selected: Vec<ObjectId>,
        min: u16,
        max: u16,
        destinations: Vec<mtg_ir::SearchDestination>,
        dest_idx: usize,
        moved_ids: Vec<ObjectId>,
        shuffle_library: bool,
        reveal: bool,
        tapped: bool,
    },
    ReorderLibrary {
        player: PlayerId,
        mode: ReorderMode,
        remaining: Vec<ObjectId>,
        top_order: Vec<ObjectId>,
        bottom_or_gy: Vec<ObjectId>,
        deciding_card: Option<ObjectId>,
        may_shuffle: bool,
    },
    DigPick {
        player: PlayerId,
        revealed: Vec<ObjectId>,
        candidates: Vec<ObjectId>,
        selected: Vec<ObjectId>,
        min: u16,
        max: u16,
        partition_picks: Vec<ObjectId>,
    },
    ChooseNumber {
        chooser: PlayerId,
        purpose: NumberPausePurpose,
    },
    ChooseMode {
        chooser: PlayerId,
        purpose: ModePausePurpose,
        options: Vec<String>,
        picked: SmallVec<[u16; 4]>,
        allow_duplicates: bool,
        min: u16,
        max: u16,
    },
    MoveCountersSelectTarget {
        chooser: PlayerId,
        from_id: ObjectId,
        counter: CounterId,
        candidates: Vec<ObjectId>,
        remaining: u32,
        transfers: Vec<(ObjectId, u32)>,
    },
    MoveCountersChooseAmount {
        chooser: PlayerId,
        from_id: ObjectId,
        counter: CounterId,
        candidates: Vec<ObjectId>,
        target_id: ObjectId,
        remaining: u32,
        transfers: Vec<(ObjectId, u32)>,
    },
    GuidedResolution {
        controller: PlayerId,
        source: ObjectId,
    },
    WaitingOnReplacement {
        batch_remaining: Vec<ObjectId>,
        batch_idx: usize,
        op_tag: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlFlowFrame {
    ForEachPlayer {
        players: Vec<PlayerId>,
        next_player_idx: usize,
        steps: Vec<EffectStep>,
        step_pc: usize,
    },
    RepeatLoop {
        steps: Vec<EffectStep>,
        step_pc: usize,
        iterations: u16,
        max_iterations: u16,
        condition: mtg_ir::Condition,
    },
    SubSteps {
        steps: Vec<EffectStep>,
        step_pc: usize,
        event_object: Option<ObjectId>,
        event_player: Option<PlayerId>,
        event_amount: Option<i32>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepWayStats {
    pub did_last_optional_step: bool,
    pub died_this_way: u32,
    pub creature_card_milled_this_way: bool,
    pub land_card_milled_this_way: bool,
    pub discarded_nonland_this_way: bool,
    pub discarded_creature_this_way: bool,
    pub discarded_noncreature_this_way: bool,
    pub not_drawn_cards_this_way: bool,
    pub did_reveal_this_way: bool,
    pub sacrificed_nonland_this_way: bool,
    pub didnt_put_onto_bf_this_way: bool,
    pub source_exiled_this_way: bool,
    pub player_didnt_discard_nonland_this_way: bool,
    pub player_discarded_hand_this_way: bool,
    pub exiled_count_this_way: u32,
    pub distinct_sacrificed_types_mask: u16,
    pub returned_filter_to_hand_this_way: bool,
    pub not_put_onto_bf_by_this_ability: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigMoveRole {
    PartitionHand,
    PartitionSecond,
    PartitionBottom,
    PickKept,
    PickRest,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingStepContinuation {
    pub remaining_players: SmallVec<[PlayerId; 4]>,
    pub remaining_damage: Vec<(Option<ObjectId>, TargetRef, u32)>,
    pub damage_do_excess: bool,
    pub fight_return_damage: Option<(ObjectId, ObjectId, u32)>,
    pub fight_excess_mana: Option<ManaColor>,
    pub search_moved_ids: Vec<ObjectId>,
    pub search_destinations: Vec<mtg_ir::SearchDestination>,
    pub search_dest_idx: usize,
    pub search_different_names: bool,
    pub search_candidates: Vec<ObjectId>,
    pub search_searched_player: Option<PlayerId>,
    pub search_chooser: Option<PlayerId>,
    pub search_shuffle: bool,
    pub search_reveal: bool,
    pub search_tapped: bool,
    pub move_shuffle_owners: SmallVec<[PlayerId; 4]>,
    pub proliferate_count: u32,
    pub proliferate_selected_perms: Vec<ObjectId>,
    pub proliferate_counter_queue: Vec<(ObjectId, CounterId)>,
    pub proliferate_chosen_players: SmallVec<[PlayerId; 4]>,
    pub dig_player: Option<PlayerId>,
    pub dig_roles: SmallVec<[DigMoveRole; 8]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionVmState {
    pub stack_item: StackItemId,
    pub controller: PlayerId,
    pub source: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub source_lki: Option<Arc<LkiSnapshot>>,
    pub x_value: u32,
    pub targets: Vec<TargetRef>,
    pub targets_by_spec: Vec<Vec<TargetRef>>,
    pub divisions: SmallVec<[(TargetRef, u32); 4]>,
    pub step_results: Vec<StepAcceptance>,
    pub pause_state: Option<StepPauseState>,
    pub pending_continuation: Option<Box<PendingStepContinuation>>,
    pub subframes: Vec<ControlFlowFrame>,
    pub step_acted_ids: Vec<ObjectId>,
    pub already_selected_controllers: SmallVec<[PlayerId; 4]>,
    pub each_loop_player: Option<PlayerId>,
    pub chosen_opponent: Option<PlayerId>,
    pub event_object: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub event_lki: Option<Arc<LkiSnapshot>>,
    pub event_player: Option<PlayerId>,
    pub event_amount: Option<i32>,
    pub defending_player: Option<PlayerId>,
    pub last_effect_amount: i32,
    pub last_excess_damage: i32,
    pub last_counters_removed: i32,
    pub last_roll_result: i32,
    pub coin_heads_count: i32,
    pub counters_removed_this_way: i32,
    pub step_way_stats: StepWayStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionBinding {
    pub name: String,
    pub objects: Vec<ObjectId>,
    #[serde(with = "vec_arc_serde")]
    pub lki: Vec<Arc<LkiSnapshot>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionFrame {
    pub stack_item: StackItemId,
    pub steps_remaining: Vec<EffectStep>,
    pub pc: usize,
    pub bindings: Vec<ResolutionBinding>,
    pub pending_prompt_id: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerManaState {
    pub snow_mana_pool: [u32; 6],
    pub retained_mana_counts: [u32; 6],
    pub restricted_mana: Vec<RestrictedManaBucket>,
    pub unemptied_mana_colors_this_turn: ColorMask,
    pub once_per_turn_abilities_used: Vec<(ObjectId, u16)>,
    pub once_per_game_abilities_used: Vec<(ObjectId, u16)>,
}

impl PlayerManaState {
    #[inline]
    pub fn is_default(&self) -> bool {
        self.snow_mana_pool == [0; 6]
            && self.retained_mana_counts == [0; 6]
            && self.restricted_mana.is_empty()
            && self.unemptied_mana_colors_this_turn.is_empty()
            && self.once_per_turn_abilities_used.is_empty()
            && self.once_per_game_abilities_used.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ManaRiderState {
    pub pending: Vec<PendingManaDelayedRider>,
    pub stack: Vec<(StackItemId, Vec<PendingManaDelayedRider>)>,
}

impl ManaRiderState {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.stack.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub id: PlayerId,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub experience: u16,
    pub rad: u16,
    pub library: Vec<ObjectId>,
    pub hand: Vec<ObjectId>,
    pub battlefield: Vec<ObjectId>,
    pub graveyard: Vec<ObjectId>,
    pub exile: Vec<ObjectId>,
    pub command: Vec<ObjectId>,
    pub commander_color_identity: ColorMask,
    /// Command-zone cast counts keyed by `ObjectId.index` (survives `CR 400.7` generation bumps).
    pub commander_cast_counts: SmallVec<[(u32, u16); 2]>,
    /// Commander combat damage taken by this player keyed by commander `ObjectId.index` (CR 903.10a).
    pub commander_damage_taken: SmallVec<[(u32, i32); 4]>,
    pub mulligan_count: u8,
    pub has_kept_hand: bool,
    pub lands_played_this_turn: u8,
    pub max_lands_per_turn: u8,
    pub mana_pool: [u32; 6],
    #[serde(with = "opt_arc_serde")]
    pub mana_ext: Option<Arc<PlayerManaState>>,
    pub used_first_activation_zero_mask: u8,
    pub attempted_empty_library_draw: bool,
    pub life_gain_events_this_turn: u16,
    pub life_loss_events_this_turn: u16,
    pub draw_step_draws_this_turn: u16,
    pub life_gained_this_turn: u32,
    pub life_lost_this_turn: u32,
    pub cards_drawn_this_turn: u32,
    pub cards_discarded_this_turn: u32,
    pub spells_cast_this_turn: u32,
    pub noncreature_spells_cast_this_turn: u32,
    pub creatures_died_this_turn: u32,
    pub nontoken_creatures_died_this_turn: u32,
    pub permanents_left_bf_this_turn: u32,
    pub committed_crime_this_turn: bool,
    pub descended_this_turn: u32,
    pub mana_expended_this_turn: u32,
    pub damage_dealt_to_this_turn: u32,
    pub targeted_by_this_turn: SmallVec<[PlayerId; 4]>,
    pub skip_draw_step_once: bool,
    pub no_max_hand_size_rest_of_game: bool,
    pub prevent_all_combat_damage_this_turn: bool,
    pub protection_from_everything: Option<mtg_ir::Duration>,
}

impl PlayerState {
    fn new(id: PlayerId, starting_life: i32) -> Self {
        Self {
            id,
            life: starting_life,
            lost: false,
            poison: 0,
            energy: 0,
            experience: 0,
            rad: 0,
            library: Vec::with_capacity(100),
            hand: Vec::with_capacity(10),
            battlefield: Vec::with_capacity(24),
            graveyard: Vec::with_capacity(16),
            exile: Vec::with_capacity(8),
            command: Vec::with_capacity(2),
            commander_color_identity: ColorMask::empty(),
            commander_cast_counts: SmallVec::new(),
            commander_damage_taken: SmallVec::new(),
            mulligan_count: 0,
            has_kept_hand: false,
            lands_played_this_turn: 0,
            max_lands_per_turn: 1,
            mana_pool: [0; 6],
            mana_ext: None,
            used_first_activation_zero_mask: 0,
            attempted_empty_library_draw: false,
            life_gain_events_this_turn: 0,
            life_loss_events_this_turn: 0,
            draw_step_draws_this_turn: 0,
            life_gained_this_turn: 0,
            life_lost_this_turn: 0,
            cards_drawn_this_turn: 0,
            cards_discarded_this_turn: 0,
            spells_cast_this_turn: 0,
            noncreature_spells_cast_this_turn: 0,
            creatures_died_this_turn: 0,
            nontoken_creatures_died_this_turn: 0,
            permanents_left_bf_this_turn: 0,
            committed_crime_this_turn: false,
            descended_this_turn: 0,
            mana_expended_this_turn: 0,
            damage_dealt_to_this_turn: 0,
            targeted_by_this_turn: SmallVec::new(),
            skip_draw_step_once: false,
            no_max_hand_size_rest_of_game: false,
            prevent_all_combat_damage_this_turn: false,
            protection_from_everything: None,
        }
    }

    #[inline]
    pub fn mana_ext(&self) -> Option<&PlayerManaState> {
        self.mana_ext.as_deref()
    }

    #[inline]
    pub(crate) fn mana_ext_mut(&mut self) -> &mut PlayerManaState {
        Arc::make_mut(
            self.mana_ext
                .get_or_insert_with(|| Arc::new(PlayerManaState::default())),
        )
    }

    #[inline]
    pub(crate) fn prune_mana_ext_if_default(&mut self) {
        if self.mana_ext.as_ref().is_some_and(|m| m.is_default()) {
            self.mana_ext = None;
        }
    }

    #[inline]
    pub fn snow_mana_pool(&self) -> [u32; 6] {
        self.mana_ext.as_ref().map_or([0; 6], |m| m.snow_mana_pool)
    }

    #[inline]
    pub fn retained_mana_counts(&self) -> [u32; 6] {
        self.mana_ext
            .as_ref()
            .map_or([0; 6], |m| m.retained_mana_counts)
    }

    #[inline]
    pub fn restricted_mana(&self) -> &[RestrictedManaBucket] {
        self.mana_ext
            .as_ref()
            .map_or(&[], |m| m.restricted_mana.as_slice())
    }

    #[inline]
    pub fn unemptied_mana_colors_this_turn(&self) -> ColorMask {
        self.mana_ext
            .as_ref()
            .map_or(ColorMask::empty(), |m| m.unemptied_mana_colors_this_turn)
    }

    #[inline]
    pub fn once_per_turn_abilities_used(&self) -> &[(ObjectId, u16)] {
        self.mana_ext
            .as_ref()
            .map_or(&[], |m| m.once_per_turn_abilities_used.as_slice())
    }

    #[inline]
    pub fn once_per_game_abilities_used(&self) -> &[(ObjectId, u16)] {
        self.mana_ext
            .as_ref()
            .map_or(&[], |m| m.once_per_game_abilities_used.as_slice())
    }

    pub fn clear_turn_counters(&mut self) {
        self.lands_played_this_turn = 0;
        self.used_first_activation_zero_mask = 0;
        if self.mana_ext.as_ref().is_some_and(|m| {
            !m.unemptied_mana_colors_this_turn.is_empty()
                || !m.once_per_turn_abilities_used.is_empty()
        }) {
            let ext = self.mana_ext_mut();
            ext.unemptied_mana_colors_this_turn = ColorMask::empty();
            ext.once_per_turn_abilities_used.clear();
        }
        self.prune_mana_ext_if_default();
        self.life_gain_events_this_turn = 0;
        self.life_loss_events_this_turn = 0;
        self.draw_step_draws_this_turn = 0;
        self.life_gained_this_turn = 0;
        self.life_lost_this_turn = 0;
        self.cards_drawn_this_turn = 0;
        self.cards_discarded_this_turn = 0;
        self.spells_cast_this_turn = 0;
        self.noncreature_spells_cast_this_turn = 0;
        self.creatures_died_this_turn = 0;
        self.nontoken_creatures_died_this_turn = 0;
        self.permanents_left_bf_this_turn = 0;
        self.committed_crime_this_turn = false;
        self.descended_this_turn = 0;
        self.mana_expended_this_turn = 0;
        self.damage_dealt_to_this_turn = 0;
        self.targeted_by_this_turn.clear();
        self.prevent_all_combat_damage_this_turn = false;
    }

    #[inline]
    pub fn commander_cast_count(&self, commander_obj_index: u32) -> u16 {
        self.commander_cast_counts
            .iter()
            .find(|(idx, _)| *idx == commander_obj_index)
            .map_or(0, |(_, c)| *c)
    }

    #[inline]
    pub fn increment_commander_cast_count(&mut self, commander_obj_index: u32) {
        if let Some((_, count)) = self
            .commander_cast_counts
            .iter_mut()
            .find(|(idx, _)| *idx == commander_obj_index)
        {
            *count = count.saturating_add(1);
        } else {
            self.commander_cast_counts.push((commander_obj_index, 1));
        }
    }

    #[inline]
    pub fn commander_damage_from(&self, commander_obj_index: u32) -> i32 {
        self.commander_damage_taken
            .iter()
            .find(|(idx, _)| *idx == commander_obj_index)
            .map_or(0, |(_, dmg)| *dmg)
    }

    #[inline]
    pub fn add_commander_damage(&mut self, commander_obj_index: u32, amount: i32) {
        if amount <= 0 {
            return;
        }
        if let Some((_, dmg)) = self
            .commander_damage_taken
            .iter_mut()
            .find(|(idx, _)| *idx == commander_obj_index)
        {
            *dmg = dmg.saturating_add(amount);
        } else {
            self.commander_damage_taken
                .push((commander_obj_index, amount));
        }
    }
}

pub type CounterList = SmallVec<[(CounterId, u32); 2]>;

/// Serializable full state snapshot excluding `Arc<CardDatabase>` and `OnceLock<LayerCache>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameStateSnapshot {
    pub config: GameConfig,
    pub rng: RngState,
    pub objects: ObjectArena<GameObject>,
    #[serde(with = "vec_u32_arc_serde")]
    pub rare_states: Vec<(u32, Arc<RareObjectState>)>,
    pub object_counters: Vec<(u32, CounterList)>,
    pub players: SmallVec<[PlayerState; 4]>,
    pub stack: Vec<ObjectId>,
    pub stack_entries: Vec<StackEntry>,
    pub turn_number: u32,
    pub active_player: PlayerId,
    pub priority_player: PlayerId,
    pub phase: GamePhase,
    pub decision: DecisionState,
    pub next_timestamp: u64,
    pub event_seq: u64,
    pub layer_rev: u64,
    pub dirty_since_eval: DirtyMask,
    pub pending_replacement: Option<Box<PendingReplacementState>>,
    pub prevention_shields: Vec<DamagePreventionShield>,
    pub next_shield_id: u32,
    pub damage_cant_be_prevented_this_turn: bool,
    pub combat_damage_cant_be_prevented_this_turn: bool,
    pub event_log: Vec<crate::events::AtomicEvent>,
    pub event_action_depth: u16,
    pub defer_flush: u16,
    pub batch_lki_scopes: Vec<Vec<crate::events::BatchLkiEntry>>,
    #[serde(with = "arc_serde")]
    pub turn_history: Arc<crate::events::TurnEventHistory>,
    pub batch_triggered_keys: Vec<crate::triggers::BatchTriggerKey>,
    pub pending_triggers: Vec<crate::triggers::PendingTrigger>,
    pub delayed_triggers: Vec<crate::triggers::DelayedTriggerEntry>,
    pub pending_reflexive_triggers: Vec<crate::triggers::PendingReflexiveTrigger>,
    pub stack_trigger_meta: Vec<(StackItemId, crate::triggers::StackTriggerMeta)>,
    pub pending_trigger_order: Option<Box<crate::triggers::PendingTriggerOrder>>,
    pub triggered_once_this_turn: Vec<crate::triggers::OncePerTurnKey>,
    pub do_this_once_taken_this_turn: Vec<crate::triggers::OncePerTurnKey>,
    pub next_trigger_order: u64,
    pub next_delayed_id: u32,
    pub inline_mana_depth: u8,
    pub monarch: Option<PlayerId>,
    pub day_night: Option<bool>,
    pub casting: Option<Box<CastingState>>,
    pub pending_prompt: Option<Box<PendingPromptContext>>,
    pub pending_land_play: Option<(PlayerId, Zone)>,
    #[serde(with = "opt_arc_serde")]
    pub mana_riders: Option<Arc<ManaRiderState>>,
    pub combat: CombatState,
    pub resolution_frames: Vec<ResolutionFrame>,
    #[serde(with = "vec_id_arc_serde")]
    pub lki_records: Vec<(ObjectId, Arc<LkiSnapshot>)>,
    pub next_stack_id: u32,
    pub next_prompt_id: u32,
    pub continuous_effects: Vec<crate::layers::predicates::TimedContinuousEffect>,
    pub player_continuous_effects: Vec<crate::layers::predicates::PlayerContinuousEffect>,
    pub resolution_vms: Vec<ResolutionVmState>,
    pub mulligan_bottom_remaining: [u8; 8],
    pub passed_priority_mask: u8,
    pub last_normal_turn_player: Option<PlayerId>,
    pub additional_combat_phases: u8,
    pub additional_main_after_combat: bool,
    pub combat_phases_this_turn: u8,
    pub turn_serial: u32,
    pub initiative: Option<PlayerId>,
    pub next_continuous_effect_id: u64,
    pub pending_replacement_resume: Option<ReplacementResumeHook>,
    pub pending_sba_batch: Option<Box<PendingSbaBatchState>>,
    pub pending_sba_in_cleanup: bool,
    pub rare_turn_state: Option<Box<RareTurnState>>,
}

/// Pure, compact, deterministically cloneable MTG game state.
///
/// Fields are intentionally private so every mutation passes through a mutator that updates
/// `layer_rev`, `dirty_since_eval`, and `layer_cache`.
#[derive(Debug)]
pub struct GameState {
    db: Arc<CardDatabase>,
    config: GameConfig,
    rng: RngState,
    objects: ObjectArena<GameObject>,
    rare_states: Vec<(u32, Arc<RareObjectState>)>,
    object_counters: Vec<(u32, CounterList)>,
    players: SmallVec<[PlayerState; 4]>,
    stack: Vec<ObjectId>,
    stack_entries: Vec<StackEntry>,
    turn_number: u32,
    active_player: PlayerId,
    priority_player: PlayerId,
    phase: GamePhase,
    decision: DecisionState,
    next_timestamp: u64,
    event_seq: u64,
    layer_rev: u64,
    dirty_since_eval: DirtyMask,
    pending_replacement: Option<Box<PendingReplacementState>>,
    prevention_shields: Vec<DamagePreventionShield>,
    next_shield_id: u32,
    damage_cant_be_prevented_this_turn: bool,
    combat_damage_cant_be_prevented_this_turn: bool,
    event_log: Vec<crate::events::AtomicEvent>,
    event_action_depth: u16,
    defer_flush: u16,
    batch_lki_scopes: Vec<Vec<crate::events::BatchLkiEntry>>,
    turn_history: Arc<crate::events::TurnEventHistory>,
    batch_triggered_keys: Vec<crate::triggers::BatchTriggerKey>,
    pending_triggers: Vec<crate::triggers::PendingTrigger>,
    delayed_triggers: Vec<crate::triggers::DelayedTriggerEntry>,
    pending_reflexive_triggers: Vec<crate::triggers::PendingReflexiveTrigger>,
    stack_trigger_meta: Vec<(StackItemId, crate::triggers::StackTriggerMeta)>,
    pending_trigger_order: Option<Box<crate::triggers::PendingTriggerOrder>>,
    triggered_once_this_turn: Vec<crate::triggers::OncePerTurnKey>,
    do_this_once_taken_this_turn: Vec<crate::triggers::OncePerTurnKey>,
    next_trigger_order: u64,
    next_delayed_id: u32,
    inline_mana_depth: u8,
    monarch: Option<PlayerId>,
    day_night: Option<bool>,
    casting: Option<Box<CastingState>>,
    pending_prompt: Option<Box<PendingPromptContext>>,
    pending_land_play: Option<(PlayerId, Zone)>,
    mana_riders: Option<Arc<ManaRiderState>>,
    combat: CombatState,
    resolution_frames: Vec<ResolutionFrame>,
    lki_records: Vec<(ObjectId, Arc<LkiSnapshot>)>,
    next_stack_id: u32,
    next_prompt_id: u32,
    continuous_effects: Vec<crate::layers::predicates::TimedContinuousEffect>,
    player_continuous_effects: Vec<crate::layers::predicates::PlayerContinuousEffect>,
    resolution_vms: Vec<ResolutionVmState>,
    mulligan_bottom_remaining: [u8; 8],
    passed_priority_mask: u8,
    last_normal_turn_player: Option<PlayerId>,
    additional_combat_phases: u8,
    additional_main_after_combat: bool,
    combat_phases_this_turn: u8,
    turn_serial: u32,
    initiative: Option<PlayerId>,
    next_continuous_effect_id: u64,
    pending_replacement_resume: Option<ReplacementResumeHook>,
    pending_sba_batch: Option<Box<PendingSbaBatchState>>,
    pending_sba_in_cleanup: bool,
    rare_turn_state: Option<Box<RareTurnState>>,
    layer_cache: OnceLock<LayerCache>,
}

impl Clone for GameState {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            config: self.config.clone(),
            rng: self.rng,
            objects: self.objects.clone(),
            rare_states: self.rare_states.clone(),
            object_counters: self.object_counters.clone(),
            players: self.players.clone(),
            stack: self.stack.clone(),
            stack_entries: self.stack_entries.clone(),
            turn_number: self.turn_number,
            active_player: self.active_player,
            priority_player: self.priority_player,
            phase: self.phase,
            decision: self.decision.clone(),
            next_timestamp: self.next_timestamp,
            event_seq: self.event_seq,
            layer_rev: self.layer_rev,
            dirty_since_eval: self.dirty_since_eval,
            pending_replacement: self.pending_replacement.clone(),
            prevention_shields: self.prevention_shields.clone(),
            next_shield_id: self.next_shield_id,
            damage_cant_be_prevented_this_turn: self.damage_cant_be_prevented_this_turn,
            combat_damage_cant_be_prevented_this_turn: self
                .combat_damage_cant_be_prevented_this_turn,
            event_log: self.event_log.clone(),
            event_action_depth: self.event_action_depth,
            defer_flush: self.defer_flush,
            batch_lki_scopes: self.batch_lki_scopes.clone(),
            turn_history: Arc::clone(&self.turn_history),
            batch_triggered_keys: self.batch_triggered_keys.clone(),
            pending_triggers: self.pending_triggers.clone(),
            delayed_triggers: self.delayed_triggers.clone(),
            pending_reflexive_triggers: self.pending_reflexive_triggers.clone(),
            stack_trigger_meta: self.stack_trigger_meta.clone(),
            pending_trigger_order: self.pending_trigger_order.clone(),
            triggered_once_this_turn: self.triggered_once_this_turn.clone(),
            do_this_once_taken_this_turn: self.do_this_once_taken_this_turn.clone(),
            next_trigger_order: self.next_trigger_order,
            next_delayed_id: self.next_delayed_id,
            inline_mana_depth: self.inline_mana_depth,
            monarch: self.monarch,
            day_night: self.day_night,
            casting: self.casting.clone(),
            pending_prompt: self.pending_prompt.clone(),
            pending_land_play: self.pending_land_play,
            mana_riders: self.mana_riders.clone(),
            combat: self.combat.clone(),
            resolution_frames: self.resolution_frames.clone(),
            lki_records: self.lki_records.clone(),
            next_stack_id: self.next_stack_id,
            next_prompt_id: self.next_prompt_id,
            continuous_effects: self.continuous_effects.clone(),
            player_continuous_effects: self.player_continuous_effects.clone(),
            resolution_vms: self.resolution_vms.clone(),
            mulligan_bottom_remaining: self.mulligan_bottom_remaining,
            passed_priority_mask: self.passed_priority_mask,
            last_normal_turn_player: self.last_normal_turn_player,
            additional_combat_phases: self.additional_combat_phases,
            additional_main_after_combat: self.additional_main_after_combat,
            combat_phases_this_turn: self.combat_phases_this_turn,
            turn_serial: self.turn_serial,
            initiative: self.initiative,
            next_continuous_effect_id: self.next_continuous_effect_id,
            pending_replacement_resume: self.pending_replacement_resume,
            pending_sba_batch: self.pending_sba_batch.clone(),
            pending_sba_in_cleanup: self.pending_sba_in_cleanup,
            rare_turn_state: self.rare_turn_state.clone(),
            layer_cache: OnceLock::new(),
        }
    }
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
        let mut players: SmallVec<[PlayerState; 4]> = SmallVec::with_capacity(decks.len());
        let mut next_ts: u64 = 1;
        let mut all_commander_indices: SmallVec<[u32; 8]> = SmallVec::new();

        for (seat_idx, deck) in decks.iter().enumerate() {
            let pid = PlayerId(seat_idx as u8);
            let mut pstate = PlayerState::new(pid, config.starting_life);

            let mut cmd_ci = ColorMask::empty();
            for &card_def in &deck.commanders {
                let Some(card) = db.get(card_def) else {
                    return Err(EngineError::UnknownCardDef(card_def));
                };
                cmd_ci |= crate::commander::card_color_identity(&db, card);
                let oid = objects.insert(GameObject::new(
                    card_def,
                    pid,
                    Zone::Command,
                    next_ts,
                    ObjectFlags::IS_COMMANDER,
                ));
                next_ts += 1;
                pstate.command.push(oid);
                pstate.commander_cast_counts.push((oid.index, 0));
                all_commander_indices.push(oid.index);
            }
            pstate.commander_color_identity = cmd_ci;

            for &card_def in &deck.mainboard {
                if db.get(card_def).is_none() {
                    return Err(EngineError::UnknownCardDef(card_def));
                }
                let oid = objects.insert(GameObject::new(
                    card_def,
                    pid,
                    Zone::Library,
                    next_ts,
                    ObjectFlags::empty(),
                ));
                next_ts += 1;
                pstate.library.push(oid);
            }

            rng.shuffle(&mut pstate.library);
            // TS places top of library at index 0; Rust places top of library at library.last().
            pstate.library.reverse();

            // Draw opening hand via library.pop() matching TS `splice(0, starting_hand_size)`.
            for _ in 0..config.starting_hand_size {
                if let Some(oid) = pstate.library.pop() {
                    if let Some(obj) = objects.get_mut(oid) {
                        obj.zone = Zone::Hand;
                        obj.entered_timestamp = next_ts;
                        next_ts += 1;
                    }
                    pstate.hand.push(oid);
                } else {
                    break;
                }
            }

            players.push(pstate);
        }

        // Pre-populate zeroed commander damage slots across all players for every commander.
        if !all_commander_indices.is_empty() {
            for pstate in &mut players {
                for &cmd_idx in &all_commander_indices {
                    pstate.commander_damage_taken.push((cmd_idx, 0));
                }
            }
        }

        Ok(Self {
            db,
            config,
            rng,
            objects,
            rare_states: Vec::new(),
            object_counters: Vec::new(),
            players,
            stack: Vec::new(),
            stack_entries: Vec::new(),
            turn_number: 1,
            active_player: PlayerId(0),
            priority_player: PlayerId(0),
            phase: GamePhase::Main1,
            decision: DecisionState::Mulligan,
            next_timestamp: next_ts,
            event_seq: 0,
            layer_rev: 1,
            dirty_since_eval: DirtyMask::ALL,
            pending_replacement: None,
            prevention_shields: Vec::new(),
            next_shield_id: 1,
            damage_cant_be_prevented_this_turn: false,
            combat_damage_cant_be_prevented_this_turn: false,
            event_log: Vec::new(),
            event_action_depth: 0,
            defer_flush: 0,
            batch_lki_scopes: Vec::new(),
            turn_history: Arc::new(crate::events::TurnEventHistory::default()),
            batch_triggered_keys: Vec::new(),
            pending_triggers: Vec::new(),
            delayed_triggers: Vec::new(),
            pending_reflexive_triggers: Vec::new(),
            stack_trigger_meta: Vec::new(),
            pending_trigger_order: None,
            triggered_once_this_turn: Vec::new(),
            do_this_once_taken_this_turn: Vec::new(),
            next_trigger_order: 1,
            next_delayed_id: 1,
            inline_mana_depth: 0,
            monarch: None,
            day_night: None,
            casting: None,
            pending_prompt: None,
            pending_land_play: None,
            mana_riders: None,
            combat: CombatState::default(),
            resolution_frames: Vec::new(),
            lki_records: Vec::new(),
            next_stack_id: 1,
            next_prompt_id: 1,
            continuous_effects: Vec::new(),
            player_continuous_effects: Vec::new(),
            resolution_vms: Vec::new(),
            mulligan_bottom_remaining: [0; 8],
            passed_priority_mask: 0,
            last_normal_turn_player: None,
            additional_combat_phases: 0,
            additional_main_after_combat: false,
            combat_phases_this_turn: 0,
            turn_serial: 1,
            initiative: None,
            next_continuous_effect_id: 1,
            pending_replacement_resume: None,
            pending_sba_batch: None,
            pending_sba_in_cleanup: false,
            rare_turn_state: None,
            layer_cache: OnceLock::new(),
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
    pub fn stack_entries(&self) -> &[StackEntry] {
        &self.stack_entries
    }

    #[inline]
    pub fn object(&self, id: ObjectId) -> Option<&GameObject> {
        self.objects.get(id)
    }

    #[inline]
    pub(crate) fn object_mut(&mut self, id: ObjectId) -> Option<&mut GameObject> {
        self.objects.get_mut(id)
    }

    #[inline]
    pub fn objects(&self) -> &ObjectArena<GameObject> {
        &self.objects
    }

    #[inline]
    pub fn rare(&self, id: ObjectId) -> Option<&RareObjectState> {
        let obj = self.objects.get(id)?;
        if !obj.has_rare() {
            return None;
        }
        self.rare_states
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
            .ok()
            .map(|pos| self.rare_states[pos].1.as_ref())
    }

    #[inline]
    pub(crate) fn rare_mut(&mut self, id: ObjectId) -> Option<&mut RareObjectState> {
        let obj = self.objects.get(id)?;
        if !obj.has_rare() {
            return None;
        }
        let pos = self
            .rare_states
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
            .ok()?;
        Some(Arc::make_mut(&mut self.rare_states[pos].1))
    }

    pub(crate) fn rare_or_default(&mut self, id: ObjectId) -> Option<&mut RareObjectState> {
        let obj = self.objects.get_mut(id)?;
        obj.aux_flags |= AUX_HAS_RARE;
        let pos = match self
            .rare_states
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            Ok(pos) => pos,
            Err(pos) => {
                self.rare_states
                    .insert(pos, (id.index, Arc::new(RareObjectState::default())));
                pos
            }
        };
        Some(Arc::make_mut(&mut self.rare_states[pos].1))
    }

    pub(crate) fn remove_rare_if_default(&mut self, id: ObjectId) {
        let Some(obj) = self.objects.get(id) else {
            return;
        };
        if !obj.has_rare() {
            return;
        }
        if let Ok(pos) = self
            .rare_states
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
            && self.rare_states[pos].1.is_default()
        {
            self.rare_states.remove(pos);
            if let Some(obj_mut) = self.objects.get_mut(id) {
                obj_mut.aux_flags &= !AUX_HAS_RARE;
            }
        }
    }

    #[inline]
    pub fn counters(&self, id: ObjectId) -> &[(CounterId, u32)] {
        let Some(obj) = self.objects.get(id) else {
            return &[];
        };
        if !obj.has_counters() {
            return &[];
        }
        self.object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
            .ok()
            .map_or(&[], |pos| self.object_counters[pos].1.as_slice())
    }

    #[inline]
    #[allow(dead_code)]
    pub fn counter_count(&self, id: ObjectId, counter: CounterId) -> u32 {
        self.counters(id)
            .iter()
            .find(|(cid, _)| *cid == counter)
            .map_or(0, |(_, count)| *count)
    }

    #[allow(dead_code)]
    pub(crate) fn counters_mut(
        &mut self,
        id: ObjectId,
    ) -> Option<&mut SmallVec<[(CounterId, u32); 2]>> {
        let obj = self.objects.get_mut(id)?;
        obj.aux_flags |= AUX_HAS_COUNTERS;
        let pos = match self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            Ok(pos) => pos,
            Err(pos) => {
                self.object_counters
                    .insert(pos, (id.index, SmallVec::new()));
                pos
            }
        };
        Some(&mut self.object_counters[pos].1)
    }

    #[allow(dead_code)]
    pub(crate) fn add_counter(&mut self, id: ObjectId, counter: CounterId, delta: u32) -> bool {
        let Some(obj) = self.objects.get_mut(id) else {
            return false;
        };
        if delta == 0 {
            return true;
        }
        obj.aux_flags |= AUX_HAS_COUNTERS;
        let pos = match self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            Ok(pos) => pos,
            Err(pos) => {
                self.object_counters
                    .insert(pos, (id.index, SmallVec::new()));
                pos
            }
        };
        let list = &mut self.object_counters[pos].1;
        if let Some((_, count)) = list.iter_mut().find(|(cid, _)| *cid == counter) {
            *count = count.saturating_add(delta);
        } else {
            list.push((counter, delta));
        }
        self.mark_dirty(DirtyMask::COUNTERS);
        true
    }

    #[allow(dead_code)]
    pub(crate) fn set_counter(&mut self, id: ObjectId, counter: CounterId, count: u32) -> bool {
        let Some(obj) = self.objects.get_mut(id) else {
            return false;
        };
        if count == 0 {
            if !obj.has_counters() {
                return true;
            }
            if let Ok(pos) = self
                .object_counters
                .binary_search_by_key(&id.index, |(idx, _)| *idx)
            {
                let list = &mut self.object_counters[pos].1;
                let before = list.len();
                list.retain(|(cid, _)| *cid != counter);
                let changed = list.len() != before;
                if list.is_empty() {
                    self.object_counters.remove(pos);
                    if let Some(obj_mut) = self.objects.get_mut(id) {
                        obj_mut.aux_flags &= !AUX_HAS_COUNTERS;
                    }
                }
                if changed {
                    self.mark_dirty(DirtyMask::COUNTERS);
                }
            }
            return true;
        }
        obj.aux_flags |= AUX_HAS_COUNTERS;
        let pos = match self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            Ok(pos) => pos,
            Err(pos) => {
                self.object_counters
                    .insert(pos, (id.index, SmallVec::new()));
                pos
            }
        };
        let list = &mut self.object_counters[pos].1;
        if let Some((_, cur)) = list.iter_mut().find(|(cid, _)| *cid == counter) {
            if *cur == count {
                return true;
            }
            *cur = count;
        } else {
            list.push((counter, count));
        }
        self.mark_dirty(DirtyMask::COUNTERS);
        true
    }

    #[allow(dead_code)]
    pub(crate) fn remove_counter(&mut self, id: ObjectId, counter: CounterId, delta: u32) -> u32 {
        if delta == 0 {
            return 0;
        }
        let Some(obj) = self.objects.get(id) else {
            return 0;
        };
        if !obj.has_counters() {
            return 0;
        }
        let Ok(pos) = self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        else {
            return 0;
        };
        let list = &mut self.object_counters[pos].1;
        let Some(idx) = list.iter().position(|(cid, _)| *cid == counter) else {
            return 0;
        };
        let removed = list[idx].1.min(delta);
        list[idx].1 -= removed;
        if list[idx].1 == 0 {
            list.remove(idx);
        }
        if list.is_empty() {
            self.object_counters.remove(pos);
            if let Some(obj_mut) = self.objects.get_mut(id) {
                obj_mut.aux_flags &= !AUX_HAS_COUNTERS;
            }
        }
        if removed > 0 {
            self.mark_dirty(DirtyMask::COUNTERS);
        }
        removed
    }

    pub(crate) fn clear_counters(&mut self, id: ObjectId) {
        let Some(obj) = self.objects.get_mut(id) else {
            return;
        };
        if !obj.has_counters() {
            return;
        }
        obj.aux_flags &= !AUX_HAS_COUNTERS;
        if let Ok(pos) = self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            self.object_counters.remove(pos);
            self.mark_dirty(DirtyMask::COUNTERS);
        }
    }

    pub(crate) fn insert_object(
        &mut self,
        mut obj: GameObject,
        rare: Option<Arc<RareObjectState>>,
    ) -> ObjectId {
        let rare = rare.filter(|r| !r.is_default());
        if rare.is_some() {
            obj.aux_flags |= AUX_HAS_RARE;
        } else {
            obj.aux_flags &= !AUX_HAS_RARE;
        }
        obj.aux_flags &= !AUX_HAS_COUNTERS;
        let id = self.objects.insert(obj);
        if let Ok(pos) = self
            .object_counters
            .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            self.object_counters.remove(pos);
        }
        match (
            self.rare_states
                .binary_search_by_key(&id.index, |(idx, _)| *idx),
            rare,
        ) {
            (Ok(pos), Some(r)) => self.rare_states[pos].1 = r,
            (Ok(pos), None) => {
                self.rare_states.remove(pos);
            }
            (Err(pos), Some(r)) => self.rare_states.insert(pos, (id.index, r)),
            (Err(_), None) => {}
        }
        id
    }

    pub(crate) fn remove_object(&mut self, id: ObjectId) -> Option<GameObject> {
        let obj = self.objects.remove(id)?;
        if obj.has_rare()
            && let Ok(pos) = self
                .rare_states
                .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            self.rare_states.remove(pos);
        }
        if obj.has_counters()
            && let Ok(pos) = self
                .object_counters
                .binary_search_by_key(&id.index, |(idx, _)| *idx)
        {
            self.object_counters.remove(pos);
        }
        Some(obj)
    }

    #[inline]
    pub(crate) fn bump_object_generation(&mut self, id: ObjectId) -> Option<ObjectId> {
        self.objects.bump_generation(id)
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
    pub fn has_cached_layer_eval(&self) -> bool {
        self.layer_cache.get().is_some()
    }

    #[inline]
    pub fn combat(&self) -> &CombatState {
        &self.combat
    }

    #[inline]
    pub(crate) fn combat_mut(&mut self) -> &mut CombatState {
        &mut self.combat
    }

    #[inline]
    pub fn casting(&self) -> Option<&CastingState> {
        self.casting.as_deref()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn casting_mut(&mut self) -> Option<&mut CastingState> {
        self.casting.as_deref_mut()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_casting(&mut self, casting: Option<CastingState>) {
        self.casting = casting.map(Box::new);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_casting(&mut self) -> Option<CastingState> {
        self.casting.take().map(|b| *b)
    }

    #[inline]
    pub fn pending_prompt(&self) -> Option<&PendingPromptContext> {
        self.pending_prompt.as_deref()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn pending_prompt_mut(&mut self) -> Option<&mut PendingPromptContext> {
        self.pending_prompt.as_deref_mut()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_pending_prompt(&mut self, pending: Option<PendingPromptContext>) {
        self.pending_prompt = pending.map(Box::new);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_pending_prompt(&mut self) -> Option<PendingPromptContext> {
        self.pending_prompt.take().map(|b| *b)
    }

    #[inline]
    pub fn pending_land_play(&self) -> Option<(PlayerId, Zone)> {
        self.pending_land_play
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_pending_land_play(&mut self, pending: Option<(PlayerId, Zone)>) {
        self.pending_land_play = pending;
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_pending_land_play(&mut self) -> Option<(PlayerId, Zone)> {
        self.pending_land_play.take()
    }

    #[inline]
    pub fn pending_mana_delayed_riders(&self) -> &[PendingManaDelayedRider] {
        self.mana_riders
            .as_ref()
            .map_or(&[], |r| r.pending.as_slice())
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn push_pending_mana_delayed_rider(&mut self, rider: PendingManaDelayedRider) {
        Arc::make_mut(
            self.mana_riders
                .get_or_insert_with(|| Arc::new(ManaRiderState::default())),
        )
        .pending
        .push(rider);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_pending_mana_delayed_riders(&mut self) -> Vec<PendingManaDelayedRider> {
        if self
            .mana_riders
            .as_ref()
            .is_none_or(|r| r.pending.is_empty())
        {
            return Vec::new();
        }
        let Some(arc) = self.mana_riders.as_mut() else {
            return Vec::new();
        };
        let state = Arc::make_mut(arc);
        let out = std::mem::take(&mut state.pending);
        if state.is_empty() {
            self.mana_riders = None;
        }
        out
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn clear_pending_mana_delayed_riders(&mut self) {
        if self
            .mana_riders
            .as_ref()
            .is_none_or(|r| r.pending.is_empty())
        {
            return;
        }
        if let Some(arc) = self.mana_riders.as_mut() {
            let state = Arc::make_mut(arc);
            state.pending.clear();
            if state.is_empty() {
                self.mana_riders = None;
            }
        }
    }

    #[inline]
    pub fn stack_mana_riders(&self) -> &[(StackItemId, Vec<PendingManaDelayedRider>)] {
        self.mana_riders
            .as_ref()
            .map_or(&[], |r| r.stack.as_slice())
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn push_stack_mana_riders(
        &mut self,
        id: StackItemId,
        riders: Vec<PendingManaDelayedRider>,
    ) {
        if riders.is_empty() {
            return;
        }
        let state = Arc::make_mut(
            self.mana_riders
                .get_or_insert_with(|| Arc::new(ManaRiderState::default())),
        );
        if let Some((_, existing)) = state.stack.iter_mut().find(|(sid, _)| *sid == id) {
            existing.extend(riders);
        } else {
            state.stack.push((id, riders));
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_stack_mana_riders(
        &mut self,
        id: StackItemId,
    ) -> Vec<PendingManaDelayedRider> {
        let Some(pos) = self
            .mana_riders
            .as_ref()
            .and_then(|r| r.stack.iter().position(|(sid, _)| *sid == id))
        else {
            return Vec::new();
        };
        let Some(arc) = self.mana_riders.as_mut() else {
            return Vec::new();
        };
        let state = Arc::make_mut(arc);
        let out = state.stack.remove(pos).1;
        if state.is_empty() {
            self.mana_riders = None;
        }
        out
    }

    #[inline]
    pub fn pending_replacement(&self) -> Option<&PendingReplacementState> {
        self.pending_replacement.as_deref()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_pending_replacement(&mut self, pending: Option<PendingReplacementState>) {
        self.pending_replacement = pending.map(Box::new);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn take_pending_replacement(&mut self) -> Option<PendingReplacementState> {
        self.pending_replacement.take().map(|b| *b)
    }

    #[inline]
    pub fn prevention_shields(&self) -> &[DamagePreventionShield] {
        &self.prevention_shields
    }

    #[inline]
    pub(crate) fn prevention_shields_mut(&mut self) -> &mut Vec<DamagePreventionShield> {
        &mut self.prevention_shields
    }

    #[inline]
    pub(crate) fn allocate_shield_id(&mut self) -> u32 {
        let id = self.next_shield_id;
        self.next_shield_id = self.next_shield_id.wrapping_add(1).max(1);
        id
    }

    #[inline]
    pub fn damage_cant_be_prevented_this_turn(&self) -> bool {
        self.damage_cant_be_prevented_this_turn
    }

    #[inline]
    pub fn combat_damage_cant_be_prevented_this_turn(&self) -> bool {
        self.combat_damage_cant_be_prevented_this_turn
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_damage_cant_be_prevented_this_turn(&mut self, combat_only: bool) {
        if combat_only {
            self.combat_damage_cant_be_prevented_this_turn = true;
        } else {
            self.damage_cant_be_prevented_this_turn = true;
        }
        self.mark_dirty(DirtyMask::COMBAT | DirtyMask::PLAYER_STATE);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn clear_eot_prevention_state(&mut self) {
        self.prevention_shields.clear();
        self.damage_cant_be_prevented_this_turn = false;
        self.combat_damage_cant_be_prevented_this_turn = false;
        self.mark_dirty(DirtyMask::COMBAT | DirtyMask::PLAYER_STATE);
    }

    #[inline]
    pub fn event_log(&self) -> &[crate::events::AtomicEvent] {
        &self.event_log
    }

    #[inline]
    pub(crate) fn event_log_mut(&mut self) -> &mut Vec<crate::events::AtomicEvent> {
        &mut self.event_log
    }

    #[inline]
    pub fn event_action_depth(&self) -> u16 {
        self.event_action_depth
    }

    #[inline]
    pub(crate) fn set_event_action_depth(&mut self, depth: u16) {
        self.event_action_depth = depth;
    }

    #[inline]
    pub fn defer_flush(&self) -> u16 {
        self.defer_flush
    }

    #[inline]
    pub(crate) fn set_defer_flush(&mut self, defer: u16) {
        self.defer_flush = defer;
    }

    #[inline]
    pub fn batch_lki_scopes(&self) -> &[Vec<crate::events::BatchLkiEntry>] {
        &self.batch_lki_scopes
    }

    #[inline]
    pub(crate) fn batch_lki_scopes_mut(&mut self) -> &mut Vec<Vec<crate::events::BatchLkiEntry>> {
        &mut self.batch_lki_scopes
    }

    #[inline]
    pub fn turn_history(&self) -> &crate::events::TurnEventHistory {
        &self.turn_history
    }

    #[inline]
    pub(crate) fn turn_history_mut(&mut self) -> &mut crate::events::TurnEventHistory {
        Arc::make_mut(&mut self.turn_history)
    }

    #[inline]
    pub fn batch_triggered_keys(&self) -> &[crate::triggers::BatchTriggerKey] {
        &self.batch_triggered_keys
    }

    #[inline]
    pub(crate) fn batch_triggered_keys_mut(
        &mut self,
    ) -> &mut Vec<crate::triggers::BatchTriggerKey> {
        &mut self.batch_triggered_keys
    }

    #[inline]
    pub fn pending_triggers(&self) -> &[crate::triggers::PendingTrigger] {
        &self.pending_triggers
    }

    #[inline]
    pub(crate) fn pending_triggers_mut(&mut self) -> &mut Vec<crate::triggers::PendingTrigger> {
        &mut self.pending_triggers
    }

    #[inline]
    pub fn delayed_triggers(&self) -> &[crate::triggers::DelayedTriggerEntry] {
        &self.delayed_triggers
    }

    #[inline]
    pub(crate) fn delayed_triggers_mut(
        &mut self,
    ) -> &mut Vec<crate::triggers::DelayedTriggerEntry> {
        &mut self.delayed_triggers
    }

    #[inline]
    pub fn pending_reflexive_triggers(&self) -> &[crate::triggers::PendingReflexiveTrigger] {
        &self.pending_reflexive_triggers
    }

    #[inline]
    pub(crate) fn pending_reflexive_triggers_mut(
        &mut self,
    ) -> &mut Vec<crate::triggers::PendingReflexiveTrigger> {
        &mut self.pending_reflexive_triggers
    }

    #[inline]
    pub fn stack_trigger_meta(
        &self,
        id: StackItemId,
    ) -> Option<&crate::triggers::StackTriggerMeta> {
        self.stack_trigger_meta
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, meta)| meta)
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn stack_trigger_meta_mut(
        &mut self,
    ) -> &mut Vec<(StackItemId, crate::triggers::StackTriggerMeta)> {
        &mut self.stack_trigger_meta
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn push_stack_trigger_meta(
        &mut self,
        id: StackItemId,
        meta: crate::triggers::StackTriggerMeta,
    ) {
        if let Some((_, existing)) = self
            .stack_trigger_meta
            .iter_mut()
            .find(|(sid, _)| *sid == id)
        {
            *existing = meta;
        } else {
            self.stack_trigger_meta.push((id, meta));
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn remove_stack_trigger_meta(
        &mut self,
        id: StackItemId,
    ) -> Option<crate::triggers::StackTriggerMeta> {
        self.stack_trigger_meta
            .iter()
            .position(|(sid, _)| *sid == id)
            .map(|idx| self.stack_trigger_meta.remove(idx).1)
    }

    #[inline]
    pub fn pending_trigger_order(&self) -> Option<&crate::triggers::PendingTriggerOrder> {
        self.pending_trigger_order.as_deref()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_pending_trigger_order(
        &mut self,
        order: Option<crate::triggers::PendingTriggerOrder>,
    ) {
        self.pending_trigger_order = order.map(Box::new);
    }

    #[inline]
    pub fn triggered_once_this_turn(&self) -> &[crate::triggers::OncePerTurnKey] {
        &self.triggered_once_this_turn
    }

    #[inline]
    pub(crate) fn triggered_once_this_turn_mut(
        &mut self,
    ) -> &mut Vec<crate::triggers::OncePerTurnKey> {
        &mut self.triggered_once_this_turn
    }

    #[inline]
    pub fn do_this_once_taken_this_turn(&self) -> &[crate::triggers::OncePerTurnKey] {
        &self.do_this_once_taken_this_turn
    }

    #[inline]
    pub(crate) fn do_this_once_taken_this_turn_mut(
        &mut self,
    ) -> &mut Vec<crate::triggers::OncePerTurnKey> {
        &mut self.do_this_once_taken_this_turn
    }

    #[inline]
    pub(crate) fn allocate_trigger_order(&mut self) -> u64 {
        let ord = self.next_trigger_order;
        self.next_trigger_order += 1;
        ord
    }

    #[inline]
    pub(crate) fn allocate_delayed_id(&mut self) -> u32 {
        let id = self.next_delayed_id;
        self.next_delayed_id = self.next_delayed_id.wrapping_add(1).max(1);
        id
    }

    #[inline]
    pub fn inline_mana_depth(&self) -> u8 {
        self.inline_mana_depth
    }

    #[inline]
    pub(crate) fn set_inline_mana_depth(&mut self, depth: u8) {
        self.inline_mana_depth = depth;
    }

    #[inline]
    pub fn monarch(&self) -> Option<PlayerId> {
        self.monarch
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_monarch(&mut self, monarch: Option<PlayerId>) {
        self.monarch = monarch;
        self.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::PLAYER_STATE);
    }

    #[inline]
    pub fn day_night(&self) -> Option<bool> {
        self.day_night
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_day_night(&mut self, day_night: Option<bool>) {
        self.day_night = day_night;
        self.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::PLAYER_STATE);
    }

    #[inline]
    pub fn initiative(&self) -> Option<PlayerId> {
        self.initiative
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_initiative(&mut self, initiative: Option<PlayerId>) {
        self.initiative = initiative;
        self.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::PLAYER_STATE);
    }

    #[inline]
    pub fn resolution_frames(&self) -> &[ResolutionFrame] {
        &self.resolution_frames
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn resolution_frames_mut(&mut self) -> &mut Vec<ResolutionFrame> {
        &mut self.resolution_frames
    }

    #[inline]
    pub fn resolution_vms(&self) -> &[ResolutionVmState] {
        &self.resolution_vms
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn resolution_vms_mut(&mut self) -> &mut Vec<ResolutionVmState> {
        &mut self.resolution_vms
    }

    #[inline]
    pub fn mulligan_bottom_remaining(&self, player: PlayerId) -> u8 {
        self.mulligan_bottom_remaining
            .get(usize::from(player.0))
            .copied()
            .unwrap_or(0)
    }

    #[inline]
    pub(crate) fn set_mulligan_bottom_remaining(&mut self, player: PlayerId, val: u8) {
        if let Some(slot) = self
            .mulligan_bottom_remaining
            .get_mut(usize::from(player.0))
        {
            *slot = val;
        }
    }

    #[inline]
    pub fn passed_priority_mask(&self) -> u8 {
        self.passed_priority_mask
    }

    #[inline]
    pub fn has_passed_priority(&self, player: PlayerId) -> bool {
        let bit = 1u8.checked_shl(u32::from(player.0)).unwrap_or(0);
        (self.passed_priority_mask & bit) != 0
    }

    #[inline]
    pub(crate) fn mark_passed_priority(&mut self, player: PlayerId) {
        if let Some(bit) = 1u8.checked_shl(u32::from(player.0)) {
            self.passed_priority_mask |= bit;
        }
    }

    #[inline]
    pub(crate) fn clear_passed_priority(&mut self) {
        self.passed_priority_mask = 0;
    }

    #[inline]
    pub fn last_normal_turn_player(&self) -> Option<PlayerId> {
        self.last_normal_turn_player
    }

    #[inline]
    pub(crate) fn set_last_normal_turn_player(&mut self, player: Option<PlayerId>) {
        self.last_normal_turn_player = player;
    }

    #[inline]
    pub fn additional_combat_phases(&self) -> u8 {
        self.additional_combat_phases
    }

    #[inline]
    pub(crate) fn set_additional_combat_phases(&mut self, val: u8) {
        self.additional_combat_phases = val;
    }

    #[inline]
    pub fn additional_main_after_combat(&self) -> bool {
        self.additional_main_after_combat
    }

    #[inline]
    pub(crate) fn set_additional_main_after_combat(&mut self, val: bool) {
        self.additional_main_after_combat = val;
    }

    #[inline]
    pub fn combat_phases_this_turn(&self) -> u8 {
        self.combat_phases_this_turn
    }

    #[inline]
    pub(crate) fn set_combat_phases_this_turn(&mut self, val: u8) {
        self.combat_phases_this_turn = val;
    }

    #[inline]
    pub fn turn_serial(&self) -> u32 {
        self.turn_serial
    }

    #[inline]
    pub(crate) fn set_turn_serial(&mut self, val: u32) {
        self.turn_serial = val;
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn allocate_continuous_effect_id(&mut self) -> u64 {
        let id = self.next_continuous_effect_id;
        self.next_continuous_effect_id = self.next_continuous_effect_id.wrapping_add(1).max(1);
        id
    }

    #[inline]
    pub fn pending_replacement_resume(&self) -> Option<ReplacementResumeHook> {
        self.pending_replacement_resume
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_pending_replacement_resume(&mut self, hook: Option<ReplacementResumeHook>) {
        self.pending_replacement_resume = hook;
    }

    #[inline]
    pub(crate) fn take_pending_replacement_resume(&mut self) -> Option<ReplacementResumeHook> {
        self.pending_replacement_resume.take()
    }

    #[inline]
    pub fn pending_sba_batch(&self) -> Option<&PendingSbaBatchState> {
        self.pending_sba_batch.as_deref()
    }

    #[inline]
    pub(crate) fn set_pending_sba_batch(&mut self, batch: Option<PendingSbaBatchState>) {
        self.pending_sba_batch = batch.map(Box::new);
    }

    #[inline]
    pub(crate) fn take_pending_sba_batch(&mut self) -> Option<PendingSbaBatchState> {
        self.pending_sba_batch.take().map(|b| *b)
    }

    #[inline]
    pub fn pending_sba_in_cleanup(&self) -> bool {
        self.pending_sba_in_cleanup
    }

    #[inline]
    pub(crate) fn set_pending_sba_in_cleanup(&mut self, val: bool) {
        self.pending_sba_in_cleanup = val;
    }

    #[inline]
    pub fn rare_turn_state(&self) -> Option<&RareTurnState> {
        self.rare_turn_state.as_deref()
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn rare_turn_state_mut(&mut self) -> &mut RareTurnState {
        self.rare_turn_state
            .get_or_insert_with(|| Box::new(RareTurnState::default()))
    }

    #[inline]
    pub(crate) fn prune_rare_turn_state_if_empty(&mut self) {
        if self.rare_turn_state.as_ref().is_some_and(|r| r.is_empty()) {
            self.rare_turn_state = None;
        }
    }

    #[inline]
    pub(crate) fn mark_dirty(&mut self, mask: DirtyMask) {
        self.layer_rev = self.layer_rev.wrapping_add(1);
        self.dirty_since_eval |= mask;
        if let Some(cached) = self.layer_cache.get_mut() {
            if (mask & cached.reads_mask).is_empty() {
                cached.rev = self.layer_rev;
                return;
            }
            self.layer_cache = OnceLock::new();
        }
    }

    /// Evaluates continuous effects and returns the memoized [`LayerCache`] (`&self`, `Send + Sync`).
    pub fn evaluate_board(&self) -> &LayerCache {
        let cached = self
            .layer_cache
            .get_or_init(|| crate::layers::cache::compute_layer_cache(self));
        #[cfg(feature = "verify-layer-cache")]
        crate::layers::cache::verify_layer_cache(self, cached);
        cached
    }

    #[inline]
    pub fn continuous_effects(&self) -> &[crate::layers::predicates::TimedContinuousEffect] {
        &self.continuous_effects
    }

    #[inline]
    pub fn player_continuous_effects(
        &self,
    ) -> &[crate::layers::predicates::PlayerContinuousEffect] {
        &self.player_continuous_effects
    }

    #[allow(dead_code)]
    pub(crate) fn add_continuous_effect(
        &mut self,
        effect: crate::layers::predicates::TimedContinuousEffect,
    ) {
        self.continuous_effects.push(effect);
        self.mark_dirty(DirtyMask::CONTINUOUS_EFFECTS);
    }

    #[allow(dead_code)]
    pub(crate) fn add_player_continuous_effect(
        &mut self,
        effect: crate::layers::predicates::PlayerContinuousEffect,
    ) {
        self.player_continuous_effects.push(effect);
        self.mark_dirty(DirtyMask::CONTINUOUS_EFFECTS | DirtyMask::PLAYER_STATE);
    }

    #[allow(dead_code)]
    pub(crate) fn retain_continuous_effects(
        &mut self,
        mut f: impl FnMut(&crate::layers::predicates::TimedContinuousEffect) -> bool,
    ) {
        let before = self.continuous_effects.len();
        self.continuous_effects.retain(|e| f(e));
        if self.continuous_effects.len() != before {
            self.mark_dirty(DirtyMask::CONTINUOUS_EFFECTS);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn retain_player_continuous_effects(
        &mut self,
        mut f: impl FnMut(&crate::layers::predicates::PlayerContinuousEffect) -> bool,
    ) {
        let before = self.player_continuous_effects.len();
        self.player_continuous_effects.retain(|e| f(e));
        if self.player_continuous_effects.len() != before {
            self.mark_dirty(DirtyMask::CONTINUOUS_EFFECTS | DirtyMask::PLAYER_STATE);
        }
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
    pub(crate) fn allocate_timestamp(&mut self) -> u64 {
        let ts = self.next_timestamp;
        self.next_timestamp += 1;
        ts
    }

    #[inline]
    pub(crate) fn allocate_stack_id(&mut self) -> StackItemId {
        let id = StackItemId(self.next_stack_id);
        self.next_stack_id += 1;
        id
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn allocate_prompt_id(&mut self) -> u32 {
        let id = self.next_prompt_id;
        self.next_prompt_id += 1;
        id
    }

    pub(crate) fn record_lki(&mut self, id: ObjectId, snapshot: Arc<LkiSnapshot>) {
        if let Some((_, existing)) = self
            .lki_records
            .iter_mut()
            .rev()
            .find(|(oid, _)| *oid == id)
        {
            *existing = snapshot;
        } else {
            if self.lki_records.len() >= MAX_LKI_RECORDS {
                let excess = self.lki_records.len() + 1 - MAX_LKI_RECORDS;
                self.lki_records.drain(..excess);
            }
            self.lki_records.push((id, snapshot));
        }
    }

    #[inline]
    pub fn lki(&self, id: ObjectId) -> Option<&LkiSnapshot> {
        self.lki_records
            .iter()
            .rev()
            .find(|(oid, _)| *oid == id)
            .map(|(_, snap)| snap.as_ref())
    }

    #[allow(dead_code)]
    pub(crate) fn prune_stale_lki(&mut self) {
        if self.stack.is_empty()
            && self.resolution_frames.is_empty()
            && self.resolution_vms.is_empty()
            && self.pending_triggers.is_empty()
            && self.pending_reflexive_triggers.is_empty()
            && self.pending_prompt.is_none()
            && self.pending_replacement.is_none()
            && self.pending_replacement_resume.is_none()
            && self.pending_sba_batch.is_none()
            && self.pending_trigger_order.is_none()
            && self.casting.is_none()
            && self.batch_lki_scopes.is_empty()
            && self.delayed_triggers.is_empty()
        {
            self.lki_records.clear();
            return;
        }
        if !self.lki_records.is_empty() {
            self.retain_reachable_lki();
        }
    }

    fn retain_reachable_lki(&mut self) {
        let mut keep: SmallVec<[ObjectId; 16]> = SmallVec::new();
        let push_oid = |keep: &mut SmallVec<[ObjectId; 16]>, id: ObjectId| {
            if id != NONE_OBJECT_ID && !keep.contains(&id) {
                keep.push(id);
            }
        };
        let push_tref = |keep: &mut SmallVec<[ObjectId; 16]>, tref: &TargetRef| {
            if let TargetRef::Object { id } = *tref {
                push_oid(keep, id);
            }
        };
        let push_lki = |keep: &mut SmallVec<[ObjectId; 16]>, lki: &LkiSnapshot| {
            push_oid(keep, lki.object_id);
            for &aid in &lki.attached_ids {
                push_oid(keep, aid);
            }
        };
        let push_bindings = |keep: &mut SmallVec<[ObjectId; 16]>,
                             bindings: &[ResolutionBinding]| {
            for b in bindings {
                for &id in &b.objects {
                    push_oid(keep, id);
                }
                for lki in &b.lki {
                    push_lki(keep, lki);
                }
            }
        };

        for &id in &self.stack {
            push_oid(&mut keep, id);
        }
        for entry in &self.stack_entries {
            match &entry.kind {
                StackEntryKind::Spell { object } => push_oid(&mut keep, *object),
                StackEntryKind::Activated { source, lki, .. }
                | StackEntryKind::Triggered { source, lki, .. } => {
                    push_oid(&mut keep, *source);
                    push_lki(&mut keep, lki);
                }
            }
            for t in &entry.targets {
                push_tref(&mut keep, t);
            }
            for spec_targets in &entry.choices.targets_by_spec {
                for t in spec_targets {
                    push_tref(&mut keep, t);
                }
            }
            for (t, _) in &entry.choices.divisions {
                push_tref(&mut keep, t);
            }
            if let Some(mt) = entry.choices.mutate_target {
                push_oid(&mut keep, mt);
            }
        }

        for (_, meta) in &self.stack_trigger_meta {
            push_oid(&mut keep, meta.source);
            if let Some(eo) = meta.event_object {
                push_oid(&mut keep, eo);
            }
            if let Some(ref lki) = meta.event_lki {
                push_lki(&mut keep, lki);
            }
            push_bindings(&mut keep, &meta.bindings);
        }

        for frame in &self.resolution_frames {
            push_bindings(&mut keep, &frame.bindings);
        }

        for vm in &self.resolution_vms {
            if let Some(s) = vm.source {
                push_oid(&mut keep, s);
            }
            if let Some(ref lki) = vm.source_lki {
                push_lki(&mut keep, lki);
            }
            if let Some(eo) = vm.event_object {
                push_oid(&mut keep, eo);
            }
            if let Some(ref lki) = vm.event_lki {
                push_lki(&mut keep, lki);
            }
            for t in &vm.targets {
                push_tref(&mut keep, t);
            }
            for spec_targets in &vm.targets_by_spec {
                for t in spec_targets {
                    push_tref(&mut keep, t);
                }
            }
            for (t, _) in &vm.divisions {
                push_tref(&mut keep, t);
            }
            for &id in &vm.step_acted_ids {
                push_oid(&mut keep, id);
            }
            for sf in &vm.subframes {
                if let ControlFlowFrame::SubSteps {
                    event_object: Some(eo),
                    ..
                } = sf
                {
                    push_oid(&mut keep, *eo);
                }
            }
            if let Some(ref ps) = vm.pause_state {
                match ps {
                    StepPauseState::SelectionPick {
                        candidates,
                        selected,
                        ..
                    } => {
                        for &id in candidates.iter().chain(selected.iter()) {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::SearchPick {
                        candidates,
                        selected,
                        moved_ids,
                        ..
                    } => {
                        for &id in candidates
                            .iter()
                            .chain(selected.iter())
                            .chain(moved_ids.iter())
                        {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::ReorderLibrary {
                        remaining,
                        top_order,
                        bottom_or_gy,
                        deciding_card,
                        ..
                    } => {
                        for &id in remaining
                            .iter()
                            .chain(top_order.iter())
                            .chain(bottom_or_gy.iter())
                        {
                            push_oid(&mut keep, id);
                        }
                        if let Some(dc) = deciding_card {
                            push_oid(&mut keep, *dc);
                        }
                    }
                    StepPauseState::DigPick {
                        revealed,
                        candidates,
                        selected,
                        partition_picks,
                        ..
                    } => {
                        for &id in revealed
                            .iter()
                            .chain(candidates.iter())
                            .chain(selected.iter())
                            .chain(partition_picks.iter())
                        {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::MoveCountersSelectTarget {
                        from_id,
                        candidates,
                        transfers,
                        ..
                    } => {
                        push_oid(&mut keep, *from_id);
                        for &id in candidates {
                            push_oid(&mut keep, id);
                        }
                        for &(id, _) in transfers {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::MoveCountersChooseAmount {
                        from_id,
                        candidates,
                        target_id,
                        transfers,
                        ..
                    } => {
                        push_oid(&mut keep, *from_id);
                        push_oid(&mut keep, *target_id);
                        for &id in candidates {
                            push_oid(&mut keep, id);
                        }
                        for &(id, _) in transfers {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::GuidedResolution { source, .. } => {
                        push_oid(&mut keep, *source);
                    }
                    StepPauseState::WaitingOnReplacement {
                        batch_remaining, ..
                    } => {
                        for &id in batch_remaining {
                            push_oid(&mut keep, id);
                        }
                    }
                    StepPauseState::OptionalChoice { .. }
                    | StepPauseState::ChooseNumber { .. }
                    | StepPauseState::ChooseMode { .. } => {}
                }
            }
            if let Some(ref cont) = vm.pending_continuation {
                for (src, tgt, _) in &cont.remaining_damage {
                    if let Some(s) = src {
                        push_oid(&mut keep, *s);
                    }
                    push_tref(&mut keep, tgt);
                }
                if let Some((a, b, _)) = cont.fight_return_damage {
                    push_oid(&mut keep, a);
                    push_oid(&mut keep, b);
                }
                for &id in cont
                    .search_moved_ids
                    .iter()
                    .chain(cont.search_candidates.iter())
                    .chain(cont.proliferate_selected_perms.iter())
                {
                    push_oid(&mut keep, id);
                }
                for &(id, _) in &cont.proliferate_counter_queue {
                    push_oid(&mut keep, id);
                }
            }
        }

        for trig in &self.pending_triggers {
            push_oid(&mut keep, trig.source);
            push_lki(&mut keep, &trig.source_lki);
            if let Some(eo) = trig.event_object {
                push_oid(&mut keep, eo);
            }
            if let Some(ref lki) = trig.event_lki {
                push_lki(&mut keep, lki);
            }
            push_bindings(&mut keep, &trig.bindings);
        }

        for rtrig in &self.pending_reflexive_triggers {
            push_oid(&mut keep, rtrig.source);
            push_lki(&mut keep, &rtrig.source_lki);
            if let Some(eo) = rtrig.event_object {
                push_oid(&mut keep, eo);
            }
            if let Some(ref lki) = rtrig.event_lki {
                push_lki(&mut keep, lki);
            }
            push_bindings(&mut keep, &rtrig.bindings);
        }

        if let Some(ref order) = self.pending_trigger_order {
            for &id in order.object_ids.iter().chain(order.chosen_top_first.iter()) {
                push_oid(&mut keep, id);
            }
        }

        if let Some(ref prompt) = self.pending_prompt {
            match &prompt.request {
                ChoiceRequest::ChooseObjects { candidates, .. } => {
                    for &id in candidates {
                        push_oid(&mut keep, id);
                    }
                }
                ChoiceRequest::OrderObjects { objects, .. } => {
                    for &id in objects {
                        push_oid(&mut keep, id);
                    }
                }
                ChoiceRequest::GuidedResolution { source, .. } => {
                    push_oid(&mut keep, *source);
                }
                ChoiceRequest::YesNo { .. }
                | ChoiceRequest::ChooseMode { .. }
                | ChoiceRequest::ChooseNumber { .. }
                | ChoiceRequest::ChooseReplacement { .. } => {}
            }
            match &prompt.owner {
                PromptOwner::Casting(ck) => match ck {
                    CastingPromptKind::CostObjectSelection {
                        candidates, picked, ..
                    } => {
                        for &id in candidates.iter().chain(picked.iter()) {
                            push_oid(&mut keep, id);
                        }
                    }
                    CastingPromptKind::GuidedFallback { source, .. } => {
                        push_oid(&mut keep, *source);
                    }
                    _ => {}
                },
                PromptOwner::ManaAbility(ma) => {
                    push_oid(&mut keep, ma.source);
                }
                PromptOwner::CostPayment(cp) => {
                    push_oid(&mut keep, cp.source);
                    if let CostPromptKind::ObjectSelection {
                        candidates, picked, ..
                    } = &cp.kind
                    {
                        for &id in candidates.iter().chain(picked.iter()) {
                            push_oid(&mut keep, id);
                        }
                    }
                }
                PromptOwner::Sba(sba) => match sba {
                    SbaPromptKind::LegendRule { candidates, .. } => {
                        for &id in candidates {
                            push_oid(&mut keep, id);
                        }
                    }
                    SbaPromptKind::CommanderReturn { commander, .. } => {
                        push_oid(&mut keep, *commander);
                    }
                },
                PromptOwner::Turn(tp) => match tp {
                    TurnPromptKind::CleanupDiscard { picked, .. } => {
                        for &id in picked {
                            push_oid(&mut keep, id);
                        }
                    }
                    TurnPromptKind::UntapOptional { candidates, .. } => {
                        for &id in candidates {
                            push_oid(&mut keep, id);
                        }
                    }
                    TurnPromptKind::EndTurnMay => {}
                },
                PromptOwner::Combat(cb) => match cb {
                    CombatPromptKind::DamageAssignmentOrder {
                        attacker,
                        unordered_blockers,
                        ordered_blockers,
                    } => {
                        push_oid(&mut keep, *attacker);
                        for &id in unordered_blockers.iter().chain(ordered_blockers.iter()) {
                            push_oid(&mut keep, id);
                        }
                    }
                    CombatPromptKind::AttackerDamageDivide { attacker, .. }
                    | CombatPromptKind::SuperTrampleAsUnblocked { attacker, .. } => {
                        push_oid(&mut keep, *attacker);
                    }
                    CombatPromptKind::BlockerDamageDivide { blocker, .. } => {
                        push_oid(&mut keep, *blocker);
                    }
                    CombatPromptKind::AttackTaxPay | CombatPromptKind::BlockTaxPay => {}
                },
                PromptOwner::Replacement
                | PromptOwner::TriggerOrder
                | PromptOwner::ResolutionFrame => {}
            }
        }

        if let Some(ref rep) = self.pending_replacement {
            match &rep.event {
                ReplaceableEventKind::ZoneChange { object, .. }
                | ReplaceableEventKind::Untap { object } => {
                    push_oid(&mut keep, *object);
                }
                ReplaceableEventKind::Draw { source, .. }
                | ReplaceableEventKind::LifeGain { source, .. }
                | ReplaceableEventKind::CreateToken { source, .. }
                | ReplaceableEventKind::Proliferate { source, .. }
                | ReplaceableEventKind::PutPlayerCounters { source, .. } => {
                    if let Some(s) = source {
                        push_oid(&mut keep, *s);
                    }
                }
                ReplaceableEventKind::Damage { source, target, .. } => {
                    if let Some(s) = source {
                        push_oid(&mut keep, *s);
                    }
                    push_tref(&mut keep, target);
                }
                ReplaceableEventKind::PutCounters { object, source, .. } => {
                    push_oid(&mut keep, *object);
                    if let Some(s) = source {
                        push_oid(&mut keep, *s);
                    }
                }
            }
            for src_id in rep.applied.iter().chain(rep.candidates.iter()) {
                push_oid(&mut keep, src_id.object);
                if let Some(gs) = src_id.grant_source {
                    push_oid(&mut keep, gs);
                }
            }
            match &rep.prompt_kind {
                PendingReplacementPrompt::EtbCopy {
                    source_id,
                    candidates,
                    ..
                } => {
                    push_oid(&mut keep, source_id.object);
                    if let Some(gs) = source_id.grant_source {
                        push_oid(&mut keep, gs);
                    }
                    for &id in candidates {
                        push_oid(&mut keep, id);
                    }
                }
                PendingReplacementPrompt::EtbChoice {
                    source_id, step, ..
                } => {
                    push_oid(&mut keep, source_id.object);
                    if let Some(gs) = source_id.grant_source {
                        push_oid(&mut keep, gs);
                    }
                    match step {
                        EtbInteractiveStep::RevealFromHandOrEnterTapped { candidates }
                        | EtbInteractiveStep::ChooseObjectOnBattlefield { candidates, .. } => {
                            for &id in candidates {
                                push_oid(&mut keep, id);
                            }
                        }
                        EtbInteractiveStep::DiscardForKeywordCounters {
                            candidates,
                            picked,
                            ..
                        }
                        | EtbInteractiveStep::RemoveCountersForPlusCounters {
                            candidates,
                            picked,
                            ..
                        } => {
                            for &id in candidates.iter().chain(picked.iter()) {
                                push_oid(&mut keep, id);
                            }
                        }
                        _ => {}
                    }
                }
                PendingReplacementPrompt::AbundanceChoice { source_id } => {
                    push_oid(&mut keep, source_id.object);
                    if let Some(gs) = source_id.grant_source {
                        push_oid(&mut keep, gs);
                    }
                }
                PendingReplacementPrompt::ChooseReplacement
                | PendingReplacementPrompt::CommanderRedirect { .. } => {}
            }
            if let ReplacementContinuation::ZoneMoveEtbCounters(ref mv) = rep.continuation {
                push_oid(&mut keep, mv.raw.old_id);
                if let Some(nid) = mv.raw.new_id {
                    push_oid(&mut keep, nid);
                }
                if let Some(ref lki) = mv.raw.lki {
                    push_lki(&mut keep, lki);
                }
            }
        }

        match self.pending_replacement_resume {
            Some(ReplacementResumeHook::StackResolutionFinish {
                attach_target: Some(at),
                ..
            }) => {
                push_oid(&mut keep, at);
            }
            Some(ReplacementResumeHook::MegamorphTurnFaceUp { object, .. }) => {
                push_oid(&mut keep, object);
            }
            _ => {}
        }

        if let Some(ref batch) = self.pending_sba_batch {
            for &id in &batch.remaining_graveyard_ids {
                push_oid(&mut keep, id);
            }
        }

        if let Some(ref cs) = self.casting {
            push_oid(&mut keep, cs.object);
            for t in &cs.targets {
                push_tref(&mut keep, t);
            }
            for spec_targets in &cs.targets_by_spec {
                for t in spec_targets {
                    push_tref(&mut keep, t);
                }
            }
            for (t, _) in &cs.divisions {
                push_tref(&mut keep, t);
            }
            for &id in &cs.paid_mana_sources {
                push_oid(&mut keep, id);
            }
            if let Some(ref pce) = cs.pre_cast_exile
                && let Some(eb) = pce.exiled_by
            {
                push_oid(&mut keep, eb);
            }
            if let Some(ref co) = cs.cast_option
                && let Some(ps) = co.permission_source
            {
                push_oid(&mut keep, ps);
            }
            if let Some(ref plan) = cs.payment_plan {
                for &id in plan
                    .convoke_tapped
                    .iter()
                    .chain(plan.improvise_tapped.iter())
                    .chain(plan.delve_exiled.iter())
                    .chain(plan.waterbend_tapped.iter())
                    .chain(plan.revealed_cards.iter())
                    .chain(plan.discarded_cards.iter())
                {
                    push_oid(&mut keep, id);
                }
                if let Some(es) = plan.emerge_sacrificed {
                    push_oid(&mut keep, es);
                }
                for lki in &plan.sacrificed_lki {
                    push_lki(&mut keep, lki);
                }
                if let Some(ref pstep) = plan.pending_step {
                    match pstep {
                        PendingNonManaCostStep::ZoneMove {
                            kind,
                            current_object,
                            remaining_objects,
                        } => {
                            push_oid(&mut keep, *current_object);
                            for &id in remaining_objects {
                                push_oid(&mut keep, id);
                            }
                            if let PendingZoneCostKind::ExileFrom { source, .. } = kind {
                                push_oid(&mut keep, *source);
                            }
                        }
                        PendingNonManaCostStep::EffectReplace {
                            kind:
                                PendingEffectReplaceKind::Untap {
                                    current_object,
                                    remaining_objects,
                                },
                        } => {
                            push_oid(&mut keep, *current_object);
                            for &id in remaining_objects {
                                push_oid(&mut keep, id);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        for scope in &self.batch_lki_scopes {
            for entry in scope {
                push_oid(&mut keep, entry.id);
                push_lki(&mut keep, &entry.lki);
            }
        }

        for dt in &self.delayed_triggers {
            if let Some(so) = dt.source_object {
                push_oid(&mut keep, so);
            }
            if let Some(ref lki) = dt.source_lki {
                push_lki(&mut keep, lki);
            }
            for &wo in &dt.watched_objects {
                push_oid(&mut keep, wo);
            }
            push_bindings(&mut keep, &dt.bindings);
        }

        loop {
            let mut added = false;
            for (oid, snap) in &self.lki_records {
                if keep.contains(oid) {
                    for &aid in &snap.attached_ids {
                        if aid != NONE_OBJECT_ID && !keep.contains(&aid) {
                            keep.push(aid);
                            added = true;
                        }
                    }
                }
            }
            if !added {
                break;
            }
        }

        self.lki_records.retain(|(oid, _)| keep.contains(oid));
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn lki_records_mut(&mut self) -> &mut Vec<(ObjectId, Arc<LkiSnapshot>)> {
        &mut self.lki_records
    }

    #[inline]
    pub(crate) fn set_decision(&mut self, decision: DecisionState) {
        self.decision = decision;
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_turn_number(&mut self, turn: u32) {
        self.turn_number = turn;
        self.mark_dirty(DirtyMask::TURN_HISTORY);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_active_player(&mut self, player: PlayerId) {
        self.active_player = player;
        self.mark_dirty(DirtyMask::TURN_HISTORY);
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_priority_player(&mut self, player: PlayerId) {
        self.priority_player = player;
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_phase(&mut self, phase: GamePhase) {
        self.phase = phase;
        self.mark_dirty(DirtyMask::TURN_HISTORY | DirtyMask::COMBAT);
    }

    #[inline]
    pub fn player(&self, id: PlayerId) -> Option<&PlayerState> {
        self.players.get(id.0 as usize)
    }

    #[inline]
    pub(crate) fn player_mut(&mut self, id: PlayerId) -> Option<&mut PlayerState> {
        self.players.get_mut(id.0 as usize)
    }

    #[inline]
    pub(crate) fn stack_mut(&mut self) -> &mut Vec<ObjectId> {
        &mut self.stack
    }

    #[inline]
    pub(crate) fn stack_entries_mut(&mut self) -> &mut Vec<StackEntry> {
        &mut self.stack_entries
    }

    #[inline]
    pub(crate) fn rng_mut(&mut self) -> &mut RngState {
        &mut self.rng
    }

    #[inline]
    pub fn commander_cast_count(&self, owner: PlayerId, commander_id: ObjectId) -> u16 {
        self.players
            .get(owner.0 as usize)
            .map_or(0, |p| p.commander_cast_count(commander_id.index))
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn increment_commander_cast_count(
        &mut self,
        owner: PlayerId,
        commander_id: ObjectId,
    ) {
        if let Some(p) = self.players.get_mut(owner.0 as usize) {
            p.increment_commander_cast_count(commander_id.index);
            self.mark_dirty(DirtyMask::PLAYER_STATE);
        }
    }

    #[inline]
    pub fn commander_damage_taken(&self, victim: PlayerId, commander_id: ObjectId) -> i32 {
        self.players
            .get(victim.0 as usize)
            .map_or(0, |p| p.commander_damage_from(commander_id.index))
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn add_commander_damage(
        &mut self,
        victim: PlayerId,
        commander_id: ObjectId,
        amount: i32,
    ) {
        if let Some(p) = self.players.get_mut(victim.0 as usize) {
            p.add_commander_damage(commander_id.index, amount);
            self.mark_dirty(DirtyMask::PLAYER_STATE);
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn set_player_commander_color_identity(
        &mut self,
        player: PlayerId,
        color_identity: ColorMask,
    ) {
        if let Some(p) = self.players.get_mut(player.0 as usize) {
            p.commander_color_identity = color_identity;
            self.mark_dirty(DirtyMask::PLAYER_STATE);
        }
    }

    pub fn to_snapshot(&self) -> GameStateSnapshot {
        GameStateSnapshot {
            config: self.config.clone(),
            rng: self.rng,
            objects: self.objects.clone(),
            rare_states: self.rare_states.clone(),
            object_counters: self.object_counters.clone(),
            players: self.players.clone(),
            stack: self.stack.clone(),
            stack_entries: self.stack_entries.clone(),
            turn_number: self.turn_number,
            active_player: self.active_player,
            priority_player: self.priority_player,
            phase: self.phase,
            decision: self.decision.clone(),
            next_timestamp: self.next_timestamp,
            event_seq: self.event_seq,
            layer_rev: self.layer_rev,
            dirty_since_eval: self.dirty_since_eval,
            pending_replacement: self.pending_replacement.clone(),
            prevention_shields: self.prevention_shields.clone(),
            next_shield_id: self.next_shield_id,
            damage_cant_be_prevented_this_turn: self.damage_cant_be_prevented_this_turn,
            combat_damage_cant_be_prevented_this_turn: self
                .combat_damage_cant_be_prevented_this_turn,
            event_log: self.event_log.clone(),
            event_action_depth: self.event_action_depth,
            defer_flush: self.defer_flush,
            batch_lki_scopes: self.batch_lki_scopes.clone(),
            turn_history: Arc::clone(&self.turn_history),
            batch_triggered_keys: self.batch_triggered_keys.clone(),
            pending_triggers: self.pending_triggers.clone(),
            delayed_triggers: self.delayed_triggers.clone(),
            pending_reflexive_triggers: self.pending_reflexive_triggers.clone(),
            stack_trigger_meta: self.stack_trigger_meta.clone(),
            pending_trigger_order: self.pending_trigger_order.clone(),
            triggered_once_this_turn: self.triggered_once_this_turn.clone(),
            do_this_once_taken_this_turn: self.do_this_once_taken_this_turn.clone(),
            next_trigger_order: self.next_trigger_order,
            next_delayed_id: self.next_delayed_id,
            inline_mana_depth: self.inline_mana_depth,
            monarch: self.monarch,
            day_night: self.day_night,
            casting: self.casting.clone(),
            pending_prompt: self.pending_prompt.clone(),
            pending_land_play: self.pending_land_play,
            mana_riders: self.mana_riders.clone(),
            combat: self.combat.clone(),
            resolution_frames: self.resolution_frames.clone(),
            lki_records: self.lki_records.clone(),
            next_stack_id: self.next_stack_id,
            next_prompt_id: self.next_prompt_id,
            continuous_effects: self.continuous_effects.clone(),
            player_continuous_effects: self.player_continuous_effects.clone(),
            resolution_vms: self.resolution_vms.clone(),
            mulligan_bottom_remaining: self.mulligan_bottom_remaining,
            passed_priority_mask: self.passed_priority_mask,
            last_normal_turn_player: self.last_normal_turn_player,
            additional_combat_phases: self.additional_combat_phases,
            additional_main_after_combat: self.additional_main_after_combat,
            combat_phases_this_turn: self.combat_phases_this_turn,
            turn_serial: self.turn_serial,
            initiative: self.initiative,
            next_continuous_effect_id: self.next_continuous_effect_id,
            pending_replacement_resume: self.pending_replacement_resume,
            pending_sba_batch: self.pending_sba_batch.clone(),
            pending_sba_in_cleanup: self.pending_sba_in_cleanup,
            rare_turn_state: self.rare_turn_state.clone(),
        }
    }

    pub fn from_snapshot(db: Arc<CardDatabase>, snap: GameStateSnapshot) -> Self {
        Self {
            db,
            config: snap.config,
            rng: snap.rng,
            objects: snap.objects,
            rare_states: snap.rare_states,
            object_counters: snap.object_counters,
            players: snap.players,
            stack: snap.stack,
            stack_entries: snap.stack_entries,
            turn_number: snap.turn_number,
            active_player: snap.active_player,
            priority_player: snap.priority_player,
            phase: snap.phase,
            decision: snap.decision,
            next_timestamp: snap.next_timestamp,
            event_seq: snap.event_seq,
            layer_rev: snap.layer_rev,
            dirty_since_eval: snap.dirty_since_eval,
            pending_replacement: snap.pending_replacement,
            prevention_shields: snap.prevention_shields,
            next_shield_id: snap.next_shield_id,
            damage_cant_be_prevented_this_turn: snap.damage_cant_be_prevented_this_turn,
            combat_damage_cant_be_prevented_this_turn: snap
                .combat_damage_cant_be_prevented_this_turn,
            event_log: snap.event_log,
            event_action_depth: snap.event_action_depth,
            defer_flush: snap.defer_flush,
            batch_lki_scopes: snap.batch_lki_scopes,
            turn_history: snap.turn_history,
            batch_triggered_keys: snap.batch_triggered_keys,
            pending_triggers: snap.pending_triggers,
            delayed_triggers: snap.delayed_triggers,
            pending_reflexive_triggers: snap.pending_reflexive_triggers,
            stack_trigger_meta: snap.stack_trigger_meta,
            pending_trigger_order: snap.pending_trigger_order,
            triggered_once_this_turn: snap.triggered_once_this_turn,
            do_this_once_taken_this_turn: snap.do_this_once_taken_this_turn,
            next_trigger_order: snap.next_trigger_order,
            next_delayed_id: snap.next_delayed_id,
            inline_mana_depth: snap.inline_mana_depth,
            monarch: snap.monarch,
            day_night: snap.day_night,
            casting: snap.casting,
            pending_prompt: snap.pending_prompt,
            pending_land_play: snap.pending_land_play,
            mana_riders: snap.mana_riders,
            combat: snap.combat,
            resolution_frames: snap.resolution_frames,
            lki_records: snap.lki_records,
            next_stack_id: snap.next_stack_id,
            next_prompt_id: snap.next_prompt_id,
            continuous_effects: snap.continuous_effects,
            player_continuous_effects: snap.player_continuous_effects,
            resolution_vms: snap.resolution_vms,
            mulligan_bottom_remaining: snap.mulligan_bottom_remaining,
            passed_priority_mask: snap.passed_priority_mask,
            last_normal_turn_player: snap.last_normal_turn_player,
            additional_combat_phases: snap.additional_combat_phases,
            additional_main_after_combat: snap.additional_main_after_combat,
            combat_phases_this_turn: snap.combat_phases_this_turn,
            turn_serial: snap.turn_serial,
            initiative: snap.initiative,
            next_continuous_effect_id: snap.next_continuous_effect_id,
            pending_replacement_resume: snap.pending_replacement_resume,
            pending_sba_batch: snap.pending_sba_batch,
            pending_sba_in_cleanup: snap.pending_sba_in_cleanup,
            rare_turn_state: snap.rare_turn_state,
            layer_cache: OnceLock::new(),
        }
    }
}

pub(crate) fn compute_timed_expiry(
    state: &GameState,
    duration: mtg_ir::Duration,
    controller: PlayerId,
) -> (Option<u32>, Option<PlayerId>) {
    let cur_serial = state.turn_serial();
    match duration {
        mtg_ir::Duration::EndOfTurn | mtg_ir::Duration::UntilEndOfCombat => {
            (Some(cur_serial), None)
        }
        mtg_ir::Duration::UntilYourNextTurn | mtg_ir::Duration::UntilEndOfYourNextTurn => {
            (Some(cur_serial.saturating_add(1)), Some(controller))
        }
        mtg_ir::Duration::ForAsLongAsYouControlSelf => (None, Some(controller)),
        mtg_ir::Duration::Permanent
        | mtg_ir::Duration::UntilSourceLeaves
        | mtg_ir::Duration::ForAsLongAsSelfRemainsTapped => (None, None),
    }
}

pub const MAX_LKI_RECORDS: usize = 64;

#[inline]
fn halve_int_round(v: i32, round: RoundMode) -> i32 {
    match round {
        RoundMode::Up => (v + 1).div_euclid(2),
        RoundMode::Down => v.div_euclid(2),
    }
}

/// Evaluates a `TokenSpec` base object with any layered `CopiableState` exceptions/modifications
/// (when `copiable.card_def == TOKEN_CARD_DEF_ID`). Returns `(effective_name, EvaluatedObject)`.
pub(crate) fn evaluate_token_spec_base(
    spec: &TokenSpec,
    copiable: Option<&CopiableState>,
) -> (String, EvaluatedObject) {
    let mut name = spec.name.clone();
    let mut pt = spec
        .types
        .contains(CardTypeMask::CREATURE)
        .then_some(PowerToughness {
            power: spec.power,
            toughness: spec.toughness,
        });
    let mut types = spec.types;
    let mut subtypes: SmallVec<[SubtypeId; 4]> = spec.subtypes.iter().copied().collect();
    let mut supertypes = spec.supertypes;
    if spec.legendary {
        supertypes.insert(SupertypeMask::LEGENDARY);
    }
    let mut colors = spec.colors & ColorMask::ALL_COLORS;
    let mut keywords: SmallVec<[KeywordId; 4]> = spec.keywords.iter().copied().collect();

    if let Some(except) = copiable.and_then(|c| c.except.as_ref()) {
        if let Some(names) = &except.names
            && !names.is_empty()
        {
            name = names.join(" // ");
        }
        if let Some(st) = except.supertypes {
            supertypes = st;
        }
        supertypes.remove(except.remove_supertypes);
        if let Some(t) = except.types {
            types = t;
        }
        types |= except.add_types;
        if let Some(sts) = &except.subtypes {
            subtypes = sts.iter().copied().collect();
        }
        for &st in &except.add_subtypes {
            if !subtypes.contains(&st) {
                subtypes.push(st);
            }
        }
        if let Some(c) = except.colors {
            colors = c & ColorMask::ALL_COLORS;
        }
        colors |= except.add_colors & ColorMask::ALL_COLORS;
        for &kw in &except.add_keywords {
            if !keywords.contains(&kw) {
                keywords.push(kw);
            }
        }
        if let Some(p) = except.pt {
            pt = Some(p);
        }
    }

    if let Some(mods) = copiable.and_then(|c| c.modifications.as_ref()) {
        if let Some(ov) = &mods.override_name {
            name = ov.clone();
        } else if let Some(nm) = &mods.name
            && !mods.keep_self_name
        {
            name = nm.clone();
        }
        if let Some(t) = mods.set_types {
            types = t;
        }
        types |= mods.add_types;
        supertypes |= mods.add_supertypes;
        supertypes.remove(mods.remove_supertypes);
        if let Some(sts) = &mods.set_subtypes {
            subtypes = sts.iter().copied().collect();
        }
        for &st in &mods.add_subtypes {
            if !subtypes.contains(&st) {
                subtypes.push(st);
            }
        }
        if let Some(c) = mods.set_colors {
            colors = c & ColorMask::ALL_COLORS;
        }
        colors |= mods.add_colors & ColorMask::ALL_COLORS;
        for &kw in &mods.add_keywords {
            if !keywords.contains(&kw) {
                keywords.push(kw);
            }
        }
        if mods.power.is_some() || mods.toughness.is_some() {
            let base = pt.unwrap_or(PowerToughness {
                power: 0,
                toughness: 0,
            });
            pt = Some(PowerToughness {
                power: mods.power.unwrap_or(base.power),
                toughness: mods.toughness.unwrap_or(base.toughness),
            });
        }
        if let Some(round) = mods.halve_pt
            && let Some(p) = &mut pt
        {
            p.power = halve_int_round(p.power, round);
            p.toughness = halve_int_round(p.toughness, round);
        }
    }

    (
        name,
        EvaluatedObject {
            pt,
            base_pt: pt,
            types,
            subtypes,
            supertypes,
            colors,
            mana_value: 0,
            keywords,
            has_all_creature_types: false,
            lost_all_abilities: false,
            loyalty: None,
            defense: None,
            controller: PlayerId(0),
            name_override: None,
            mana_cost: ManaCost::default(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtg_ir::{
        CardDef, CardLayout, Characteristics, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle,
        PrintedPt,
    };

    const _: () = {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_copy<T: Copy>() {}
        fn check() {
            assert_send_sync::<GameState>();
            assert_send_sync::<LayerCache>();
            assert_send_sync::<GameStateSnapshot>();
            assert_copy::<GameObject>();
            assert_copy::<ReplacementSourceId>();
            assert!(std::mem::size_of::<GameObject>() <= 48);
        }
        let _ = check;
    };

    fn test_db_with_cards(count: usize) -> Arc<CardDatabase> {
        let mut cards = Vec::with_capacity(count);
        for i in 0..count {
            cards.push(CardDef {
                oracle_id: format!("oracle_{i}"),
                compiler_version: 46,
                text_hash: "0000".into(),
                name: format!("Card {i}"),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: format!("Card {i}"),
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
            });
        }
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn game_state_new_reverses_library_for_ts_top_parity_and_draws_opening_hand() {
        let db = test_db_with_cards(12);
        let mainboard: Vec<CardDefId> = (0..12).map(|i| CardDefId(i as u32)).collect();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.rng_mode = RngMode::Mulberry32Compat;
        cfg.starting_hand_size = 7;

        let state = GameState::new(Arc::clone(&db), cfg, &[deck.clone(), deck], 42).unwrap();
        // Golden Mulberry32(42) shuffle of 0..12 is [2, 0, 5, 10, 9, 11, 3, 1, 6, 8, 4, 7].
        // TS takes [0..7] for opening hand: [2, 0, 5, 10, 9, 11, 3]; remaining [1, 6, 8, 4, 7].
        // Rust reverses so top is at library.last(): remaining library bottom->top is [7, 4, 8, 6, 1].
        let p0 = &state.players()[0];
        let hand_defs: Vec<u32> = p0
            .hand
            .iter()
            .map(|&id| state.object(id).unwrap().card_def.0)
            .collect();
        assert_eq!(hand_defs, vec![2, 0, 5, 10, 9, 11, 3]);

        let lib_defs: Vec<u32> = p0
            .library
            .iter()
            .map(|&id| state.object(id).unwrap().card_def.0)
            .collect();
        assert_eq!(lib_defs, vec![7, 4, 8, 6, 1]);
        assert_eq!(
            p0.library
                .last()
                .map(|&id| state.object(id).unwrap().card_def.0),
            Some(1)
        );
    }

    #[test]
    fn oncelock_layer_cache_selective_invalidation_and_clone_reset() {
        let db = test_db_with_cards(1);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 99).unwrap();

        assert!(!state.has_cached_layer_eval());
        let rev0 = state.evaluate_board().rev;
        assert!(state.has_cached_layer_eval());

        // Life change does not intersect TOPOLOGY | COUNTERS, so cache is retained and rev advances.
        state.mark_dirty(DirtyMask::LIFE);
        assert!(state.has_cached_layer_eval());
        assert_eq!(state.evaluate_board().rev, rev0 + 1);

        // Clone resets OnceLock to empty.
        let cloned = state.clone();
        assert!(!cloned.has_cached_layer_eval());
        assert_eq!(cloned.layer_rev(), state.layer_rev());

        // Topology change intersects reads_mask, invalidating OnceLock.
        state.mark_dirty(DirtyMask::TOPOLOGY);
        assert!(!state.has_cached_layer_eval());
        let rev_after = state.evaluate_board().rev;
        assert_eq!(rev_after, rev0 + 2);
    }

    #[test]
    fn snapshot_round_trip_preserves_commander_counters_and_state() {
        let db = test_db_with_cards(2);
        let deck = DeckSpec {
            commanders: vec![CardDefId(1)],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::commander(2),
            &[deck.clone(), deck],
            7,
        )
        .unwrap();

        let cmd0 = state.players()[0].command[0];
        state.increment_commander_cast_count(PlayerId(0), cmd0);
        state.add_commander_damage(PlayerId(1), cmd0, 7);
        assert_eq!(state.commander_cast_count(PlayerId(0), cmd0), 1);
        assert_eq!(state.commander_damage_taken(PlayerId(1), cmd0), 7);

        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: GameStateSnapshot = serde_json::from_str(&json).unwrap();
        let restored = GameState::from_snapshot(db, decoded);
        assert_eq!(restored.commander_cast_count(PlayerId(0), cmd0), 1);
        assert_eq!(restored.commander_damage_taken(PlayerId(1), cmd0), 7);
        assert_eq!(restored.players()[0].hand.len(), 7);
        assert!(!restored.has_cached_layer_eval());
    }

    #[test]
    fn mutators_mark_expected_dirty_masks() {
        let db = test_db_with_cards(1);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 55).unwrap();

        state.dirty_since_eval = DirtyMask::empty();
        let rev0 = state.layer_rev();
        state.set_turn_number(2);
        assert_eq!(state.layer_rev(), rev0 + 1);
        assert_eq!(state.dirty_since_eval(), DirtyMask::TURN_HISTORY);

        state.dirty_since_eval = DirtyMask::empty();
        state.set_active_player(PlayerId(1));
        assert_eq!(state.layer_rev(), rev0 + 2);
        assert_eq!(state.dirty_since_eval(), DirtyMask::TURN_HISTORY);

        state.dirty_since_eval = DirtyMask::empty();
        state.set_phase(GamePhase::CombatDamage);
        assert_eq!(state.layer_rev(), rev0 + 3);
        assert_eq!(
            state.dirty_since_eval(),
            DirtyMask::TURN_HISTORY | DirtyMask::COMBAT
        );

        state.dirty_since_eval = DirtyMask::empty();
        state.set_player_commander_color_identity(PlayerId(0), ColorMask::W | ColorMask::U);
        assert_eq!(state.layer_rev(), rev0 + 4);
        assert_eq!(state.dirty_since_eval(), DirtyMask::PLAYER_STATE);
    }

    #[test]
    fn game_object_layout_and_populated_snapshot_round_trip() {
        assert_eq!(std::mem::size_of::<GameObject>(), 40);

        let db = test_db_with_cards(1);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            77,
        )
        .unwrap();

        let obj_id = state.players()[0].hand[0];
        assert!(state.add_counter(obj_id, CounterId(0), 3));
        assert_eq!(state.counter_count(obj_id, CounterId(0)), 3);
        assert!(state.set_counter(obj_id, CounterId(1), 2));
        assert_eq!(state.remove_counter(obj_id, CounterId(1), 2), 2);
        assert_eq!(state.counter_count(obj_id, CounterId(1)), 0);

        {
            let rare = state.rare_or_default(obj_id).unwrap();
            rare.token_spec = Some(Arc::new(TokenSpec {
                name: "Synth".into(),
                power: 2,
                toughness: 2,
                colors: ColorMask::G,
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: "Creature".into(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: vec![],
                legendary: false,
            }));
            rare.copiable = Some(Arc::new(CopiableState {
                card_def: TOKEN_CARD_DEF_ID,
                face_index: Some(0),
                fused: false,
                is_single_face_copy: false,
                prototyped: false,
                merged_components: SmallVec::new(),
                except: None,
                modifications: None,
            }));
            rare.face_down = Some(Arc::new(FaceDownSpec::default()));
        }

        let snap_lki = Arc::new(LkiSnapshot {
            object_id: obj_id,
            card_def: CardDefId(0),
            name: "Card 0".into(),
            owner: PlayerId(0),
            controller: PlayerId(0),
            last_zone: Zone::Battlefield,
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
            keywords: SmallVec::new(),
            counters: SmallVec::from_slice(&[(CounterId(0), 3)]),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        });
        state.record_lki(obj_id, Arc::clone(&snap_lki));
        let sid = state.allocate_stack_id();
        state.stack_mut().push(obj_id);
        state.stack_entries_mut().push(StackEntry {
            id: sid,
            kind: StackEntryKind::Activated {
                source: obj_id,
                ability_idx: 0,
                lki: Arc::clone(&snap_lki),
            },
            controller: PlayerId(0),
            choices: StackChoices::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });

        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: GameStateSnapshot = serde_json::from_str(&json).unwrap();
        let restored = GameState::from_snapshot(db, decoded);
        assert_eq!(restored.counter_count(obj_id, CounterId(0)), 3);
        assert_eq!(
            restored
                .rare(obj_id)
                .and_then(|r| r.token_spec.as_ref())
                .map(|t| t.name.as_str()),
            Some("Synth")
        );
        assert!(
            restored
                .rare(obj_id)
                .and_then(|r| r.copiable.as_ref())
                .is_some()
        );
        assert!(
            restored
                .rare(obj_id)
                .and_then(|r| r.face_down.as_ref())
                .is_some()
        );
        assert_eq!(
            restored.lki(obj_id).map(|l| l.name.as_str()),
            Some("Card 0")
        );
        assert_eq!(restored.stack_entries().len(), 1);
    }

    #[test]
    fn cow_arc_mana_ext_and_rider_state_sizes_pruning_and_round_trip() {
        assert_eq!(std::mem::size_of::<GameObject>(), 40);
        assert!(
            std::mem::size_of::<PlayerState>() <= 360,
            "PlayerState size {} exceeded 360 B",
            std::mem::size_of::<PlayerState>()
        );
        assert!(
            std::mem::size_of::<GameState>() <= 2900,
            "GameState size {} exceeded 2900 B",
            std::mem::size_of::<GameState>()
        );

        let db = test_db_with_cards(1);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            123,
        )
        .unwrap();

        let obj_id = state.players()[0].hand[0];
        assert!(state.players()[0].mana_ext().is_none());

        // Populate mana_ext with turn-only fields and verify clear_turn_counters prunes back to None.
        {
            let p = state.player_mut(PlayerId(0)).unwrap();
            let ext = p.mana_ext_mut();
            ext.unemptied_mana_colors_this_turn = ColorMask::G;
            ext.once_per_turn_abilities_used.push((obj_id, 0));
            assert!(p.mana_ext().is_some());
            p.clear_turn_counters();
            assert!(p.mana_ext().is_none());
        }

        // Populate persistent mana_ext + turn_history + mana_riders + PendingNonManaCostStep and round-trip.
        {
            let p = state.player_mut(PlayerId(0)).unwrap();
            let ext = p.mana_ext_mut();
            ext.snow_mana_pool[4] = 1;
            ext.once_per_game_abilities_used.push((obj_id, 2));
            ext.restricted_mana.push(RestrictedManaBucket {
                colors: [0, 0, 0, 0, 1, 0],
                restriction: Arc::new(ManaSpendingRestriction::default()),
                source_id: Some(obj_id),
                source_tag: None,
                source_kind_flags: 0,
                is_snow: true,
                retain_until_eot: false,
                delayed_rider: None,
            });
            p.clear_turn_counters();
            assert!(p.mana_ext().is_some());
        }

        state.turn_history_mut().first_upkeep_seen.push(PlayerId(0));
        let sid = state.allocate_stack_id();
        state.push_pending_mana_delayed_rider(PendingManaDelayedRider {
            controller: PlayerId(0),
            source: Some(obj_id),
            payload: Arc::new(DelayedTriggerPayload {
                trigger: Box::new(mtg_ir::TriggerDef {
                    event: mtg_ir::EventKind::ZoneChange,
                    filter: mtg_ir::EventFilter::default(),
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: false,
                    do_this_once_per_turn: false,
                    is_state_trigger: false,
                    or: vec![],
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                }),
                effect: vec![],
                targets: vec![],
                next_turn: false,
            }),
        });
        let riders = state.take_pending_mana_delayed_riders();
        assert_eq!(riders.len(), 1);
        assert!(state.mana_riders.is_none());
        state.push_stack_mana_riders(sid, riders);
        assert_eq!(state.stack_mana_riders().len(), 1);

        state.set_casting(Some(CastingState {
            caster: PlayerId(0),
            object: obj_id,
            face_idx: 0,
            from_zone: Zone::Hand,
            step: CastStep::PayCosts,
            chosen_modes: SmallVec::new(),
            x_value: None,
            optional_costs: SmallVec::new(),
            optional_cost_cursor: 1,
            targets: vec![],
            divisions: SmallVec::new(),
            locked_cost: ManaCost::default(),
            mana_paid: SmallVec::new(),
            paid_mana_colors: ColorMask::empty(),
            kind: CastKind::Spell,
            option_id: 0,
            alt_kind: AltCastKind::Normal,
            is_kicked: false,
            kicked_indices: SmallVec::new(),
            kicked_tags: SmallVec::new(),
            fused: false,
            cleaved: false,
            gift_promised: false,
            additional_cost_paid: false,
            bargained: false,
            target_spec_cursor: 0,
            targets_by_spec: vec![],
            division_cursor: 0,
            payment_plan: Some(Box::new(PendingCostPaymentPlan {
                locked_mana_cost: ManaCost::default(),
                remaining_mana: ManaCost::default(),
                remaining_non_mana_costs: vec![],
                non_mana_cursor: 0,
                pending_step: Some(PendingNonManaCostStep::ZoneMove {
                    kind: PendingZoneCostKind::Sacrifice,
                    current_object: obj_id,
                    remaining_objects: vec![],
                }),
                chosen_optional_costs: SmallVec::new(),
                convoke_tapped: SmallVec::new(),
                improvise_tapped: SmallVec::new(),
                delve_exiled: SmallVec::new(),
                emerge_sacrificed: None,
                waterbend_tapped: SmallVec::new(),
                sacrificed_lki: vec![],
                revealed_cards: vec![],
                revealed_card_defs: vec![],
                discarded_cards: vec![],
                counters_removed: 0,
                phyrexian_pay_life: SmallVec::new(),
                cant_be_countered_from_mana: false,
                mana_commander_bonus_counters: 0,
            })),
            paid_mana_sources: SmallVec::new(),
            cant_be_countered_from_mana: false,
            mana_commander_bonus_counters: 0,
            mana_spent_total: 0,
            cast_option: None,
            pre_cast_exile: None,
        }));

        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: GameStateSnapshot = serde_json::from_str(&json).unwrap();
        let mut restored = GameState::from_snapshot(db, decoded);
        assert_eq!(restored.players()[0].snow_mana_pool()[4], 1);
        assert_eq!(restored.players()[0].restricted_mana().len(), 1);
        assert_eq!(restored.turn_history().first_upkeep_seen.len(), 1);
        assert_eq!(restored.casting().unwrap().optional_cost_cursor, 1);
        assert!(matches!(
            restored
                .casting()
                .unwrap()
                .payment_plan
                .as_ref()
                .unwrap()
                .pending_step,
            Some(PendingNonManaCostStep::ZoneMove {
                kind: PendingZoneCostKind::Sacrifice,
                ..
            })
        ));
        let taken = restored.take_stack_mana_riders(sid);
        assert_eq!(taken.len(), 1);
        assert!(restored.mana_riders.is_none());

        restored
            .player_mut(PlayerId(0))
            .unwrap()
            .protection_from_everything = Some(mtg_ir::Duration::UntilYourNextTurn);
        let snap2 = restored.to_snapshot();
        let json2 = serde_json::to_string(&snap2).unwrap();
        let decoded2: GameStateSnapshot = serde_json::from_str(&json2).unwrap();
        let restored2 = GameState::from_snapshot(restored.db, decoded2);
        assert_eq!(
            restored2
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything,
            Some(mtg_ir::Duration::UntilYourNextTurn)
        );
        assert_eq!(
            compute_timed_expiry(&restored2, mtg_ir::Duration::EndOfTurn, PlayerId(0)),
            (Some(restored2.turn_serial()), None)
        );
        assert_eq!(
            compute_timed_expiry(&restored2, mtg_ir::Duration::UntilYourNextTurn, PlayerId(1)),
            (
                Some(restored2.turn_serial().saturating_add(1)),
                Some(PlayerId(1))
            )
        );
        assert_eq!(
            compute_timed_expiry(
                &restored2,
                mtg_ir::Duration::UntilEndOfYourNextTurn,
                PlayerId(0)
            ),
            (
                Some(restored2.turn_serial().saturating_add(1)),
                Some(PlayerId(0))
            )
        );
        assert_eq!(
            compute_timed_expiry(
                &restored2,
                mtg_ir::Duration::ForAsLongAsYouControlSelf,
                PlayerId(0)
            ),
            (None, Some(PlayerId(0)))
        );
    }

    #[test]
    fn snapshot_postcard_round_trip_preserves_m5b_and_rare_state() {
        let db = test_db_with_cards(2);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            2026,
        )
        .unwrap();

        let obj_id = state.players()[0].hand[0];
        let p0 = state.player_mut(PlayerId(0)).unwrap();
        p0.poison = 3;
        p0.energy = 4;
        p0.experience = 2;
        p0.rad = 1;
        p0.skip_draw_step_once = true;
        p0.no_max_hand_size_rest_of_game = true;
        p0.prevent_all_combat_damage_this_turn = true;

        if let Some(rare) = state.rare_or_default(obj_id) {
            rare.face_down = Some(Arc::new(FaceDownSpec {
                kind: FaceDownKind::Morph,
                turn_up_mana_cost: Some(ManaCost::parse("{1}{G}")),
                turn_up_cost_defs: vec![CostDef::PayLife {
                    amount: Box::new(mtg_ir::Amount::Const { n: 2 }),
                    half_life_rounded_up: false,
                }],
                ward_cost: None,
                supertypes: SupertypeMask::empty(),
                types: CardTypeMask::CREATURE,
                subtypes: SmallVec::new(),
                pt: Some(PowerToughness {
                    power: 2,
                    toughness: 2,
                }),
                colors: ColorMask::empty(),
                known_to: SmallVec::from_slice(&[PlayerId(0)]),
                opaque_seq: 7,
            }));
        }

        state.combat = CombatState {
            active: true,
            attackers: vec![AttackerEntry {
                attacker: obj_id,
                target: AttackTarget::Player { id: PlayerId(1) },
            }],
            blockers: Vec::new(),
            damage_assignment_order: Vec::new(),
            first_strike_dealt: true,
            combat_damage_dealt: false,
            attacked_this_combat: vec![obj_id],
            attackers_declared: true,
            declared_blockers_players: vec![PlayerId(1)],
            blocked_attackers: Vec::new(),
            dealt_first_strike_ids: vec![obj_id],
            pending_damage: Some(Box::new(PendingCombatDamageState::default())),
        };

        let sid = state.allocate_stack_id();
        state.resolution_vms.push(ResolutionVmState {
            stack_item: sid,
            controller: PlayerId(0),
            source: Some(obj_id),
            source_lki: None,
            x_value: 3,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
            targets_by_spec: vec![vec![TargetRef::Player { id: PlayerId(1) }]],
            divisions: SmallVec::new(),
            step_results: vec![StepAcceptance::Accepted],
            pause_state: Some(StepPauseState::SelectionPick {
                chooser: PlayerId(0),
                candidates: vec![obj_id],
                selected: Vec::new(),
                min: 1,
                max: 1,
            }),
            pending_continuation: None,
            subframes: Vec::new(),
            step_acted_ids: vec![obj_id],
            already_selected_controllers: SmallVec::from_slice(&[PlayerId(0)]),
            each_loop_player: Some(PlayerId(0)),
            chosen_opponent: Some(PlayerId(1)),
            event_object: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: Some(PlayerId(1)),
            last_effect_amount: 2,
            last_excess_damage: 1,
            last_counters_removed: 0,
            last_roll_result: 6,
            coin_heads_count: 1,
            counters_removed_this_way: 0,
            step_way_stats: StepWayStats::default(),
        });

        state
            .rare_turn_state_mut()
            .extra_turn_queue
            .push(PlayerId(1));
        state
            .rare_turn_state_mut()
            .regeneration_shields
            .push((obj_id, 2));

        state.pending_replacement = Some(Box::new(PendingReplacementState {
            prompt_id: 11,
            chooser: PlayerId(0),
            event: ReplaceableEventKind::Draw {
                player: PlayerId(0),
                count: 1,
                source: None,
            },
            applied: SmallVec::new(),
            candidates: vec![ReplacementSourceId::non_card(
                ReplacementSourceKind::PreventionShield,
                NONE_OBJECT_ID,
                1,
            )],
            prompt_kind: PendingReplacementPrompt::ChooseReplacement,
            mods: ReplacementAccumulatedMods::default(),
            continuation: ReplacementContinuation::DrawEffect,
            resume_decision: DecisionState::Priority,
            resume_priority_player: PlayerId(0),
        }));

        state.pending_prompt = Some(Box::new(PendingPromptContext {
            prompt_id: 12,
            chooser: PlayerId(0),
            request: ChoiceRequest::ChooseObjects {
                prompt_id: 12,
                candidates: vec![obj_id],
                min: 1,
                max: 1,
            },
            owner: PromptOwner::Turn(TurnPromptKind::CleanupDiscard {
                remaining: 1,
                picked: Vec::new(),
            }),
        }));

        state.initiative = Some(PlayerId(0));
        state.monarch = Some(PlayerId(1));
        state.day_night = Some(true);
        state.mulligan_bottom_remaining = [1, 2, 0, 0, 0, 0, 0, 0];
        state.passed_priority_mask = 0b01;
        state.last_normal_turn_player = Some(PlayerId(0));
        state.additional_combat_phases = 1;
        state.additional_main_after_combat = true;
        state.combat_phases_this_turn = 1;
        state.turn_serial = 5;
        state.pending_sba_in_cleanup = true;

        let lki_snap = Arc::new(LkiSnapshot {
            object_id: obj_id,
            card_def: CardDefId(0),
            name: "Synth Bear 0".into(),
            owner: PlayerId(0),
            controller: PlayerId(0),
            last_zone: Zone::Hand,
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: false,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        });
        state.lki_records_mut().push((obj_id, lki_snap));

        let snap = state.to_snapshot();
        let bytes = postcard::to_allocvec(&snap).expect("postcard serialize snapshot");
        let decoded: GameStateSnapshot =
            postcard::from_bytes(&bytes).expect("postcard deserialize snapshot");
        let restored = GameState::from_snapshot(db, decoded);

        let rp0 = restored.player(PlayerId(0)).unwrap();
        assert_eq!(rp0.poison, 3);
        assert_eq!(rp0.energy, 4);
        assert_eq!(rp0.experience, 2);
        assert_eq!(rp0.rad, 1);
        assert!(rp0.skip_draw_step_once);
        assert!(rp0.no_max_hand_size_rest_of_game);
        assert!(rp0.prevent_all_combat_damage_this_turn);

        let rfd = restored
            .rare(obj_id)
            .and_then(|r| r.face_down.as_deref())
            .unwrap();
        assert_eq!(rfd.kind, FaceDownKind::Morph);
        assert_eq!(rfd.turn_up_mana_cost, Some(ManaCost::parse("{1}{G}")));
        assert_eq!(rfd.turn_up_cost_defs.len(), 1);

        assert!(restored.combat().active);
        assert!(restored.combat().first_strike_dealt);
        assert!(restored.combat().pending_damage.is_some());
        assert_eq!(restored.resolution_vms().len(), 1);
        assert_eq!(restored.resolution_vms()[0].x_value, 3);
        assert_eq!(
            restored
                .rare_turn_state()
                .map(|r| r.extra_turn_queue.as_slice()),
            Some([PlayerId(1)].as_slice())
        );
        assert_eq!(
            restored.pending_replacement().map(|p| p.prompt_id),
            Some(11)
        );
        assert_eq!(restored.pending_prompt().map(|p| p.prompt_id), Some(12));
        assert_eq!(restored.initiative(), Some(PlayerId(0)));
        assert_eq!(restored.monarch(), Some(PlayerId(1)));
        assert_eq!(restored.day_night(), Some(true));
        assert_eq!(restored.mulligan_bottom_remaining(PlayerId(1)), 2);
        assert_eq!(restored.turn_serial(), 5);
        assert!(restored.pending_sba_in_cleanup);
        assert_eq!(
            restored.lki(obj_id).map(|l| l.name.as_str()),
            Some("Synth Bear 0")
        );
    }

    #[test]
    fn synth_prune_stale_lki_retains_across_prompt_and_pending_triggers_and_gc_on_delayed() {
        let db = test_db_with_cards(2);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            909,
        )
        .unwrap();

        let make_lki = |id: ObjectId, name: &str, attached_ids: SmallVec<[ObjectId; 2]>| {
            Arc::new(LkiSnapshot {
                object_id: id,
                card_def: CardDefId(0),
                name: name.into(),
                owner: PlayerId(0),
                controller: PlayerId(0),
                last_zone: Zone::Battlefield,
                pt: Some(PowerToughness {
                    power: 3,
                    toughness: 3,
                }),
                types: CardTypeMask::CREATURE,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::R,
                mana_value: 3,
                keywords: SmallVec::new(),
                counters: SmallVec::new(),
                damage_marked: 0,
                is_token: false,
                is_commander: false,
                tapped: false,
                phased_out: false,
                face_down: false,
                lost_all_abilities: false,
                has_all_creature_types: false,
                attached_ids,
                chosen_color: None,
                chosen_subtype: None,
                chosen_card_name: None,
                active_face_index: 0,
            })
        };

        let dead_referenced = ObjectId {
            index: 100,
            generation: 3,
        };
        let dead_attached_aura = ObjectId {
            index: 101,
            generation: 2,
        };
        let dead_unreferenced = ObjectId {
            index: 102,
            generation: 4,
        };
        let src_id = state.players()[0].hand[0];

        state.record_lki(
            dead_referenced,
            make_lki(
                dead_referenced,
                "Synth Ref Elemental",
                smallvec::smallvec![dead_attached_aura],
            ),
        );
        state.record_lki(
            dead_attached_aura,
            make_lki(dead_attached_aura, "Synth Ref Aura", SmallVec::new()),
        );
        state.record_lki(
            dead_unreferenced,
            make_lki(dead_unreferenced, "Synth Unref Bear", SmallVec::new()),
        );

        let dummy_trigger_def = mtg_ir::TriggerDef {
            event: mtg_ir::EventKind::Sacrificed,
            filter: mtg_ir::EventFilter::default(),
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
        let src_lki = make_lki(src_id, "Synth Source", SmallVec::new());
        state
            .pending_triggers
            .push(crate::triggers::PendingTrigger {
                order: 1,
                mode: crate::triggers::TriggerMode::Stack,
                source: src_id,
                source_lki: Arc::clone(&src_lki),
                controller: PlayerId(0),
                kind: crate::triggers::TriggerSourceKind::Innate {
                    component_index: 0,
                    face_index: 0,
                    ability_idx: 0,
                },
                trigger_def: dummy_trigger_def.clone(),
                condition: None,
                targets: Vec::new(),
                effect: Vec::new(),
                modal: None,
                event_object: Some(dead_referenced),
                event_lki: Some(make_lki(
                    dead_referenced,
                    "Synth Ref Elemental",
                    smallvec::smallvec![dead_attached_aura],
                )),
                event_player: Some(PlayerId(0)),
                event_amount: None,
                defending_player: None,
                last_tapped_mana_colors: SmallVec::new(),
                is_look_back: false,
                x_value: 0,
                bindings: Vec::new(),
            });
        state.pending_prompt = Some(Box::new(PendingPromptContext {
            prompt_id: 77,
            chooser: PlayerId(0),
            request: ChoiceRequest::YesNo {
                prompt_id: 77,
                label: "Return commander?".into(),
            },
            owner: PromptOwner::Sba(SbaPromptKind::CommanderReturn {
                owner: PlayerId(0),
                commander: ObjectId {
                    index: 100,
                    generation: 4,
                },
                from_zone: Zone::Graveyard,
            }),
        }));
        // Also add a delayed trigger to verify unreferenced LKI is still GC'd even when delayed_triggers is non-empty.
        state
            .delayed_triggers
            .push(crate::triggers::DelayedTriggerEntry {
                id: 1,
                controller: PlayerId(0),
                source_card: CardDefId(0),
                source_object: Some(src_id),
                source_lki: Some(src_lki),
                trigger: dummy_trigger_def,
                condition: None,
                targets: Vec::new(),
                effect: Vec::new(),
                expiry: crate::triggers::DelayedExpiry::ThisTurn,
                watched_objects: SmallVec::new(),
                x_value: 0,
                bindings: Vec::new(),
            });

        assert!(state.stack().is_empty());
        assert!(state.resolution_frames().is_empty());

        state.prune_stale_lki();
        assert!(state.lki(dead_referenced).is_some());
        assert!(state.lki(dead_attached_aura).is_some());
        assert!(state.lki(dead_unreferenced).is_none());

        // Snapshot round-trip preserves retained LKI deterministically.
        let bytes = postcard::to_allocvec(&state.to_snapshot()).unwrap();
        let decoded: GameStateSnapshot = postcard::from_bytes(&bytes).unwrap();
        let mut restored = GameState::from_snapshot(db, decoded);
        assert_eq!(
            restored.lki(dead_referenced).map(|l| l.name.as_str()),
            Some("Synth Ref Elemental")
        );
        assert_eq!(
            restored.lki(dead_attached_aura).map(|l| l.name.as_str()),
            Some("Synth Ref Aura")
        );
        assert!(restored.lki(dead_unreferenced).is_none());

        // Once all active holders clear, idle fast-path clears lki_records completely.
        restored.pending_prompt = None;
        restored.pending_triggers.clear();
        restored.delayed_triggers.clear();
        restored.prune_stale_lki();
        assert!(restored.lki(dead_referenced).is_none());
        assert!(restored.lki(dead_attached_aura).is_none());
        assert!(restored.lki_records.is_empty());
    }
}
