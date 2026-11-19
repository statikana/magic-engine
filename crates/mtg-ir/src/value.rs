use serde::{Deserialize, Serialize};

use crate::filter::{ObjectFilter, RelativeControl};
use crate::types::{
    ColorMask, CompareOp, CounterId, CounterTarget, ManaColor, PlayerCounterKind, RoundMode,
    StatKind, SubtypeId, Zone,
};

/// Object reference resolved at runtime (CR 608.2h LKI-aware).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Ref {
    SelfObj,
    Target { index: u16 },
    Binding { name: String },
    EventObject,
    Affected,
    AttachedTo,
    LinkedExile,
}

/// Player reference resolved at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerRef {
    You,
    ControllerOf { of: Box<Ref> },
    OwnerOf { of: Box<Ref> },
    Target { index: u16 },
    ActivePlayer,
    EventPlayer,
    DefendingPlayer,
    ChosenOpponent,
    EachLoopPlayer,
    PlayerToYourRight,
    ControllerOrControllerOfPlaneswalker { target_index: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ZoneScopeWhose {
    You,
    Opponents,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerCounterScope {
    You,
    Opponents,
    All,
    DefendingPlayer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectColorTarget {
    SelfObj,
    AttachedTo,
    Affected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachmentOwnerTarget {
    SelfObj,
    AttachedTo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SacrificedStatKind {
    Power,
    Toughness,
    ManaValue,
    ColorsCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CountedThisWayKind {
    ExiledFromHand,
    DamagePrevented,
    GreatestDiscarded,
    ManaPaidJoinForces,
    TappedThisWay,
    DestroyedThisWay,
    SacrificedThisWay,
    DiscardedThisWay,
    ExiledThisWay,
    MilledThisWay,
    MilledTotalManaValue,
    OpponentsDrewThisWay,
    PunisherDeclinedThisWay,
    PunisherPaidThisWay,
    DiscardedHandPlayersThisWay,
    DamageDealtThisWay,
    OpponentsDidThisWay,
    OpponentsDidntThisWay,
    NonlandRevealedThisWay,
    LandCardsPutIntoGraveyardThisWay,
    DestroyedControlledByPlayerThisWay,
    CreaturesDealtDamageThisWay,
    DrawnThisWay,
    ReturnedToHandThisWay,
    ExiledCardsThisWay,
    LandCardsMilledThisWay,
    ConvokedCreaturesCount,
    RevealedThisWay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpellsCastWho {
    You,
    ActivePlayer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostPaidCountKind {
    Exiled,
    Sacrificed,
    RemovedCounters,
}

/// Symbolic integer expression evaluated against live or LKI game state.
///
/// All TS Wave wrappers (`p17bMetric`, `p20bCostPaidCount`, etc.) are flattened into first-class
/// domain variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Amount {
    Const {
        n: i32,
    },
    X,
    AnyNumber,
    Count {
        of: Box<ObjectFilter>,
    },
    Stat {
        of: Ref,
        stat: StatKind,
        #[serde(default)]
        lki: bool,
    },
    Devotion {
        colors: ColorMask,
        #[serde(default)]
        player: Option<PlayerRef>,
    },
    ResultCount {
        of: String,
    },
    EventAmount,
    CardsIn {
        zone: Zone,
        of: PlayerRef,
    },
    Life {
        of: PlayerRef,
    },
    CommanderCastCount,
    KickedCount {
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        multiplier: Option<i32>,
    },
    StormCount,
    GravestormCount,
    Negate {
        of: Box<Amount>,
    },
    Sum {
        args: Vec<Amount>,
    },
    Min {
        args: Vec<Amount>,
    },
    Max {
        args: Vec<Amount>,
    },
    Mul {
        factor: i32,
        of: Box<Amount>,
    },
    Half {
        of: Box<Amount>,
        round: RoundMode,
    },
    Difference {
        left: Box<Amount>,
        right: Box<Amount>,
    },
    Conditional {
        condition: Box<Condition>,
        then_amt: Box<Amount>,
        else_amt: Box<Amount>,
    },

    EnergyCount {
        player: PlayerRef,
    },
    PlayerCounters {
        counter: PlayerCounterKind,
        player: PlayerRef,
        #[serde(default)]
        players: Option<PlayerCounterScope>,
    },
    PlayerCounterDifference {
        player: PlayerRef,
        counter: PlayerCounterKind,
        target: i32,
    },
    CountersOn {
        counter: CounterTarget,
        of: Ref,
    },
    LkiCounterCount {
        counter: CounterTarget,
    },
    LkiTotalCounters {
        of: Ref,
    },
    CountersRemovedThisWay,
    DistinctCounterKindsOn {
        of: Ref,
    },
    TotalCountersAmong {
        filter: Box<ObjectFilter>,
    },
    CountersAmong {
        filter: Box<ObjectFilter>,
        counter: CounterId,
    },
    CountersAmongControlled {
        filter: Box<ObjectFilter>,
        player: PlayerRef,
    },
    CountersAmongPlayersAndPermanents {
        #[serde(default)]
        counter: Option<CounterTarget>,
    },

    TappedCostPower,
    ColorsOfManaSpentToCast,
    ManaSpentToCast,
    DistinctCardTypesInZone {
        zone: Zone,
        whose: ZoneScopeWhose,
    },
    LastRollResult,
    CoinHeadsCount,
    DevouredCount,
    OpponentCount,
    Domain {
        player: PlayerRef,
    },
    PartyCount {
        player: PlayerRef,
    },
    AttachmentsOn {
        target: AttachmentOwnerTarget,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    UnlockedDoorsCount {
        player: PlayerRef,
    },
    ExcessDamage,
    LastEffectAmount,
    ColorsAmongPermanents {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        player: PlayerRef,
    },
    DungeonsCompleted {
        player: PlayerRef,
    },
    GreatestStatAmong {
        filter: Box<ObjectFilter>,
        stat: StatKind,
    },
    LeastStatAmong {
        filter: Box<ObjectFilter>,
        stat: StatKind,
    },
    TotalStatAmong {
        filter: Box<ObjectFilter>,
        stat: StatKind,
    },
    GreatestStatAmongControlledAsCast {
        stat: StatKind,
        filter: Box<ObjectFilter>,
    },
    GreatestBaseToughnessControlled {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    GreatestControlledCount {
        filter: Box<ObjectFilter>,
    },
    TargetsBeyondFirst,
    ColorsOfObject {
        of: ObjectColorTarget,
    },
    PlayerSpeed {
        player: PlayerRef,
    },

    // Flattened metrics (replacing TS `p17bMetric` / `p20bCostPaidCount`)
    DistinctManaValuesInGraveyard,
    DistinctCreatureTypesControlled,
    DistinctLandNamesControlled,
    DistinctNamesControlled,
    DistinctNamesCount {
        of: Box<ObjectFilter>,
    },
    DistinctColorsAmongControlled,
    DistinctPowersControlled,
    LifeGainedThisTurn,
    StartingLifeDifference,
    CreaturesAttackedWithThisTurn,
    CreaturesAttackedTotalThisTurn,
    OpponentsDealtDamageThisTurn,
    CommanderColorIdentityCount,
    LandsOpponentControlsProducingColorless,
    SameNameInGraveyardAsSpell,
    TargetsCount,
    TargetsMatchingCount {
        filter: Box<ObjectFilter>,
    },
    TargetColorsCount,
    TargetCounterCount {
        #[serde(default)]
        counter: Option<CounterTarget>,
    },
    TargetPower,
    TargetsTotalManaValue,
    TotalManaValueOfTargetSpec {
        index: u16,
    },
    CostPaidCount {
        kind: CostPaidCountKind,
    },
    SacrificedCostStat {
        stat: SacrificedStatKind,
    },
    RevealedCostStat {
        stat: StatKind,
    },

    ChosenPlayerZoneCount {
        zone: Zone,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    OpponentsWithLifeLessThanHalfStarting,
    CountedThisWay {
        kind: CountedThisWayKind,
    },
    CreaturesDiedThisTurn {
        #[serde(default)]
        controller: Option<RelativeControl>,
    },
    NontokenCreaturesDiedThisTurn {
        #[serde(default)]
        controller: Option<RelativeControl>,
    },
    SpellsCastThisTurnCount {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        who: Option<SpellsCastWho>,
    },
    OpponentsLifeLostThisTurn,
    CardsDiscardedThisTurn,
    OpponentsControllingMoreCount {
        filter: Box<ObjectFilter>,
    },
    OpponentsControllingFilterCount {
        filter: Box<ObjectFilter>,
    },
    OpponentsWithHandMaxCount {
        max: u16,
    },
    OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn {
        subtype: SubtypeId,
    },
    AttackingCountToPlayer {
        player: PlayerRef,
    },
    FractionalLifeLoss {
        player: PlayerRef,
        denominator: u16,
        round: RoundMode,
    },
    HandSizeDifference {
        #[serde(default)]
        target_player: Option<PlayerRef>,
        #[serde(default)]
        cap: Option<i32>,
    },
    TokensCreatedThisTurn {
        #[serde(default)]
        player: Option<PlayerRef>,
    },
    CardsPutIntoGraveyardFromHandOrLibraryThisTurn {
        #[serde(default)]
        player: Option<PlayerRef>,
    },
    DamageDealtToOpponentsThisTurn,
    DamageDealtToSelfByPlayerThisTurn {
        player: PlayerRef,
    },
    PlayersLostLifeThisTurnCount,
    RingBearerPower {
        #[serde(default)]
        player: Option<PlayerRef>,
    },
    SacrificedPermsThisTurn,
    GreatestOpponentCardsDrawnThisTurn,
    ExiledCardsSharingTypeCount {
        #[serde(default)]
        owner: Option<RelativeControl>,
    },
    TimesAttackedThisTurn {
        #[serde(default)]
        of: Option<Ref>,
    },
    UnspentManaTotal {
        #[serde(default)]
        player: Option<PlayerRef>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpponentComparisonMetric {
    Lands,
    Creatures,
    Life,
    Hand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OpponentMetricKind {
    Life,
    Hand,
    Graveyard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpellsLastTurnMode {
    None,
    SinglePlayerAtLeast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NamedAltCostKind {
    Surge,
    Prowl,
    Spectacle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LifeCompareWho {
    You,
    AnyOpponent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaSourceKind {
    Treasure,
}

/// Boolean condition evaluated for intervening-if triggers, static conditions, and step gates.
///
/// All TS Wave wrappers (`p17bCondition`, `p20bCondition`) are flattened into first-class variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Condition {
    Compare {
        left: Box<Amount>,
        op: CompareOp,
        right: Box<Amount>,
    },
    And {
        of: Vec<Condition>,
    },
    Or {
        of: Vec<Condition>,
    },
    Not {
        of: Box<Condition>,
    },

    GraveyardAbove {
        filter: Box<ObjectFilter>,
        count: u16,
    },
    IsTurnOf {
        player: PlayerRef,
    },
    IsYourTurn,
    NotYourTurn,
    NotStartingPlayer,
    NotSpellControllerTurn,
    NotEventPlayerTurn,
    IsYourMainPhase,
    ControlCommander,
    Kicked {
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        min_count: Option<u16>,
    },
    Evoked,
    Escaped,
    Foretold,
    Blitzed,
    Saddled,
    ExertedThisTurn,
    Monstrous,
    GiftPromised,
    TributePaid,
    IsPaired,
    HasEnduringStory,
    IsHarnessed,
    IsSuspended,
    IsRenowned,
    SelfPrepared,
    IsImpending,
    IsDay,
    IsNight,
    HasCompletedDungeon,
    HasInitiative,
    HasCitysBlessing,
    HasMaxSpeed,
    IsMonarch {
        #[serde(default)]
        player: Option<PlayerRef>,
        #[serde(default)]
        relation: Option<RelativeControl>,
    },

    EvolveCompare,
    MatchesFilter {
        #[serde(default)]
        target: Option<Ref>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        any: bool,
    },
    ControlsFilter {
        filter: Box<ObjectFilter>,
    },
    PlayerControlsFilter {
        player: PlayerRef,
        filter: Box<ObjectFilter>,
    },
    ObjectState {
        of: AttachmentOwnerTarget,
        #[serde(default)]
        tapped: Option<bool>,
        #[serde(default)]
        attacking: Option<bool>,
        #[serde(default)]
        blocking: Option<bool>,
        #[serde(default)]
        monstrous: Option<bool>,
        #[serde(default)]
        saddled: Option<bool>,
        #[serde(default)]
        equipped: Option<bool>,
        #[serde(default)]
        enchanted: Option<bool>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    AttachedPermanentMatches {
        #[serde(default)]
        colors: ColorMask,
        #[serde(default)]
        untapped: Option<bool>,
    },

    CreatureDiedThisTurn,
    CreatureDiedUnderYourControlThisTurn,
    AttackedThisTurn,
    AttackedWithCommanderThisTurn,
    PermanentLeftBattlefieldThisTurn,
    GainedLifeThisTurn {
        #[serde(default)]
        min: Option<Box<Amount>>,
    },
    OpponentGainedLifeThisTurn,
    YourTeamGainedLifeThisTurn,
    WasCast {
        #[serde(default)]
        from_hand: bool,
    },
    OpponentCount {
        op: CompareOp,
        count: u16,
    },
    AnyPlayerLife {
        op: CompareOp,
        count: i32,
    },
    AnyOpponentMetric {
        metric: OpponentMetricKind,
        op: CompareOp,
        count: i32,
    },
    DistinctControlledCreaturePowers {
        count: u16,
    },
    CameUnderControlSinceLastUpkeep,
    NinjutsuWindow,
    LkiHasNoCounter {
        counter: CounterId,
    },
    HadCounterOnIt {
        counter: CounterTarget,
    },
    LkiToughnessLessThan {
        max: i32,
    },
    OpponentLostLifeThisTurn,
    YouLostLifeThisTurn,
    PlayerLostLifeThisTurn {
        player: PlayerRef,
    },
    PlayerLostLifeAtLeastThisTurn {
        min: i32,
        #[serde(default)]
        opponent_only: bool,
    },
    PlayerLostLifeLastTurn,
    DescendedThisTurn,
    ManaColorSpentToCast {
        #[serde(default)]
        color: Option<ManaColor>,
        #[serde(default)]
        min_total: Option<u16>,
    },
    SameColorManaSpentAtLeast {
        min: u16,
        #[serde(default)]
        color: Option<ManaColor>,
    },
    TreasureManaSpentToCast,
    ManaSpentGreaterThanSelfPower,
    OpponentComparison {
        metric: OpponentComparisonMetric,
    },
    OpponentControlsAtLeastNMore {
        filter: Box<ObjectFilter>,
        n: u16,
    },
    OpponentHasMoreCardsInHandThan {
        who: PlayerRef,
    },
    MoreCardsInHandThanEachOpponent,
    YouHaveMostLife,
    YourLifeLessThanStarting,
    LifeAtLeastAboveStarting {
        delta: i32,
    },
    LifeCompareHalfStarting {
        who: LifeCompareWho,
        op: CompareOp,
    },

    ClassLevelIs {
        level: u8,
    },
    ClassLevelAtLeast {
        level: u8,
    },
    CaseSolved,
    CaseUnsolved,
    BoastAttackedThisTurn,
    ExhaustUnused {
        ability_id: String,
    },
    TrainingGreaterAttacker,
    DethroneMostLife,
    CreaturesAttackedCountThisTurn {
        min: u16,
    },
    TotalPowerControlled {
        min: i32,
    },
    ColorsAmongControlledPermanents {
        min: u8,
    },
    SourcesDealtDamageThisTurn {
        min: u16,
    },
    CardsPutIntoGraveyardFromAnywhereThisTurn {
        min: u16,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        whose: Option<RelativeControl>,
    },
    SpellsCastThisTurnAtLeast {
        min: u16,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    SelfInZone {
        zone: Zone,
    },
    SpellsCastLastTurn {
        mode: SpellsLastTurnMode,
        count: u16,
    },
    OpponentPoisonAtLeast {
        min: u16,
    },
    NonlandEnteredThisTurn {
        min: u16,
    },
    VoidThisTurn,
    CardsDrawnThisTurnAtLeast {
        min: u16,
    },
    NthMatchingSpellThisTurn {
        ordinal: u8,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    EnteredWithXAtLeast {
        n: i32,
    },
    AttackingTotalPowerAtLeast {
        min: i32,
    },
    AttackingCount {
        op: CompareOp,
        n: u16,
    },
    AttackingYouCount {
        op: CompareOp,
        n: u16,
    },
    FirstCombatPhaseOfTurn,
    EventPlayerHandCount {
        op: CompareOp,
        count: u16,
    },
    CardLeftYourGraveyardThisTurn,
    CommittedCrimeThisTurn,
    ControlledByOtherThanOwner,
    EventObjectMatchedLki {
        filter: Box<ObjectFilter>,
    },
    AnchorWordChosen {
        word: String,
    },
    YouWereAttackedThisCombat,
    DiedThisWay {
        #[serde(default)]
        of: Option<Ref>,
    },
    MostCommonColorAmongPermanents {
        color: ManaColor,
        allow_tie: bool,
    },
    NoSpellCastFromHandThisTurn,
    NoSpellCastThisTurn,
    ChoseOtherRingBearer,
    EnteredUnderYourControlThisTurn {
        filter: Box<ObjectFilter>,
        #[serde(default)]
        min: Option<u16>,
    },
    PaidNamedAltCost {
        kind: NamedAltCostKind,
    },
    AltCostPaid {
        #[serde(default)]
        mana_cost: Option<String>,
    },
    AdditionalCostPaid {
        #[serde(default)]
        mana_cost: Option<String>,
    },
    NotBeingDeclaredAsAttacker,
    SelfDealtDamageToOpponentThisTurn,
    DamageDealtToSelfThisTurnAtLeast {
        min: i32,
    },
    EventObjectEnteredThisTurn,
    SelfEnteredThisTurn,
    DistinctManaValuesInGraveyardAtLeast {
        min: u16,
    },
    AnyGraveyardAtLeast {
        min: u16,
    },
    ControlledTargetAtResolution {
        target_index: u16,
    },
    AbilityActivatedCountThisTurn {
        min: u16,
    },
    IsAttackingAlone {
        of: AttachmentOwnerTarget,
    },
    HasntDealtDamageYet {
        of: AttachmentOwnerTarget,
    },
    YourTurnsTakenAtLeast {
        min: u16,
        #[serde(default)]
        only_during_your_turn: bool,
    },
    YouDiscardedThisTurn,
    DiscardedCardThisTurn {
        who: RelativeControl,
    },
    NoMatchingCardsInHand {
        filter: Box<ObjectFilter>,
    },
    OnlyNamedOrNoMatchingInHand {
        filter: Box<ObjectFilter>,
    },
    SpellWasBargained,
    AbilityTargetsFilter {
        filter: Box<ObjectFilter>,
    },
    TargetsStackSpell {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    OpponentCastSpellsAtLeast {
        n: u16,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    OpponentHadPermEnteredThisTurn {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    OpponentDrewCardsAtLeastThisTurn {
        min: u16,
    },
    OpponentCardsPutIntoGraveyardAtLeastThisTurn {
        min: u16,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    OpponentSearchedLibraryThisTurn,
    YourCreatureSpellCounteredByOpponentThisTurn,
    YourNoncreatureDestroyedByOpponentThisTurn,
    DealtDamageByCreaturesAtLeastThisTurn {
        min: i32,
    },

    EventAbilityIsNotMana,
    EventAbilityLifePaid,
    EventSpellWasKicked,
    EventSpellHasKeyword {
        keyword: String,
    },
    EventSpellNoColoredManaSpent,
    EventSpellManaSpentLessThanMv,
    EventEnteredOrCastFromZone {
        zone: Zone,
    },
    EventEnteredOrCastFromGraveyardAny,
    EventObjectPowerOrToughnessGreaterThanSelf,
    EventObjectWasNotSubtype {
        subtype: SubtypeId,
    },
    EventWasAttachedEnchantedOrEquipped,
    EventCrewCountEquals {
        count: u16,
    },
    EventDamageDealtBySubtype {
        subtype: SubtypeId,
    },
    EventDamagedTargetDealtExcessDamageThisTurn,
    EventAttackingPlayerWithMostLife,
    EventAttackerHasAnotherUnattackedOpponent,
    EventMinAttackersToYouOrPlaneswalkers {
        min: u16,
    },
    EventSharesCardTypeWithLinkedExile,
    EventPlayerControlsMoreThanYou {
        filter: Box<ObjectFilter>,
    },
    EventPlayerControlsMoreThanEachOther {
        filter: Box<ObjectFilter>,
    },
    EventObjectGreatestPowerAmongAll,
    EventSpellMatchesFilter {
        filter: Box<ObjectFilter>,
    },
    EventObjectMatchesOrSubtype {
        filter: Box<ObjectFilter>,
    },

    PutCounterOnCreatureThisTurn {
        who: RelativeControl,
    },
    SacrificedFilterThisTurn {
        filter: Box<ObjectFilter>,
        min: u16,
        who: RelativeControl,
    },
    NoCreaturesAttackedThisTurn,
    CastCreatureAndNoncreatureThisTurn,
    EnchantedCreaturePowerAtLeast {
        min: i32,
    },
    AllLandsAreSubtype {
        subtype: SubtypeId,
    },
    YouControlGreatestManaValueFilter {
        filter: Box<ObjectFilter>,
    },
    YourTeamControlsOtherFilter {
        filter: Box<ObjectFilter>,
    },
    PlayersLostGameAtLeast {
        min: u16,
    },
    SelfAttackedThisCombat,
    SelfDidntAttackThisTurn,
    SelfHaventAddedManaThisTurn,
    ZoneChangeFromToThisTurn {
        #[serde(default)]
        from: Option<Zone>,
        to: Zone,
        filter: Box<ObjectFilter>,
        min: u16,
    },
    CounterPutOnSelfThisTurn,
    PlayerHadAnotherLandEnterThisTurn,
    HasCardsExiledWithSelf,
    NoSameNameControlledOrInGraveyard,
    CountersAmongFilterAtLeast {
        filter: Box<ObjectFilter>,
        min: u16,
    },
    DidntActivateLoyaltyThisTurn,
    NotPutOntoBattlefieldByThisAbility,
    AnyAttackedPlayerWithMostLife,
    WasDealtCombatDamageBySelfNameThisGame {
        player: PlayerRef,
    },
    PlayerDidntDiscardNonlandThisWay,
    PlayerDiscardedHandThisWay,
    CreatedTokenThisTurn,
    SameNameLandsControlledAtLeast {
        min: u16,
    },
    PlayedLandThisTurn,
    SourceHasCounter {
        counter: CounterId,
        #[serde(default)]
        min: Option<u16>,
    },
    DistinctCardTypesAmongSacrificedAtLeast {
        min: u16,
    },
    CreatureCardMilledThisWay,
    LandCardMilledThisWay,
    ReturnedFilterToHandThisWay {
        filter: Box<ObjectFilter>,
    },
    DiscardedNonlandThisWay,
    DiscardedCreatureThisWay,
    DiscardedNoncreatureThisWay,
    PlayerHasFewerHandThan {
        #[serde(default)]
        target_player: Option<PlayerRef>,
        #[serde(default)]
        max: Option<u16>,
    },
    PlayerHasFewerCountersThan {
        counter: PlayerCounterKind,
        max: u16,
    },
    TargetHasFewerCountersThan {
        counter: PlayerCounterKind,
        max: u16,
    },
    SpellCastFromZone {
        zone: Zone,
    },
    NotDrawnCardsThisWay,
    ExiledCountAtLeast {
        min: u16,
    },
    DidRevealThisWay,
    RingBearerAndTemptedAtLeast {
        min: u8,
    },
    WonClashThisWay,
    SacrificedNonlandThisWay,
    DidntPutOntoBattlefieldThisWay,
    CastTriggerCreatureSpell,
    ControlAttackingLegendary,
    AllPlayersCantCastCheck,
    SelfRemovedFromCombatOrAttached,
    DidLastOptionalStep,
    SourceExiledThisWay,
    FirstSpellThisTurnWithManaFrom {
        mana_source: ManaSourceKind,
    },
}
