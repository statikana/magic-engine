use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::cost::{CostDef, CostDelta};
use crate::effect::EffectStep;
use crate::filter::{ObjectFilter, PlayerFilter, TargetSpec};
use crate::replace::ReplacementDef;
use crate::static_def::StaticDef;
use crate::trigger::TriggerDef;
use crate::types::{
    CardLayout, CardTypeMask, ColorMask, KeywordId, ManaCost, PrintedPt, Span, SubtypeId,
    SupertypeMask, Timing, ZoneMask,
};
use crate::value::{Amount, Condition};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrototypeSpec {
    pub mana_cost: ManaCost,
    pub power: i32,
    pub toughness: i32,
    pub colors: ColorMask,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Characteristics {
    pub mana_cost: ManaCost,
    pub types: CardTypeMask,
    #[serde(default)]
    pub subtypes: SmallVec<[SubtypeId; 3]>,
    #[serde(default)]
    pub supertypes: SupertypeMask,
    #[serde(default)]
    pub colors: ColorMask,
    #[serde(default)]
    pub color_indicator: Option<ColorMask>,
    #[serde(default)]
    pub pt: Option<PrintedPt>,
    #[serde(default)]
    pub loyalty: Option<i32>,
    #[serde(default)]
    pub defense: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpellCastRestrictionKind {
    DuringCombat,
    DuringYourTurn,
    BeforeAttackers,
    AfterAttackers,
    DuringBlockers,
    BeforeBlockers,
    AfterBlockers,
    DuringOpponentsTurn,
    DuringYourUpkeep,
    AfterCombat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivationRestriction {
    OnceEachTurn,
    DuringYourTurn,
    DuringYourUpkeep,
    DuringCombat,
    BeforeAttackers,
    OncePerGame,
    AfterAttackers,
    BeforeBlockers,
    DuringBlockers,
    AfterBlockers,
    AfterCombat,
    DuringOpponentsTurn,
    OnlyAsInstant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivatorWho {
    You,
    Any,
    Opponent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GiftKind {
    Card,
    Food,
    Treasure,
    TappedFish,
    Octopus,
    ExtraTurn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KickerCostOption {
    pub cost: ManaCost,
    pub cost_defs: Vec<CostDef>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalCostReduction {
    pub delta: CostDelta,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeywordDef {
    pub keyword: KeywordId,
    #[serde(default)]
    pub cost: Option<ManaCost>,
    #[serde(default)]
    pub param: Option<String>,
    #[serde(default)]
    pub n: Option<i32>,
    #[serde(default)]
    pub cost_defs: Vec<CostDef>,
    #[serde(default)]
    pub kicker_costs: Vec<KickerCostOption>,
    #[serde(default)]
    pub equip_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub enchant_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub enchant_player: Option<PlayerFilter>,
    #[serde(default)]
    pub affinity_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub cycling_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub cycling_desc: Option<String>,
    #[serde(default)]
    pub prototype_spec: Option<PrototypeSpec>,
    #[serde(default)]
    pub awaken_n: Option<u16>,
    #[serde(default)]
    pub emerge_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub champion_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub devour_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub devour_x: bool,
    #[serde(default)]
    pub equip_allow_planeswalker: bool,
    #[serde(default)]
    pub backup_ability_start_index: Option<u16>,
    #[serde(default)]
    pub gift_kind: Option<GiftKind>,
    #[serde(default)]
    pub craft_materials: Vec<ObjectFilter>,
    #[serde(default)]
    pub craft_any_number: bool,
    #[serde(default)]
    pub craft_share_card_type: bool,
    #[serde(default)]
    pub splice_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub impending_n: Option<u16>,
    #[serde(default)]
    pub landwalk_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub suspend_x: bool,
    #[serde(default)]
    pub equip_cost_reduction: Option<ConditionalCostReduction>,
    #[serde(default)]
    pub dynamic_amount: Option<Box<Amount>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalMode {
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpponentChoosesModal {
    TargetOpponent,
    Opponent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalModalRange {
    pub condition: Box<Condition>,
    pub min: u16,
    pub max: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModalDef {
    pub min: u16,
    pub max: u16,
    #[serde(default)]
    pub allow_repeat: bool,
    #[serde(default)]
    pub once_per_turn_unique: bool,
    #[serde(default)]
    pub up_to: bool,
    #[serde(default)]
    pub conditional_choose_all: Option<Box<Condition>>,
    pub modes: Vec<ModalMode>,
    #[serde(default)]
    pub per_mode_costs: Vec<Vec<CostDef>>,
    #[serde(default)]
    pub unchosen_permanent: bool,
    #[serde(default)]
    pub opponent_chooses: Option<OpponentChoosesModal>,
    #[serde(default)]
    pub pawprint_budget: Option<u16>,
    #[serde(default)]
    pub pawprint_costs: Vec<u16>,
    #[serde(default)]
    pub dynamic_max: Option<Box<Amount>>,
    #[serde(default)]
    pub random_mode: bool,
    #[serde(default)]
    pub prefix_effect: Vec<EffectStep>,
    #[serde(default)]
    pub suffix_effect: Vec<EffectStep>,
    #[serde(default)]
    pub conditional_choose_range: Option<ConditionalModalRange>,
    #[serde(default)]
    pub conditional_not_random: Option<Box<Condition>>,
    #[serde(default)]
    pub different_player_targets_per_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeywordAbility {
    pub keyword: KeywordDef,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpellAbility {
    #[serde(default)]
    pub additional_costs: Vec<CostDef>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    #[serde(default)]
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub modal: Option<ModalDef>,
    #[serde(default)]
    pub cast_restrictions: Vec<SpellCastRestrictionKind>,
    #[serde(default)]
    pub cast_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub x_cant_be_zero: bool,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivatedAbility {
    pub id: String,
    pub cost: Vec<CostDef>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    #[serde(default)]
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub modal: Option<ModalDef>,
    pub timing: Timing,
    #[serde(default)]
    pub is_mana: bool,
    #[serde(default)]
    pub activation_restrictions: Vec<ActivationRestriction>,
    #[serde(default)]
    pub activation_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub activator_who: Option<ActivatorWho>,
    #[serde(default)]
    pub power_up_reduction: bool,
    #[serde(default)]
    pub x_cant_be_zero: bool,
    #[serde(default)]
    pub activation_cost_reduction: Option<ConditionalCostReduction>,
    #[serde(default)]
    pub activation_zones: ZoneMask,
    #[serde(default)]
    pub cost_keyword: Option<KeywordId>,
    #[serde(default)]
    pub intrinsic: bool,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub grant_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub x_spend_colors: ColorMask,
    #[serde(default)]
    pub x_colored_only: bool,
    #[serde(default)]
    pub x_max_one_per_color: bool,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggeredAbility {
    pub trigger: TriggerDef,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
    #[serde(default)]
    pub grant_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    #[serde(default)]
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub modal: Option<ModalDef>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticAbility {
    pub static_def: StaticDef,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementAbility {
    pub replacement: ReplacementDef,
    #[serde(default)]
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedReason {
    Unparsed,
    KeywordNotImplemented,
    StaticNotCompiled,
    Multiface,
    EventNotImplemented,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsupportedAbility {
    pub reason: UnsupportedReason,
    pub text: String,
    #[serde(default)]
    pub span: Span,
    #[serde(default)]
    pub unparsed: Vec<Span>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub cost: Vec<CostDef>,
    #[serde(default)]
    pub cost_complete: Option<bool>,
    #[serde(default)]
    pub timing: Option<Timing>,
    #[serde(default)]
    pub is_mana: bool,
    #[serde(default)]
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    #[serde(default)]
    pub activation_restrictions: Vec<ActivationRestriction>,
    #[serde(default)]
    pub activation_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub activator_who: Option<ActivatorWho>,
    #[serde(default)]
    pub power_up_reduction: bool,
    #[serde(default)]
    pub x_cant_be_zero: bool,
    #[serde(default)]
    pub activation_cost_reduction: Option<ConditionalCostReduction>,
    #[serde(default)]
    pub cast_restrictions: Vec<SpellCastRestrictionKind>,
    #[serde(default)]
    pub cast_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub trigger: Option<TriggerDef>,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
    #[serde(default)]
    pub grant_condition: Option<Box<Condition>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AbilityDef {
    Keyword(KeywordAbility),
    Spell(SpellAbility),
    Activated(ActivatedAbility),
    Triggered(TriggeredAbility),
    Static(StaticAbility),
    Replacement(ReplacementAbility),
    Unsupported(UnsupportedAbility),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaceDef {
    pub name: String,
    pub characteristics: Characteristics,
    pub abilities: Vec<AbilityDef>,
    #[serde(default)]
    pub has_read_ahead: bool,
    #[serde(default)]
    pub prototype_spec: Option<PrototypeSpec>,
    #[serde(default)]
    pub cleaved_spell_spec: Option<Box<SpellAbility>>,
    #[serde(default)]
    pub gift_promised_spell_spec: Option<Box<SpellAbility>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardDef {
    pub oracle_id: String,
    pub compiler_version: u32,
    pub text_hash: String,
    /// Display and CR 704.5j/201.2 name-equality semantics only; rules logic never dispatches on names.
    pub name: String,
    #[serde(default)]
    pub layout: CardLayout,
    pub faces: Vec<FaceDef>,
    pub fully_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckEntry {
    pub oracle_id: String,
    pub count: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckDef {
    pub id: String,
    pub name: String,
    pub commanders: Vec<String>,
    pub cards: Vec<DeckEntry>,
}
