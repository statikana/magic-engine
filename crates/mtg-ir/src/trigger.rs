use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::filter::{ObjectFilter, PlayerFilter, RelativeControl};
use crate::types::{
    CounterId, GamePhase, KeywordId, ManaColor, SubtypeId, TriggerScope, Zone, ZoneMask,
};
use crate::value::Condition;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum TriggerSubject {
    SelfObj,
    AttachedTo,
    Filter { filter: Box<ObjectFilter> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DamageRecipient {
    Player,
    PlayerOrPlaneswalker,
    Creature,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DamageTargetKind {
    Any,
    Battle,
    PlayerOrBattle,
    Planeswalker,
    Permanent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetedByKind {
    Spell,
    Ability,
    SpellOrAbility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DuringTurnKind {
    Your,
    Opponent,
    EventPlayer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttackedTargetFilter {
    You,
    YouOrPlaneswalkerYouControl,
    Opponent,
    OpponentOrPlaneswalkerOpponentControls,
    EnchantedPlayer,
    Planeswalker,
    Battle,
    PlayerWithMostLife,
    AnyPlayer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DayNightDirection {
    Any,
    DayNightFlip,
    ToDay,
    ToNight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PhaseTurnFilter {
    FirstOfGame,
    YourFirstUpkeep,
    Monarch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpellDelayedDuration {
    NextThisTurn,
    ThisTurn,
    NextUpkeep,
    UntilYourNextTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventKind {
    ZoneChange,
    Cast,
    Attacks,
    Blocks,
    DealsDamage,
    PhaseBegin,
    CounterPlaced,
    CounterRemoved,
    BecomesTarget,
    Attached,
    Unattached,
    BecomesBlocked,
    BecomesBlockedBy,
    BlocksOrBecomesBlocked,
    AttackedAndUnblocked,
    BecomesTapped,
    BecomesUntapped,
    DealtDamage,
    DrawCard,
    GainLife,
    LoseLife,
    TappedForMana,
    ManaAddedByLandAbility,
    Cycle,
    Discard,
    Transforms,
    TurnFaceUp,
    PhasedIn,
    Investigate,
    Surveil,
    Explore,
    Connive,
    Scry,
    BecomesCrewed,
    CrewedVehicle,
    BecomesSaddled,
    SaddledMount,
    CombatDamageBatchPlayer,
    BecameMonarch,
    Foretell,
    Plot,
    Exert,
    Amass,
    ManifestDread,
    BecameMonstrous,
    CommittedCrime,
    Exploited,
    Expended,
    Sacrificed,
    GraveyardLeaveBatch,
    Discover,
    Forage,
    CollectEvidence,
    ClassLevelGained,
    CaseSolved,
    BecameRenowned,
    DoorUnlocked,
    RoomFullyUnlocked,
    RingTempted,
    DungeonRoomEntered,
    DungeonCompleted,
    TookInitiative,
    Mutates,
    SoulbondPaired,
    HauntedCreatureDies,
    StateTriggered,
    DayNightChanged,
    FacedDilemma,
    ExhaustActivated,
    LandPlayed,
    LibrarySearched,
    LibraryShuffled,
    PlayerLostGame,
    CoinFlipped,
    DieRolled,
    EnergyGained,
    Proliferated,
    Evolved,
    AbilityActivated,
    PlayersFinishedVoting,
    Clash,
    SpellCopied,
    SpellCountered,
    SagaChapterResolved,
    GiftGiven,
    TokenCreated,
    AttackTriggered,
    // Replacement-only event kinds
    DrawReplacement,
    DamageReplacement,
    LifeGainReplacement,
    ProliferateReplacement,
}

/// Domain-grouped event qualifiers attached to an [`EventPattern`].
///
/// Replaces TS's 70-field flat `EventFilter` struct with clean, sparse domain fields.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventFilter {
    #[serde(default)]
    pub object: Option<TriggerSubject>,
    #[serde(default)]
    pub or_self: bool,
    #[serde(default)]
    pub or_attached_to_self: bool,
    #[serde(default)]
    pub or_commander: bool,
    #[serde(default)]
    pub player: Option<PlayerFilter>,
    #[serde(default)]
    pub one_or_more: bool,
    #[serde(default)]
    pub during_turn: Option<DuringTurnKind>,
    #[serde(default)]
    pub during_step: Option<GamePhase>,
    #[serde(default)]
    pub during_combat: bool,
    #[serde(default)]
    pub during_main_phase: bool,
    #[serde(default)]
    pub first_time_each_turn: bool,
    #[serde(default)]
    pub nth_this_turn: Option<u16>,
    #[serde(default)]
    pub nth_event_this_turn: Option<u16>,
    #[serde(default)]
    pub attached_filter: Option<Box<ObjectFilter>>,
    #[serde(default)]
    pub watched_bound_object: bool,
    #[serde(default)]
    pub detail: Option<EventDetail>,
}

/// Event-kind-specific filter qualifiers (only allocated for the event kind in question).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum EventDetail {
    ZoneChange {
        #[serde(default)]
        from: ZoneMask,
        #[serde(default)]
        to: Option<Zone>,
        #[serde(default)]
        from_anywhere: bool,
        #[serde(default)]
        not_from_zone: Option<Zone>,
        #[serde(default)]
        not_to_zone: Option<Zone>,
        #[serde(default)]
        enters_untapped: bool,
        #[serde(default)]
        dealt_damage_by_self_this_turn: bool,
    },
    Cast {
        #[serde(default)]
        or_copy: bool,
        #[serde(default)]
        targets_self: bool,
        #[serde(default)]
        targets_only_self: bool,
        #[serde(default)]
        during_opponent_turn: bool,
        #[serde(default)]
        cast_from_not_hand: bool,
        #[serde(default)]
        cast_from_zone: Option<Zone>,
        #[serde(default)]
        was_kicked: bool,
        #[serde(default)]
        spell_ordinal: Option<i8>,
        #[serde(default)]
        spell_has_keyword: Option<KeywordId>,
        #[serde(default)]
        enchanted_player_casts: bool,
    },
    Combat {
        #[serde(default)]
        alone: bool,
        #[serde(default)]
        saddled: bool,
        #[serde(default)]
        min_attackers: Option<u16>,
        #[serde(default)]
        dethrone_most_life: bool,
        #[serde(default)]
        training_greater_attacker: bool,
        #[serde(default)]
        blocked_attacker_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        host: Option<Box<ObjectFilter>>,
        #[serde(default)]
        without_flanking: bool,
        #[serde(default)]
        attacked_target: Option<AttackedTargetFilter>,
        #[serde(default)]
        co_attacker_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        co_attacker_min: Option<u16>,
        #[serde(default)]
        per_attacked_player: bool,
        #[serde(default)]
        attacking_with_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        min_attacking_with_count: Option<u16>,
        #[serde(default)]
        attacking_total_power_min: Option<i32>,
        #[serde(default)]
        attack_enchanted_or_attacked_by_enchanted: bool,
        #[serde(default)]
        attacking_unblocked: bool,
    },
    LandPlayed {
        #[serde(default)]
        cast_from_not_hand: bool,
        #[serde(default)]
        cast_from_zone: Option<Zone>,
    },
    Damage {
        #[serde(default)]
        combat: Option<bool>,
        #[serde(default)]
        noncombat_only: bool,
        #[serde(default)]
        damage_to: Option<DamageRecipient>,
        #[serde(default)]
        damage_target_kind: Option<DamageTargetKind>,
        #[serde(default)]
        damage_player_relation: Option<RelativeControl>,
        #[serde(default)]
        damaged_player: Option<RelativeControl>,
        #[serde(default)]
        damage_target_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        damage_target_enchanted_player: bool,
        #[serde(default)]
        single_source_damage_min: Option<i32>,
        #[serde(default)]
        combat_damage_batch_player: Option<RelativeControl>,
    },
    Phase {
        #[serde(default)]
        phase: Option<GamePhase>,
        #[serde(default)]
        any_main_phase: bool,
        #[serde(default)]
        enchanted_controller_upkeep: bool,
        #[serde(default)]
        enchanted_player_upkeep: bool,
        #[serde(default)]
        enchanted_opponent_end_step: bool,
        #[serde(default)]
        phase_chosen_player: bool,
        #[serde(default)]
        turn_filter: Option<PhaseTurnFilter>,
    },
    Counter {
        #[serde(default)]
        counter: Option<CounterId>,
        #[serde(default)]
        last_counter: bool,
        #[serde(default)]
        put_by_player: Option<RelativeControl>,
        #[serde(default)]
        nth_counter_on_self: Option<u16>,
    },
    BecomesTarget {
        #[serde(default)]
        targeted_by: Option<TargetedByKind>,
        #[serde(default)]
        targeted_by_activated_ability: bool,
        #[serde(default)]
        first_time_targeted_by_you_this_turn: bool,
        #[serde(default)]
        targeting_source_filter: Option<Box<ObjectFilter>>,
        #[serde(default)]
        target_player: Option<RelativeControl>,
    },
    Attach {
        #[serde(default)]
        host: Option<Box<ObjectFilter>>,
        #[serde(default)]
        host_self: bool,
    },
    Mana {
        #[serde(default)]
        tapped_mana_color: Option<ManaColor>,
        #[serde(default)]
        tapped_mana_chosen_color: bool,
    },
    AbilityActivated {
        #[serde(default)]
        is_loyalty_ability: bool,
        #[serde(default)]
        is_non_mana_ability: bool,
        #[serde(default)]
        ability_source: Option<TriggerSubject>,
        #[serde(default)]
        ability_source_controlled_by_event_player: bool,
    },
    Discard {
        #[serde(default)]
        madness_exiled: bool,
        #[serde(default)]
        caused_by_opponent: bool,
    },
    Draw {
        #[serde(default)]
        except_first_in_draw_step: bool,
    },
    Transform {
        #[serde(default)]
        to_face_filter: Option<Box<ObjectFilter>>,
    },
    Tapped {
        #[serde(default)]
        tapped_by_player: Option<RelativeControl>,
    },
    DieOrCoin {
        #[serde(default)]
        die_roll_min: Option<u16>,
        #[serde(default)]
        die_roll_values: SmallVec<[u16; 4]>,
        #[serde(default)]
        won_coin_flip: bool,
    },
    State {
        condition: Box<Condition>,
    },
    Misc {
        #[serde(default)]
        amass_subtype: Option<SubtypeId>,
        #[serde(default)]
        expend_threshold: Option<u16>,
        #[serde(default)]
        class_level: Option<u8>,
        #[serde(default)]
        this_door_face_index: Option<u8>,
        #[serde(default)]
        day_night_direction: Option<DayNightDirection>,
        #[serde(default)]
        final_chapter: bool,
        #[serde(default)]
        won: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPattern {
    pub event: EventKind,
    #[serde(default)]
    pub filter: EventFilter,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    pub event: EventKind,
    #[serde(default)]
    pub filter: EventFilter,
    #[serde(default)]
    pub scope: Option<TriggerScope>,
    /// `None` = battlefield only; `Some(ZoneMask::ALL)` = any zone; `Some(mask)` = specific zones.
    #[serde(default)]
    pub from_zones: Option<ZoneMask>,
    #[serde(default)]
    pub only_once_per_turn: bool,
    #[serde(default)]
    pub do_this_once_per_turn: bool,
    #[serde(default)]
    pub is_state_trigger: bool,
    #[serde(default)]
    pub or: Vec<TriggerDef>,
    #[serde(default)]
    pub chapter_numbers: SmallVec<[u8; 3]>,
    #[serde(default)]
    pub is_chapter_ability: bool,
    #[serde(default)]
    pub spell_delayed: Option<SpellDelayedDuration>,
}
