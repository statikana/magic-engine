use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::types::{
    CardTypeMask, ColorMask, CompareOp, CounterTarget, KeywordId, StackTargetKind, StatKind,
    SubtypeId, SupertypeMask, Zone, ZoneMask,
};
use crate::value::{Amount, Condition, PlayerRef};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    pub op: CompareOp,
    pub value: Box<Amount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerRelation {
    You,
    Opponent,
    EachOpponent,
    EachPlayer,
    Any,
    Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerFilter {
    pub relation: PlayerRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelativeControl {
    You,
    Opponent,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParityFilter {
    Odd,
    Even,
    ChosenQuality,
    NotChosenQuality,
    ChosenParity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachedHostRef {
    SelfObj,
    AttachedTo,
    HostOfSelf,
    ReanimatedBySelf,
}

bitflags! {
    /// Fast-path boolean flags on [`ObjectFilter`].
    ///
    /// Positive/negative tri-state properties (e.g. tapped vs untapped) use separate bits so
    /// an unspecified property leaves both bits clear.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct FilterFlags: u64 {
        const OTHER                        = 1 << 0;
        const TAPPED                       = 1 << 1;
        const UNTAPPED                     = 1 << 2;
        const TOKEN                        = 1 << 3;
        const NONTOKEN                     = 1 << 4;
        const ATTACKING                    = 1 << 5;
        const NOT_ATTACKING                = 1 << 6;
        const BLOCKING                     = 1 << 7;
        const NOT_BLOCKING                 = 1 << 8;
        const IS_COMMANDER                 = 1 << 9;
        const NOT_COMMANDER                = 1 << 10;
        const MULTICOLORED                 = 1 << 11;
        const NOT_MULTICOLORED             = 1 << 12;
        const MONOCOLORED                  = 1 << 13;
        const NOT_MONOCOLORED              = 1 << 14;
        const IS_COLORED                   = 1 << 15;
        const IS_COLORLESS                 = 1 << 16;
        const FACE_DOWN                    = 1 << 17;
        const FACE_UP                      = 1 << 18;
        const HISTORIC                     = 1 << 19;
        const MODIFIED                     = 1 << 20;
        const NOT_MODIFIED                 = 1 << 21;
        const OUTLAW                       = 1 << 22;
        const EQUIPPED                     = 1 << 23;
        const NOT_EQUIPPED                 = 1 << 24;
        const ENCHANTED                    = 1 << 25;
        const NOT_ENCHANTED                = 1 << 26;
        const ENCHANTED_OR_EQUIPPED        = 1 << 27;
        const HAS_ANY_COUNTER              = 1 << 28;
        const HAS_NO_COUNTER               = 1 << 29;
        const ATTACKING_OR_BLOCKING        = 1 << 30;
        const UNBLOCKED_ATTACKER           = 1 << 31;
        const ENTERED_THIS_TURN            = 1 << 32;
        const NOT_ENTERED_THIS_TURN        = 1 << 33;
        const DEALT_DAMAGE_THIS_TURN       = 1 << 34;
        const SADDLED                      = 1 << 35;
        const MONSTROUS                    = 1 << 36;
        const NOT_MONSTROUS                = 1 << 37;
        const SUSPECTED                    = 1 << 38;
        const NOT_SUSPECTED                = 1 << 39;
        const PREPARED                     = 1 << 40;
        const NOT_PREPARED                 = 1 << 41;
        const RENOWNED                     = 1 << 42;
        const NOT_RENOWNED                 = 1 << 43;
        const GOADED                       = 1 << 44;
        const HAS_ADVENTURE                = 1 << 45;
        const HAS_X_IN_MANA_COST           = 1 << 46;
        const KICKED_SPELL                 = 1 << 47;
        const BARGAINED_SPELL              = 1 << 48;
        const WAS_NOT_CAST                 = 1 << 49;
        const CAST_OUTSIDE_HAND            = 1 << 50;
        const WITH_CYCLING_ABILITY         = 1 << 51;
        const ARTIFACT_ENCHANTMENT_OR_TOKEN = 1 << 52;
    }
}

/// Rarer relational/dynamic predicates on [`ObjectFilter`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectPredicate {
    Named {
        name: String,
    },
    ControllerRef {
        player: PlayerRef,
    },
    OwnerRef {
        player: PlayerRef,
    },
    ControllerAndNotOwner {
        relation: RelativeControl,
    },
    SameOwnerAsSource,
    ControlledByEnchantedPlayer,
    ControlledByChosenPlayer,
    ControllerOfTarget {
        index: u16,
    },
    ControllerOfEventObject,
    ControllerDealtCombatDamageBySelfThisTurn,
    DifferentControllers,

    HasKeywords {
        keywords: SmallVec<[KeywordId; 2]>,
    },
    LacksKeywords {
        keywords: SmallVec<[KeywordId; 2]>,
    },
    HasCounter {
        counter: CounterTarget,
    },
    LacksCounter {
        counter: CounterTarget,
    },

    ExiledWithSelf,
    ExiledByAnyPlayer,
    AssimilatedWithSelf,
    CreatedWithSelf,
    PairedWithSource,
    ChosenObject,
    AttachedToRef {
        host: AttachedHostRef,
    },
    HasAnotherAuraAttached,

    OfChosenType,
    NotOfChosenType,
    OfChosenColor,
    OfChosenName,
    SameNameAsSource,
    NotSameNameAsSource,
    SameNameAsAffected,
    SameNameAsLinkedExile,
    SameNameAsNontokenPermanent,
    SameNameAsTarget {
        index: u16,
    },
    SharesTypeWithCommander,
    SharesColorWithSelf,
    SharesColorWithSpell,
    SharesCreatureTypeWithSelf,
    SharesCardTypeWithExiled,
    SharesCardTypeWithBinding {
        binding: String,
    },
    SharesSubtypeWithSacrificed,

    TargetsSelf,
    TargetsAttached,
    TargetsFilter {
        filter: Box<ObjectFilter>,
    },
    TargetsYouOrFilter {
        filter: Box<ObjectFilter>,
    },
    TargetsPlayer {
        player: PlayerFilter,
    },
    WithSingleTarget,
    SingleTargetFilter {
        filter: Box<ObjectFilter>,
    },
    SingleTargetPlayer {
        player: PlayerFilter,
    },
    AbilitySourceFilter {
        filter: Box<ObjectFilter>,
    },

    CastFromZones {
        zones: ZoneMask,
    },
    ManaValueParity {
        parity: ParityFilter,
    },
    ManaValueEqualsSourcePower,
    ManaValueEqualsChosenNumber,
    ManaValueLteEventObject,
    ManaValueLtRevealedCostCard,
    ManaValueLteEventAmount,
    ManaValueLteTotalCounters,
    PowerGreaterThanBase,
    PowerEqualsToughness,
    ToughnessGreaterThanPower,
    PowerLessThanSource,
    PowerLtSelf,
    PowerLteRevealedCostCard,
    PowerOrToughness {
        cmp: Compare,
    },
    BasePowerOrToughness {
        cmp: Compare,
    },
    UntappedNonattackingSummoningReady,
    DealtDamageByAttachedThisTurn,
    GreatestStatAmongControlled {
        stat: StatKind,
        scope_types: CardTypeMask,
    },
    LeastStatAmongControlled {
        stat: StatKind,
    },
    GreatestPowerAmongAll,
    EnteredGraveyardFromBattlefieldThisTurn,
    PutIntoGraveyardFromNonBattlefieldThisTurn,
    NotYetChosenInStep,

    AnyOf {
        filters: Vec<ObjectFilter>,
    },
}

/// Fast-path bitmask [`ObjectFilter`] with an auxiliary predicate list for rarer constraints.
///
/// Most MTG card filters only set zone/type/color/controller/flags and leave `extra` empty.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectFilter {
    #[serde(default)]
    pub zones: ZoneMask,
    #[serde(default)]
    pub stack_kind: Option<StackTargetKind>,

    #[serde(default)]
    pub types_any: CardTypeMask,
    #[serde(default)]
    pub types_all: CardTypeMask,
    #[serde(default)]
    pub types_none: CardTypeMask,
    #[serde(default)]
    pub supertypes_any: SupertypeMask,
    #[serde(default)]
    pub supertypes_none: SupertypeMask,
    #[serde(default)]
    pub colors_any: ColorMask,
    #[serde(default)]
    pub colors_all: ColorMask,
    #[serde(default)]
    pub colors_none: ColorMask,

    #[serde(default)]
    pub subtypes_any: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub subtypes_all: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub subtypes_none: SmallVec<[SubtypeId; 2]>,

    #[serde(default)]
    pub controller: Option<RelativeControl>,
    #[serde(default)]
    pub owner: Option<RelativeControl>,
    #[serde(default)]
    pub flags: FilterFlags,

    #[serde(default)]
    pub mana_value: Option<Compare>,
    #[serde(default)]
    pub power: Option<Compare>,
    #[serde(default)]
    pub toughness: Option<Compare>,

    #[serde(default)]
    pub extra: Vec<ObjectPredicate>,
}

/// Lightweight filter used inside mana spending restrictions (CR 106.6).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlainManaFilter {
    #[serde(default)]
    pub types_any: CardTypeMask,
    #[serde(default)]
    pub types_all: CardTypeMask,
    #[serde(default)]
    pub types_none: CardTypeMask,
    #[serde(default)]
    pub supertypes_any: SupertypeMask,
    #[serde(default)]
    pub supertypes_none: SupertypeMask,
    #[serde(default)]
    pub subtypes_any: SmallVec<[SubtypeId; 2]>,
    #[serde(default)]
    pub colors_any: ColorMask,
    #[serde(default)]
    pub colors_none: ColorMask,
    #[serde(default)]
    pub multicolored: Option<bool>,
    #[serde(default)]
    pub monocolored: Option<bool>,
    #[serde(default)]
    pub colorless: Option<bool>,
    #[serde(default)]
    pub min_mana_value: Option<u16>,
    #[serde(default)]
    pub max_mana_value: Option<u16>,
    #[serde(default)]
    pub has_keywords: SmallVec<[KeywordId; 2]>,
    #[serde(default)]
    pub cast_from_zone: Option<Zone>,
    #[serde(default)]
    pub of_chosen_type: bool,
    #[serde(default)]
    pub any_of: Vec<PlainManaFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    Object,
    Player,
    ObjectOrPlayer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    pub kind: TargetKind,
    #[serde(default)]
    pub filter: ObjectFilter,
    #[serde(default)]
    pub player: Option<PlayerFilter>,
    pub min: u16,
    pub max: u16,
    #[serde(default)]
    pub same_owner: bool,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
    #[serde(default)]
    pub other_target: bool,
    #[serde(default)]
    pub divide_total: Option<Box<Amount>>,
    #[serde(default)]
    pub with_single_target: bool,
    #[serde(default)]
    pub dynamic_count: Option<Box<Amount>>,
    #[serde(default)]
    pub dynamic_up_to: bool,
    #[serde(default)]
    pub max_total_mana_value: Option<u16>,
    #[serde(default)]
    pub also_spells: bool,
    #[serde(default)]
    pub per_opponent: bool,
    #[serde(default)]
    pub different_controllers: bool,
    #[serde(default)]
    pub chosen_by_event_player: bool,
    #[serde(default)]
    pub shares_card_type_with_target: Option<u16>,
    #[serde(default)]
    pub zones: ZoneMask,
}
