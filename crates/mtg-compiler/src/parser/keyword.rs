use mtg_ir::{
    AbilityDef, Affected, Amount, CardTypeMask, ColorMask, CompanionId, Compare, CompareOp,
    Condition, CostDef, CostDelta, CostDeltaMode, CounterTarget, EffectOp, EffectStep, EventDetail,
    EventFilter, EventKind, EventPattern, FilterFlags, GamePhase, GiftKind, KeywordAbility,
    KeywordDef, Layer, LayerMod, ManaColor, ManaCost, ObjectFilter, ObjectPredicate, PlayerFilter,
    PlayerRef, PlayerRelation, PrototypeSpec, RelativeControl, ReplacementAbility, ReplacementDef,
    ReplacementWith, SearchDestination, SearchSpec, SelectionMod, Selector, Span,
    SpellOrAbilityFilter, StaticAbility, StaticBody, StaticDef, SubtypeId, SupertypeMask,
    TargetKind, TargetSpec, TriggerDef, TriggerScope, TriggerSubject, TriggeredAbility,
    UnsupportedAbility, UnsupportedReason, Zone, ZoneChangeMod, ZoneMask,
};
use smallvec::{SmallVec, smallvec};

use crate::input::derive_colors_from_mana_cost;
use crate::intern::Interner;
use crate::lexer::{Punct, TokenKind, tokenize};
use crate::normalize::NormalizedLine;
use crate::parser::ParseCtx;
use crate::parser::cost::{
    is_cast_payable_cost, is_equip_payable_cost, is_supported_optional_cost, is_ward_payable_cost,
    parse_cost_list,
};
use crate::parser::filter::{
    FilterOpts, OUTLAW_SUBTYPES, parse_multi_or_object_phrase, parse_object_phrase,
    singular_subtype,
};
use crate::parser::value::parse_small_number;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordParamKind {
    None,
    Cost,
    Number,
    Quality,
    CardName,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordImplKind {
    Primitive,
    CastOption,
    Trigger,
    Static,
    Activated,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostParseMode {
    CastOption,
    OptionalCost,
    Activated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltCostMeta {
    pub id: &'static str,
    pub zones: ZoneMask,
    pub requires_zone_permission: bool,
    pub disallow_land: bool,
    pub exile_on_leave_stack: bool,
    pub cast_transformed_back_face: bool,
    pub targets_become_each: bool,
    pub face_down_ward_cost: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct KeywordSpec {
    pub name: &'static str,
    pub cr: &'static str,
    pub param_kind: KeywordParamKind,
    pub impl_kind: KeywordImplKind,
    pub grantable: bool,
    pub implemented: bool,
    pub has_display_label: bool,
    pub cost_parse_mode: Option<CostParseMode>,
    pub activated_id_prefix: Option<&'static str>,
    pub optional_parameter: bool,
    pub alt_cost: Option<AltCostMeta>,
}

const fn simple(name: &'static str, cr: &'static str, grantable: bool) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::None,
        impl_kind: KeywordImplKind::Primitive,
        grantable,
        implemented: true,
        has_display_label: true,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

const fn simple_kind(
    name: &'static str,
    cr: &'static str,
    impl_kind: KeywordImplKind,
    grantable: bool,
) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::None,
        impl_kind,
        grantable,
        implemented: true,
        has_display_label: true,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

const fn landwalk(name: &'static str) -> KeywordSpec {
    simple(name, "702.14", true)
}

const fn kw_num(
    name: &'static str,
    cr: &'static str,
    impl_kind: KeywordImplKind,
    grantable: bool,
    optional_parameter: bool,
) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::Number,
        impl_kind,
        grantable,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter,
        alt_cost: None,
    }
}

const fn kw_cost(
    name: &'static str,
    cr: &'static str,
    impl_kind: KeywordImplKind,
    grantable: bool,
) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::Cost,
        impl_kind,
        grantable,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

const fn kw_text(name: &'static str, cr: &'static str, impl_kind: KeywordImplKind) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::Text,
        impl_kind,
        grantable: false,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

const fn kw_qual(
    name: &'static str,
    cr: &'static str,
    impl_kind: KeywordImplKind,
    grantable: bool,
    has_display_label: bool,
) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::Quality,
        impl_kind,
        grantable,
        implemented: true,
        has_display_label,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

#[allow(clippy::too_many_arguments)]
const fn kw_alt(
    name: &'static str,
    cr: &'static str,
    id: &'static str,
    zones: ZoneMask,
    requires_zone_permission: bool,
    disallow_land: bool,
    exile_on_leave_stack: bool,
    cast_transformed_back_face: bool,
    targets_become_each: bool,
    face_down_ward_cost: Option<&'static str>,
    cost_parse_mode: Option<CostParseMode>,
) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::Cost,
        impl_kind: KeywordImplKind::CastOption,
        grantable: false,
        implemented: true,
        has_display_label: false,
        cost_parse_mode,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: Some(AltCostMeta {
            id,
            zones,
            requires_zone_permission,
            disallow_land,
            exile_on_leave_stack,
            cast_transformed_back_face,
            targets_become_each,
            face_down_ward_cost,
        }),
    }
}

const fn unimpl(name: &'static str, cr: &'static str) -> KeywordSpec {
    KeywordSpec {
        name,
        cr,
        param_kind: KeywordParamKind::None,
        impl_kind: KeywordImplKind::None,
        grantable: false,
        implemented: false,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    }
}

const fn mask_hand_cmd_exile() -> ZoneMask {
    ZoneMask::from_bits_truncate(
        ZoneMask::HAND.bits() | ZoneMask::COMMAND.bits() | ZoneMask::EXILE.bits(),
    )
}

const fn mask_all_cast() -> ZoneMask {
    ZoneMask::from_bits_truncate(
        ZoneMask::HAND.bits()
            | ZoneMask::COMMAND.bits()
            | ZoneMask::EXILE.bits()
            | ZoneMask::GRAVEYARD.bits()
            | ZoneMask::LIBRARY.bits(),
    )
}

/// Complete 218-entry CR 702 keyword registry (214 implemented + 4 unimplemented).
pub static KEYWORD_SPECS: [KeywordSpec; 218] = [
    // Simple parameterless (alphabetical/logical order from keywords.ts)
    simple("flying", "702.9", true),
    simple("first strike", "702.7", true),
    simple("double strike", "702.4", true),
    simple("deathtouch", "702.2", true),
    simple("defender", "702.3", true),
    simple("flash", "702.8", true),
    simple("haste", "702.10", true),
    kw_qual("hexproof", "702.11", KeywordImplKind::Primitive, true, true),
    simple("indestructible", "702.12", true),
    simple("infect", "702.90", true),
    simple("lifelink", "702.15", true),
    simple("menace", "702.111", true),
    simple("phasing", "702.26", true),
    simple("reach", "702.17", true),
    simple("shroud", "702.18", true),
    simple("trample", "702.19", true),
    simple("trample over planeswalkers", "702.19c", true),
    simple("vigilance", "702.20", true),
    simple("changeling", "702.73", true),
    simple("fear", "702.36", true),
    simple("intimidate", "702.13", true),
    simple("wither", "702.80", true),
    simple("horsemanship", "702.31", true),
    simple("shadow", "702.28", true),
    simple("skulk", "702.118", true),
    simple_kind("prowess", "702.108", KeywordImplKind::Trigger, true),
    kw_qual("enchant", "702.5", KeywordImplKind::Primitive, false, false),
    kw_cost("equip", "702.6", KeywordImplKind::Activated, false),
    kw_qual(
        "protection",
        "702.16",
        KeywordImplKind::Primitive,
        true,
        false,
    ),
    kw_cost("ward", "702.21", KeywordImplKind::Trigger, true),
    kw_alt(
        "flashback",
        "702.34",
        "alt:flashback",
        ZoneMask::GRAVEYARD,
        false,
        false,
        true,
        false,
        false,
        None,
        None,
    ),
    KeywordSpec {
        name: "fuse",
        cr: "702.102",
        param_kind: KeywordParamKind::None,
        impl_kind: KeywordImplKind::CastOption,
        grantable: false,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    },
    kw_cost("kicker", "702.33", KeywordImplKind::CastOption, false),
    kw_cost("multikicker", "702.33c", KeywordImplKind::CastOption, false),
    kw_alt(
        "overload",
        "702.96",
        "alt:overload",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        true,
        None,
        None,
    ),
    kw_cost("suspend", "702.62", KeywordImplKind::CastOption, false),
    KeywordSpec {
        name: "partner",
        cr: "702.124",
        param_kind: KeywordParamKind::CardName,
        impl_kind: KeywordImplKind::Primitive,
        grantable: false,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: None,
        activated_id_prefix: None,
        optional_parameter: false,
        alt_cost: None,
    },
    simple_kind("battle cry", "702.91", KeywordImplKind::Trigger, true),
    kw_num(
        "afterlife",
        "702.135",
        KeywordImplKind::Trigger,
        false,
        false,
    ),
    kw_num("bushido", "702.45", KeywordImplKind::Trigger, false, false),
    simple_kind("living weapon", "702.92", KeywordImplKind::Trigger, false),
    simple_kind("for mirrodin!", "702.163", KeywordImplKind::Trigger, false),
    kw_qual("affinity", "702.41", KeywordImplKind::Static, false, false),
    simple_kind("exploit", "702.110", KeywordImplKind::Trigger, false),
    simple_kind("exalted", "702.83", KeywordImplKind::Trigger, true),
    kw_num(
        "annihilator",
        "702.86",
        KeywordImplKind::Trigger,
        false,
        false,
    ),
    landwalk("landwalk"),
    landwalk("plainswalk"),
    landwalk("islandwalk"),
    landwalk("swampwalk"),
    landwalk("mountainwalk"),
    landwalk("forestwalk"),
    landwalk("nonbasic landwalk"),
    landwalk("snow landwalk"),
    landwalk("snow plainswalk"),
    landwalk("snow islandwalk"),
    landwalk("snow swampwalk"),
    landwalk("snow mountainwalk"),
    landwalk("snow forestwalk"),
    landwalk("legendary landwalk"),
    landwalk("artifact landwalk"),
    kw_num("absorb", "702.64", KeywordImplKind::Primitive, true, false),
    simple_kind("aftermath", "702.127", KeywordImplKind::CastOption, false),
    kw_num("afflict", "702.130", KeywordImplKind::Trigger, true, false),
    kw_num("amplify", "702.38", KeywordImplKind::Static, false, false),
    simple_kind("ascend", "702.131", KeywordImplKind::Static, true),
    simple_kind("assist", "702.132", KeywordImplKind::CastOption, false),
    kw_cost("aura swap", "702.65", KeywordImplKind::Activated, false),
    simple("choose a background", "702.124k", false),
    kw_text("awaken", "702.113", KeywordImplKind::CastOption),
    kw_num("backup", "702.165", KeywordImplKind::Trigger, false, false),
    simple("banding", "702.22", true),
    simple_kind("bargain", "702.166", KeywordImplKind::CastOption, false),
    kw_cost("beam me up", "702.196", KeywordImplKind::CastOption, false),
    kw_cost("bestow", "702.103", KeywordImplKind::CastOption, false),
    kw_alt(
        "blitz",
        "702.152",
        "alt:blitz",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        false,
        None,
        None,
    ),
    kw_num(
        "bloodthirst",
        "702.54",
        KeywordImplKind::Static,
        false,
        false,
    ),
    kw_text("boast", "702.142", KeywordImplKind::Static),
    kw_cost("buyback", "702.27", KeywordImplKind::CastOption, false),
    simple_kind("cascade", "702.85", KeywordImplKind::Trigger, false),
    kw_num(
        "casualty",
        "702.153",
        KeywordImplKind::CastOption,
        false,
        false,
    ),
    kw_qual("champion", "702.72", KeywordImplKind::Trigger, false, false),
    simple_kind("cipher", "702.99", KeywordImplKind::CastOption, false),
    kw_cost("cleave", "702.148", KeywordImplKind::CastOption, false),
    kw_text("companion", "702.139", KeywordImplKind::Static),
    kw_cost(
        "commander ninjutsu",
        "702.49d",
        KeywordImplKind::Activated,
        false,
    ),
    simple_kind("compleated", "702.150", KeywordImplKind::Static, false),
    simple_kind("conspire", "702.78", KeywordImplKind::CastOption, false),
    simple_kind("convoke", "702.51", KeywordImplKind::Static, false),
    kw_text("craft", "702.167", KeywordImplKind::Activated),
    kw_num("crew", "702.122", KeywordImplKind::Activated, false, false),
    kw_cost(
        "cumulative upkeep",
        "702.24",
        KeywordImplKind::Trigger,
        false,
    ),
    kw_cost("cycling", "702.29", KeywordImplKind::Activated, false),
    kw_alt(
        "dash",
        "702.109",
        "alt:dash",
        mask_hand_cmd_exile(),
        true,
        true,
        false,
        false,
        false,
        None,
        None,
    ),
    simple_kind("daybound", "702.145", KeywordImplKind::Static, false),
    simple_kind("decayed", "702.147", KeywordImplKind::Trigger, true),
    simple_kind("delve", "702.66", KeywordImplKind::Static, false),
    simple("demonstrate", "702.144", false),
    simple("doctor's companion", "702.124m", false),
    simple_kind("dethrone", "702.105", KeywordImplKind::Trigger, true),
    simple_kind("devoid", "702.114", KeywordImplKind::Static, false),
    kw_num("devour", "702.82", KeywordImplKind::Static, false, false),
    kw_alt(
        "disguise",
        "702.168",
        "alt:disguise",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        false,
        Some("{2}"),
        None,
    ),
    kw_alt(
        "disturb",
        "702.146",
        "alt:disturb",
        ZoneMask::GRAVEYARD,
        false,
        true,
        false,
        true,
        false,
        None,
        None,
    ),
    unimpl("double team", "digital:double-team"),
    kw_num("dredge", "702.52", KeywordImplKind::Static, false, false),
    kw_cost("echo", "702.30", KeywordImplKind::Trigger, false),
    kw_cost("embalm", "702.128", KeywordImplKind::Activated, false),
    kw_cost("emerge", "702.119", KeywordImplKind::CastOption, false),
    kw_cost("encore", "702.141", KeywordImplKind::Activated, false),
    simple_kind("enlist", "702.154", KeywordImplKind::Trigger, true),
    kw_cost("entwine", "702.42", KeywordImplKind::CastOption, false),
    simple("epic", "702.50", false),
    kw_cost("escalate", "702.120", KeywordImplKind::CastOption, false),
    kw_alt(
        "escape",
        "702.138",
        "alt:escape",
        ZoneMask::GRAVEYARD,
        false,
        true,
        false,
        false,
        false,
        None,
        None,
    ),
    kw_cost("eternalize", "702.129", KeywordImplKind::Activated, false),
    kw_alt(
        "evoke",
        "702.74",
        "alt:evoke",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        false,
        None,
        None,
    ),
    simple_kind("evolve", "702.100", KeywordImplKind::Trigger, true),
    simple_kind("extort", "702.101", KeywordImplKind::Trigger, true),
    kw_num(
        "fabricate",
        "702.123",
        KeywordImplKind::Trigger,
        false,
        false,
    ),
    kw_num("fading", "702.32", KeywordImplKind::Static, false, false),
    simple_kind("flanking", "702.25", KeywordImplKind::Trigger, true),
    kw_text("forecast", "702.57", KeywordImplKind::Static),
    kw_cost("foretell", "702.143", KeywordImplKind::CastOption, false),
    KeywordSpec {
        name: "fortify",
        cr: "702.67",
        param_kind: KeywordParamKind::Cost,
        impl_kind: KeywordImplKind::Activated,
        grantable: false,
        implemented: true,
        has_display_label: false,
        cost_parse_mode: Some(CostParseMode::Activated),
        activated_id_prefix: Some("fortify"),
        optional_parameter: false,
        alt_cost: None,
    },
    kw_num("frenzy", "702.68", KeywordImplKind::Trigger, true, false),
    kw_cost("freerunning", "702.173", KeywordImplKind::CastOption, false),
    simple("friends forever", "702.124j", false),
    kw_qual("gift", "702.174", KeywordImplKind::CastOption, false, false),
    kw_num("graft", "702.58", KeywordImplKind::Trigger, false, false),
    simple_kind("gravestorm", "702.69", KeywordImplKind::Trigger, false),
    simple_kind("haunt", "702.55", KeywordImplKind::Trigger, false),
    unimpl("hidden agenda", "702.106"),
    kw_num("hideaway", "702.75", KeywordImplKind::Trigger, false, true),
    simple_kind("improvise", "702.126", KeywordImplKind::Static, false),
    kw_text("impending", "702.176", KeywordImplKind::CastOption),
    simple("increment", "702.191", true),
    simple_kind("ingest", "702.115", KeywordImplKind::Trigger, true),
    simple_kind("jump-start", "702.133", KeywordImplKind::CastOption, false),
    kw_cost(
        "basic landcycling",
        "702.29e",
        KeywordImplKind::Activated,
        false,
    ),
    kw_cost("landcycling", "702.29e", KeywordImplKind::Activated, false),
    kw_cost("typecycling", "702.29e", KeywordImplKind::Activated, false),
    kw_cost("level up", "702.87", KeywordImplKind::Activated, false),
    simple_kind("living metal", "702.161", KeywordImplKind::Static, true),
    kw_cost("madness", "702.35", KeywordImplKind::Trigger, false),
    kw_alt(
        "megamorph",
        "702.37b",
        "alt:megamorph",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        false,
        None,
        None,
    ),
    kw_cost("mayhem", "702.187", KeywordImplKind::CastOption, false),
    simple_kind("melee", "702.121", KeywordImplKind::Trigger, true),
    simple_kind("mentor", "702.134", KeywordImplKind::Trigger, true),
    kw_cost("miracle", "702.94", KeywordImplKind::CastOption, false),
    kw_num("modular", "702.43", KeywordImplKind::Trigger, false, false),
    kw_alt(
        "morph",
        "702.37",
        "alt:morph",
        mask_hand_cmd_exile(),
        true,
        false,
        false,
        false,
        false,
        None,
        None,
    ),
    kw_cost(
        "more than meets the eye",
        "702.162",
        KeywordImplKind::CastOption,
        false,
    ),
    kw_alt(
        "mutate",
        "702.140",
        "alt:mutate",
        mask_all_cast(),
        true,
        true,
        false,
        false,
        false,
        None,
        Some(CostParseMode::CastOption),
    ),
    simple_kind("myriad", "702.116", KeywordImplKind::Trigger, true),
    simple_kind("nightbound", "702.145", KeywordImplKind::Static, false),
    kw_cost("ninjutsu", "702.49", KeywordImplKind::Activated, false),
    kw_qual("offering", "702.48", KeywordImplKind::Static, false, false),
    kw_cost("offspring", "702.175", KeywordImplKind::CastOption, false),
    kw_cost("outlast", "702.107", KeywordImplKind::Activated, false),
    simple_kind("persist", "702.79", KeywordImplKind::Trigger, true),
    kw_cost("plot", "702.170", KeywordImplKind::CastOption, false),
    kw_num("poisonous", "702.70", KeywordImplKind::Trigger, true, false),
    simple_kind("paradigm", "702.192", KeywordImplKind::CastOption, false),
    kw_text("power-up", "702.193", KeywordImplKind::Static),
    kw_text("prototype", "702.160", KeywordImplKind::CastOption),
    simple("provoke", "702.39", true),
    kw_cost("prowl", "702.76", KeywordImplKind::CastOption, false),
    kw_num("rampage", "702.23", KeywordImplKind::Trigger, true, false),
    simple_kind("ravenous", "702.156", KeywordImplKind::Trigger, false),
    simple_kind("read ahead", "702.155", KeywordImplKind::Static, false),
    simple_kind("rebound", "702.88", KeywordImplKind::Static, false),
    kw_cost("reconfigure", "702.151", KeywordImplKind::Activated, false),
    kw_cost("recover", "702.59", KeywordImplKind::Trigger, false),
    kw_text("reinforce", "702.77", KeywordImplKind::Activated),
    kw_num("renown", "702.112", KeywordImplKind::Trigger, true, false),
    kw_cost("replicate", "702.56", KeywordImplKind::CastOption, false),
    simple_kind("retrace", "702.81", KeywordImplKind::CastOption, false),
    simple_kind("riot", "702.136", KeywordImplKind::Static, true),
    kw_num("ripple", "702.60", KeywordImplKind::Primitive, false, false),
    kw_num(
        "saddle",
        "702.171",
        KeywordImplKind::Activated,
        false,
        false,
    ),
    kw_cost("scavenge", "702.97", KeywordImplKind::Activated, false),
    simple_kind("soulbond", "702.95", KeywordImplKind::Trigger, true),
    kw_num(
        "soulshift",
        "702.46",
        KeywordImplKind::Trigger,
        false,
        false,
    ),
    unimpl("space sculptor", "702.158"),
    kw_cost("spectacle", "702.137", KeywordImplKind::CastOption, false),
    kw_text("splice", "702.47", KeywordImplKind::CastOption),
    simple_kind(
        "start your engines!",
        "702.179",
        KeywordImplKind::Static,
        true,
    ),
    simple("split second", "702.61", true),
    kw_cost("squad", "702.157", KeywordImplKind::CastOption, false),
    simple_kind("storm", "702.40", KeywordImplKind::Trigger, false),
    simple_kind("sunburst", "702.44", KeywordImplKind::Static, false),
    kw_cost("surge", "702.117", KeywordImplKind::CastOption, false),
    simple_kind("totem armor", "702.89b", KeywordImplKind::Static, true),
    simple_kind("training", "702.149", KeywordImplKind::Trigger, true),
    kw_cost("transfigure", "702.71", KeywordImplKind::Activated, false),
    kw_cost("transmute", "702.53", KeywordImplKind::Activated, false),
    kw_num("tribute", "702.104", KeywordImplKind::Static, false, false),
    simple("umbra armor", "702.89", true),
    simple_kind("undaunted", "702.125", KeywordImplKind::Static, false),
    simple_kind("undying", "702.93", KeywordImplKind::Trigger, true),
    kw_cost("unearth", "702.84", KeywordImplKind::Activated, false),
    simple_kind("unleash", "702.98", KeywordImplKind::Static, true),
    kw_num("vanishing", "702.63", KeywordImplKind::Static, false, false),
    unimpl("visit", "702.159"),
    kw_num("toxic", "702.164", KeywordImplKind::Primitive, true, false),
    simple_kind("station", "702.184", KeywordImplKind::Activated, false),
    kw_text("exhaust", "702.177", KeywordImplKind::Static),
    kw_cost("harmonize", "702.180", KeywordImplKind::CastOption, false),
    kw_num("mobilize", "702.181", KeywordImplKind::Trigger, true, false),
    kw_cost(
        "web-slinging",
        "702.188",
        KeywordImplKind::CastOption,
        false,
    ),
    kw_cost("sneak", "702.190", KeywordImplKind::CastOption, false),
    kw_num(
        "firebending",
        "702.189",
        KeywordImplKind::Trigger,
        true,
        false,
    ),
    kw_cost("waterbend", "701.67", KeywordImplKind::Static, false),
    simple_kind("job select", "702.182", KeywordImplKind::Trigger, false),
    kw_alt(
        "warp",
        "702.185",
        "alt:warp",
        ZoneMask::HAND,
        false,
        true,
        false,
        false,
        false,
        None,
        None,
    ),
    kw_num(
        "teamwork",
        "702.194",
        KeywordImplKind::CastOption,
        false,
        true,
    ),
    simple("storied", "702.195", true),
];

pub static GRANTABLE_KEYWORDS: [&str; 109] = [
    "flying",
    "first strike",
    "double strike",
    "deathtouch",
    "defender",
    "flash",
    "haste",
    "hexproof",
    "indestructible",
    "infect",
    "lifelink",
    "menace",
    "phasing",
    "reach",
    "shroud",
    "trample",
    "trample over planeswalkers",
    "vigilance",
    "changeling",
    "fear",
    "intimidate",
    "wither",
    "horsemanship",
    "shadow",
    "skulk",
    "prowess",
    "battle cry",
    "exalted",
    "landwalk",
    "plainswalk",
    "islandwalk",
    "swampwalk",
    "mountainwalk",
    "forestwalk",
    "nonbasic landwalk",
    "snow landwalk",
    "snow plainswalk",
    "snow islandwalk",
    "snow swampwalk",
    "snow mountainwalk",
    "snow forestwalk",
    "legendary landwalk",
    "artifact landwalk",
    "ascend",
    "banding",
    "dethrone",
    "enlist",
    "evolve",
    "extort",
    "flanking",
    "increment",
    "ingest",
    "living metal",
    "melee",
    "mentor",
    "myriad",
    "persist",
    "provoke",
    "decayed",
    "split second",
    "start your engines!",
    "totem armor",
    "training",
    "umbra armor",
    "undying",
    "unleash",
    "soulbond",
    "riot",
    "storied",
    "toxic 1",
    "toxic 2",
    "toxic 3",
    "toxic 4",
    "toxic 5",
    "toxic 6",
    "afflict 1",
    "afflict 2",
    "afflict 3",
    "afflict 4",
    "firebending 1",
    "firebending 2",
    "firebending 3",
    "firebending 4",
    "mobilize 1",
    "mobilize 2",
    "mobilize 3",
    "mobilize 4",
    "rampage 1",
    "rampage 2",
    "rampage 3",
    "rampage 4",
    "renown 1",
    "renown 2",
    "renown 3",
    "absorb 1",
    "absorb 2",
    "absorb 3",
    "absorb 4",
    "absorb 5",
    "frenzy 1",
    "frenzy 2",
    "frenzy 3",
    "frenzy 4",
    "frenzy 5",
    "poisonous 1",
    "poisonous 2",
    "poisonous 3",
    "poisonous 4",
    "poisonous 5",
];

#[inline]
const fn cmp_ascii_ci(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let min_len = if a.len() < b.len() { a.len() } else { b.len() };
    let mut i = 0;
    while i < min_len {
        let ac = a[i].to_ascii_lowercase();
        let bc = b[i].to_ascii_lowercase();
        if ac < bc {
            return std::cmp::Ordering::Less;
        }
        if ac > bc {
            return std::cmp::Ordering::Greater;
        }
        i += 1;
    }
    if a.len() < b.len() {
        std::cmp::Ordering::Less
    } else if a.len() > b.len() {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

static KEYWORD_SPECS_BY_NAME: [&KeywordSpec; KEYWORD_SPECS.len()] = {
    let mut arr = [&KEYWORD_SPECS[0]; KEYWORD_SPECS.len()];
    let mut i = 0;
    while i < KEYWORD_SPECS.len() {
        arr[i] = &KEYWORD_SPECS[i];
        i += 1;
    }
    let mut j = 1;
    while j < KEYWORD_SPECS.len() {
        let key = arr[j];
        let mut k = j;
        while k > 0
            && matches!(
                cmp_ascii_ci(arr[k - 1].name.as_bytes(), key.name.as_bytes()),
                std::cmp::Ordering::Greater
            )
        {
            arr[k] = arr[k - 1];
            k -= 1;
        }
        arr[k] = key;
        j += 1;
    }
    arr
};

static KEYWORD_SPECS_LONGEST_FIRST: [&KeywordSpec; KEYWORD_SPECS.len()] = {
    let mut arr = [&KEYWORD_SPECS[0]; KEYWORD_SPECS.len()];
    let mut i = 0;
    while i < KEYWORD_SPECS.len() {
        arr[i] = &KEYWORD_SPECS[i];
        i += 1;
    }
    let mut j = 1;
    while j < KEYWORD_SPECS.len() {
        let key = arr[j];
        let mut k = j;
        while k > 0 {
            let prev = arr[k - 1];
            let swap = prev.name.len() < key.name.len()
                || (prev.name.len() == key.name.len()
                    && matches!(
                        cmp_ascii_ci(prev.name.as_bytes(), key.name.as_bytes()),
                        std::cmp::Ordering::Greater
                    ));
            if !swap {
                break;
            }
            arr[k] = prev;
            k -= 1;
        }
        arr[k] = key;
        j += 1;
    }
    arr
};

pub fn known_keywords_longest_first() -> &'static [&'static KeywordSpec] {
    &KEYWORD_SPECS_LONGEST_FIRST
}

pub fn get_keyword_spec(name: &str) -> Option<&'static KeywordSpec> {
    let trimmed = name.trim().as_bytes();
    if trimmed.is_empty() {
        return None;
    }
    KEYWORD_SPECS_BY_NAME
        .binary_search_by(|spec| cmp_ascii_ci(spec.name.as_bytes(), trimmed))
        .ok()
        .map(|idx| KEYWORD_SPECS_BY_NAME[idx])
}

pub fn is_known_keyword(name: &str) -> bool {
    get_keyword_spec(name).is_some()
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedKw {
    pub name: String,
    pub cost: Option<ManaCost>,
    pub param: Option<String>,
    pub n: Option<i32>,
    pub cycling_filter: Option<Box<ObjectFilter>>,
    pub cycling_desc: Option<String>,
    pub prototype_spec: Option<PrototypeSpec>,
}

#[inline]
pub(crate) fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[inline]
pub(crate) fn is_pure_mana_braces(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b[0] != b'{' {
        return false;
    }
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'{' {
            return false;
        }
        let Some(rel_end) = s[i + 1..].find('}') else {
            return false;
        };
        if rel_end == 0 {
            return false;
        }
        i += 2 + rel_end;
    }
    true
}

fn is_dash_char(c: char) -> bool {
    matches!(c, '—' | '–' | '-')
}

fn trim_leading_dash_ws(s: &str) -> &str {
    let mut t = s.trim_start();
    if let Some(c) = t.chars().next()
        && is_dash_char(c)
    {
        t = t[c.len_utf8()..].trim_start();
    }
    t
}

fn is_cost_trailing_period_stripped_kw(kw: &str, spec: &KeywordSpec) -> bool {
    spec.param_kind == KeywordParamKind::Cost
        || matches!(
            kw,
            "prototype"
                | "reinforce"
                | "impending"
                | "awaken"
                | "devour"
                | "partner"
                | "gift"
                | "splice"
                | "craft"
        )
        || spec.cost_parse_mode.is_some()
}

#[inline]
fn has_dash_byte(s: &str) -> bool {
    s.as_bytes().iter().any(|&b| b == b'-' || b >= 0xE2)
}

pub fn is_dash_cost_keyword_line(body: &str) -> bool {
    if !has_dash_byte(body) {
        return false;
    }
    for spec in known_keywords_longest_first() {
        if spec.param_kind != KeywordParamKind::Cost {
            continue;
        }
        if starts_with_ci(body, spec.name) {
            let rest = body[spec.name.len()..].trim_start();
            if rest.chars().next().is_some_and(is_dash_char) {
                return true;
            }
        }
    }
    false
}

fn is_single_keyword_part(seg: &str) -> bool {
    for p in [
        "enchant ",
        "protection from ",
        "hexproof from ",
        "partner with ",
        "craft with ",
        "gift a ",
        "gift an ",
        "splice onto ",
        "prototype ",
        "devour x",
    ] {
        if starts_with_ci(seg, p) {
            return true;
        }
    }
    if starts_with_ci(seg, "equip ") {
        let after_eq = &seg["equip ".len()..];
        if let Some(comma) = after_eq.find(',')
            && !after_eq[..comma].contains('{')
            && after_eq[comma + 1..]
                .trim_start()
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            return true;
        }
    }
    if !has_dash_byte(seg) {
        return false;
    }
    if is_dash_cost_keyword_line(seg) {
        return true;
    }
    // Check cycling or parameterized keywords followed by dash (`reinforce N —`, `impending N —`, `awaken N —`, `equip ... —`, `<type>cycling —`)
    if seg.len() >= 8
        && let Some(cycling_idx) = seg
            .as_bytes()
            .windows(7)
            .position(|w| w.eq_ignore_ascii_case(b"cycling"))
        && seg.is_char_boundary(cycling_idx)
    {
        let head = &seg[..cycling_idx];
        if (head.is_empty()
            || head
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-'))
            && seg[cycling_idx + "cycling".len()..]
                .trim_start()
                .chars()
                .next()
                .is_some_and(is_dash_char)
        {
            return true;
        }
    }
    for prefix in [
        "equip",
        "reinforce",
        "awaken",
        "impending",
        "craft",
        "partner",
    ] {
        if starts_with_ci(seg, prefix) {
            let rest = &seg[prefix.len()..];
            if (rest.starts_with(' ') || rest.chars().next().is_some_and(is_dash_char))
                && let Some(dash_pos) = rest.find(is_dash_char)
            {
                let before = rest[..dash_pos].trim();
                if prefix == "equip"
                    || before.is_empty()
                    || before.bytes().all(|b| b.is_ascii_digit())
                {
                    return true;
                }
            }
        }
    }
    false
}

pub fn split_keyword_line_parts(body: &str) -> Vec<&str> {
    let semi_segments: Vec<&str> = if body.contains(';') && !body.contains(['"', '“', '”']) {
        body.split(';').collect()
    } else {
        vec![body]
    };
    let mut out = Vec::new();
    for seg in semi_segments {
        let trimmed = seg.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_dash_cost_keyword_line(trimmed) || is_single_keyword_part(trimmed) {
            out.push(trimmed);
        } else {
            for p in trimmed.split(',') {
                let pt = p.trim();
                if !pt.is_empty() {
                    out.push(pt);
                }
            }
        }
    }
    out
}

fn match_companion_condition(raw: &str) -> Option<CompanionId> {
    let s = raw.trim().trim_end_matches('.').trim().to_ascii_lowercase();
    let norm = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match norm.as_str() {
        "each nonland card in your starting deck has an even mana value"
        | "your starting deck contains only cards with even mana values" => {
            Some(CompanionId::Gyruda)
        }
        "no card in your starting deck has more than one of the same mana symbol in its mana cost" => {
            Some(CompanionId::Jegantha)
        }
        "each creature card in your starting deck is a cat, elemental, nightmare, dinosaur, or beast card" => {
            Some(CompanionId::Kaheera)
        }
        "your starting deck contains only land cards and cards with mana value 3 or greater"
        | "your starting deck contains only cards with mana value 3 or greater and land cards" => {
            Some(CompanionId::Keruga)
        }
        "each permanent card in your starting deck has mana value 2 or less" => {
            Some(CompanionId::Lurrus)
        }
        "each nonland card in your starting deck has a different name" => Some(CompanionId::Lutri),
        "your starting deck contains only land cards and cards with odd mana values"
        | "your starting deck contains only cards with odd mana values and land cards" => {
            Some(CompanionId::Obosh)
        }
        "each nonland card in your starting deck shares a card type" => Some(CompanionId::Umori),
        "your starting deck contains at least twenty cards more than the minimum deck size" => {
            Some(CompanionId::Yorion)
        }
        "each permanent card in your starting deck has an activated ability" => {
            Some(CompanionId::Zirda)
        }
        _ => None,
    }
}

fn unsupported(reason: UnsupportedReason, text: &str, span: Span) -> AbilityDef {
    AbilityDef::Unsupported(UnsupportedAbility {
        reason,
        text: text.to_string(),
        span,
        unparsed: vec![span],
        id: None,
        cost: Vec::new(),
        cost_complete: None,
        cost_keyword: None,
        timing: None,
        is_mana: false,
        effect: Vec::new(),
        targets: Vec::new(),
        activation_restrictions: Vec::new(),
        activation_condition: None,
        activator_who: None,
        power_up_reduction: false,
        x_cant_be_zero: false,
        activation_cost_reduction: None,
        cast_restrictions: Vec::new(),
        cast_condition: None,
        trigger: None,
        condition: None,
        grant_condition: None,
    })
}

fn parse_frame_keyword_line(
    ctx: &mut ParseCtx<'_>,
    line: &NormalizedLine,
) -> Option<Vec<AbilityDef>> {
    if line.ability_word.is_some() {
        return None;
    }
    let trimmed = line.body.trim();

    // 1. Companion — <condition>
    if starts_with_ci(trimmed, "companion") {
        let rest = trimmed["companion".len()..].trim_start();
        if let Some(after_dash) = rest.strip_prefix('—') {
            let cond_body = after_dash.trim();
            if let Some(companion) = match_companion_condition(cond_body) {
                let kw_id = ctx.interner.intern_keyword("companion");
                return Some(vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: KeywordDef {
                            keyword: kw_id,
                            cost: None,
                            param: Some(cond_body.to_string()),
                            n: None,
                            cost_defs: Vec::new(),
                            kicker_costs: Vec::new(),
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
                            craft_materials: Vec::new(),
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
                        span: line.span,
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CompanionCondition { companion },
                            condition: None,
                        },
                        span: line.span,
                    }),
                ]);
            }
            return Some(vec![unsupported(
                UnsupportedReason::KeywordNotImplemented,
                &line.text,
                line.span,
            )]);
        }
    }

    // 2. <Quality> offering (no period)
    if !trimmed.ends_with('.')
        && let Some(off_idx) = trimmed.len().checked_sub("offering".len())
        && off_idx > 0
        && trimmed.is_char_boundary(off_idx)
        && trimmed[off_idx..].eq_ignore_ascii_case("offering")
        && trimmed[..off_idx].ends_with(char::is_whitespace)
    {
        let qual_raw = trimmed[..off_idx].trim();
        if !qual_raw.is_empty()
            && qual_raw
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-')
        {
            let filter = parse_multi_or_object_phrase(
                qual_raw,
                ctx,
                FilterOpts {
                    default_zone: ZoneMask::empty(),
                    allow_plural_and_disjunction: false,
                    allow_spell_noun: false,
                    strip_trailing_card: false,
                    expand_outlaw_subtypes: false,
                },
            );
            if let Some(f) = filter
                && f != ObjectFilter::default()
            {
                let kw_id = ctx.interner.intern_keyword("offering");
                return Some(vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_empty_kw_def(kw_id, Some(qual_raw.to_string())),
                        id: None,
                        span: line.span,
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Offering {
                                filter: Box::new(f),
                                quality: qual_raw.to_string(),
                            },
                            condition: None,
                        },
                        span: line.span,
                    }),
                ]);
            }
            return Some(vec![unsupported(
                UnsupportedReason::KeywordNotImplemented,
                &line.text,
                line.span,
            )]);
        }
    }

    // 3. Modular—Sunburst
    if starts_with_ci(trimmed, "modular") {
        let rest = trim_leading_dash_ws(&trimmed["modular".len()..]);
        if rest.eq_ignore_ascii_case("sunburst") {
            let sun_id = ctx.interner.intern_keyword("sunburst");
            let mod_id = ctx.interner.intern_keyword("modular");
            let mut mod_kw = make_empty_kw_def(mod_id, None);
            mod_kw.n = Some(0);
            return Some(vec![
                AbilityDef::Keyword(KeywordAbility {
                    keyword: make_empty_kw_def(sun_id, None),
                    id: None,
                    span: line.span,
                }),
                AbilityDef::Keyword(KeywordAbility {
                    keyword: mod_kw,
                    id: None,
                    span: line.span,
                }),
            ]);
        }
    }

    // 4. Suspend X—{cost}[. X can't be 0.]
    if starts_with_ci(trimmed, "suspend") {
        let rest = trimmed["suspend".len()..].trim_start();
        if let Some(after_x) = rest.strip_prefix(['X', 'x']) {
            let after_dash = trim_leading_dash_ws(after_x);
            let cost_part = if let Some(dot) = after_dash.find('.') {
                let tail = after_dash[dot + 1..].trim().trim_end_matches('.');
                if tail.eq_ignore_ascii_case("x can't be 0")
                    || tail.eq_ignore_ascii_case("x can’t be 0")
                {
                    after_dash[..dot].trim()
                } else {
                    ""
                }
            } else {
                after_dash.trim()
            };
            if is_pure_mana_braces(cost_part) {
                let kw_id = ctx.interner.intern_keyword("suspend");
                let mut kd = make_empty_kw_def(kw_id, None);
                kd.cost = Some(ManaCost::parse(cost_part));
                kd.suspend_x = true;
                return Some(vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: kd,
                    id: None,
                    span: line.span,
                })]);
            }
        }
    }

    // 5. Space sculptor[.]
    {
        let no_dot = trimmed.trim_end_matches('.').trim();
        let parts: Vec<&str> = no_dot.split_whitespace().collect();
        if parts.len() == 2
            && parts[0].eq_ignore_ascii_case("space")
            && parts[1].eq_ignore_ascii_case("sculptor")
        {
            return Some(vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::SpaceSculptorSectors,
                    condition: None,
                },
                span: line.span,
            })]);
        }
    }

    // 6. Inline Equip cost reduction: "Equip {3}. This ability costs {1} less to activate [for each ...]."
    if starts_with_ci(trimmed, "equip")
        && let Some(dot) = trimmed.find(". ")
    {
        let equip_head = trimmed[..dot].trim();
        let tail = trimmed[dot + 2..].trim().trim_end_matches('.').trim();
        if starts_with_ci(tail, "this ability costs ") {
            let tail_toks = tokenize(tail, 0);
            if tail_toks.len() >= 8
                && tail_toks[0].eq_ci("this")
                && tail_toks[1].eq_ci("ability")
                && tail_toks[2].eq_ci("costs")
                && let crate::lexer::TokenKind::ManaSymbol(crate::lexer::ManaSymbolKind::Generic(
                    gen_val,
                )) = tail_toks[3].kind
                && gen_val >= 1
                && tail_toks[4].eq_ci("less")
                && tail_toks[5].eq_ci("to")
                && tail_toks[6].eq_ci("activate")
            {
                let per_amt = if tail_toks.len() == 7 {
                    Some(None)
                } else if tail_toks.len() > 9
                    && tail_toks[7].eq_ci("for")
                    && tail_toks[8].eq_ci("each")
                {
                    let f_toks = &tail_toks[9..];
                    let cp = ctx.interner.checkpoint();
                    let mut sub_ctx = ctx.with_normalized_text(tail);
                    let parsed = crate::parser::filter::parse_object_filter(
                        f_toks,
                        &mut sub_ctx,
                        FilterOpts {
                            default_zone: ZoneMask::BATTLEFIELD,
                            allow_plural_and_disjunction: true,
                            ..Default::default()
                        },
                        &crate::parser::filter::NullScope,
                    )
                    .filter(|(_, u)| *u == f_toks.len())
                    .map(|(f, _)| Some(Box::new(Amount::Count { of: Box::new(f) })));
                    if parsed.is_none() {
                        ctx.interner.rollback(cp);
                    }
                    parsed
                } else {
                    None
                };
                if let Some(per) = per_amt {
                    let head_span = Span {
                        start: line.body_span.start,
                        end: line.body_span.start + equip_head.len() as u32,
                    };
                    let synth_line = NormalizedLine {
                        raw_index: line.raw_index,
                        raw: equip_head.to_string(),
                        text: equip_head.to_string(),
                        ability_word: None,
                        body: equip_head.to_string(),
                        span: head_span,
                        body_span: head_span,
                        sentences: vec![crate::normalize::NormalizedSentence {
                            text: equip_head.to_string(),
                            span: head_span,
                        }],
                    };
                    if let Some(mut equip_abs) = parse_keyword_line(ctx, &synth_line)
                        && equip_abs
                            .iter()
                            .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
                    {
                        let equip_kw_id = ctx.interner.intern_keyword("equip");
                        let delta = CostDelta {
                            mode: CostDeltaMode::Reduce,
                            generic: gen_val as u16,
                            per: per.clone(),
                            additional_costs: Vec::new(),
                            colored: SmallVec::new(),
                            hybrid_colored: SmallVec::new(),
                            colored_only: false,
                            min_total_mana: None,
                            max_reduction: None,
                            per_target_life: None,
                        };
                        for ab in &mut equip_abs {
                            if let AbilityDef::Keyword(ka) = ab
                                && ka.keyword.keyword == equip_kw_id
                            {
                                ka.keyword.equip_cost_reduction =
                                    Some(mtg_ir::ConditionalCostReduction {
                                        delta: delta.clone(),
                                        condition: None,
                                    });
                            }
                        }
                        let tail_span = Span {
                            start: (head_span.end + 2).min(line.span.end),
                            end: line.span.end,
                        };
                        equip_abs.push(AbilityDef::Static(StaticAbility {
                            static_def: StaticDef {
                                body: StaticBody::CostModifier {
                                    affects: SpellOrAbilityFilter::Ability {
                                        keyword: Some(equip_kw_id),
                                        filter: None,
                                        non_mana_only: false,
                                        loyalty_only: false,
                                        source_zone: None,
                                        self_source_only: true,
                                        attached_source_only: false,
                                        targets_self: false,
                                        targets_attached: false,
                                        targets_filter: None,
                                        first_matching_this_turn: false,
                                    },
                                    delta,
                                    who: PlayerFilter {
                                        relation: PlayerRelation::You,
                                    },
                                    who_enchanted_player: false,
                                    active_zones: ZoneMask::empty(),
                                },
                                condition: None,
                            },
                            span: tail_span,
                        }));
                        return Some(equip_abs);
                    }
                }
            }
        }
    }

    // 7. Dynamic parameterized keywords: "Mobilize X, where X is ..." / "Firebending X, where X is ..."
    if (starts_with_ci(trimmed, "mobilize x, where x is ")
        || starts_with_ci(trimmed, "firebending x, where x is "))
        && let Some(kd) = crate::parser::static_def::parse_dynamic_keyword_grant_def(trimmed, ctx)
    {
        return Some(vec![AbilityDef::Static(StaticAbility {
            static_def: StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::GrantAbility {
                        ability: Box::new(AbilityDef::Keyword(KeywordAbility {
                            keyword: kd,
                            id: None,
                            span: line.span,
                        })),
                    },
                    active_in_zone: None,
                },
                condition: None,
            },
            span: line.span,
        })]);
    }

    None
}

pub(crate) fn make_empty_kw_def(keyword: mtg_ir::KeywordId, param: Option<String>) -> KeywordDef {
    KeywordDef {
        keyword,
        cost: None,
        param,
        n: None,
        cost_defs: Vec::new(),
        kicker_costs: Vec::new(),
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
        craft_materials: Vec::new(),
        craft_any_number: false,
        craft_share_card_type: false,
        splice_filter: None,
        impending_n: None,
        landwalk_filter: None,
        suspend_x: false,
        equip_cost_reduction: None,
        dynamic_amount: None,
    }
}

fn parse_keyword_part(part: &str, ctx: &mut ParseCtx<'_>) -> Option<ParsedKw> {
    for spec in known_keywords_longest_first() {
        let kw = spec.name;
        if part.eq_ignore_ascii_case(kw) {
            return Some(ParsedKw {
                name: kw.to_string(),
                cost: None,
                param: None,
                n: None,
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
            });
        }
        if !starts_with_ci(part, kw) {
            continue;
        }
        let rest = &part[kw.len()..];
        let Some(first_c) = rest.chars().next() else {
            continue;
        };
        if !(first_c.is_whitespace() || is_dash_char(first_c)) {
            continue;
        }
        let raw_arg = trim_leading_dash_ws(rest).trim();
        let arg = if is_cost_trailing_period_stripped_kw(kw, spec) {
            raw_arg.trim_end_matches('.').trim()
        } else {
            raw_arg
        };

        if kw == "prototype"
            && let Some(dash_pos) = arg.find(is_dash_char)
        {
            let mc_str = arg[..dash_pos].trim();
            let pt_str = arg[dash_pos + arg[dash_pos..].chars().next()?.len_utf8()..].trim();
            if is_pure_mana_braces(mc_str)
                && !mc_str.to_ascii_uppercase().contains("{X}")
                && let Some(slash) = pt_str.find('/')
                && let (Ok(power), Ok(toughness)) = (
                    pt_str[..slash].trim().parse::<i32>(),
                    pt_str[slash + 1..].trim().parse::<i32>(),
                )
            {
                let mut color_mask = ColorMask::empty();
                for col in derive_colors_from_mana_cost(mc_str) {
                    color_mask |= match col.as_str() {
                        "W" => ColorMask::W,
                        "U" => ColorMask::U,
                        "B" => ColorMask::B,
                        "R" => ColorMask::R,
                        "G" => ColorMask::G,
                        _ => ColorMask::empty(),
                    };
                }
                let mc = ManaCost::parse(mc_str);
                return Some(ParsedKw {
                    name: "prototype".to_string(),
                    cost: Some(mc.clone()),
                    param: None,
                    n: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: Some(PrototypeSpec {
                        mana_cost: mc,
                        power,
                        toughness,
                        colors: color_mask,
                    }),
                });
            }
        }

        if (kw == "reinforce" || kw == "impending")
            && let Some(dash_pos) = arg.find(is_dash_char)
        {
            let left = arg[..dash_pos].trim();
            let right = arg[dash_pos + arg[dash_pos..].chars().next()?.len_utf8()..].trim();
            if !left.is_empty()
                && left.bytes().all(|b| b.is_ascii_digit())
                && let Ok(n) = left.parse::<i32>()
            {
                if is_pure_mana_braces(right) {
                    return Some(ParsedKw {
                        name: kw.to_string(),
                        cost: Some(ManaCost::parse(right)),
                        param: None,
                        n: Some(n),
                        cycling_filter: None,
                        cycling_desc: None,
                        prototype_spec: None,
                    });
                }
                return Some(ParsedKw {
                    name: kw.to_string(),
                    cost: None,
                    param: Some(right.to_string()),
                    n: Some(n),
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                });
            }
        }

        if is_pure_mana_braces(arg) {
            return Some(ParsedKw {
                name: kw.to_string(),
                cost: Some(ManaCost::parse(arg)),
                param: None,
                n: None,
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
            });
        }

        if !arg.is_empty()
            && arg.bytes().all(|b| b.is_ascii_digit())
            && let Ok(n) = arg.parse::<i32>()
        {
            return Some(ParsedKw {
                name: kw.to_string(),
                cost: None,
                param: None,
                n: Some(n),
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
            });
        }

        if let Some(dash_pos) = arg.find(is_dash_char) {
            let left = arg[..dash_pos].trim();
            let right = arg[dash_pos + arg[dash_pos..].chars().next()?.len_utf8()..].trim();
            if !left.is_empty()
                && left.bytes().all(|b| b.is_ascii_digit())
                && let Ok(n) = left.parse::<i32>()
                && is_pure_mana_braces(right)
            {
                return Some(ParsedKw {
                    name: kw.to_string(),
                    cost: Some(ManaCost::parse(right)),
                    param: None,
                    n: Some(n),
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                });
            }
        }

        if (starts_with_ci(arg, "from ") || kw == "enchant" || kw == "protection")
            && !arg.is_empty()
        {
            let stripped = if starts_with_ci(arg, "from ") {
                arg[5..].trim_start()
            } else {
                arg
            };
            return Some(ParsedKw {
                name: kw.to_string(),
                cost: None,
                param: Some(stripped.to_string()),
                n: None,
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
            });
        }

        if !arg.is_empty() && !arg.ends_with('.') {
            return Some(ParsedKw {
                name: kw.to_string(),
                cost: None,
                param: Some(arg.to_string()),
                n: None,
                cycling_filter: None,
                cycling_desc: None,
                prototype_spec: None,
            });
        }
    }

    // Fallback 1: "snow-covered <land>walk"
    if starts_with_ci(part, "snow-covered ") {
        let rest = part["snow-covered ".len()..].trim();
        for lw in ["plains", "island", "swamp", "mountain", "forest", "land"] {
            let target = format!("{lw}walk");
            if rest.eq_ignore_ascii_case(&target) {
                return Some(ParsedKw {
                    name: format!("snow {target}"),
                    cost: None,
                    param: None,
                    n: None,
                    cycling_filter: None,
                    cycling_desc: None,
                    prototype_spec: None,
                });
            }
        }
    }

    // Fallback 2: Typecycling (CR 702.29e)
    let low = part.to_ascii_lowercase();
    if let Some(cyc_idx) = low.find("cycling") {
        let quality = part[..cyc_idx].trim();
        let rest = &part[cyc_idx + "cycling".len()..];
        if !quality.is_empty()
            && quality
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-')
            && rest
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || is_dash_char(c))
        {
            let arg = trim_leading_dash_ws(rest).trim_end_matches('.').trim();
            if !arg.is_empty() {
                let reserved = matches!(
                    quality.to_ascii_lowercase().as_str(),
                    "type"
                        | "a"
                        | "an"
                        | "the"
                        | "any"
                        | "each"
                        | "another"
                        | "other"
                        | "all"
                        | "target"
                );
                let cyc_opts = FilterOpts {
                    default_zone: ZoneMask::empty(),
                    allow_plural_and_disjunction: false,
                    allow_spell_noun: false,
                    strip_trailing_card: true,
                    expand_outlaw_subtypes: false,
                };
                let filter = if reserved {
                    None
                } else {
                    parse_object_phrase(quality, ctx, cyc_opts)
                        .or_else(|| {
                            parse_object_phrase(&quality.to_ascii_lowercase(), ctx, cyc_opts)
                        })
                        .or_else(|| {
                            let tc = to_title_case(quality);
                            parse_object_phrase(&tc, ctx, cyc_opts)
                        })
                };
                let Some(f) = filter else {
                    return Some(ParsedKw {
                        name: "typecycling".to_string(),
                        cost: None,
                        param: Some(arg.to_string()),
                        n: None,
                        cycling_filter: None,
                        cycling_desc: None,
                        prototype_spec: None,
                    });
                };
                let desc = format_cycling_desc(quality, &f, ctx.interner);
                let is_mana = is_pure_mana_braces(arg);
                return Some(ParsedKw {
                    name: "typecycling".to_string(),
                    cost: if is_mana {
                        Some(ManaCost::parse(arg))
                    } else {
                        None
                    },
                    param: if is_mana { None } else { Some(arg.to_string()) },
                    n: None,
                    cycling_filter: Some(Box::new(f)),
                    cycling_desc: Some(desc),
                    prototype_spec: None,
                });
            }
        }
    }

    None
}

fn to_title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                None => String::new(),
                Some(c) => format!(
                    "{}{}",
                    c.to_ascii_uppercase(),
                    cs.as_str().to_ascii_lowercase()
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_cycling_desc(raw_quality: &str, filter: &ObjectFilter, interner: &Interner) -> String {
    let sub_names: Vec<String> = filter
        .subtypes_any
        .iter()
        .filter_map(|&id| interner.subtype_name(id).map(|s| s.to_ascii_lowercase()))
        .collect();
    raw_quality
        .split_whitespace()
        .map(|w| {
            let low = w.to_ascii_lowercase();
            if sub_names.iter().any(|s| s == &low) {
                let mut cs = low.chars();
                match cs.next() {
                    Some(c) => format!("{}{}", c.to_ascii_uppercase(), cs.as_str()),
                    None => low,
                }
            } else {
                low
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_keyword_cost_text(raw: &str) -> String {
    let trimmed = raw.trim();
    let toks = tokenize(trimmed, 0);
    // "pay N {C}"
    if toks.len() == 3
        && toks[0].eq_ci("pay")
        && let Some(n) = parse_small_number(toks[1].raw)
        && (1..=20).contains(&n)
        && matches!(toks[2].kind, TokenKind::ManaSymbol(_))
    {
        return toks[2].raw.to_ascii_uppercase().repeat(n as usize);
    }
    // "{A} or {B}" / "{A} or <other>"
    if let Some(or_pos) = toks.iter().position(|t| t.eq_ci("or"))
        && or_pos > 0
        && or_pos + 1 < toks.len()
        && toks[..or_pos]
            .iter()
            .all(|t| matches!(t.kind, TokenKind::ManaSymbol(_)))
    {
        let left: String = toks[..or_pos].iter().map(|t| t.raw).collect();
        if toks[or_pos + 1..]
            .iter()
            .all(|t| matches!(t.kind, TokenKind::ManaSymbol(_)))
        {
            let right: String = toks[or_pos + 1..].iter().map(|t| t.raw).collect();
            return format!("pay {left} or pay {right}");
        }
        let right_start = toks[or_pos + 1].span.start as usize;
        return format!("pay {left} or {}", trimmed[right_start..].trim());
    }
    trimmed.to_string()
}

fn parse_equip_quality_phrase(
    raw_phrase: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<(ObjectFilter, bool)> {
    let phrase = raw_phrase.trim();
    if phrase.is_empty() {
        return None;
    }
    let low = phrase.to_ascii_lowercase();
    if low == "worthy" || low == "worthy creature" || low == "worthy creatures" {
        let villain_id = ctx.interner.intern_subtype("Villain");
        let mut f = ObjectFilter {
            supertypes_any: SupertypeMask::LEGENDARY,
            colors_any: ColorMask::R | ColorMask::W,
            ..Default::default()
        };
        f.subtypes_none.push(villain_id);
        return Some((f, false));
    }
    if low == "planeswalker" || low == "planeswalkers" {
        return Some((
            ObjectFilter {
                types_any: CardTypeMask::PLANESWALKER,
                ..Default::default()
            },
            true,
        ));
    }
    if low == "creature or planeswalker"
        || low == "creatures or planeswalker"
        || low == "creature or planeswalkers"
        || low == "creatures or planeswalkers"
    {
        return Some((
            ObjectFilter {
                types_any: CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER,
                ..Default::default()
            },
            true,
        ));
    }
    if low == "creature token"
        || low == "creature tokens"
        || low == "token creature"
        || low == "token creatures"
    {
        return Some((
            ObjectFilter {
                flags: FilterFlags::TOKEN,
                ..Default::default()
            },
            false,
        ));
    }
    let mut f = parse_multi_or_object_phrase(
        phrase,
        ctx,
        FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: false,
            allow_spell_noun: false,
            strip_trailing_card: false,
            expand_outlaw_subtypes: false,
        },
    )?;
    let allows_pw = f.types_any.contains(CardTypeMask::PLANESWALKER);
    if f.types_any == CardTypeMask::CREATURE {
        f.types_any = CardTypeMask::empty();
    }
    Some((f, allows_pw))
}

fn parse_p21_dynamic_equip_cost(raw_cost: &str, ctx: &mut ParseCtx<'_>) -> Option<Vec<CostDef>> {
    let toks = tokenize(raw_cost, 0);
    if toks.len() < 7
        || !toks[0].eq_ci("pay")
        || !toks[2].eq_ci("life")
        || !toks[3].eq_ci("for")
        || !toks[4].eq_ci("each")
    {
        return None;
    }
    let n = parse_small_number(toks[1].raw)?;
    if n < 1 {
        return None;
    }
    let rest = &toks[5..];
    if rest.len() == 4
        && rest[0].eq_ci("card")
        && rest[1].eq_ci("in")
        && rest[2].eq_ci("your")
        && rest[3].eq_ci("hand")
    {
        let base = Amount::CardsIn {
            zone: Zone::Hand,
            of: PlayerRef::You,
        };
        let amount = if n == 1 {
            base
        } else {
            Amount::Mul {
                factor: n,
                of: Box::new(base),
            }
        };
        return Some(vec![CostDef::PayLife {
            amount: Box::new(amount),
            half_life_rounded_up: false,
        }]);
    }
    let cp = ctx.interner.checkpoint();
    let mut sub_ctx = ctx.with_normalized_text(raw_cost);
    let Some((filter, used)) = crate::parser::filter::parse_object_filter(
        rest,
        &mut sub_ctx,
        FilterOpts {
            default_zone: ZoneMask::BATTLEFIELD,
            allow_plural_and_disjunction: true,
            ..Default::default()
        },
        &crate::parser::filter::NullScope,
    ) else {
        ctx.interner.rollback(cp);
        return None;
    };
    if used != rest.len() {
        ctx.interner.rollback(cp);
        return None;
    }
    let base = Amount::Count {
        of: Box::new(filter),
    };
    let amount = if n == 1 {
        base
    } else {
        Amount::Mul {
            factor: n,
            of: Box::new(base),
        }
    };
    Some(vec![CostDef::PayLife {
        amount: Box::new(amount),
        half_life_rounded_up: false,
    }])
}

fn parse_gift_kind(raw: &str) -> Option<GiftKind> {
    match raw
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "a card" => Some(GiftKind::Card),
        "a food" => Some(GiftKind::Food),
        "a treasure" => Some(GiftKind::Treasure),
        "a tapped fish" => Some(GiftKind::TappedFish),
        "an octopus" => Some(GiftKind::Octopus),
        "an extra turn" => Some(GiftKind::ExtraTurn),
        _ => None,
    }
}

fn canonical_kicker_tag(raw: &str) -> String {
    let t = raw.trim();
    if is_pure_mana_braces(t) {
        t.to_ascii_uppercase()
    } else {
        t.to_ascii_lowercase()
    }
}

fn kicker_tags_for_entry(raw_entry: &str, defs: &[CostDef]) -> Vec<String> {
    let mut tags = vec![canonical_kicker_tag(raw_entry)];
    let mut colors = ColorMask::empty();
    for def in defs {
        if let CostDef::Mana { cost, .. } = def {
            for pip in &cost.pips {
                match *pip {
                    mtg_ir::ManaPip::Colored { color }
                    | mtg_ir::ManaPip::TwoBrid { color }
                    | mtg_ir::ManaPip::Phyrexian { color }
                    | mtg_ir::ManaPip::ColorlessHybrid { color } => {
                        colors |= ColorMask::from_mana_color(color);
                    }
                    mtg_ir::ManaPip::Hybrid { a, b }
                    | mtg_ir::ManaPip::HybridPhyrexian { a, b } => {
                        colors |= ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b);
                    }
                    mtg_ir::ManaPip::Snow => {}
                }
            }
        }
    }
    let wubrg = colors & ColorMask::ALL_COLORS;
    if wubrg.bits().count_ones() == 1 {
        let name = if wubrg.contains(ColorMask::W) {
            "white"
        } else if wubrg.contains(ColorMask::U) {
            "blue"
        } else if wubrg.contains(ColorMask::B) {
            "black"
        } else if wubrg.contains(ColorMask::R) {
            "red"
        } else {
            "green"
        };
        if !tags.iter().any(|t| t == name) {
            tags.push(name.to_string());
        }
    }
    tags
}

fn parse_affinity_quality(phrase: &str, ctx: &mut ParseCtx<'_>) -> Option<ObjectFilter> {
    let low = phrase.trim().to_ascii_lowercase();
    if low == "outlaw" || low == "outlaws" {
        let mut f = ObjectFilter::default();
        for sub in OUTLAW_SUBTYPES {
            f.subtypes_any.push(ctx.interner.intern_subtype(sub));
        }
        return Some(f);
    }
    if low == "historic" {
        let saga_id = ctx.interner.intern_subtype("Saga");
        return Some(ObjectFilter {
            extra: vec![ObjectPredicate::AnyOf {
                filters: vec![
                    ObjectFilter {
                        types_any: CardTypeMask::ARTIFACT,
                        ..Default::default()
                    },
                    ObjectFilter {
                        supertypes_any: SupertypeMask::LEGENDARY,
                        ..Default::default()
                    },
                    ObjectFilter {
                        subtypes_any: smallvec![saga_id],
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        });
    }
    parse_object_phrase(
        phrase,
        ctx,
        FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: false,
            allow_spell_noun: false,
            strip_trailing_card: true,
            expand_outlaw_subtypes: false,
        },
    )
}

fn parse_enchant_phrase(raw_param: &str, ctx: &mut ParseCtx<'_>) -> Option<ObjectFilter> {
    let mut phrase = raw_param.trim().to_string();
    if phrase.is_empty() {
        return None;
    }

    // Graveyard-card Auras
    let toks = tokenize(&phrase, 0);
    if let Some(in_pos) = toks.iter().position(|t| t.eq_ci("in"))
        && in_pos >= 2
        && (toks[in_pos - 1].eq_ci("card") || toks[in_pos - 1].eq_ci("cards"))
        && toks.last().is_some_and(|t| t.eq_ci("graveyard"))
    {
        let tail = &toks[in_pos + 1..toks.len() - 1];
        let owner = if tail.len() == 1 && tail[0].eq_ci("a") {
            Some(None)
        } else if tail.len() == 1 && tail[0].eq_ci("your") {
            Some(Some(RelativeControl::You))
        } else if tail.len() == 2 && tail[0].eq_ci("an") && tail[1].eq_ci("opponent's") {
            Some(Some(RelativeControl::Opponent))
        } else {
            None
        };
        if let Some(owner) = owner {
            let head_raw = phrase[..toks[in_pos - 1].span.start as usize].trim();
            let mut base = parse_multi_or_object_phrase(head_raw, ctx, FilterOpts::default())?;
            base.zones = ZoneMask::GRAVEYARD;
            base.owner = owner;
            return Some(base);
        }
        return None;
    }
    if toks.iter().any(|t| t.eq_ci("graveyard")) {
        return None;
    }

    let mut has_another_aura = false;
    if let Some(idx) = phrase
        .to_ascii_lowercase()
        .find(" with another aura attached to it")
        && idx + " with another aura attached to it".len() == phrase.len()
    {
        has_another_aura = true;
        phrase = phrase[..idx].trim().to_string();
    }

    let mut controller = None;
    let low = phrase.to_ascii_lowercase();
    if let Some(stem) = low.strip_suffix(" you control") {
        controller = Some(RelativeControl::You);
        phrase = phrase[..stem.len()].trim().to_string();
    } else if let Some(stem) = low
        .strip_suffix(" an opponent controls")
        .or_else(|| low.strip_suffix(" you don't control"))
    {
        controller = Some(RelativeControl::Opponent);
        phrase = phrase[..stem.len()].trim().to_string();
    }

    let mut flags = FilterFlags::empty();
    loop {
        let low = phrase.to_ascii_lowercase();
        if low.starts_with("another ") {
            flags |= FilterFlags::OTHER;
            phrase = phrase["another ".len()..].trim().to_string();
            continue;
        }
        if low.starts_with("tapped ") {
            flags |= FilterFlags::TAPPED;
            phrase = phrase["tapped ".len()..].trim().to_string();
            continue;
        }
        if low.starts_with("untapped ") {
            flags |= FilterFlags::UNTAPPED;
            phrase = phrase["untapped ".len()..].trim().to_string();
            continue;
        }
        if low.starts_with("noncommander ") {
            flags |= FilterFlags::NOT_COMMANDER;
            phrase = phrase["noncommander ".len()..].trim().to_string();
            continue;
        }
        break;
    }

    let mut base = parse_multi_or_object_phrase(&phrase, ctx, FilterOpts::default())?;
    base.flags |= flags;
    if let Some(ctrl) = controller {
        base.controller = Some(ctrl);
    }
    if has_another_aura {
        base.extra.push(ObjectPredicate::HasAnotherAuraAttached);
    }
    Some(base)
}

fn parse_craft_materials_spec(
    raw_materials: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<(Vec<ObjectFilter>, bool, Option<i32>, bool)> {
    let s = raw_materials.trim();
    if s.is_empty() {
        return None;
    }
    let toks = tokenize(s, 0);
    // "N [permanents|cards] that share a card type"
    if toks.len() >= 6
        && toks[toks.len() - 5].eq_ci("that")
        && toks[toks.len() - 4].eq_ci("share")
        && toks[toks.len() - 3].eq_ci("a")
        && toks[toks.len() - 2].eq_ci("card")
        && toks[toks.len() - 1].eq_ci("type")
        && let Some(cnt) = parse_small_number(toks[0].raw)
        && (2..=10).contains(&cnt)
    {
        return Some((
            vec![ObjectFilter::default(); cnt as usize],
            false,
            None,
            true,
        ));
    }

    // "any number of <mat>" / "one or more <mat>"
    if starts_with_ci(s, "any number of ") {
        let rest = s["any number of ".len()..].trim();
        let f = parse_single_craft_material(rest, ctx)?;
        return Some((vec![f], true, Some(0), false));
    }
    if starts_with_ci(s, "one or more ") {
        let rest = s["one or more ".len()..].trim();
        let f = parse_single_craft_material(rest, ctx)?;
        return Some((vec![f], true, Some(1), false));
    }

    // "<N> <mat>" without "and"
    if toks.len() >= 2
        && !toks.iter().any(|t| t.eq_ci("and"))
        && let Some(cnt) = parse_small_number(toks[0].raw)
        && !toks[0].eq_ci("a")
        && !toks[0].eq_ci("an")
        && !toks[0].eq_ci("one")
        && (1..=10).contains(&cnt)
    {
        let rest = s[toks[1].span.start as usize..].trim();
        let f = parse_single_craft_material(rest, ctx)?;
        return Some((vec![f; cnt as usize], false, None, false));
    }

    // "A and B" / "A, B, and C"
    if toks.iter().any(|t| t.eq_ci("and")) {
        let mut materials = Vec::new();
        for seg in s.split(',') {
            for part in seg.split(" and ") {
                let pt = part.trim();
                if pt.is_empty() {
                    continue;
                }
                let ptoks = tokenize(pt, 0);
                if ptoks.len() >= 2
                    && !ptoks[0].eq_ci("a")
                    && !ptoks[0].eq_ci("an")
                    && !ptoks[0].eq_ci("one")
                    && let Some(cnt) = parse_small_number(ptoks[0].raw)
                    && (1..=10).contains(&cnt)
                {
                    let rest = pt[ptoks[1].span.start as usize..].trim();
                    let f = parse_single_craft_material(rest, ctx)?;
                    for _ in 0..cnt {
                        materials.push(f.clone());
                    }
                } else {
                    materials.push(parse_single_craft_material(pt, ctx)?);
                }
            }
        }
        if materials.len() >= 2 {
            return Some((materials, false, None, false));
        }
    }

    let single = parse_single_craft_material(s, ctx)?;
    Some((vec![single], false, None, false))
}

fn parse_single_craft_material(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<ObjectFilter> {
    let mut s = raw.trim();
    for p in ["one ", "an ", "a "] {
        if starts_with_ci(s, p) {
            s = s[p.len()..].trim();
            break;
        }
    }
    if s.eq_ignore_ascii_case("permanent") || s.eq_ignore_ascii_case("permanents") {
        return Some(ObjectFilter {
            types_any: CardTypeMask::PERMANENT,
            ..Default::default()
        });
    }
    if s.eq_ignore_ascii_case("nonland permanent") || s.eq_ignore_ascii_case("nonland permanents") {
        return Some(ObjectFilter {
            types_none: CardTypeMask::LAND,
            ..Default::default()
        });
    }
    parse_multi_or_object_phrase(
        s,
        ctx,
        FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: false,
            allow_spell_noun: false,
            strip_trailing_card: true,
            expand_outlaw_subtypes: false,
        },
    )
}

pub fn parse_keyword_line(
    ctx: &mut ParseCtx<'_>,
    line: &NormalizedLine,
) -> Option<Vec<AbilityDef>> {
    if let Some(frame_kw) = parse_frame_keyword_line(ctx, line) {
        return Some(frame_kw);
    }

    let body_trimmed = line.body.trim();
    if (starts_with_ci(body_trimmed, "boast")
        || starts_with_ci(body_trimmed, "forecast")
        || starts_with_ci(body_trimmed, "power-up")
        || starts_with_ci(body_trimmed, "exhaust")
        || starts_with_ci(body_trimmed, "waterbend"))
        && crate::parser::card::cost_colon(body_trimmed).is_some()
    {
        return None;
    }
    if (line.ability_word.is_some() && !starts_with_ci(body_trimmed, "equip"))
        || (body_trimmed.ends_with('.') && !is_single_keyword_part(body_trimmed))
    {
        return None;
    }

    let parts = split_keyword_line_parts(&line.body);
    if parts.is_empty() {
        return None;
    }
    let line_cp = ctx.interner.checkpoint();
    let mut parsed_parts = Vec::with_capacity(parts.len());
    for part in &parts {
        let Some(p) = parse_keyword_part(part, ctx) else {
            ctx.interner.rollback(line_cp);
            return None;
        };
        parsed_parts.push(p);
    }

    let mut out = Vec::new();
    let mut cursor = (line.body_span.start - line.span.start) as usize;

    for (i, (part, parsed)) in parts.into_iter().zip(parsed_parts).enumerate() {
        let rel = line.text[cursor..].find(part).unwrap_or(0);
        let abs_start = line.span.start + (cursor + rel) as u32;
        let span = Span {
            start: abs_start,
            end: abs_start + part.len() as u32,
        };
        cursor = cursor + rel + part.len();

        let part_cp = ctx.interner.checkpoint();
        let kw_name = parsed.name.clone();
        let spec = get_keyword_spec(&kw_name);
        let mut implemented = spec.is_some_and(|s| s.implemented);
        let kw_id = ctx.interner.intern_keyword(&kw_name);
        let mut kd = make_empty_kw_def(kw_id, parsed.param.clone());
        kd.cost = parsed.cost.clone();
        kd.n = parsed.n;
        kd.cycling_filter = parsed.cycling_filter;
        kd.cycling_desc = parsed.cycling_desc;
        kd.prototype_spec = parsed.prototype_spec;

        // 1. Equip
        if kw_name == "equip" && kd.cost.is_none() {
            if let Some(dash_pos) = part.find(is_dash_char) {
                let left = part[..dash_pos].trim();
                let raw_qual = if left.len() > "equip".len() {
                    Some(left["equip".len()..].trim())
                } else {
                    None
                };
                let raw_cost = normalize_keyword_cost_text(
                    part[dash_pos + part[dash_pos..].chars().next()?.len_utf8()..]
                        .trim_end_matches('.')
                        .trim(),
                );
                let cp = {
                    let mut sub_ctx = ctx.with_normalized_text(&raw_cost);
                    parse_cost_list(&raw_cost, 0, &mut sub_ctx)
                };
                let dyn_defs = if !cp.unparsed.is_empty() {
                    parse_p21_dynamic_equip_cost(&raw_cost, ctx)
                } else {
                    None
                };
                let eff_defs = dyn_defs.as_ref().unwrap_or(&cp.defs);
                let qual_parsed = raw_qual.and_then(|q| parse_equip_quality_phrase(q, ctx));
                if (raw_qual.is_some() && qual_parsed.is_none())
                    || (dyn_defs.is_none() && !cp.unparsed.is_empty())
                    || eff_defs.is_empty()
                    || !eff_defs.iter().all(is_equip_payable_cost)
                {
                    ctx.interner.rollback(part_cp);
                    out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                    continue;
                }
                if let Some((ef, allow_pw)) = qual_parsed {
                    kd.equip_filter = Some(Box::new(ef));
                    kd.equip_allow_planeswalker = allow_pw;
                    kd.param = raw_qual.map(str::to_string);
                } else {
                    kd.param = None;
                }
                kd.cost_defs = eff_defs.clone();
                if eff_defs.len() == 1
                    && let CostDef::Mana { cost, .. } = &eff_defs[0]
                {
                    kd.cost = Some(cost.clone());
                }
            } else {
                let direct = kd.param.as_deref().map(|p| {
                    let norm = normalize_keyword_cost_text(p);
                    let mut sub_ctx = ctx.with_normalized_text(&norm);
                    parse_cost_list(&norm, 0, &mut sub_ctx)
                });
                if let Some(cp) = direct
                    && cp.unparsed.is_empty()
                    && !cp.defs.is_empty()
                    && cp.defs.iter().all(is_equip_payable_cost)
                {
                    kd.cost_defs = cp.defs.clone();
                    if cp.defs.len() == 1
                        && let CostDef::Mana { cost, .. } = &cp.defs[0]
                    {
                        kd.cost = Some(cost.clone());
                    }
                    kd.param = None;
                } else if let Some(p) = kd.param.clone()
                    && let Some(brace_pos) = p.find('{')
                {
                    let qual = p[..brace_pos].trim();
                    let mc_str = p[brace_pos..].trim();
                    if !qual.is_empty()
                        && is_pure_mana_braces(mc_str)
                        && !mc_str.to_ascii_uppercase().contains("{X}")
                        && let Some((ef, allow_pw)) = parse_equip_quality_phrase(qual, ctx)
                    {
                        kd.cost = Some(ManaCost::parse(mc_str));
                        kd.equip_filter = Some(Box::new(ef));
                        kd.equip_allow_planeswalker = allow_pw;
                        kd.param = None;
                    } else {
                        implemented = false;
                    }
                } else {
                    implemented = false;
                }
            }
        }

        // 2. Ward
        if kw_name == "ward" {
            let raw_w = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_w);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty()
                || cp.defs.is_empty()
                || !cp.defs.iter().all(is_ward_payable_cost)
            {
                ctx.interner.rollback(part_cp);
                let mut ua = UnsupportedAbility {
                    reason: UnsupportedReason::Unparsed,
                    text: part.to_string(),
                    span,
                    unparsed: vec![span],
                    id: None,
                    cost: Vec::new(),
                    cost_complete: None,
                    cost_keyword: None,
                    timing: None,
                    is_mana: false,
                    effect: Vec::new(),
                    targets: Vec::new(),
                    activation_restrictions: Vec::new(),
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    cast_restrictions: Vec::new(),
                    cast_condition: None,
                    trigger: Some(TriggerDef {
                        event: EventKind::BecomesTarget,
                        filter: EventFilter {
                            object: Some(TriggerSubject::SelfObj),
                            player: Some(PlayerFilter {
                                relation: PlayerRelation::Opponent,
                            }),
                            detail: Some(EventDetail::BecomesTarget {
                                targeted_by: Some(mtg_ir::TargetedByKind::SpellOrAbility),
                                targeted_by_activated_ability: false,
                                first_time_targeted_by_you_this_turn: false,
                                targeting_source_filter: None,
                                target_player: None,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: Vec::new(),
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    }),
                    condition: None,
                    grant_condition: None,
                };
                let _ = &mut ua;
                out.push(AbilityDef::Unsupported(ua));
                continue;
            }
            kd.cost_defs = cp.defs;
        }

        // 3. Group A
        if matches!(
            kw_name.as_str(),
            "flashback"
                | "morph"
                | "megamorph"
                | "disguise"
                | "unearth"
                | "buyback"
                | "evoke"
                | "escape"
                | "foretell"
                | "plot"
                | "entwine"
                | "disturb"
                | "blitz"
                | "warp"
                | "madness"
                | "dash"
                | "bestow"
        ) {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty()
                || cp.defs.is_empty()
                || (kw_name == "bestow" && !cp.defs.iter().all(is_cast_payable_cost))
            {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs.clone();
            if kw_name == "bestow"
                && cp.defs.len() == 1
                && let CostDef::Mana { cost, .. } = &cp.defs[0]
            {
                kd.cost = Some(cost.clone());
            }
        }

        // Group B
        if matches!(
            kw_name.as_str(),
            "offspring" | "squad" | "replicate" | "echo" | "escalate"
        ) {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            let ok = cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().all(|d| {
                    is_supported_optional_cost(d)
                        && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                });
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs.clone();
            if kw_name == "escalate"
                && cp.defs.len() == 1
                && let CostDef::Mana { cost, .. } = &cp.defs[0]
            {
                kd.cost = Some(cost.clone());
            }
        }

        // Group C
        if matches!(
            kw_name.as_str(),
            "level up"
                | "transmute"
                | "reinforce"
                | "ninjutsu"
                | "commander ninjutsu"
                | "scavenge"
                | "outlast"
                | "embalm"
                | "eternalize"
                | "encore"
        ) {
            if kw_name == "reinforce" && kd.n.is_none() {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            let ok = cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().all(|d| {
                    !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                });
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs;
            if kw_name == "transmute" {
                kd.n = Some(ctx.card_mana_value as i32);
            }
        }

        // Group D: recover
        if kw_name == "recover" {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty() || cp.defs.len() != 1 {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            if let CostDef::Mana { cost, .. } = &cp.defs[0]
                && cost.x_count == 0
                && !cost.raw.to_ascii_uppercase().contains("{X}")
            {
                kd.cost = Some(cost.clone());
                kd.cost_defs = cp.defs;
                kd.param = None;
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        // Group E: harmonize, miracle, spectacle, surge
        if matches!(
            kw_name.as_str(),
            "harmonize" | "miracle" | "spectacle" | "surge"
        ) {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if cp.unparsed.is_empty()
                && cp.defs.len() == 1
                && let CostDef::Mana { cost, .. } = &cp.defs[0]
            {
                kd.cost = Some(cost.clone());
                kd.cost_defs = cp.defs;
                kd.param = None;
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        // Group F: sneak, beam me up, web-slinging, mayhem
        if matches!(
            kw_name.as_str(),
            "sneak" | "beam me up" | "web-slinging" | "mayhem"
        ) {
            if kw_name == "mayhem" && kd.cost.is_none() && kd.param.is_none() && kd.n.is_none() {
                // Bare mayhem valid
            } else {
                let raw_c = kd
                    .cost
                    .as_ref()
                    .map(|c| c.raw.as_str())
                    .or(kd.param.as_deref())
                    .unwrap_or("");
                let norm = normalize_keyword_cost_text(raw_c);
                let cp = {
                    let mut sub_ctx = ctx.with_normalized_text(&norm);
                    parse_cost_list(&norm, 0, &mut sub_ctx)
                };
                let ok = cp.unparsed.is_empty()
                    && !cp.defs.is_empty()
                    && cp.defs.iter().all(|d| {
                        is_cast_payable_cost(d)
                            && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                    });
                if !ok {
                    ctx.interner.rollback(part_cp);
                    out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                    continue;
                }
                let mana_raw: String = cp
                    .defs
                    .iter()
                    .filter_map(|d| match d {
                        CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                        _ => None,
                    })
                    .collect();
                if !mana_raw.is_empty() {
                    kd.cost = Some(ManaCost::parse(&mana_raw));
                }
                kd.cost_defs = cp.defs;
                kd.param = None;
            }
        }

        // Group G: cleave, freerunning, prowl, more than meets the eye
        if matches!(
            kw_name.as_str(),
            "cleave" | "freerunning" | "prowl" | "more than meets the eye"
        ) {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty()
                || cp.defs.is_empty()
                || !cp.defs.iter().all(is_cast_payable_cost)
            {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            let mana_raw: String = cp
                .defs
                .iter()
                .filter_map(|d| match d {
                    CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                    _ => None,
                })
                .collect();
            if !mana_raw.is_empty() {
                kd.cost = Some(ManaCost::parse(&mana_raw));
            }
            kd.cost_defs = cp.defs;
            kd.param = None;
            if kw_name == "cleave" {
                ctx.has_cleave_keyword = true;
            }
        }

        // Group H: reconfigure, transfigure, aura swap
        if matches!(
            kw_name.as_str(),
            "reconfigure" | "transfigure" | "aura swap"
        ) {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty()
                || cp.defs.is_empty()
                || !cp.defs.iter().all(is_equip_payable_cost)
            {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs.clone();
            if cp.defs.len() == 1
                && let CostDef::Mana { cost, .. } = &cp.defs[0]
            {
                kd.cost = Some(cost.clone());
                kd.param = None;
            }
        }

        // Awaken
        if kw_name == "awaken" {
            let ok = kd.n.is_some_and(|n| n > 0)
                && kd
                    .cost
                    .as_ref()
                    .is_some_and(|c| !c.raw.is_empty() && is_pure_mana_braces(&c.raw));
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.awaken_n = kd.n.map(|n| n as u16);
            kd.cost_defs = vec![CostDef::Mana {
                cost: kd.cost.clone().unwrap(),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }];
        }

        // Emerge
        if kw_name == "emerge" {
            if kd.cost.is_none()
                && let Some(p) = kd.param.clone()
                && let Some(brace) = p.find('{')
            {
                let filt_str = p[..brace].trim();
                let mc_str = p[brace..].trim();
                if is_pure_mana_braces(mc_str)
                    && let Some(ef) =
                        parse_multi_or_object_phrase(filt_str, ctx, FilterOpts::default())
                {
                    kd.emerge_filter = Some(Box::new(ef));
                    kd.cost = Some(ManaCost::parse(mc_str));
                    kd.param = None;
                } else {
                    ctx.interner.rollback(part_cp);
                    out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                    continue;
                }
            }
            let ok = kd.cost.as_ref().is_some_and(|c| {
                is_pure_mana_braces(&c.raw) && !c.raw.to_ascii_uppercase().contains("{X}")
            });
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = vec![CostDef::Mana {
                cost: kd.cost.clone().unwrap(),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }];
        }

        // Champion
        if kw_name == "champion" {
            let raw_c = kd.param.as_deref().unwrap_or("").trim();
            let stripped = if starts_with_ci(raw_c, "an ") {
                raw_c[3..].trim()
            } else if starts_with_ci(raw_c, "a ") {
                raw_c[2..].trim()
            } else {
                raw_c
            };
            let cf = if stripped.is_empty() {
                None
            } else {
                parse_multi_or_object_phrase(stripped, ctx, FilterOpts::default())
            };
            if let Some(f) = cf
                && kd.cost.is_none()
                && kd.n.is_none()
            {
                kd.champion_filter = Some(Box::new(f));
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        // Prototype
        if kw_name == "prototype" {
            if let Some(ps) = &kd.prototype_spec {
                ctx.prototype_spec = Some(ps.clone());
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        // Casualty / Fading / Vanishing
        if (kw_name == "casualty" || kw_name == "fading")
            && (kd.n.is_none() || kd.cost.is_some() || kd.param.is_some())
        {
            ctx.interner.rollback(part_cp);
            out.push(unsupported(UnsupportedReason::Unparsed, part, span));
            continue;
        }
        if kw_name == "vanishing" && (kd.cost.is_some() || kd.param.is_some()) {
            ctx.interner.rollback(part_cp);
            out.push(unsupported(UnsupportedReason::Unparsed, part, span));
            continue;
        }

        // Cumulative upkeep
        if kw_name == "cumulative upkeep" {
            let mut kw_cost = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("")
                .trim()
                .to_string();
            let toks = tokenize(&kw_cost, 0);
            if toks.len() == 3
                && toks[1].eq_ci("or")
                && let (
                    TokenKind::ManaSymbol(crate::lexer::ManaSymbolKind::Colored(a)),
                    TokenKind::ManaSymbol(crate::lexer::ManaSymbolKind::Colored(b)),
                ) = (&toks[0].kind, &toks[2].kind)
                && *a != ManaColor::C
                && *b != ManaColor::C
            {
                let sa = &toks[0].raw[1..toks[0].raw.len() - 1];
                let sb = &toks[2].raw[1..toks[2].raw.len() - 1];
                kw_cost = format!(
                    "{{{}/{}}}",
                    sa.to_ascii_uppercase(),
                    sb.to_ascii_uppercase()
                );
            } else if toks.len() >= 5
                && toks[0].eq_ci("pay")
                && let Some(and_pos) = toks.iter().position(|t| t.eq_ci("and"))
                && and_pos >= 2
                && and_pos + 3 == toks.len()
                && toks[1..and_pos]
                    .iter()
                    .all(|t| matches!(t.kind, TokenKind::ManaSymbol(_)))
                && matches!(toks[and_pos + 1].kind, TokenKind::Number(_))
                && toks[and_pos + 2].eq_ci("life")
            {
                let mana_str: String = toks[1..and_pos].iter().map(|t| t.raw).collect();
                kw_cost = format!("{mana_str}, Pay {} life", toks[and_pos + 1].raw);
            }
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&kw_cost);
                parse_cost_list(&kw_cost, 0, &mut sub_ctx)
            };
            let scalable = cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().all(|d| {
                    if !is_supported_optional_cost(d) {
                        return false;
                    }
                    match d {
                        CostDef::Mana { cost, .. } => {
                            cost.x_count == 0 && !cost.raw.to_ascii_uppercase().contains("{X}")
                        }
                        CostDef::PayLife { amount, .. } => matches!(**amount, Amount::Const { .. }),
                        CostDef::RemoveCounters { count, .. } => {
                            matches!(**count, Amount::Const { .. })
                        }
                        CostDef::TapSubsetSum { min_total, .. } => {
                            matches!(**min_total, Amount::Const { .. })
                        }
                        CostDef::Sacrifice { count, target, .. } => {
                            matches!(**count, Amount::Const { .. })
                                && !matches!(target, mtg_ir::CostObjectTarget::SelfObj)
                        }
                        CostDef::Discard { count, target, .. }
                        | CostDef::ExileFrom { count, target, .. } => {
                            matches!(**count, Amount::Const { .. })
                                && !matches!(target, Some(mtg_ir::CostObjectTarget::SelfObj))
                        }
                        CostDef::TapOthers { count, .. } => matches!(**count, Amount::Const { .. }),
                        _ => false,
                    }
                });
            if !scalable {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs;
        }

        // Kicker / Multikicker
        if kw_name == "kicker" || kw_name == "multikicker" {
            let raw_k = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("")
                .trim()
                .to_string();
            let raw_entries: Vec<&str> = if kw_name == "kicker" {
                raw_k.split(" and/or ").map(str::trim).collect()
            } else {
                vec![raw_k.as_str()]
            };
            if raw_k.is_empty() || raw_entries.iter().any(|e| e.is_empty()) {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            let mut parsed_entries = Vec::new();
            let mut failed = false;
            for entry in raw_entries {
                let cp = {
                    let mut sub_ctx = ctx.with_normalized_text(entry);
                    parse_cost_list(entry, 0, &mut sub_ctx)
                };
                if !cp.unparsed.is_empty() || cp.defs.is_empty() {
                    failed = true;
                    break;
                }
                let tags = kicker_tags_for_entry(entry, &cp.defs);
                parsed_entries.push(mtg_ir::KickerCostOption {
                    cost: ManaCost::parse(entry),
                    cost_defs: cp.defs,
                    tags,
                });
            }
            if failed {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            if parsed_entries.len() == 1 {
                kd.cost_defs = parsed_entries[0].cost_defs.clone();
            }
            kd.kicker_costs = parsed_entries;
        }

        if spec.is_some_and(|s| s.param_kind == KeywordParamKind::None)
            && (kd.cost.is_some() || kd.n.is_some() || kd.param.is_some())
        {
            implemented = false;
        }

        if kw_name == "bloodthirst" {
            if kd.n.is_none()
                && kd.cost.is_none()
                && kd
                    .param
                    .as_deref()
                    .is_some_and(|p| p.trim().eq_ignore_ascii_case("x"))
            {
                kd.param = Some("X".to_string());
            } else if !(kd.n.is_some() && kd.cost.is_none() && kd.param.is_none()) {
                implemented = false;
            }
        } else if kw_name == "devour" {
            if kd.n.is_some() && kd.cost.is_none() && kd.param.is_none() {
                // Devour N
            } else if kd.n.is_none()
                && kd.cost.is_none()
                && let Some(p) = kd.param.clone()
            {
                let pt = p.trim();
                if pt.eq_ignore_ascii_case("x")
                    || pt.eq_ignore_ascii_case(
                        "x, where x is the number of creatures devoured this way",
                    )
                {
                    kd.devour_x = true;
                    kd.param = None;
                } else if let Some(last_sp) = pt.rfind(' ')
                    && let Ok(dn) = pt[last_sp + 1..].trim().parse::<i32>()
                    && let Some(df) = parse_multi_or_object_phrase(
                        pt[..last_sp].trim(),
                        ctx,
                        FilterOpts::default(),
                    )
                {
                    kd.devour_filter = Some(Box::new(df));
                    kd.n = Some(dn);
                    kd.param = None;
                } else {
                    implemented = false;
                }
            } else {
                implemented = false;
            }
        } else if spec.is_some_and(|s| s.param_kind == KeywordParamKind::Number)
            && !(spec.is_some_and(|s| s.optional_parameter || s.name == "vanishing")
                && kd.n.is_none()
                && kd.cost.is_none()
                && kd.param.is_none())
            && (kd.n.is_none() || kd.cost.is_some() || kd.param.is_some())
        {
            implemented = false;
        }

        if kw_name == "affinity" {
            if let Some(p) = kd.param.as_deref()
                && starts_with_ci(p.trim(), "for ")
                && let Some(af) = parse_affinity_quality(p.trim()["for ".len()..].trim(), ctx)
            {
                kd.affinity_filter = Some(Box::new(af));
            } else {
                implemented = false;
            }
        }

        if kw_name == "read ahead" {
            ctx.has_read_ahead = true;
        }

        if kw_name == "gift" {
            if let Some(gk) = kd.param.as_deref().and_then(parse_gift_kind)
                && kd.cost.is_none()
                && kd.n.is_none()
            {
                kd.gift_kind = Some(gk);
                ctx.has_gift_keyword = true;
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        if kw_name == "splice" {
            let trimmed_p = part.trim_end_matches('.').trim();
            if starts_with_ci(trimmed_p, "splice onto ") {
                let after = trimmed_p["splice onto ".len()..].trim();
                let (qual_raw, cost_raw) = if let Some(d) = after.find(is_dash_char) {
                    (
                        after[..d].trim(),
                        after[d + after[d..].chars().next()?.len_utf8()..].trim(),
                    )
                } else if let Some(b) = after.find('{') {
                    (after[..b].trim(), after[b..].trim())
                } else {
                    ("", "")
                };
                let sf = if qual_raw.eq_ignore_ascii_case("arcane") {
                    let arc = ctx.interner.intern_subtype("Arcane");
                    Some(ObjectFilter {
                        subtypes_any: smallvec![arc],
                        ..Default::default()
                    })
                } else if !qual_raw.is_empty() {
                    parse_multi_or_object_phrase(qual_raw, ctx, FilterOpts::default())
                } else {
                    None
                };
                let norm = normalize_keyword_cost_text(cost_raw);
                let cp = {
                    let mut sub_ctx = ctx.with_normalized_text(&norm);
                    parse_cost_list(&norm, 0, &mut sub_ctx)
                };
                if let Some(splice_f) = sf
                    && cp.unparsed.is_empty()
                    && !cp.defs.is_empty()
                    && cp.defs.iter().all(|d| {
                        is_cast_payable_cost(d)
                            && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                    })
                {
                    kd.splice_filter = Some(Box::new(splice_f));
                    kd.cost_defs = cp.defs.clone();
                    kd.param = Some(qual_raw.to_string());
                    if cp.defs.len() == 1
                        && let CostDef::Mana { cost, .. } = &cp.defs[0]
                    {
                        kd.cost = Some(cost.clone());
                    }
                } else {
                    ctx.interner.rollback(part_cp);
                    out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                    continue;
                }
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        if kw_name == "impending" {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if kd.n.is_some_and(|n| n >= 1)
                && cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().all(|d| {
                    is_cast_payable_cost(d)
                        && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                })
            {
                kd.impending_n = kd.n.map(|n| n as u16);
                kd.cost_defs = cp.defs.clone();
                let mana_raw: String = cp
                    .defs
                    .iter()
                    .filter_map(|d| match d {
                        CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                        _ => None,
                    })
                    .collect();
                if !mana_raw.is_empty() {
                    kd.cost = Some(ManaCost::parse(&mana_raw));
                }
                kd.param = None;
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        if kw_name == "craft" {
            let trimmed_p = part.trim_end_matches('.').trim();
            if starts_with_ci(trimmed_p, "craft with ") {
                let after = trimmed_p["craft with ".len()..].trim();
                let (mat_raw, cost_raw) = if let Some(d) = after.find(is_dash_char) {
                    (
                        after[..d].trim(),
                        after[d + after[d..].chars().next()?.len_utf8()..].trim(),
                    )
                } else if let Some(b) = after.find('{') {
                    (after[..b].trim(), after[b..].trim())
                } else {
                    ("", "")
                };
                let mat_spec = parse_craft_materials_spec(mat_raw, ctx);
                let norm = normalize_keyword_cost_text(cost_raw);
                let cp = if norm.is_empty() {
                    None
                } else {
                    let mut sub_ctx = ctx.with_normalized_text(&norm);
                    Some(parse_cost_list(&norm, 0, &mut sub_ctx))
                };
                if let (Some((materials, any_number, min_count, share_type)), Some(cp)) =
                    (mat_spec, cp)
                    && cp.unparsed.is_empty()
                    && !cp.defs.is_empty()
                    && cp.defs.iter().all(is_equip_payable_cost)
                {
                    kd.param = Some(mat_raw.to_string());
                    kd.craft_materials = materials;
                    kd.craft_share_card_type = share_type;
                    if any_number {
                        kd.craft_any_number = true;
                        kd.n = min_count.or(Some(1));
                    }
                    kd.cost_defs = cp.defs.clone();
                    let mana_raw: String = cp
                        .defs
                        .iter()
                        .filter_map(|d| match d {
                            CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !mana_raw.is_empty() {
                        kd.cost = Some(ManaCost::parse(&mana_raw));
                    }
                } else {
                    ctx.interner.rollback(part_cp);
                    out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                    continue;
                }
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        }

        if kw_name == "partner" {
            if kd.cost.is_some() || kd.n.is_some() {
                implemented = false;
            } else if kd.param.is_some() {
                let is_with = starts_with_ci(part, "partner with ")
                    && !part["partner with ".len()..].trim().is_empty();
                let is_dash = starts_with_ci(part, "partner")
                    && part["partner".len()..]
                        .trim_start()
                        .chars()
                        .next()
                        .is_some_and(is_dash_char)
                    && !trim_leading_dash_ws(&part["partner".len()..]).is_empty();
                if !is_with && !is_dash {
                    implemented = false;
                }
            }
        }

        // Generic cost_parse_mode hook (fortify, mutate, etc.)
        if implemented
            && let Some(mode) = spec.and_then(|s| s.cost_parse_mode)
            && kd.cost_defs.is_empty()
        {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            let valid = match mode {
                CostParseMode::Activated => cp.defs.iter().all(is_equip_payable_cost),
                CostParseMode::CastOption | CostParseMode::OptionalCost => {
                    cp.defs.iter().all(is_cast_payable_cost)
                }
            };
            if !cp.unparsed.is_empty() || cp.defs.is_empty() || !valid {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs.clone();
            let mana_raw: String = cp
                .defs
                .iter()
                .filter_map(|d| match d {
                    CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                    _ => None,
                })
                .collect();
            if !mana_raw.is_empty() && (mode != CostParseMode::Activated || cp.defs.len() == 1) {
                kd.cost = Some(ManaCost::parse(&mana_raw));
            }
            if mode != CostParseMode::Activated || cp.defs.len() == 1 {
                kd.param = None;
            }
            if mode == CostParseMode::OptionalCost {
                kd.kicker_costs = vec![mtg_ir::KickerCostOption {
                    cost: ManaCost::parse(&norm),
                    cost_defs: cp.defs,
                    tags: kicker_tags_for_entry(&norm, &kd.cost_defs),
                }];
            }
        }

        // Validate remaining 11 parameterized keywords in KEYWORD_SPECS
        if kw_name == "suspend" {
            let ok = kd.n.is_some_and(|n| n >= 0)
                && kd
                    .cost
                    .as_ref()
                    .is_some_and(|c| is_pure_mana_braces(&c.raw))
                && kd.param.is_none();
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        } else if kw_name == "overload" {
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let norm = normalize_keyword_cost_text(raw_c);
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(&norm);
                parse_cost_list(&norm, 0, &mut sub_ctx)
            };
            if kd.n.is_some()
                || !cp.unparsed.is_empty()
                || cp.defs.is_empty()
                || !cp.defs.iter().all(is_cast_payable_cost)
            {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            if kd.cost.is_none() {
                let mana_raw: String = cp
                    .defs
                    .iter()
                    .filter_map(|d| match d {
                        CostDef::Mana { cost, .. } => Some(cost.raw.as_str()),
                        _ => None,
                    })
                    .collect();
                if !mana_raw.is_empty() {
                    kd.cost = Some(ManaCost::parse(&mana_raw));
                }
            }
            kd.param = None;
        } else if kw_name == "waterbend" {
            let ok = kd
                .cost
                .as_ref()
                .is_some_and(|c| is_pure_mana_braces(&c.raw))
                && kd.n.is_none()
                && kd.param.is_none();
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        } else if kw_name == "hexproof" {
            let has_from = starts_with_ci(part.trim(), "hexproof from ");
            let ok = kd.cost.is_none()
                && kd.n.is_none()
                && (kd.param.is_none()
                    || (has_from && kd.param.as_deref().is_some_and(|p| !p.trim().is_empty())));
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        } else if kw_name == "protection" {
            let has_from = starts_with_ci(part.trim(), "protection from ");
            let ok = kd.cost.is_none()
                && kd.n.is_none()
                && has_from
                && kd.param.as_deref().is_some_and(|p| !p.trim().is_empty());
            if !ok {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
        } else if matches!(
            kw_name.as_str(),
            "offering" | "boast" | "forecast" | "power-up" | "exhaust"
        ) {
            ctx.interner.rollback(part_cp);
            out.push(unsupported(UnsupportedReason::Unparsed, part, span));
            continue;
        } else if kw_name == "companion" {
            ctx.interner.rollback(part_cp);
            out.push(unsupported(
                UnsupportedReason::KeywordNotImplemented,
                part,
                span,
            ));
            continue;
        }

        if !implemented {
            ctx.interner.rollback(part_cp);
            out.push(unsupported(
                UnsupportedReason::KeywordNotImplemented,
                part,
                span,
            ));
            continue;
        }

        if kw_name == "enchant" {
            let raw_param = kd.param.as_deref().unwrap_or("").trim();
            if raw_param.eq_ignore_ascii_case("player") {
                kd.enchant_player = Some(PlayerFilter {
                    relation: PlayerRelation::Any,
                });
            } else if raw_param.eq_ignore_ascii_case("opponent")
                || raw_param.eq_ignore_ascii_case("an opponent")
            {
                kd.enchant_player = Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                });
            } else if raw_param.eq_ignore_ascii_case("you") {
                kd.enchant_player = Some(PlayerFilter {
                    relation: PlayerRelation::You,
                });
            } else if let Some(ef) = parse_enchant_phrase(raw_param, ctx) {
                kd.enchant_filter = Some(Box::new(ef));
            } else {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(
                    UnsupportedReason::KeywordNotImplemented,
                    part,
                    span,
                ));
                continue;
            }
        }

        if matches!(
            kw_name.as_str(),
            "cycling" | "basic landcycling" | "landcycling" | "typecycling"
        ) {
            if kw_name == "basic landcycling" {
                kd.cycling_filter = Some(Box::new(ObjectFilter {
                    supertypes_any: SupertypeMask::BASIC,
                    types_any: CardTypeMask::LAND,
                    ..Default::default()
                }));
                kd.cycling_desc = Some("basic land".to_string());
            } else if kw_name == "landcycling" {
                kd.cycling_filter = Some(Box::new(ObjectFilter {
                    types_any: CardTypeMask::LAND,
                    ..Default::default()
                }));
                kd.cycling_desc = Some("land".to_string());
            } else if kw_name == "typecycling" && kd.cycling_filter.is_none() {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(
                    UnsupportedReason::KeywordNotImplemented,
                    part,
                    span,
                ));
                continue;
            }
            let raw_c = kd
                .cost
                .as_ref()
                .map(|c| c.raw.as_str())
                .or(kd.param.as_deref())
                .unwrap_or("");
            let cp = {
                let mut sub_ctx = ctx.with_normalized_text(raw_c);
                parse_cost_list(raw_c, 0, &mut sub_ctx)
            };
            if !cp.unparsed.is_empty() || cp.defs.is_empty() {
                ctx.interner.rollback(part_cp);
                out.push(unsupported(UnsupportedReason::Unparsed, part, span));
                continue;
            }
            kd.cost_defs = cp.defs;
        }

        let raw_idx = line.raw_index;
        let id = if kw_name == "equip" {
            Some(format!("equip_{raw_idx}"))
        } else {
            let prefix = match kw_name.as_str() {
                "cycling" | "basic landcycling" | "landcycling" | "typecycling" => Some("cycling"),
                "crew" => Some("crew"),
                "saddle" => Some("saddle"),
                "unearth" => Some("unearth"),
                "station" => Some("station"),
                "level up" => Some("level_up"),
                "transmute" => Some("transmute"),
                "reinforce" => Some("reinforce"),
                "ninjutsu" => Some("ninjutsu"),
                "commander ninjutsu" => Some("commander_ninjutsu"),
                "scavenge" => Some("scavenge"),
                "outlast" => Some("outlast"),
                "embalm" => Some("embalm"),
                "eternalize" => Some("eternalize"),
                "encore" => Some("encore"),
                "craft" => Some("craft"),
                "reconfigure" => Some("reconfigure"),
                "transfigure" => Some("transfigure"),
                "aura swap" => Some("aura_swap"),
                _ => spec
                    .filter(|s| s.cost_parse_mode == Some(CostParseMode::Activated))
                    .and_then(|s| s.activated_id_prefix),
            };
            prefix.map(|p| {
                if i == 0 {
                    format!("{p}_{raw_idx}")
                } else {
                    format!("{p}_{raw_idx}_{i}")
                }
            })
        };

        let saved_n = kd.n;
        let saved_param = kd.param.clone();
        out.push(AbilityDef::Keyword(KeywordAbility {
            keyword: kd,
            id,
            span,
        }));

        if kw_name == "vanishing" {
            out.extend(build_vanishing_abilities(saved_n, span, ctx.interner));
        } else if kw_name == "fading"
            && let Some(fn_val) = saved_n
        {
            out.extend(build_fading_abilities(fn_val, span, ctx.interner));
        } else if kw_name == "partner"
            && let Some(p) = saved_param
            && starts_with_ci(&p, "with ")
        {
            let partner_name = p["with ".len()..].trim().to_string();
            let mut search_filter = ObjectFilter::default();
            search_filter.extra.push(ObjectPredicate::Named {
                name: partner_name.clone(),
            });
            out.push(AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: false,
                    do_this_once_per_turn: false,
                    is_state_trigger: false,
                    or: Vec::new(),
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                },
                condition: None,
                grant_condition: None,
                targets: vec![TargetSpec {
                    kind: TargetKind::Player,
                    filter: ObjectFilter::default(),
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::Any,
                    }),
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
                    zones: ZoneMask::empty(),
                }],
                effect: vec![EffectStep {
                    op: EffectOp::Search {
                        spec: Box::new(SearchSpec {
                            filter: Box::new(search_filter),
                            count: 1,
                            up_to: true,
                            stated_quality: true,
                            destinations: vec![SearchDestination {
                                zone: Zone::Hand,
                                tapped: false,
                                on_top: false,
                            }],
                            reveal: true,
                            description: Some(format!("a card named {partner_name}")),
                            zones: ZoneMask::empty(),
                            different_names: false,
                            shuffle_only_if_library: false,
                            searched_player: None,
                            cast_free: false,
                            target_controller_as_actor: false,
                        }),
                        tapped: false,
                    },
                    subject: Selector::None,
                    actor: Some(PlayerRef::Target { index: 0 }),
                    amount: Some(Box::new(Amount::Const { n: 1 })),
                    optional: Some(mtg_ir::OptionalSpec {
                        chooser: PlayerRef::Target { index: 0 },
                        cost: Vec::new(),
                        mode: mtg_ir::OptionalMode::DoIfPaid,
                    }),
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span,
                }],
                modal: None,
                text: Some(part.to_string()),
                span,
            }));
        }
    }

    Some(out)
}

fn build_vanishing_abilities(
    n: Option<i32>,
    span: Span,
    interner: &mut Interner,
) -> Vec<AbilityDef> {
    let time_id = interner.intern_counter("time");
    let mut out = Vec::new();
    if let Some(cnt) = n
        && cnt > 0
    {
        out.push(AbilityDef::Replacement(ReplacementAbility {
            replacement: ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((time_id, Box::new(Amount::Const { n: cnt }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            },
            span,
        }));
    }
    out.push(AbilityDef::Triggered(TriggeredAbility {
        trigger: TriggerDef {
            event: EventKind::PhaseBegin,
            filter: EventFilter {
                detail: Some(EventDetail::Phase {
                    phase: Some(GamePhase::Upkeep),
                    any_main_phase: false,
                    enchanted_controller_upkeep: false,
                    enchanted_player_upkeep: false,
                    enchanted_opponent_end_step: false,
                    phase_chosen_player: false,
                    turn_filter: None,
                }),
                ..Default::default()
            },
            scope: Some(TriggerScope::ControllerTurn),
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: Vec::new(),
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        },
        condition: Some(Box::new(Condition::Compare {
            left: Box::new(Amount::CountersOn {
                counter: CounterTarget::Specific(time_id),
                of: mtg_ir::Ref::SelfObj,
            }),
            op: CompareOp::Ge,
            right: Box::new(Amount::Const { n: 1 }),
        })),
        grant_condition: None,
        targets: Vec::new(),
        effect: vec![EffectStep {
            op: EffectOp::RemoveCounters {
                counter: Some(time_id),
                up_to: false,
                if_unable_sacrifice_self: false,
                remove_all_from_target_or_player: false,
            },
            subject: Selector::SelfObj,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 1 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span,
        }],
        modal: None,
        text: Some(
            "At the beginning of your upkeep, if ~ has a time counter on it, remove a time counter from it."
                .to_string(),
        ),
        span,
    }));
    out.push(AbilityDef::Triggered(TriggeredAbility {
        trigger: TriggerDef {
            event: EventKind::CounterRemoved,
            filter: EventFilter {
                object: Some(TriggerSubject::SelfObj),
                detail: Some(EventDetail::Counter {
                    counter: Some(time_id),
                    last_counter: true,
                    put_by_player: None,
                    nth_counter_on_self: None,
                }),
                ..Default::default()
            },
            scope: None,
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: Vec::new(),
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        },
        condition: None,
        grant_condition: None,
        targets: Vec::new(),
        effect: vec![EffectStep {
            op: EffectOp::Sacrifice {
                select: SelectionMod::default(),
            },
            subject: Selector::SelfObj,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span,
        }],
        modal: None,
        text: Some("When the last time counter is removed from ~, sacrifice it.".to_string()),
        span,
    }));
    out
}

fn build_fading_abilities(n: i32, span: Span, interner: &mut Interner) -> Vec<AbilityDef> {
    let fade_id = interner.intern_counter("fade");
    let mut out = Vec::new();
    if n > 0 {
        out.push(AbilityDef::Replacement(ReplacementAbility {
            replacement: ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((fade_id, Box::new(Amount::Const { n }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            },
            span,
        }));
    }
    out.push(AbilityDef::Triggered(TriggeredAbility {
        trigger: TriggerDef {
            event: EventKind::PhaseBegin,
            filter: EventFilter {
                detail: Some(EventDetail::Phase {
                    phase: Some(GamePhase::Upkeep),
                    any_main_phase: false,
                    enchanted_controller_upkeep: false,
                    enchanted_player_upkeep: false,
                    enchanted_opponent_end_step: false,
                    phase_chosen_player: false,
                    turn_filter: None,
                }),
                ..Default::default()
            },
            scope: Some(TriggerScope::ControllerTurn),
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: Vec::new(),
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        },
        condition: None,
        grant_condition: None,
        targets: Vec::new(),
        effect: vec![EffectStep {
            op: EffectOp::RemoveCounters {
                counter: Some(fade_id),
                up_to: false,
                if_unable_sacrifice_self: true,
                remove_all_from_target_or_player: false,
            },
            subject: Selector::SelfObj,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 1 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span,
        }],
        modal: None,
        text: Some(
            "At the beginning of your upkeep, remove a fade counter from ~. If you can't, sacrifice it."
                .to_string(),
        ),
        span,
    }));
    out
}

#[allow(dead_code)]
fn _keep_unused_imports_referenced(_c: Compare, _s: SubtypeId, _p: Punct) {
    let _ = (parse_small_number("1"), singular_subtype("Elves"));
}
