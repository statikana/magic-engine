use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::effect::{CopyModificationSpec, EffectStep, TokenSpec};
use crate::filter::{ObjectFilter, PlayerFilter};
use crate::static_def::Affected;
use crate::trigger::EventPattern;
use crate::types::{CardTypeMask, CounterId, KeywordId, ManaColor, ManaCost, SubtypeId, Zone};
use crate::value::{Amount, Condition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubTypeChoiceScope {
    Creature,
    BasicLand,
    Land,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DamageModMode {
    Double,
    Triple,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DamageModTarget {
    #[default]
    Any,
    Opponent,
    OpponentOrOpponentPerm,
    ChosenPlayer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DamageScaleSpec {
    pub mode: DamageModMode,
    #[serde(default)]
    pub add: Option<Box<Amount>>,
    #[serde(default)]
    pub noncombat_only: bool,
    #[serde(default)]
    pub combat_only: bool,
    #[serde(default)]
    pub source_filter: Option<Affected>,
    #[serde(default)]
    pub target_filter: DamageModTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifeGainScaleSpec {
    pub double: bool,
    #[serde(default)]
    pub add: Option<Box<Amount>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrawScaleSpec {
    #[serde(default)]
    pub factor: Option<u16>,
    #[serde(default)]
    pub delta: Option<i16>,
    #[serde(default)]
    pub skip_first_in_your_draw_step: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DealtDamageExileScope {
    SelfSource,
    ControlledSource,
    ControlledSourceOpponentCreature,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EtbCopySpec {
    pub filter: Box<ObjectFilter>,
    pub optional: bool,
    #[serde(default)]
    pub from_graveyards: bool,
    #[serde(default)]
    pub tapped_if_copied: bool,
    #[serde(default)]
    pub modifications: Option<Box<CopyModificationSpec>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum EtbChoiceSpec {
    SubType {
        scope: SubTypeChoiceScope,
    },
    SubTypeOptions {
        subtypes: SmallVec<[SubtypeId; 4]>,
    },
    Color {
        #[serde(default)]
        exclude: Option<ManaColor>,
    },
    TwoColors,
    CardName {
        #[serde(default)]
        nonland_only: bool,
        #[serde(default)]
        look_at_opponent_hand: bool,
    },
    Counter {
        options: SmallVec<[CounterId; 3]>,
    },
    AnchorWord {
        words: [String; 2],
    },
    Player {
        opponent_only: bool,
    },
    TwoPlayers,
    Parity,
    CardType {
        #[serde(default)]
        exclude: CardTypeMask,
        #[serde(default)]
        options: CardTypeMask,
        #[serde(default)]
        look_at_opponent_hand: bool,
    },
    Number {
        #[serde(default)]
        min: Option<i32>,
        #[serde(default)]
        max: Option<i32>,
    },
    ObjectOnBattlefield {
        filter: Box<ObjectFilter>,
        #[serde(default)]
        optional: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum OpponentDiscardToBattlefield {
    Plain,
    WithCounters { counter: CounterId, count: u16 },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneChangeMod {
    #[serde(default)]
    pub to: Option<Zone>,
    #[serde(default)]
    pub shuffle_into_library: bool,
    #[serde(default)]
    pub amount: Option<Box<Amount>>,
    #[serde(default)]
    pub tapped: Option<bool>,
    #[serde(default)]
    pub counters: Option<(CounterId, Box<Amount>)>,
    #[serde(default)]
    pub additional_counters: Option<(CounterId, Box<Amount>)>,
    #[serde(default)]
    pub conditional_counters: Option<(Box<Condition>, CounterId, Box<Amount>)>,
    #[serde(default)]
    pub conditional_or_fallback_counters: Option<(Box<Condition>, CounterId, u16, u16)>,
    #[serde(default)]
    pub counters_per_artifact_mana: Option<CounterId>,
    #[serde(default)]
    pub copy: Option<EtbCopySpec>,
    #[serde(default)]
    pub etb_choices: Vec<EtbChoiceSpec>,
    #[serde(default)]
    pub pay_life_or_enter_tapped: Option<u16>,
    #[serde(default)]
    pub reveal_from_hand_or_enter_tapped: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub reveal_unless_controls: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub enters_prepared: bool,
    #[serde(default)]
    pub becomes_day_if_neither: bool,
    #[serde(default)]
    pub enter_under_opponent_control: bool,
    #[serde(default)]
    pub exile_with_counter: Option<CounterId>,
    #[serde(default)]
    pub opponent_discard_to_battlefield: Option<OpponentDiscardToBattlefield>,
    #[serde(default)]
    pub dealt_damage_exile_on_die: Option<DealtDamageExileScope>,
    #[serde(default)]
    pub exile_and_extra_turn_on_bf_to_gy: bool,
    #[serde(default)]
    pub dies_exile_and_reflexive: Option<(ManaCost, Box<TokenSpec>, String)>,
    #[serde(default)]
    pub etb_discard_for_keyword_counters: SmallVec<[KeywordId; 4]>,
    #[serde(default)]
    pub etb_remove_counters_for_plus_counters: Option<(u16, Option<Box<ObjectFilter>>)>,
}

/// Strongly-typed replacement outcome (CR 614–616), eliminating TS's 55-field `modify` bag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ReplacementWith {
    Skip,
    Execute {
        steps: Vec<EffectStep>,
    },
    ZoneChange {
        mod_spec: Box<ZoneChangeMod>,
    },
    DamageScale {
        spec: DamageScaleSpec,
    },
    DamageToMinusCounters,
    DamageToSelfAsCounters {
        counter: CounterId,
    },
    LifeGainScale {
        spec: LifeGainScaleSpec,
    },
    DrawScale {
        spec: DrawScaleSpec,
    },
    StealOpponentExtraDraws,
    AbundanceDrawChoice,
    EmptyLibraryGraveyardReanimate {
        filter: Box<ObjectFilter>,
        #[serde(default)]
        return_other_cards_to_hand: bool,
    },
    ProliferateMultiplier {
        factor: u16,
    },
    RegenerateSelfOnDestroy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementDef {
    pub replaces: EventPattern,
    pub with: ReplacementWith,
    #[serde(default)]
    pub self_replacement: bool,
    #[serde(default)]
    pub who: Option<PlayerFilter>,
    #[serde(default)]
    pub condition: Option<Box<Condition>>,
}
