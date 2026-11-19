use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::card::{AbilityDef, SpellCastRestrictionKind};
use crate::cost::{CostDef, CostDelta};
use crate::effect::TokenSpec;
use crate::filter::{ObjectFilter, PlayerFilter, RelativeControl, TargetSpec};
use crate::trigger::EventPattern;
use crate::types::{
    CardTypeMask, ColorMask, CounterId, KeywordId, ManaColor, ManaCost, RoundMode, SubtypeId, Zone,
    ZoneMask,
};
use crate::value::{Amount, Condition, PlayerRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Layer {
    #[serde(rename = "1")]
    Layer1,
    #[serde(rename = "2")]
    Layer2,
    #[serde(rename = "3")]
    Layer3,
    #[serde(rename = "4")]
    Layer4,
    #[serde(rename = "5")]
    Layer5,
    #[serde(rename = "6")]
    Layer6,
    #[serde(rename = "7a")]
    Layer7a,
    #[serde(rename = "7b")]
    Layer7b,
    #[serde(rename = "7c")]
    Layer7c,
    #[serde(rename = "7d")]
    Layer7d,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Affected {
    SelfObj,
    AttachedTo,
    Filter { filter: Box<ObjectFilter> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtectionAttachmentExemption {
    SelfObj,
    AllAuras,
    ControlledAuras,
    ControlledAttachments,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AbilityGrantSource {
    ExiledWithSelf {
        filter: Box<ObjectFilter>,
    },
    ExiledCardsWithCounter {
        counter: CounterId,
        #[serde(default)]
        owner: Option<RelativeControl>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    Battlefield {
        filter: Box<ObjectFilter>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayerMod {
    SetController {
        controller: PlayerRef,
    },
    Pt {
        power: Box<Amount>,
        toughness: Box<Amount>,
    },
    SetBasePt {
        power: Box<Amount>,
        toughness: Box<Amount>,
    },
    SetBasePower {
        power: Box<Amount>,
    },
    SetBaseToughness {
        toughness: Box<Amount>,
    },
    SwitchPt,
    AddKeywords {
        keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        protection_attachment_exemption: Option<ProtectionAttachmentExemption>,
    },
    RemoveKeywords {
        keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        cant_have_or_gain: bool,
    },
    RemoveAllAbilities {
        #[serde(default)]
        keep_mana_abilities: bool,
    },
    AddTypes {
        #[serde(default)]
        types: CardTypeMask,
        #[serde(default)]
        subtypes: SmallVec<[SubtypeId; 2]>,
    },
    SetTypes {
        #[serde(default)]
        types: CardTypeMask,
        #[serde(default)]
        subtypes: SmallVec<[SubtypeId; 2]>,
    },
    RemoveTypes {
        types: CardTypeMask,
    },
    SetSubtypes {
        subtypes: SmallVec<[SubtypeId; 2]>,
    },
    AllCreatureTypes,
    AddChosenSubtype,
    SetChosenSubtype,
    SetColors {
        colors: ColorMask,
    },
    AddColors {
        colors: ColorMask,
    },
    SetChosenColor,
    AddChosenColor,
    GrantAbility {
        ability: Box<AbilityDef>,
    },
    ProtectionFromChosenColor {
        #[serde(default)]
        protection_attachment_exemption: Option<ProtectionAttachmentExemption>,
    },
    ProtectionFromChosenPlayer,
    ProtectionFromChosenCardType,
    SetName {
        name: String,
    },
    GrantAbilitiesFromObjects {
        from: AbilityGrantSource,
        #[serde(default)]
        mana_as_any_color: bool,
        #[serde(default)]
        once_each_turn: bool,
        #[serde(default)]
        loyalty_only: bool,
    },
    CopyKeywordsFromExiled {
        keywords: SmallVec<[KeywordId; 4]>,
    },
    CopyStatsAndSubtypesFromLastExiled {
        #[serde(default)]
        include_subtypes: bool,
        #[serde(default)]
        keep_self_name: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpellOrAbilityFilter {
    Spell {
        filter: Box<ObjectFilter>,
        #[serde(default)]
        alt_cost_keyword: Option<KeywordId>,
    },
    Ability {
        #[serde(default)]
        keyword: Option<KeywordId>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        non_mana_only: bool,
        #[serde(default)]
        loyalty_only: bool,
        #[serde(default)]
        source_zone: Option<Zone>,
        #[serde(default)]
        self_source_only: bool,
        #[serde(default)]
        attached_source_only: bool,
        #[serde(default)]
        targets_self: bool,
        #[serde(default)]
        targets_attached: bool,
        #[serde(default)]
        targets_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        first_matching_this_turn: bool,
    },
    SelfObj,
}

/// Strongly-typed game-rule modifier replacing TS's `RuleModKind` + `Record<string, unknown>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleMod {
    NoMaxHandSize,
    MaxHandSizeDelta {
        delta: i32,
    },
    SetMaxHandSize {
        size: u16,
    },
    CantLose,
    DontLoseAtZeroOrLessLife,
    OpponentsCantWin,
    CastOnlySorcerySpeed,
    OneSpellPerTurn,
    FilteredSpellCapPerTurn {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        max: u16,
    },
    CastOnlyFromHand,
    CastOnlyDuringOwnTurn,
    ActivateOnlyDuringOwnTurn,
    CastAsThoughFlash {
        #[serde(default)]
        spells: Option<Box<ObjectFilter>>,
        #[serde(default)]
        self_only: bool,
        #[serde(default)]
        first_matching_this_turn: bool,
        #[serde(default)]
        sacrifice_at_next_cleanup_if_non_sorcery: bool,
    },
    AttackTax {
        #[serde(default)]
        generic: Option<u16>,
        #[serde(default)]
        cost: Option<ManaCost>,
        #[serde(default)]
        mana_symbol: Option<String>,
        #[serde(default)]
        dynamic_amount: Option<Box<Amount>>,
        #[serde(default)]
        affected: Option<Affected>,
        #[serde(default)]
        defending: Option<String>,
    },
    BlockTax {
        #[serde(default)]
        generic: Option<u16>,
        #[serde(default)]
        dynamic_amount: Option<Box<Amount>>,
        #[serde(default)]
        affected: Option<Affected>,
    },
    PlayerHexproof,
    PlayerShroud,
    SearchControl {
        #[serde(default)]
        exiles_found_cards: bool,
        #[serde(default)]
        grant_play_from_exile: bool,
        #[serde(default)]
        spend_mana_as_any_color: bool,
    },
    SpendManaAsAnyColor {
        #[serde(default)]
        abilities_only: bool,
        #[serde(default)]
        source_filter: Option<Box<ObjectFilter>>,
    },
    OpponentsCantCastDuringYourTurn {
        #[serde(default)]
        spells_only: bool,
    },
    LifeTotalCantChange,
    ExtraLandPlays {
        count: u16,
    },
    AllOffBattlefieldAndSpellsColorless,
    DamageCantBePrevented,
    CantGainLife,
    SkipDrawStep,
    CantCastChosenName,
    CantCastSameNameAsLinkedExile,
    CantCastMatchingSpells {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        from_zones: ZoneMask,
    },
    CantCastDuringCombat,
    CantPlayLands,
    CantDrawCards,
    CantDrawExtraCards,
    CantSearchLibraries {
        #[serde(default)]
        top_n: Option<u16>,
    },
    CantPayLifeOrSacrificeForCosts {
        #[serde(default)]
        cant_pay_life: bool,
        #[serde(default)]
        non_mana_abilities_only: bool,
        #[serde(default)]
        sacrifice_filter: Option<Box<ObjectFilter>>,
    },
    LegendRuleExempt {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    Flagbearer,
    UntapStepCap {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        max: u16,
    },
    GraveyardCantBeTargeted,
    DamageToYouAsInfect,
    AllDamageHasWither,
    SkipExtraTurns,
    PlayerCantGetCounters {
        #[serde(default)]
        counter: Option<CounterId>,
    },
    ImmuneToOpponentForcedSacrifice,
    ImmuneToOpponentForcedDiscard,
    PlayerProtectionFromChosenCardType,
    IgnoreHexproof,
    SuppressWardTriggers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RestrictionKind {
    CantAttack,
    CantBlock,
    CantAttackOrBlock,
    CantBeBlocked,
    AttacksEachCombat,
    BlocksEachCombat,
    AttacksOrBlocksEachCombat,
    CantAttackYou,
    CantAttackYouOrPlaneswalkers,
    CantBeCountered,
    DoesntUntap,
    CantCrewVehicles,
    CantHaveCounters,
    CantBeCopied,
    CantBeSacrificed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CrewSaddleScope {
    Crew,
    Saddle,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CombatRuleSpec {
    CantBlockExcept {
        filter: Box<ObjectFilter>,
    },
    AdditionalBlockCount {
        /// `None` represents unlimited additional blocks.
        count: Option<u16>,
    },
    AdditionalBlockCountDynamic {
        amount: Box<Amount>,
    },
    CantBeBlockedBy {
        filter: Box<ObjectFilter>,
    },
    CantBeBlockedByPowerLessThanSelf,
    CantBeBlockedByPowerGreaterThanSelf,
    CantBeBlockedExceptBy {
        filter: Box<ObjectFilter>,
        #[serde(default)]
        or_filter: Option<Box<ObjectFilter>>,
    },
    MaxBlockers {
        max: u16,
    },
    MinBlockers {
        min: u16,
    },
    MustBeBlockedIfAble,
    AllAbleBlockers,
    CantAttackUnlessDefenderControls {
        filter: Box<ObjectFilter>,
    },
    CantAttackAlone,
    CantBlockAlone,
    GlobalMaxAttackers {
        max: u16,
        #[serde(default)]
        against_you_only: bool,
    },
    GlobalMaxBlockers {
        max: u16,
    },
    AssignCombatDamageAsThoughUnblocked,
    CanAttackDespiteDefender,
    CantBeBlockedIfDefenderControls {
        filter: Box<ObjectFilter>,
    },
    CantBlockFilter {
        filter: Box<ObjectFilter>,
    },
    CantBlockPowerGreaterThanSelf,
    CantBlockPowerLessThanSelf,
    IsGoaded,
    IgnoreLandwalk {
        keyword: KeywordId,
    },
    CanBlockAsThoughReach {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    CrewSaddlePowerMod {
        applies_to: CrewSaddleScope,
        #[serde(default)]
        delta: Option<i32>,
        #[serde(default)]
        use_toughness: bool,
    },
    CantAttackAlreadyAttackedPlayerThisTurn,
    CantAttackIfDefenderControls {
        filter: Box<ObjectFilter>,
    },
    CantBlockIfYouControl {
        filter: Box<ObjectFilter>,
    },
    CantAttackUnlessMoreCreaturesThanDefender,
    CantBlockUnlessMoreCreaturesThanAttacker,
    CanAttackAsThoughHaste,
    MustAttackAttachedController,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompanionId {
    Lurrus,
    Yorion,
    Kaheera,
    Jegantha,
    Obosh,
    Gyruda,
    Zirda,
    Umori,
    Keruga,
    Lutri,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MultiplierTarget {
    Counters,
    Tokens,
    Triggers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotMode {
    Unlimited,
    OncePerTurn,
    PerPermanentType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticDamageAmount {
    Fixed { n: u16 },
    All,
    HalfRoundedUp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticDamageTarget {
    SelfObj,
    AttachedTo,
    You,
    YouAndControlled,
    Any,
    Filter { filter: Box<ObjectFilter> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticDamageSourceFilter {
    Filter { filter: Box<ObjectFilter> },
    BlockingOrBlockedBySelf,
    BlockingSelf,
    BlockedBySelf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhantomCounterRemoval {
    pub counter: CounterId,
    pub remove_that_many: bool,
    #[serde(default)]
    pub require_counter_present: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticOnPrevented {
    PutPlusCountersPerDamage,
    ShuffleTargetIntoLibrary,
    ReflexiveDamageTrigger { target: Box<TargetSpec> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaSpendScope {
    SelfSpell,
    SelfAbility,
    SelfBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaSourceRestrictionKind {
    Creature,
    BasicLand,
    Snow,
    Treasure,
    ChosenColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FirstActivationZeroKind {
    Equip,
    PowerUp,
    Cycling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GraveyardSelfCastKeyword {
    Blitz,
    Dash,
    Bestow,
    Mutate,
    Warp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GraveyardGrantKeyword {
    Scavenge,
    Unearth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticDef {
    pub body: StaticBody,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticBody {
    Continuous {
        layer: Layer,
        affected: Affected,
        modifier: LayerMod,
        #[serde(default)]
        active_in_zone: Option<Zone>,
    },
    CostModifier {
        affects: SpellOrAbilityFilter,
        delta: CostDelta,
        who: PlayerFilter,
        #[serde(default)]
        who_enchanted_player: bool,
        #[serde(default)]
        active_zones: ZoneMask,
    },
    RuleModifier {
        rule: RuleMod,
        who: PlayerFilter,
    },
    Restriction {
        affected: Affected,
        restriction: RestrictionKind,
    },
    Multiplier {
        what: MultiplierTarget,
        factor: u16,
        #[serde(default)]
        delta: i16,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        counter: Option<CounterId>,
        #[serde(default)]
        cause: Option<EventPattern>,
        #[serde(default)]
        cause_or: Vec<EventPattern>,
        #[serde(default)]
        round: Option<RoundMode>,
        #[serde(default)]
        applies_to_players: bool,
        #[serde(default)]
        who_places: Option<RelativeControl>,
        #[serde(default)]
        who_creates: Option<RelativeControl>,
        #[serde(default)]
        extra_token: Option<Box<TokenSpec>>,
        #[serde(default)]
        extra_token_matching_count: bool,
        #[serde(default)]
        trigger_source_attached_to: bool,
    },
    PregameBeginOnBattlefield,
    CombatRule {
        affected: Affected,
        rule: CombatRuleSpec,
    },
    CantActivateAbilities {
        affected: Affected,
        #[serde(default)]
        allow_mana_abilities: bool,
    },
    TopOfLibraryVisibility {
        reveal_to_all: bool,
        all_players: bool,
    },
    ZonePlayPermission {
        zone: Zone,
        #[serde(default)]
        allow_lands: bool,
        #[serde(default)]
        allow_spells: bool,
        #[serde(default)]
        spell_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        self_only: bool,
        #[serde(default)]
        during_your_turn_only: bool,
        #[serde(default)]
        slot_mode: Option<SlotMode>,
        #[serde(default)]
        exile_if_put_into_graveyard: bool,
        #[serde(default)]
        exiled_with_self: bool,
        #[serde(default)]
        require_counter: Option<CounterId>,
        #[serde(default)]
        card_owner: Option<RelativeControl>,
        #[serde(default)]
        spend_mana_as_any_color: bool,
        #[serde(default)]
        enters_tapped: bool,
        #[serde(default)]
        grant_haste_until_eot: bool,
        #[serde(default)]
        put_into_graveyard_from_non_battlefield_this_turn: bool,
        #[serde(default)]
        alt_cost_defs: Vec<CostDef>,
        #[serde(default)]
        grant_flash: bool,
    },
    AdditionalCastCost {
        costs: Vec<CostDef>,
    },
    AltCastPermission {
        mana_cost: ManaCost,
        #[serde(default)]
        cost_defs: Vec<CostDef>,
        affected: Affected,
        #[serde(default)]
        allowed_zones: ZoneMask,
        #[serde(default)]
        once_each_turn: bool,
        #[serde(default)]
        during_your_turn_only: bool,
        #[serde(default)]
        grant_flash: bool,
        #[serde(default)]
        any_player: bool,
        #[serde(default)]
        dynamic_mana_cost_from_mv: bool,
        #[serde(default)]
        max_mv_by_self_counters: Option<CounterId>,
        #[serde(default)]
        max_mv_by_controlled_count: Option<Box<ObjectFilter>>,
        #[serde(default)]
        enters_with_counters: Vec<(CounterId, u16)>,
        #[serde(default)]
        additional_only: bool,
        #[serde(default)]
        additional_per_metric: Option<Box<Amount>>,
        #[serde(default)]
        require_zone_permission: bool,
        #[serde(default)]
        pay_life_equal_to_mv: bool,
        #[serde(default)]
        slot_mode: Option<SlotMode>,
    },
    MayChooseNotToUntap {
        affected: Affected,
    },
    AssignCombatDamageByToughness {
        affected: Affected,
    },
    CommanderDesignation,
    DeckCopyLimit {
        /// `None` means any number of copies (e.g. Relentless Rats).
        limit: Option<u16>,
    },
    StaticDamagePrevention {
        #[serde(default)]
        combat_only: bool,
        #[serde(default)]
        noncombat_only: bool,
        amount: StaticDamageAmount,
        target: StaticDamageTarget,
        #[serde(default)]
        also_prevent_dealt_by: bool,
        #[serde(default)]
        dealt_by_only: Option<Affected>,
        #[serde(default)]
        dealt_by_target_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        source_filter: Option<StaticDamageSourceFilter>,
        #[serde(default)]
        phantom_counter_removal: Option<PhantomCounterRemoval>,
        #[serde(default)]
        on_prevented: Option<StaticOnPrevented>,
        #[serde(default)]
        prevent_opponent_only: bool,
    },
    EntersPrepared,
    CompanionCondition {
        companion: CompanionId,
    },
    Offering {
        filter: Box<ObjectFilter>,
        quality: String,
    },
    SelfCastRestriction {
        #[serde(default)]
        restrictions: Vec<SpellCastRestrictionKind>,
        #[serde(default)]
        cast_condition: Option<Box<Condition>>,
        #[serde(default)]
        x_cant_be_zero: bool,
    },
    LoyaltyActivationsPerTurn {
        max: u8,
    },
    HandVisibility {
        players: RelativeControl,
    },
    RetainUnspentMana {
        all_players: bool,
        /// `ColorMask::ALL_COLORS | ColorMask::C` for all.
        colors: ColorMask,
        #[serde(default)]
        convert_to: Option<ManaColor>,
    },
    UntapDuringOtherUntapSteps {
        affected: Affected,
    },
    OffBattlefieldCharacteristics {
        zones: ZoneMask,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        all_players: bool,
        #[serde(default)]
        add_all_creature_types: bool,
        #[serde(default)]
        add_chosen_subtype: bool,
        #[serde(default)]
        add_types: CardTypeMask,
        #[serde(default)]
        add_subtypes: SmallVec<[SubtypeId; 2]>,
        #[serde(default)]
        add_chosen_color: bool,
        #[serde(default)]
        add_colors: ColorMask,
    },
    SpellGrantKeyword {
        filter: Box<ObjectFilter>,
        keywords: SmallVec<[KeywordId; 2]>,
        #[serde(default)]
        evoke_cost: Option<ManaCost>,
        #[serde(default)]
        cascade_count: Option<u8>,
        #[serde(default)]
        param_n: Option<u16>,
    },
    ActivateAsThoughHaste {
        filter: Box<ObjectFilter>,
    },
    CantBeTargetedBy {
        affected: Affected,
        #[serde(default)]
        source_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        spells_only: bool,
        #[serde(default)]
        abilities_only: bool,
        #[serde(default)]
        opponent_controller_only: bool,
    },
    TapForManaMultiplier {
        factor: u16,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        all_players: bool,
    },
    DraftNoOp,
    ManaSpendRestriction {
        scope: ManaSpendScope,
        #[serde(default)]
        x_colors: ColorMask,
        #[serde(default)]
        x_colored_only: bool,
        #[serde(default)]
        x_max_one_per_color: bool,
        #[serde(default)]
        source_kind: Option<ManaSourceRestrictionKind>,
    },
    DefilerCostReduction {
        color: ManaColor,
        life_amount: u16,
        filter: Box<ObjectFilter>,
    },
    FirstActivationZeroCost {
        ability_keyword: FirstActivationZeroKind,
        #[serde(default)]
        during_your_turn_only: bool,
    },
    CommanderTaxAsLife {
        life_per_cast: u16,
    },
    CastFromLibraryWhileSearching,
    SpaceSculptorSectors,
    CommanderPreGameColorChoice,
    StaticDamageRedirection {
        include_other_controlled: bool,
        to_attached: bool,
    },
    UnpreventableDamage {
        #[serde(default)]
        combat_only: bool,
        #[serde(default)]
        source_filter: Option<Affected>,
    },
    CantHaveCounterType {
        affected: Affected,
        #[serde(default)]
        counter: Option<CounterId>,
    },
    CountersRemainInNonHiddenZones,
    DamageNotRemovedInCleanup {
        affected: Affected,
    },
    MayLookAtExiledWithSelf,
    SelfGraveyardKeywordCast {
        keyword: GraveyardSelfCastKeyword,
    },
    GrantGraveyardEscape {
        filter: Box<ObjectFilter>,
        exile_count: u16,
    },
    GrantGraveyardKeywordAbility {
        keyword: GraveyardGrantKeyword,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        cost_override: Option<ManaCost>,
    },
    GrantHandForetell {
        filter: Box<ObjectFilter>,
        reduce_generic: u16,
    },
    LandsCantEnterBattlefield,
    DynamicWardMana {
        affected: Affected,
        generic: Box<Amount>,
    },
    CantEnterFromZones {
        filter: Box<ObjectFilter>,
        zones: ZoneMask,
        #[serde(default)]
        who: Option<RelativeControl>,
    },
    TokenReplacement {
        one_of_each: bool,
        #[serde(default)]
        match_subtypes: SmallVec<[SubtypeId; 2]>,
        #[serde(default)]
        replacement_token: Option<Box<TokenSpec>>,
    },
}
