use mtg_ir::{
    AbilityDef, ActivatedAbility, ActivatorWho, Affected, CardDatabase, CardTypeMask, ColorMask,
    CombatRuleSpec, CostDef, EffectOp, EffectStep, KeywordAbility, KeywordDef, KeywordId,
    ObjectFilter, ProtectionAttachmentExemption, RelativeControl, RestrictionKind, SelectionMod,
    Selector, StaticBody, SupertypeMask, TargetKind, TargetSpec, Timing, Zone, ZoneMask,
};

use crate::action::TargetRef;
use crate::eval::{BoardView, EvalContext, evaluate_condition, off_battlefield_characteristics};
use crate::faces::{for_each_active_ability_with_card_on_faces, resolve_object_faces};
use crate::filter::{matches_object_filter, matches_player_filter};
use crate::id::{ObjectId, PlayerId};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::state::{GameState, LayerCache, LkiSnapshot, ObjectFlags};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeywordCategory {
    CombatEvasion,
    CombatDamage,
    StaticProtection,
    CastAlternative,
    ActivatedKeyword,
    TriggeredKeyword,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeywordSpec {
    pub name: &'static str,
    pub cr_rule: &'static str,
    pub category: KeywordCategory,
}

/// Canonical `CR 702` keyword registry (lowercase names matching [`CardDatabase::keyword_id`]).
pub const KEYWORD_REGISTRY: &[KeywordSpec] = &[
    KeywordSpec {
        name: "deathtouch",
        cr_rule: "CR 702.2",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "defender",
        cr_rule: "CR 702.3",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "double strike",
        cr_rule: "CR 702.4",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "enchant",
        cr_rule: "CR 702.5",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "equip",
        cr_rule: "CR 702.6",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "first strike",
        cr_rule: "CR 702.7",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "flash",
        cr_rule: "CR 702.8",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "flying",
        cr_rule: "CR 702.9",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "haste",
        cr_rule: "CR 702.10",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "hexproof",
        cr_rule: "CR 702.11",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "indestructible",
        cr_rule: "CR 702.12",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "intimidate",
        cr_rule: "CR 702.13",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "landwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "plainswalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "islandwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "swampwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "mountainwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "forestwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "nonbasic landwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "legendary landwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "snow landwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "desertwalk",
        cr_rule: "CR 702.14",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "lifelink",
        cr_rule: "CR 702.15",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "protection",
        cr_rule: "CR 702.16",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "reach",
        cr_rule: "CR 702.17",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "shroud",
        cr_rule: "CR 702.18",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "trample",
        cr_rule: "CR 702.19",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "trample over planeswalkers",
        cr_rule: "CR 702.19c",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "vigilance",
        cr_rule: "CR 702.20",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "ward",
        cr_rule: "CR 702.21",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "banding",
        cr_rule: "CR 702.22",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "rampage",
        cr_rule: "CR 702.23",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "cumulative upkeep",
        cr_rule: "CR 702.24",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "flanking",
        cr_rule: "CR 702.25",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "phasing",
        cr_rule: "CR 702.26",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "buyback",
        cr_rule: "CR 702.27",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "shadow",
        cr_rule: "CR 702.28",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "cycling",
        cr_rule: "CR 702.29",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "echo",
        cr_rule: "CR 702.30",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "horsemanship",
        cr_rule: "CR 702.31",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "fading",
        cr_rule: "CR 702.32",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "kicker",
        cr_rule: "CR 702.33",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "multikicker",
        cr_rule: "CR 702.33c",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "flashback",
        cr_rule: "CR 702.34",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "madness",
        cr_rule: "CR 702.35",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "fear",
        cr_rule: "CR 702.36",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "morph",
        cr_rule: "CR 702.37",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "megamorph",
        cr_rule: "CR 702.37b",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "amplify",
        cr_rule: "CR 702.38",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "provoke",
        cr_rule: "CR 702.39",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "storm",
        cr_rule: "CR 702.40",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "affinity",
        cr_rule: "CR 702.41",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "entwine",
        cr_rule: "CR 702.42",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "modular",
        cr_rule: "CR 702.43",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "sunburst",
        cr_rule: "CR 702.44",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "bushido",
        cr_rule: "CR 702.45",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "soulshift",
        cr_rule: "CR 702.46",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "splice",
        cr_rule: "CR 702.47",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "offering",
        cr_rule: "CR 702.48",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "ninjutsu",
        cr_rule: "CR 702.49",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "convoke",
        cr_rule: "CR 702.51",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "dredge",
        cr_rule: "CR 702.52",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "transmute",
        cr_rule: "CR 702.53",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "bloodthirst",
        cr_rule: "CR 702.54",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "graft",
        cr_rule: "CR 702.58",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "replicate",
        cr_rule: "CR 702.56",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "ripple",
        cr_rule: "CR 702.60",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "split second",
        cr_rule: "CR 702.61",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "suspend",
        cr_rule: "CR 702.62",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "vanishing",
        cr_rule: "CR 702.63",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "absorb",
        cr_rule: "CR 702.64",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "delve",
        cr_rule: "CR 702.66",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "poisonous",
        cr_rule: "CR 702.70",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "transfigure",
        cr_rule: "CR 702.71",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "champion",
        cr_rule: "CR 702.72",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "changeling",
        cr_rule: "CR 702.73",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "evoke",
        cr_rule: "CR 702.74",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "hideaway",
        cr_rule: "CR 702.75",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "prowl",
        cr_rule: "CR 702.76",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "reinforce",
        cr_rule: "CR 702.77",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "conspire",
        cr_rule: "CR 702.78",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "persist",
        cr_rule: "CR 702.79",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "wither",
        cr_rule: "CR 702.80",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "retrace",
        cr_rule: "CR 702.81",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "devour",
        cr_rule: "CR 702.82",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "exalted",
        cr_rule: "CR 702.83",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "unearth",
        cr_rule: "CR 702.84",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "cascade",
        cr_rule: "CR 702.85",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "annihilator",
        cr_rule: "CR 702.86",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "level up",
        cr_rule: "CR 702.87",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "rebound",
        cr_rule: "CR 702.88",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "umbra armor",
        cr_rule: "CR 702.89",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "totem armor",
        cr_rule: "CR 702.89",
        category: KeywordCategory::StaticProtection,
    },
    KeywordSpec {
        name: "infect",
        cr_rule: "CR 702.90",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "battle cry",
        cr_rule: "CR 702.91",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "living weapon",
        cr_rule: "CR 702.92",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "undying",
        cr_rule: "CR 702.93",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "miracle",
        cr_rule: "CR 702.94",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "soulbond",
        cr_rule: "CR 702.95",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "overload",
        cr_rule: "CR 702.96",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "scavenge",
        cr_rule: "CR 702.97",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "unleash",
        cr_rule: "CR 702.98",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "cipher",
        cr_rule: "CR 702.99",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "evolve",
        cr_rule: "CR 702.100",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "extort",
        cr_rule: "CR 702.101",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "fuse",
        cr_rule: "CR 702.102",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "bestow",
        cr_rule: "CR 702.103",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "tribute",
        cr_rule: "CR 702.104",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "dethrone",
        cr_rule: "CR 702.105",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "hidden agenda",
        cr_rule: "CR 702.106",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "outlast",
        cr_rule: "CR 702.107",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "prowess",
        cr_rule: "CR 702.108",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "dash",
        cr_rule: "CR 702.109",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "exploit",
        cr_rule: "CR 702.110",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "menace",
        cr_rule: "CR 702.111",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "renown",
        cr_rule: "CR 702.112",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "awaken",
        cr_rule: "CR 702.113",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "devoid",
        cr_rule: "CR 702.114",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "ingest",
        cr_rule: "CR 702.115",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "myriad",
        cr_rule: "CR 702.116",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "surge",
        cr_rule: "CR 702.117",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "skulk",
        cr_rule: "CR 702.118",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "emerge",
        cr_rule: "CR 702.119",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "escalate",
        cr_rule: "CR 702.120",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "melee",
        cr_rule: "CR 702.121",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "crew",
        cr_rule: "CR 702.122",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "fabricate",
        cr_rule: "CR 702.123",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "partner",
        cr_rule: "CR 702.124",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "undaunted",
        cr_rule: "CR 702.125",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "improvise",
        cr_rule: "CR 702.126",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "aftermath",
        cr_rule: "CR 702.127",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "embalm",
        cr_rule: "CR 702.128",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "eternalize",
        cr_rule: "CR 702.129",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "afflict",
        cr_rule: "CR 702.130",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "ascend",
        cr_rule: "CR 702.131",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "assist",
        cr_rule: "CR 702.132",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "jump-start",
        cr_rule: "CR 702.133",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "mentor",
        cr_rule: "CR 702.134",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "afterlife",
        cr_rule: "CR 702.135",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "riot",
        cr_rule: "CR 702.136",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "spectacle",
        cr_rule: "CR 702.137",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "escape",
        cr_rule: "CR 702.138",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "companion",
        cr_rule: "CR 702.139",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "mutate",
        cr_rule: "CR 702.140",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "encore",
        cr_rule: "CR 702.141",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "boast",
        cr_rule: "CR 702.142",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "foretell",
        cr_rule: "CR 702.143",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "demonstrate",
        cr_rule: "CR 702.144",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "daybound",
        cr_rule: "CR 702.145",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "nightbound",
        cr_rule: "CR 702.145",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "disturb",
        cr_rule: "CR 702.146",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "decayed",
        cr_rule: "CR 702.147",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "cleave",
        cr_rule: "CR 702.148",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "training",
        cr_rule: "CR 702.149",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "compleated",
        cr_rule: "CR 702.150",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "reconfigure",
        cr_rule: "CR 702.151",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "blitz",
        cr_rule: "CR 702.152",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "casualty",
        cr_rule: "CR 702.153",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "enlist",
        cr_rule: "CR 702.154",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "read ahead",
        cr_rule: "CR 702.155",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "ravenous",
        cr_rule: "CR 702.156",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "squad",
        cr_rule: "CR 702.157",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "spaceflight",
        cr_rule: "CR 702.158",
        category: KeywordCategory::CombatEvasion,
    },
    KeywordSpec {
        name: "prototype",
        cr_rule: "CR 702.160",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "living metal",
        cr_rule: "CR 702.161",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "more than meets the eye",
        cr_rule: "CR 702.162",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "for mirrodin!",
        cr_rule: "CR 702.163",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "toxic",
        cr_rule: "CR 702.164",
        category: KeywordCategory::CombatDamage,
    },
    KeywordSpec {
        name: "backup",
        cr_rule: "CR 702.165",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "bargain",
        cr_rule: "CR 702.166",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "craft",
        cr_rule: "CR 702.167",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "disguise",
        cr_rule: "CR 702.168",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "solved",
        cr_rule: "CR 702.169",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "saddle",
        cr_rule: "CR 702.171",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "spree",
        cr_rule: "CR 702.172",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "freerunning",
        cr_rule: "CR 702.173",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "gift",
        cr_rule: "CR 702.174",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "offspring",
        cr_rule: "CR 702.175",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "impending",
        cr_rule: "CR 702.176",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "exhaust",
        cr_rule: "CR 702.177",
        category: KeywordCategory::ActivatedKeyword,
    },
    KeywordSpec {
        name: "max speed",
        cr_rule: "CR 702.178",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "start your engines!",
        cr_rule: "CR 702.179",
        category: KeywordCategory::Other,
    },
    KeywordSpec {
        name: "harmonize",
        cr_rule: "CR 702.180",
        category: KeywordCategory::CastAlternative,
    },
    KeywordSpec {
        name: "mobilize",
        cr_rule: "CR 702.181",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "job select",
        cr_rule: "CR 702.182",
        category: KeywordCategory::TriggeredKeyword,
    },
    KeywordSpec {
        name: "warp",
        cr_rule: "CR 702.183",
        category: KeywordCategory::CastAlternative,
    },
];

/// Looks up a [`KeywordSpec`] by lowercase or mixed-case name.
#[inline]
pub fn lookup_keyword_spec(name: &str) -> Option<&'static KeywordSpec> {
    let lower = name.trim().to_ascii_lowercase();
    KEYWORD_REGISTRY.iter().find(|spec| spec.name == lower)
}

/// Resolves a keyword name to its [`KeywordId`] in `db` using exact then case-insensitive matching.
#[inline]
pub fn keyword_id_ci(db: &CardDatabase, name: &str) -> Option<KeywordId> {
    if let Some(id) = db.keyword_id(name) {
        return Some(id);
    }
    let lower = name.to_ascii_lowercase();
    if let Some(id) = db.keyword_id(&lower) {
        return Some(id);
    }
    db.tables
        .keywords
        .iter()
        .enumerate()
        .find(|(_, k)| k.eq_ignore_ascii_case(name))
        .map(|(i, _)| KeywordId(i as u16))
}

/// Returns the activation zone mask if `kw_ab` is a keyword-backed activated ability
/// synthesized by [`synthesize_keyword_activated_ability`] (`Equip` `CR 702.6a`, `Fortify` `CR 702.67a`).
#[inline]
pub(crate) fn keyword_activated_zones(
    db: &CardDatabase,
    kw_ab: &KeywordAbility,
) -> Option<ZoneMask> {
    let name = db.keyword_name(kw_ab.keyword.keyword)?;
    if name.eq_ignore_ascii_case("equip") || name.eq_ignore_ascii_case("fortify") {
        Some(ZoneMask::BATTLEFIELD)
    } else {
        None
    }
}

/// Synthesizes an [`ActivatedAbility`] on the fly for `AbilityDef::Keyword` entries whose CR rules
/// define an activated ability (`Equip` `CR 702.6a`, `Fortify` `CR 702.67a`).
pub(crate) fn synthesize_keyword_activated_ability(
    db: &CardDatabase,
    kw_ab: &KeywordAbility,
    fallback_idx: usize,
) -> Option<ActivatedAbility> {
    let raw_name = db.keyword_name(kw_ab.keyword.keyword)?;
    let (canonical_name, is_fortify) = if raw_name.eq_ignore_ascii_case("equip") {
        ("equip", false)
    } else if raw_name.eq_ignore_ascii_case("fortify") {
        ("fortify", true)
    } else {
        return None;
    };

    let id = kw_ab
        .id
        .clone()
        .unwrap_or_else(|| format!("{canonical_name}_{fallback_idx}"));

    let mut cost = kw_ab.keyword.cost_defs.clone();
    if cost.is_empty()
        && let Some(mc) = &kw_ab.keyword.cost
        && (mc.generic > 0 || mc.x_count > 0 || !mc.pips.is_empty())
    {
        cost.push(CostDef::Mana {
            cost: mc.clone(),
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
        });
    }

    let mut filter = if !is_fortify {
        kw_ab
            .keyword
            .equip_filter
            .as_deref()
            .cloned()
            .unwrap_or_default()
    } else {
        ObjectFilter::default()
    };
    filter.zones = ZoneMask::BATTLEFIELD;
    filter.controller = Some(RelativeControl::You);
    if is_fortify {
        filter.types_any = CardTypeMask::LAND;
    } else if filter.types_any.is_empty() {
        filter.types_any = if kw_ab.keyword.equip_allow_planeswalker {
            CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER
        } else {
            CardTypeMask::CREATURE
        };
    }

    let target = TargetSpec {
        kind: TargetKind::Object,
        filter,
        player: None,
        min: 1,
        max: 1,
        same_owner: false,
        condition: None,
        other_target: false,
        divide_total: None,
        with_single_target: false,
        dynamic_count: None,
        dynamic_up_to: false,
        max_total_mana_value: None,
        also_spells: false,
        per_opponent: false,
        different_controllers: false,
        chosen_by_event_player: false,
        shares_card_type_with_target: None,
        zones: ZoneMask::BATTLEFIELD,
    };

    let step = EffectStep {
        op: EffectOp::Attach {
            attach_object: None,
            select: SelectionMod::default(),
            attach_equipment_from_self_lki: false,
        },
        subject: Selector::Target { index: 0 },
        actor: None,
        amount: None,
        optional: None,
        gate: None,
        bind_as: None,
        do_this_once_per_turn: false,
        reflexive_trigger: None,
        span: kw_ab.span,
    };

    Some(ActivatedAbility {
        id,
        cost,
        targets: vec![target],
        effect: vec![step],
        modal: None,
        timing: Timing::Sorcery,
        is_mana: false,
        activation_restrictions: Vec::new(),
        activation_condition: None,
        activator_who: Some(ActivatorWho::You),
        power_up_reduction: false,
        x_cant_be_zero: false,
        activation_cost_reduction: kw_ab.keyword.equip_cost_reduction.clone(),
        activation_zones: ZoneMask::BATTLEFIELD,
        cost_keyword: Some(kw_ab.keyword.keyword),
        intrinsic: false,
        text: None,
        grant_condition: None,
        x_spend_colors: ColorMask::empty(),
        x_colored_only: false,
        x_max_one_per_color: false,
        span: kw_ab.span,
    })
}

#[inline]
fn cache_has_keyword_id(cache: &LayerCache, id: ObjectId, kw: KeywordId) -> bool {
    if let Some(eval) = cache.get(id) {
        if cache
            .extra(id)
            .is_some_and(|ex| ex.cant_have_keywords.contains(&kw))
        {
            return false;
        }
        return eval.keywords.contains(&kw);
    }
    false
}

/// Returns `true` if `id` currently has keyword `kw` (honoring Layer 6 grants/removals and `cant_have_keywords`).
pub fn has_keyword_id(state: &GameState, id: ObjectId, kw: KeywordId) -> bool {
    let cache = state.evaluate_board();
    if cache.get(id).is_some() {
        return cache_has_keyword_id(cache, id, kw);
    }
    let Some(obj) = state.object(id) else {
        return false;
    };
    let ctx = EvalContext::new(obj.controller).with_board(BoardView::Cached(cache));
    let off = off_battlefield_characteristics(state, id, obj, obj.zone, &ctx);
    off.keywords.contains(&kw)
}

/// Returns `true` if `id` currently has the named keyword (case-insensitive, honoring Layer 6 & `cant_have_keywords`).
pub fn has_keyword(state: &GameState, id: ObjectId, name: &str) -> bool {
    let db = state.db();
    let cache = state.evaluate_board();
    if let Some(kw) = keyword_id_ci(db, name)
        && cache.get(id).is_some()
    {
        return cache_has_keyword_id(cache, id, kw);
    }
    if let Some(eval) = cache.get(id) {
        let extras = cache.extra(id);
        return eval.keywords.iter().any(|&kid| {
            !extras.is_some_and(|ex| ex.cant_have_keywords.contains(&kid))
                && db
                    .keyword_name(kid)
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
        });
    }
    let Some(obj) = state.object(id) else {
        return false;
    };
    let ctx = EvalContext::new(obj.controller).with_board(BoardView::Cached(cache));
    let off = off_battlefield_characteristics(state, id, obj, obj.zone, &ctx);
    off.keywords.iter().any(|&kid| {
        db.keyword_name(kid)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

#[inline]
fn cache_has_keyword_name(state: &GameState, cache: &LayerCache, id: ObjectId, name: &str) -> bool {
    let db = state.db();
    if let Some(kw) = keyword_id_ci(db, name) {
        return cache_has_keyword_id(cache, id, kw);
    }
    let Some(eval) = cache.get(id) else {
        return false;
    };
    let extras = cache.extra(id);
    eval.keywords.iter().any(|&kid| {
        !extras.is_some_and(|ex| ex.cant_have_keywords.contains(&kid))
            && db
                .keyword_name(kid)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

#[inline]
pub fn has_first_strike(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "first strike")
}

#[inline]
pub fn has_double_strike(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "double strike")
}

#[inline]
pub fn has_trample(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "trample")
}

#[inline]
pub fn has_trample_over_planeswalkers(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "trample over planeswalkers")
}

#[inline]
pub fn has_deathtouch(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "deathtouch")
}

#[inline]
pub fn has_lifelink(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "lifelink")
}

#[inline]
pub fn has_wither(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "wither")
}

#[inline]
pub fn has_infect(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "infect")
}

#[inline]
pub fn has_vigilance(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "vigilance")
}

#[inline]
pub fn has_haste(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "haste")
}

#[inline]
pub fn has_defender(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "defender")
}

#[inline]
pub fn has_indestructible(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "indestructible")
}

#[inline]
pub fn has_menace(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "menace")
}

#[inline]
pub fn has_flying(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "flying")
}

#[inline]
pub fn has_reach(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "reach")
}

#[inline]
pub fn has_phasing(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "phasing")
}

#[inline]
pub fn has_banding(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "banding")
}

#[inline]
pub fn has_umbra_armor(state: &GameState, id: ObjectId) -> bool {
    has_keyword(state, id, "umbra armor") || has_keyword(state, id, "totem armor")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceProtContext<'a> {
    pub colors: ColorMask,
    pub types: CardTypeMask,
    pub subtypes: &'a [mtg_ir::SubtypeId],
    pub supertypes: SupertypeMask,
    pub is_spell: bool,
    pub is_ability: bool,
    pub has_all_creature_types: bool,
    pub mana_value: u32,
    pub is_modified: bool,
    pub was_cast_this_turn: bool,
    pub is_on_battlefield: bool,
    pub counters: &'a [(mtg_ir::CounterId, u32)],
}

struct OwnedObjectChars {
    controller: PlayerId,
    colors: ColorMask,
    types: CardTypeMask,
    subtypes: smallvec::SmallVec<[mtg_ir::SubtypeId; 4]>,
    supertypes: SupertypeMask,
    is_spell: bool,
    has_all_creature_types: bool,
    mana_value: u32,
    was_cast_this_turn: bool,
    is_on_battlefield: bool,
    counters: smallvec::SmallVec<[(mtg_ir::CounterId, u32); 2]>,
}

fn object_chars_cached(
    state: &GameState,
    cache: &LayerCache,
    id: ObjectId,
) -> Option<OwnedObjectChars> {
    let Some(obj) = state.object(id) else {
        let lki = state.lki(id)?;
        return Some(OwnedObjectChars {
            controller: lki.controller,
            colors: lki.colors,
            types: lki.types,
            subtypes: lki.subtypes.clone(),
            supertypes: lki.supertypes,
            is_spell: lki.last_zone == Zone::Stack,
            has_all_creature_types: lki.has_all_creature_types,
            mana_value: lki.mana_value,
            was_cast_this_turn: false,
            is_on_battlefield: lki.last_zone == Zone::Battlefield,
            counters: lki.counters.clone(),
        });
    };
    let counters: smallvec::SmallVec<[(mtg_ir::CounterId, u32); 2]> =
        state.counters(id).iter().copied().collect();
    let was_cast_this_turn = !obj.is_token()
        && obj.flags.contains(ObjectFlags::WAS_CAST)
        && (obj.zone == Zone::Stack || obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN));
    if let Some(ev) = cache.get(id) {
        return Some(OwnedObjectChars {
            controller: ev.controller,
            colors: ev.colors,
            types: ev.types,
            subtypes: ev.subtypes.clone(),
            supertypes: ev.supertypes,
            is_spell: obj.zone == Zone::Stack,
            has_all_creature_types: ev.has_all_creature_types,
            mana_value: ev.mana_value,
            was_cast_this_turn,
            is_on_battlefield: obj.zone == Zone::Battlefield,
            counters,
        });
    }
    let ctx = EvalContext::new(obj.controller).with_board(BoardView::Cached(cache));
    let off = off_battlefield_characteristics(state, id, obj, obj.zone, &ctx);
    let ctrl = if obj.zone == Zone::Stack {
        state
            .stack_entries()
            .iter()
            .find_map(|se| match se.kind {
                crate::state::StackEntryKind::Spell { object } if object == id => {
                    Some(se.controller)
                }
                _ => None,
            })
            .unwrap_or(obj.controller)
    } else {
        obj.controller
    };
    Some(OwnedObjectChars {
        controller: ctrl,
        colors: off.colors,
        types: off.types,
        subtypes: off.subtypes,
        supertypes: off.supertypes,
        is_spell: obj.zone == Zone::Stack,
        has_all_creature_types: off.has_all_creature_types,
        mana_value: off.mana_value,
        was_cast_this_turn,
        is_on_battlefield: obj.zone == Zone::Battlefield,
        counters,
    })
}

pub(crate) fn is_object_modified_for_protection(
    state: &GameState,
    cache: &LayerCache,
    id: ObjectId,
    lki: Option<&LkiSnapshot>,
    controller: PlayerId,
    counters: &[(mtg_ir::CounterId, u32)],
) -> bool {
    if counters.iter().any(|&(_, c)| c > 0) {
        return true;
    }
    let db = state.db();
    let eq_sid = db.subtype_id("Equipment");
    let aura_sid = db.subtype_id("Aura");
    if let Some(obj) = state.object(id) {
        if obj.zone != Zone::Battlefield {
            return false;
        }
        for p in state.players() {
            if p.lost {
                continue;
            }
            for &cand in &p.battlefield {
                let Some(c_obj) = state.object(cand) else {
                    continue;
                };
                if c_obj.is_phased_out() || c_obj.attached_to != Some(id) {
                    continue;
                }
                if let Some(ev) = cache.get(cand) {
                    if eq_sid.is_some_and(|sid| ev.subtypes.contains(&sid)) {
                        return true;
                    }
                    if aura_sid.is_some_and(|sid| ev.subtypes.contains(&sid))
                        && ev.controller == controller
                    {
                        return true;
                    }
                } else {
                    let ctx =
                        EvalContext::new(c_obj.controller).with_board(BoardView::Cached(cache));
                    let off = off_battlefield_characteristics(state, cand, c_obj, c_obj.zone, &ctx);
                    if eq_sid.is_some_and(|sid| off.subtypes.contains(&sid)) {
                        return true;
                    }
                    if aura_sid.is_some_and(|sid| off.subtypes.contains(&sid))
                        && c_obj.controller == controller
                    {
                        return true;
                    }
                }
            }
        }
        return false;
    }
    if let Some(snap) = lki {
        for &att_id in &snap.attached_ids {
            if let Some(ev) = cache.get(att_id) {
                if eq_sid.is_some_and(|sid| ev.subtypes.contains(&sid)) {
                    return true;
                }
                if aura_sid.is_some_and(|sid| ev.subtypes.contains(&sid))
                    && ev.controller == controller
                {
                    return true;
                }
            } else if let Some(att_lki) = state.lki(att_id) {
                if eq_sid.is_some_and(|sid| att_lki.subtypes.contains(&sid)) {
                    return true;
                }
                if aura_sid.is_some_and(|sid| att_lki.subtypes.contains(&sid))
                    && att_lki.controller == controller
                {
                    return true;
                }
            }
        }
    }
    false
}

fn singular_subtype_matches(sl: &str, q: &str) -> bool {
    if sl == q {
        return true;
    }
    match q {
        "oxen" => return sl == "ox",
        "mice" => return sl == "mouse",
        "lice" => return sl == "louse",
        "geese" => return sl == "goose",
        _ => {}
    }
    if let Some(stem) = q.strip_suffix("ves")
        && (sl == format!("{stem}f") || sl == format!("{stem}fe"))
    {
        return true;
    }
    if let Some(stem) = q.strip_suffix("ies")
        && sl == format!("{stem}y")
    {
        return true;
    }
    if let Some(stem) = q.strip_suffix("es")
        && sl == stem
    {
        return true;
    }
    if let Some(stem) = q.strip_suffix('s')
        && sl == stem
    {
        return true;
    }
    false
}

fn protection_subtype_matches(
    db: &CardDatabase,
    q: &str,
    subtypes: &[mtg_ir::SubtypeId],
    has_all_creature_types: bool,
) -> bool {
    if has_all_creature_types && !crate::faces::is_non_creature_subtype(q) {
        let non_creature_match = q
            .strip_suffix('s')
            .is_some_and(crate::faces::is_non_creature_subtype);
        if !non_creature_match {
            return true;
        }
    }
    subtypes.iter().any(|&sid| {
        db.subtype_name(sid)
            .is_some_and(|s| singular_subtype_matches(&s.to_ascii_lowercase(), q))
    })
}

fn counter_id_ci(db: &CardDatabase, name: &str) -> Option<mtg_ir::CounterId> {
    db.counter_id(name).or_else(|| {
        db.tables
            .counters
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .map(|i| mtg_ir::CounterId(i as u16))
    })
}

fn source_matches_single_protection_quality(
    db: &CardDatabase,
    q: &str,
    src: &SourceProtContext<'_>,
) -> bool {
    match q {
        "everything" => true,
        "white" => src.colors.contains(ColorMask::W),
        "blue" => src.colors.contains(ColorMask::U),
        "black" => src.colors.contains(ColorMask::B),
        "red" => src.colors.contains(ColorMask::R),
        "green" => src.colors.contains(ColorMask::G),
        "colorless" => (src.colors & ColorMask::ALL_COLORS).is_empty(),
        "colored" | "all colors" | "each color" => !(src.colors & ColorMask::ALL_COLORS).is_empty(),
        "multicolored" => src.colors.color_count() >= 2,
        "monocolored" => src.colors.color_count() == 1,
        "enemy-colored multicolored" => {
            let c = src.colors & ColorMask::ALL_COLORS;
            c.color_count() >= 2
                && (c.contains(ColorMask::W | ColorMask::B)
                    || c.contains(ColorMask::W | ColorMask::R)
                    || c.contains(ColorMask::U | ColorMask::R)
                    || c.contains(ColorMask::U | ColorMask::G)
                    || c.contains(ColorMask::B | ColorMask::G))
        }
        "spells that are one or more colors" => {
            src.is_spell && !(src.colors & ColorMask::ALL_COLORS).is_empty()
        }
        "permanents that were cast this turn" => {
            src.is_on_battlefield
                && !src.types.intersection(CardTypeMask::PERMANENT).is_empty()
                && src.was_cast_this_turn
        }
        "modified creatures" => src.types.contains(CardTypeMask::CREATURE) && src.is_modified,
        "artifacts" | "artifact" => src.types.contains(CardTypeMask::ARTIFACT),
        "creatures" | "creature" => src.types.contains(CardTypeMask::CREATURE),
        "enchantments" | "enchantment" => src.types.contains(CardTypeMask::ENCHANTMENT),
        "instants" | "instant" => src.types.contains(CardTypeMask::INSTANT),
        "sorceries" | "sorcery" => src.types.contains(CardTypeMask::SORCERY),
        "planeswalkers" | "planeswalker" => src.types.contains(CardTypeMask::PLANESWALKER),
        "lands" | "land" => src.types.contains(CardTypeMask::LAND),
        "battles" | "battle" => src.types.contains(CardTypeMask::BATTLE),
        "kindred" | "tribal" => src.types.contains(CardTypeMask::KINDRED),
        "permanents" | "permanent" => !src.types.intersection(CardTypeMask::PERMANENT).is_empty(),
        "spells" | "spell" => src.is_spell,
        "activated abilities" | "activated and triggered abilities" => src.is_ability,
        "legendary" => src.supertypes.contains(SupertypeMask::LEGENDARY),
        "snow" => src.supertypes.contains(SupertypeMask::SNOW),
        "legendary creatures" => {
            src.supertypes.contains(SupertypeMask::LEGENDARY)
                && src.types.contains(CardTypeMask::CREATURE)
        }
        "god creatures" => {
            src.types.contains(CardTypeMask::CREATURE)
                && protection_subtype_matches(db, "god", src.subtypes, src.has_all_creature_types)
        }
        // Silver-bordered / Un-set qualities not modeled in black-border engine state.
        "black borders"
        | "common"
        | "even collector numbers"
        | "loose lips"
        | "odd collector numbers"
        | "two-word names"
        | "watermarks"
        | "wordy" => false,
        _ => {
            if let Some(mv_rest) = q.strip_prefix("mana value ") {
                if let Some(n_str) = mv_rest.strip_suffix(" or greater")
                    && let Ok(n) = n_str.trim().parse::<u32>()
                {
                    return src.mana_value >= n;
                }
                if let Some(n_str) = mv_rest.strip_suffix(" or less")
                    && let Ok(n) = n_str.trim().parse::<u32>()
                {
                    return src.mana_value <= n;
                }
                if let Ok(n) = mv_rest.trim().parse::<u32>() {
                    return src.mana_value == n;
                }
                return false;
            }
            if let Some(cnt_rest) = q.strip_prefix("permanents with ")
                && let Some(cname) = cnt_rest
                    .strip_suffix(" counters on them")
                    .or_else(|| cnt_rest.strip_suffix(" counter on them"))
            {
                return src.is_on_battlefield
                    && !src.types.intersection(CardTypeMask::PERMANENT).is_empty()
                    && counter_id_ci(db, cname.trim())
                        .is_some_and(|cid| src.counters.iter().any(|&(c, n)| c == cid && n > 0));
            }
            if let Some(rest) = q.strip_prefix("non-") {
                if let Some(sub) = rest.strip_suffix(" creatures") {
                    return src.types.contains(CardTypeMask::CREATURE)
                        && !protection_subtype_matches(
                            db,
                            sub.trim(),
                            src.subtypes,
                            src.has_all_creature_types,
                        );
                }
                if let Some(sub) = rest.strip_suffix(" creature") {
                    return src.types.contains(CardTypeMask::CREATURE)
                        && !protection_subtype_matches(
                            db,
                            sub.trim(),
                            src.subtypes,
                            src.has_all_creature_types,
                        );
                }
                return !protection_subtype_matches(
                    db,
                    rest.trim(),
                    src.subtypes,
                    src.has_all_creature_types,
                );
            }
            protection_subtype_matches(db, q, src.subtypes, src.has_all_creature_types)
        }
    }
}

fn is_atomic_multiword_protection_quality(s: &str) -> bool {
    matches!(
        s,
        "all colors"
            | "each color"
            | "activated and triggered abilities"
            | "one or more colors"
            | "spells that are one or more colors"
            | "enemy-colored multicolored"
            | "permanents that were cast this turn"
    ) || s.starts_with("mana value ")
        || s.starts_with("permanents with ")
}

fn matches_protection_quality_segment(
    db: &CardDatabase,
    seg: &str,
    src: &SourceProtContext<'_>,
) -> bool {
    let q = seg.trim();
    if q.is_empty() {
        return false;
    }
    if is_atomic_multiword_protection_quality(q) {
        return source_matches_single_protection_quality(db, q, src);
    }
    if q.contains(" from ") {
        let normalized = q
            .replace(", and from ", "\0")
            .replace(" and from ", "\0")
            .replace(", from ", "\0");
        if normalized.contains('\0') {
            return normalized
                .split('\0')
                .any(|sub| matches_protection_quality_segment(db, sub, src));
        }
    }
    if q.contains(", and ") || q.contains(", ") || q.contains(" and ") {
        let normalized = q
            .replace(", and ", "\0")
            .replace(", ", "\0")
            .replace(" and ", "\0");
        return normalized
            .split('\0')
            .any(|sub| matches_protection_quality_segment(db, sub, src));
    }
    source_matches_single_protection_quality(db, q, src)
}

pub(crate) fn source_matches_protection_quality_ctx(
    db: &CardDatabase,
    quality: &str,
    src: &SourceProtContext<'_>,
) -> bool {
    let q = quality.trim().to_ascii_lowercase();
    if q.is_empty() {
        return false;
    }
    matches_protection_quality_segment(db, &q, src)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn source_matches_protection_quality_parts(
    db: &mtg_ir::CardDatabase,
    quality: &str,
    colors: ColorMask,
    types: CardTypeMask,
    subtypes: &[mtg_ir::SubtypeId],
    supertypes: SupertypeMask,
    is_spell: bool,
    is_ability: bool,
    has_all_creature_types: bool,
) -> bool {
    let ctx = SourceProtContext {
        colors,
        types,
        subtypes,
        supertypes,
        is_spell,
        is_ability,
        has_all_creature_types,
        mana_value: 0,
        is_modified: false,
        was_cast_this_turn: false,
        is_on_battlefield: !is_spell && !is_ability,
        counters: &[],
    };
    source_matches_protection_quality_ctx(db, quality, &ctx)
}

fn is_aura_object(state: &GameState, cache: &LayerCache, id: ObjectId) -> bool {
    let Some(aura_sid) = state.db().subtype_id("Aura") else {
        return false;
    };
    if let Some(ev) = cache.get(id) {
        return ev.subtypes.contains(&aura_sid);
    }
    if let Some(obj) = state.object(id) {
        let ctx = EvalContext::new(obj.controller).with_board(BoardView::Cached(cache));
        return off_battlefield_characteristics(state, id, obj, obj.zone, &ctx)
            .subtypes
            .contains(&aura_sid);
    }
    state
        .lki(id)
        .is_some_and(|lki| lki.subtypes.contains(&aura_sid))
}

fn source_grants_self_protection_exemption(state: &GameState, source: ObjectId) -> bool {
    let Some(src_obj) = state.object(source) else {
        return false;
    };
    let Some(opts) = crate::faces::face_opts_for_object(state, source) else {
        return false;
    };
    let Some(view) = resolve_object_faces(state.db(), src_obj.card_def, &opts) else {
        return false;
    };
    let mut found = false;
    for_each_active_ability_with_card_on_faces(
        state.db(),
        src_obj.card_def,
        &opts,
        &view.active_face_indices,
        |_| true,
        |ab| {
            if let AbilityDef::Static(st) = ab.ability
                && let StaticBody::Continuous { modifier, .. } = &st.static_def.body
            {
                match modifier {
                    mtg_ir::LayerMod::AddKeywords {
                        protection_attachment_exemption:
                            Some(ProtectionAttachmentExemption::SelfObj),
                        ..
                    }
                    | mtg_ir::LayerMod::ProtectionFromChosenColor {
                        protection_attachment_exemption:
                            Some(ProtectionAttachmentExemption::SelfObj),
                    } => {
                        found = true;
                    }
                    _ => {}
                }
            }
        },
    );
    found
}

fn is_attachment_exempt(
    state: &GameState,
    cache: &LayerCache,
    target: ObjectId,
    target_ctrl: PlayerId,
    source: ObjectId,
    source_ctrl: PlayerId,
    ex: ProtectionAttachmentExemption,
) -> bool {
    match ex {
        ProtectionAttachmentExemption::SelfObj => {
            state
                .object(source)
                .is_some_and(|o| o.attached_to == Some(target))
                && source_grants_self_protection_exemption(state, source)
        }
        ProtectionAttachmentExemption::AllAuras => is_aura_object(state, cache, source),
        ProtectionAttachmentExemption::ControlledAuras => {
            source_ctrl == target_ctrl && is_aura_object(state, cache, source)
        }
        ProtectionAttachmentExemption::ControlledAttachments => source_ctrl == target_ctrl,
    }
}

/// Returns `true` if `target` has protection from `source` (`CR 702.16`), honoring
/// `extras.protection_exemptions` when `for_attachment == true`.
pub fn has_protection_from(
    state: &GameState,
    target: ObjectId,
    source: ObjectId,
    for_attachment: bool,
) -> bool {
    let cache = state.evaluate_board();
    let Some(target_eval) = cache.get(target) else {
        return false;
    };
    let Some(src_chars) = object_chars_cached(state, cache, source) else {
        return false;
    };
    let src_ctrl = src_chars.controller;
    let src_colors = src_chars.colors;
    let src_types = src_chars.types;
    let mut cached_modified: Option<bool> = None;
    let mut matches_quality = |db: &CardDatabase, quality: &str| -> bool {
        let lower_q = quality.to_ascii_lowercase();
        let is_modified = if lower_q.contains("modified") {
            *cached_modified.get_or_insert_with(|| {
                is_object_modified_for_protection(
                    state,
                    cache,
                    source,
                    state.lki(source),
                    src_ctrl,
                    &src_chars.counters,
                )
            })
        } else {
            false
        };
        let src_ctx = SourceProtContext {
            colors: src_colors,
            types: src_types,
            subtypes: &src_chars.subtypes,
            supertypes: src_chars.supertypes,
            is_spell: src_chars.is_spell,
            is_ability: false,
            has_all_creature_types: src_chars.has_all_creature_types,
            mana_value: src_chars.mana_value,
            is_modified,
            was_cast_this_turn: src_chars.was_cast_this_turn,
            is_on_battlefield: src_chars.is_on_battlefield,
            counters: &src_chars.counters,
        };
        source_matches_protection_quality_ctx(db, &lower_q, &src_ctx)
    };

    let extras = cache.extra(target);
    let db = state.db();

    for &kw in &target_eval.keywords {
        if extras.is_some_and(|ex| ex.cant_have_keywords.contains(&kw)) {
            continue;
        }
        let Some(name) = db.keyword_name(kw) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let quality = if let Some(qual) = lower.strip_prefix("protection from ") {
            qual
        } else {
            continue;
        };
        if !matches_quality(db, quality) {
            continue;
        }
        if for_attachment
            && let Some(ex) = extras
                .and_then(|ex| ex.protection_exemptions.iter().find(|(k, _)| *k == kw))
                .map(|(_, e)| *e)
            && is_attachment_exempt(
                state,
                cache,
                target,
                target_eval.controller,
                source,
                src_ctrl,
                ex,
            )
        {
            continue;
        }
        return true;
    }

    if let Some(ex) = extras {
        for (kw, param) in &ex.parameterized_keywords {
            if ex.cant_have_keywords.contains(kw) {
                continue;
            }
            let Some(name) = db.keyword_name(*kw) else {
                continue;
            };
            if !name.eq_ignore_ascii_case("protection") {
                continue;
            }
            if !matches_quality(db, param) {
                continue;
            }
            if for_attachment
                && let Some(ex_kind) = ex
                    .protection_exemptions
                    .iter()
                    .find(|(k, _)| k == kw)
                    .map(|(_, e)| *e)
                && is_attachment_exempt(
                    state,
                    cache,
                    target,
                    target_eval.controller,
                    source,
                    src_ctrl,
                    ex_kind,
                )
            {
                continue;
            }
            return true;
        }
        if let Some((prot_colors, maybe_exempt)) = ex.protection_from_chosen_color
            && !(src_colors & prot_colors).is_empty()
        {
            let exempt = for_attachment
                && maybe_exempt.is_some_and(|e| {
                    is_attachment_exempt(
                        state,
                        cache,
                        target,
                        target_eval.controller,
                        source,
                        src_ctrl,
                        e,
                    )
                });
            if !exempt {
                return true;
            }
        }
        if ex.protection_from_players.contains(&src_ctrl) {
            return true;
        }
        if !(src_types & ex.protection_from_card_types).is_empty() {
            return true;
        }
    }

    false
}

/// Returns `true` if `target_player` has protection from `source` (`CR 702.16e / 702.16j`).
pub fn player_has_protection_from(
    state: &GameState,
    target_player: PlayerId,
    source: Option<ObjectId>,
) -> bool {
    let cache = state.evaluate_board();
    let Some(ep) = cache.player(target_player) else {
        return false;
    };
    if ep.protection_from_everything {
        return true;
    }
    if !ep.protection_from_card_types.is_empty()
        && let Some(src_id) = source
        && let Some(src_chars) = object_chars_cached(state, cache, src_id)
        && !(src_chars.types & ep.protection_from_card_types).is_empty()
    {
        return true;
    }
    false
}

/// Visits active `StaticDef`s on all non-phased battlefield permanents & command emblems.
fn for_each_active_battlefield_static<'a>(
    state: &'a GameState,
    cache: &'a LayerCache,
    mut visit: impl FnMut(ObjectId, PlayerId, &'a mtg_ir::StaticDef),
) {
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &perm_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(obj) = state.object(perm_id) else {
                continue;
            };
            if obj.zone == Zone::Battlefield && obj.is_phased_out() {
                continue;
            }
            if obj.zone == Zone::Command
                && !obj.flags.contains(crate::state::ObjectFlags::IS_EMBLEM)
            {
                continue;
            }
            let eval = cache.get(perm_id);
            let extras = cache.extra(perm_id);
            let lost_innate = eval.is_some_and(|e| e.lost_all_abilities)
                || extras.is_some_and(|ex| ex.innate_abilities_removed);
            let ctrl = eval.map_or(obj.controller, |e| e.controller);
            let ctx = EvalContext::for_source(ctrl, perm_id).with_board(BoardView::Cached(cache));

            if !lost_innate
                && !obj.is_face_down()
                && let Some(opts) = crate::faces::face_opts_for_object(state, perm_id)
                && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
            {
                for_each_active_ability_with_card_on_faces(
                    state.db(),
                    obj.card_def,
                    &opts,
                    &view.active_face_indices,
                    |cond| evaluate_condition(state, cond, &ctx),
                    |ab| {
                        if let AbilityDef::Static(st) = ab.ability
                            && st
                                .static_def
                                .condition
                                .as_deref()
                                .is_none_or(|c| evaluate_condition(state, c, &ctx))
                        {
                            visit(perm_id, ctrl, &st.static_def);
                        }
                    },
                );
            }
            if let Some(ex) = extras {
                for granted in &ex.granted_abilities {
                    match granted {
                        GrantedAbilityEntry::Inline {
                            ability,
                            grant_controller,
                            ..
                        } => {
                            if let AbilityDef::Static(st) = ability.as_ref()
                                && st
                                    .static_def
                                    .condition
                                    .as_deref()
                                    .is_none_or(|c| evaluate_condition(state, c, &ctx))
                            {
                                visit(perm_id, *grant_controller, &st.static_def);
                            }
                        }
                        GrantedAbilityEntry::CardAbility {
                            card_def,
                            face_index,
                            ability_index,
                            grant_controller,
                            ..
                        } => {
                            if let Some(card) = state.db().get(*card_def)
                                && let Some(face) = card.faces.get(usize::from(*face_index))
                                && let Some(AbilityDef::Static(st)) =
                                    face.abilities.get(*ability_index)
                                && st
                                    .static_def
                                    .condition
                                    .as_deref()
                                    .is_none_or(|c| evaluate_condition(state, c, &ctx))
                            {
                                visit(perm_id, *grant_controller, &st.static_def);
                            }
                        }
                        GrantedAbilityEntry::IntrinsicBasicMana { .. }
                        | GrantedAbilityEntry::DynamicWard { .. } => {}
                    }
                }
            }
        }
    }
}

fn affected_matches(
    state: &GameState,
    cache: &LayerCache,
    source_id: ObjectId,
    source_ctrl: PlayerId,
    affected: &Affected,
    target_id: ObjectId,
) -> bool {
    match affected {
        Affected::SelfObj => source_id == target_id,
        Affected::AttachedTo => state
            .object(source_id)
            .and_then(|o| o.attached_to)
            .is_some_and(|h| h == target_id),
        Affected::Filter { filter } => {
            let ctx = EvalContext::for_source(source_ctrl, source_id)
                .with_board(BoardView::Cached(cache));
            matches_object_filter(state, target_id, filter, &ctx)
        }
    }
}

fn attacker_landwalk_matches(
    state: &GameState,
    cache: &LayerCache,
    attacker: ObjectId,
    defender: PlayerId,
    kw: KeywordId,
    kw_name: &str,
) -> bool {
    let lower = kw_name.to_ascii_lowercase();
    let Some(prefix) = lower.strip_suffix("walk") else {
        return false;
    };
    let Some(def_player) = state.player(defender) else {
        return false;
    };

    // Check explicit KeywordDef::landwalk_filter if present on attacker's faces.
    if let Some(obj) = state.object(attacker)
        && let Some(opts) = crate::faces::face_opts_for_object(state, attacker)
        && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        let mut custom_filter: Option<&ObjectFilter> = None;
        for_each_active_ability_with_card_on_faces(
            state.db(),
            obj.card_def,
            &opts,
            &view.active_face_indices,
            |_| true,
            |ab| {
                if let AbilityDef::Keyword(k) = ab.ability
                    && k.keyword.keyword == kw
                    && let Some(lf) = k.keyword.landwalk_filter.as_deref()
                {
                    custom_filter = Some(lf);
                }
            },
        );
        if let Some(lf) = custom_filter {
            let ctrl = cache.get(attacker).map_or(obj.controller, |e| e.controller);
            let ctx = EvalContext::for_source(ctrl, attacker).with_board(BoardView::Cached(cache));
            return def_player
                .battlefield
                .iter()
                .any(|&lid| matches_object_filter(state, lid, lf, &ctx));
        }
    }

    let qual = prefix.trim();
    let db = state.db();
    for &lid in &def_player.battlefield {
        let Some(lobj) = state.object(lid) else {
            continue;
        };
        if lobj.is_phased_out() {
            continue;
        }
        let Some(lev) = cache.get(lid) else {
            continue;
        };
        if !lev.types.contains(CardTypeMask::LAND) {
            continue;
        }
        match qual {
            "" | "land" => return true,
            "nonbasic land" => {
                if !lev.supertypes.contains(SupertypeMask::BASIC) {
                    return true;
                }
            }
            "legendary land" => {
                if lev.supertypes.contains(SupertypeMask::LEGENDARY) {
                    return true;
                }
            }
            "snow land" => {
                if lev.supertypes.contains(SupertypeMask::SNOW) {
                    return true;
                }
            }
            sub => {
                if lev.subtypes.iter().any(|&sid| {
                    db.subtype_name(sid)
                        .is_some_and(|s| s.eq_ignore_ascii_case(sub))
                }) {
                    return true;
                }
            }
        }
    }
    false
}

/// Checks single-blocker vs single-attacker evasion & blocking restrictions (`CR 509.1b`, `CR 702`).
///
/// Menace (`>= 2` blockers) and global blocker counts are validated at `ConfirmBlockers` in `combat.rs`.
pub fn check_evasion_can_block(
    state: &GameState,
    blocker: ObjectId,
    attacker: ObjectId,
) -> Result<(), &'static str> {
    let cache = state.evaluate_board();
    let Some(blk_obj) = state.object(blocker) else {
        return Err("blocker does not exist");
    };
    let Some(atk_obj) = state.object(attacker) else {
        return Err("attacker does not exist");
    };
    if blk_obj.zone != Zone::Battlefield || blk_obj.is_phased_out() {
        return Err("blocker is not an active permanent on battlefield");
    }
    if atk_obj.zone != Zone::Battlefield || atk_obj.is_phased_out() {
        return Err("attacker is not an active permanent on battlefield");
    }
    let Some(blk_ev) = cache.get(blocker) else {
        return Err("blocker not evaluated");
    };
    let Some(atk_ev) = cache.get(attacker) else {
        return Err("attacker not evaluated");
    };
    if !blk_ev.types.contains(CardTypeMask::CREATURE) {
        return Err("blocker is not a creature");
    }

    // CR 702.16e: Creature with protection from blocker can't be blocked by it.
    if has_protection_from(state, attacker, blocker, false) {
        return Err("attacker has protection from blocker");
    }

    // Scan active static CombatRule & Restriction effects.
    let mut can_block_as_though_reach = false;
    let mut ignored_landwalk_kws: smallvec::SmallVec<[KeywordId; 4]> = smallvec::SmallVec::new();
    let mut static_block_err: Option<&'static str> = None;

    for_each_active_battlefield_static(state, cache, |src_id, src_ctrl, st| {
        if static_block_err.is_some() {
            return;
        }
        match &st.body {
            StaticBody::Restriction {
                affected,
                restriction,
            } => match restriction {
                RestrictionKind::CantBlock | RestrictionKind::CantAttackOrBlock => {
                    if affected_matches(state, cache, src_id, src_ctrl, affected, blocker) {
                        static_block_err = Some("blocker can't block");
                    }
                }
                RestrictionKind::CantBeBlocked
                    if affected_matches(state, cache, src_id, src_ctrl, affected, attacker) =>
                {
                    static_block_err = Some("attacker can't be blocked");
                }
                _ => {}
            },
            StaticBody::CombatRule { affected, rule } => {
                let on_blocker =
                    affected_matches(state, cache, src_id, src_ctrl, affected, blocker);
                let on_attacker =
                    affected_matches(state, cache, src_id, src_ctrl, affected, attacker);
                let ctx =
                    EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
                match rule {
                    CombatRuleSpec::CanBlockAsThoughReach { filter } if on_blocker => {
                        if filter
                            .as_deref()
                            .is_none_or(|f| matches_object_filter(state, attacker, f, &ctx))
                        {
                            can_block_as_though_reach = true;
                        }
                    }
                    CombatRuleSpec::IgnoreLandwalk { keyword } => {
                        ignored_landwalk_kws.push(*keyword);
                    }
                    CombatRuleSpec::CantBlockExcept { filter } if on_blocker => {
                        if !matches_object_filter(state, attacker, filter, &ctx) {
                            static_block_err = Some("blocker can't block this attacker");
                        }
                    }
                    CombatRuleSpec::CantBlockFilter { filter } if on_blocker => {
                        if matches_object_filter(state, attacker, filter, &ctx) {
                            static_block_err = Some("blocker can't block matching attacker");
                        }
                    }
                    CombatRuleSpec::CantBlockPowerGreaterThanSelf if on_blocker => {
                        let bp = blk_ev.pt.map_or(0, |p| p.power);
                        let ap = atk_ev.pt.map_or(0, |p| p.power);
                        if ap > bp {
                            static_block_err =
                                Some("blocker can't block creatures with greater power");
                        }
                    }
                    CombatRuleSpec::CantBlockPowerLessThanSelf if on_blocker => {
                        let bp = blk_ev.pt.map_or(0, |p| p.power);
                        let ap = atk_ev.pt.map_or(0, |p| p.power);
                        if ap < bp {
                            static_block_err =
                                Some("blocker can't block creatures with lesser power");
                        }
                    }
                    CombatRuleSpec::CantBlockIfYouControl { filter } if on_blocker => {
                        if let Some(bp) = state.player(blk_ev.controller)
                            && bp
                                .battlefield
                                .iter()
                                .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                        {
                            static_block_err =
                                Some("blocker can't block due to controlled permanent");
                        }
                    }
                    CombatRuleSpec::CantBeBlockedBy { filter } if on_attacker => {
                        if matches_object_filter(state, blocker, filter, &ctx) {
                            static_block_err = Some("attacker can't be blocked by this blocker");
                        }
                    }
                    CombatRuleSpec::CantBeBlockedByPowerLessThanSelf if on_attacker => {
                        let bp = blk_ev.pt.map_or(0, |p| p.power);
                        let ap = atk_ev.pt.map_or(0, |p| p.power);
                        if bp < ap {
                            static_block_err =
                                Some("attacker can't be blocked by creatures with lesser power");
                        }
                    }
                    CombatRuleSpec::CantBeBlockedByPowerGreaterThanSelf if on_attacker => {
                        let bp = blk_ev.pt.map_or(0, |p| p.power);
                        let ap = atk_ev.pt.map_or(0, |p| p.power);
                        if bp > ap {
                            static_block_err =
                                Some("attacker can't be blocked by creatures with greater power");
                        }
                    }
                    CombatRuleSpec::CantBeBlockedExceptBy { filter, or_filter } if on_attacker => {
                        let ok = matches_object_filter(state, blocker, filter, &ctx)
                            || or_filter
                                .as_deref()
                                .is_some_and(|of| matches_object_filter(state, blocker, of, &ctx));
                        if !ok {
                            static_block_err = Some("attacker can't be blocked by this creature");
                        }
                    }
                    CombatRuleSpec::CantBeBlockedIfDefenderControls { filter } if on_attacker => {
                        if let Some(bp) = state.player(blk_ev.controller)
                            && bp
                                .battlefield
                                .iter()
                                .any(|&pid| matches_object_filter(state, pid, filter, &ctx))
                        {
                            static_block_err = Some(
                                "attacker can't be blocked because defender controls matching permanent",
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    });

    if let Some(err) = static_block_err {
        return Err(err);
    }

    // Flying (CR 702.9) & Reach (CR 702.17)
    if cache_has_keyword_name(state, cache, attacker, "flying")
        && !cache_has_keyword_name(state, cache, blocker, "flying")
        && !cache_has_keyword_name(state, cache, blocker, "reach")
        && !can_block_as_though_reach
    {
        return Err("creature without flying or reach can't block creature with flying");
    }

    // Spaceflight (CR 702.158)
    if cache_has_keyword_name(state, cache, attacker, "spaceflight")
        && !cache_has_keyword_name(state, cache, blocker, "spaceflight")
    {
        return Err("creature without spaceflight can't block creature with spaceflight");
    }

    // Shadow (CR 702.28b)
    let atk_shadow = cache_has_keyword_name(state, cache, attacker, "shadow");
    let blk_shadow = cache_has_keyword_name(state, cache, blocker, "shadow");
    if atk_shadow != blk_shadow {
        return Err("shadow creatures can only block or be blocked by shadow creatures");
    }

    // Horsemanship (CR 702.31)
    if cache_has_keyword_name(state, cache, attacker, "horsemanship")
        && !cache_has_keyword_name(state, cache, blocker, "horsemanship")
    {
        return Err("creature without horsemanship can't block creature with horsemanship");
    }

    // Fear (CR 702.36)
    if cache_has_keyword_name(state, cache, attacker, "fear")
        && !blk_ev.types.contains(CardTypeMask::ARTIFACT)
        && !blk_ev.colors.contains(ColorMask::B)
    {
        return Err("creature with fear can only be blocked by artifact or black creatures");
    }

    // Intimidate (CR 702.13)
    if cache_has_keyword_name(state, cache, attacker, "intimidate")
        && !blk_ev.types.contains(CardTypeMask::ARTIFACT)
        && (blk_ev.colors & atk_ev.colors & ColorMask::ALL_COLORS).is_empty()
    {
        return Err(
            "creature with intimidate can only be blocked by artifact creatures or creatures sharing a color",
        );
    }

    // Skulk (CR 702.118)
    if cache_has_keyword_name(state, cache, attacker, "skulk") {
        let atk_p = atk_ev.pt.map_or(0, |p| p.power);
        let blk_p = blk_ev.pt.map_or(0, |p| p.power);
        if blk_p > atk_p {
            return Err("creature with skulk can't be blocked by creature with greater power");
        }
    }

    // Landwalk (CR 702.14)
    let atk_extras = cache.extra(attacker);
    for &kw in &atk_ev.keywords {
        if atk_extras.is_some_and(|ex| ex.cant_have_keywords.contains(&kw)) {
            continue;
        }
        if ignored_landwalk_kws.contains(&kw) {
            continue;
        }
        if let Some(kw_name) = state.db().keyword_name(kw)
            && kw_name.to_ascii_lowercase().ends_with("walk")
            && attacker_landwalk_matches(state, cache, attacker, blk_ev.controller, kw, kw_name)
        {
            return Err("attacker has landwalk for a land defending player controls");
        }
    }

    Ok(())
}

/// Checks whether `aura` can legally enchant `attached_to` (`CR 303.4 / 702.5 / 704.5m`).
///
/// Honors `KeywordDef::enchant_filter` / `enchant_player`, `cant_have_keywords`,
/// and `has_protection_from(..., for_attachment = true)`.
pub fn enchant_legality_for_aura(
    state: &GameState,
    aura: ObjectId,
    attached_to: TargetRef,
) -> bool {
    let Some(aura_obj) = state.object(aura) else {
        return false;
    };
    if aura_obj.is_phased_out() || aura_obj.is_face_down() {
        return false;
    }

    let cache = state.evaluate_board();
    let aura_eval = cache.get(aura);
    let aura_extras = cache.extra(aura);
    if aura_eval.is_some_and(|e| e.lost_all_abilities)
        || aura_extras.is_some_and(|ex| ex.innate_abilities_removed)
    {
        return false;
    }

    let enchant_kw = keyword_id_ci(state.db(), "enchant");
    if let Some(ekw) = enchant_kw
        && aura_extras.is_some_and(|ex| ex.cant_have_keywords.contains(&ekw))
    {
        return false;
    }

    let aura_ctrl = aura_eval.map_or(aura_obj.controller, |e| e.controller);
    let Some(opts) = crate::faces::face_opts_for_object(state, aura) else {
        return false;
    };
    let Some(view) = resolve_object_faces(state.db(), aura_obj.card_def, &opts) else {
        return false;
    };

    let mut enchant_defs: smallvec::SmallVec<[&KeywordDef; 2]> = smallvec::SmallVec::new();
    for_each_active_ability_with_card_on_faces(
        state.db(),
        aura_obj.card_def,
        &opts,
        &view.active_face_indices,
        |_| true,
        |ab| {
            if let AbilityDef::Keyword(k) = ab.ability {
                let is_enchant = enchant_kw == Some(k.keyword.keyword)
                    || state
                        .db()
                        .keyword_name(k.keyword.keyword)
                        .is_some_and(|n| n.eq_ignore_ascii_case("enchant"));
                if is_enchant
                    && !aura_extras
                        .is_some_and(|ex| ex.cant_have_keywords.contains(&k.keyword.keyword))
                {
                    enchant_defs.push(&k.keyword);
                }
            }
        },
    );

    if let Some(ex) = aura_extras {
        for granted in &ex.granted_abilities {
            let ab = match granted {
                GrantedAbilityEntry::Inline { ability, .. } => Some(ability.as_ref()),
                GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index,
                    ..
                } => state
                    .db()
                    .get(*card_def)
                    .and_then(|c| c.faces.get(usize::from(*face_index)))
                    .and_then(|f| f.abilities.get(*ability_index)),
                _ => None,
            };
            if let Some(AbilityDef::Keyword(k)) = ab {
                let is_enchant = enchant_kw == Some(k.keyword.keyword)
                    || state
                        .db()
                        .keyword_name(k.keyword.keyword)
                        .is_some_and(|n| n.eq_ignore_ascii_case("enchant"));
                if is_enchant && !ex.cant_have_keywords.contains(&k.keyword.keyword) {
                    enchant_defs.push(&k.keyword);
                }
            }
        }
    }

    if enchant_defs.is_empty() {
        return false;
    }

    let ctx = EvalContext::for_source(aura_ctrl, aura).with_board(BoardView::Cached(cache));
    match attached_to {
        TargetRef::Object { id: host } => {
            if host == aura {
                return false;
            }
            let Some(host_obj) = state.object(host) else {
                return false;
            };
            if host_obj.zone == Zone::Battlefield && host_obj.is_phased_out() {
                return false;
            }
            if has_protection_from(state, host, aura, true) {
                return false;
            }
            enchant_defs.iter().any(|kdef| {
                let Some(filter) = kdef.enchant_filter.as_deref() else {
                    return false;
                };
                if filter.zones.is_empty() && host_obj.zone != Zone::Battlefield {
                    return false;
                }
                if !filter.zones.is_empty()
                    && !filter.zones.contains(ZoneMask::from_zone(host_obj.zone))
                {
                    return false;
                }
                matches_object_filter(state, host, filter, &ctx)
            })
        }
        TargetRef::Player { id: pid } => {
            let Some(p) = state.player(pid) else {
                return false;
            };
            if p.lost {
                return false;
            }
            if player_has_protection_from(state, pid, Some(aura)) {
                return false;
            }
            enchant_defs.iter().any(|kdef| {
                kdef.enchant_player
                    .as_ref()
                    .is_some_and(|pf| matches_player_filter(state, pid, pf, &ctx))
            })
        }
        TargetRef::Stack { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::layers::predicates::{ContinuousTarget, TimedContinuousEffect};
    use crate::state::{GameObject, ObjectFlags};
    use mtg_ir::{
        CardDef, CardDefId, CardLayout, Characteristics, Duration, FaceDef, IR_V2_VERSION,
        InternTables, IrV2Bundle, KeywordAbility, Layer, LayerMod, ManaCost, ObjectFilter,
        PrintedPt, StaticAbility, StaticDef,
    };
    use smallvec::SmallVec;
    use std::sync::Arc;

    fn mk_creature(
        name: &str,
        colors: ColorMask,
        types: CardTypeMask,
        p: i32,
        t: i32,
        kws: Vec<KeywordId>,
    ) -> CardDef {
        let abilities = kws
            .into_iter()
            .map(|kw| {
                AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw,
                        cost: None,
                        param: None,
                        n: None,
                        cost_defs: vec![],
                        kicker_costs: vec![],
                        equip_filter: None,
                        enchant_filter: None,
                        enchant_player: None,
                        affinity_filter: None,
                        cycling_filter: None,
                        cycling_desc: None,
                        prototype_spec: None,
                        awaken_n: None,
                        emerge_filter: None,
                        champion_filter: None,
                        devour_filter: None,
                        devour_x: false,
                        equip_allow_planeswalker: false,
                        backup_ability_start_index: None,
                        gift_kind: None,
                        craft_materials: vec![],
                        craft_any_number: false,
                        craft_share_card_type: false,
                        splice_filter: None,
                        impending_n: None,
                        landwalk_filter: None,
                        suspend_x: false,
                        equip_cost_reduction: None,
                        dynamic_amount: None,
                    },
                    id: None,
                    span: Default::default(),
                })
            })
            .collect();
        CardDef {
            oracle_id: format!("oid_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: p,
                        toughness: t,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities,
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        }
    }

    fn spawn_bf(state: &mut GameState, ctrl: PlayerId, card: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let mut obj = GameObject::new(card, ctrl, Zone::Battlefield, ts, ObjectFlags::empty());
        obj.controller = ctrl;
        let oid = state.insert_object(obj, None);
        if let Some(p) = state.player_mut(ctrl) {
            p.battlefield.push(oid);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        oid
    }

    #[test]
    fn evasion_protection_and_cant_have_keywords_enforced() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into()); // 0
        tables.keywords.push("reach".into()); // 1
        tables.keywords.push("protection from white".into()); // 2
        tables.keywords.push("shadow".into()); // 3
        tables.keywords.push("skulk".into()); // 4
        tables.keywords.push("islandwalk".into()); // 5
        tables.keywords.push("enchant".into()); // 6
        tables.subtypes.push("Island".into()); // 0
        tables.subtypes.push("Aura".into()); // 1

        let flyer = mk_creature(
            "Synth Flyer",
            ColorMask::U,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![KeywordId(0)],
        );
        let reacher = mk_creature(
            "Synth Archer",
            ColorMask::G,
            CardTypeMask::CREATURE,
            2,
            3,
            vec![KeywordId(1)],
        );
        let bear = mk_creature(
            "Synth Bear",
            ColorMask::W,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![],
        );
        let skulker = mk_creature(
            "Synth Skulker",
            ColorMask::B,
            CardTypeMask::CREATURE,
            1,
            1,
            vec![KeywordId(4)],
        );
        let islandwalker = mk_creature(
            "Synth Swimmer",
            ColorMask::U,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![KeywordId(5)],
        );
        let island = CardDef {
            oracle_id: "oid_island".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Island".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Island".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: smallvec::smallvec![mtg_ir::SubtypeId(0)],
                    supertypes: SupertypeMask::BASIC,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
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
        };
        let aura = CardDef {
            oracle_id: "oid_aura".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Ward Aura".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Ward Aura".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: smallvec::smallvec![mtg_ir::SubtypeId(1)],
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: KeywordDef {
                            keyword: KeywordId(6),
                            cost: None,
                            param: None,
                            n: None,
                            cost_defs: vec![],
                            kicker_costs: vec![],
                            equip_filter: None,
                            enchant_filter: Some(Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            })),
                            enchant_player: None,
                            affinity_filter: None,
                            cycling_filter: None,
                            cycling_desc: None,
                            prototype_spec: None,
                            awaken_n: None,
                            emerge_filter: None,
                            champion_filter: None,
                            devour_filter: None,
                            devour_x: false,
                            equip_allow_planeswalker: false,
                            backup_ability_start_index: None,
                            gift_kind: None,
                            craft_materials: vec![],
                            craft_any_number: false,
                            craft_share_card_type: false,
                            splice_filter: None,
                            impending_n: None,
                            landwalk_filter: None,
                            suspend_x: false,
                            equip_cost_reduction: None,
                            dynamic_amount: None,
                        },
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer6,
                                affected: Affected::AttachedTo,
                                modifier: LayerMod::AddKeywords {
                                    keywords: smallvec::smallvec![KeywordId(2)],
                                    protection_attachment_exemption: Some(
                                        ProtectionAttachmentExemption::AllAuras,
                                    ),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: Default::default(),
                    }),
                ],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![flyer, reacher, bear, skulker, islandwalker, island, aura],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(5); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 55).unwrap();

        let p0_flyer = spawn_bf(&mut state, PlayerId(0), CardDefId(0));
        let p1_reacher = spawn_bf(&mut state, PlayerId(1), CardDefId(1));
        let p1_bear = spawn_bf(&mut state, PlayerId(1), CardDefId(2));
        let p0_skulker = spawn_bf(&mut state, PlayerId(0), CardDefId(3));
        let p0_walker = spawn_bf(&mut state, PlayerId(0), CardDefId(4));
        let _p1_island = spawn_bf(&mut state, PlayerId(1), CardDefId(5));
        let p0_aura = spawn_bf(&mut state, PlayerId(0), CardDefId(6));

        assert!(has_flying(&state, p0_flyer));
        assert!(has_reach(&state, p1_reacher));
        assert!(check_evasion_can_block(&state, p1_bear, p0_flyer).is_err());
        assert!(check_evasion_can_block(&state, p1_reacher, p0_flyer).is_ok());
        assert!(check_evasion_can_block(&state, p1_bear, p0_skulker).is_err());
        assert!(check_evasion_can_block(&state, p1_bear, p0_walker).is_err());

        // Attach white Aura granting protection from white with AllAuras exemption.
        crate::zones::attach_raw(&mut state, p0_aura, p1_bear).unwrap();
        assert!(has_protection_from(&state, p1_bear, p0_aura, false));
        assert!(!has_protection_from(&state, p1_bear, p0_aura, true));
        assert!(enchant_legality_for_aura(
            &state,
            p0_aura,
            TargetRef::Object { id: p1_bear }
        ));

        // Remove flying and bar gaining it via cant_have_or_gain.
        let ce_id = state.allocate_continuous_effect_id();
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: ce_id,
            source: None,
            controller: PlayerId(1),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![p0_flyer]),
            modifier: LayerMod::RemoveKeywords {
                keywords: smallvec::smallvec![KeywordId(0)],
                cant_have_or_gain: true,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts,
            expires_turn: Some(state.turn_number()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(!has_flying(&state, p0_flyer));
        assert!(check_evasion_can_block(&state, p1_bear, p0_flyer).is_ok());
    }

    #[test]
    fn combat_damage_and_layer6_suppression_enforced() {
        let mut tables = InternTables::default();
        tables.keywords.push("deathtouch".into()); // 0
        tables.keywords.push("trample".into()); // 1
        tables.keywords.push("indestructible".into()); // 2

        let crusher = mk_creature(
            "Synth Crusher",
            ColorMask::G,
            CardTypeMask::CREATURE,
            5,
            5,
            vec![KeywordId(0), KeywordId(1)],
        );
        let sentinel = mk_creature(
            "Synth Sentinel",
            ColorMask::W,
            CardTypeMask::CREATURE,
            2,
            4,
            vec![KeywordId(2)],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![crusher, sentinel],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 77).unwrap();

        let atk = spawn_bf(&mut state, PlayerId(0), CardDefId(0));
        let blk = spawn_bf(&mut state, PlayerId(1), CardDefId(1));

        assert!(has_deathtouch(&state, atk));
        assert!(has_trample(&state, atk));
        assert!(has_indestructible(&state, blk));

        // Remove deathtouch with cant_have_or_gain: true and verify even a later AddKeywords cannot restore it.
        let ce1 = state.allocate_continuous_effect_id();
        let ts1 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: ce1,
            source: None,
            controller: PlayerId(1),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![atk]),
            modifier: LayerMod::RemoveKeywords {
                keywords: smallvec::smallvec![KeywordId(0)],
                cant_have_or_gain: true,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts1,
            expires_turn: Some(state.turn_number()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        let ce2 = state.allocate_continuous_effect_id();
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: ce2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![atk]),
            modifier: LayerMod::AddKeywords {
                keywords: smallvec::smallvec![KeywordId(0)],
                protection_attachment_exemption: None,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: Some(state.turn_number()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(!has_deathtouch(&state, atk));
        assert!(has_trample(&state, atk));
    }

    #[test]
    fn parameterized_protection_and_compound_qualities_enforced() {
        let mut tables = InternTables::default();
        tables.keywords.push("protection".into()); // 0
        tables.subtypes.push("Human".into()); // 0
        tables.subtypes.push("Spirit".into()); // 1
        tables.subtypes.push("Elf".into()); // 2

        let mut prot_compound = mk_creature(
            "Synth Compound Knight",
            ColorMask::W,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![],
        );
        prot_compound.faces[0]
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(0),
                    cost: None,
                    param: Some("black and from green".into()),
                    n: None,
                    cost_defs: vec![],
                    kicker_costs: vec![],
                    equip_filter: None,
                    enchant_filter: None,
                    enchant_player: None,
                    affinity_filter: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                    awaken_n: None,
                    emerge_filter: None,
                    champion_filter: None,
                    devour_filter: None,
                    devour_x: false,
                    equip_allow_planeswalker: false,
                    backup_ability_start_index: None,
                    gift_kind: None,
                    craft_materials: vec![],
                    craft_any_number: false,
                    craft_share_card_type: false,
                    splice_filter: None,
                    impending_n: None,
                    landwalk_filter: None,
                    suspend_x: false,
                    equip_cost_reduction: None,
                    dynamic_amount: None,
                },
                id: None,
                span: Default::default(),
            }));

        let mut prot_each_color = mk_creature(
            "Synth Chromatic Ward",
            ColorMask::empty(),
            CardTypeMask::CREATURE,
            2,
            2,
            vec![],
        );
        prot_each_color.faces[0]
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(0),
                    cost: None,
                    param: Some("each color".into()),
                    n: None,
                    cost_defs: vec![],
                    kicker_costs: vec![],
                    equip_filter: None,
                    enchant_filter: None,
                    enchant_player: None,
                    affinity_filter: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                    awaken_n: None,
                    emerge_filter: None,
                    champion_filter: None,
                    devour_filter: None,
                    devour_x: false,
                    equip_allow_planeswalker: false,
                    backup_ability_start_index: None,
                    gift_kind: None,
                    craft_materials: vec![],
                    craft_any_number: false,
                    craft_share_card_type: false,
                    splice_filter: None,
                    impending_n: None,
                    landwalk_filter: None,
                    suspend_x: false,
                    equip_cost_reduction: None,
                    dynamic_amount: None,
                },
                id: None,
                span: Default::default(),
            }));

        let black_elf = mk_creature(
            "Synth Shadow Elf",
            ColorMask::B,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![],
        );
        let mut green_human = mk_creature(
            "Synth Forest Human",
            ColorMask::G,
            CardTypeMask::CREATURE,
            2,
            2,
            vec![],
        );
        green_human.faces[0].characteristics.subtypes = smallvec::smallvec![mtg_ir::SubtypeId(0)];
        let red_spirit = {
            let mut c = mk_creature(
                "Synth Flame Spirit",
                ColorMask::R,
                CardTypeMask::CREATURE,
                2,
                2,
                vec![],
            );
            c.faces[0].characteristics.subtypes = smallvec::smallvec![mtg_ir::SubtypeId(1)];
            c
        };
        let colorless_golem = mk_creature(
            "Synth Iron Golem",
            ColorMask::empty(),
            CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
            3,
            3,
            vec![],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    prot_compound,
                    prot_each_color,
                    black_elf,
                    green_human,
                    red_spirit,
                    colorless_golem,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // Direct matcher checks: compound, plural subtypes, each color, non-subtype.
        assert!(source_matches_protection_quality_parts(
            &db,
            "black and from green",
            ColorMask::G,
            CardTypeMask::CREATURE,
            &[],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));
        assert!(!source_matches_protection_quality_parts(
            &db,
            "black and from green",
            ColorMask::R,
            CardTypeMask::CREATURE,
            &[],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));
        assert!(source_matches_protection_quality_parts(
            &db,
            "Vampires, from Humans, and from Zombies",
            ColorMask::G,
            CardTypeMask::CREATURE,
            &[mtg_ir::SubtypeId(0)],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));
        assert!(source_matches_protection_quality_parts(
            &db,
            "each color",
            ColorMask::U,
            CardTypeMask::INSTANT,
            &[],
            SupertypeMask::empty(),
            true,
            false,
            false,
        ));
        assert!(!source_matches_protection_quality_parts(
            &db,
            "each color",
            ColorMask::empty(),
            CardTypeMask::ARTIFACT,
            &[],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));
        assert!(source_matches_protection_quality_parts(
            &db,
            "non-Spirit creatures",
            ColorMask::G,
            CardTypeMask::CREATURE,
            &[mtg_ir::SubtypeId(0)],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));
        assert!(!source_matches_protection_quality_parts(
            &db,
            "non-Spirit creatures",
            ColorMask::R,
            CardTypeMask::CREATURE,
            &[mtg_ir::SubtypeId(1)],
            SupertypeMask::empty(),
            false,
            false,
            false,
        ));

        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(5); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 91).unwrap();

        let p0_knight = spawn_bf(&mut state, PlayerId(0), CardDefId(0));
        let p0_ward = spawn_bf(&mut state, PlayerId(0), CardDefId(1));
        let p1_black = spawn_bf(&mut state, PlayerId(1), CardDefId(2));
        let p1_green = spawn_bf(&mut state, PlayerId(1), CardDefId(3));
        let p1_red = spawn_bf(&mut state, PlayerId(1), CardDefId(4));
        let p1_golem = spawn_bf(&mut state, PlayerId(1), CardDefId(5));

        assert!(has_protection_from(&state, p0_knight, p1_black, false));
        assert!(has_protection_from(&state, p0_knight, p1_green, false));
        assert!(!has_protection_from(&state, p0_knight, p1_red, false));
        assert!(check_evasion_can_block(&state, p1_black, p0_knight).is_err());
        assert!(check_evasion_can_block(&state, p1_red, p0_knight).is_ok());

        assert!(has_protection_from(&state, p0_ward, p1_red, false));
        assert!(!has_protection_from(&state, p0_ward, p1_golem, false));

        // Remove protection via RemoveAllAbilities -> protection no longer blocks.
        let ce = state.allocate_continuous_effect_id();
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: ce,
            source: None,
            controller: PlayerId(1),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![p0_knight]),
            modifier: LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts,
            expires_turn: Some(state.turn_number()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(!has_protection_from(&state, p0_knight, p1_black, false));
    }

    #[test]
    fn synth_protection_all_black_border_qualities_and_irregular_plurals() {
        let mut tables = InternTables::default();
        tables.keywords.push("protection".into()); // 0
        tables.keywords.push("enchant".into()); // 1
        tables.subtypes.push("Elf".into()); // 0
        tables.subtypes.push("Werewolf".into()); // 1
        tables.subtypes.push("Vampire".into()); // 2
        tables.subtypes.push("Zombie".into()); // 3
        tables.subtypes.push("Ox".into()); // 4
        tables.subtypes.push("Equipment".into()); // 5
        tables.subtypes.push("Aura".into()); // 6
        tables.counters.push("+1/+1".into()); // 0
        tables.counters.push("corruption".into()); // 1

        let mk_prot = |oid: &str, name: &str, param: &str| {
            let mut c = mk_creature(name, ColorMask::W, CardTypeMask::CREATURE, 2, 2, vec![]);
            c.oracle_id = oid.into();
            c.faces[0]
                .abilities
                .push(AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: KeywordId(0),
                        cost: None,
                        param: Some(param.into()),
                        n: None,
                        cost_defs: vec![],
                        kicker_costs: vec![],
                        equip_filter: None,
                        enchant_filter: None,
                        enchant_player: None,
                        affinity_filter: None,
                        cycling_filter: None,
                        cycling_desc: None,
                        prototype_spec: None,
                        awaken_n: None,
                        emerge_filter: None,
                        champion_filter: None,
                        devour_filter: None,
                        devour_x: false,
                        equip_allow_planeswalker: false,
                        backup_ability_start_index: None,
                        gift_kind: None,
                        craft_materials: vec![],
                        craft_any_number: false,
                        craft_share_card_type: false,
                        splice_filter: None,
                        impending_n: None,
                        landwalk_filter: None,
                        suspend_x: false,
                        equip_cost_reduction: None,
                        dynamic_amount: None,
                    },
                    id: None,
                    span: Default::default(),
                }));
            c
        };
        let mk_sub =
            |oid: &str, name: &str, sub: mtg_ir::SubtypeId, mv_cost: &str, colors: ColorMask| {
                let mut c = mk_creature(name, colors, CardTypeMask::CREATURE, 2, 2, vec![]);
                c.oracle_id = oid.into();
                c.faces[0].characteristics.subtypes.push(sub);
                c.faces[0].characteristics.mana_cost = ManaCost::parse(mv_cost);
                c
            };

        let c_elves = mk_prot("oid_p_elves", "Synth Ward Elves", "Elves");
        let c_compound = mk_prot(
            "oid_p_comp",
            "Synth Ward Horde",
            "Vampires, from Werewolves, and from Zombies",
        );
        let c_oxen = mk_prot("oid_p_oxen", "Synth Ward Oxen", "Oxen");
        let c_colored_spells = mk_prot(
            "oid_p_cspells",
            "Synth Ward Colored Spells",
            "spells that are one or more colors",
        );
        let c_mv_ge3 = mk_prot(
            "oid_p_mvge3",
            "Synth Ward Big MV",
            "mana value 3 or greater",
        );
        let c_mv_le2 = mk_prot("oid_p_mvle2", "Synth Ward Small MV", "mana value 2 or less");
        let c_cast_turn = mk_prot(
            "oid_p_cast",
            "Synth Ward Cast",
            "spells and from permanents that were cast this turn",
        );
        let c_mod = mk_prot("oid_p_mod", "Synth Ward Unmod", "modified creatures");
        let c_corr = mk_prot(
            "oid_p_corr",
            "Synth Ward Corrupted",
            "permanents with corruption counters on them",
        );

        let c_elf = mk_sub(
            "oid_s_elf",
            "Synth Elf",
            mtg_ir::SubtypeId(0),
            "{1}{G}",
            ColorMask::G,
        );
        let c_wolf = mk_sub(
            "oid_s_wolf",
            "Synth Wolf",
            mtg_ir::SubtypeId(1),
            "{2}{R}",
            ColorMask::R,
        );
        let c_vamp = mk_sub(
            "oid_s_vamp",
            "Synth Vamp",
            mtg_ir::SubtypeId(2),
            "{B}",
            ColorMask::B,
        );
        let c_zomb = mk_sub(
            "oid_s_zomb",
            "Synth Zomb",
            mtg_ir::SubtypeId(3),
            "{3}{B}",
            ColorMask::B,
        );
        let c_ox = mk_sub(
            "oid_s_ox",
            "Synth Ox",
            mtg_ir::SubtypeId(4),
            "{3}{W}",
            ColorMask::empty(),
        );
        let c_colorless_spell = {
            let mut c = mk_creature(
                "Synth Colorless Bolt",
                ColorMask::empty(),
                CardTypeMask::INSTANT,
                0,
                0,
                vec![],
            );
            c.oracle_id = "oid_c_bolt".into();
            c.faces[0].characteristics.pt = None;
            c.faces[0].characteristics.mana_cost = ManaCost::parse("{2}");
            c
        };
        let c_colored_spell = {
            let mut c = mk_creature(
                "Synth Colored Bolt",
                ColorMask::R,
                CardTypeMask::INSTANT,
                0,
                0,
                vec![],
            );
            c.oracle_id = "oid_r_bolt".into();
            c.faces[0].characteristics.pt = None;
            c.faces[0].characteristics.mana_cost = ManaCost::parse("{R}");
            c
        };
        let c_equip = {
            let mut c = mk_creature(
                "Synth Blade",
                ColorMask::empty(),
                CardTypeMask::ARTIFACT,
                0,
                0,
                vec![],
            );
            c.oracle_id = "oid_eq".into();
            c.faces[0].characteristics.pt = None;
            c.faces[0]
                .characteristics
                .subtypes
                .push(mtg_ir::SubtypeId(5));
            c
        };
        let mut c_curse_player = mk_creature(
            "Synth Player Curse",
            ColorMask::B,
            CardTypeMask::ENCHANTMENT,
            0,
            0,
            vec![],
        );
        c_curse_player.oracle_id = "oid_curse".into();
        c_curse_player.faces[0].characteristics.pt = None;
        c_curse_player.faces[0]
            .characteristics
            .subtypes
            .push(mtg_ir::SubtypeId(6));
        c_curse_player.faces[0]
            .abilities
            .push(AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(1),
                    cost: None,
                    param: Some("player".into()),
                    n: None,
                    cost_defs: vec![],
                    kicker_costs: vec![],
                    equip_filter: None,
                    enchant_filter: None,
                    enchant_player: Some(mtg_ir::PlayerFilter {
                        relation: mtg_ir::PlayerRelation::Opponent,
                    }),
                    affinity_filter: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                    awaken_n: None,
                    emerge_filter: None,
                    champion_filter: None,
                    devour_filter: None,
                    devour_x: false,
                    equip_allow_planeswalker: false,
                    backup_ability_start_index: None,
                    gift_kind: None,
                    craft_materials: vec![],
                    craft_any_number: false,
                    craft_share_card_type: false,
                    splice_filter: None,
                    impending_n: None,
                    landwalk_filter: None,
                    suspend_x: false,
                    equip_cost_reduction: None,
                    dynamic_amount: None,
                },
                id: None,
                span: Default::default(),
            }));

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    c_elves,           // 0
                    c_compound,        // 1
                    c_oxen,            // 2
                    c_colored_spells,  // 3
                    c_mv_ge3,          // 4
                    c_mv_le2,          // 5
                    c_cast_turn,       // 6
                    c_mod,             // 7
                    c_corr,            // 8
                    c_elf,             // 9
                    c_wolf,            // 10
                    c_vamp,            // 11
                    c_zomb,            // 12
                    c_ox,              // 13
                    c_colorless_spell, // 14
                    c_colored_spell,   // 15
                    c_equip,           // 16
                    c_curse_player,    // 17
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(9); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 77).unwrap();

        let w_elves = spawn_bf(&mut state, PlayerId(0), CardDefId(0));
        let w_comp = spawn_bf(&mut state, PlayerId(0), CardDefId(1));
        let w_oxen = spawn_bf(&mut state, PlayerId(0), CardDefId(2));
        let w_cspells = spawn_bf(&mut state, PlayerId(0), CardDefId(3));
        let w_mvge3 = spawn_bf(&mut state, PlayerId(0), CardDefId(4));
        let w_mvle2 = spawn_bf(&mut state, PlayerId(0), CardDefId(5));
        let w_cast = spawn_bf(&mut state, PlayerId(0), CardDefId(6));
        let w_mod = spawn_bf(&mut state, PlayerId(0), CardDefId(7));
        let w_corr = spawn_bf(&mut state, PlayerId(0), CardDefId(8));

        let elf = spawn_bf(&mut state, PlayerId(1), CardDefId(9));
        let wolf = spawn_bf(&mut state, PlayerId(1), CardDefId(10));
        let vamp = spawn_bf(&mut state, PlayerId(1), CardDefId(11));
        let zomb = spawn_bf(&mut state, PlayerId(1), CardDefId(12));
        let ox = spawn_bf(&mut state, PlayerId(1), CardDefId(13));
        let equip = spawn_bf(&mut state, PlayerId(1), CardDefId(16));

        let spawn_stack = |state: &mut GameState, card: CardDefId, ctrl: PlayerId| {
            let ts = state.allocate_timestamp();
            let obj = GameObject::new(card, ctrl, Zone::Stack, ts, ObjectFlags::empty());
            state.insert_object(obj, None)
        };
        let colorless_spell = spawn_stack(&mut state, CardDefId(14), PlayerId(1));
        let colored_spell = spawn_stack(&mut state, CardDefId(15), PlayerId(1));

        // 1) Irregular plurals & compound lists ("Vampires, from Werewolves, and from Zombies")
        assert!(has_protection_from(&state, w_elves, elf, false));
        assert!(!has_protection_from(&state, w_elves, wolf, false));
        assert!(has_protection_from(&state, w_comp, vamp, false));
        assert!(has_protection_from(&state, w_comp, wolf, false));
        assert!(has_protection_from(&state, w_comp, zomb, false));
        assert!(!has_protection_from(&state, w_comp, elf, false));
        assert!(has_protection_from(&state, w_oxen, ox, false));

        // 2) "spells that are one or more colors": blocks colored spell, not colorless spell, not colored permanent ability
        assert!(has_protection_from(&state, w_cspells, colored_spell, false));
        assert!(!has_protection_from(
            &state,
            w_cspells,
            colorless_spell,
            false
        ));
        assert!(!has_protection_from(&state, w_cspells, wolf, false));

        // 3) Mana value 3 or greater / 2 or less
        assert!(has_protection_from(&state, w_mvge3, wolf, false)); // MV 3
        assert!(has_protection_from(&state, w_mvge3, zomb, false)); // MV 4
        assert!(!has_protection_from(&state, w_mvge3, elf, false)); // MV 2
        assert!(has_protection_from(&state, w_mvle2, elf, false)); // MV 2
        assert!(has_protection_from(&state, w_mvle2, vamp, false)); // MV 1
        assert!(!has_protection_from(&state, w_mvle2, wolf, false)); // MV 3

        // 4) "spells and from permanents that were cast this turn"
        assert!(has_protection_from(&state, w_cast, colored_spell, false));
        assert!(!has_protection_from(&state, w_cast, wolf, false)); // not WAS_CAST yet
        state
            .object_mut(wolf)
            .unwrap()
            .flags
            .insert(ObjectFlags::WAS_CAST | ObjectFlags::ENTERED_THIS_TURN);
        assert!(has_protection_from(&state, w_cast, wolf, false));
        let token_wolf = {
            let ts = state.allocate_timestamp();
            let obj = GameObject::new(
                CardDefId(10),
                PlayerId(1),
                Zone::Battlefield,
                ts,
                ObjectFlags::IS_TOKEN | ObjectFlags::WAS_CAST | ObjectFlags::ENTERED_THIS_TURN,
            );
            let id = state.insert_object(obj, None);
            state.player_mut(PlayerId(1)).unwrap().battlefield.push(id);
            state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };
        assert!(!has_protection_from(&state, w_cast, token_wolf, false)); // tokens were not cast
        let _ = crate::turn::run_cleanup_step(&mut state).unwrap();
        assert!(!has_protection_from(&state, w_cast, wolf, false)); // WAS_CAST/ENTERED_THIS_TURN cleared by cleanup

        // 5) "modified creatures": counter, Equipment, Aura (own vs opponent's)
        assert!(!has_protection_from(&state, w_mod, elf, false));
        state.add_counter(elf, mtg_ir::CounterId(0), 1);
        assert!(has_protection_from(&state, w_mod, elf, false));
        state.set_counter(elf, mtg_ir::CounterId(0), 0);
        assert!(!has_protection_from(&state, w_mod, elf, false));
        state.object_mut(equip).unwrap().attached_to = Some(elf);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(has_protection_from(&state, w_mod, elf, false));
        state.object_mut(equip).unwrap().attached_to = None;
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // Opponent's Aura does not make creature modified; controller's Aura does!
        let p0_aura = spawn_bf(&mut state, PlayerId(0), CardDefId(17));
        state.object_mut(p0_aura).unwrap().attached_to = Some(elf);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(!has_protection_from(&state, w_mod, elf, false));
        state.object_mut(p0_aura).unwrap().controller = PlayerId(1);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(has_protection_from(&state, w_mod, elf, false));

        // 6) "permanents with corruption counters on them"
        assert!(!has_protection_from(&state, w_corr, zomb, false));
        state.add_counter(zomb, mtg_ir::CounterId(1), 1);
        assert!(has_protection_from(&state, w_corr, zomb, false));

        // 7) LKI fallback in object_chars_cached when source died after dealing damage
        let lki_snap = {
            let ev_vamp = state
                .evaluate_board()
                .get(vamp)
                .cloned()
                .expect("vamp evaluated on battlefield");
            Arc::new(crate::events::build_batch_lki_snapshot(
                &state, vamp, &ev_vamp,
            ))
        };
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .battlefield
            .retain(|&id| id != vamp);
        state.player_mut(PlayerId(1)).unwrap().graveyard.push(vamp);
        state.object_mut(vamp).unwrap().zone = Zone::Graveyard;
        state.record_lki(vamp, lki_snap);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(has_protection_from(&state, w_comp, vamp, false));

        // 8) enchant_legality_for_aura blocks TargetRef::Player with ProtectionFromEverything
        let p0_curse = spawn_bf(&mut state, PlayerId(0), CardDefId(17));
        assert!(enchant_legality_for_aura(
            &state,
            p0_curse,
            TargetRef::Player { id: PlayerId(1) }
        ));
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .protection_from_everything = Some(Duration::UntilYourNextTurn);
        state.mark_dirty(crate::state::DirtyMask::PLAYER_STATE);
        assert!(!enchant_legality_for_aura(
            &state,
            p0_curse,
            TargetRef::Player { id: PlayerId(1) }
        ));
    }
}
