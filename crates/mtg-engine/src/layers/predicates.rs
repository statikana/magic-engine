use std::cell::Cell;

use bitflags::bitflags;
use mtg_ir::{
    AbilityDef, Amount, CardDatabase, CardDefId, CardTypeMask, ColorMask, Condition, CounterId,
    Duration, FilterFlags, KeywordId, Layer, LayerMod, ManaColor, ObjectFilter, ObjectPredicate,
    PlayerFilter, RuleMod, Span, StaticDef, SubtypeId, Zone, ZoneMask,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::eval::LayerBoardView;
use crate::id::{ObjectId, PlayerId};
use crate::state::{DirtyMask, EvaluatedObject, ReplacementGrantOrigin};
use crate::view::PowerToughness;

/// Target selector for a [`TimedContinuousEffect`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContinuousTarget {
    /// Specific resolved object handles (e.g. "target creature gets +2/+2 until end of turn").
    Objects(SmallVec<[ObjectId; 4]>),
    /// Dynamic filter evaluated inside layer resolution (e.g. "creatures you control gain +2/+2").
    Filter(ObjectFilter),
}

/// Active continuous effect created by a resolved spell, activated/triggered ability, or static grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedContinuousEffect {
    pub id: u64,
    pub source: Option<ObjectId>,
    pub controller: PlayerId,
    pub layer: Layer,
    pub affected: ContinuousTarget,
    pub modifier: LayerMod,
    pub duration: Duration,
    pub timestamp: u64,
    pub expires_turn: Option<u32>,
    pub expires_on_player: Option<PlayerId>,
    pub chosen_color: Option<ColorMask>,
    pub chosen_subtype: Option<SubtypeId>,
    pub chosen_player: Option<PlayerId>,
    pub chosen_card_type: Option<CardTypeMask>,
    pub locked_pt: Option<PowerToughness>,
    pub is_cda: bool,
}

/// Active player-facing continuous rule modifier (`CR 613.11`) created by a resolved spell/ability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerContinuousEffect {
    pub id: u64,
    pub source: Option<ObjectId>,
    pub controller: PlayerId,
    pub who: PlayerFilter,
    pub rule: RuleMod,
    pub duration: Duration,
    pub timestamp: u64,
    pub expires_turn: Option<u32>,
    pub expires_on_player: Option<PlayerId>,
    pub chosen_card_name: Option<String>,
    pub chosen_card_type: Option<CardTypeMask>,
    pub linked_exile_names: SmallVec<[String; 2]>,
}

/// Origin category for a static effect in layer evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum StaticGrantOrigin {
    #[default]
    Innate,
    TimedEffect,
    Nested,
}

/// Deterministic provenance metadata carried from source static ability collection into
/// [`LayerEffectItem`], [`WorkStatic`], [`GrantedAbilityEntry::CardAbility`], and
/// [`GrantedAbilityEntry::Inline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct StaticProvenance {
    pub origin: StaticGrantOrigin,
    pub card_def: Option<CardDefId>,
    pub face_index: u8,
    pub component_index: u8,
    pub ability_index: u32,
    pub effect_id: u64,
}

/// Ability granted to a permanent in Layer 4 (`CR 305.7`) or Layer 6 (`CR 613.1f`).
#[derive(Debug, Clone, PartialEq)]
pub enum GrantedAbilityEntry {
    /// Intrinsic `{T}: Add {W/U/B/R/G}` mana ability conferred by a basic land subtype (`CR 305.7`).
    IntrinsicBasicMana {
        color: ManaColor,
        subtype: SubtypeId,
    },
    /// Activated, triggered, or keyword-backed ability copied/granted from an existing card face.
    CardAbility {
        card_def: CardDefId,
        face_index: u8,
        ability_index: usize,
        is_mana: bool,
        mana_as_any_color: bool,
        once_each_turn: bool,
        grant_source: Option<ObjectId>,
        grant_controller: PlayerId,
        donor_object: ObjectId,
        donor_component_index: u8,
        granter_origin: StaticGrantOrigin,
        granter_card_def: Option<CardDefId>,
        granter_face_index: u8,
        granter_component_index: u8,
        granter_ability_index: u32,
        source_effect_id: u64,
    },
    /// Inline ability specified directly inside `LayerMod::GrantAbility`.
    Inline {
        ability: Box<AbilityDef>,
        is_mana: bool,
        grant_source: Option<ObjectId>,
        grant_controller: PlayerId,
        grant_origin: ReplacementGrantOrigin,
        grant_card_def: Option<CardDefId>,
        source_face_index: u8,
        source_component_index: u8,
        source_ability_index: u32,
        source_effect_id: u64,
    },
    /// Synthesized ward ability from `StaticBody::DynamicWardMana`.
    DynamicWard {
        generic: u32,
        grant_source: Option<ObjectId>,
    },
}

/// Sparse per-object layer extras stored on [`crate::state::LayerCache`] without bloating `EvaluatedObject`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EvaluatedObjectExtras {
    pub innate_abilities_removed: bool,
    pub innate_mana_abilities_removed: bool,
    pub removed_ability_timestamps: SmallVec<[(u64, bool); 2]>,
    pub granted_abilities: Vec<GrantedAbilityEntry>,
    pub cant_have_keywords: SmallVec<[KeywordId; 2]>,
    pub protection_exemptions: SmallVec<[(KeywordId, mtg_ir::ProtectionAttachmentExemption); 2]>,
    pub protection_from_chosen_color:
        Option<(ColorMask, Option<mtg_ir::ProtectionAttachmentExemption>)>,
    pub protection_from_players: SmallVec<[PlayerId; 2]>,
    pub protection_from_card_types: CardTypeMask,
    pub copied_keywords_from_exiled: SmallVec<[KeywordId; 4]>,
    pub parameterized_keywords: SmallVec<[(KeywordId, String); 2]>,
}

impl EvaluatedObjectExtras {
    #[inline]
    pub fn is_default(&self) -> bool {
        !self.innate_abilities_removed
            && !self.innate_mana_abilities_removed
            && self.removed_ability_timestamps.is_empty()
            && self.granted_abilities.is_empty()
            && self.cant_have_keywords.is_empty()
            && self.protection_exemptions.is_empty()
            && self.protection_from_chosen_color.is_none()
            && self.protection_from_players.is_empty()
            && self.protection_from_card_types.is_empty()
            && self.copied_keywords_from_exiled.is_empty()
            && self.parameterized_keywords.is_empty()
    }
}

/// Active player-scoped `RuleMod` entry retained for parameterized queries (taxes, caps, filters).
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRuleModEntry {
    pub source: Option<ObjectId>,
    pub controller: PlayerId,
    pub rule: RuleMod,
    pub chosen_card_name: Option<String>,
    pub chosen_card_type: Option<CardTypeMask>,
    pub linked_exile_names: SmallVec<[String; 2]>,
}

/// Layer 613.11 evaluated player continuous rule state (`CR 613.10–613.11`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EvaluatedPlayerState {
    pub no_max_hand_size: bool,
    pub max_hand_size_delta: i32,
    pub max_hand_size_set: Option<u16>,
    pub cant_lose: bool,
    pub dont_lose_at_zero_life: bool,
    pub opponents_cant_win: bool,
    pub cast_only_sorcery_speed: bool,
    pub one_spell_per_turn: bool,
    pub cast_only_from_hand: bool,
    pub cast_only_during_own_turn: bool,
    pub activate_only_during_own_turn: bool,
    pub hexproof: bool,
    pub shroud: bool,
    pub life_cant_change: bool,
    pub extra_land_plays: u16,
    pub all_off_bf_colorless: bool,
    pub damage_cant_be_prevented: bool,
    pub cant_gain_life: bool,
    pub skip_draw_step: bool,
    pub cant_cast_during_combat: bool,
    pub cant_play_lands: bool,
    pub cant_draw_cards: bool,
    pub cant_draw_extra_cards: bool,
    pub legend_rule_exempt_all: bool,
    pub flagbearer: bool,
    pub graveyard_cant_be_targeted: bool,
    pub damage_to_you_as_infect: bool,
    pub all_damage_has_wither: bool,
    pub skip_extra_turns: bool,
    pub cant_get_any_counters: bool,
    pub immune_to_opp_forced_sacrifice: bool,
    pub immune_to_opp_forced_discard: bool,
    pub protection_from_card_types: CardTypeMask,
    pub protection_from_everything: bool,
    pub ignore_hexproof: bool,
    pub suppress_ward_triggers: bool,
    pub active_rule_mods: Vec<ActiveRuleModEntry>,
}

impl EvaluatedPlayerState {
    pub fn effective_max_hand_size(&self, base: u16) -> Option<u32> {
        if self.no_max_hand_size {
            return None;
        }
        let base_val = i32::from(self.max_hand_size_set.unwrap_or(base));
        Some((base_val + self.max_hand_size_delta).max(0) as u32)
    }
}

bitflags! {
    /// Subtype domain classification used for `CR 205.1a` correlated subtype stripping & `CR 305.7`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct SubtypeDomain: u8 {
        const BASIC_LAND   = 1 << 0;
        const LAND         = 1 << 1;
        const ARTIFACT     = 1 << 2;
        const ENCHANTMENT  = 1 << 3;
        const SPELL        = 1 << 4;
        const PLANESWALKER = 1 << 5;
        const BATTLE       = 1 << 6;
    }
}

/// Pre-indexed subtype domain table built once per evaluation from `CardDatabase::tables.subtypes`.
pub struct SubtypeTable {
    pub domains: Vec<SubtypeDomain>,
    pub basic_lands: [(Option<SubtypeId>, ManaColor); 5],
    pub reconfigure_kw: Option<KeywordId>,
    pub impending_kw: Option<KeywordId>,
    pub changeling_kw: Option<KeywordId>,
    pub ward_kw: Option<KeywordId>,
    pub devoid_kw: Option<KeywordId>,
    pub menace_kw: Option<KeywordId>,
    pub haste_kw: Option<KeywordId>,
    pub spacecraft_sid: Option<SubtypeId>,
    pub time_counter: Option<CounterId>,
}

const SUBTYPE_DOMAINS: &[(&str, SubtypeDomain)] = &[
    (
        "Plains",
        SubtypeDomain::BASIC_LAND.union(SubtypeDomain::LAND),
    ),
    (
        "Island",
        SubtypeDomain::BASIC_LAND.union(SubtypeDomain::LAND),
    ),
    (
        "Swamp",
        SubtypeDomain::BASIC_LAND.union(SubtypeDomain::LAND),
    ),
    (
        "Mountain",
        SubtypeDomain::BASIC_LAND.union(SubtypeDomain::LAND),
    ),
    (
        "Forest",
        SubtypeDomain::BASIC_LAND.union(SubtypeDomain::LAND),
    ),
    ("Gate", SubtypeDomain::LAND),
    ("Locus", SubtypeDomain::LAND),
    ("Cave", SubtypeDomain::LAND),
    ("Desert", SubtypeDomain::LAND),
    ("Lair", SubtypeDomain::LAND),
    ("Sphere", SubtypeDomain::LAND),
    ("Tower", SubtypeDomain::LAND),
    ("Mine", SubtypeDomain::LAND),
    ("Power-Plant", SubtypeDomain::LAND),
    ("Town", SubtypeDomain::LAND),
    ("Urza's", SubtypeDomain::LAND),
    ("Plan", SubtypeDomain::LAND),
    ("Planet", SubtypeDomain::LAND),
    ("Equipment", SubtypeDomain::ARTIFACT),
    ("Vehicle", SubtypeDomain::ARTIFACT),
    ("Fortification", SubtypeDomain::ARTIFACT),
    ("Spacecraft", SubtypeDomain::ARTIFACT),
    ("Treasure", SubtypeDomain::ARTIFACT),
    ("Clue", SubtypeDomain::ARTIFACT),
    ("Food", SubtypeDomain::ARTIFACT),
    ("Blood", SubtypeDomain::ARTIFACT),
    ("Map", SubtypeDomain::ARTIFACT),
    ("Powerstone", SubtypeDomain::ARTIFACT),
    ("Gold", SubtypeDomain::ARTIFACT),
    ("Shard", SubtypeDomain::ARTIFACT),
    ("Lander", SubtypeDomain::ARTIFACT),
    ("Mutagen", SubtypeDomain::ARTIFACT),
    ("Heartwood", SubtypeDomain::ARTIFACT),
    ("Junk", SubtypeDomain::ARTIFACT),
    ("Vibranium", SubtypeDomain::ARTIFACT),
    ("Bobblehead", SubtypeDomain::ARTIFACT),
    ("Book", SubtypeDomain::ARTIFACT),
    ("Communicator", SubtypeDomain::ARTIFACT),
    ("Incubator", SubtypeDomain::ARTIFACT),
    ("Infinity", SubtypeDomain::ARTIFACT),
    ("Stone", SubtypeDomain::ARTIFACT),
    ("Contraption", SubtypeDomain::ARTIFACT),
    ("Attraction", SubtypeDomain::ARTIFACT),
    ("Aura", SubtypeDomain::ENCHANTMENT),
    ("Saga", SubtypeDomain::ENCHANTMENT),
    ("Class", SubtypeDomain::ENCHANTMENT),
    ("Case", SubtypeDomain::ENCHANTMENT),
    ("Room", SubtypeDomain::ENCHANTMENT),
    ("Background", SubtypeDomain::ENCHANTMENT),
    ("Shrine", SubtypeDomain::ENCHANTMENT),
    ("Role", SubtypeDomain::ENCHANTMENT),
    ("Cartouche", SubtypeDomain::ENCHANTMENT),
    ("Curse", SubtypeDomain::ENCHANTMENT),
    ("Rune", SubtypeDomain::ENCHANTMENT),
    ("Arcane", SubtypeDomain::SPELL),
    ("Adventure", SubtypeDomain::SPELL),
    ("Lesson", SubtypeDomain::SPELL),
    ("Omen", SubtypeDomain::SPELL),
    ("Trap", SubtypeDomain::SPELL),
    ("Chorus", SubtypeDomain::SPELL),
    ("Siege", SubtypeDomain::BATTLE),
    // CR 205.3j Planeswalker subtypes
    ("Abian", SubtypeDomain::PLANESWALKER),
    ("Ajani", SubtypeDomain::PLANESWALKER),
    ("Aminatou", SubtypeDomain::PLANESWALKER),
    ("Angrath", SubtypeDomain::PLANESWALKER),
    ("Arlinn", SubtypeDomain::PLANESWALKER),
    ("Arzakon", SubtypeDomain::PLANESWALKER),
    ("Ashiok", SubtypeDomain::PLANESWALKER),
    ("B.O.B.", SubtypeDomain::PLANESWALKER),
    ("Bahamut", SubtypeDomain::PLANESWALKER),
    ("Basri", SubtypeDomain::PLANESWALKER),
    ("Bolas", SubtypeDomain::PLANESWALKER),
    ("Calix", SubtypeDomain::PLANESWALKER),
    ("Chandra", SubtypeDomain::PLANESWALKER),
    ("Comet", SubtypeDomain::PLANESWALKER),
    ("Dack", SubtypeDomain::PLANESWALKER),
    ("Dakkon", SubtypeDomain::PLANESWALKER),
    ("Daretti", SubtypeDomain::PLANESWALKER),
    ("Davriel", SubtypeDomain::PLANESWALKER),
    ("Deb", SubtypeDomain::PLANESWALKER),
    ("Dellian", SubtypeDomain::PLANESWALKER),
    ("Dihada", SubtypeDomain::PLANESWALKER),
    ("Domri", SubtypeDomain::PLANESWALKER),
    ("Dovin", SubtypeDomain::PLANESWALKER),
    ("Duck", SubtypeDomain::PLANESWALKER),
    ("Dungeon", SubtypeDomain::PLANESWALKER),
    ("Dyfed", SubtypeDomain::PLANESWALKER),
    ("Ellywick", SubtypeDomain::PLANESWALKER),
    ("Elminster", SubtypeDomain::PLANESWALKER),
    ("Elspeth", SubtypeDomain::PLANESWALKER),
    ("Ersta", SubtypeDomain::PLANESWALKER),
    ("Estrid", SubtypeDomain::PLANESWALKER),
    ("Feroz", SubtypeDomain::PLANESWALKER),
    ("Freyalise", SubtypeDomain::PLANESWALKER),
    ("Garruk", SubtypeDomain::PLANESWALKER),
    ("Gideon", SubtypeDomain::PLANESWALKER),
    ("Greensleeves", SubtypeDomain::PLANESWALKER),
    ("Grist", SubtypeDomain::PLANESWALKER),
    ("Guff", SubtypeDomain::PLANESWALKER),
    ("Huatli", SubtypeDomain::PLANESWALKER),
    ("Inzerva", SubtypeDomain::PLANESWALKER),
    ("Jace", SubtypeDomain::PLANESWALKER),
    ("Jared", SubtypeDomain::PLANESWALKER),
    ("Jaya", SubtypeDomain::PLANESWALKER),
    ("Jeska", SubtypeDomain::PLANESWALKER),
    ("Kaito", SubtypeDomain::PLANESWALKER),
    ("Karn", SubtypeDomain::PLANESWALKER),
    ("Kasmina", SubtypeDomain::PLANESWALKER),
    ("Kaya", SubtypeDomain::PLANESWALKER),
    ("Kiora", SubtypeDomain::PLANESWALKER),
    ("Koth", SubtypeDomain::PLANESWALKER),
    ("Liliana", SubtypeDomain::PLANESWALKER),
    ("Lolth", SubtypeDomain::PLANESWALKER),
    ("Lukka", SubtypeDomain::PLANESWALKER),
    ("Luxior", SubtypeDomain::PLANESWALKER),
    ("Master", SubtypeDomain::PLANESWALKER),
    ("Minsc", SubtypeDomain::PLANESWALKER),
    ("Monopoly", SubtypeDomain::PLANESWALKER),
    ("Mordenkainen", SubtypeDomain::PLANESWALKER),
    ("Nahiri", SubtypeDomain::PLANESWALKER),
    ("Narset", SubtypeDomain::PLANESWALKER),
    ("Niko", SubtypeDomain::PLANESWALKER),
    ("Nissa", SubtypeDomain::PLANESWALKER),
    ("Nixilis", SubtypeDomain::PLANESWALKER),
    ("Oko", SubtypeDomain::PLANESWALKER),
    ("Quintorius", SubtypeDomain::PLANESWALKER),
    ("Ral", SubtypeDomain::PLANESWALKER),
    ("Rowan", SubtypeDomain::PLANESWALKER),
    ("Saheeli", SubtypeDomain::PLANESWALKER),
    ("Samut", SubtypeDomain::PLANESWALKER),
    ("Sarkhan", SubtypeDomain::PLANESWALKER),
    ("Serra", SubtypeDomain::PLANESWALKER),
    ("Sifa", SubtypeDomain::PLANESWALKER),
    ("Sivitri", SubtypeDomain::PLANESWALKER),
    ("Sorin", SubtypeDomain::PLANESWALKER),
    ("Svega", SubtypeDomain::PLANESWALKER),
    ("Szat", SubtypeDomain::PLANESWALKER),
    ("Tamiyo", SubtypeDomain::PLANESWALKER),
    ("Tasha", SubtypeDomain::PLANESWALKER),
    ("Teferi", SubtypeDomain::PLANESWALKER),
    ("Teyo", SubtypeDomain::PLANESWALKER),
    ("Tezzeret", SubtypeDomain::PLANESWALKER),
    ("Thomil", SubtypeDomain::PLANESWALKER),
    ("Tibalt", SubtypeDomain::PLANESWALKER),
    ("Tyvar", SubtypeDomain::PLANESWALKER),
    ("Ugin", SubtypeDomain::PLANESWALKER),
    ("Urza", SubtypeDomain::PLANESWALKER),
    ("Venser", SubtypeDomain::PLANESWALKER),
    ("Vivien", SubtypeDomain::PLANESWALKER),
    ("Vraska", SubtypeDomain::PLANESWALKER),
    ("Vronos", SubtypeDomain::PLANESWALKER),
    ("Wanderer", SubtypeDomain::PLANESWALKER),
    ("Will", SubtypeDomain::PLANESWALKER),
    ("Windgrace", SubtypeDomain::PLANESWALKER),
    ("Worzel", SubtypeDomain::PLANESWALKER),
    ("Wrenn", SubtypeDomain::PLANESWALKER),
    ("Xenagos", SubtypeDomain::PLANESWALKER),
    ("Yanggu", SubtypeDomain::PLANESWALKER),
    ("Yanling", SubtypeDomain::PLANESWALKER),
    ("You", SubtypeDomain::PLANESWALKER),
    ("Zariel", SubtypeDomain::PLANESWALKER),
];

impl SubtypeTable {
    pub fn from_db(db: &CardDatabase) -> Self {
        let mut domains = vec![SubtypeDomain::empty(); db.tables.subtypes.len()];
        for &(name, dom) in SUBTYPE_DOMAINS {
            if let Some(id) = db.subtype_id(name) {
                domains[usize::from(id.0)] = dom;
            }
        }
        let basic_lands = [
            (db.subtype_id("Plains"), ManaColor::W),
            (db.subtype_id("Island"), ManaColor::U),
            (db.subtype_id("Swamp"), ManaColor::B),
            (db.subtype_id("Mountain"), ManaColor::R),
            (db.subtype_id("Forest"), ManaColor::G),
        ];
        Self {
            domains,
            basic_lands,
            reconfigure_kw: db.keyword_id("reconfigure"),
            impending_kw: db.keyword_id("impending"),
            changeling_kw: db.keyword_id("changeling"),
            ward_kw: db.keyword_id("ward"),
            devoid_kw: db.keyword_id("devoid"),
            menace_kw: db.keyword_id("menace"),
            haste_kw: db.keyword_id("haste"),
            spacecraft_sid: db.subtype_id("Spacecraft"),
            time_counter: db.counter_id("time"),
        }
    }

    #[inline]
    pub fn domain_of(&self, id: SubtypeId) -> SubtypeDomain {
        self.domains
            .get(usize::from(id.0))
            .copied()
            .unwrap_or_default()
    }

    #[inline]
    pub fn basic_land_color(&self, id: SubtypeId) -> Option<ManaColor> {
        for &(sid, color) in &self.basic_lands {
            if sid == Some(id) {
                return Some(color);
            }
        }
        None
    }

    #[inline]
    pub fn is_non_creature_subtype(&self, id: SubtypeId) -> bool {
        !self.domain_of(id).is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct WorkAbilityItem {
    pub id: u32,
    pub is_innate: bool,
    pub is_intrinsic_basic_mana: bool,
    pub required_basic_subtype: Option<SubtypeId>,
    pub is_mana_ability: bool,
    pub keyword: Option<KeywordId>,
    pub keyword_param: Option<String>,
    pub granted_ability: Option<GrantedAbilityEntry>,
    pub timestamp: u64,
    pub removed: bool,
}

#[derive(Debug, Clone)]
pub struct WorkStatic<'a> {
    pub static_idx: usize,
    pub provenance: StaticProvenance,
    pub static_def: &'a StaticDef,
    pub span: Span,
    pub timestamp: u64,
    pub started_in_earlier_layer: bool,
    pub granted_item_id: Option<u32>,
    pub applied_targets: SmallVec<[ObjectId; 4]>,
}

#[derive(Debug, Clone)]
pub struct PermWorkState<'a> {
    pub id: ObjectId,
    pub owner: PlayerId,
    pub timestamp: u64,
    pub board_order: usize,
    pub base_name: String,
    pub is_face_down: bool,
    pub is_pt_cda: bool,
    pub has_copy_pt_power_except: bool,
    pub has_copy_pt_toughness_except: bool,
    pub lost_innate_statics: bool,
    pub eval: EvaluatedObject,
    pub extras: EvaluatedObjectExtras,
    pub items: SmallVec<[WorkAbilityItem; 8]>,
    pub statics: SmallVec<[WorkStatic<'a>; 4]>,
    pub notes: Vec<String>,
    pub next_item_id: u32,
}

impl<'a> PermWorkState<'a> {
    pub fn alloc_item_id(&mut self) -> u32 {
        let id = self.next_item_id;
        self.next_item_id += 1;
        id
    }

    pub fn push_note(&mut self, collect_notes: bool, note: impl FnOnce() -> String) {
        if collect_notes {
            self.notes.push(note());
        }
    }

    pub fn has_active_keyword(&self, kw: KeywordId) -> bool {
        if self.extras.cant_have_keywords.contains(&kw) {
            return false;
        }
        self.items
            .iter()
            .any(|item| !item.removed && item.keyword == Some(kw))
    }

    /// Rebuilds `eval.keywords`, `extras.parameterized_keywords`, `eval.lost_all_abilities`,
    /// and `extras.granted_abilities` from `items` and `cant_have_keywords` (`CR 113.11`).
    pub fn sync_abilities(&mut self) {
        self.eval.keywords.clear();
        self.extras.parameterized_keywords.clear();
        self.extras.granted_abilities.clear();
        for item in &self.items {
            if item.removed {
                continue;
            }
            if let Some(kw) = item.keyword
                && !self.extras.cant_have_keywords.contains(&kw)
            {
                if let Some(param) = &item.keyword_param {
                    if !self
                        .extras
                        .parameterized_keywords
                        .iter()
                        .any(|(k, p)| *k == kw && p == param)
                    {
                        self.extras.parameterized_keywords.push((kw, param.clone()));
                    }
                } else if !self.eval.keywords.contains(&kw) {
                    self.eval.keywords.push(kw);
                }
            }
            if let Some(entry) = &item.granted_ability {
                self.extras.granted_abilities.push(entry.clone());
            }
        }
        self.eval.lost_all_abilities = self.extras.innate_abilities_removed;
    }

    /// Applies `CR 205.1a`, `CR 205.4b`, and `CR 305.7` when setting types and/or subtypes in Layer 4.
    pub fn apply_set_types_or_subtypes(
        &mut self,
        subtype_table: &SubtypeTable,
        new_types: Option<CardTypeMask>,
        new_subtypes: Option<&[SubtypeId]>,
        timestamp: u64,
        source_label: Option<&str>,
        collect_notes: bool,
    ) {
        if let Some(t) = new_types {
            // CR 205.1a: Instant & Sorcery types persist; supertypes are untouched (CR 205.4b).
            let retained_spell_types =
                self.eval.types & (CardTypeMask::INSTANT | CardTypeMask::SORCERY);
            self.eval.types = retained_spell_types | t;
            if let Some(sts) = new_subtypes {
                self.eval.subtypes = sts.iter().copied().collect();
                self.eval.has_all_creature_types = false;
            } else {
                self.strip_orphaned_subtypes(subtype_table);
            }
        } else if let Some(sts) = new_subtypes {
            // Setting subtypes without overwriting card types: replace subtypes only within
            // the domain(s) being set, preserving subtypes of other domains (e.g. Land Creature).
            let sets_creature_domain = sts.iter().any(|&s| subtype_table.domain_of(s).is_empty());
            let sets_non_creature_domains = sts.iter().fold(SubtypeDomain::empty(), |acc, &s| {
                acc | subtype_table.domain_of(s)
            });
            self.eval.subtypes.retain(|s| {
                let dom = subtype_table.domain_of(*s);
                if dom.is_empty() {
                    !sets_creature_domain && !sts.is_empty()
                } else {
                    !dom.intersects(sets_non_creature_domains)
                }
            });
            for &s in sts {
                if !self.eval.subtypes.contains(&s) {
                    self.eval.subtypes.push(s);
                }
            }
            if sets_creature_domain || sts.is_empty() {
                self.eval.has_all_creature_types = false;
            }
        }

        self.strip_orphaned_subtypes(subtype_table);

        // CR 305.7: Remove any previously granted Layer 4 intrinsic basic-land mana items
        // whose basic land subtype is no longer present.
        for item in &mut self.items {
            if item.is_intrinsic_basic_mana
                && let Some(req) = item.required_basic_subtype
                && !self.eval.subtypes.contains(&req)
            {
                item.removed = true;
            }
        }

        // CR 305.7: If the object is a Land and now has a basic land subtype set via SetTypes/SetSubtypes,
        // it loses all innate rules-text abilities and gains the intrinsic basic land mana abilities.
        if self.eval.types.contains(CardTypeMask::LAND) {
            let mut added_basic: SmallVec<[(SubtypeId, ManaColor); 5]> = SmallVec::new();
            for &sid in &self.eval.subtypes {
                if let Some(color) = subtype_table.basic_land_color(sid)
                    && !added_basic.iter().any(|(s, _)| *s == sid)
                {
                    added_basic.push((sid, color));
                }
            }
            if !added_basic.is_empty() {
                self.lost_innate_statics = true;
                self.extras.innate_abilities_removed = true;
                self.extras.innate_mana_abilities_removed = true;
                self.extras
                    .removed_ability_timestamps
                    .push((timestamp, false));
                for item in &mut self.items {
                    if item.is_innate {
                        item.removed = true;
                    }
                }
                for (subtype, color) in added_basic {
                    let already_present = self.items.iter().any(|it| {
                        !it.removed
                            && it.is_intrinsic_basic_mana
                            && it.required_basic_subtype == Some(subtype)
                    });
                    if !already_present {
                        let id = self.alloc_item_id();
                        self.items.push(WorkAbilityItem {
                            id,
                            is_innate: false,
                            is_intrinsic_basic_mana: true,
                            required_basic_subtype: Some(subtype),
                            is_mana_ability: true,
                            keyword: None,
                            keyword_param: None,
                            granted_ability: Some(GrantedAbilityEntry::IntrinsicBasicMana {
                                color,
                                subtype,
                            }),
                            timestamp,
                            removed: false,
                        });
                    }
                }
            }
        }

        self.sync_abilities();
        if let Some(lbl) = source_label {
            self.push_note(collect_notes, || {
                format!("Layer 4: {lbl} set types/subtypes")
            });
        }
    }

    /// Applies `LayerMod::AddTypes` (`"in addition to its other types"`), adding basic land mana
    /// abilities without stripping existing abilities (`CR 305.7` final sentence).
    pub fn apply_add_types(
        &mut self,
        subtype_table: &SubtypeTable,
        add_types: CardTypeMask,
        add_subtypes: &[SubtypeId],
        timestamp: u64,
        source_label: Option<&str>,
        collect_notes: bool,
    ) {
        self.eval.types |= add_types;
        for &sid in add_subtypes {
            if !self.eval.subtypes.contains(&sid) {
                self.eval.subtypes.push(sid);
            }
        }
        if self.eval.types.contains(CardTypeMask::LAND) {
            for &sid in add_subtypes {
                if let Some(color) = subtype_table.basic_land_color(sid) {
                    let already_present = self.items.iter().any(|it| {
                        !it.removed
                            && it.is_intrinsic_basic_mana
                            && it.required_basic_subtype == Some(sid)
                    });
                    if !already_present {
                        let id = self.alloc_item_id();
                        self.items.push(WorkAbilityItem {
                            id,
                            is_innate: false,
                            is_intrinsic_basic_mana: true,
                            required_basic_subtype: Some(sid),
                            is_mana_ability: true,
                            keyword: None,
                            keyword_param: None,
                            granted_ability: Some(GrantedAbilityEntry::IntrinsicBasicMana {
                                color,
                                subtype: sid,
                            }),
                            timestamp,
                            removed: false,
                        });
                    }
                }
            }
        }
        self.sync_abilities();
        if let Some(lbl) = source_label {
            self.push_note(collect_notes, || {
                format!("Layer 4: {lbl} added types/subtypes")
            });
        }
    }

    /// Applies `LayerMod::RemoveTypes` and enforces `CR 205.1a` correlated subtype stripping.
    pub fn apply_remove_types(
        &mut self,
        subtype_table: &SubtypeTable,
        remove_types: CardTypeMask,
        source_label: Option<&str>,
        collect_notes: bool,
    ) {
        self.eval.types.remove(remove_types);
        self.strip_orphaned_subtypes(subtype_table);
        for item in &mut self.items {
            if item.is_intrinsic_basic_mana
                && let Some(req) = item.required_basic_subtype
                && (!self.eval.types.contains(CardTypeMask::LAND)
                    || !self.eval.subtypes.contains(&req))
            {
                item.removed = true;
            }
        }
        self.sync_abilities();
        if let Some(lbl) = source_label {
            self.push_note(collect_notes, || format!("Layer 4: {lbl} removed types"));
        }
    }

    /// Strips subtypes whose correlated card type is no longer present (`CR 205.1a`).
    pub fn strip_orphaned_subtypes(&mut self, subtype_table: &SubtypeTable) {
        let t = self.eval.types;
        let has_creature_or_kindred = t.intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED);
        if !has_creature_or_kindred {
            self.eval.has_all_creature_types = false;
        }
        self.eval.subtypes.retain(|sid| {
            let dom = subtype_table.domain_of(*sid);
            if dom.is_empty() {
                return has_creature_or_kindred;
            }
            if dom.intersects(SubtypeDomain::LAND) && t.contains(CardTypeMask::LAND) {
                return true;
            }
            if dom.intersects(SubtypeDomain::ARTIFACT) && t.contains(CardTypeMask::ARTIFACT) {
                return true;
            }
            if dom.intersects(SubtypeDomain::ENCHANTMENT) && t.contains(CardTypeMask::ENCHANTMENT) {
                return true;
            }
            if dom.intersects(SubtypeDomain::SPELL)
                && t.intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
            {
                return true;
            }
            if dom.intersects(SubtypeDomain::PLANESWALKER) && t.contains(CardTypeMask::PLANESWALKER)
            {
                return true;
            }
            if dom.intersects(SubtypeDomain::BATTLE) && t.contains(CardTypeMask::BATTLE) {
                return true;
            }
            false
        });
    }

    pub fn apply_remove_all_abilities(
        &mut self,
        keep_mana_abilities: bool,
        timestamp: u64,
        source_label: Option<&str>,
        collect_notes: bool,
    ) {
        self.extras.innate_abilities_removed = true;
        if !keep_mana_abilities {
            self.extras.innate_mana_abilities_removed = true;
        }
        self.extras
            .removed_ability_timestamps
            .push((timestamp, keep_mana_abilities));
        for item in &mut self.items {
            if item.removed {
                continue;
            }
            if keep_mana_abilities && item.is_mana_ability {
                continue;
            }
            item.removed = true;
        }
        self.sync_abilities();
        if let Some(lbl) = source_label {
            self.push_note(collect_notes, || {
                if keep_mana_abilities {
                    format!("Layer 6: {lbl} removed non-mana abilities")
                } else {
                    format!("Layer 6: {lbl} removed all abilities")
                }
            });
        }
    }
}

/// In-progress board state used during `CR 613` layer evaluation (implements [`LayerBoardView`]).
#[derive(Debug, Clone)]
pub struct WorkBoard<'a> {
    pub perms: Vec<PermWorkState<'a>>,
    pub index_by_obj: Vec<u32>,
    pub reads: Cell<DirtyMask>,
    pub may_have_off_bf_char_grants: bool,
    pub off_bf_grantor_ids: SmallVec<[ObjectId; 4]>,
    pub off_bf_grantors_unbounded: bool,
    pub may_have_all_off_bf_colorless: bool,
}

impl<'a> WorkBoard<'a> {
    pub fn with_capacity(slot_cap: usize, perm_cap: usize) -> Self {
        Self {
            perms: Vec::with_capacity(perm_cap),
            index_by_obj: vec![u32::MAX; slot_cap],
            reads: Cell::new(
                DirtyMask::TOPOLOGY | DirtyMask::COUNTERS | DirtyMask::CONTINUOUS_EFFECTS,
            ),
            may_have_off_bf_char_grants: false,
            off_bf_grantor_ids: SmallVec::new(),
            off_bf_grantors_unbounded: false,
            may_have_all_off_bf_colorless: false,
        }
    }

    #[inline]
    pub fn record_off_bf_grantor(&mut self, id: ObjectId) {
        if !self.off_bf_grantor_ids.contains(&id) {
            self.off_bf_grantor_ids.push(id);
        }
    }

    #[inline]
    pub fn perm_index(&self, id: ObjectId) -> Option<usize> {
        let idx = *self.index_by_obj.get(id.index as usize)?;
        if idx == u32::MAX {
            return None;
        }
        let idx = idx as usize;
        let p = self.perms.get(idx)?;
        (p.id == id).then_some(idx)
    }

    #[inline]
    pub fn perm(&self, id: ObjectId) -> Option<&PermWorkState<'a>> {
        let idx = self.perm_index(id)?;
        self.perms.get(idx)
    }

    #[inline]
    pub fn perm_mut(&mut self, id: ObjectId) -> Option<&mut PermWorkState<'a>> {
        let idx = self.perm_index(id)?;
        self.perms.get_mut(idx)
    }
}

impl<'a> LayerBoardView for WorkBoard<'a> {
    #[inline]
    fn evaluated(&self, id: ObjectId) -> Option<&EvaluatedObject> {
        self.perm(id).map(|p| &p.eval)
    }

    #[inline]
    fn effective_controller(&self, id: ObjectId) -> Option<PlayerId> {
        self.perm(id).map(|p| p.eval.controller)
    }

    #[inline]
    fn effective_name(&self, id: ObjectId) -> Option<&str> {
        let p = self.perm(id)?;
        if p.is_face_down {
            Some("")
        } else {
            Some(p.eval.name_override.as_deref().unwrap_or(&p.base_name))
        }
    }

    #[inline]
    fn record_read(&self, mask: DirtyMask) {
        self.reads.set(self.reads.get() | mask);
    }

    #[inline]
    fn may_have_off_bf_char_grants(&self) -> bool {
        self.may_have_off_bf_char_grants
    }

    #[inline]
    fn off_bf_grantor_ids(&self) -> Option<&[ObjectId]> {
        if self.off_bf_grantors_unbounded {
            None
        } else {
            Some(&self.off_bf_grantor_ids)
        }
    }

    #[inline]
    fn may_have_all_off_bf_colorless(&self) -> bool {
        self.may_have_all_off_bf_colorless
    }
}

/// Conservative `FilterFlags` (all 53 bits `0..=52`) -> `DirtyMask` mapping.
pub fn dirty_mask_for_filter_flags(flags: FilterFlags) -> DirtyMask {
    let mut out = DirtyMask::empty();
    if flags.intersects(FilterFlags::TAPPED | FilterFlags::UNTAPPED) {
        out |= DirtyMask::TAPPED;
    }
    if flags.intersects(
        FilterFlags::ATTACKING
            | FilterFlags::NOT_ATTACKING
            | FilterFlags::BLOCKING
            | FilterFlags::NOT_BLOCKING
            | FilterFlags::ATTACKING_OR_BLOCKING
            | FilterFlags::UNBLOCKED_ATTACKER,
    ) {
        out |= DirtyMask::COMBAT;
    }
    if flags.intersects(
        FilterFlags::MODIFIED
            | FilterFlags::NOT_MODIFIED
            | FilterFlags::HAS_ANY_COUNTER
            | FilterFlags::HAS_NO_COUNTER,
    ) {
        out |= DirtyMask::COUNTERS | DirtyMask::TOPOLOGY;
    }
    if flags.intersects(FilterFlags::ENTERED_THIS_TURN | FilterFlags::NOT_ENTERED_THIS_TURN) {
        out |= DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY;
    }
    if flags.contains(FilterFlags::DEALT_DAMAGE_THIS_TURN) {
        out |= DirtyMask::COMBAT | DirtyMask::TURN_HISTORY;
    }
    if flags.intersects(FilterFlags::SADDLED | FilterFlags::GOADED) {
        out |= DirtyMask::DESIGNATIONS | DirtyMask::TURN_HISTORY;
    }
    if flags.intersects(
        FilterFlags::MONSTROUS
            | FilterFlags::NOT_MONSTROUS
            | FilterFlags::SUSPECTED
            | FilterFlags::NOT_SUSPECTED
            | FilterFlags::PREPARED
            | FilterFlags::NOT_PREPARED
            | FilterFlags::RENOWNED
            | FilterFlags::NOT_RENOWNED,
    ) {
        out |= DirtyMask::DESIGNATIONS;
    }
    if flags.intersects(
        FilterFlags::KICKED_SPELL
            | FilterFlags::BARGAINED_SPELL
            | FilterFlags::WAS_NOT_CAST
            | FilterFlags::CAST_OUTSIDE_HAND,
    ) {
        out |= DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY;
    }
    if flags.intersects(
        FilterFlags::OTHER
            | FilterFlags::TOKEN
            | FilterFlags::NONTOKEN
            | FilterFlags::IS_COMMANDER
            | FilterFlags::NOT_COMMANDER
            | FilterFlags::MULTICOLORED
            | FilterFlags::NOT_MULTICOLORED
            | FilterFlags::MONOCOLORED
            | FilterFlags::NOT_MONOCOLORED
            | FilterFlags::IS_COLORED
            | FilterFlags::IS_COLORLESS
            | FilterFlags::FACE_DOWN
            | FilterFlags::FACE_UP
            | FilterFlags::HISTORIC
            | FilterFlags::OUTLAW
            | FilterFlags::EQUIPPED
            | FilterFlags::NOT_EQUIPPED
            | FilterFlags::ENCHANTED
            | FilterFlags::NOT_ENCHANTED
            | FilterFlags::ENCHANTED_OR_EQUIPPED
            | FilterFlags::HAS_ADVENTURE
            | FilterFlags::HAS_X_IN_MANA_COST
            | FilterFlags::WITH_CYCLING_ABILITY
            | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN,
    ) {
        out |= DirtyMask::TOPOLOGY;
    }
    out
}

pub fn dirty_mask_for_filter(filter: &ObjectFilter) -> DirtyMask {
    let mut out = DirtyMask::TOPOLOGY | dirty_mask_for_filter_flags(filter.flags);
    if filter.zones.contains(ZoneMask::GRAVEYARD) {
        out |= DirtyMask::GRAVEYARDS;
    }
    if filter.zones.contains(ZoneMask::EXILE) {
        out |= DirtyMask::EXILE;
    }
    if filter
        .zones
        .intersects(ZoneMask::HAND | ZoneMask::LIBRARY | ZoneMask::COMMAND | ZoneMask::STACK)
    {
        out |= DirtyMask::PLAYER_STATE;
    }
    if let Some(cmp) = &filter.mana_value {
        out |= dirty_mask_for_amount(&cmp.value);
    }
    if let Some(cmp) = &filter.power {
        out |= dirty_mask_for_amount(&cmp.value);
    }
    if let Some(cmp) = &filter.toughness {
        out |= dirty_mask_for_amount(&cmp.value);
    }
    for pred in &filter.extra {
        out |= dirty_mask_for_predicate(pred);
    }
    out
}

fn dirty_mask_for_predicate(pred: &ObjectPredicate) -> DirtyMask {
    match pred {
        ObjectPredicate::NotSelf
        | ObjectPredicate::PlayerOnly
        | ObjectPredicate::Named { .. }
        | ObjectPredicate::ControllerRef { .. }
        | ObjectPredicate::OwnerRef { .. }
        | ObjectPredicate::ControllerAndNotOwner { .. }
        | ObjectPredicate::SameOwnerAsSource
        | ObjectPredicate::ControllerOfTarget { .. }
        | ObjectPredicate::ControllerOfEventObject
        | ObjectPredicate::DifferentControllers
        | ObjectPredicate::HasKeywords { .. }
        | ObjectPredicate::LacksKeywords { .. }
        | ObjectPredicate::CreatedWithSelf
        | ObjectPredicate::AttachedToRef { .. }
        | ObjectPredicate::HasAnotherAuraAttached
        | ObjectPredicate::SameNameAsSource
        | ObjectPredicate::NotSameNameAsSource
        | ObjectPredicate::SameNameAsAffected
        | ObjectPredicate::SameNameAsNontokenPermanent
        | ObjectPredicate::SameNameAsTarget { .. }
        | ObjectPredicate::SharesTypeWithCommander
        | ObjectPredicate::SharesColorWithSelf
        | ObjectPredicate::SharesColorWithSpell
        | ObjectPredicate::SharesCreatureTypeWithSelf
        | ObjectPredicate::SharesCardTypeWithBinding { .. }
        | ObjectPredicate::SharesSubtypeWithSacrificed
        | ObjectPredicate::TargetsSelf
        | ObjectPredicate::TargetsAttached
        | ObjectPredicate::TargetsPlayer { .. }
        | ObjectPredicate::WithSingleTarget
        | ObjectPredicate::SingleTargetPlayer { .. }
        | ObjectPredicate::CastFromZones { .. }
        | ObjectPredicate::ManaValueEqualsSourcePower
        | ObjectPredicate::ManaValueLteEventObject
        | ObjectPredicate::ManaValueLtRevealedCostCard
        | ObjectPredicate::ManaValueLteEventAmount
        | ObjectPredicate::PowerGreaterThanBase
        | ObjectPredicate::PowerEqualsToughness
        | ObjectPredicate::ToughnessGreaterThanPower
        | ObjectPredicate::PowerLessThanSource
        | ObjectPredicate::PowerLtSelf
        | ObjectPredicate::PowerLteRevealedCostCard
        | ObjectPredicate::GreatestStatAmongControlled { .. }
        | ObjectPredicate::LeastStatAmongControlled { .. }
        | ObjectPredicate::GreatestPowerAmongAll => DirtyMask::TOPOLOGY,

        ObjectPredicate::ControllerDealtCombatDamageBySelfThisTurn
        | ObjectPredicate::DealtDamageByAttachedThisTurn => {
            DirtyMask::COMBAT | DirtyMask::TURN_HISTORY
        }
        ObjectPredicate::UntappedNonattackingSummoningReady => {
            DirtyMask::TAPPED | DirtyMask::COMBAT | DirtyMask::TURN_HISTORY
        }
        ObjectPredicate::HasCounter { .. }
        | ObjectPredicate::LacksCounter { .. }
        | ObjectPredicate::ManaValueLteTotalCounters => DirtyMask::COUNTERS,

        ObjectPredicate::ExiledWithSelf
        | ObjectPredicate::ExiledByAnyPlayer
        | ObjectPredicate::AssimilatedWithSelf
        | ObjectPredicate::SharesCardTypeWithExiled => DirtyMask::EXILE,

        ObjectPredicate::SameNameAsLinkedExile => DirtyMask::EXILE | DirtyMask::DESIGNATIONS,

        ObjectPredicate::ControlledByEnchantedPlayer
        | ObjectPredicate::ControlledByChosenPlayer
        | ObjectPredicate::PairedWithSource
        | ObjectPredicate::ChosenObject
        | ObjectPredicate::OfChosenType
        | ObjectPredicate::NotOfChosenType
        | ObjectPredicate::OfChosenColor
        | ObjectPredicate::OfChosenName
        | ObjectPredicate::ManaValueEqualsChosenNumber
        | ObjectPredicate::NotYetChosenInStep => DirtyMask::DESIGNATIONS,

        ObjectPredicate::ManaValueParity { parity } => match parity {
            mtg_ir::ParityFilter::Odd | mtg_ir::ParityFilter::Even => DirtyMask::TOPOLOGY,
            mtg_ir::ParityFilter::ChosenQuality
            | mtg_ir::ParityFilter::NotChosenQuality
            | mtg_ir::ParityFilter::ChosenParity => DirtyMask::DESIGNATIONS,
        },

        ObjectPredicate::TargetsFilter { filter }
        | ObjectPredicate::TargetsYouOrFilter { filter }
        | ObjectPredicate::SingleTargetFilter { filter }
        | ObjectPredicate::AbilitySourceFilter { filter } => dirty_mask_for_filter(filter),

        ObjectPredicate::PowerOrToughness { cmp }
        | ObjectPredicate::BasePowerOrToughness { cmp } => dirty_mask_for_amount(&cmp.value),

        ObjectPredicate::EnteredGraveyardFromBattlefieldThisTurn
        | ObjectPredicate::PutIntoGraveyardFromNonBattlefieldThisTurn => {
            DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY
        }

        ObjectPredicate::AnyOf { filters } => {
            let mut acc = DirtyMask::empty();
            for sub in filters {
                acc |= dirty_mask_for_filter(sub);
            }
            acc
        }
    }
}

pub fn dirty_mask_for_amount(amt: &Amount) -> DirtyMask {
    match amt {
        Amount::Const { .. }
        | Amount::X
        | Amount::AnyNumber
        | Amount::ResultCount { .. }
        | Amount::EventAmount
        | Amount::TappedCostPower
        | Amount::ColorsOfManaSpentToCast
        | Amount::ManaSpentToCast
        | Amount::LastRollResult
        | Amount::CoinHeadsCount
        | Amount::DevouredCount
        | Amount::ExcessDamage
        | Amount::LastEffectAmount
        | Amount::TargetsBeyondFirst
        | Amount::TargetsCount
        | Amount::TargetColorsCount
        | Amount::TargetPower
        | Amount::TargetsTotalManaValue
        | Amount::TotalManaValueOfTargetSpec { .. }
        | Amount::CostPaidCount { .. }
        | Amount::SacrificedCostStat { .. }
        | Amount::RevealedCostStat { .. } => DirtyMask::empty(),

        Amount::Negate { of } | Amount::Mul { of, .. } | Amount::Half { of, .. } => {
            dirty_mask_for_amount(of)
        }

        Amount::Sum { args } | Amount::Min { args } | Amount::Max { args } => {
            let mut acc = DirtyMask::empty();
            for a in args {
                acc |= dirty_mask_for_amount(a);
            }
            acc
        }

        Amount::Difference { left, right } => {
            dirty_mask_for_amount(left) | dirty_mask_for_amount(right)
        }

        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            dirty_mask_for_condition(condition)
                | dirty_mask_for_amount(then_amt)
                | dirty_mask_for_amount(else_amt)
        }

        Amount::Count { of }
        | Amount::TotalCountersAmong { filter: of }
        | Amount::DistinctNamesCount { of }
        | Amount::TargetsMatchingCount { filter: of }
        | Amount::OpponentsControllingMoreCount { filter: of }
        | Amount::OpponentsControllingFilterCount { filter: of } => {
            dirty_mask_for_filter(of) | DirtyMask::COUNTERS
        }

        Amount::GreatestStatAmong { filter, .. }
        | Amount::LeastStatAmong { filter, .. }
        | Amount::TotalStatAmong { filter, .. }
        | Amount::GreatestStatAmongControlledAsCast { filter, .. }
        | Amount::GreatestControlledCount { filter } => dirty_mask_for_filter(filter),

        Amount::GreatestBaseToughnessControlled { filter }
        | Amount::DistinctNamesControlled { filter }
        | Amount::ColorsAmongPermanents { filter, .. }
        | Amount::AttachmentsOn { filter, .. } => {
            DirtyMask::TOPOLOGY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Amount::CountersAmong { filter, .. } | Amount::CountersAmongControlled { filter, .. } => {
            DirtyMask::COUNTERS | dirty_mask_for_filter(filter)
        }

        Amount::Stat { .. }
        | Amount::Devotion { .. }
        | Amount::OpponentCount
        | Amount::Domain { .. }
        | Amount::PartyCount { .. }
        | Amount::ColorsOfObject { .. }
        | Amount::DistinctCreatureTypesControlled
        | Amount::DistinctLandNamesControlled
        | Amount::DistinctColorsAmongControlled
        | Amount::DistinctPowersControlled
        | Amount::LandsOpponentControlsProducingColorless
        | Amount::RingBearerPower { .. } => DirtyMask::TOPOLOGY,

        Amount::CardsIn { zone, .. } | Amount::DistinctCardTypesInZone { zone, .. } => {
            zone_dirty_mask(*zone)
        }

        Amount::ChosenPlayerZoneCount { zone, filter } => {
            DirtyMask::DESIGNATIONS
                | zone_dirty_mask(*zone)
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Amount::Life { .. }
        | Amount::StartingLifeDifference
        | Amount::OpponentsWithLifeLessThanHalfStarting
        | Amount::FractionalLifeLoss { .. } => DirtyMask::LIFE,

        Amount::LifeGainedThisTurn
        | Amount::OpponentsLifeLostThisTurn
        | Amount::PlayersLostLifeThisTurnCount => DirtyMask::LIFE | DirtyMask::TURN_HISTORY,

        Amount::CommanderCastCount
        | Amount::EnergyCount { .. }
        | Amount::PlayerCounters { .. }
        | Amount::PlayerCounterDifference { .. }
        | Amount::UnlockedDoorsCount { .. }
        | Amount::DungeonsCompleted { .. }
        | Amount::PlayerSpeed { .. }
        | Amount::CommanderColorIdentityCount
        | Amount::OpponentsWithHandMaxCount { .. }
        | Amount::HandSizeDifference { .. }
        | Amount::UnspentManaTotal { .. } => DirtyMask::PLAYER_STATE,

        Amount::KickedCount { .. } => DirtyMask::TOPOLOGY,

        Amount::CountersOn { .. }
        | Amount::LkiCounterCount { .. }
        | Amount::LkiTotalCounters { .. }
        | Amount::CountersRemovedThisWay
        | Amount::DistinctCounterKindsOn { .. }
        | Amount::CountersAmongPlayersAndPermanents { .. }
        | Amount::TargetCounterCount { .. } => DirtyMask::COUNTERS | DirtyMask::PLAYER_STATE,

        Amount::DistinctManaValuesInGraveyard { filter } => {
            DirtyMask::GRAVEYARDS
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }
        Amount::SameNameInGraveyardAsSpell => DirtyMask::GRAVEYARDS,

        Amount::StormCount
        | Amount::GravestormCount
        | Amount::CountedThisWay { .. }
        | Amount::CreaturesDiedThisTurn { .. }
        | Amount::NontokenCreaturesDiedThisTurn { .. }
        | Amount::CardsDiscardedThisTurn
        | Amount::TokensCreatedThisTurn { .. }
        | Amount::CardsPutIntoGraveyardFromHandOrLibraryThisTurn { .. }
        | Amount::SacrificedPermsThisTurn
        | Amount::GreatestOpponentCardsDrawnThisTurn => {
            DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS
        }

        Amount::SpellsCastThisTurnCount { filter, .. } => {
            DirtyMask::TURN_HISTORY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Amount::CreaturesAttackedWithThisTurn
        | Amount::CreaturesAttackedTotalThisTurn
        | Amount::OpponentsDealtDamageThisTurn
        | Amount::OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn { .. }
        | Amount::AttackingCountToPlayer { .. }
        | Amount::DamageDealtToOpponentsThisTurn
        | Amount::DamageDealtToSelfByPlayerThisTurn { .. }
        | Amount::TimesAttackedThisTurn { .. } => DirtyMask::COMBAT | DirtyMask::TURN_HISTORY,

        Amount::ExiledCardsSharingTypeCount { .. } => DirtyMask::EXILE,
    }
}

pub fn dirty_mask_for_condition(cond: &Condition) -> DirtyMask {
    match cond {
        Condition::Compare { left, right, .. } => {
            dirty_mask_for_amount(left) | dirty_mask_for_amount(right)
        }
        Condition::And { of } | Condition::Or { of } => {
            let mut acc = DirtyMask::empty();
            for c in of {
                acc |= dirty_mask_for_condition(c);
            }
            acc
        }
        Condition::Not { of } => dirty_mask_for_condition(of),

        Condition::GraveyardAbove { filter, .. } => {
            DirtyMask::GRAVEYARDS | dirty_mask_for_filter(filter)
        }

        Condition::IsTurnOf { .. }
        | Condition::IsYourTurn
        | Condition::NotYourTurn
        | Condition::NotStartingPlayer
        | Condition::NotSpellControllerTurn
        | Condition::NotEventPlayerTurn
        | Condition::IsYourMainPhase
        | Condition::YourTurnsTakenAtLeast { .. } => DirtyMask::TURN_HISTORY,

        Condition::ControlCommander
        | Condition::DistinctControlledCreaturePowers { .. }
        | Condition::TotalPowerControlled { .. }
        | Condition::ColorsAmongControlledPermanents { .. }
        | Condition::MostCommonColorAmongPermanents { .. }
        | Condition::SameNameLandsControlledAtLeast { .. }
        | Condition::AllLandsAreSubtype { .. }
        | Condition::OpponentCount { .. }
        | Condition::SelfInZone { .. } => DirtyMask::TOPOLOGY,

        Condition::Kicked { .. }
        | Condition::Evoked
        | Condition::Escaped
        | Condition::Foretold
        | Condition::Blitzed
        | Condition::Saddled
        | Condition::Monstrous
        | Condition::GiftPromised
        | Condition::TributePaid
        | Condition::IsPaired
        | Condition::HasEnduringStory
        | Condition::IsHarnessed
        | Condition::IsSuspended
        | Condition::IsRenowned
        | Condition::SelfPrepared
        | Condition::IsImpending
        | Condition::ClassLevelIs { .. }
        | Condition::ClassLevelAtLeast { .. }
        | Condition::CaseSolved
        | Condition::CaseUnsolved
        | Condition::AnchorWordChosen { .. }
        | Condition::ExhaustUnused { .. }
        | Condition::WasCast { .. }
        | Condition::ControlledByOtherThanOwner
        | Condition::PaidNamedAltCost { .. }
        | Condition::AltCostPaid { .. }
        | Condition::AdditionalCostPaid { .. }
        | Condition::SpellWasBargained
        | Condition::SpellCastFromZone { .. }
        | Condition::EnteredWithXAtLeast { .. }
        | Condition::ManaColorSpentToCast { .. }
        | Condition::SameColorManaSpentAtLeast { .. }
        | Condition::TreasureManaSpentToCast
        | Condition::ManaSpentGreaterThanSelfPower => DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY,

        Condition::IsDay
        | Condition::IsNight
        | Condition::HasCompletedDungeon
        | Condition::HasInitiative
        | Condition::HasCitysBlessing
        | Condition::HasMaxSpeed
        | Condition::IsMonarch { .. }
        | Condition::OpponentPoisonAtLeast { .. }
        | Condition::PlayerHasFewerHandThan { .. }
        | Condition::PlayerHasFewerCountersThan { .. }
        | Condition::TargetHasFewerCountersThan { .. }
        | Condition::OpponentHasMoreCardsInHandThan { .. }
        | Condition::MoreCardsInHandThanEachOpponent
        | Condition::PlayersLostGameAtLeast { .. }
        | Condition::RingBearerAndTemptedAtLeast { .. }
        | Condition::EventPlayerHandCount { .. } => DirtyMask::PLAYER_STATE,

        Condition::MatchesFilter { filter, .. }
        | Condition::ControlsFilter { filter }
        | Condition::PlayerControlsFilter { filter, .. }
        | Condition::OpponentControlsAtLeastNMore { filter, .. }
        | Condition::NoMatchingCardsInHand { filter }
        | Condition::OnlyNamedOrNoMatchingInHand { filter }
        | Condition::AbilityTargetsFilter { filter }
        | Condition::YouControlGreatestManaValueFilter { filter }
        | Condition::YourTeamControlsOtherFilter { filter } => {
            dirty_mask_for_filter(filter) | DirtyMask::PLAYER_STATE
        }

        Condition::TargetsStackSpell { filter } => {
            DirtyMask::TOPOLOGY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Condition::ObjectState { filter, .. } => {
            DirtyMask::TAPPED
                | DirtyMask::COMBAT
                | DirtyMask::DESIGNATIONS
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Condition::AttachedPermanentMatches { .. } => DirtyMask::TOPOLOGY | DirtyMask::TAPPED,

        Condition::EvolveCompare | Condition::EnchantedCreaturePowerAtLeast { .. } => {
            DirtyMask::TOPOLOGY | DirtyMask::COUNTERS
        }

        Condition::AnyPlayerLife { .. }
        | Condition::YouHaveMostLife
        | Condition::YourLifeLessThanStarting
        | Condition::LifeAtLeastAboveStarting { .. }
        | Condition::LifeCompareHalfStarting { .. }
        | Condition::DethroneMostLife => DirtyMask::LIFE,

        Condition::AnyOpponentMetric { metric, .. } => match metric {
            mtg_ir::OpponentMetricKind::Life => DirtyMask::LIFE,
            mtg_ir::OpponentMetricKind::Hand => DirtyMask::PLAYER_STATE,
            mtg_ir::OpponentMetricKind::Graveyard => DirtyMask::GRAVEYARDS,
        },

        Condition::OpponentComparison { metric } => match metric {
            mtg_ir::OpponentComparisonMetric::Lands
            | mtg_ir::OpponentComparisonMetric::Creatures => DirtyMask::TOPOLOGY,
            mtg_ir::OpponentComparisonMetric::Life => DirtyMask::LIFE,
            mtg_ir::OpponentComparisonMetric::Hand => DirtyMask::PLAYER_STATE,
        },

        Condition::GainedLifeThisTurn { min } => {
            DirtyMask::LIFE
                | DirtyMask::TURN_HISTORY
                | min
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_amount)
        }

        Condition::OpponentGainedLifeThisTurn
        | Condition::YourTeamGainedLifeThisTurn
        | Condition::OpponentLostLifeThisTurn
        | Condition::YouLostLifeThisTurn
        | Condition::PlayerLostLifeThisTurn { .. }
        | Condition::PlayerLostLifeAtLeastThisTurn { .. }
        | Condition::PlayerLostLifeLastTurn => DirtyMask::LIFE | DirtyMask::TURN_HISTORY,

        Condition::AttackedThisTurn
        | Condition::AttackedWithCommanderThisTurn
        | Condition::ExertedThisTurn
        | Condition::BoastAttackedThisTurn
        | Condition::TrainingGreaterAttacker
        | Condition::CreaturesAttackedCountThisTurn { .. }
        | Condition::SourcesDealtDamageThisTurn { .. }
        | Condition::AttackingTotalPowerAtLeast { .. }
        | Condition::YouWereAttackedThisCombat
        | Condition::NotBeingDeclaredAsAttacker
        | Condition::SelfDealtDamageToOpponentThisTurn
        | Condition::DamageDealtToSelfThisTurnAtLeast { .. }
        | Condition::DealtDamageByCreaturesAtLeastThisTurn { .. }
        | Condition::NoCreaturesAttackedThisTurn
        | Condition::SelfAttackedThisCombat
        | Condition::SelfDidntAttackThisTurn
        | Condition::IsAttackingAlone { .. }
        | Condition::HasntDealtDamageYet { .. }
        | Condition::AnyAttackedPlayerWithMostLife
        | Condition::FirstCombatPhaseOfTurn
        | Condition::ControlAttackingLegendary
        | Condition::SelfRemovedFromCombatOrAttached => DirtyMask::COMBAT | DirtyMask::TURN_HISTORY,

        Condition::AttackingCount { filter, .. } => {
            DirtyMask::COMBAT
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }
        Condition::AttackingYouCount { .. } => DirtyMask::COMBAT,

        Condition::WasDealtCombatDamageBySelfNameThisGame { .. } => {
            DirtyMask::COMBAT | DirtyMask::PLAYER_STATE
        }

        Condition::CreatureDiedThisTurn
        | Condition::CreatureDiedUnderYourControlThisTurn
        | Condition::PermanentLeftBattlefieldThisTurn
        | Condition::DescendedThisTurn
        | Condition::CardLeftYourGraveyardThisTurn
        | Condition::DistinctManaValuesInGraveyardAtLeast { .. }
        | Condition::AnyGraveyardAtLeast { .. }
        | Condition::NoSameNameControlledOrInGraveyard => {
            DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY
        }

        Condition::CardsPutIntoGraveyardFromAnywhereThisTurn { filter, .. }
        | Condition::OpponentCardsPutIntoGraveyardAtLeastThisTurn { filter, .. } => {
            DirtyMask::GRAVEYARDS
                | DirtyMask::TURN_HISTORY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Condition::SpellsCastThisTurnAtLeast { filter, .. }
        | Condition::NthMatchingSpellThisTurn { filter, .. }
        | Condition::OpponentCastSpellsAtLeast { filter, .. } => {
            DirtyMask::TURN_HISTORY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Condition::EnteredUnderYourControlThisTurn { filter, .. } => {
            DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY | dirty_mask_for_filter(filter)
        }

        Condition::OpponentHadPermEnteredThisTurn { filter, .. } => {
            DirtyMask::TOPOLOGY
                | DirtyMask::TURN_HISTORY
                | filter
                    .as_deref()
                    .map_or(DirtyMask::empty(), dirty_mask_for_filter)
        }

        Condition::SacrificedFilterThisTurn { filter, .. } => {
            DirtyMask::TURN_HISTORY | dirty_mask_for_filter(filter)
        }

        Condition::ZoneChangeFromToThisTurn {
            from, to, filter, ..
        } => {
            DirtyMask::TURN_HISTORY
                | zone_dirty_mask(*to)
                | from.map_or(DirtyMask::empty(), zone_dirty_mask)
                | dirty_mask_for_filter(filter)
        }

        Condition::CameUnderControlSinceLastUpkeep
        | Condition::NinjutsuWindow
        | Condition::SpellsCastLastTurn { .. }
        | Condition::NonlandEnteredThisTurn { .. }
        | Condition::VoidThisTurn
        | Condition::CardsDrawnThisTurnAtLeast { .. }
        | Condition::CommittedCrimeThisTurn
        | Condition::NoSpellCastFromHandThisTurn
        | Condition::NoSpellCastThisTurn
        | Condition::EventObjectEnteredThisTurn
        | Condition::SelfEnteredThisTurn
        | Condition::AbilityActivatedCountThisTurn { .. }
        | Condition::YouDiscardedThisTurn
        | Condition::DiscardedCardThisTurn { .. }
        | Condition::OpponentDrewCardsAtLeastThisTurn { .. }
        | Condition::OpponentSearchedLibraryThisTurn
        | Condition::YourCreatureSpellCounteredByOpponentThisTurn
        | Condition::YourNoncreatureDestroyedByOpponentThisTurn
        | Condition::PutCounterOnCreatureThisTurn { .. }
        | Condition::CastCreatureAndNoncreatureThisTurn
        | Condition::SelfHaventAddedManaThisTurn
        | Condition::CounterPutOnSelfThisTurn
        | Condition::PlayerHadAnotherLandEnterThisTurn
        | Condition::CreatedTokenThisTurn
        | Condition::PlayedLandThisTurn
        | Condition::DidntActivateLoyaltyThisTurn
        | Condition::FirstSpellThisTurnWithManaFrom { .. } => {
            DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE
        }

        Condition::LkiHasNoCounter { .. }
        | Condition::HadCounterOnIt { .. }
        | Condition::SourceHasCounter { .. } => DirtyMask::COUNTERS,

        Condition::CountersAmongFilterAtLeast { filter, .. } => {
            DirtyMask::COUNTERS | dirty_mask_for_filter(filter)
        }

        Condition::HasCardsExiledWithSelf | Condition::SourceExiledThisWay => DirtyMask::EXILE,

        Condition::LkiToughnessLessThan { .. }
        | Condition::ControlledTargetAtResolution { .. }
        | Condition::ChoseOtherRingBearer
        | Condition::AllPlayersCantCastCheck
        | Condition::DiedThisWay { .. }
        | Condition::NotPutOntoBattlefieldByThisAbility
        | Condition::PlayerDidntDiscardNonlandThisWay
        | Condition::PlayerDiscardedHandThisWay
        | Condition::DistinctCardTypesAmongSacrificedAtLeast { .. }
        | Condition::CreatureCardMilledThisWay
        | Condition::LandCardMilledThisWay
        | Condition::DiscardedNonlandThisWay
        | Condition::DiscardedCreatureThisWay
        | Condition::DiscardedNoncreatureThisWay
        | Condition::NotDrawnCardsThisWay
        | Condition::ExiledCountAtLeast { .. }
        | Condition::DidRevealThisWay
        | Condition::WonClashThisWay
        | Condition::SacrificedNonlandThisWay
        | Condition::DidntPutOntoBattlefieldThisWay
        | Condition::CastTriggerCreatureSpell
        | Condition::DidLastOptionalStep
        | Condition::EventAbilityIsNotMana
        | Condition::EventAbilityLifePaid
        | Condition::EventSpellWasKicked
        | Condition::EventSpellHasKeyword { .. }
        | Condition::EventSpellNoColoredManaSpent
        | Condition::EventSpellManaSpentLessThanMv
        | Condition::EventEnteredOrCastFromZone { .. }
        | Condition::EventEnteredOrCastFromGraveyardAny
        | Condition::EventObjectPowerOrToughnessGreaterThanSelf
        | Condition::EventObjectWasNotSubtype { .. }
        | Condition::EventWasAttachedEnchantedOrEquipped
        | Condition::EventCrewCountEquals { .. }
        | Condition::EventDamageDealtBySubtype { .. }
        | Condition::EventDamagedTargetDealtExcessDamageThisTurn
        | Condition::EventAttackingPlayerWithMostLife
        | Condition::EventAttackerHasAnotherUnattackedOpponent
        | Condition::EventMinAttackersToYouOrPlaneswalkers { .. }
        | Condition::EventSharesCardTypeWithLinkedExile
        | Condition::EventObjectGreatestPowerAmongAll => DirtyMask::empty(),

        Condition::EventObjectMatchedLki { filter }
        | Condition::EventPlayerControlsMoreThanYou { filter }
        | Condition::EventPlayerControlsMoreThanEachOther { filter }
        | Condition::EventSpellMatchesFilter { filter }
        | Condition::EventObjectMatchesOrSubtype { filter }
        | Condition::ReturnedFilterToHandThisWay { filter } => dirty_mask_for_filter(filter),
    }
}

#[inline]
fn zone_dirty_mask(zone: Zone) -> DirtyMask {
    match zone {
        Zone::Battlefield | Zone::Stack => DirtyMask::TOPOLOGY,
        Zone::Graveyard => DirtyMask::GRAVEYARDS,
        Zone::Exile => DirtyMask::EXILE,
        Zone::Hand | Zone::Library | Zone::Command => DirtyMask::PLAYER_STATE,
    }
}
