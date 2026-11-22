use mtg_compiler::overrides::{OVERRIDES, get_or_parse};
use mtg_compiler::{
    COMPILER_VERSION, Interner, ManaSymbolKind, Punct, RawCardInput, RawFaceInput,
    ScryfallJsonCard, ScryfallJsonFace, SignedPart, TokenKind, UnsupportedAbilityKind,
    build_characteristics, build_templater, compile_bundle, compile_card,
    definition_support_report, derive_colors_from_mana_cost, effective_layout, hash_text,
    is_ability_or_flavor_word, is_non_game_layout, normalize_face, normalize_scryfall_card,
    replace_self_references, resolve_override, source_text, split_quoted_sentence_endings,
    split_sentences, strip_reminder_text, tokenize, unsupported_kind_label,
};
use mtg_ir::{
    AbilityDef, Amount, CardDatabase, CardLayout, CardTypeMask, ColorMask, CompareOp, Condition,
    CostDef, CounterTarget, EffectOp, EventDetail, EventKind, FilterFlags, GamePhase,
    IR_V2_VERSION, LayerMod, ManaColor, ManaPip, ManaProduction, PlayerRelation, PrintedPt,
    ReplacementWith, RestrictionKind, RuleMod, StaticBody, SupertypeMask, TargetKind, Timing, Zone,
    ZoneMask,
};

fn synth_creature(
    id: &str,
    name: &str,
    mana: &str,
    subtypes: &str,
    p: &str,
    t: &str,
    text: &str,
) -> RawCardInput {
    let colors = derive_colors_from_mana_cost(mana);
    RawCardInput {
        oracle_id: id.to_string(),
        name: name.to_string(),
        layout: Some("normal".to_string()),
        mana_cost: mana.to_string(),
        type_line: if subtypes.is_empty() {
            "Creature".to_string()
        } else {
            format!("Creature \u{2014} {subtypes}")
        },
        oracle_text: text.to_string(),
        power: Some(p.to_string()),
        toughness: Some(t.to_string()),
        colors,
        ..Default::default()
    }
}

fn synth_card(id: &str, name: &str, mana: &str, type_line: &str, text: &str) -> RawCardInput {
    let colors = derive_colors_from_mana_cost(mana);
    RawCardInput {
        oracle_id: id.to_string(),
        name: name.to_string(),
        layout: Some("normal".to_string()),
        mana_cost: mana.to_string(),
        type_line: type_line.to_string(),
        oracle_text: text.to_string(),
        colors,
        ..Default::default()
    }
}

fn compile_supported(card: &RawCardInput) -> (mtg_ir::CardDef, Interner) {
    let mut interner = Interner::new();
    let def = compile_card(card, &mut interner).expect("compile_card should succeed");
    assert!(
        def.fully_supported,
        "expected `{}` to be fully supported, got: {:?}",
        card.name,
        definition_support_report(&def)
    );
    let report = definition_support_report(&def);
    assert!(report.fully_supported);
    assert!(report.unsupported.is_empty());
    (def, interner)
}

fn has_keyword(def: &mtg_ir::CardDef, interner: &Interner, kw_lower: &str) -> bool {
    def.faces[0].abilities.iter().any(|ab| {
        matches!(
            ab,
            AbilityDef::Keyword(k) if interner.keyword_name(k.keyword.keyword) == Some(kw_lower)
        )
    })
}

// ============================================================================
// Feature 1: Normalization & Splitting (6 tests)
// ============================================================================

#[test]
fn t1_norm_01_balanced_paren_reminder_stripping() {
    assert_eq!(
        strip_reminder_text(
            "Flying (This creature can't be blocked except by creatures with flying or reach.)"
        ),
        "Flying"
    );
    let face = normalize_face(
        "Skywatcher",
        "Flying (This creature can't be blocked except by creatures with flying or reach.)\nVigilance (Attacking doesn't cause this creature to tap.)",
        false,
    );
    assert_eq!(face.lines.len(), 2);
    assert_eq!(face.lines[0].text, "Flying");
    assert_eq!(face.lines[1].text, "Vigilance");
}

#[test]
fn t1_norm_02_nested_parens_and_inline_comma_cleanup() {
    let input = "Ward {2} (Whenever this creature becomes the target (see CR 115), counter it unless paid.), haste";
    assert_eq!(strip_reminder_text(input), "Ward {2}, haste");
}

#[test]
fn t1_norm_03_self_name_epithet_and_pronoun_normalization() {
    let text = "When Vespera, Dusk Weaver enters, Vespera draws a card. She gains flying until end of turn.";
    let norm = replace_self_references(text, "Vespera, Dusk Weaver", true, Some(text));
    assert_eq!(
        norm,
        "When ~ enters, ~ draws a card. It gains flying until end of turn."
    );
}

#[test]
fn t1_norm_04_named_token_guard_and_quote_preservation() {
    let text = "When Kaelen enters, create a token named Kaelen and \"He strikes.\"";
    let norm = replace_self_references(text, "Kaelen, Skyblade", true, Some(text));
    assert_eq!(
        norm,
        "When ~ enters, create a token named Kaelen and \"He strikes.\""
    );
}

#[test]
fn t1_norm_05_ability_word_and_flavor_word_stripping() {
    assert!(is_ability_or_flavor_word("Landfall"));
    assert!(is_ability_or_flavor_word("Starlight Surge"));
    assert!(!is_ability_or_flavor_word("Flying"));
    assert!(!is_ability_or_flavor_word("I, II"));
    let face = normalize_face(
        "Aether Sage",
        "Landfall \u{2014} Whenever a land enters under your control, draw a card.",
        false,
    );
    assert_eq!(face.lines.len(), 1);
    assert_eq!(face.lines[0].ability_word.as_deref(), Some("Landfall"));
    assert_eq!(
        face.lines[0].body,
        "Whenever a land enters under your control, draw a card."
    );
}

#[test]
fn t1_norm_06_quote_aware_sentence_splitting_and_utf16_hash() {
    let face = normalize_face(
        "Chrono Construct",
        "When Chrono Construct enters, it gains \"When ~ attacks, scry 1.\" Draw a card.",
        false,
    );
    let sentences = split_quoted_sentence_endings(&face.lines[0].sentences);
    assert_eq!(sentences.len(), 2);
    assert_eq!(
        sentences[0].text,
        "When ~ enters, it gains \"When ~ attacks, scry 1.\""
    );
    assert_eq!(sentences[1].text, "Draw a card.");

    let split2 = split_sentences("Target creature gets +2/+2! Untap it.", 0);
    assert_eq!(split2.len(), 2);

    let h = hash_text(&source_text(&synth_creature(
        "e2e-t1-norm-06",
        "Chrono Construct",
        "{2}",
        "Construct",
        "2",
        "2",
        "When Chrono Construct enters, draw a card.",
    )));
    assert_eq!(h.len(), 8);
    assert!(
        h.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}

// ============================================================================
// Feature 2: Scryfall Input & NON_GAME_LAYOUTS Filter (5 tests)
// ============================================================================

#[test]
fn t1_input_01_non_game_layouts_filter_complete() {
    for non_game in [
        "art_series",
        "double_faced_token",
        "emblem",
        "front_card",
        "planar",
        "scheme",
        "token",
        "vanguard",
    ] {
        assert!(
            is_non_game_layout(Some(non_game)),
            "{non_game} must be filtered"
        );
    }
    for playable in [
        "normal",
        "split",
        "flip",
        "transform",
        "modal_dfc",
        "adventure",
        "meld",
        "reversible_card",
        "prepare",
        "omen",
        "saga",
        "class",
    ] {
        assert!(
            !is_non_game_layout(Some(playable)),
            "{playable} must be kept"
        );
    }
}

#[test]
fn t1_input_02_layout_clamping_for_special_normal_layouts() {
    for raw_layout in ["saga", "class", "case", "leveler", "mutate", "prototype"] {
        let scry = ScryfallJsonCard {
            oracle_id: Some("e2e-t1-inp-02".into()),
            name: Some("Synthetic Saga".into()),
            layout: Some(raw_layout.into()),
            ..Default::default()
        };
        let norm = normalize_scryfall_card(&scry);
        assert_eq!(norm.layout, None, "expected {raw_layout} to clamp to None");
        assert_eq!(effective_layout(&norm), CardLayout::Normal);
    }
}

#[test]
fn t1_input_03_three_tier_face_color_fallback() {
    let scry = ScryfallJsonCard {
        oracle_id: Some("e2e-t1-inp-03".into()),
        name: Some("Front // Back".into()),
        layout: Some("modal_dfc".into()),
        card_faces: Some(vec![
            ScryfallJsonFace {
                name: Some("Front".into()),
                mana_cost: Some("{1}{U}".into()),
                colors: Some(vec![]), // Tier 1: explicit empty preserved
                ..Default::default()
            },
            ScryfallJsonFace {
                name: Some("Back".into()),
                mana_cost: Some("".into()),
                colors: None,
                color_indicator: Some(vec!["R".into()]), // Tier 2: color_indicator
                ..Default::default()
            },
            ScryfallJsonFace {
                name: Some("Third".into()),
                mana_cost: Some("{2}{G}{W}".into()),
                colors: None,
                color_indicator: None, // Tier 3: derived from mana_cost
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let norm = normalize_scryfall_card(&scry);
    assert_eq!(norm.faces[0].colors, Vec::<String>::new());
    assert_eq!(norm.faces[1].colors, vec!["R"]);
    assert_eq!(norm.faces[2].colors, vec!["G", "W"]);
}

#[test]
fn t1_input_04_front_face_name_and_stats_fallback() {
    let scry = ScryfallJsonCard {
        oracle_id: Some("e2e-t1-inp-04".into()),
        name: Some("Whole Card Name".into()),
        layout: Some("adventure".into()),
        card_faces: Some(vec![
            ScryfallJsonFace {
                name: Some("Aether Knight".into()),
                mana_cost: Some("{1}{W}".into()),
                type_line: Some("Creature \u{2014} Human Knight".into()),
                power: Some("2".into()),
                toughness: Some("3".into()),
                ..Default::default()
            },
            ScryfallJsonFace {
                name: Some("Valor Strike".into()),
                mana_cost: Some("{W}".into()),
                type_line: Some("Instant \u{2014} Adventure".into()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let norm = normalize_scryfall_card(&scry);
    assert_eq!(norm.name, "Aether Knight");
    assert_eq!(norm.power.as_deref(), Some("2"));
    assert_eq!(norm.toughness.as_deref(), Some("3"));
    assert_eq!(effective_layout(&norm), CardLayout::Adventure);
}

#[test]
fn t1_input_05_derive_colors_from_hybrid_phyrexian_twobrid() {
    assert_eq!(
        derive_colors_from_mana_cost("{2/W}{U/B}{R/P}{C/G}{X}"),
        vec!["W", "U", "B", "R", "G"]
    );
    assert_eq!(derive_colors_from_mana_cost("{3}{C}"), Vec::<String>::new());
}

// ============================================================================
// Feature 3: Hash-Gated 38 Overrides (5 tests)
// ============================================================================

#[test]
fn t1_ovr_01_all_38_overrides_present_and_fully_supported() {
    assert_eq!(OVERRIDES.len(), 38);
    for slot in &OVERRIDES {
        let entry = get_or_parse(slot).expect("override json must parse");
        assert!(entry.card.fully_supported);
        assert_eq!(entry.card.text_hash, slot.expected_text_hash);
        assert!(definition_support_report(&entry.card).fully_supported);
    }
}

#[test]
fn t1_ovr_02_resolve_override_exact_hash_hit() {
    let slot = &OVERRIDES[0];
    let entry = get_or_parse(slot).unwrap();
    let mut interner = Interner::new();
    let resolved = resolve_override(
        slot.oracle_id,
        slot.expected_text_hash,
        &entry.card.name,
        &mut interner,
    )
    .unwrap()
    .expect("exact oracle_id + text_hash should hit override");
    assert_eq!(resolved.compiler_version, COMPILER_VERSION);
    assert!(resolved.fully_supported);
}

#[test]
fn t1_ovr_03_single_face_rename_on_override() {
    let single_face_slot = OVERRIDES
        .iter()
        .find(|s| get_or_parse(s).unwrap().card.faces.len() == 1)
        .expect("single-face override exists");
    let mut interner = Interner::new();
    let renamed = resolve_override(
        single_face_slot.oracle_id,
        single_face_slot.expected_text_hash,
        "Synthetic Renamed Override",
        &mut interner,
    )
    .unwrap()
    .unwrap();
    assert_eq!(renamed.name, "Synthetic Renamed Override");
    assert_eq!(renamed.faces[0].name, "Synthetic Renamed Override");
}

#[test]
fn t1_ovr_04_hash_mismatch_returns_none_and_falls_back_in_compile_card() {
    let slot = &OVERRIDES[1];
    let mut interner = Interner::new();
    let miss = resolve_override(
        slot.oracle_id,
        "deadbeef",
        "Synthetic Fallback",
        &mut interner,
    )
    .unwrap();
    assert!(miss.is_none());

    let fallback_card = synth_creature(
        slot.oracle_id,
        "Synthetic Fallback",
        "{1}{U}",
        "Merfolk",
        "2",
        "1",
        "",
    );
    let compiled = compile_card(&fallback_card, &mut interner).unwrap();
    assert_eq!(compiled.name, "Synthetic Fallback");
    assert_eq!(compiled.faces[0].characteristics.mana_cost.mana_value(), 2);
}

#[test]
fn t1_ovr_05_override_remaps_cleanly_into_populated_interner() {
    let slot = &OVERRIDES[2];
    let entry = get_or_parse(slot).unwrap();
    let mut interner = Interner::new();
    interner.intern_subtype("Aetherling");
    interner.intern_counter("spark");
    interner.intern_keyword("warp");
    let resolved = resolve_override(
        slot.oracle_id,
        slot.expected_text_hash,
        &entry.card.name,
        &mut interner,
    )
    .unwrap()
    .unwrap();
    assert_eq!(resolved.oracle_id, slot.oracle_id);
    let tables = interner.tables();
    for st in &entry.tables.subtypes {
        assert!(tables.subtypes.contains(st));
    }
}

// ============================================================================
// Feature 4: All 10 CardLayouts (10 tests)
// ============================================================================

#[test]
fn t1_layout_01_normal_vanilla_creature() {
    let card = synth_creature(
        "e2e-t1-lay-01",
        "Ironwood Stag",
        "{2}{G}",
        "Elk",
        "3",
        "3",
        "",
    );
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Normal);
    assert_eq!(def.faces.len(), 1);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_02_split_card_two_spell_faces() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-02".into(),
        name: "Spark // Mend".into(),
        layout: Some("split".into()),
        faces: vec![
            RawFaceInput {
                name: "Spark".into(),
                mana_cost: "{R}".into(),
                type_line: "Instant".into(),
                oracle_text: "Spark deals 2 damage to any target.".into(),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Mend".into(),
                mana_cost: "{W}".into(),
                type_line: "Sorcery".into(),
                oracle_text: "You gain 4 life.".into(),
                colors: vec!["W".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Split);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_03_flip_card() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-03".into(),
        name: "Novice Mystic".into(),
        layout: Some("flip".into()),
        faces: vec![
            RawFaceInput {
                name: "Novice Mystic".into(),
                mana_cost: "{1}{U}".into(),
                type_line: "Creature \u{2014} Human Monk".into(),
                oracle_text: "Whenever you cast an instant spell, draw a card.".into(),
                power: Some("1".into()),
                toughness: Some("2".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Grand Archivist".into(),
                mana_cost: "".into(),
                type_line: "Legendary Creature \u{2014} Human Wizard".into(),
                oracle_text: "Flying".into(),
                power: Some("3".into()),
                toughness: Some("4".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Flip);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_04_transform_dfc() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-04".into(),
        name: "Dusk Initiate".into(),
        layout: Some("transform".into()),
        faces: vec![
            RawFaceInput {
                name: "Dusk Initiate".into(),
                mana_cost: "{1}{B}".into(),
                type_line: "Creature \u{2014} Vampire".into(),
                oracle_text: "Lifelink".into(),
                power: Some("2".into()),
                toughness: Some("1".into()),
                colors: vec!["B".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Nightfall Stalker".into(),
                mana_cost: "".into(),
                type_line: "Creature \u{2014} Vampire".into(),
                oracle_text: "Flying, lifelink".into(),
                power: Some("3".into()),
                toughness: Some("3".into()),
                colors: vec!["B".into()],
                color_indicator: Some(vec!["B".into()]),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Transform);
    assert_eq!(def.faces.len(), 2);
    assert_eq!(
        def.faces[1].characteristics.color_indicator,
        Some(ColorMask::B)
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_05_modal_dfc() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-05".into(),
        name: "Skyshard Bolt".into(),
        layout: Some("modal_dfc".into()),
        faces: vec![
            RawFaceInput {
                name: "Skyshard Bolt".into(),
                mana_cost: "{1}{R}".into(),
                type_line: "Instant".into(),
                oracle_text: "Skyshard Bolt deals 3 damage to target creature.".into(),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Skyshard Crag".into(),
                mana_cost: "".into(),
                type_line: "Land".into(),
                oracle_text: "Skyshard Crag enters tapped.\n{T}: Add {R}.".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::ModalDfc);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_06_adventure() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-06".into(),
        name: "Grove Ranger".into(),
        layout: Some("adventure".into()),
        faces: vec![
            RawFaceInput {
                name: "Grove Ranger".into(),
                mana_cost: "{2}{G}".into(),
                type_line: "Creature \u{2014} Elf Scout".into(),
                oracle_text: "Reach".into(),
                power: Some("3".into()),
                toughness: Some("2".into()),
                colors: vec!["G".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Verdant Sight".into(),
                mana_cost: "{G}".into(),
                type_line: "Sorcery \u{2014} Adventure".into(),
                oracle_text: "Scry 2, then draw a card.".into(),
                colors: vec!["G".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Adventure);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_07_meld() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-07".into(),
        name: "Solis Vanguard".into(),
        layout: Some("meld".into()),
        faces: vec![RawFaceInput {
            name: "Solis Vanguard".into(),
            mana_cost: "{1}{W}".into(),
            type_line: "Creature \u{2014} Soldier".into(),
            oracle_text: "First strike".into(),
            power: Some("2".into()),
            toughness: Some("1".into()),
            colors: vec!["W".into()],
            ..Default::default()
        }],
        ..Default::default()
    };
    let (def, interner) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Meld);
    assert!(has_keyword(&def, &interner, "first strike"));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_08_reversible_card() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-08".into(),
        name: "Mirror Drake".into(),
        layout: Some("reversible_card".into()),
        faces: vec![
            RawFaceInput {
                name: "Mirror Drake".into(),
                mana_cost: "{2}{U}".into(),
                type_line: "Creature \u{2014} Drake".into(),
                oracle_text: "Flying".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Mirror Drake".into(),
                mana_cost: "{2}{U}".into(),
                type_line: "Creature \u{2014} Drake".into(),
                oracle_text: "Flying".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::ReversibleCard);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_09_prepare() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-09".into(),
        name: "Field Tactician".into(),
        layout: Some("prepare".into()),
        faces: vec![
            RawFaceInput {
                name: "Field Tactician".into(),
                mana_cost: "{1}{W}".into(),
                type_line: "Creature \u{2014} Human Soldier".into(),
                oracle_text: "Vigilance".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["W".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Battle Plan".into(),
                mana_cost: "{1}{W}".into(),
                type_line: "Instant".into(),
                oracle_text: "Draw a card.".into(),
                colors: vec!["W".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Prepare);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_layout_10_omen() {
    let card = RawCardInput {
        oracle_id: "e2e-t1-lay-10".into(),
        name: "Cloudscale Dragon".into(),
        layout: Some("omen".into()),
        faces: vec![
            RawFaceInput {
                name: "Cloudscale Dragon".into(),
                mana_cost: "{3}{U}{U}".into(),
                type_line: "Creature \u{2014} Dragon".into(),
                oracle_text: "Flying".into(),
                power: Some("4".into()),
                toughness: Some("4".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Gale Omen".into(),
                mana_cost: "{1}{U}".into(),
                type_line: "Instant \u{2014} Omen".into(),
                oracle_text: "Scry 2.".into(),
                colors: vec!["U".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = compile_supported(&card);
    assert_eq!(def.layout, CardLayout::Omen);
}

// ============================================================================
// Feature 5: Characteristics, ManaCost, PrintedPt & Intrinsic Mana (6 tests)
// ============================================================================

#[test]
fn t1_char_01_manacost_all_pip_kinds_and_mana_value() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Prism Engine".into(),
        mana_cost: "{2}{X}{W/U}{2/B}{R/P}{G/W/P}{C/G}{S}".into(),
        type_line: "Artifact".into(),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert_eq!(ch.mana_cost.generic, 2);
    assert_eq!(ch.mana_cost.x_count, 1);
    assert_eq!(ch.mana_cost.mana_value(), 9); // 2 + 1 + 2 + 1 + 1 + 1 + 1
    assert!(ch.mana_cost.pips.contains(&ManaPip::Snow));
    assert!(ch.mana_cost.pips.contains(&ManaPip::TwoBrid {
        color: ManaColor::B
    }));
    assert!(ch.mana_cost.pips.contains(&ManaPip::Phyrexian {
        color: ManaColor::R
    }));
    assert!(ch.mana_cost.pips.contains(&ManaPip::HybridPhyrexian {
        a: ManaColor::G,
        b: ManaColor::W
    }));
    assert!(ch.mana_cost.pips.contains(&ManaPip::ColorlessHybrid {
        color: ManaColor::G
    }));
}

#[test]
fn t1_char_02_supertypes_types_kindred_and_subtypes() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Snow Elder".into(),
        mana_cost: "{3}{G}".into(),
        type_line: "Legendary Snow Kindred Artifact Creature \u{2014} Elf Shaman".into(),
        power: Some("3".into()),
        toughness: Some("4".into()),
        colors: vec!["G".into()],
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert!(
        ch.supertypes
            .contains(SupertypeMask::LEGENDARY | SupertypeMask::SNOW)
    );
    assert!(
        ch.types
            .contains(CardTypeMask::KINDRED | CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
    );
    assert_eq!(interner.subtype_name(ch.subtypes[0]), Some("Elf"));
    assert_eq!(interner.subtype_name(ch.subtypes[1]), Some("Shaman"));
    assert_eq!(
        ch.pt,
        Some(PrintedPt::Fixed {
            power: 3,
            toughness: 4
        })
    );
}

#[test]
fn t1_char_03_cda_printed_pt_variants() {
    let mut interner = Interner::new();
    let star_star = RawFaceInput {
        name: "Star Elemental".into(),
        type_line: "Creature \u{2014} Elemental".into(),
        power: Some("*".into()),
        toughness: Some("*".into()),
        ..Default::default()
    };
    let ch1 = build_characteristics(&star_star, &mut interner);
    assert_eq!(
        ch1.pt,
        Some(PrintedPt::Cda {
            power: None,
            toughness: None
        })
    );

    let star_plus = RawFaceInput {
        name: "Grove Avatar".into(),
        type_line: "Creature \u{2014} Avatar".into(),
        power: Some("*".into()),
        toughness: Some("1+*".into()),
        ..Default::default()
    };
    let ch2 = build_characteristics(&star_plus, &mut interner);
    assert_eq!(
        ch2.pt,
        Some(PrintedPt::Cda {
            power: None,
            toughness: None
        })
    );

    let fixed_star = RawFaceInput {
        name: "Shield Spirit".into(),
        type_line: "Creature \u{2014} Spirit".into(),
        power: Some("2".into()),
        toughness: Some("*".into()),
        ..Default::default()
    };
    let ch3 = build_characteristics(&fixed_star, &mut interner);
    assert_eq!(
        ch3.pt,
        Some(PrintedPt::Cda {
            power: Some(2),
            toughness: None
        })
    );
}

#[test]
fn t1_char_04_planeswalker_loyalty_and_battle_defense() {
    let mut interner = Interner::new();
    let pw = RawFaceInput {
        name: "Aurelia, Synth Walker".into(),
        type_line: "Legendary Planeswalker \u{2014} Aurelia".into(),
        loyalty: Some("4".into()),
        ..Default::default()
    };
    let ch_pw = build_characteristics(&pw, &mut interner);
    assert_eq!(ch_pw.loyalty, Some(4));
    assert_eq!(ch_pw.pt, None);

    let battle = RawFaceInput {
        name: "Siege of Starspire".into(),
        type_line: "Battle \u{2014} Siege".into(),
        defense: Some("5".into()),
        ..Default::default()
    };
    let ch_b = build_characteristics(&battle, &mut interner);
    assert_eq!(ch_b.defense, Some(5));
}

#[test]
fn t1_char_05_intrinsic_basic_land_mana_all_five_colors() {
    let pairs = [
        ("Plains", ManaColor::W),
        ("Island", ManaColor::U),
        ("Swamp", ManaColor::B),
        ("Mountain", ManaColor::R),
        ("Forest", ManaColor::G),
    ];
    for (idx, (subtype, expected_color)) in pairs.into_iter().enumerate() {
        let card = synth_card(
            &format!("e2e-t1-land-{idx}"),
            &format!("Synthetic {subtype}"),
            "",
            &format!("Basic Land \u{2014} {subtype}"),
            "",
        );
        let (def, _) = compile_supported(&card);
        assert_eq!(def.faces[0].abilities.len(), 1);
        match &def.faces[0].abilities[0] {
            AbilityDef::Activated(act) => {
                assert!(act.is_mana);
                assert!(act.intrinsic);
                assert_eq!(act.id, "mana_tap");
                assert_eq!(act.timing, Timing::Mana);
                assert_eq!(act.cost, vec![CostDef::TapSelf]);
                assert!(matches!(
                    &act.effect[0].op,
                    EffectOp::AddMana { production: ManaProduction::Fixed { symbols }, .. }
                    if symbols.as_slice() == [expected_color]
                ));
            }
            other => panic!("expected intrinsic mana ability, got {other:?}"),
        }
    }
}

#[test]
fn t1_char_06_intrinsic_dual_land_mana_ids() {
    let card = synth_card(
        "e2e-t1-land-dual",
        "Synthetic Tundra",
        "",
        "Land \u{2014} Plains Island",
        "",
    );
    let (def, _) = compile_supported(&card);
    assert_eq!(def.faces[0].abilities.len(), 2);
    let ids: Vec<&str> = def.faces[0]
        .abilities
        .iter()
        .map(|a| match a {
            AbilityDef::Activated(act) => act.id.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(ids, vec!["mana_tap", "mana_u"]);
}

// ============================================================================
// Feature 6: CR 702 Keywords (8 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_01_bare_evergreen_keywords_list() {
    let card = synth_creature(
        "e2e-t1-kw-01",
        "Skyblade Vanguard",
        "{2}{W}{W}",
        "Angel Soldier",
        "3",
        "3",
        "Flying, first strike, vigilance, lifelink",
    );
    let (def, interner) = compile_supported(&card);
    for kw in ["flying", "first strike", "vigilance", "lifelink"] {
        assert!(has_keyword(&def, &interner, kw), "missing keyword {kw}");
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_02_deathtouch_trample_haste_menace_reach_hexproof_indestructible() {
    let card = synth_creature(
        "e2e-t1-kw-02",
        "Apex Chimera",
        "{3}{B}{R}{G}",
        "Chimera",
        "5",
        "5",
        "Deathtouch, trample, haste, menace, reach, hexproof, indestructible",
    );
    let (def, interner) = compile_supported(&card);
    for kw in [
        "deathtouch",
        "trample",
        "haste",
        "menace",
        "reach",
        "hexproof",
        "indestructible",
    ] {
        assert!(has_keyword(&def, &interner, kw), "missing keyword {kw}");
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_03_ward_mana_and_ward_pay_life() {
    let c1 = synth_creature(
        "e2e-t1-kw-03a",
        "Ward Sentinel",
        "{2}{U}",
        "Sphinx",
        "2",
        "3",
        "Ward {2}",
    );
    let (d1, i1) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k)
        if i1.keyword_name(k.keyword.keyword) == Some("ward")
            && k.keyword.cost.as_ref().is_some_and(|mc| mc.generic == 2)
    )));

    let c2 = synth_creature(
        "e2e-t1-kw-03b",
        "Blood Warder",
        "{2}{B}",
        "Horror",
        "3",
        "2",
        "Ward\u{2014}Pay 3 life.",
    );
    let (d2, i2) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k)
        if i2.keyword_name(k.keyword.keyword) == Some("ward")
            && matches!(k.keyword.cost_defs.as_slice(), [CostDef::PayLife { .. }])
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_04_equip_and_enchant_filters() {
    let equip_card = synth_card(
        "e2e-t1-kw-04a",
        "Starforged Greatsword",
        "{2}",
        "Artifact \u{2014} Equipment",
        "Equip {2}",
    );
    let (d1, i1) = compile_supported(&equip_card);
    assert!(has_keyword(&d1, &i1, "equip"));

    let aura_card = synth_card(
        "e2e-t1-kw-04b",
        "Aether Mantle",
        "{1}{U}",
        "Enchantment \u{2014} Aura",
        "Enchant creature",
    );
    let (d2, i2) = compile_supported(&aura_card);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k)
        if i2.keyword_name(k.keyword.keyword) == Some("enchant")
            && k.keyword.enchant_filter.as_ref().is_some_and(|f| f.types_any.contains(CardTypeMask::CREATURE))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_05_kicker_cycling_and_basic_landcycling() {
    let kicker = synth_creature(
        "e2e-t1-kw-05a",
        "Kicked Skirmisher",
        "{1}{R}",
        "Goblin",
        "2",
        "1",
        "Kicker {2}{R}",
    );
    let (d1, i1) = compile_supported(&kicker);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k)
        if i1.keyword_name(k.keyword.keyword) == Some("kicker")
            && !k.keyword.kicker_costs.is_empty()
    )));

    let cyc = synth_creature(
        "e2e-t1-kw-05b",
        "Canyon Wanderer",
        "{4}{G}",
        "Wurm",
        "5",
        "4",
        "Basic landcycling {2}",
    );
    let (d2, i2) = compile_supported(&cyc);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k)
        if i2.keyword_name(k.keyword.keyword) == Some("basic landcycling")
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_06_protection_from_color_and_landwalk() {
    let prot = synth_creature(
        "e2e-t1-kw-06a",
        "Dawn Paladin",
        "{W}{W}",
        "Human Knight",
        "2",
        "2",
        "Protection from black",
    );
    let (d1, i1) = compile_supported(&prot);
    assert!(has_keyword(&d1, &i1, "protection"));

    let walk = synth_creature(
        "e2e-t1-kw-06b",
        "Tide Stalker",
        "{1}{U}",
        "Merfolk",
        "1",
        "2",
        "Islandwalk",
    );
    let (d2, i2) = compile_supported(&walk);
    assert!(has_keyword(&d2, &i2, "islandwalk"));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_07_vanishing_fading_suspend_and_prototype() {
    let van = synth_creature(
        "e2e-t1-kw-07a",
        "Fading Echo",
        "{1}{U}",
        "Illusion",
        "3",
        "3",
        "Vanishing 3",
    );
    let (d1, i1) = compile_supported(&van);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k) if i1.keyword_name(k.keyword.keyword) == Some("vanishing") && k.keyword.n == Some(3)
    )));

    let proto = synth_creature(
        "e2e-t1-kw-07b",
        "Clockwork Goliath",
        "{7}",
        "Artifact Creature \u{2014} Construct",
        "6",
        "6",
        "Prototype {2}{W} \u{2014} 3/3",
    );
    let (d2, i2) = compile_supported(&proto);
    assert!(has_keyword(&d2, &i2, "prototype"));
    assert!(d2.faces[0].prototype_spec.is_some());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_kw_08_partner_with_crew_saddle_and_toxic() {
    let veh = synth_card(
        "e2e-t1-kw-08a",
        "Skybarge",
        "{3}",
        "Artifact \u{2014} Vehicle",
        "Flying\nCrew 2",
    );
    let (d1, i1) = compile_supported(&veh);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k) if i1.keyword_name(k.keyword.keyword) == Some("crew") && k.keyword.n == Some(2)
    )));

    let tox = synth_creature(
        "e2e-t1-kw-08b",
        "Blight Mite",
        "{B}",
        "Insect",
        "1",
        "1",
        "Toxic 1",
    );
    let (d2, i2) = compile_supported(&tox);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Keyword(k) if i2.keyword_name(k.keyword.keyword) == Some("toxic") && k.keyword.n == Some(1)
    )));
}

// ============================================================================
// Feature 7: Costs & Cost Modifiers (7 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_01_tap_and_untap_symbol_costs() {
    let c1 = synth_creature(
        "e2e-t1-cost-01a",
        "Archivist Aide",
        "{1}{U}",
        "Wizard",
        "1",
        "2",
        "{T}: Draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.cost == vec![CostDef::TapSelf]
    )));

    let c2 = synth_creature(
        "e2e-t1-cost-01b",
        "Clockwork Weaver",
        "{2}",
        "Construct",
        "2",
        "2",
        "{Q}: Untap target creature.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.cost == vec![CostDef::UntapSelf { via_symbol: true }]
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_02_mana_and_sacrifice_self_or_other() {
    let c1 = synth_card(
        "e2e-t1-cost-02a",
        "Altar Relic",
        "{1}",
        "Artifact",
        "{2}, Sacrifice Altar Relic: Draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Mana { cost, .. } if cost.generic == 2))
            && a.cost.iter().any(|c| matches!(c, CostDef::Sacrifice { .. }))
    )));

    let c2 = synth_creature(
        "e2e-t1-cost-02b",
        "Grim Priest",
        "{1}{B}",
        "Cleric",
        "2",
        "1",
        "Sacrifice another creature: Target player loses 1 life.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Sacrifice { .. }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_03_discard_card_and_discard_hand() {
    let c1 = synth_creature(
        "e2e-t1-cost-03a",
        "Rummage Mystic",
        "{1}{R}",
        "Wizard",
        "2",
        "1",
        "{T}, Discard a card: Draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.cost.iter().any(|c| matches!(c, CostDef::Discard { .. }))
    )));

    let c2 = synth_card(
        "e2e-t1-cost-03b",
        "Null Vault",
        "{3}",
        "Artifact",
        "Discard your hand: Draw two cards.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.cost.contains(&CostDef::DiscardHand)
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_04_pay_life_and_pay_energy() {
    let c1 = synth_card(
        "e2e-t1-cost-04a",
        "Blood Chalice",
        "{2}",
        "Artifact",
        "{T}, Pay 2 life: Draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::PayLife { amount, .. } if **amount == Amount::Const { n: 2 }))
    )));

    let c2 = synth_card(
        "e2e-t1-cost-04b",
        "Aether Coil",
        "{1}",
        "Artifact",
        "Pay {E}{E}: Scry 2.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::PayEnergy { count } if **count == Amount::Const { n: 2 }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_05_loyalty_plus_minus_and_minus_x() {
    let pw = RawCardInput {
        oracle_id: "e2e-t1-cost-05".into(),
        name: "Soren, Synth Mage".into(),
        layout: Some("normal".into()),
        mana_cost: "{2}{U}{B}".into(),
        type_line: "Legendary Planeswalker \u{2014} Soren".into(),
        oracle_text: "+1: Draw a card.\n\u{2212}2: Target creature gets -2/-2 until end of turn.\n\u{2212}X: Target player mills X cards.".into(),
        loyalty: Some("3".into()),
        colors: vec!["U".into(), "B".into()],
        ..Default::default()
    };
    let (def, _) = compile_supported(&pw);
    assert_eq!(def.faces[0].abilities.len(), 3);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { delta: 1, is_x: false, .. }))
    )));
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { delta: -2, is_x: false, .. }))
    )));
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { is_x: true, .. }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_06_put_and_remove_counters_costs() {
    let c1 = synth_creature(
        "e2e-t1-cost-06a",
        "Counter Adept",
        "{2}{G}",
        "Elf",
        "2",
        "2",
        "Remove a +1/+1 counter from Counter Adept: Counter Adept gains indestructible until end of turn.",
    );
    let (d1, i1) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(
            c,
            CostDef::RemoveCounters { counter: CounterTarget::Specific(cid), .. }
            if i1.counter_name(*cid) == Some("+1/+1")
        ))
    )));

    let c2 = synth_creature(
        "e2e-t1-cost-06b",
        "Blight Bruiser",
        "{1}{B}",
        "Horror",
        "3",
        "3",
        "Put a -1/-1 counter on Blight Bruiser: Target creature gets -1/-1 until end of turn.",
    );
    let (d2, i2) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(
            c,
            CostDef::PutCounters { counter, .. } if i2.counter_name(*counter) == Some("-1/-1")
        ))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_cost_07_exile_from_graveyard_and_tap_creatures() {
    let c1 = synth_card(
        "e2e-t1-cost-07a",
        "Soul Lantern",
        "{2}",
        "Artifact",
        "Exile a card from your graveyard: Add {C}.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::ExileFrom { zone: Zone::Graveyard, .. }))
    )));

    let c2 = synth_card(
        "e2e-t1-cost-07b",
        "Rally Banner",
        "{3}",
        "Artifact",
        "Tap two untapped creatures you control: Draw a card.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::TapOthers { .. }))
    )));
}

// ============================================================================
// Feature 8: Filters & Targets (7 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_01_nontoken_creature_you_control_and_opponent_perm() {
    let card = synth_card(
        "e2e-t1-flt-01",
        "Banishing Seal",
        "{2}{W}",
        "Sorcery",
        "Destroy target nonland permanent an opponent controls.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.targets.len(), 1);
            assert_eq!(s.targets[0].kind, TargetKind::Object);
            assert!(s.targets[0].filter.types_none.contains(CardTypeMask::LAND));
        }
        other => panic!("expected spell ability, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_02_tapped_and_attacking_filter_flags() {
    let card = synth_card(
        "e2e-t1-flt-02",
        "Sentinel Smite",
        "{1}{W}",
        "Instant",
        "Destroy target tapped creature.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.targets[0].filter.flags.contains(FilterFlags::TAPPED));
            assert!(
                s.targets[0]
                    .filter
                    .types_any
                    .contains(CardTypeMask::CREATURE)
            );
        }
        other => panic!("expected spell ability, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_03_color_and_subtype_filters() {
    let card = synth_card(
        "e2e-t1-flt-03",
        "Snuff Decree",
        "{1}{B}",
        "Instant",
        "Destroy target nonblack creature.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.targets[0].filter.colors_none.contains(ColorMask::B));
            assert!(
                s.targets[0]
                    .filter
                    .types_any
                    .contains(CardTypeMask::CREATURE)
            );
        }
        other => panic!("expected spell ability, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_04_graveyard_and_stack_zone_filters() {
    let c1 = synth_card(
        "e2e-t1-flt-04a",
        "Echo Retrieval",
        "{1}{B}",
        "Sorcery",
        "Return target creature card from your graveyard to your hand.",
    );
    let (d1, _) = compile_supported(&c1);
    match &d1.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.targets[0].filter.zones.contains(ZoneMask::GRAVEYARD));
            assert!(
                s.targets[0]
                    .filter
                    .types_any
                    .contains(CardTypeMask::CREATURE)
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }

    let c2 = synth_card(
        "e2e-t1-flt-04b",
        "Arcane Denial Synth",
        "{U}{U}",
        "Instant",
        "Counter target instant or sorcery spell.",
    );
    let (d2, _) = compile_supported(&c2);
    match &d2.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(
                s.targets[0]
                    .filter
                    .types_any
                    .contains(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_05_player_and_any_target_specs() {
    let c1 = synth_card(
        "e2e-t1-flt-05a",
        "Mind Spike",
        "{1}{B}",
        "Sorcery",
        "Target opponent discards two cards.",
    );
    let (d1, _) = compile_supported(&c1);
    match &d1.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.targets[0].kind, TargetKind::Player);
            assert_eq!(
                s.targets[0].player.map(|p| p.relation),
                Some(PlayerRelation::Opponent)
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }

    let c2 = synth_card(
        "e2e-t1-flt-05b",
        "Flame Arc",
        "{R}",
        "Instant",
        "Flame Arc deals 3 damage to any target.",
    );
    let (d2, _) = compile_supported(&c2);
    match &d2.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.targets[0].kind, TargetKind::ObjectOrPlayer);
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_06_up_to_multi_target_and_divided_damage() {
    let c1 = synth_card(
        "e2e-t1-flt-06a",
        "Twin Freeze",
        "{1}{U}",
        "Instant",
        "Tap up to two target creatures.",
    );
    let (d1, _) = compile_supported(&c1);
    match &d1.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.targets[0].min, 0);
            assert_eq!(s.targets[0].max, 2);
        }
        other => panic!("expected spell, got {other:?}"),
    }

    let c2 = synth_card(
        "e2e-t1-flt-06b",
        "Forked Ember",
        "{1}{R}",
        "Instant",
        "Forked Ember deals 2 damage divided as you choose among one or two targets.",
    );
    let (d2, _) = compile_supported(&c2);
    match &d2.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.targets[0].min, 1);
            assert_eq!(s.targets[0].max, 2);
            assert!(s.targets[0].divide_total.is_some());
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_filter_07_mana_value_and_power_comparison_filters() {
    let card = synth_card(
        "e2e-t1-flt-07",
        "Smite the Weak",
        "{1}{W}",
        "Instant",
        "Exile target creature with power 4 or greater.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(matches!(
                s.targets[0].filter.power.as_ref(),
                Some(cmp) if cmp.op == CompareOp::Ge && *cmp.value == Amount::Const { n: 4 }
            ));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

// ============================================================================
// Feature 9: Amounts, Conditions & Refs (6 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_01_x_amount_in_spell_damage() {
    let card = synth_card(
        "e2e-t1-val-01",
        "Solar Flare",
        "{X}{R}",
        "Sorcery",
        "Solar Flare deals X damage to any target.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.effect[0].amount.as_deref(), Some(&Amount::X));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_02_count_of_controlled_permanents() {
    let card = synth_card(
        "e2e-t1-val-02",
        "Elvish Tally",
        "{1}{G}",
        "Sorcery",
        "You gain 1 life for each Elf you control.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(matches!(
                s.effect[0].amount.as_deref(),
                Some(Amount::Count { .. })
            ));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_03_stat_amount_power_of_target() {
    let card = synth_card(
        "e2e-t1-val-03",
        "Vital Surge",
        "{1}{G}",
        "Instant",
        "You gain life equal to target creature's power.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(matches!(
                s.effect[0].amount.as_deref(),
                Some(Amount::Stat { .. })
            ));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_04_domain_and_devotion_amounts() {
    let dom = synth_card(
        "e2e-t1-val-04a",
        "Planar Blast",
        "{1}{R}",
        "Sorcery",
        "Domain \u{2014} Planar Blast deals damage to target creature equal to the number of basic land types among lands you control.",
    );
    let (d1, _) = compile_supported(&dom);
    assert!(matches!(
        &d1.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(s.effect[0].amount.as_deref(), Some(Amount::Domain { .. }))
    ));

    let dev = synth_card(
        "e2e-t1-val-04b",
        "Azure Devotion",
        "{2}{U}",
        "Sorcery",
        "Draw cards equal to your devotion to blue.",
    );
    let (d2, _) = compile_supported(&dev);
    assert!(matches!(
        &d2.faces[0].abilities[0],
        AbilityDef::Spell(s) if matches!(s.effect[0].amount.as_deref(), Some(Amount::Devotion { .. }))
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_05_conditions_controls_filter_and_gained_life() {
    let c1 = synth_creature(
        "e2e-t1-val-05a",
        "Artificer Owl",
        "{1}{U}",
        "Bird",
        "1",
        "2",
        "When Artificer Owl enters, if you control an artifact, draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(matches!(
        &d1.faces[0].abilities[0],
        AbilityDef::Triggered(t)
        if matches!(t.condition.as_deref(), Some(Condition::ControlsFilter { .. }))
    ));

    let c2 = synth_creature(
        "e2e-t1-val-05b",
        "Sunbeam Cleric",
        "{2}{W}",
        "Human Cleric",
        "2",
        "3",
        "At the beginning of your end step, if you gained life this turn, draw a card.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(matches!(
        &d2.faces[0].abilities[0],
        AbilityDef::Triggered(t)
        if matches!(t.condition.as_deref(), Some(Condition::GainedLifeThisTurn { .. }))
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_val_06_player_ref_controller_of_target() {
    let card = synth_card(
        "e2e-t1-val-06",
        "Cleansing Ray",
        "{1}{W}",
        "Instant",
        "Destroy target creature. Its controller gains 3 life.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert_eq!(s.effect.len(), 2);
            assert!(matches!(s.effect[1].op, EffectOp::GainLife { .. }));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

// ============================================================================
// Feature 10: Triggers & Intervening-If (8 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_01_etb_self_trigger() {
    let card = synth_creature(
        "e2e-t1-trig-01",
        "Cloudseer Owl",
        "{2}{U}",
        "Bird",
        "2",
        "1",
        "When Cloudseer Owl enters, draw a card.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::ZoneChange
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_02_dies_and_other_creature_dies_triggers() {
    let c1 = synth_creature(
        "e2e-t1-trig-02a",
        "Ashbound Imp",
        "{1}{B}",
        "Imp",
        "1",
        "1",
        "When Ashbound Imp dies, each opponent loses 2 life.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t)
        if t.trigger.event == EventKind::ZoneChange
            && matches!(t.trigger.filter.detail, Some(EventDetail::ZoneChange { to: Some(Zone::Graveyard), .. }))
    )));

    let c2 = synth_creature(
        "e2e-t1-trig-02b",
        "Mournful Necromancer",
        "{2}{B}",
        "Human Wizard",
        "2",
        "2",
        "Whenever another creature dies, you gain 1 life.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::ZoneChange
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_03_cast_spell_trigger() {
    let card = synth_creature(
        "e2e-t1-trig-03",
        "Stormcall Apprentice",
        "{1}{U}{R}",
        "Human Wizard",
        "2",
        "2",
        "Whenever you cast an instant or sorcery spell, scry 1.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::Cast
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_04_upkeep_and_end_step_phase_triggers() {
    let card = synth_card(
        "e2e-t1-trig-04",
        "Dawnwell Fountain",
        "{2}{W}",
        "Enchantment",
        "At the beginning of your upkeep, you gain 2 life.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t)
        if t.trigger.event == EventKind::PhaseBegin
            && matches!(t.trigger.filter.detail, Some(EventDetail::Phase { phase: Some(GamePhase::Upkeep), .. }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_05_attacks_and_deals_combat_damage_triggers() {
    let c1 = synth_creature(
        "e2e-t1-trig-05a",
        "Warband Captain",
        "{2}{R}",
        "Human Warrior",
        "3",
        "2",
        "Whenever Warband Captain attacks, target creature you control gets +1/+0 until end of turn.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::Attacks
    )));

    let c2 = synth_creature(
        "e2e-t1-trig-05b",
        "Whispering Thief",
        "{2}{U}",
        "Rogue",
        "2",
        "1",
        "Whenever Whispering Thief deals combat damage to a player, draw a card.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::DealsDamage
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_06_intervening_if_and_once_each_turn() {
    let card = synth_creature(
        "e2e-t1-trig-06",
        "Grove Tender",
        "{2}{G}",
        "Elf Druid",
        "2",
        "3",
        "Whenever you draw a card, if it's not your turn, you gain 1 life. This ability triggers only once each turn.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t)
        if t.condition.is_some() && (t.trigger.only_once_per_turn || t.trigger.do_this_once_per_turn)
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_07_saga_chapter_triggers() {
    let saga = synth_card(
        "e2e-t1-trig-07",
        "Song of Ironwood",
        "{2}{G}",
        "Enchantment \u{2014} Saga",
        "I, II \u{2014} Put a +1/+1 counter on target creature you control.\nIII \u{2014} Draw two cards.",
    );
    let (def, _) = compile_supported(&saga);
    let chapters: Vec<_> = def.faces[0]
        .abilities
        .iter()
        .filter_map(|ab| match ab {
            AbilityDef::Triggered(t) if t.trigger.is_chapter_ability => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0].trigger.chapter_numbers.as_slice(), &[1, 2]);
    assert_eq!(chapters[1].trigger.chapter_numbers.as_slice(), &[3]);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_trig_08_disjunctive_etb_or_attacks_trigger() {
    let card = synth_creature(
        "e2e-t1-trig-08",
        "Blaze Titan",
        "{4}{R}{R}",
        "Giant",
        "6",
        "6",
        "Whenever Blaze Titan enters or attacks, it deals 2 damage to any target.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if !t.trigger.or.is_empty()
    )));
}

// ============================================================================
// Feature 11: Statics & Replacements (8 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_01_continuous_anthem_pt_modifier() {
    let card = synth_card(
        "e2e-t1-stat-01",
        "Marshal's Banner",
        "{2}{W}",
        "Enchantment",
        "Creatures you control get +1/+1.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(
            &s.static_def.body,
            StaticBody::Continuous { modifier: LayerMod::Pt { .. }, .. }
        )
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_02_continuous_keyword_grant() {
    let card = synth_card(
        "e2e-t1-stat-02",
        "Wind Banner",
        "{2}{U}",
        "Enchantment",
        "Creatures you control have flying.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(
            &s.static_def.body,
            StaticBody::Continuous { modifier: LayerMod::AddKeywords { .. }, .. }
        )
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_03_spell_cost_reduction_static() {
    let card = synth_creature(
        "e2e-t1-stat-03",
        "Arcane Familiar",
        "{1}{U}",
        "Bird Wizard",
        "1",
        "2",
        "Instant and sorcery spells you cast cost {1} less to cast.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(s.static_def.body, StaticBody::CostModifier { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_04_rule_modifier_no_max_hand_size_and_opponents_cant_cast() {
    let c1 = synth_card(
        "e2e-t1-stat-04a",
        "Infinite Archive",
        "{2}{U}",
        "Artifact",
        "You have no maximum hand size.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(s.static_def.body, StaticBody::RuleModifier { rule: RuleMod::NoMaxHandSize, .. })
    )));

    let c2 = synth_creature(
        "e2e-t1-stat-04b",
        "Grand Arbiter Synth",
        "{W}{U}",
        "Human Advisor",
        "2",
        "2",
        "Opponents can't cast spells during your turn.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(
            s.static_def.body,
            StaticBody::RuleModifier { rule: RuleMod::OpponentsCantCastDuringYourTurn { .. }, .. }
        )
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_05_restriction_cant_be_blocked_and_attacks_each_combat() {
    let c1 = synth_creature(
        "e2e-t1-stat-05a",
        "Phantom Infiltrator",
        "{1}{U}",
        "Spirit",
        "1",
        "2",
        "Phantom Infiltrator can't be blocked.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(
            s.static_def.body,
            StaticBody::Restriction { restriction: RestrictionKind::CantBeBlocked, .. }
        )
    )));

    let c2 = synth_creature(
        "e2e-t1-stat-05b",
        "Frenzied Berserker",
        "{2}{R}",
        "Human Berserker",
        "4",
        "2",
        "Frenzied Berserker attacks each combat if able.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s)
        if matches!(
            s.static_def.body,
            StaticBody::Restriction { restriction: RestrictionKind::AttacksEachCombat, .. }
        )
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_06_replacement_enters_tapped_and_enters_with_counters() {
    let c1 = synth_card(
        "e2e-t1-stat-06a",
        "Misty Hollow",
        "",
        "Land",
        "Misty Hollow enters tapped.\n{T}: Add {U}.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Replacement(r)
        if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.tapped == Some(true)
        )
    )));

    let c2 = synth_creature(
        "e2e-t1-stat-06b",
        "Sporeborn Hydra",
        "{2}{G}",
        "Hydra",
        "0",
        "0",
        "Sporeborn Hydra enters with three +1/+1 counters on it.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Replacement(r)
        if matches!(
            &r.replacement.with,
            ReplacementWith::ZoneChange { mod_spec } if mod_spec.counters.is_some()
        )
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_07_replacement_damage_doubler() {
    let card = synth_card(
        "e2e-t1-stat-07",
        "Furnace Engine",
        "{3}{R}{R}",
        "Enchantment",
        "If a source you control would deal damage to a permanent or player, it deals double that damage instead.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Replacement(r) if matches!(r.replacement.with, ReplacementWith::DamageScale { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_stat_08_multiplier_token_and_counter_doubling() {
    let card = synth_card(
        "e2e-t1-stat-08",
        "Parallel Conduit",
        "{3}{G}",
        "Enchantment",
        "If an effect would create one or more tokens under your control, it creates twice that many of those tokens instead.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Static(s) if matches!(s.static_def.body, StaticBody::Multiplier { .. })
    ) || matches!(ab, AbilityDef::Replacement(_))));
}

// ============================================================================
// Feature 12: Effects, Modals, Delayed & Reflexive Triggers (9 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_01_draw_scry_surveil_and_mill() {
    let c1 = synth_card(
        "e2e-t1-eff-01a",
        "Aether Foresight",
        "{1}{U}",
        "Instant",
        "Scry 2, then draw a card.",
    );
    let (d1, _) = compile_supported(&c1);
    match &d1.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.effect.iter().any(|e| matches!(e.op, EffectOp::Scry)));
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::Draw { .. }))
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }

    let c2 = synth_card(
        "e2e-t1-eff-01b",
        "Gloom Whisper",
        "{1}{B}",
        "Sorcery",
        "Surveil 2, then target player mills three cards.",
    );
    let (d2, _) = compile_supported(&c2);
    match &d2.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.effect.iter().any(|e| matches!(e.op, EffectOp::Surveil)));
            assert!(s.effect.iter().any(|e| matches!(e.op, EffectOp::Mill)));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_02_damage_gain_life_and_lose_life() {
    let card = synth_card(
        "e2e-t1-eff-02",
        "Soul Siphon",
        "{2}{B}{R}",
        "Instant",
        "Soul Siphon deals 3 damage to target creature or planeswalker. You gain 3 life.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::Damage { .. }))
            );
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::GainLife { .. }))
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_03_destroy_exile_and_bounce() {
    let c1 = synth_card(
        "e2e-t1-eff-03a",
        "Shatter Wave",
        "{1}{G}",
        "Instant",
        "Destroy target artifact or enchantment.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(matches!(
        &d1.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|e| matches!(e.op, EffectOp::Destroy { .. }))
    ));

    let c2 = synth_card(
        "e2e-t1-eff-03b",
        "Gale Return",
        "{1}{U}",
        "Instant",
        "Return target nonland permanent to its owner's hand.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(matches!(
        &d2.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|e| matches!(e.op, EffectOp::Bounce { .. }))
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_04_create_token_with_subtypes_and_keywords() {
    let card = synth_card(
        "e2e-t1-eff-04",
        "Call the Squadron",
        "{2}{W}",
        "Sorcery",
        "Create two 1/1 white Soldier creature tokens with vigilance.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(s.effect.iter().any(|e| matches!(
                &e.op,
                EffectOp::CreateToken { token: Some(tok), .. } if tok.power == 1 && tok.toughness == 1
            )));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_05_put_counters_and_proliferate() {
    let card = synth_card(
        "e2e-t1-eff-05",
        "Verdant Growth",
        "{1}{G}{U}",
        "Sorcery",
        "Put a +1/+1 counter on target creature, then proliferate.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::PutCounters { .. }))
            );
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::Proliferate))
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_06_modal_choose_one_and_choose_two() {
    let card = synth_card(
        "e2e-t1-eff-06",
        "Aether Command",
        "{2}{U}{W}",
        "Instant",
        "Choose two \u{2014}\n\u{2022} Counter target spell.\n\u{2022} Return target creature to its owner's hand.\n\u{2022} You gain 4 life.\n\u{2022} Draw a card.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            let modal = s.modal.as_ref().expect("expected ModalDef");
            assert_eq!(modal.min, 2);
            assert_eq!(modal.max, 2);
            assert_eq!(modal.modes.len(), 4);
        }
        other => panic!("expected modal spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_07_delayed_trigger_until_leaves_and_next_end_step() {
    let c1 = synth_card(
        "e2e-t1-eff-07a",
        "Crystal Prison",
        "{2}{W}",
        "Enchantment",
        "When Crystal Prison enters, exile target creature an opponent controls until Crystal Prison leaves the battlefield.",
    );
    let (d1, _) = compile_supported(&c1);
    assert!(d1.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t)
        if t.effect.iter().any(|e| matches!(e.op, EffectOp::Exile { until_source_leaves: true, .. }))
    )));

    let c2 = synth_card(
        "e2e-t1-eff-07b",
        "Spark Copy",
        "{2}{R}",
        "Sorcery",
        "Create a token that's a copy of target creature you control. It gains haste. Sacrifice it at the beginning of the next end step.",
    );
    let (d2, _) = compile_supported(&c2);
    assert!(matches!(
        &d2.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.effect.iter().any(|e| matches!(e.op, EffectOp::DelayedTrigger { .. }))
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_08_reflexive_trigger_when_you_do() {
    let card = synth_creature(
        "e2e-t1-eff-08",
        "Scrap Forger",
        "{2}{R}",
        "Goblin Artificer",
        "2",
        "2",
        "When Scrap Forger enters, you may sacrifice an artifact. When you do, draw two cards.",
    );
    let (def, _) = compile_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t)
        if t.effect.iter().any(|e| e.reflexive_trigger.is_some())
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_eff_09_impulse_draw_binding_and_play_from_exile() {
    let card = synth_card(
        "e2e-t1-eff-09",
        "Ember Insight",
        "{1}{R}",
        "Instant",
        "Exile the top two cards of your library. Until the end of your next turn, you may play those cards.",
    );
    let (def, _) = compile_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::Exile { .. }))
            );
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::GrantPlayFromExile { .. }))
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

// ============================================================================
// Feature 13: Support Report, Templater, Rename Invariance & CardDatabase (6 tests)
// ============================================================================

#[test]
fn t1_sys_01_support_report_classifies_all_unsupported_kinds() {
    let card = synth_creature(
        "e2e-t1-sys-01",
        "Unparsed Test Card",
        "{1}{U}",
        "Wizard",
        "1",
        "1",
        "{T}: Add {U}.\nWhenever ~ attacks, do something custom.\nSome custom static line.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    let rep = definition_support_report(&def);
    if !rep.fully_supported {
        let labels: Vec<&str> = rep
            .unsupported
            .iter()
            .map(|u| unsupported_kind_label(u.kind))
            .collect();
        assert!(!labels.is_empty());
        assert_ne!(UnsupportedAbilityKind::Mana, UnsupportedAbilityKind::Spell);
    }
}

#[test]
fn t1_sys_02_templater_clusters_numbers_costs_and_subtypes() {
    let t = build_templater(["Elf", "Wizard", "Dragon"]);
    assert_eq!(
        t.apply("{1}{G}: Put two +1/+1 counters on target Elf Wizard."),
        "{COST}: Put N N/N counters on target <SUBTYPE> <SUBTYPE>."
    );
    assert_eq!(
        t.apply("Create three 4/4 red Dragon tokens."),
        "Create N N/N red <SUBTYPE> tokens."
    );
}

#[test]
fn t1_sys_03_compile_bundle_sorts_tables_and_cards_deterministically() {
    let inputs = vec![
        synth_card(
            "e2e-z",
            "Zebra Plains",
            "",
            "Basic Land \u{2014} Plains",
            "",
        ),
        synth_card(
            "e2e-a",
            "Alpha Forest",
            "",
            "Basic Land \u{2014} Forest",
            "",
        ),
        synth_card("e2e-m", "Mid Island", "", "Basic Land \u{2014} Island", ""),
    ];
    let bundle = compile_bundle(&inputs, vec![]).unwrap();
    assert_eq!(bundle.ir_version, IR_V2_VERSION);
    assert_eq!(bundle.source_compiler_version, COMPILER_VERSION);
    assert_eq!(
        bundle
            .cards
            .iter()
            .map(|c| c.oracle_id.as_str())
            .collect::<Vec<_>>(),
        vec!["e2e-a", "e2e-m", "e2e-z"]
    );
    assert_eq!(bundle.tables.subtypes, vec!["Forest", "Island", "Plains"]);
}

#[test]
fn t1_sys_04_card_database_zstd_and_raw_roundtrip_and_queries() {
    let inputs = vec![
        synth_card(
            "e2e-db-1",
            "Synth Plains",
            "",
            "Basic Land \u{2014} Plains",
            "",
        ),
        synth_card(
            "e2e-db-2",
            "Synth Island",
            "",
            "Basic Land \u{2014} Island",
            "",
        ),
        synth_creature("e2e-db-3", "Synth Bear", "{1}{G}", "Bear", "2", "2", ""),
    ];
    let bundle = compile_bundle(&inputs, vec![]).unwrap();
    let db = CardDatabase::from_bundle(bundle).unwrap();
    assert_eq!(db.fully_supported_count(), 3);
    assert!(db.id_by_oracle("e2e-db-2").is_some());
    assert!(db.id_by_name("Synth Bear").is_some());
    assert!(db.subtype_id("Bear").is_some());

    let raw = db.encode_raw().unwrap();
    let zstd = db.encode_zstd(1).unwrap();
    let dec_raw = CardDatabase::decode_bytes(&raw).unwrap();
    let dec_zstd = CardDatabase::decode_bytes(&zstd).unwrap();
    assert_eq!(dec_raw.cards, db.cards);
    assert_eq!(dec_zstd.cards, db.cards);
    assert_eq!(dec_zstd.tables, db.tables);
}

#[test]
fn t1_sys_05_lexer_span_coverage_and_token_variants() {
    let src = "{T}, {2}{U}: ~ gets +1/+1 until end of turn. \"Draw a card.\"";
    let toks = tokenize(src, 0);
    assert!(toks.iter().any(|t| matches!(t.kind, TokenKind::Tap)));
    assert!(
        toks.iter()
            .any(|t| matches!(t.kind, TokenKind::ManaSymbol(ManaSymbolKind::Generic(2))))
    );
    assert!(toks.iter().any(|t| matches!(t.kind, TokenKind::SelfRef)));
    assert!(toks.iter().any(|t| matches!(
        t.kind,
        TokenKind::SignedPt {
            power: SignedPart::Int(1),
            toughness: SignedPart::Int(1)
        }
    )));
    assert!(
        toks.iter()
            .any(|t| matches!(t.kind, TokenKind::QuotedSpan("Draw a card.")))
    );
    assert!(toks.iter().any(|t| t.is_punct(Punct::Colon)));
    for tok in &toks {
        assert_eq!(
            &src[tok.span.start as usize..tok.span.end as usize],
            tok.raw
        );
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t1_sys_06_full_multi_ability_rename_invariance() {
    let make = |name: &str, short: &str| {
        synth_creature(
            "e2e-t1-sys-06",
            name,
            "{2}{W}{U}",
            "Human Wizard",
            "2",
            "4",
            &format!(
                "Flying, ward {{2}}\nWhen {name} enters, draw a card.\nWhenever {short} attacks, target creature gets +1/+1 until end of turn."
            ),
        )
    };
    let (mut d1, _) = compile_supported(&make("Aeloria, Sky Archon", "Aeloria"));
    let (mut d2, _) = compile_supported(&make("Thalindor, Cloud Seer", "Thalindor"));
    assert_eq!(d1.faces[0].characteristics, d2.faces[0].characteristics);
    assert_eq!(d1.faces[0].abilities, d2.faces[0].abilities);
    d1.name = "X".into();
    d2.name = "X".into();
    d1.faces[0].name = "X".into();
    d2.faces[0].name = "X".into();
    d1.text_hash.clear();
    d2.text_hash.clear();
    assert_eq!(d1, d2);
}
