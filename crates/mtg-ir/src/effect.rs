use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::card::ModalDef;
use crate::cost::{CostDef, ManaSpendingRestriction};
use crate::filter::{ObjectFilter, PlayerFilter, TargetSpec};
use crate::trigger::TriggerDef;
use crate::types::{
    CardTypeMask, ColorMask, CounterId, CounterTarget, Duration, KeywordId, ManaColor, ManaCost,
    PlayerCounterKind, RoundMode, Span, SubtypeId, SupertypeMask, Zone, ZoneMask,
};
use crate::value::{Amount, Condition, PlayerRef, Ref};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Selector {
    SelfObj,
    Binding {
        name: String,
    },
    Target {
        index: u16,
    },
    All {
        filter: Box<ObjectFilter>,
    },
    Player {
        filter: PlayerFilter,
    },
    PlayerRef {
        player: PlayerRef,
    },
    EventObject,
    AttachedTo,
    LibraryTop {
        #[serde(default)]
        player: Option<PlayerRef>,
    },
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OptionalMode {
    DoIfPaid,
    DoUnlessPaid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalSpec {
    pub chooser: PlayerRef,
    #[serde(default)]
    pub cost: Vec<CostDef>,
    #[serde(default = "default_optional_mode")]
    pub mode: OptionalMode,
}

fn default_optional_mode() -> OptionalMode {
    OptionalMode::DoIfPaid
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StepGate {
    IfPrecedingAccepted {
        step_index: u16,
    },
    IfPrecedingDeclined {
        step_index: u16,
    },
    Condition {
        condition: Box<Condition>,
    },
    Combined {
        #[serde(default)]
        if_preceding_accepted: Option<u16>,
        #[serde(default)]
        if_preceding_declined: Option<u16>,
        #[serde(default)]
        condition: Option<Box<Condition>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChooserOverride {
    Opponent,
    You,
    EventPlayer,
}

/// Shared selection modifier for verbs that let a player pick a subset of matching objects.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionMod {
    #[serde(default)]
    pub choice_count: Option<Box<Amount>>,
    #[serde(default)]
    pub up_to: bool,
    #[serde(default)]
    pub at_random: bool,
    #[serde(default)]
    pub filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub chooser: Option<ChooserOverride>,
}

impl SelectionMod {
    pub fn is_empty(&self) -> bool {
        self.choice_count.is_none()
            && !self.up_to
            && !self.at_random
            && self.filter.is_none()
            && self.chooser.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectStep {
    pub op: EffectOp,
    pub subject: Selector,
    #[serde(default)]
    pub actor: Option<PlayerRef>,
    #[serde(default)]
    pub amount: Option<Box<Amount>>,
    #[serde(default)]
    pub optional: Option<OptionalSpec>,
    #[serde(default)]
    pub gate: Option<StepGate>,
    #[serde(default)]
    pub bind_as: Option<String>,
    #[serde(default)]
    pub do_this_once_per_turn: bool,
    #[serde(default)]
    pub reflexive_trigger: Option<Box<ReflexiveTriggerSpec>>,
    #[serde(default)]
    pub span: Span,
}

// ---- Structured payload types for EffectOp --------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSpec {
    pub name: String,
    pub power: i32,
    pub toughness: i32,
    #[serde(default)]
    pub colors: ColorMask,
    #[serde(default)]
    pub types: CardTypeMask,
    #[serde(default)]
    pub supertypes: SupertypeMask,
    #[serde(default)]
    pub subtypes: SmallVec<[SubtypeId; 3]>,
    #[serde(default)]
    pub type_line: String,
    #[serde(default)]
    pub keywords: SmallVec<[KeywordId; 3]>,
    #[serde(default)]
    pub tapped: bool,
    #[serde(default)]
    pub quoted_abilities: Vec<String>,
    #[serde(default)]
    pub legendary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalCopyCounter {
    pub condition: Box<Condition>,
    pub counter: CounterId,
    pub count: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalTypeCounter {
    pub card_type: CardTypeMask,
    pub counter: CounterId,
    pub count: u16,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyModificationSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub keep_self_name: bool,
    #[serde(default)]
    pub override_name: Option<String>,
    #[serde(default)]
    pub power: Option<i32>,
    #[serde(default)]
    pub toughness: Option<i32>,
    #[serde(default)]
    pub set_colors: Option<ColorMask>,
    #[serde(default)]
    pub add_colors: ColorMask,
    #[serde(default)]
    pub add_types: CardTypeMask,
    #[serde(default)]
    pub set_types: Option<CardTypeMask>,
    #[serde(default)]
    pub add_subtypes: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub set_subtypes: Option<SmallVec<[SubtypeId; 2]>>,
    #[serde(default)]
    pub add_supertypes: SupertypeMask,
    #[serde(default)]
    pub remove_supertypes: SupertypeMask,
    #[serde(default)]
    pub add_keywords: SmallVec<[KeywordId; 2]>,
    #[serde(default)]
    pub no_mana_cost: bool,
    #[serde(default)]
    pub added_ability_texts: Vec<String>,
    #[serde(default)]
    pub halve_pt: Option<RoundMode>,
    #[serde(default)]
    pub keep_this_ability: bool,
    #[serde(default)]
    pub additional_counters: Option<(CounterId, Box<Amount>)>,
    #[serde(default)]
    pub conditional_counters: Option<ConditionalCopyCounter>,
    #[serde(default)]
    pub conditional_type_counters: Vec<ConditionalTypeCounter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreventMode {
    All,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreventScope {
    Any,
    Subject,
    YouAndCreaturesYouControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreventRedirectTo {
    SourceController,
    ChosenPermanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceKindFilter {
    Creature,
    Noncreature,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnPreventedKind {
    GainLifeEqual,
    DealDamageEqual,
    PutCountersEqual,
    DrawEqual,
    DrawOne,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnPreventedRider {
    pub kind: OnPreventedKind,
    #[serde(default)]
    pub counter: Option<CounterId>,
    #[serde(default)]
    pub recipient: Option<Box<Selector>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreventReflectTarget {
    SourceCreature,
    SourceController,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DamagePreventionSpec {
    pub mode: PreventMode,
    #[serde(default)]
    pub combat_only: bool,
    pub scope: PreventScope,
    #[serde(default)]
    pub from_source: Option<Box<Selector>>,
    #[serde(default)]
    pub also_prevent_dealt_by_subject: bool,
    #[serde(default)]
    pub choose_source: bool,
    #[serde(default)]
    pub next_damage_event_only: bool,
    #[serde(default)]
    pub on_prevented: Option<OnPreventedRider>,
    #[serde(default)]
    pub redirect_to: Option<PreventRedirectTo>,
    #[serde(default)]
    pub redirect_source_color_filter: ColorMask,
    #[serde(default)]
    pub redirect_source_kind: Option<SourceKindFilter>,
    #[serde(default)]
    pub min_life_floor: Option<i32>,
    #[serde(default)]
    pub all_damage_to_you_and_perms: bool,
    #[serde(default)]
    pub you_and_planeswalkers_you_control: bool,
    #[serde(default)]
    pub from_sources_you_dont_control: bool,
    #[serde(default)]
    pub from_source_colors: ColorMask,
    #[serde(default)]
    pub from_source_not_subtypes: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub on_prevented_creature: Option<PreventReflectTarget>,
    #[serde(default)]
    pub on_prevented_noncreature: Option<PreventReflectTarget>,
    #[serde(default)]
    pub on_prevented_to_damage_source_controller: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimedCombatRestrictionKind {
    CantBlock,
    CantBeBlocked,
    CantAttack,
    CantAttackOrBlock,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockerExceptionSpec {
    #[serde(default)]
    pub types: CardTypeMask,
    #[serde(default)]
    pub subtypes: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub colors: ColorMask,
    #[serde(default)]
    pub not_colors: ColorMask,
    #[serde(default)]
    pub artifact_or_colors: ColorMask,
    #[serde(default)]
    pub keywords: SmallVec<[KeywordId; 2]>,
    #[serde(default)]
    pub min_blockers: Option<u16>,
    #[serde(default)]
    pub max_power: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimedCombatRestrictionSpec {
    pub kind: TimedCombatRestrictionKind,
    #[serde(default)]
    pub except_by: Option<BlockerExceptionSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaChoiceKind {
    Any,
    CommanderIdentity,
    OpponentLandColors,
    ChosenColor,
    EventProducedTypes,
    EventProducedColors,
    ControlledLandProducedTypes,
    ControlledLandProducedColors,
    Colors,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaProduction {
    Fixed {
        symbols: SmallVec<[ManaColor; 4]>,
    },
    Choice {
        kind: ManaChoiceKind,
        #[serde(default)]
        colors: ColorMask,
    },
    Bundles {
        options: Vec<SmallVec<[ManaColor; 3]>>,
    },
    Combination {
        #[serde(default)]
        any_color: bool,
        #[serde(default)]
        colors: ColorMask,
    },
    OrChosenColor {
        colors: ColorMask,
    },
    WithChosenColor {
        colors: ColorMask,
    },
    FromExiledColors,
    PerColorControlled,
    LandProducedTypes,
    ChosenColorAmount {
        amount: Box<Amount>,
    },
    FromProducedEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaRestrictionTag {
    ArtifactSpellOrNonSpell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LibraryPosition {
    Top,
    Bottom,
    TopOrBottom,
    SecondOrBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LibraryChoiceBy {
    Controller,
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayFromExileDuration {
    ThisTurn,
    UntilEndOfYourNextTurn,
    UntilYourNextEndStep,
    AsLongAsExiled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayFromExileSpec {
    pub duration: PlayFromExileDuration,
    #[serde(default)]
    pub spend_mana_as_any_color: bool,
    #[serde(default)]
    pub spells_only: bool,
    #[serde(default)]
    pub without_paying_mana_cost: bool,
    #[serde(default)]
    pub alternative_mana_cost: Option<ManaCost>,
    #[serde(default)]
    pub pay_life_equal_mana_value: bool,
    #[serde(default)]
    pub cost_reduction_generic: Option<u16>,
    #[serde(default)]
    pub plotted: bool,
    #[serde(default)]
    pub total_max_mv: Option<Box<Amount>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DigKeepTo {
    LibraryTop,
    Hand,
    Battlefield,
    Graveyard,
    Exile,
    CastFree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DigRestTo {
    LibraryBottom,
    Graveyard,
    LibraryTop,
    Exile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigPartition {
    pub count: u16,
    pub to: DigRestTo,
    #[serde(default)]
    pub to_hand: bool,
    #[serde(default)]
    pub playable_this_turn: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigDestinationBucket {
    pub filter: Box<ObjectFilter>,
    pub destination: Zone,
    #[serde(default)]
    pub tapped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigSpec {
    pub keep: u16,
    #[serde(default)]
    pub up_to: bool,
    #[serde(default)]
    pub keep_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub keep_all_matching: bool,
    #[serde(default)]
    pub keep_label: Option<String>,
    pub keep_to: DigKeepTo,
    #[serde(default)]
    pub keep_tapped: bool,
    #[serde(default)]
    pub keep_attacking: bool,
    #[serde(default)]
    pub keep_attacking_player: Option<PlayerRef>,
    #[serde(default)]
    pub reveal_all: bool,
    #[serde(default)]
    pub reveal_kept: bool,
    pub rest: DigRestTo,
    #[serde(default)]
    pub rest_random: bool,
    #[serde(default)]
    pub until_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub rest_to_hand: bool,
    #[serde(default)]
    pub shuffle_rest: bool,
    #[serde(default)]
    pub keep_max_mana_value: Option<Box<Amount>>,
    #[serde(default)]
    pub keep_max_mv_strict_lt: bool,
    #[serde(default)]
    pub choose_one_of_exiled: bool,
    #[serde(default)]
    pub play_from_exile: Option<PlayFromExileSpec>,
    #[serde(default)]
    pub partitions: Vec<DigPartition>,
    #[serde(default)]
    pub destination_buckets: Vec<DigDestinationBucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchDestination {
    pub zone: Zone,
    #[serde(default)]
    pub tapped: bool,
    #[serde(default)]
    pub on_top: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSpec {
    pub filter: Box<ObjectFilter>,
    pub count: u16,
    #[serde(default)]
    pub up_to: bool,
    #[serde(default)]
    pub stated_quality: bool,
    pub destinations: Vec<SearchDestination>,
    #[serde(default)]
    pub reveal: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub zones: ZoneMask,
    #[serde(default)]
    pub different_names: bool,
    #[serde(default)]
    pub shuffle_only_if_library: bool,
    #[serde(default)]
    pub searched_player: Option<PlayerRef>,
    #[serde(default)]
    pub cast_free: bool,
    #[serde(default)]
    pub target_controller_as_actor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CounterReplacementZone {
    Exile,
    LibraryTop,
    LibraryBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RedirectMode {
    ChangeSingleTarget,
    ChooseNewTargets,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedirectTargetsSpec {
    pub mode: RedirectMode,
    #[serde(default)]
    pub force_target_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExileUntilHitAction {
    PlayThisTurn,
    PlayUntilEndOfNextTurn,
    CastFree,
    Hand,
    Battlefield,
    BattlefieldOrHand,
    StayExiled,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExileUntilRest {
    LibraryBottomRandom,
    Graveyard,
    StayExiled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExileUntilSpec {
    pub filter: Box<ObjectFilter>,
    #[serde(default)]
    pub library_owner: Option<Box<Selector>>,
    #[serde(default)]
    pub actor: Option<PlayerRef>,
    pub hit_action: ExileUntilHitAction,
    #[serde(default)]
    pub cast_free_condition: Option<Box<Condition>>,
    #[serde(default)]
    pub fallback_to_hand: bool,
    #[serde(default)]
    pub spend_mana_as_any_color: bool,
    #[serde(default)]
    pub without_paying_mana_cost: bool,
    #[serde(default)]
    pub spells_only: bool,
    #[serde(default)]
    pub pay_life_equal_mana_value: bool,
    pub rest_destination: ExileUntilRest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PileSide {
    You,
    Opponent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PileSplitSpec {
    #[serde(default)]
    pub reveal: bool,
    pub pile_maker: PileSide,
    pub pile_chooser: PileSide,
    pub chosen_to: Zone,
    pub other_to: Zone,
    #[serde(default)]
    pub other_random_order: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CastFreeSpec {
    #[serde(default)]
    pub copy_card_first: bool,
    #[serde(default)]
    pub without_paying_mana_cost: bool,
    #[serde(default)]
    pub filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub up_to_count: Option<u16>,
    #[serde(default)]
    pub exile_if_put_into_graveyard: bool,
    #[serde(default)]
    pub alternative_mana_cost: Option<ManaCost>,
    #[serde(default)]
    pub pay_life_equal_mana_value: bool,
    #[serde(default)]
    pub total_max_mv: Option<Box<Amount>>,
    #[serde(default)]
    pub exiled_with_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleTokenKind {
    Cursed,
    Monster,
    Royal,
    Sorcerer,
    Virtuous,
    Wicked,
    YoungHero,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoubleTarget {
    Power,
    Toughness,
    Pt,
    Counters,
    Life,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoubleOrTripleSpec {
    pub factor: u8,
    pub target: DoubleTarget,
    #[serde(default)]
    pub counter: Option<CounterTarget>,
    #[serde(default)]
    pub duration: Option<Duration>,
    #[serde(default)]
    pub repeat_factor_count: Option<Box<Amount>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExchangeKind {
    Control,
    LifeTotals,
    LifeAndPower,
    LifeAndToughness,
    SwapAllControlledCreatures,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeSpec {
    pub kind: ExchangeKind,
    #[serde(default)]
    pub second: Option<Box<Selector>>,
    #[serde(default)]
    pub duration: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraveyardCastGrantSpec {
    pub until_end_of_next_turn: bool,
    #[serde(default)]
    pub flashback_equal_mana_cost: bool,
    #[serde(default)]
    pub alternative_mana_cost: Option<ManaCost>,
    #[serde(default)]
    pub without_paying_mana_cost: bool,
    #[serde(default)]
    pub exile_if_would_enter_graveyard: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteSpec {
    pub options: Vec<String>,
    #[serde(default)]
    pub extra_votes_for_actor: u16,
    #[serde(default)]
    pub per_option_effect: Vec<Vec<EffectStep>>,
    #[serde(default)]
    pub per_winning_effect: Vec<Vec<EffectStep>>,
    #[serde(default)]
    pub tie_winner_index: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReflexiveEventFilter {
    WhenYouDo,
    SacrificedThisWay,
    DiedThisWay,
    DiscardedThisWay,
    ExiledThisWay,
    RemovedCounterThisWay,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflexiveTriggerSpec {
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub modal: Option<Box<ModalDef>>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub event_filter: Option<ReflexiveEventFilter>,
    #[serde(default)]
    pub object_filter: Option<Box<ObjectFilter>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DieRollBranch {
    pub min: i32,
    pub max: i32,
    pub effect: Vec<EffectStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoerciveDiscardSpec {
    pub chooser: PlayerRef,
    #[serde(default)]
    pub filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub optional_choice: bool,
    pub destination: Zone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PartialRevealSpec {
    RandomOne,
    Count { amount: Box<Amount> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DesignationKind {
    Color,
    CreatureType,
    CardName,
    NonlandCardName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PerOpponentAttackMode {
    All,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerRuleKind {
    LifeTotalCantChange,
    ProtectionFromEverything,
    DamageCantBePrevented,
    CombatDamageCantBePrevented,
    ExtraLandPlay,
    CannotLoseGame,
    Hexproof,
    OpponentsCantCastThisTurn,
    CantCastSpellsThisTurn,
    SpellsCantBeCounteredThisTurn,
    CastAsThoughFlashThisTurn,
    NextSpellFlash,
    NextSpellCantBeCountered,
    NoMaxHandSizeRestOfGame,
    CantCastNoncreatureSpellsThisTurn,
    CantCastInstantsOrSorceriesThisTurn,
    CantActivateNonManaAbilitiesThisTurn,
    CantCastAdditionalSpellsThisTurn,
    CreatureSpellsCantBeCounteredThisTurn,
    NextArtifactSpellGainsSunburst,
    AllPlayersCantCastSpellsThisTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoopPlayersKind {
    EachOpponent,
    EachPlayer,
    EachOtherPlayer,
    TurnOrderStartingWithYou,
    TurnOrderStartingWithNextOpponent,
    ChooseOpponent,
    ChooseRandomOpponent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PunisherPlayersKind {
    EachOpponent,
    EachPlayer,
    SubjectPlayer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PunisherSpec {
    #[serde(default)]
    pub players: Option<PunisherPlayersKind>,
    pub default_steps: Vec<EffectStep>,
    #[serde(default)]
    pub unless_cost: Vec<CostDef>,
    #[serde(default)]
    pub unless_effect: Vec<EffectStep>,
    pub unless_who: PlayerRef,
    #[serde(default)]
    pub repeat_count: Option<Box<Amount>>,
    #[serde(default)]
    pub may_first: bool,
    #[serde(default)]
    pub mandatory_if_able: bool,
    #[serde(default)]
    pub track_not_done_kind: Option<String>,
    #[serde(default)]
    pub track_done_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveCounterSpec {
    pub from: Box<Selector>,
    pub counter: CounterTarget,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub up_to: bool,
    #[serde(default)]
    pub each_kind_not_on_dest: bool,
    #[serde(default)]
    pub one_or_more: bool,
    #[serde(default)]
    pub from_any_permanents: bool,
    #[serde(default)]
    pub choose_kind_on_permanent: bool,
    #[serde(default)]
    pub put_or_remove_kind: bool,
    #[serde(default)]
    pub any_number: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EachCreatureDealsDamage {
    pub filter: Box<ObjectFilter>,
    pub to_controller: bool,
    /// `None` means equal to each creature's power.
    #[serde(default)]
    pub amount: Option<Box<Amount>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnimateColorChoice {
    Single,
    OneOrMore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanarActionKind {
    Planeswalk,
    Chaos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpecialRevealTag {
    SpellweaverVolute,
    SpaceSectorLock,
    SpaceSectorCounters,
    SpaceSectorDestroy,
    NecromancyFlashCleanup,
    SacrificeSelfCleanup,
    Timetwister,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DelayedInlineDuration {
    ThisTurn,
    UntilYourNextTurn,
    NextCombat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayedTriggerPayload {
    pub trigger: Box<TriggerDef>,
    pub effect: Vec<EffectStep>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
    #[serde(default)]
    pub next_turn: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayedInlineTriggerSpec {
    pub duration: DelayedInlineDuration,
    #[serde(default)]
    pub sub_trigger_index: Option<u16>,
    #[serde(default)]
    pub bind_subject_as_watched: bool,
}

/// Strongly-typed effect operation enum replacing TS's `Verb` + 150-field `EffectParams` bag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectOp {
    Destroy {
        #[serde(default)]
        cant_be_regenerated: bool,
        #[serde(default)]
        select: SelectionMod,
    },
    Exile {
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        until_source_leaves: bool,
        #[serde(default)]
        until_opponent_becomes_monarch: bool,
        #[serde(default)]
        exile_all_except_bottom: bool,
        #[serde(default)]
        face_down: bool,
        #[serde(default)]
        look_only: bool,
        #[serde(default)]
        exile_with_counters: Option<(CounterId, u16)>,
    },
    Bounce {
        #[serde(default)]
        select: SelectionMod,
    },
    Move {
        #[serde(default)]
        from: Option<Zone>,
        #[serde(default)]
        to: Option<Zone>,
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        tapped: bool,
        #[serde(default)]
        under_your_control: bool,
        #[serde(default)]
        topmost_only: bool,
        #[serde(default)]
        may_shuffle: bool,
        #[serde(default)]
        position: Option<LibraryPosition>,
        #[serde(default)]
        from_top: Option<u16>,
        #[serde(default)]
        library_choice_by: Option<LibraryChoiceBy>,
        #[serde(default)]
        random_order: bool,
        #[serde(default)]
        entered_transformed: bool,
        #[serde(default)]
        entered_flipped: bool,
        #[serde(default)]
        enters_tapped_and_attacking: bool,
        #[serde(default)]
        as_enchantment_only: bool,
        #[serde(default)]
        unearth: bool,
        #[serde(default)]
        with_counters: Option<(CounterId, u16)>,
        #[serde(default)]
        attached_to: Option<Box<Selector>>,
        #[serde(default)]
        condition: Option<Box<Condition>>,
        #[serde(default)]
        choose_at_random_from_exiled_with_self: bool,
        #[serde(default)]
        leave_chosen_in_graveyard_rest_to_hand: bool,
        #[serde(default)]
        split_exiled_cost_cards_to_bottom_and_battlefield: bool,
    },
    Draw {
        #[serde(default)]
        up_to: bool,
        #[serde(default)]
        reveal_drawn: bool,
    },
    Scry,
    Surveil,
    LookAndReorder {
        #[serde(default)]
        may_shuffle: bool,
    },
    Investigate,
    Mill,
    Damage {
        #[serde(default)]
        source: Option<Box<Selector>>,
        #[serde(default)]
        also_players: Option<PlayerFilter>,
        #[serde(default)]
        divided: bool,
        #[serde(default)]
        excess_to_controller: bool,
        #[serde(default)]
        excess_to_controller_if_trample: bool,
        #[serde(default)]
        or_planeswalker_controlled_by: Option<PlayerRef>,
        #[serde(default)]
        damage_attacked_target_by_self: bool,
        #[serde(default)]
        soulfire_exile_damage_per_target: bool,
        #[serde(default)]
        each_creature_deals: Option<EachCreatureDealsDamage>,
    },
    GainLife {
        #[serde(default)]
        set_life_total: bool,
    },
    LoseLife,
    CreateToken {
        #[serde(default)]
        token: Option<Box<TokenSpec>>,
        #[serde(default)]
        dynamic_pt: Option<(Box<Amount>, Box<Amount>)>,
        #[serde(default)]
        tapped_and_attacking: bool,
        #[serde(default)]
        per_opponent_tapped_attacking: Option<PerOpponentAttackMode>,
        #[serde(default)]
        multiply_by_opponents_did_this_way: bool,
        #[serde(default)]
        benthic_anomaly_clone: bool,
    },
    CopyPermanent {
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        not_legendary: bool,
        #[serde(default)]
        tapped: bool,
        #[serde(default)]
        tapped_and_attacking: bool,
        #[serde(default)]
        per_opponent_tapped_attacking: Option<PerOpponentAttackMode>,
        #[serde(default)]
        copy_mod: Option<Box<CopyModificationSpec>>,
        #[serde(default)]
        except_pt: Option<(i32, i32)>,
    },
    CopySpell {
        #[serde(default)]
        count: Option<Box<Amount>>,
        #[serde(default)]
        may_choose_new_targets: bool,
        #[serde(default)]
        non_legendary: bool,
        #[serde(default)]
        except_colors: ColorMask,
        #[serde(default)]
        copy_event_ability: bool,
        #[serde(default)]
        copy_per_sacrificed_cost: bool,
        #[serde(default)]
        copy_all_controlled_spells_and_other_abilities: bool,
    },
    PutCounters {
        #[serde(default)]
        counter: Option<CounterId>,
        #[serde(default)]
        multi_counters: Vec<(CounterId, Box<Amount>)>,
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        divided: bool,
        #[serde(default)]
        allow_exile_counters: bool,
        #[serde(default)]
        remove_instead_of_add: bool,
        #[serde(default)]
        copy_counters_from: Option<Box<Selector>>,
        #[serde(default)]
        choose_counter_on_subject_and_add_one: bool,
        #[serde(default)]
        conditional_counter_on_type: Option<(CounterId, CounterId, bool)>,
        #[serde(default)]
        cast_bolas_bonus_loyalty: bool,
    },
    RemoveCounters {
        #[serde(default)]
        counter: Option<CounterId>,
        #[serde(default)]
        up_to: bool,
        #[serde(default)]
        if_unable_sacrifice_self: bool,
        #[serde(default)]
        remove_all_from_target_or_player: bool,
    },
    MoveCounters {
        #[serde(default)]
        counter: Option<CounterId>,
        #[serde(default)]
        to_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        spec: Option<MoveCounterSpec>,
    },
    PutPlayerCounters {
        counter: PlayerCounterKind,
    },
    Proliferate,
    Search {
        spec: Box<SearchSpec>,
        #[serde(default)]
        tapped: bool,
    },
    Pump {
        #[serde(default)]
        toughness: Option<Box<Amount>>,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        pump_choices: Vec<(i32, i32)>,
    },
    Grant {
        #[serde(default)]
        keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        remove_keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        cant_have_or_gain: bool,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        granted_ability_texts: Vec<String>,
        #[serde(default)]
        choose_keyword_options: SmallVec<[KeywordId; 3]>,
        #[serde(default)]
        choose_lose_keyword_options: SmallVec<[KeywordId; 3]>,
        #[serde(default)]
        choose_protection_color: bool,
        #[serde(default)]
        remove_all_abilities: bool,
        #[serde(default)]
        can_attack_no_defender: bool,
        #[serde(default)]
        must_attack_this_turn: bool,
        #[serde(default)]
        must_be_blocked_this_turn: bool,
        #[serde(default)]
        must_block_this_turn: bool,
        #[serde(default)]
        must_block_self_this_turn: bool,
        #[serde(default)]
        cant_block_self_this_turn: bool,
        #[serde(default)]
        cant_be_blocked_by_opponents_creatures: bool,
        #[serde(default)]
        assigns_no_combat_damage_this_turn: bool,
        #[serde(default)]
        grant_suspend_if_missing: bool,
        #[serde(default)]
        exile_resolving_spell_to_linked: bool,
        #[serde(default)]
        grant_trigger_spell_sunburst: bool,
        #[serde(default)]
        pay_any_mana_for_damage: Option<ManaColor>,
        #[serde(default)]
        pay_up_to_life_gained_mana_to_draw: bool,
        #[serde(default)]
        gambit_combat_directive: bool,
    },
    Animate {
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        add_types: CardTypeMask,
        #[serde(default)]
        set_types: Option<CardTypeMask>,
        #[serde(default)]
        remove_types: CardTypeMask,
        #[serde(default)]
        add_subtypes: SmallVec<[SubtypeId; 2]>,
        #[serde(default)]
        set_subtypes: Option<SmallVec<[SubtypeId; 2]>>,
        #[serde(default)]
        set_colors: Option<ColorMask>,
        #[serde(default)]
        add_colors: ColorMask,
        #[serde(default)]
        add_keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        set_power: Option<Box<Amount>>,
        #[serde(default)]
        set_toughness: Option<Box<Amount>>,
        #[serde(default)]
        keep_existing_types: bool,
        #[serde(default)]
        still_a_land: bool,
        #[serde(default)]
        remove_all_abilities: bool,
        #[serde(default)]
        granted_ability_texts: Vec<String>,
        #[serde(default)]
        choose_creature_type: bool,
        #[serde(default)]
        choose_basic_land_type: bool,
        #[serde(default)]
        choose_color: Option<AnimateColorChoice>,
    },
    Attach {
        #[serde(default)]
        attach_object: Option<Box<Selector>>,
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        attach_equipment_from_self_lki: bool,
    },
    SwitchPt {
        #[serde(default)]
        duration: Option<Duration>,
    },
    CounterSpell {
        #[serde(default)]
        replacement_zone: Option<CounterReplacementZone>,
        #[serde(default)]
        unless_pays_generic: Option<Box<Amount>>,
        #[serde(default)]
        on_countered_steal_filter: Option<Box<ObjectFilter>>,
    },
    AddMana {
        production: ManaProduction,
        #[serde(default)]
        restriction: Option<ManaRestrictionTag>,
        #[serde(default)]
        restriction_spec: Option<Box<ManaSpendingRestriction>>,
        #[serde(default)]
        retain_until_eot: bool,
    },
    RetainMana {
        /// `ColorMask::ALL_COLORS | ColorMask::C` for all.
        colors: ColorMask,
    },
    PhaseOut {
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        cant_phase_in_while_source_controlled: bool,
    },
    ExtraTurn,
    SkipTurn,
    EndTurn {
        #[serde(default)]
        active_player_may: bool,
    },
    ExtraCombatPhase,
    SkipCombatPhase,
    EndCombatPhase,
    Sacrifice {
        #[serde(default)]
        select: SelectionMod,
    },
    Win {
        #[serde(default)]
        condition: Option<Box<Condition>>,
    },
    Lose {
        #[serde(default)]
        condition: Option<Box<Condition>>,
    },
    SuspendTick,
    SuspendCast,
    DelayedTrigger {
        #[serde(default)]
        delayed: Option<DelayedTriggerPayload>,
        #[serde(default)]
        inline_delayed: Option<DelayedInlineTriggerSpec>,
        #[serde(default)]
        return_target_and_sacrificed_at_upkeep: bool,
    },
    DelayedTapManaTrigger {
        all_players: bool,
        #[serde(default)]
        land_subtype: Option<SubtypeId>,
        mana: SmallVec<[ManaColor; 2]>,
        to_tapping_player: bool,
    },
    DelayedNextSpellCopy {
        #[serde(default)]
        types: CardTypeMask,
        #[serde(default)]
        not_types: CardTypeMask,
        #[serde(default)]
        colors: ColorMask,
        copy_count: u16,
        #[serde(default)]
        may_choose_new_targets: bool,
    },
    GrantPlayer {
        #[serde(default)]
        rules: Vec<PlayerRuleKind>,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        extra_land_plays: Option<u16>,
        #[serde(default)]
        next_spell_types: Option<(CardTypeMask, CardTypeMask)>,
        #[serde(default)]
        spend_mana_as_any_color_top_of_library: bool,
        #[serde(default)]
        steal_opponent_entering_creatures_until_eot: bool,
        #[serde(default)]
        opponents_spells_cost_more_until_next_turn: Option<(bool, u16)>,
        #[serde(default)]
        attack_tax_until_your_next_turn: Option<u16>,
        #[serde(default)]
        next_spell_cost_reduction_generic: Option<Box<Amount>>,
        #[serde(default)]
        next_creature_enters_with_plus_counters: Option<Box<Amount>>,
        #[serde(default)]
        next_main_phase_add_mana: Option<(ManaColor, Box<Amount>)>,
        #[serde(default)]
        double_tokens_this_turn: bool,
        #[serde(default)]
        flash_cast_condition: Option<Box<Condition>>,
        #[serde(default)]
        next_spell_cascade: Option<u8>,
        #[serde(default)]
        extra_loyalty_activations_this_turn: Option<u8>,
        #[serde(default)]
        planar_action: Option<PlanarActionKind>,
    },
    Fight {
        with: Box<Selector>,
        #[serde(default)]
        excess_damage_to_mana: Option<ManaColor>,
    },
    Discard {
        #[serde(default)]
        select: SelectionMod,
    },
    DiscardHand {
        #[serde(default)]
        draw_that_many: bool,
        #[serde(default)]
        draw_after: Option<Box<Amount>>,
    },
    RevealAndDiscard {
        #[serde(default)]
        spec: Option<CoerciveDiscardSpec>,
        #[serde(default)]
        partial_reveal: Option<PartialRevealSpec>,
        #[serde(default)]
        coercive_count: Option<Box<Amount>>,
        #[serde(default)]
        include_graveyard: bool,
        #[serde(default)]
        discard_all_matching_sacrificed_colors: bool,
        #[serde(default)]
        discard_all_with_chosen_name: bool,
    },
    Tap {
        #[serde(default)]
        select: SelectionMod,
    },
    Untap {
        #[serde(default)]
        select: SelectionMod,
    },
    TapOrUntap,
    FreezeUntap {
        #[serde(default)]
        duration: Option<Duration>,
    },
    Transform,
    Flip,
    Convert,
    TurnFace {
        face_up: bool,
        #[serde(default)]
        select: SelectionMod,
    },
    Dig {
        spec: Box<DigSpec>,
        #[serde(default)]
        look_only: bool,
    },
    ExileUntil {
        spec: Box<ExileUntilSpec>,
    },
    PileSplit {
        spec: PileSplitSpec,
    },
    GrantPlayFromExile {
        spec: PlayFromExileSpec,
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        cast_spell_cost_tax_for_opponents: Option<u16>,
    },
    GrantGraveyardCast {
        #[serde(default)]
        spec: Option<GraveyardCastGrantSpec>,
        #[serde(default)]
        grant_adventure_until_end_of_next_turn: bool,
    },
    CastFree {
        #[serde(default)]
        spec: Option<Box<CastFreeSpec>>,
        #[serde(default)]
        at_random: bool,
        #[serde(default)]
        cast_any_number_from_exiled: bool,
    },
    Goad {
        #[serde(default)]
        duration: Option<Duration>,
    },
    Explore,
    Connive {
        #[serde(default)]
        for_each_convoking_creature_steps: Vec<EffectStep>,
    },
    Regenerate,
    CantRegenerate,
    PayCost,
    GainControl {
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        untap: bool,
    },
    Reveal {
        #[serde(default)]
        select: SelectionMod,
        #[serde(default)]
        look_only: bool,
        #[serde(default)]
        special_tag: Option<SpecialRevealTag>,
    },
    PreventDamage {
        spec: Box<DamagePreventionSpec>,
    },
    ExileInsteadOfDying {
        #[serde(default)]
        duration: Option<Duration>,
    },
    CombatRestriction {
        #[serde(default)]
        spec: Option<TimedCombatRestrictionSpec>,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        choose_odd_or_even_cant_block: bool,
        #[serde(default)]
        must_attack_next_combat: bool,
        #[serde(default)]
        must_attack_player: Option<PlayerRef>,
        #[serde(default)]
        until_your_next_turn_directive: Option<(bool, bool)>,
    },
    BecomeCopy {
        copy_source: Box<Selector>,
        #[serde(default)]
        duration: Option<Duration>,
        #[serde(default)]
        copy_mod: Option<Box<CopyModificationSpec>>,
    },
    RedirectTargets {
        spec: RedirectTargetsSpec,
    },
    CastNameLock {
        name_ref: Ref,
        #[serde(default)]
        duration: Option<Duration>,
    },
    // Keyword actions & special mechanics
    GainEnergy,
    BecomeMonarch,
    Saddle,
    CumulativeUpkeep {
        cost: Vec<CostDef>,
        #[serde(default)]
        text: Option<String>,
    },
    Exert,
    Amass {
        #[serde(default)]
        subtype: Option<SubtypeId>,
    },
    Incubate {
        #[serde(default)]
        count: Option<Box<Amount>>,
    },
    Populate,
    Manifest,
    Cloak,
    ManifestDread,
    Monstrosity,
    Bolster,
    Adapt,
    Discover,
    Clash,
    Detain,
    Suspect {
        #[serde(default)]
        unsuspect: bool,
    },
    CollectEvidence,
    Forage,
    Endure,
    TimeTravel,
    Blight,
    Fateseal,
    FlipCoin {
        #[serde(default)]
        until_lose: bool,
        #[serde(default)]
        win_effect: Vec<EffectStep>,
        #[serde(default)]
        lose_effect: Vec<EffectStep>,
    },
    RollDie {
        sides: u16,
        #[serde(default)]
        bonus: Option<Box<Amount>>,
        #[serde(default)]
        table: Vec<DieRollBranch>,
    },
    CreateEmblem {
        abilities: Vec<String>,
    },
    CreateRoleToken {
        role: RoleTokenKind,
    },
    DoubleOrTriple {
        spec: DoubleOrTripleSpec,
    },
    Exchange {
        spec: ExchangeSpec,
    },
    ChooseDesignation {
        kind: DesignationKind,
    },
    VentureIntoDungeon {
        undercity: bool,
    },
    TakeInitiative,
    TemptWithRing,
    Earthbend,
    Airbend,
    Learn,
    VillainousChoice {
        options: [Vec<EffectStep>; 2],
        labels: [String; 2],
    },
    Vote {
        spec: VoteSpec,
    },
    Prepare,
    Unprepare,
    IncreaseSpeed,
    Meld {
        partner_name: String,
        result_name: String,
        #[serde(default)]
        tapped_and_attacking: bool,
        #[serde(default)]
        partner_must_attack: bool,
    },
    Harness,
    RemoveFromCombat,
    ReselectAttackTarget,
    EmpowerJace,
    Assimilate,
    Recruit,
    Heal,
    SolveCase,
    // Structured control flow
    ForEachPlayerLoop {
        players: LoopPlayersKind,
        #[serde(default)]
        steps: Vec<EffectStep>,
        #[serde(default)]
        choose_random_opponent: bool,
        #[serde(default)]
        exclude_attacked_last_combat: bool,
        #[serde(default)]
        exclude_event_player: bool,
        #[serde(default)]
        choose_object_to_bind: Option<(Box<ObjectFilter>, bool)>,
    },
    PunisherChoice {
        spec: PunisherSpec,
    },
    PartitionKeepByTypes {
        #[serde(default)]
        types: CardTypeMask,
        #[serde(default)]
        keep_count_per_player: Option<Box<Amount>>,
        #[serde(default)]
        chooser_you: bool,
        #[serde(default)]
        all_types: bool,
        #[serde(default)]
        nonland_only: bool,
        #[serde(default)]
        put_vow_counter: bool,
    },
    RepeatSequence {
        steps: Vec<EffectStep>,
        condition: Box<Condition>,
        #[serde(default)]
        max_iterations: Option<u16>,
    },
    ConditionalSequence {
        condition: Box<Condition>,
        then_steps: Vec<EffectStep>,
        #[serde(default)]
        else_steps: Vec<EffectStep>,
    },
}
