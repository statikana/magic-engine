use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::filter::{ObjectFilter, PlainManaFilter};
use crate::types::{ColorMask, CounterId, CounterTarget, KeywordId, ManaColor, ManaCost, Zone};
use crate::value::Amount;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostObjectTarget {
    SelfObj,
    Filter { filter: Box<ObjectFilter> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoyaltySign {
    Plus,
    Minus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinColoredManaSymbols {
    pub color: ManaColor,
    pub min: u16,
}

/// Cost paid to cast a spell, activate an ability, or satisfy an optional/punisher step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostDef {
    Mana {
        cost: ManaCost,
        #[serde(default)]
        x_spend_colors: ColorMask,
        #[serde(default)]
        x_colored_only: bool,
        #[serde(default)]
        x_max_one_per_color: bool,
    },
    TapSelf,
    UntapSelf {
        /// `true` for `{Q}` untap symbol (respects summoning sickness), `false` for spelled-out "Untap ~".
        #[serde(default = "default_true")]
        via_symbol: bool,
    },
    TapOthers {
        count: Box<Amount>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        share_creature_type: bool,
    },
    UntapOthers {
        count: Box<Amount>,
        filter: Box<ObjectFilter>,
    },
    TapAttached,
    ExertSelf,
    Sacrifice {
        count: Box<Amount>,
        target: CostObjectTarget,
        #[serde(default)]
        sacrifice_all: bool,
        #[serde(default)]
        half_rounded_up: bool,
        #[serde(default)]
        attached_creature: bool,
        #[serde(default)]
        one_or_more: bool,
        #[serde(default)]
        up_to: Option<u16>,
        #[serde(default)]
        different_names: bool,
    },
    Discard {
        count: Box<Amount>,
        #[serde(default)]
        target: Option<CostObjectTarget>,
        #[serde(default)]
        random: bool,
        #[serde(default)]
        same_name_pair: bool,
        #[serde(default)]
        same_name_as_controlled: Option<Box<ObjectFilter>>,
        #[serde(default)]
        different_names: bool,
    },
    DiscardHand,
    DiscardLastDrawnThisTurn,
    PayLife {
        amount: Box<Amount>,
        #[serde(default)]
        half_life_rounded_up: bool,
    },
    OpponentGainsLife {
        amount: Box<Amount>,
        #[serde(default)]
        each_other_player: bool,
    },
    Loyalty {
        delta: i32,
        #[serde(default)]
        is_x: bool,
        #[serde(default)]
        sign: Option<LoyaltySign>,
    },
    RemoveCounters {
        counter: CounterTarget,
        count: Box<Amount>,
        #[serde(default)]
        remove_all: bool,
        from: CostObjectTarget,
        #[serde(default)]
        among: bool,
        #[serde(default)]
        one_or_more: bool,
        #[serde(default)]
        or_suspended_you_own: bool,
    },
    PutCounters {
        count: Box<Amount>,
        counter: CounterId,
        on: CostObjectTarget,
    },
    ExileFrom {
        zone: Zone,
        count: Box<Amount>,
        #[serde(default)]
        target: Option<CostObjectTarget>,
        #[serde(default)]
        from_top: bool,
        #[serde(default)]
        any_graveyard: bool,
        #[serde(default)]
        single_graveyard: bool,
        #[serde(default)]
        min_total_mana_value: Option<u16>,
        #[serde(default)]
        min_total_mana_value_amount: Option<Box<Amount>>,
        #[serde(default)]
        min_colored_mana_symbols: Option<MinColoredManaSymbols>,
        #[serde(default)]
        collect_evidence: bool,
        #[serde(default)]
        one_or_more: bool,
        #[serde(default)]
        total_mana_value_equals_x: bool,
        #[serde(default)]
        and_other_same_name_count: Option<u16>,
    },
    RevealFromHand {
        count: Box<Amount>,
        #[serde(default)]
        target: Option<CostObjectTarget>,
        #[serde(default)]
        share_color: bool,
    },
    RevealHand,
    RevealSelfFromHand,
    Choice {
        options: Vec<Vec<CostDef>>,
        #[serde(default)]
        forage: bool,
    },
    Optional {
        cost: Vec<CostDef>,
    },
    TapSubsetSum {
        min_total: Box<Amount>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        exclude_self: bool,
        #[serde(default)]
        restriction_tag: Option<String>,
    },
    PayEnergy {
        count: Box<Amount>,
    },
    ReturnToHand {
        count: Box<Amount>,
        target: CostObjectTarget,
    },
    Mill {
        count: Box<Amount>,
    },
    Waterbend {
        cost: ManaCost,
    },
    Behold {
        count: Box<Amount>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        exile: bool,
    },
    BeholdOrChoose {
        count: Box<Amount>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        of_chosen_creature_type: bool,
    },
    PutFromHandOnTopOfLibrary {
        count: Box<Amount>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    PutOnBottomOfLibrary {
        from: Zone,
        count: Box<Amount>,
        #[serde(default)]
        target: Option<CostObjectTarget>,
    },
    ProcessOpponentExile {
        count: Box<Amount>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    PutExiledWithSelfToGraveyard {
        count: Box<Amount>,
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    UnattachSelf,
    UnattachOther {
        count: Box<Amount>,
        filter: Box<ObjectFilter>,
        #[serde(default)]
        from_self: bool,
    },
    RollDie {
        sides: u16,
    },
    FlipCoin,
    ChooseCreatureType,
    ChooseNumberBetween {
        min: i32,
        max: i32,
    },
    RevealChosenPlayer,
    RevealChosenSubType,
    ChooseFromKeywordList {
        keywords: SmallVec<[KeywordId; 4]>,
    },
    PayAttachedManaCost,
    ExileFromGraveyardAndPayManaCost {
        #[serde(default)]
        filter: Option<Box<ObjectFilter>>,
    },
    ExileFaceDownPermanentFaceUp,
    PayManaPerCounter {
        per_mana: ManaCost,
        counter: CounterId,
    },
    RepeatableOptionalMana {
        cost: ManaCost,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostDeltaMode {
    SetZero,
    Reduce,
    Increase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColoredCount {
    pub color: ManaColor,
    pub count: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostDelta {
    pub mode: CostDeltaMode,
    #[serde(default)]
    pub generic: u16,
    #[serde(default)]
    pub per: Option<Box<Amount>>,
    #[serde(default)]
    pub additional_costs: Vec<CostDef>,
    #[serde(default)]
    pub colored: SmallVec<[ColoredCount; 3]>,
    #[serde(default)]
    pub hybrid_colored: SmallVec<[(ManaColor, ManaColor); 2]>,
    #[serde(default)]
    pub colored_only: bool,
    #[serde(default)]
    pub min_total_mana: Option<u16>,
    #[serde(default)]
    pub max_reduction: Option<u16>,
    #[serde(default)]
    pub per_target_life: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowSpellsRestriction {
    #[serde(default)]
    pub filter: Option<PlainManaFilter>,
    #[serde(default)]
    pub has_x_in_cost: bool,
    #[serde(default)]
    pub without_x_in_cost: bool,
    #[serde(default)]
    pub is_commander: bool,
    #[serde(default)]
    pub alt_filter: Option<PlainManaFilter>,
    #[serde(default)]
    pub alt_has_x_in_cost: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowAbilitiesRestriction {
    #[serde(default)]
    pub any_ability: bool,
    #[serde(default)]
    pub source_filter: Option<PlainManaFilter>,
    #[serde(default)]
    pub foretell: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManaSpendingRestriction {
    #[serde(default)]
    pub allow_spells: Option<AllowSpellsRestriction>,
    #[serde(default)]
    pub allow_abilities: Option<AllowAbilitiesRestriction>,
    #[serde(default)]
    pub source_chosen_subtype: Option<String>,
    #[serde(default)]
    pub disallow_generic: bool,
    #[serde(default)]
    pub on_spend_cant_be_countered: bool,
    #[serde(default)]
    pub on_spend_commander_bonus_counters: bool,
}
