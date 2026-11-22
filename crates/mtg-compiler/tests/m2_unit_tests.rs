use mtg_compiler::parser::ParseCtx;
use mtg_compiler::parser::cost::{
    is_cast_payable_cost, is_equip_payable_cost, is_legacy_cast_payable_cost,
    is_spell_alt_cost_compatible, is_supported_optional_cost, parse_additional_cost_sentence,
    parse_cost_delta, parse_cost_list, parse_cost_modifier_sentence, parse_loyalty_cost,
    parse_mana_spending_restriction, split_cost_parts,
};
use mtg_compiler::parser::filter::{
    FilterOpts, NullScope, object_filter_to_plain_mana_filter, parse_divided_targets,
    parse_multi_or_object_phrase, parse_object_filter, parse_object_filter_exact,
    parse_plain_mana_filter, parse_player_filter, parse_single_object_filter, parse_target_spec,
    singular_subtype,
};
use mtg_compiler::parser::keyword::{GRANTABLE_KEYWORDS, KEYWORD_SPECS};
use mtg_compiler::parser::value::{
    parse_amount, parse_compare, parse_condition, parse_count_word, parse_devotion_color_list,
    parse_player_ref, parse_ref, parse_small_number,
};
use mtg_compiler::{
    Interner, NormalizedSentence, RawCardInput, RawFaceInput, build_characteristics,
    compile_bundle, compile_card, replace_self_references, split_quoted_sentence_endings, tokenize,
};
use mtg_ir::{
    AbilityDef, ActivationRestriction, Amount, CardLayout, ColorMask, CompareOp, Condition,
    CostDef, CostDeltaMode, CostObjectTarget, CostPaidCountKind, FilterFlags,
    FirstActivationZeroKind, LoyaltySign, ManaColor, ObjectPredicate, PlayerRef, PlayerRelation,
    Ref, RelativeControl, RoundMode, Span, SpellCastRestrictionKind, StaticBody, TargetKind,
    Timing, UnsupportedReason, Zone, ZoneMask,
};

fn make_face(name: &str, mana_cost: &str, type_line: &str, oracle_text: &str) -> RawFaceInput {
    RawFaceInput {
        name: name.into(),
        mana_cost: mana_cost.into(),
        type_line: type_line.into(),
        oracle_text: oracle_text.into(),
        power: if type_line.contains("Creature")
            || type_line.contains("Vehicle")
            || type_line.contains("Spacecraft")
        {
            Some("2".into())
        } else {
            None
        },
        toughness: if type_line.contains("Creature")
            || type_line.contains("Vehicle")
            || type_line.contains("Spacecraft")
        {
            Some("3".into())
        } else {
            None
        },
        loyalty: if type_line.contains("Planeswalker") {
            Some("4".into())
        } else {
            None
        },
        defense: if type_line.contains("Battle") {
            Some("5".into())
        } else {
            None
        },
        colors: Vec::new(),
        color_indicator: None,
        keywords: Vec::new(),
        cmc: None,
    }
}

fn make_card(
    oracle_id: &str,
    name: &str,
    layout: Option<&str>,
    mana_cost: &str,
    type_line: &str,
    oracle_text: &str,
    faces: Vec<RawFaceInput>,
) -> RawCardInput {
    let f = make_face(name, mana_cost, type_line, oracle_text);
    RawCardInput {
        oracle_id: oracle_id.into(),
        name: name.into(),
        layout: layout.map(str::to_string),
        mana_cost: mana_cost.into(),
        type_line: type_line.into(),
        oracle_text: oracle_text.into(),
        power: f.power.clone(),
        toughness: f.toughness.clone(),
        loyalty: f.loyalty.clone(),
        defense: f.defense.clone(),
        colors: Vec::new(),
        color_indicator: None,
        faces,
        back_face: None,
    }
}

#[test]
fn test_all_10_layouts_and_split_combined_mv() {
    let mut interner = Interner::new();

    // 1. Normal
    let c_norm = make_card(
        "syn-l1",
        "Synth Flyer",
        Some("normal"),
        "{1}{W}",
        "Creature — Bird",
        "Flying",
        vec![],
    );
    let ir_norm = compile_card(&c_norm, &mut interner).unwrap();
    assert_eq!(ir_norm.layout, CardLayout::Normal);
    assert!(ir_norm.fully_supported);

    // 2. Split: combined mana value
    let f1 = make_face("Alpha", "{1}{R}", "Instant", "Flying");
    let f2 = make_face("Beta", "{2}{U}", "Sorcery", "Vigilance");
    let c_split = make_card(
        "syn-l2",
        "Alpha // Beta",
        Some("split"),
        "",
        "",
        "",
        vec![f1, f2],
    );
    let ir_split = compile_card(&c_split, &mut interner).unwrap();
    assert_eq!(ir_split.layout, CardLayout::Split);
    assert_eq!(ir_split.faces.len(), 2);
    assert_eq!(ir_split.faces[0].characteristics.mana_cost.mana_value(), 2);
    assert_eq!(ir_split.faces[1].characteristics.mana_cost.mana_value(), 3);

    // 3..9 Other supported multiface/special layouts
    for (raw_layout, expected) in [
        ("adventure", CardLayout::Adventure),
        ("transform", CardLayout::Transform),
        ("modal_dfc", CardLayout::ModalDfc),
        ("flip", CardLayout::Flip),
        ("omen", CardLayout::Omen),
        ("prepare", CardLayout::Prepare),
        ("meld", CardLayout::Meld),
    ] {
        let a = make_face("Front Side", "{1}{G}", "Creature — Elf", "Reach");
        let b = make_face("Back Side", "{2}{G}", "Creature — Beast", "Trample");
        let c = make_card(
            &format!("syn-{raw_layout}"),
            "Front Side // Back Side",
            Some(raw_layout),
            "",
            "",
            "",
            vec![a, b],
        );
        let ir = compile_card(&c, &mut interner).unwrap();
        assert_eq!(ir.layout, expected, "layout mismatch for {raw_layout}");
        assert_eq!(ir.faces.len(), 2);
        assert!(ir.fully_supported);
    }

    // 10. ReversibleCard -> flagged unsupported with UnsupportedReason::Multiface
    let ra = make_face("Rev A", "{W}", "Creature — Fox", "First strike");
    let rb = make_face("Rev B", "{W}", "Creature — Fox", "Lifelink");
    let c_rev = make_card(
        "syn-rev",
        "Rev A // Rev B",
        Some("reversible_card"),
        "",
        "",
        "",
        vec![ra, rb],
    );
    let ir_rev = compile_card(&c_rev, &mut interner).unwrap();
    assert_eq!(ir_rev.layout, CardLayout::ReversibleCard);
    assert!(!ir_rev.fully_supported);
    assert!(matches!(
        ir_rev.faces[0].abilities.last(),
        Some(AbilityDef::Unsupported(u)) if u.reason == UnsupportedReason::Multiface
    ));
}

#[test]
fn test_station_pt_clearing_battle_gate_and_basic_land_mana_sharing() {
    let mut interner = Interner::new();

    // CR 722.2c: Non-creature Spacecraft with Station clears printed P/T to None when lacking creature-at-N+
    let c_station_noncreature = make_card(
        "syn-st1",
        "Orbital Platform",
        Some("normal"),
        "{3}",
        "Artifact — Spacecraft",
        "Station\n3+ | Flying",
        vec![],
    );
    let ir_st1 = compile_card(&c_station_noncreature, &mut interner).unwrap();
    assert!(ir_st1.faces[0].characteristics.pt.is_none());
    assert!(ir_st1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("station")
    )));

    // Spacecraft with threshold striation
    let c_station_thresh = make_card(
        "syn-st2",
        "Battle Cruiser",
        Some("normal"),
        "{4}",
        "Artifact — Spacecraft",
        "Station\n3+ | Flying\n6+ | Trample\n5/6",
        vec![],
    );
    let ir_st2 = compile_card(&c_station_thresh, &mut interner).unwrap();
    assert!(ir_st2.faces[0].abilities.len() >= 3);

    // Battle — Control Point gate -> UnsupportedReason::Unparsed
    let c_cp = make_card(
        "syn-cp",
        "Frontier Node",
        Some("normal"),
        "{3}{R}",
        "Battle — Control Point",
        "Flying",
        vec![],
    );
    let ir_cp = compile_card(&c_cp, &mut interner).unwrap();
    assert!(!ir_cp.fully_supported);
    assert!(matches!(
        ir_cp.faces[0].abilities.as_slice(),
        [AbilityDef::Unsupported(u)] if u.reason == UnsupportedReason::StaticNotCompiled
    ));

    // Siege Battle gets Siege inherent ability + keywords
    let c_siege = make_card(
        "syn-siege",
        "Invasion Test",
        Some("normal"),
        "{2}{W}",
        "Battle — Siege",
        "Vigilance",
        vec![],
    );
    let ir_siege = compile_card(&c_siege, &mut interner).unwrap();
    assert!(!ir_siege.faces[0].abilities.is_empty());

    // CR 305.6 Dual basic-land types without explicit "{T}: Add ..." emit shared intrinsic mana ability
    let c_dual = make_card(
        "syn-dual",
        "Synth Savanna",
        Some("normal"),
        "",
        "Land — Forest Plains",
        "",
        vec![],
    );
    let ir_dual = compile_card(&c_dual, &mut interner).unwrap();
    assert!(ir_dual.fully_supported);
    assert_eq!(ir_dual.faces[0].abilities.len(), 2);
    assert!(ir_dual.faces[0].abilities.iter().all(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.intrinsic && a.is_mana && a.timing == Timing::Mana
    )));
}

#[test]
fn test_class_leveler_case_and_backup_striations() {
    let mut interner = Interner::new();

    // Class card with levels 1, 2, 3
    let c_class = make_card(
        "syn-class",
        "Test Scholar Class",
        Some("normal"),
        "{1}{U}",
        "Enchantment — Class",
        "Vigilance\n{2}{U}: Level 2\nFlying\n{4}{U}: Level 3\nHexproof",
        vec![],
    );
    let ir_class = compile_card(&c_class, &mut interner).unwrap();
    assert!(ir_class.fully_supported);
    assert!(ir_class.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.id.starts_with("class_level_") && a.timing == Timing::Sorcery
    )));

    // Leveler creature with Level up + level 2-4 + level 5+ striations
    let c_lvl = make_card(
        "syn-lvl",
        "Ascendant Apprentice",
        Some("normal"),
        "{W}",
        "Creature — Human Monk",
        "Level up {1}{W}\nLEVEL 2-4\n3/4\nFlying\nLEVEL 5+\n5/5\nDouble strike",
        vec![],
    );
    let ir_lvl = compile_card(&c_lvl, &mut interner).unwrap();
    assert!(ir_lvl.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if s.static_def.condition.is_some()
    )));

    // Case card: "To solve" / "Solved" recognition
    let c_case = make_card(
        "syn-case",
        "Case of the Synthetic Clue",
        Some("normal"),
        "{1}{W}",
        "Enchantment — Case",
        "Vigilance\nTo solve — You control three or more artifacts.\nSolved — Hexproof",
        vec![],
    );
    let ir_case = compile_card(&c_case, &mut interner).unwrap();
    assert!(ir_case.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(sa) if sa.static_def.condition.is_some()
            && matches!(sa.static_def.body, StaticBody::Continuous { .. })
    )));

    // Backup N post-pass sets backup_ability_start_index on backup keyword ability
    let c_backup = make_card(
        "syn-backup",
        "Supportive Scout",
        Some("normal"),
        "{2}{W}",
        "Creature — Bird Soldier",
        "Backup 1\nFlying\nLifelink",
        vec![],
    );
    let ir_backup = compile_card(&c_backup, &mut interner).unwrap();
    let backup_ab = ir_backup.faces[0].abilities.iter().find_map(|ab| match ab {
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("backup") => {
            Some(&ka.keyword)
        }
        _ => None,
    });
    assert_eq!(
        backup_ab.and_then(|k| k.backup_ability_start_index),
        Some(1)
    );

    // Max speed + infinity threshold
    let c_maxspeed = make_card(
        "syn-maxspeed",
        "Velocity Racer",
        Some("normal"),
        "{1}{R}",
        "Creature — Pilot",
        "Haste\nMax speed — Menace\n∞ — Trample",
        vec![],
    );
    let ir_ms = compile_card(&c_maxspeed, &mut interner).unwrap();
    let gated_kws = ir_ms.faces[0]
        .abilities
        .iter()
        .filter(|ab| matches!(ab, AbilityDef::Static(sa) if sa.static_def.condition.is_some()))
        .count();
    assert_eq!(gated_kws, 2);
}

#[test]
fn test_keyword_catalog_and_special_keywords() {
    assert_eq!(KEYWORD_SPECS.len(), 218);
    assert_eq!(KEYWORD_SPECS.iter().filter(|s| s.implemented).count(), 214);
    assert_eq!(KEYWORD_SPECS.iter().filter(|s| !s.implemented).count(), 4);
    assert_eq!(GRANTABLE_KEYWORDS.len(), 109);

    let mut interner = Interner::new();

    // Unimplemented keyword: Visit; Space sculptor -> StaticBody::SpaceSculptorSectors
    let c_visit = make_card(
        "syn-visit",
        "Park Patron",
        Some("normal"),
        "{2}{W}",
        "Creature — Guest",
        "Visit",
        vec![],
    );
    let ir_visit = compile_card(&c_visit, &mut interner).unwrap();
    assert!(!ir_visit.fully_supported);
    assert!(matches!(
        ir_visit.faces[0].abilities.as_slice(),
        [AbilityDef::Unsupported(u)] if u.reason == UnsupportedReason::KeywordNotImplemented
    ));

    let c_space = make_card(
        "syn-space",
        "Sector Mapper",
        Some("normal"),
        "{2}{U}",
        "Creature — Human",
        "Space sculptor",
        vec![],
    );
    let ir_space = compile_card(&c_space, &mut interner).unwrap();
    assert!(ir_space.fully_supported);
    assert!(matches!(
        ir_space.faces[0].abilities.as_slice(),
        [AbilityDef::Static(s)] if matches!(s.static_def.body, StaticBody::SpaceSculptorSectors)
    ));

    // Vanishing 3 expands into Vanishing + ETB counters + upkeep/sac triggers
    let c_van = make_card(
        "syn-van",
        "Ephemeral Wisp",
        Some("normal"),
        "{1}{U}",
        "Creature — Illusion",
        "Vanishing 3",
        vec![],
    );
    let ir_van = compile_card(&c_van, &mut interner).unwrap();
    assert!(ir_van.fully_supported);
    assert!(ir_van.faces[0].abilities.len() >= 3);

    // Fading 2 expands into Fading + ETB fade counters + upkeep/sac trigger
    let c_fade = make_card(
        "syn-fade",
        "Fading Automaton",
        Some("normal"),
        "{2}",
        "Artifact Creature — Construct",
        "Fading 2",
        vec![],
    );
    let ir_fade = compile_card(&c_fade, &mut interner).unwrap();
    assert!(ir_fade.fully_supported);
    assert!(ir_fade.faces[0].abilities.len() >= 2);

    // Modular—Sunburst
    let c_mod_sun = make_card(
        "syn-modsun",
        "Prismatic Construct",
        Some("normal"),
        "{4}",
        "Artifact Creature — Construct",
        "Modular—Sunburst",
        vec![],
    );
    let ir_mod = compile_card(&c_mod_sun, &mut interner).unwrap();
    assert!(ir_mod.fully_supported);
    assert!(ir_mod.faces[0].abilities.len() >= 2);

    // Offering: Fox offering
    let c_offer = make_card(
        "syn-offer",
        "Patron Spirit",
        Some("normal"),
        "{3}{W}{W}",
        "Creature — Spirit",
        "Fox offering",
        vec![],
    );
    let ir_offer = compile_card(&c_offer, &mut interner).unwrap();
    assert!(ir_offer.fully_supported);
    assert!(ir_offer.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("offering")
    )));

    // Suspend X — {W}{W}
    let c_susp_x = make_card(
        "syn-suspx",
        "Delayed Decree",
        Some("normal"),
        "{X}{W}",
        "Sorcery",
        "Suspend X—{W}{W}",
        vec![],
    );
    let ir_suspx = compile_card(&c_susp_x, &mut interner).unwrap();
    assert!(ir_suspx.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("suspend") && ka.keyword.suspend_x
    )));

    // Companion
    let c_comp = make_card(
        "syn-comp",
        "Loyal Companion",
        Some("normal"),
        "{1}{W}",
        "Legendary Creature — Cat",
        "Companion — Each nonland card in your starting deck has an even mana value.",
        vec![],
    );
    let ir_comp = compile_card(&c_comp, &mut interner).unwrap();
    assert!(ir_comp.fully_supported);
    assert!(ir_comp.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("companion")
    )));

    // Prototype {1}{G} — 2/2
    let c_proto = make_card(
        "syn-proto",
        "Steel Colossus",
        Some("normal"),
        "{7}",
        "Artifact Creature — Golem",
        "Prototype {1}{G} — 2/2\nTrample",
        vec![],
    );
    let ir_proto = compile_card(&c_proto, &mut interner).unwrap();
    assert!(ir_proto.fully_supported);
    assert!(ir_proto.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if ka.keyword.prototype_spec.is_some()
    )));

    // Equip {2} -> id `equip_0` without `_{i}` suffix and empty cost_defs; Cycling {2} -> id `cycling_1`
    let c_equip = make_card(
        "syn-equip",
        "Gear Blade",
        Some("normal"),
        "{2}",
        "Artifact — Equipment",
        "Equip {2}\nCycling {2}",
        vec![],
    );
    let ir_eq = compile_card(&c_equip, &mut interner).unwrap();
    assert!(ir_eq.fully_supported);
    let eq_kw = ir_eq.faces[0].abilities.iter().find_map(|ab| match ab {
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("equip") => {
            Some(ka)
        }
        _ => None,
    });
    assert_eq!(eq_kw.and_then(|k| k.id.as_deref()), Some("equip_0"));
    assert!(eq_kw.unwrap().keyword.cost_defs.is_empty());

    // Partner with Synthetic Ally
    let c_partner = make_card(
        "syn-part",
        "Paired Warrior",
        Some("normal"),
        "{2}{W}",
        "Creature — Soldier",
        "Partner with Synthetic Ally",
        vec![],
    );
    let ir_part = compile_card(&c_partner, &mut interner).unwrap();
    assert!(ir_part.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("partner")
    )));

    // Special activated prefixes: Forecast in HAND zone, Boast, Exhaust, Waterbend, Power-up
    let c_act = make_card(
        "syn-actkw",
        "Keyword Specialist",
        Some("normal"),
        "{1}{U}",
        "Creature — Wizard",
        "Forecast — {1}{U}, Reveal ~ from your hand: Draw a card.\nBoast — {1}{R}: Put a +1/+1 counter on ~.\nExhaust — {3}{G}: Put two +1/+1 counters on ~.\nWaterbend {2}: Draw a card.\nPower-up — {1}{W}: ~ gains flying until end of turn.",
        vec![],
    );
    let ir_act = compile_card(&c_act, &mut interner).unwrap();
    for expected_id in ["forecast_0", "boast_1", "exhaust_2", "power_up_4"] {
        assert!(
            ir_act.faces[0].abilities.iter().any(|ab| match ab {
                AbilityDef::Unsupported(u) => u.id.as_deref() == Some(expected_id),
                AbilityDef::Activated(a) => a.id == expected_id,
                _ => false,
            }),
            "missing activated keyword prefix id {expected_id}"
        );
    }
    assert!(ir_act.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(ka) if interner.keyword_name(ka.keyword.keyword) == Some("waterbend")
    )));
}

#[test]
fn test_costs_loyalty_delta_spending_and_validators() {
    let mut interner = Interner::new();
    let face = make_face("Synthetic Card", "{1}{W}", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);
    let text = "{1}{B}, {T}, Sacrifice a creature, Pay 2 life, Discard a card";
    let mut ctx = ParseCtx {
        face_name: "Synthetic Card",
        characteristics: &ch,
        interner: &mut interner,
        normalized_text: text,
        card_mana_value: 2,
        has_read_ahead: false,
        mana_tap_taken: false,
        is_planeswalker: false,
        is_spell: false,
        is_class: false,
        is_case: false,
        is_battle: false,
        has_cleave_keyword: false,
        has_gift_keyword: false,
        prototype_spec: None,
    };

    let toks = tokenize(text, 0);
    let parts = split_cost_parts(text, &toks, 0);
    assert_eq!(parts.len(), 5);

    let parsed = parse_cost_list(text, 0, &mut ctx);
    assert!(parsed.unparsed.is_empty());
    assert_eq!(parsed.defs.len(), 5);
    assert!(matches!(parsed.defs[0], CostDef::Mana { .. }));
    assert!(matches!(parsed.defs[1], CostDef::TapSelf));
    assert!(matches!(parsed.defs[2], CostDef::Sacrifice { .. }));
    assert!(matches!(parsed.defs[3], CostDef::PayLife { .. }));
    assert!(matches!(parsed.defs[4], CostDef::Discard { .. }));

    // Loyalty cost edge cases (+2, -3, 0, +X, -X, bare X rejected as unparsed)
    let span = Span { start: 0, end: 2 };
    let l_plus = parse_loyalty_cost(&tokenize("+2", 0), "+2", span, true).unwrap();
    assert!(matches!(
        l_plus.defs.as_slice(),
        [CostDef::Loyalty {
            delta: 2,
            is_x: false,
            sign: None
        }]
    ));
    let l_minus = parse_loyalty_cost(&tokenize("-3", 0), "-3", span, true).unwrap();
    assert!(matches!(
        l_minus.defs.as_slice(),
        [CostDef::Loyalty {
            delta: -3,
            is_x: false,
            sign: None
        }]
    ));
    let l_zero = parse_loyalty_cost(&tokenize("0", 0), "0", span, true).unwrap();
    assert!(matches!(
        l_zero.defs.as_slice(),
        [CostDef::Loyalty {
            delta: 0,
            is_x: false,
            sign: None
        }]
    ));
    let l_px = parse_loyalty_cost(&tokenize("+X", 0), "+X", span, true).unwrap();
    assert!(matches!(
        l_px.defs.as_slice(),
        [CostDef::Loyalty {
            is_x: true,
            sign: Some(LoyaltySign::Plus),
            ..
        }]
    ));
    let l_mx = parse_loyalty_cost(&tokenize("-X", 0), "-X", span, true).unwrap();
    assert!(matches!(
        l_mx.defs.as_slice(),
        [CostDef::Loyalty {
            is_x: true,
            sign: Some(LoyaltySign::Minus),
            ..
        }]
    ));
    let l_bare_x = parse_loyalty_cost(&tokenize("X", 0), "X", span, true).unwrap();
    assert!(l_bare_x.defs.is_empty() && !l_bare_x.unparsed.is_empty());

    // CostDelta colored canonical ASCII order (B=66, G=71, R=82, U=85, W=87)
    let mana_toks = tokenize("{R}{W}{U}{2}", 0);
    let delta = parse_cost_delta(&mana_toks, CostDeltaMode::Reduce).unwrap();
    assert_eq!(delta.generic, 2);
    assert_eq!(delta.colored.len(), 3);
    assert_eq!(delta.colored[0].color, ManaColor::R);
    assert_eq!(delta.colored[1].color, ManaColor::U);
    assert_eq!(delta.colored[2].color, ManaColor::W);

    // Additional cost & cost modifier sentences
    let add_cp = parse_additional_cost_sentence(
        "As an additional cost to cast this spell, sacrifice a creature.",
        0,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(add_cp.unparsed.is_empty());
    assert!(matches!(
        add_cp.defs.as_slice(),
        [CostDef::Sacrifice { .. }]
    ));

    let mod_each = parse_cost_modifier_sentence("Spells cost {1} more to cast.", &mut ctx).unwrap();
    match &mod_each.body {
        StaticBody::CostModifier { who, .. } => {
            assert_eq!(who.relation, PlayerRelation::EachPlayer)
        }
        other => panic!("unexpected static body: {other:?}"),
    }

    let mod_opp = parse_cost_modifier_sentence(
        "Spells your opponents cast cost {1} more to cast.",
        &mut ctx,
    )
    .unwrap();
    match &mod_opp.body {
        StaticBody::CostModifier { who, .. } => {
            assert_eq!(who.relation, PlayerRelation::EachOpponent)
        }
        other => panic!("unexpected static body: {other:?}"),
    }

    // Mana spending restriction
    let restr =
        parse_mana_spending_restriction("Spend this mana only to cast creature spells.", &mut ctx)
            .unwrap();
    assert!(restr.allow_spells.is_some());

    // Validators
    let tap_cost = CostDef::TapSelf;
    assert!(!is_cast_payable_cost(&tap_cost));
    assert!(is_equip_payable_cost(&tap_cost));
    assert!(is_equip_payable_cost(&CostDef::PayLife {
        amount: Box::new(Amount::Const { n: 2 }),
        half_life_rounded_up: false
    }));
    let mana_x_cost = CostDef::Mana {
        cost: mtg_ir::ManaCost::parse("{X}"),
        x_spend_colors: ColorMask::empty(),
        x_colored_only: false,
        x_max_one_per_color: false,
    };
    assert!(!is_equip_payable_cost(&mana_x_cost));
    assert!(!is_legacy_cast_payable_cost(&CostDef::Sacrifice {
        count: Box::new(Amount::Const { n: 1 }),
        target: CostObjectTarget::SelfObj,
        sacrifice_all: false,
        half_rounded_up: false,
        attached_creature: false,
        one_or_more: false,
        up_to: None,
        different_names: false,
    }));
    assert!(is_spell_alt_cost_compatible(&parsed.defs[2]));
    assert!(is_supported_optional_cost(&parsed.defs[2]));
}

#[test]
fn test_filters_targets_values_and_conditions() {
    let mut interner = Interner::new();
    let face = make_face("Synthetic Card", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);
    let mut ctx = ParseCtx {
        face_name: "Synthetic Card",
        characteristics: &ch,
        interner: &mut interner,
        normalized_text: "",
        card_mana_value: 2,
        has_read_ahead: false,
        mana_tap_taken: false,
        is_planeswalker: false,
        is_spell: false,
        is_class: false,
        is_case: false,
        is_battle: false,
        has_cleave_keyword: false,
        has_gift_keyword: false,
        prototype_spec: None,
    };
    let mut scope = NullScope;

    // 1. Explicit zone overrides default_zone (graveyard -> ZoneMask::GRAVEYARD, not 4|8)
    let gy_toks = tokenize("creature card in your graveyard", 0);
    let (gy_f, used) = parse_object_filter(
        &gy_toks,
        &mut ctx,
        FilterOpts {
            default_zone: ZoneMask::BATTLEFIELD,
            ..Default::default()
        },
        &scope,
    )
    .unwrap();
    assert_eq!(used, gy_toks.len());
    assert_eq!(gy_f.zones, ZoneMask::GRAVEYARD);
    assert_eq!(gy_f.owner, Some(RelativeControl::You));

    // 2. Pure-color disjunction collapses to colors_any (no AnyOf)
    let col_toks = tokenize("green spells and white spells", 0);
    let (col_f, c_used) = parse_object_filter(
        &col_toks,
        &mut ctx,
        FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: true,
            allow_spell_noun: true,
            ..Default::default()
        },
        &scope,
    )
    .unwrap();
    assert_eq!(c_used, col_toks.len());
    assert!(col_f.extra.is_empty());
    assert_eq!(col_f.colors_any, ColorMask::G | ColorMask::W);

    // 3. CastFromZones & CastOutsideHand predicates + outlaw expansion
    let outlaw_f = parse_multi_or_object_phrase(
        "outlaws",
        &mut ctx,
        FilterOpts {
            expand_outlaw_subtypes: true,
            allow_plural_and_disjunction: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(outlaw_f.subtypes_any.len(), 5);

    let cf_toks = tokenize("spell cast from your graveyard", 0);
    let (cf_f, _) = parse_object_filter(&cf_toks, &mut ctx, FilterOpts::default(), &scope).unwrap();
    assert!(cf_f.extra.iter().any(|p| matches!(
        p,
        ObjectPredicate::CastFromZones { zones } if *zones == ZoneMask::GRAVEYARD
    )));

    let coh_toks = tokenize("spell cast from anywhere other than your hand", 0);
    let (coh_f, _) =
        parse_object_filter(&coh_toks, &mut ctx, FilterOpts::default(), &scope).unwrap();
    assert!(coh_f.flags.contains(FilterFlags::CAST_OUTSIDE_HAND));

    // 4. PlainManaFilter conversion & TargetSpec & Divided targets
    assert!(object_filter_to_plain_mana_filter(&gy_f).is_none());
    let pmf = object_filter_to_plain_mana_filter(&col_f).unwrap();
    assert_eq!(pmf.colors_any, ColorMask::G | ColorMask::W);
    let (pmf2, _) =
        parse_plain_mana_filter(&tokenize("white or blue creature", 0), &mut ctx).unwrap();
    assert!(pmf2.colors_any.contains(ColorMask::W | ColorMask::U));

    let (tgt, _) = parse_target_spec(
        &tokenize("target creature an opponent controls", 0),
        &mut ctx,
        &mut scope,
    )
    .unwrap();
    assert_eq!(tgt.kind, TargetKind::Object);
    assert_eq!(tgt.min, 1);
    assert_eq!(tgt.max, 1);

    let (div, _) = parse_divided_targets(
        &tokenize(
            "3 damage divided as you choose among one or two target creatures",
            0,
        ),
        &mut ctx,
        &mut scope,
    )
    .unwrap();
    assert_eq!(div.min, 1);
    assert_eq!(div.max, 2);
    assert!(matches!(
        div.divide_total.as_deref(),
        Some(Amount::Const { n: 3 })
    ));

    let (pf, _) = parse_player_filter(&tokenize("each opponent", 0)).unwrap();
    assert_eq!(pf.relation, PlayerRelation::EachOpponent);
    assert_eq!(singular_subtype("Wolves").as_deref(), Some("Wolf"));

    // 5. Values, references, compares, amounts, conditions
    assert_eq!(parse_small_number("seventeen"), Some(17));
    assert!(matches!(
        parse_count_word("three"),
        Some(Amount::Const { n: 3 })
    ));
    assert_eq!(
        parse_devotion_color_list("white and blue"),
        Some(ColorMask::W | ColorMask::U)
    );
    assert_eq!(
        parse_ref(&tokenize("this creature", 0), &scope),
        Some((Ref::SelfObj, 2))
    );
    let (pref, _) = parse_player_ref(&tokenize("its controller", 0), &mut scope).unwrap();
    assert_eq!(
        pref,
        PlayerRef::ControllerOf {
            of: Box::new(Ref::SelfObj)
        }
    );
    let (cmp, _) = parse_compare(&tokenize("three or greater", 0), &mut ctx, &mut scope).unwrap();
    assert_eq!(cmp.op, CompareOp::Ge);
    let (amt, _) = parse_amount(&tokenize("twice X", 0), &mut ctx, &mut scope).unwrap();
    assert!(matches!(amt, Amount::Mul { factor: 2, .. }));
    let (cond, _) = parse_condition(
        &tokenize("if you control a creature", 0),
        &mut ctx,
        &mut scope,
    )
    .unwrap();
    assert!(matches!(
        cond,
        Condition::Compare {
            op: CompareOp::Ge,
            ..
        }
    ));
}

#[test]
fn test_rename_invariance_m2_and_normalize_m1_cleanups() {
    // 1. Rename invariance across M2 with real self-references (full name, short name, possessive, named <FullName>, named <OtherCard>)
    let mut i1 = Interner::new();
    let mut i2 = Interner::new();
    let c1_full = make_card(
        "syn-ren-1",
        "Zeta, Sky Sentinel",
        Some("normal"),
        "{2}{W}",
        "Legendary Artifact Creature — Angel",
        "Zeta, Sky Sentinel costs {1} less to cast for each artifact you control.\n\
         Creature spells named Other Legend cost {1} less to cast.\n\
         Activated abilities of creatures you control cost {X} less to activate, where X is Zeta, Sky Sentinel's power.\n\
         Ward—Pay 2 life.\n\
         Flying\n\
         Forecast — {0}, Reveal Zeta, Sky Sentinel from your hand: Draw a card.\n\
         {1}{W}, Sacrifice Zeta, Sky Sentinel: Draw a card.\n\
         A deck can have any number of cards named Zeta, Sky Sentinel.",
        vec![],
    );
    let c2_full = make_card(
        "syn-ren-2",
        "Alpha, Dawn Guardian",
        Some("normal"),
        "{2}{W}",
        "Legendary Artifact Creature — Angel",
        "Alpha, Dawn Guardian costs {1} less to cast for each artifact you control.\n\
         Creature spells named Other Legend cost {1} less to cast.\n\
         Activated abilities of creatures you control cost {X} less to activate, where X is Alpha, Dawn Guardian's power.\n\
         Ward—Pay 2 life.\n\
         Flying\n\
         Forecast — {0}, Reveal Alpha, Dawn Guardian from your hand: Draw a card.\n\
         {1}{W}, Sacrifice Alpha, Dawn Guardian: Draw a card.\n\
         A deck can have any number of cards named Alpha, Dawn Guardian.",
        vec![],
    );
    let c1_short = make_card(
        "syn-ren-s",
        "Zeta, Sky Sentinel",
        Some("normal"),
        "{2}{W}",
        "Legendary Creature — Angel",
        "Zeta costs {1} less to cast if Zeta is enchanted.\n\
         Forecast — {0}, Reveal Zeta from your hand: Draw a card.\n\
         Put Zeta from your graveyard on the bottom of your library: Draw a card.\n\
         Activated abilities of creatures you control cost {X} less to activate, where X is Zeta’s power.",
        vec![],
    );
    let c2_short = make_card(
        "syn-ren-s",
        "Alpha, Dawn Guardian",
        Some("normal"),
        "{2}{W}",
        "Legendary Creature — Angel",
        "Alpha costs {1} less to cast if Alpha is enchanted.\n\
         Forecast — {0}, Reveal Alpha from your hand: Draw a card.\n\
         Put Alpha from your graveyard on the bottom of your library: Draw a card.\n\
         Activated abilities of creatures you control cost {X} less to activate, where X is Alpha’s power.",
        vec![],
    );

    let ir1 = compile_card(&c1_full, &mut i1).unwrap();
    let ir2 = compile_card(&c2_full, &mut i2).unwrap();
    assert_eq!(ir1.faces[0].abilities, ir2.faces[0].abilities);
    assert_eq!(
        ir1.faces[0].characteristics.pt,
        ir2.faces[0].characteristics.pt
    );
    assert_eq!(i1.tables(), i2.tables());

    let ir1_s = compile_card(&c1_short, &mut Interner::new()).unwrap();
    let ir2_s = compile_card(&c2_short, &mut Interner::new()).unwrap();
    assert_eq!(ir1_s.faces[0].abilities, ir2_s.faces[0].abilities);

    // Bundle-level invariance (sorted InternTables + remapped CardDefs)
    let b1 = compile_bundle(&[c1_full], vec![]).unwrap();
    let b2 = compile_bundle(&[c2_full], vec![]).unwrap();
    assert_eq!(b1.tables, b2.tables);
    assert_eq!(
        b1.cards[0].faces[0].abilities,
        b2.cards[0].faces[0].abilities
    );

    // M1 Cleanup 1: >200 card name occurrences all replaced without truncation cap
    let single_name = "Bumble";
    let many = std::iter::repeat_n("When Bumble attacks, Bumble gets +1/+1.", 220)
        .collect::<Vec<_>>()
        .join(" ");
    let replaced = replace_self_references(&many, single_name, false, None);
    assert!(!replaced.contains("Bumble"));

    // M1 Cleanup 2: Unclosed ASCII quote before curly quoted ability
    let tricky_text = "Weird \" marker then “Bumble has flying.” Then Bumble attacks.";
    let r2 = replace_self_references(tricky_text, "Bumble", false, None);
    assert!(r2.ends_with("Then ~ attacks."));

    // M1 Cleanup 3: split_quoted_sentence_endings handles curly quotes and non-breaking spaces (\u{00A0})
    let quoted_multi = "“First sentence. Second sentence.”\u{00A0}Third sentence.";
    let sents = split_quoted_sentence_endings(&[NormalizedSentence {
        text: quoted_multi.into(),
        span: Span {
            start: 0,
            end: quoted_multi.len() as u32,
        },
    }]);
    assert!(sents.len() >= 2);
}

fn compile_single_for_test(
    name: &str,
    type_line: &str,
    oracle: &str,
) -> (mtg_ir::CardDef, Interner) {
    let mut interner = Interner::new();
    let raw = make_card(
        name,
        name,
        Some("normal"),
        "{2}{B}",
        type_line,
        oracle,
        vec![],
    );
    let card = compile_card(&raw, &mut interner).unwrap();
    (card, interner)
}

fn make_ctx<'a>(
    face_name: &'a str,
    ch: &'a mtg_ir::Characteristics,
    normalized_text: &'a str,
    interner: &'a mut Interner,
) -> ParseCtx<'a> {
    ParseCtx {
        face_name,
        characteristics: ch,
        interner,
        normalized_text,
        card_mana_value: 2,
        has_read_ahead: false,
        mana_tap_taken: false,
        is_planeswalker: false,
        is_spell: false,
        is_class: false,
        is_case: false,
        is_battle: false,
        has_cleave_keyword: false,
        has_gift_keyword: false,
        prototype_spec: None,
    }
}

#[test]
fn test_m2_r2_cost_parsing_regressions() {
    // 1. Chitinous Crawler: "Exile a permanent card from your graveyard" -> target: None
    let (cc, _) = compile_single_for_test(
        "Chitinous Crawler",
        "Creature — Insect",
        "{T}, Exile a permanent card from your graveyard: Put a +1/+1 counter on Chitinous Crawler.",
    );
    let cc_cost = match &cc.faces[0].abilities[0] {
        AbilityDef::Activated(a) => &a.cost,
        AbilityDef::Unsupported(u) => {
            assert_eq!(u.cost_complete, Some(true));
            &u.cost
        }
        other => panic!("expected Activated or Unsupported ability with cost, got {other:?}"),
    };
    assert!(matches!(
        &cc_cost[1],
        CostDef::ExileFrom {
            zone: mtg_ir::Zone::Graveyard,
            target: None,
            ..
        }
    ));

    // 2. Zombie Assassin: "Exile two cards from your graveyard and ~"
    let (za, _) = compile_single_for_test(
        "Zombie Assassin",
        "Creature — Zombie Assassin",
        "{T}, Exile two cards from your graveyard and Zombie Assassin: Destroy target nonblack creature.",
    );
    let za_cost = match &za.faces[0].abilities[0] {
        AbilityDef::Activated(a) => &a.cost,
        AbilityDef::Unsupported(u) => {
            assert_eq!(u.cost_complete, Some(true));
            &u.cost
        }
        other => panic!("expected Zombie Assassin Activated or Unsupported cost, got {other:?}"),
    };
    assert!(matches!(&za_cost[1], CostDef::ExileFrom { .. }));
    assert!(matches!(
        &za_cost[2],
        CostDef::ExileFrom {
            target: Some(CostObjectTarget::SelfObj),
            ..
        }
    ));

    // 3. Daniels, Temporal Agent: "Put this creature on the bottom of your library"
    let mut interner = Interner::new();
    let face = make_face("Korlash", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);
    let cp = parse_cost_list(
        "{T}, Put this creature on the bottom of your library",
        0,
        &mut make_ctx("Korlash", &ch, "", &mut interner),
    );
    assert!(cp.unparsed.is_empty());
    assert!(matches!(
        &cp.defs[1],
        CostDef::PutOnBottomOfLibrary {
            target: Some(CostObjectTarget::SelfObj),
            ..
        }
    ));

    // 4. Grandeur: "Discard another card named ~"
    let cp_g = parse_cost_list(
        "Discard another card named ~",
        0,
        &mut make_ctx("Korlash", &ch, "", &mut interner),
    );
    assert!(cp_g.unparsed.is_empty());
    if let CostDef::Discard {
        target: Some(CostObjectTarget::Filter { filter }),
        ..
    } = &cp_g.defs[0]
    {
        assert!(filter.flags.contains(FilterFlags::OTHER));
        assert!(filter.extra.contains(&ObjectPredicate::SameNameAsSource));
    } else {
        panic!("expected Grandeur Discard filter");
    }

    // 5. Tomb of Urami: "Sacrifice all lands you control" -> count=1, sacrifice_all=true
    let cp_tu = parse_cost_list(
        "Sacrifice all lands you control",
        0,
        &mut make_ctx("Tomb", &ch, "", &mut interner),
    );
    assert!(cp_tu.unparsed.is_empty());
    if let CostDef::Sacrifice {
        count,
        sacrifice_all,
        ..
    } = &cp_tu.defs[0]
    {
        assert!(*sacrifice_all);
        assert!(matches!(**count, Amount::Const { n: 1 }));
    } else {
        panic!("expected Sacrifice all");
    }

    // 6. Tectonic Split: "Sacrifice half the lands you control, rounded up"
    let cp_ts = parse_cost_list(
        "Sacrifice half the lands you control, rounded up",
        0,
        &mut make_ctx("Split", &ch, "", &mut interner),
    );
    assert!(cp_ts.unparsed.is_empty());
    if let CostDef::Sacrifice {
        count,
        half_rounded_up,
        ..
    } = &cp_ts.defs[0]
    {
        assert!(*half_rounded_up);
        assert!(matches!(**count, Amount::Const { n: 1 }));
    } else {
        panic!("expected half_rounded_up sacrifice");
    }

    // 7. Ion Storm: "Remove a +1/+1 counter or a charge counter from a permanent you control"
    let cp_ion = parse_cost_list(
        "Remove a +1/+1 counter or a charge counter from a permanent you control",
        0,
        &mut make_ctx("Ion", &ch, "", &mut interner),
    );
    assert!(cp_ion.unparsed.is_empty());
    assert!(matches!(&cp_ion.defs[0], CostDef::Choice { options, .. } if options.len() == 2));

    // 8. Rift Elemental: "Remove a time counter from a permanent you control or suspended card you own"
    let cp_rift = parse_cost_list(
        "Remove a time counter from a permanent you control or suspended card you own",
        0,
        &mut make_ctx("Rift", &ch, "", &mut interner),
    );
    assert!(cp_rift.unparsed.is_empty());
    if let CostDef::RemoveCounters {
        from: CostObjectTarget::Filter { filter },
        or_suspended_you_own,
        ..
    } = &cp_rift.defs[0]
    {
        assert!(*or_suspended_you_own);
        assert_eq!(filter.controller, None);
    } else {
        panic!("expected RemoveCounters");
    }

    // 9. Mossbridge Troll: "Tap any number of untapped creatures you control other than ~ with total power 10 or greater"
    let cp_mbt = parse_cost_list(
        "Tap any number of untapped creatures you control other than ~ with total power 10 or greater",
        0,
        &mut make_ctx("Troll", &ch, "", &mut interner),
    );
    assert!(cp_mbt.unparsed.is_empty());
    assert!(matches!(
        &cp_mbt.defs[0],
        CostDef::TapSubsetSum {
            exclude_self: true,
            ..
        }
    ));

    // 10. War Room: plural commanders'
    let cp_wr = parse_cost_list(
        "Pay life equal to the number of colors in your commanders' color identity",
        0,
        &mut make_ctx("War Room", &ch, "", &mut interner),
    );
    assert!(cp_wr.unparsed.is_empty());
    assert!(matches!(&cp_wr.defs[0], CostDef::PayLife { .. }));

    // 11. Key to the Side-Door: "Discard a card with the same name as a permanent you control"
    let cp_key = parse_cost_list(
        "Discard a card with the same name as a permanent you control",
        0,
        &mut make_ctx("Key", &ch, "", &mut interner),
    );
    assert!(cp_key.unparsed.is_empty());
    assert!(matches!(
        &cp_key.defs[0],
        CostDef::Discard {
            same_name_as_controlled: Some(_),
            ..
        }
    ));

    // 12. Betrothed of Fire: "Sacrifice enchanted creature" -> types_any empty, attached_creature=true
    let cp_bf = parse_cost_list(
        "Sacrifice enchanted creature",
        0,
        &mut make_ctx("Betrothed", &ch, "", &mut interner),
    );
    assert!(cp_bf.unparsed.is_empty());
    if let CostDef::Sacrifice {
        target: CostObjectTarget::Filter { filter },
        attached_creature,
        ..
    } = &cp_bf.defs[0]
    {
        assert!(*attached_creature);
        assert!(filter.types_any.is_empty());
    } else {
        panic!("expected Betrothed of Fire sacrifice");
    }

    // 13. Umbral Collar Zealot: "Sacrifice another creature or artifact" -> OTHER + CREATURE|ARTIFACT
    let cp_ucz = parse_cost_list(
        "Sacrifice another creature or artifact",
        0,
        &mut make_ctx("Zealot", &ch, "", &mut interner),
    );
    assert!(cp_ucz.unparsed.is_empty());
    if let CostDef::Sacrifice {
        target: CostObjectTarget::Filter { filter },
        ..
    } = &cp_ucz.defs[0]
    {
        assert!(filter.flags.contains(FilterFlags::OTHER));
        assert_eq!(
            filter.types_any,
            mtg_ir::CardTypeMask::CREATURE | mtg_ir::CardTypeMask::ARTIFACT
        );
    } else {
        panic!("expected Umbral Collar Zealot sacrifice");
    }
}

#[test]
fn test_m2_r2_cost_modifiers_and_additional_costs() {
    // Gorex / Torgaar / Defiler / Morophon / Agatha / Zirda / Deadly Rollick / Puresteel / Aven Interrupter / Oakhame / Scarlet Witch / Traxos
    let (gorex, _) = compile_single_for_test(
        "Gorex, the Tombshell",
        "Legendary Creature — Zombie Turtle",
        "As an additional cost to cast this spell, you may exile any number of creature cards from your graveyard. This spell costs {2} less to cast for each card exiled this way.",
    );
    assert!(matches!(
        &gorex.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier { affects: mtg_ir::SpellOrAbilityFilter::SelfObj, delta, .. }
            if !delta.additional_costs.is_empty() && delta.per.is_some()
        )
    ));

    let (torgaar, _) = compile_single_for_test(
        "Torgaar, Famine Incarnate",
        "Legendary Creature — Avatar",
        "As an additional cost to cast this spell, you may sacrifice any number of creatures. This spell costs {2} less to cast for each creature sacrificed this way.",
    );
    assert!(matches!(
        &torgaar.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier { affects: mtg_ir::SpellOrAbilityFilter::SelfObj, .. }
        )
    ));

    let (defiler, _) = compile_single_for_test(
        "Defiler of Vigor",
        "Creature — Phyrexian Wurm",
        "As an additional cost to cast green permanent spells, you may pay 2 life. Those spells cost {G} less to cast if you paid life this way. This effect reduces only the amount of green mana you pay.",
    );
    assert!(matches!(
        &defiler.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::DefilerCostReduction { color: ManaColor::G, life_amount: 2, .. }
        )
    ));

    let (morophon, _) = compile_single_for_test(
        "Morophon, the Boundless",
        "Legendary Creature — Shapeshifter",
        "Spells of the chosen type you cast cost {W}{U}{B}{R}{G} less to cast. This effect reduces only the amount of colored mana you pay.",
    );
    assert!(matches!(
        &morophon.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier { delta, .. } if delta.colored_only && delta.colored.len() == 5
        )
    ));

    let (agatha, _) = compile_single_for_test(
        "Agatha of the Vile Cauldron",
        "Legendary Creature — Human Warlock",
        "Activated abilities of creatures you control cost {X} less to activate, where X is Agatha of the Vile Cauldron's power. This effect can't reduce the mana in that cost to less than one mana.",
    );
    assert!(matches!(
        &agatha.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier { delta, .. } if delta.min_total_mana == Some(1)
        )
    ));

    let (zirda, _) = compile_single_for_test(
        "Zirda, the Dawnwaker",
        "Legendary Creature — Elemental Fox",
        "Abilities you activate that aren't mana abilities cost {2} less to activate. This effect can't reduce the mana in that cost to less than one mana.",
    );
    assert!(matches!(
        &zirda.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier {
                affects: mtg_ir::SpellOrAbilityFilter::Ability { non_mana_only: false, .. },
                delta,
                ..
            } if delta.min_total_mana == Some(1) && delta.generic == 2
        )
    ));

    let (rollick, _) = compile_single_for_test(
        "Deadly Rollick",
        "Instant",
        "If you control a commander, you may cast this spell without paying its mana cost.",
    );
    assert!(matches!(
        &rollick.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier {
                affects: mtg_ir::SpellOrAbilityFilter::SelfObj,
                delta,
                ..
            } if delta.mode == CostDeltaMode::SetZero
        )
    ));

    let (paladin, _) = compile_single_for_test(
        "Puresteel Paladin",
        "Creature — Human Knight",
        "Metalcraft — Equipment you control have equip {0} as long as you control three or more artifacts.",
    );
    assert!(matches!(
        &paladin.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier {
                affects: mtg_ir::SpellOrAbilityFilter::Ability { keyword: Some(_), .. },
                delta,
                ..
            } if delta.mode == CostDeltaMode::SetZero
        ) && sa.static_def.condition.is_some()
    ));

    let (aven, _) = compile_single_for_test(
        "Aven Interrupter",
        "Creature — Bird Rogue",
        "Spells your opponents cast from graveyards or from exile cost {2} more to cast.",
    );
    assert!(matches!(
        &aven.faces[0].abilities[0],
        AbilityDef::Static(sa) if matches!(
            &sa.static_def.body,
            StaticBody::CostModifier {
                who,
                delta,
                ..
            } if who.relation == PlayerRelation::Opponent && delta.mode == CostDeltaMode::Increase
        )
    ));

    let (oakhame, _) = compile_single_for_test(
        "Oakhame Adversary",
        "Creature — Elf Warrior",
        "This spell costs {2} less to cast if an opponent controls a green permanent.",
    );
    assert!(matches!(
        &oakhame.faces[0].abilities[0],
        AbilityDef::Static(sa) if sa.static_def.condition.is_some()
    ));

    let (witch, _) = compile_single_for_test(
        "The Scarlet Witch",
        "Legendary Creature — Human Warlock",
        "Instant and sorcery spells you cast with mana value 4 or greater cost {X} less to cast, where X is The Scarlet Witch's power.",
    );
    if let AbilityDef::Static(sa) = &witch.faces[0].abilities[0]
        && let StaticBody::CostModifier {
            affects: mtg_ir::SpellOrAbilityFilter::Spell { filter, .. },
            ..
        } = &sa.static_def.body
    {
        assert_eq!(
            filter.types_any,
            mtg_ir::CardTypeMask::INSTANT | mtg_ir::CardTypeMask::SORCERY
        );
        assert!(filter.mana_value.is_some());
    } else {
        panic!("expected Scarlet Witch CostModifier");
    }

    let (traxos, _) = compile_single_for_test(
        "Traxos, Academy Guardian",
        "Artifact Creature — Construct",
        "This spell costs {2} less to cast if you've cast a noncreature spell this turn.",
    );
    if let AbilityDef::Static(sa) = &traxos.faces[0].abilities[0]
        && let Some(cond) = &sa.static_def.condition
        && let Condition::SpellsCastThisTurnAtLeast {
            filter: Some(f), ..
        } = cond.as_ref()
    {
        assert!(f.zones.is_empty());
    } else {
        panic!("expected Traxos condition with empty zones");
    }
}

#[test]
fn test_m2_r2_cards_keywords_striations_and_rollback() {
    // 1. Reversible cards: dual normal creature faces
    let mut interner = Interner::new();
    let rev = make_card(
        "rev-id",
        "Zndrsplt // Zndrsplt",
        Some("reversible_card"),
        "{4}{U}",
        "Legendary Creature — Homunculus",
        "Flying",
        vec![
            make_face(
                "Zndrsplt, Eye of Wisdom",
                "{4}{U}",
                "Legendary Creature — Homunculus",
                "Flying",
            ),
            make_face(
                "Zndrsplt, Eye of Wisdom",
                "{4}{U}",
                "Legendary Creature — Homunculus",
                "Flying",
            ),
        ],
    );
    let rev_ir = compile_card(&rev, &mut interner).unwrap();
    assert_eq!(rev_ir.layout, CardLayout::ReversibleCard);
    assert_eq!(rev_ir.faces.len(), 1);

    // 2. Leveler striations
    let (lvl, _) = compile_single_for_test(
        "Student of Warfare",
        "Creature — Human Knight",
        "Level up {W}\nLEVEL 2-6\n3/3\nFirst strike\nLEVEL 7+\n4/4\nDouble strike",
    );
    assert!(
        lvl.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Static(_)))
    );

    // 3. Station striations: printed PT cleared on non-creature Spacecraft, Level 4+7+ synthesized
    let (stn, _) = compile_single_for_test(
        "Dawnhaven Station",
        "Artifact — Spacecraft",
        "Station\n4+ | Flying\n4/4",
    );
    assert_eq!(stn.faces[0].characteristics.pt, None);
    assert!(!stn.faces[0].abilities.is_empty());

    // 4. Forecast {0} elision + RevealFromHand { SelfObj }
    let (fc, _) = compile_single_for_test(
        "Pride of the Clouds",
        "Creature — Elemental Cat",
        "Forecast — {0}, Reveal Pride of the Clouds from your hand: Create a 1/1 white and blue Bird creature token with flying.",
    );
    let fc_cost = match &fc.faces[0].abilities[1] {
        AbilityDef::Activated(a) => &a.cost,
        AbilityDef::Unsupported(u) => {
            assert_eq!(u.cost_complete, Some(true));
            &u.cost
        }
        other => panic!("expected Forecast Activated or Unsupported ability, got {other:?}"),
    };
    assert!(matches!(
        &fc_cost[0],
        CostDef::RevealFromHand {
            target: Some(CostObjectTarget::SelfObj),
            ..
        }
    ));

    // 5. Saga chapter header with flavor word & optional trailing period
    let (saga, _) = compile_single_for_test(
        "The Elder Dragon War",
        "Enchantment — Saga",
        "I — Read the Runes. — Draw two cards.\nII, III — Create a Dragon token.",
    );
    assert_eq!(saga.faces[0].abilities.len(), 2);

    // 6. All KEYWORD_SPECS reject malformed cost-colon text cleanly without panics or intern leaks
    for spec in KEYWORD_SPECS {
        let mut i_loc = Interner::new();
        let text = format!("{} malformed_xyzzy: Draw a card.", spec.name);
        let raw = make_card(
            "Spec Test",
            "Spec Test",
            Some("normal"),
            "{1}",
            "Creature — Human",
            &text,
            vec![],
        );
        let card = compile_card(&raw, &mut i_loc).unwrap();
        assert!(
            !card.faces[0]
                .abilities
                .iter()
                .any(|a| matches!(a, AbilityDef::Keyword(_))),
            "malformed keyword should not parse as KeywordAbility for spec {}",
            spec.name
        );
    }

    // 7. Interner zero-leak on failed cost / filter / condition parses
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);
    let base_cp_dbg = format!("{:?}", interner.checkpoint());
    let mut ctx = make_ctx(
        "Test",
        &ch,
        "an opponent controls invalid_foo_bar_baz",
        &mut interner,
    );
    assert!(
        parse_condition(
            &tokenize("an opponent controls invalid_foo_bar_baz", 0),
            &mut ctx,
            &mut NullScope,
        )
        .is_none()
    );
    assert_eq!(format!("{:?}", interner.checkpoint()), base_cp_dbg);

    // 8. Delirium / OpponentComparison / Tectonic Edge / ObjectState
    let mut ctx_d = make_ctx(
        "Test",
        &ch,
        "there are four or more card types among cards in your graveyard",
        &mut interner,
    );
    let (c_del, _) = parse_condition(
        &tokenize(
            "there are four or more card types among cards in your graveyard",
            0,
        ),
        &mut ctx_d,
        &mut NullScope,
    )
    .unwrap();
    assert!(matches!(c_del, Condition::Compare { .. }));

    let mut ctx_te = make_ctx(
        "Test",
        &ch,
        "an opponent controls four or more lands",
        &mut interner,
    );
    let (c_te, _) = parse_condition(
        &tokenize("an opponent controls four or more lands", 0),
        &mut ctx_te,
        &mut NullScope,
    )
    .unwrap();
    assert!(matches!(
        c_te,
        Condition::Compare {
            op: CompareOp::Ge,
            ..
        }
    ));
}

#[test]
fn test_m2_r3_filter_value_rollback_and_separator_regressions() {
    let mut interner = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);

    // 1. "nonland permanent except Dragons" (positive + negative zero-leak)
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("nonland permanent except Dragons", 0);
        let (f, used) =
            parse_object_filter(&toks, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used, toks.len());
        assert_eq!(f.types_none, mtg_ir::CardTypeMask::LAND);
        assert_eq!(f.subtypes_none.len(), 1);
        assert_eq!(i.tables().subtypes, vec!["Dragon"]);

        let mut i_neg = Interner::new();
        let mut ctx_neg = make_ctx("Test", &ch, "", &mut i_neg);
        let toks_neg = tokenize("nonland permanent except foozzle", 0);
        let (f_neg, used_neg) =
            parse_object_filter(&toks_neg, &mut ctx_neg, FilterOpts::default(), &NullScope)
                .unwrap();
        assert_eq!(used_neg, 2); // "nonland permanent"
        assert!(f_neg.subtypes_none.is_empty());
        assert!(i_neg.tables().subtypes.is_empty());
    }

    // 2. "spell that targets ~" & "Equipment attached to ~" (positive + negative)
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let opts = FilterOpts {
            allow_spell_noun: true,
            ..Default::default()
        };
        let toks = tokenize("spell that targets ~", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, opts, &NullScope).unwrap();
        assert_eq!(used, toks.len());
        assert!(
            f.extra
                .iter()
                .any(|p| matches!(p, ObjectPredicate::TargetsSelf))
        );

        let toks_neg = tokenize("spell that targets foozzle", 0);
        let (f_neg, used_neg) = parse_object_filter(&toks_neg, &mut ctx, opts, &NullScope).unwrap();
        assert_eq!(used_neg, 1);
        assert!(f_neg.extra.is_empty());
        assert!(ctx.interner.tables().subtypes.is_empty());

        let toks_eq = tokenize("Equipment attached to ~", 0);
        let (f_eq, used_eq) =
            parse_object_filter(&toks_eq, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used_eq, toks_eq.len());
        assert!(
            f_eq.extra
                .iter()
                .any(|p| matches!(p, ObjectPredicate::AttachedToRef { .. }))
        );

        let toks_eq_neg = tokenize("Equipment attached to foozzle", 0);
        let (f_eq_neg, used_eq_neg) =
            parse_object_filter(&toks_eq_neg, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used_eq_neg, 1);
        assert!(f_eq_neg.extra.is_empty());
        assert_eq!(i.tables().subtypes, vec!["Equipment"]);
    }

    // 3. "white, blue, or black creature" & "with mana value 1 or greater" & "with power 4 or greater"
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks_col = tokenize("white, blue, or black creature", 0);
        let (f_col, used_col) =
            parse_object_filter(&toks_col, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used_col, toks_col.len());
        assert_eq!(f_col.colors_any, ColorMask::W | ColorMask::U | ColorMask::B);
        assert_eq!(f_col.types_any, mtg_ir::CardTypeMask::CREATURE);
        assert!(f_col.extra.is_empty());

        let toks_mv = tokenize("permanent with mana value 1 or greater", 0);
        let (f_mv, used_mv) =
            parse_object_filter(&toks_mv, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used_mv, toks_mv.len());
        assert!(f_mv.mana_value.is_some());

        let toks_pw = tokenize("creature with power 4 or greater", 0);
        let (f_pw, used_pw) =
            parse_object_filter(&toks_pw, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(used_pw, toks_pw.len());
        assert!(f_pw.power.is_some());
    }

    // 4. Finding 5: trailing separator try_parse in `except [for]` and `parse_keyword_predicate`
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let t_exc = tokenize("nonland permanent except Dragons, draw a card", 0);
        let (f_exc, u_exc) =
            parse_object_filter(&t_exc, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(u_exc, 4); // stops before comma
        assert_eq!(f_exc.subtypes_none.len(), 1);

        let t_slinn = tokenize(
            "creatures except for Merfolk, Krakens, Leviathans, Octopuses, and Serpents",
            0,
        );
        let (f_slinn, u_slinn) = parse_object_filter(
            &t_slinn,
            &mut ctx,
            FilterOpts {
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            &NullScope,
        )
        .unwrap();
        assert_eq!(u_slinn, t_slinn.len());
        assert_eq!(f_slinn.subtypes_none.len(), 5);

        let t_kw1 = tokenize("creature with flying, or sacrifice it", 0);
        let (f_kw1, u_kw1) =
            parse_object_filter(&t_kw1, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(u_kw1, 3); // "creature with flying" (comma untouched)
        assert!(
            matches!(&f_kw1.extra[0], ObjectPredicate::HasKeywords { keywords } if keywords.len() == 1)
        );

        let t_kw2 = tokenize("creature with flying and draw a card", 0);
        let (_f_kw2, u_kw2) =
            parse_object_filter(&t_kw2, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(u_kw2, 3); // "and" untouched

        let t_kw3 = tokenize("creature with flying, first strike, and trample", 0);
        let (f_kw3, u_kw3) =
            parse_object_filter(&t_kw3, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(u_kw3, t_kw3.len());
        assert!(
            matches!(&f_kw3.extra[0], ObjectPredicate::HasKeywords { keywords } if keywords.len() == 3)
        );

        let t_kw4 = tokenize("creature without flying or reach", 0);
        let (f_kw4, u_kw4) =
            parse_object_filter(&t_kw4, &mut ctx, FilterOpts::default(), &NullScope).unwrap();
        assert_eq!(u_kw4, t_kw4.len());
        assert!(
            matches!(&f_kw4.extra[0], ObjectPredicate::LacksKeywords { keywords } if keywords.len() == 2)
        );

        // Challenge 2 regression: `with` must NOT consume `or` / `, or` / `and/or` / `, and/or`,
        // while `without` MUST support `or`, `, or`, `and/or`, `, and/or`, `,`, `and`, `, and`.
        {
            let mut i_kw = Interner::new();
            let mut ctx_kw = make_ctx("Test", &ch, "", &mut i_kw);

            // 1. Bare "or" after `with`: stops after "flying", zero interner leak for "haste"
            let t_with_or = tokenize("creatures with flying or haste", 0);
            let (f_with_or, u_with_or) =
                parse_object_filter(&t_with_or, &mut ctx_kw, FilterOpts::default(), &NullScope)
                    .unwrap();
            assert_eq!(u_with_or, 3);
            assert!(matches!(
                &f_with_or.extra[0],
                ObjectPredicate::HasKeywords { keywords } if keywords.len() == 1
            ));
            assert_eq!(
                ctx_kw.interner.tables().keywords,
                vec!["flying".to_string()]
            );

            // 2. ", or" / "and/or" / ", and/or" after `with`: must also stop and NOT leak
            for phrase in [
                "creatures with flying, or haste",
                "creatures with flying and/or haste",
                "creatures with flying, deathtouch, and/or lifelink",
            ] {
                let mut i_sub = Interner::new();
                let mut ctx_sub = make_ctx("Test", &ch, "", &mut i_sub);
                let toks = tokenize(phrase, 0);
                assert!(
                    parse_object_filter_exact(&toks, &mut ctx_sub, FilterOpts::default()).is_none(),
                    "expected parse_object_filter_exact to reject `{phrase}`"
                );
                assert!(
                    ctx_sub.interner.tables().keywords.is_empty(),
                    "interner leaked on rejected `{phrase}`: {:?}",
                    ctx_sub.interner.tables().keywords
                );
            }

            // 3. Valid `with` lists: "and", ", ", ", and"
            for (phrase, expected_kws) in [
                ("creatures with flying and haste", vec!["flying", "haste"]),
                (
                    "creatures with flying, first strike, and trample",
                    vec!["flying", "first strike", "trample"],
                ),
                ("creatures with flying, haste", vec!["flying", "haste"]),
            ] {
                let mut i_sub = Interner::new();
                let mut ctx_sub = make_ctx("Test", &ch, "", &mut i_sub);
                let toks = tokenize(phrase, 0);
                let f = parse_object_filter_exact(&toks, &mut ctx_sub, FilterOpts::default())
                    .expect("expected valid `with` keyword list to parse");
                assert!(matches!(
                    &f.extra[0],
                    ObjectPredicate::HasKeywords { keywords } if keywords.len() == expected_kws.len()
                ));
                let mut got = ctx_sub.interner.tables().keywords.clone();
                let mut exp: Vec<String> = expected_kws.into_iter().map(String::from).collect();
                got.sort();
                exp.sort();
                assert_eq!(got, exp);
            }

            // 4. Valid `without` lists: `or`, `, or`, `and/or`, `, and/or`, `, and`, `and`
            for (phrase, expected_kws) in [
                ("creatures without flying or reach", vec!["flying", "reach"]),
                (
                    "creatures without first strike, double strike, or vigilance",
                    vec!["first strike", "double strike", "vigilance"],
                ),
                (
                    "creatures without flying and/or reach",
                    vec!["flying", "reach"],
                ),
                (
                    "creatures without flying, trample, and/or reach",
                    vec!["flying", "trample", "reach"],
                ),
                (
                    "creatures without flying, trample, and haste",
                    vec!["flying", "trample", "haste"],
                ),
            ] {
                let mut i_sub = Interner::new();
                let mut ctx_sub = make_ctx("Test", &ch, "", &mut i_sub);
                let toks = tokenize(phrase, 0);
                let f = parse_object_filter_exact(&toks, &mut ctx_sub, FilterOpts::default())
                    .unwrap_or_else(|| panic!("expected valid `without` list `{phrase}` to parse"));
                assert!(matches!(
                    &f.extra[0],
                    ObjectPredicate::LacksKeywords { keywords } if keywords.len() == expected_kws.len()
                ));
                let mut got = ctx_sub.interner.tables().keywords.clone();
                let mut exp: Vec<String> = expected_kws.into_iter().map(String::from).collect();
                got.sort();
                exp.sort();
                assert_eq!(got, exp);
            }

            // 5. Trailing non-keyword clause after `without ..., or ...` rolls back cleanly
            let mut i_tail = Interner::new();
            let mut ctx_tail = make_ctx("Test", &ch, "", &mut i_tail);
            let t_tail = tokenize("creatures without flying, or draw a card", 0);
            let (f_tail, u_tail) =
                parse_object_filter(&t_tail, &mut ctx_tail, FilterOpts::default(), &NullScope)
                    .unwrap();
            assert_eq!(u_tail, 3);
            assert!(matches!(
                &f_tail.extra[0],
                ObjectPredicate::LacksKeywords { keywords } if keywords.len() == 1
            ));
            assert_eq!(
                ctx_tail.interner.tables().keywords,
                vec!["flying".to_string()]
            );
        }
    }

    // 5. `value.rs`: "half X, then draw a card" / "half X, rounded up" / "instead if" zero-leak / ObjectState / OpponentComparison
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let t_half1 = tokenize("half X, then draw a card", 0);
        let (a1, u1) = parse_amount(&t_half1, &mut ctx, &mut NullScope).unwrap();
        assert_eq!(u1, 2); // comma untouched
        assert!(matches!(
            a1,
            Amount::Half {
                round: RoundMode::Down,
                ..
            }
        ));

        let t_half2 = tokenize("half X, rounded up", 0);
        let (a2, u2) = parse_amount(&t_half2, &mut ctx, &mut NullScope).unwrap();
        assert_eq!(u2, t_half2.len());
        assert!(matches!(
            a2,
            Amount::Half {
                round: RoundMode::Up,
                ..
            }
        ));

        // Finding 4: "instead if" must NOT leak "Goblin"
        let t_inst = tokenize("2 instead if you control a Goblin", 0);
        let (a_inst, u_inst) = parse_amount(&t_inst, &mut ctx, &mut NullScope).unwrap();
        assert_eq!(u_inst, 1);
        assert!(matches!(a_inst, Amount::Const { n: 2 }));
        assert!(ctx.interner.tables().subtypes.is_empty());

        for cond_s in [
            "~ is enchanted",
            "~ is equipped",
            "an opponent controls more creatures than you",
        ] {
            let tc = tokenize(cond_s, 0);
            let (cond, uc) = parse_condition(&tc, &mut ctx, &mut NullScope).unwrap();
            assert_eq!(uc, tc.len(), "failed full consume for {cond_s}");
            let _ = cond;
        }
    }
}

#[test]
fn test_m2_r3_cost_card_and_interner_regressions() {
    let mut interner = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);

    // 1. FirstActivationZeroCost positive & negative (zero silent drops)
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Forge Anew", &ch, "", &mut i);
        let fa = parse_cost_modifier_sentence(
            "You may pay {0} rather than pay the equip cost of the first equip ability you activate during each of your turns.",
            &mut ctx,
        )
        .unwrap();
        assert!(matches!(
            fa.body,
            StaticBody::FirstActivationZeroCost {
                ability_keyword: FirstActivationZeroKind::Equip,
                during_your_turn_only: true
            }
        ));

        let bru = parse_cost_modifier_sentence(
            "You may pay {0} rather than pay the equip cost of the first equip ability you activate each turn.",
            &mut ctx,
        )
        .unwrap();
        assert!(matches!(
            bru.body,
            StaticBody::FirstActivationZeroCost {
                ability_keyword: FirstActivationZeroKind::Equip,
                during_your_turn_only: false
            }
        ));

        let cyc = parse_cost_modifier_sentence(
            "You may pay {0} rather than pay the cycling cost of the first card you cycle each turn.",
            &mut ctx,
        )
        .unwrap();
        assert!(matches!(
            cyc.body,
            StaticBody::FirstActivationZeroCost {
                ability_keyword: FirstActivationZeroKind::Cycling,
                during_your_turn_only: false
            }
        ));

        let pwr = parse_cost_modifier_sentence(
            "You may pay {0} rather than pay the power-up cost of the first power-up ability you activate during each of your turns.",
            &mut ctx,
        )
        .unwrap();
        assert!(matches!(
            pwr.body,
            StaticBody::FirstActivationZeroCost {
                ability_keyword: FirstActivationZeroKind::PowerUp,
                during_your_turn_only: true
            }
        ));

        for bad in [
            "You may pay {0} rather than pay the equip cost of your opponent's creatures.",
            "You may pay {0} rather than pay the cycling cost of the second card you cycle each turn.",
            "You may pay {0} rather than pay the equip cost of the first equip ability you activate during each of your.",
            "You may pay {0} rather than pay the equip cost of the first equip ability you activate each turn if you control an artifact.",
            "You may pay {0} rather than pay the equip cost of the first card you cycle each turn.",
        ] {
            assert!(
                parse_cost_modifier_sentence(bad, &mut ctx).is_none(),
                "must reject malformed FirstActivationZeroCost: {bad}"
            );
        }
    }

    // 2. try_parse_put_costs ("Put ~ from your graveyard on the bottom of your library")
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        for s in [
            "Put ~ from your graveyard on the bottom of your library",
            "Put this creature from your graveyard on the bottom of your library",
        ] {
            let cp = parse_cost_list(s, 0, &mut ctx);
            assert!(cp.unparsed.is_empty(), "failed: {s}");
            assert!(matches!(
                &cp.defs[0],
                CostDef::PutOnBottomOfLibrary {
                    from: Zone::Graveyard,
                    target: Some(CostObjectTarget::SelfObj),
                    ..
                }
            ));
        }

        let cp_top = parse_cost_list(
            "Put an artifact card from your hand on top of your library",
            0,
            &mut ctx,
        );
        assert!(cp_top.unparsed.is_empty());
        assert!(matches!(
            &cp_top.defs[0],
            CostDef::PutFromHandOnTopOfLibrary { .. }
        ));

        let cp_card_bot = parse_cost_list(
            "Put a card from your graveyard on the bottom of your library",
            0,
            &mut ctx,
        );
        assert!(cp_card_bot.unparsed.is_empty());
        assert!(matches!(
            &cp_card_bot.defs[0],
            CostDef::PutOnBottomOfLibrary {
                from: Zone::Graveyard,
                target: None,
                ..
            }
        ));

        let cp_bad = parse_cost_list("Put this creature under an invalid foozzle", 0, &mut ctx);
        assert!(cp_bad.defs.is_empty());
        assert!(!cp_bad.unparsed.is_empty());
        assert!(i.tables().subtypes.is_empty());
    }

    // 3. Finding 4: cost.rs partial-parse Interner rollbacks & early `?` fixes
    {
        let cases = [
            "Discard a Goblin card and foozzle",
            "Discard a card with the same name as a Goblin you don't control",
        ];
        for s in cases {
            let mut i = Interner::new();
            let mut ctx = make_ctx("Test", &ch, s, &mut i);
            let cp = parse_cost_list(s, 0, &mut ctx);
            assert!(cp.defs.is_empty());
            assert!(
                i.tables().subtypes.is_empty(),
                "leaked subtype on cost: {s}"
            );
        }

        let mod_cases = [
            "Nonpermanent spells you control with Goblin cost {1} less to cast.",
            "Instant and sorcery spells you control with Goblin cost {1} less to cast.",
            "Goblin spells you control cost {1} less to cast.",
            "As long as you control a Goblin, activated abilities of Elves you control cost {1} less to activate.",
            "As long as foozzle, activated abilities of Elves cost an additional \"{T}\" to activate.",
            "Activated abilities of Elves that target you or a Goblin cost {1} more to activate.",
            "Each Goblin spell you control costs {1} less to cast.",
            "Goblin spells the enchanted player casts with foozzle cost {1} less to cast.",
            "As an additional cost to cast ~, sacrifice a Goblin. ~ gets +1/+1.",
            "This spell costs {2} less to cast if there are four or more nonexistent_foozzle cards in your graveyard.",
        ];
        for s in mod_cases {
            let mut i = Interner::new();
            let mut ctx = make_ctx("Test", &ch, s, &mut i);
            assert!(
                parse_cost_modifier_sentence(s, &mut ctx).is_none(),
                "expected None for {s}"
            );
            assert!(
                i.tables().subtypes.is_empty(),
                "leaked subtype on modifier: {s}"
            );
        }
    }

    // 4. Finding 6: parse_additional_cost_sentence offset > 0 span & sub_ctx alignment
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let sent = "  As an additional cost to cast ~, foozzle.";
        let cp = parse_additional_cost_sentence(sent, 50, false, &mut ctx).unwrap();
        assert_eq!(cp.unparsed, vec![Span { start: 85, end: 92 }]);
        assert!(i.tables().subtypes.is_empty());
    }

    // 5. UTF-8 multi-line named <Card> with em-dash & King Darien / Playtest / During zero-leak
    {
        let (card_utf8, i_u) = compile_single_for_test(
            "Test UTF8",
            "Creature — Wizard",
            "Ward—Pay 2 life.\nCreature spells named Alpha Beta cost {1} less to cast.",
        );
        assert!(matches!(
            &card_utf8.faces[0].abilities[1],
            AbilityDef::Static(sa) if matches!(
                &sa.static_def.body,
                StaticBody::CostModifier {
                    affects: mtg_ir::SpellOrAbilityFilter::Spell { filter, .. },
                    ..
                } if filter.extra.contains(&ObjectPredicate::Named { name: "Alpha Beta".into() })
            )
        ));
        assert_eq!(i_u.tables().subtypes, vec!["Wizard"]);

        let (kd, i_kd) = compile_single_for_test(
            "Other Card",
            "Creature — Human",
            "Sacrifice King Darien: Draw a card.",
        );
        assert!(matches!(
            &kd.faces[0].abilities[0],
            AbilityDef::Unsupported(u) if u.cost_complete == Some(false)
        ));
        assert_eq!(i_kd.tables().subtypes, vec!["Human"]);

        let mut i_pd = Interner::new();
        let mut ctx_pd = make_ctx("Test", &ch, "", &mut i_pd);
        assert!(
            parse_cost_modifier_sentence("Playtest spells cost {1} less to cast.", &mut ctx_pd)
                .is_none()
        );
        assert!(
            parse_object_filter(
                &tokenize("During your turn", 0),
                &mut ctx_pd,
                FilterOpts::default(),
                &NullScope
            )
            .is_none()
        );
        assert!(i_pd.tables().subtypes.is_empty());
    }

    // 6. Curly apostrophe `’` in Activate only / Cast only
    {
        let (act_curly, _) = compile_single_for_test(
            "Curly Activator",
            "Creature — Wizard",
            "{T}: Draw a card. Activate only as a sorcery on an opponent’s turn.",
        );
        match &act_curly.faces[0].abilities[0] {
            AbilityDef::Activated(a) => {
                assert_eq!(a.timing, Timing::Sorcery);
                assert!(
                    a.activation_restrictions
                        .contains(&ActivationRestriction::DuringOpponentsTurn)
                );
            }
            AbilityDef::Unsupported(u) => {
                assert_eq!(u.timing, Some(Timing::Sorcery));
                assert!(
                    u.activation_restrictions
                        .contains(&ActivationRestriction::DuringOpponentsTurn)
                );
            }
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        }

        let (act_bad, _) = compile_single_for_test(
            "Curly Bad",
            "Creature — Wizard",
            "{T}: Draw a card. Activate only foozzle on an opponent’s turn.",
        );
        match &act_bad.faces[0].abilities[0] {
            AbilityDef::Activated(a) => {
                assert_eq!(a.timing, Timing::Instant);
                assert!(a.activation_restrictions.is_empty());
            }
            AbilityDef::Unsupported(u) => {
                assert_eq!(u.timing, Some(Timing::Instant));
                assert!(u.activation_restrictions.is_empty());
            }
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        }

        let (cast_curly, _) = compile_single_for_test(
            "Curly Spell",
            "Instant",
            "Cast Curly Spell only during combat on an opponent’s turn.\nGain 3 life.",
        );
        let cast_restrictions = match &cast_curly.faces[0].abilities[0] {
            AbilityDef::Spell(s) => &s.cast_restrictions,
            AbilityDef::Unsupported(u) => &u.cast_restrictions,
            other => panic!("expected Spell or Unsupported ability, got {other:?}"),
        };
        assert!(cast_restrictions.contains(&SpellCastRestrictionKind::DuringCombat));
        assert!(cast_restrictions.contains(&SpellCastRestrictionKind::DuringOpponentsTurn));
    }

    // 7. March of Reckless Joy / Spell hook parity (CostModifier without additional_costs, Unsupported(cast).cost populated)
    {
        let (march, _) = compile_single_for_test(
            "March of Reckless Joy",
            "Instant",
            "As an additional cost to cast March of Reckless Joy, you may exile any number of red cards from your hand. March of Reckless Joy costs {2} less to cast for each card exiled this way.\nExile the top X cards of your library.",
        );
        assert!(matches!(
            &march.faces[0].abilities[0],
            AbilityDef::Static(sa) if matches!(
                &sa.static_def.body,
                StaticBody::CostModifier { affects: mtg_ir::SpellOrAbilityFilter::SelfObj, delta, .. }
                if delta.additional_costs.is_empty()
                    && delta.generic == 2
                    && matches!(delta.per.as_deref(), Some(Amount::CostPaidCount { kind: CostPaidCountKind::Exiled }))
            )
        ));
        let march_cost = match &march.faces[0].abilities[1] {
            AbilityDef::Spell(s) => &s.additional_costs,
            AbilityDef::Unsupported(u) => {
                assert_eq!(u.id.as_deref(), Some("cast"));
                assert_eq!(u.cost_complete, Some(true));
                &u.cost
            }
            other => panic!("expected Spell or Unsupported(cast), got {other:?}"),
        };
        assert!(matches!(
            &march_cost[0],
            CostDef::Optional { cost } if matches!(&cost[0], CostDef::ExileFrom { zone: Zone::Hand, .. })
        ));
    }
}

#[test]
fn test_m2_r3_sacrifice_delirium_and_audit_regressions() {
    let mut interner = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut interner);

    // 1. Conjunctive multi-sacrifice splits into 2/3 items
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Jarad", &ch, "", &mut i);
        let cp_jarad = parse_cost_list("Sacrifice a Swamp and a Forest", 0, &mut ctx);
        assert!(cp_jarad.unparsed.is_empty());
        assert_eq!(cp_jarad.defs.len(), 2);
        assert!(matches!(
            &cp_jarad.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { .. },
                ..
            }
        ));
        assert!(matches!(
            &cp_jarad.defs[1],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { .. },
                ..
            }
        ));

        for two_item in [
            "Sacrifice a creature and a Swamp",
            "Sacrifice a creature and a snow Swamp",
        ] {
            let cp = parse_cost_list(two_item, 0, &mut ctx);
            assert!(cp.unparsed.is_empty(), "failed: {two_item}");
            assert_eq!(cp.defs.len(), 2, "expected 2 items for {two_item}");
        }

        let cp_herald = parse_cost_list(
            "Sacrifice a white creature, a blue creature, and a black creature",
            0,
            &mut ctx,
        );
        assert!(cp_herald.unparsed.is_empty());
        assert_eq!(cp_herald.defs.len(), 3);
    }

    // 2. Single disjunctive & multi-modifier sacrifices remain 1 item
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        for single in [
            "Sacrifice two other artifacts and/or creatures",
            "Sacrifice an artifact or creature",
            "Sacrifice a creature or planeswalker",
            "Sacrifice a creature without flying or reach",
            "Sacrifice a white and blue creature",
            "Sacrifice a red, white, or blue creature",
        ] {
            let cp = parse_cost_list(single, 0, &mut ctx);
            assert!(cp.unparsed.is_empty(), "failed: {single}");
            assert_eq!(cp.defs.len(), 1, "expected 1 item for {single}");
        }

        let cp_named = parse_cost_list("Sacrifice a creature named Alpha and Omega", 0, &mut ctx);
        assert!(cp_named.unparsed.is_empty());
        assert_eq!(cp_named.defs.len(), 1);
        assert!(matches!(
            &cp_named.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.extra.contains(&ObjectPredicate::Named { name: "Alpha and Omega".into() })
        ));

        // Tekuthal cost_filter_opts() still works for RemoveCounters
        let cp_tek = parse_cost_list(
            "Remove three counters from among other artifacts, creatures, and planeswalkers you control",
            0,
            &mut ctx,
        );
        assert!(cp_tek.unparsed.is_empty());
        assert_eq!(cp_tek.defs.len(), 1);
    }

    // 3. Delirium fallback in CostModifier (Dusk Feaster / Scour the Laboratory)
    {
        let (dusk, _) = compile_single_for_test(
            "Dusk Feaster",
            "Creature — Vampire",
            "Delirium — This spell costs {2} less to cast if there are four or more card types among cards in your graveyard.",
        );
        assert!(matches!(
            &dusk.faces[0].abilities[0],
            AbilityDef::Static(sa) if matches!(
                sa.static_def.condition.as_deref(),
                Some(Condition::Compare {
                    left,
                    op: CompareOp::Ge,
                    right,
                }) if matches!(**left, Amount::DistinctCardTypesInZone { zone: Zone::Graveyard, .. })
                    && matches!(**right, Amount::Const { n: 4 })
            )
        ));
    }

    // 4. Quoted sentence endings in extract_activation_restrictions (Strago and Relm)
    {
        let (strago, _) = compile_single_for_test(
            "Strago and Relm",
            "Legendary Creature — Human",
            "{T}: Until end of turn, ~ gains \"Whenever a creature dies, sacrifice this creature.\" Activate only as a sorcery.",
        );
        match &strago.faces[0].abilities[0] {
            AbilityDef::Activated(a) => assert_eq!(a.timing, Timing::Sorcery),
            AbilityDef::Unsupported(u) => assert_eq!(u.timing, Some(Timing::Sorcery)),
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        }
    }

    // 5. "has a +1/+1 counter on it" & "there is a <card> in your graveyard" -> CompareOp::Ge 1
    {
        let (skarrgan, _) = compile_single_for_test(
            "Skarrgan Hellkite",
            "Creature — Dragon",
            "{3}{R}: Skarrgan Hellkite deals 2 damage to any target. Activate only if Skarrgan Hellkite has a +1/+1 counter on it.",
        );
        let sk_cond = match &skarrgan.faces[0].abilities[0] {
            AbilityDef::Activated(a) => a.activation_condition.as_deref(),
            AbilityDef::Unsupported(u) => u.activation_condition.as_deref(),
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        };
        assert!(matches!(
            sk_cond,
            Some(Condition::Compare {
                op: CompareOp::Ge,
                ..
            })
        ));

        let (wall, _) = compile_single_for_test(
            "Wall of Forgotten Pharaohs",
            "Artifact Creature — Wall",
            "{T}: Target player loses 1 life. Activate only if you control a Desert or there is a Desert card in your graveyard.",
        );
        let wall_cond = match &wall.faces[0].abilities[0] {
            AbilityDef::Activated(a) => a.activation_condition.as_deref(),
            AbilityDef::Unsupported(u) => u.activation_condition.as_deref(),
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        };
        assert!(matches!(
            wall_cond,
            Some(Condition::Or { of }) if of.iter().all(|c| matches!(c, Condition::Compare { op: CompareOp::Ge, .. }))
        ));
    }

    // 6. Chronatog Totem, Chittering Skitterling, Defiant Stand restriction conditions
    {
        let (totem, _) = compile_single_for_test(
            "Chronatog Totem",
            "Artifact",
            "{0}: Chronatog Totem gets +3/+3 until end of turn. Activate only once each turn and only if Chronatog Totem is a creature.",
        );
        let (totem_r, totem_c) = match &totem.faces[0].abilities[0] {
            AbilityDef::Activated(a) => (
                &a.activation_restrictions,
                a.activation_condition.as_deref(),
            ),
            AbilityDef::Unsupported(u) => (
                &u.activation_restrictions,
                u.activation_condition.as_deref(),
            ),
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        };
        assert!(totem_r.contains(&ActivationRestriction::OnceEachTurn));
        assert!(matches!(
            totem_c,
            Some(Condition::MatchesFilter {
                target: Some(Ref::SelfObj),
                ..
            })
        ));

        let (chitter, _) = compile_single_for_test(
            "Chittering Skitterling",
            "Creature — Phyrexian Rat",
            "Sacrifice another creature: Draw a card. Activate only once each turn and only if an opponent has three or more poison counters.",
        );
        let (ch_r, ch_c) = match &chitter.faces[0].abilities[0] {
            AbilityDef::Activated(a) => (
                &a.activation_restrictions,
                a.activation_condition.as_deref(),
            ),
            AbilityDef::Unsupported(u) => (
                &u.activation_restrictions,
                u.activation_condition.as_deref(),
            ),
            other => panic!("expected Activated or Unsupported ability, got {other:?}"),
        };
        assert!(ch_r.contains(&ActivationRestriction::OnceEachTurn));
        assert!(matches!(
            ch_c,
            Some(Condition::OpponentPoisonAtLeast { min: 3 })
        ));

        let (defiant, _) = compile_single_for_test(
            "Defiant Stand",
            "Instant",
            "Cast Defiant Stand only during the declare attackers step and only if you've been attacked this step.\nTarget creature gets +1/+3 until end of turn.",
        );
        let (def_r, def_c) = match &defiant.faces[0].abilities[0] {
            AbilityDef::Spell(s) => (&s.cast_restrictions, s.cast_condition.as_deref()),
            AbilityDef::Unsupported(u) => (&u.cast_restrictions, u.cast_condition.as_deref()),
            other => panic!("expected Spell or Unsupported ability, got {other:?}"),
        };
        assert!(def_r.contains(&SpellCastRestrictionKind::AfterAttackers));
        assert!(matches!(def_c, Some(Condition::YouWereAttackedThisCombat)));
    }
}

#[test]
fn test_m2_r4_named_top_level_disjunction_and_comma_names() {
    let mut base_i = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut base_i);

    let disj_opts = FilterOpts {
        default_zone: ZoneMask::BATTLEFIELD,
        allow_plural_and_disjunction: true,
        allow_spell_noun: false,
        strip_trailing_card: false,
        expand_outlaw_subtypes: false,
    };

    // 1. All 4 Challenge 1 paths with "Alpha and Omega" and allow_plural_and_disjunction: true
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let cm = parse_cost_modifier_sentence(
            "Creature spells named Alpha and Omega cost {1} less to cast.",
            &mut ctx,
        )
        .unwrap();
        assert!(matches!(
            &cm.body,
            StaticBody::CostModifier {
                affects: mtg_ir::SpellOrAbilityFilter::Spell { filter, .. },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_disc = parse_cost_list("Discard a card named Alpha and Omega", 0, &mut ctx);
        assert!(cp_disc.unparsed.is_empty());
        assert_eq!(cp_disc.defs.len(), 1);
        assert!(matches!(
            &cp_disc.defs[0],
            CostDef::Discard {
                target: Some(CostObjectTarget::Filter { filter }),
                ..
            } if filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_beh = parse_cost_list("Behold a creature named Alpha and Omega", 0, &mut ctx);
        assert!(cp_beh.unparsed.is_empty());
        assert_eq!(cp_beh.defs.len(), 1);
        assert!(matches!(
            &cp_beh.defs[0],
            CostDef::Behold { filter, .. }
            if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_sac = parse_cost_list(
            "Sacrifice one or more creatures named Alpha and Omega",
            0,
            &mut ctx,
        );
        assert!(cp_sac.unparsed.is_empty());
        assert_eq!(cp_sac.defs.len(), 1);
        assert!(matches!(
            &cp_sac.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // 2. Real Scryfall card-name shapes with "and" and "," (Pia and Kiran Nalaar, Rin and Seri, Inseparable, Rakdos, the Showstopper, Foo, Bar, The Lost and the Damned)
    for (phrase, expected_name) in [
        (
            "creatures named Pia and Kiran Nalaar",
            "Pia and Kiran Nalaar",
        ),
        (
            "a creature named Rin and Seri, Inseparable",
            "Rin and Seri, Inseparable",
        ),
        (
            "a creature named Rakdos, the Showstopper",
            "Rakdos, the Showstopper",
        ),
        ("creatures named Foo, Bar", "Foo, Bar"),
        (
            "a card named The Lost and the Damned",
            "The Lost and the Damned",
        ),
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, phrase, &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len(), "expected full consume for {phrase}");
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named {
                name: expected_name.into()
            }]
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // 3. Disjunctions AFTER or AROUND "named" still split properly
    for (phrase, expect_named) in [
        ("creatures named Foo or artifacts", "Foo"),
        (
            "creatures named Alpha and Omega or artifacts",
            "Alpha and Omega",
        ),
        ("creatures named Foo, or artifacts", "Foo"),
        (
            "creatures named Rin and Seri, Inseparable, or artifacts",
            "Rin and Seri, Inseparable",
        ),
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, phrase, &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len(), "expected full consume for {phrase}");
        assert!(matches!(
            f.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].types_any == mtg_ir::CardTypeMask::CREATURE
                && filters[0].extra == vec![ObjectPredicate::Named { name: expect_named.into() }]
                && filters[1].types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // Disjunction with "named" in second branch + SelfRef (~) before "or" + lowercase post-name conjunction
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("artifacts or creatures named Alpha and Omega", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len());
        assert!(matches!(
            f.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].types_any == mtg_ir::CardTypeMask::ARTIFACT
                && filters[1].types_any == mtg_ir::CardTypeMask::CREATURE
                && filters[1].extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let toks_self = tokenize("creatures named ~ or artifacts", 0);
        let (f_self, used_self) =
            parse_object_filter(&toks_self, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used_self, toks_self.len());
        assert!(matches!(
            f_self.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].extra == vec![ObjectPredicate::SameNameAsSource]
                && filters[1].types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));

        let toks_akron = tokenize(
            "creatures named Akron Legionnaire and artifact creatures",
            0,
        );
        let (f_akron, used_akron) =
            parse_object_filter(&toks_akron, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used_akron, toks_akron.len());
        assert!(matches!(
            f_akron.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].extra == vec![ObjectPredicate::Named { name: "Akron Legionnaire".into() }]
                && filters[1].types_all == (mtg_ir::CardTypeMask::ARTIFACT | mtg_ir::CardTypeMask::CREATURE)
        ));
    }

    // 4. Trailing clause boundary after comma-containing card name stops cleanly
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("a creature named Rakdos, the Showstopper, exile them", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, 7); // stops before ", exile them"
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named {
                name: "Rakdos, the Showstopper".into()
            }]
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // 5. Negative & zero-leak tests
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        for bad in ["creatures named", "creatures named or artifacts"] {
            let toks = tokenize(bad, 0);
            assert!(
                parse_object_filter_exact(&toks, &mut ctx, disj_opts).is_none(),
                "expected exact rejection for {bad}"
            );
            assert!(ctx.interner.tables().subtypes.is_empty());
        }
        assert!(
            parse_cost_modifier_sentence(
                "Creature spells named Alpha and Omega foozzle cost {1} less to cast.",
                &mut ctx,
            )
            .is_none()
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }
}

#[test]
fn test_m2_r4_sacrifice_one_or_more_and_poison_counter() {
    let mut base_i = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut base_i);

    // 1. "one or more ... and ..." multi-sacrifice + "and/or" single sacrifice preservation
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);

        let cp1 = parse_cost_list(
            "Sacrifice one or more artifacts and a creature",
            0,
            &mut ctx,
        );
        assert!(cp1.unparsed.is_empty());
        assert_eq!(cp1.defs.len(), 2);
        assert!(matches!(
            &cp1.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));
        assert!(matches!(
            &cp1.defs[1],
            CostDef::Sacrifice {
                one_or_more: false,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
        ));

        let cp2 = parse_cost_list(
            "Sacrifice a creature and one or more artifacts",
            0,
            &mut ctx,
        );
        assert!(cp2.unparsed.is_empty());
        assert_eq!(cp2.defs.len(), 2);
        assert!(matches!(
            &cp2.defs[0],
            CostDef::Sacrifice {
                one_or_more: false,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
        ));
        assert!(matches!(
            &cp2.defs[1],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));

        let cp_single = parse_cost_list(
            "Sacrifice one or more artifacts and/or enchantments",
            0,
            &mut ctx,
        );
        assert!(cp_single.unparsed.is_empty());
        assert_eq!(cp_single.defs.len(), 1);
        assert!(matches!(
            &cp_single.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { .. },
                ..
            }
        ));

        // Negative: disjunctive "or" not inside "one or more" must not split as conjunction
        let cp_bad = parse_cost_list(
            "Sacrifice a creature with mana value one or less and an artifact",
            0,
            &mut ctx,
        );
        assert!(cp_bad.defs.is_empty() || !cp_bad.unparsed.is_empty());
    }

    // 2. "an opponent has a/one/1 poison counter"
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        for phrase in [
            "an opponent has a poison counter",
            "an opponent has one poison counter",
            "an opponent has 1 poison counter",
            "an opponent has at least one poison counter",
            "an opponent has one or more poison counters",
        ] {
            let toks = tokenize(phrase, 0);
            let (cond, used) = parse_condition(&toks, &mut ctx, &mut NullScope).unwrap();
            assert_eq!(used, toks.len(), "failed full consume for {phrase}");
            assert!(
                matches!(cond, Condition::OpponentPoisonAtLeast { min: 1 }),
                "unexpected cond for {phrase}: {cond:?}"
            );
        }

        // Negative: zero or "fewer than three" poison counters must not match OpponentPoisonAtLeast
        for bad in [
            "an opponent has zero poison counters",
            "an opponent has fewer than three poison counters",
        ] {
            let toks = tokenize(bad, 0);
            assert!(
                parse_condition(&toks, &mut ctx, &mut NullScope).is_none(),
                "expected None for {bad}"
            );
        }
    }
}

#[test]
fn test_m2_r5_collect_name_tokens_comma_and_stopword_regressions() {
    let mut base_i = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut base_i);

    let disj_opts = FilterOpts {
        default_zone: ZoneMask::BATTLEFIELD,
        allow_plural_and_disjunction: true,
        allow_spell_noun: false,
        strip_trailing_card: false,
        expand_outlaw_subtypes: false,
    };
    let no_and_opts = FilterOpts {
        allow_plural_and_disjunction: false,
        ..disj_opts
    };

    // 1. ", at <Cap>" and ", at the <Cap>" (Scryfall: "U.S.S. Discovery, at Black Alert", "Aang, at the Crossroads")
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("creatures named Discovery, at Black Alert", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len());
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named {
                name: "Discovery, at Black Alert".into()
            }]
        );
        assert!(ctx.interner.tables().subtypes.is_empty());

        let mut i_aang = Interner::new();
        let mut ctx_aang = make_ctx("Test", &ch, "", &mut i_aang);
        let toks_aang = tokenize("artifacts named Aang, at the Crossroads", 0);
        let (f_aang, used_aang) =
            parse_object_filter(&toks_aang, &mut ctx_aang, disj_opts, &NullScope).unwrap();
        assert_eq!(used_aang, 7);
        assert_eq!(used_aang, toks_aang.len());
        assert_eq!(
            f_aang.extra,
            vec![ObjectPredicate::Named {
                name: "Aang, at the Crossroads".into()
            }]
        );
        assert!(ctx_aang.interner.tables().subtypes.is_empty());

        // Lowercase follower after ", at the" MUST stop before comma (no dangling ",")
        let mut i_low = Interner::new();
        let mut ctx_low = make_ctx("Test", &ch, "", &mut i_low);
        let toks_low = tokenize("creatures named Foo, at the beginning of combat", 0);
        let (f_low, used_low) =
            parse_object_filter(&toks_low, &mut ctx_low, disj_opts, &NullScope).unwrap();
        assert_eq!(used_low, 3);
        assert_eq!(
            f_low.extra,
            vec![ObjectPredicate::Named { name: "Foo".into() }]
        );
        assert!(ctx_low.interner.tables().subtypes.is_empty());
    }

    // 2. ", in <Cap>" and "in <lowercase>" stop name collection cleanly without dangling comma
    for phrase in [
        "creatures named Foo, in New Capenna",
        "creatures named Foo in your graveyard",
        "creatures named Foo, in your graveyard",
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        if phrase.contains(',') {
            assert_eq!(used, 3, "must stop before comma for `{phrase}`");
        } else {
            assert_eq!(used, toks.len());
            assert_eq!(f.zones, ZoneMask::GRAVEYARD);
        }
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named { name: "Foo".into() }],
            "unexpected predicate for `{phrase}`"
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("cards named Life Burst in each graveyard", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, 4);
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named {
                name: "Life Burst".into()
            }]
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // 3. Capitalized stop words after comma or connector MUST NOT consume `,` / connector
    // 3a. "creatures named Foo, Or Bar":
    //     - parse_object_filter splits on ", Or" into AnyOf([Creature Named("Foo"), Subtype("Bar")])
    //       without any dangling comma on "Foo".
    //     - parse_single_object_filter and "creatures named Foo, Or draw a card" stop cleanly at used == 3.
    {
        let toks_or_bar = tokenize("creatures named Foo, Or Bar", 0);

        let mut i_disj = Interner::new();
        let mut ctx_disj = make_ctx("Test", &ch, "", &mut i_disj);
        let (f_disj, used_disj) =
            parse_object_filter(&toks_or_bar, &mut ctx_disj, disj_opts, &NullScope).unwrap();
        assert_eq!(used_disj, toks_or_bar.len());
        assert!(matches!(
            f_disj.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].types_any == mtg_ir::CardTypeMask::CREATURE
                && filters[0].extra == vec![ObjectPredicate::Named { name: "Foo".into() }]
                && filters[1].subtypes_any.len() == 1
        ));
        assert_eq!(i_disj.tables().subtypes, vec!["Bar".to_string()]);

        let mut i_single = Interner::new();
        let mut ctx_single = make_ctx("Test", &ch, "", &mut i_single);
        let (f_single, used_single) =
            parse_single_object_filter(&toks_or_bar, &mut ctx_single, disj_opts, &NullScope)
                .unwrap();
        assert_eq!(used_single, 3);
        assert_eq!(
            f_single.extra,
            vec![ObjectPredicate::Named { name: "Foo".into() }]
        );
        assert!(i_single.tables().subtypes.is_empty());

        let mut i_draw = Interner::new();
        let mut ctx_draw = make_ctx("Test", &ch, "", &mut i_draw);
        let toks_draw = tokenize("creatures named Foo, Or draw a card", 0);
        let (f_draw, used_draw) =
            parse_object_filter(&toks_draw, &mut ctx_draw, disj_opts, &NullScope).unwrap();
        assert_eq!(used_draw, 3);
        assert_eq!(
            f_draw.extra,
            vec![ObjectPredicate::Named { name: "Foo".into() }]
        );
        assert!(parse_object_filter_exact(&toks_draw, &mut ctx_draw, disj_opts).is_none());
        assert!(i_draw.tables().subtypes.is_empty());
    }

    for phrase in [
        "creatures named Foo, In Play",
        "creatures named Foo, From Exile",
        "creatures named Foo, On Battlefield",
        "creatures named Foo, You Control",
        "creatures named Foo, With Flying",
        "creatures named Foo of Or",
        "creatures named Foo and Or",
        "creatures named Foo and the Or",
        "creatures named Foo of the Or",
        "creatures named Foo, the Or",
        "creatures named Foo, at Or",
        "creatures named Foo, at the Or",
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, 3, "expected clean stop at `Foo` for `{phrase}`");
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named { name: "Foo".into() }],
            "dangling comma/connector leaked into Named for `{phrase}`"
        );
        assert!(
            parse_object_filter_exact(&toks, &mut ctx, disj_opts).is_none(),
            "expected parse_object_filter_exact to reject `{phrase}`"
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    for phrase in [
        "creatures named Foo, and Bar",
        "creatures named Foo and a Goblin",
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, no_and_opts, &NullScope).unwrap();
        assert_eq!(used, 3, "expected clean stop at `Foo` for `{phrase}`");
        assert_eq!(f.extra, vec![ObjectPredicate::Named { name: "Foo".into() }]);
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    // 4. Multi-connector real Scryfall card names & post-name disjunction
    for (phrase, expected_name) in [
        ("creatures named Wolves of the Hunt", "Wolves of the Hunt"),
        (
            "creatures named Approach of the Second Sun",
            "Approach of the Second Sun",
        ),
        (
            "creatures named Death of a Thousand Stings",
            "Death of a Thousand Stings",
        ),
    ] {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize(phrase, 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len(), "expected full consume for `{phrase}`");
        assert_eq!(
            f.extra,
            vec![ObjectPredicate::Named {
                name: expected_name.into()
            }]
        );
        assert!(ctx.interner.tables().subtypes.is_empty());
    }

    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let toks = tokenize("creatures named Discovery, at Black Alert or artifacts", 0);
        let (f, used) = parse_object_filter(&toks, &mut ctx, disj_opts, &NullScope).unwrap();
        assert_eq!(used, toks.len());
        assert!(matches!(
            f.extra.as_slice(),
            [ObjectPredicate::AnyOf { filters }]
            if filters.len() == 2
                && filters[0].extra == vec![ObjectPredicate::Named { name: "Discovery, at Black Alert".into() }]
                && filters[1].types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());
    }
}

#[test]
fn test_m2_r5_challenge_sacrifice_named_another_and_poison_u16() {
    let mut base_i = Interner::new();
    let face = make_face("Test", "", "Creature — Test", "");
    let ch = build_characteristics(&face, &mut base_i);

    // 1. split_sacrifice_list with "named" + "and" / "," (positive, reverse, SelfRef, negative zero-leak)
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);

        let cp_ao = parse_cost_list(
            "Sacrifice a creature named Alpha and Omega and an artifact",
            0,
            &mut ctx,
        );
        assert!(cp_ao.unparsed.is_empty());
        assert_eq!(cp_ao.defs.len(), 2);
        assert!(matches!(
            &cp_ao.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.controller == Some(RelativeControl::You)
                && filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(matches!(
            &cp_ao.defs[1],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
                && filter.extra.is_empty()
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_pia = parse_cost_list(
            "Sacrifice a creature named Pia and Kiran Nalaar and an artifact",
            0,
            &mut ctx,
        );
        assert!(cp_pia.unparsed.is_empty());
        assert_eq!(cp_pia.defs.len(), 2);
        assert!(matches!(
            &cp_pia.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.extra == vec![ObjectPredicate::Named { name: "Pia and Kiran Nalaar".into() }]
        ));

        let cp_rak = parse_cost_list(
            "Sacrifice a creature named Rakdos, the Showstopper and an artifact",
            0,
            &mut ctx,
        );
        assert!(cp_rak.unparsed.is_empty());
        assert_eq!(cp_rak.defs.len(), 2);
        assert!(matches!(
            &cp_rak.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.extra == vec![ObjectPredicate::Named { name: "Rakdos, the Showstopper".into() }]
        ));

        let cp_rin = parse_cost_list(
            "Sacrifice a creature named Rin and Seri, Inseparable and an artifact",
            0,
            &mut ctx,
        );
        assert!(cp_rin.unparsed.is_empty());
        assert_eq!(cp_rin.defs.len(), 2);
        assert!(matches!(
            &cp_rin.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.extra == vec![ObjectPredicate::Named { name: "Rin and Seri, Inseparable".into() }]
        ));
        assert!(matches!(
            &cp_rin.defs[1],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
                && filter.extra.is_empty()
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_rev = parse_cost_list(
            "Sacrifice one or more artifacts and a creature named Alpha and Omega",
            0,
            &mut ctx,
        );
        assert!(cp_rev.unparsed.is_empty());
        assert_eq!(cp_rev.defs.len(), 2);
        assert!(matches!(
            &cp_rev.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));
        assert!(matches!(
            &cp_rev.defs[1],
            CostDef::Sacrifice {
                one_or_more: false,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.extra == vec![ObjectPredicate::Named { name: "Alpha and Omega".into() }]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        let cp_self = parse_cost_list("Sacrifice a creature named ~ and an artifact", 0, &mut ctx);
        assert!(cp_self.unparsed.is_empty());
        assert_eq!(cp_self.defs.len(), 2);
        assert!(matches!(
            &cp_self.defs[0],
            CostDef::Sacrifice {
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.extra == vec![ObjectPredicate::SameNameAsSource]
        ));
        assert!(ctx.interner.tables().subtypes.is_empty());

        // Negative / malformed rollback check: zero subtype/interner leaks
        for bad in [
            "Sacrifice a creature named Alpha and Omega and foozzle",
            "Sacrifice a creature named Alpha and foozzle",
            "Sacrifice a creature named Elf and Goblin and foozzle",
            "Sacrifice a creature named and an artifact",
        ] {
            let mut i_bad = Interner::new();
            let mut ctx_bad = make_ctx("Test", &ch, "", &mut i_bad);
            let cp_bad = parse_cost_list(bad, 0, &mut ctx_bad);
            assert!(cp_bad.defs.is_empty(), "expected empty defs for `{bad}`");
            assert!(
                !cp_bad.unparsed.is_empty(),
                "expected unparsed span for `{bad}`"
            );
            assert!(
                i_bad.tables().subtypes.is_empty(),
                "interner leaked subtype on `{bad}`: {:?}",
                i_bad.tables().subtypes
            );
        }
    }

    // 2. "Sacrifice one or more artifacts and another creature" -> 2 CostDef::Sacrifice items
    {
        let mut i = Interner::new();
        let mut ctx = make_ctx("Test", &ch, "", &mut i);
        let cp = parse_cost_list(
            "Sacrifice one or more artifacts and another creature",
            0,
            &mut ctx,
        );
        assert!(cp.unparsed.is_empty());
        assert_eq!(cp.defs.len(), 2);
        assert!(matches!(
            &cp.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::ARTIFACT
        ));
        assert!(matches!(
            &cp.defs[1],
            CostDef::Sacrifice {
                one_or_more: false,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.flags.contains(FilterFlags::OTHER)
        ));

        let cp_other = parse_cost_list(
            "Sacrifice one or more artifacts and other creatures",
            0,
            &mut ctx,
        );
        assert!(cp_other.unparsed.is_empty());
        assert_eq!(cp_other.defs.len(), 2);
        assert!(matches!(
            &cp_other.defs[1],
            CostDef::Sacrifice {
                one_or_more: false,
                target: CostObjectTarget::Filter { filter },
                ..
            } if filter.types_any == mtg_ir::CardTypeMask::CREATURE
                && filter.flags.contains(FilterFlags::OTHER)
        ));

        // Disjunctive "and/or other creatures" must remain 1 Sacrifice cost
        let cp_disj = parse_cost_list(
            "Sacrifice one or more artifacts and/or other creatures",
            0,
            &mut ctx,
        );
        assert!(cp_disj.unparsed.is_empty());
        assert_eq!(cp_disj.defs.len(), 1);
        assert!(matches!(
            &cp_disj.defs[0],
            CostDef::Sacrifice {
                one_or_more: true,
                ..
            }
        ));
    }

    // 3. u16::try_from(n) overflow guards in value.rs -> clean rejection (None, zero leaks, no wraparound)
    {
        for bad in [
            "an opponent has 70000 or more poison counters",
            "an opponent has 65536 or more poison counters",
            "an opponent has at least 70000 poison counters",
            "you've cast 70000 or more spells this turn",
            "you control 70000 or more lands with the same name",
        ] {
            let mut i = Interner::new();
            let mut ctx = make_ctx("Test", &ch, "", &mut i);
            let toks = tokenize(bad, 0);
            assert!(
                parse_condition(&toks, &mut ctx, &mut NullScope).is_none(),
                "must reject out-of-range u16 count without wraparound: `{bad}`"
            );
            assert!(i.tables().subtypes.is_empty());
            assert!(i.tables().counters.is_empty());
            assert!(i.tables().keywords.is_empty());
        }

        // Max valid u16 boundary still works cleanly
        let mut i_ok = Interner::new();
        let mut ctx_ok = make_ctx("Test", &ch, "", &mut i_ok);
        let toks_ok = tokenize("an opponent has 65535 or more poison counters", 0);
        let (cond, used) = parse_condition(&toks_ok, &mut ctx_ok, &mut NullScope).unwrap();
        assert_eq!(used, toks_ok.len());
        assert!(matches!(
            cond,
            Condition::OpponentPoisonAtLeast { min: 65535 }
        ));

        let toks_spells = tokenize("you've cast 65535 or more spells this turn", 0);
        let (cond_spells, used_spells) =
            parse_condition(&toks_spells, &mut ctx_ok, &mut NullScope).unwrap();
        assert_eq!(used_spells, toks_spells.len());
        assert!(matches!(
            cond_spells,
            Condition::SpellsCastThisTurnAtLeast {
                min: 65535,
                filter: None
            }
        ));

        let toks_lands = tokenize("you control 65535 or more lands with the same name", 0);
        let (cond_lands, used_lands) =
            parse_condition(&toks_lands, &mut ctx_ok, &mut NullScope).unwrap();
        assert_eq!(used_lands, toks_lands.len());
        assert!(matches!(
            cond_lands,
            Condition::SameNameLandsControlledAtLeast { min: 65535 }
        ));
    }
}
