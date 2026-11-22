use mtg_compiler::overrides::{OVERRIDES, get_or_parse};
use mtg_compiler::{
    Interner, ManaSymbolKind, RawCardInput, RawFaceInput, ScryfallJsonCard, TokenKind,
    build_characteristics, build_templater, compile_bundle, compile_card,
    definition_support_report, derive_colors_from_mana_cost, effective_layout, hash_text,
    is_ability_or_flavor_word, is_non_game_layout, normalize_counter_name, normalize_face,
    normalize_scryfall_card, parse_card_layout, replace_self_references, resolve_override,
    strip_reminder_text, tokenize,
};
use mtg_ir::{
    AbilityDef, CardDatabase, CardLayout, CardTypeMask, CostDef, FilterFlags, IrCodecError,
    ManaCost, PrintedPt, SupertypeMask,
};

fn make_raw(id: &str, name: &str, mana: &str, type_line: &str, text: &str) -> RawCardInput {
    RawCardInput {
        oracle_id: id.to_string(),
        name: name.to_string(),
        layout: Some("normal".to_string()),
        mana_cost: mana.to_string(),
        type_line: type_line.to_string(),
        oracle_text: text.to_string(),
        colors: derive_colors_from_mana_cost(mana),
        ..Default::default()
    }
}

// ============================================================================
// Boundary Group 1: Empty & Whitespace Inputs (5 tests)
// ============================================================================

#[test]
fn t2_empty_01_vanilla_creature_empty_oracle_text() {
    let mut card = make_raw(
        "e2e-t2-emp-01",
        "Vanilla Sentry",
        "{1}{W}",
        "Creature \u{2014} Soldier",
        "",
    );
    card.power = Some("2".into());
    card.toughness = Some("2".into());
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(def.faces[0].abilities.is_empty());
}

#[test]
fn t2_empty_02_whitespace_only_oracle_text() {
    let card = make_raw(
        "e2e-t2-emp-02",
        "Blank Relic",
        "{0}",
        "Artifact",
        "   \n\t  \r\n  ",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(def.faces[0].abilities.is_empty());
}

#[test]
fn t2_empty_03_empty_type_line_and_empty_mana_cost() {
    let card = make_raw("e2e-t2-emp-03", "Typeless Token", "", "", "");
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert_eq!(def.faces[0].characteristics.types, CardTypeMask::empty());
    assert!(def.faces[0].characteristics.mana_cost.is_empty());
    assert!(def.fully_supported);
}

#[test]
fn t2_empty_04_empty_reminder_parens_only() {
    assert_eq!(strip_reminder_text("()"), "");
    assert_eq!(strip_reminder_text("   (Just reminder text.)   "), "");
    let card = make_raw(
        "e2e-t2-emp-04",
        "Reminder Only Relic",
        "{1}",
        "Artifact",
        "(This is pure reminder text.)",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(def.faces[0].abilities.is_empty());
}

#[test]
fn t2_empty_05_empty_string_normalization_and_tokens() {
    let norm = normalize_face("Empty", "", false);
    assert!(norm.lines.is_empty());
    assert!(tokenize("", 0).is_empty());
    assert_eq!(hash_text("").len(), 8);
}

// ============================================================================
// Boundary Group 2: Unbalanced Parentheses & Quotes (5 tests)
// ============================================================================

#[test]
fn t2_paren_01_unclosed_open_paren_not_silently_dropped() {
    let s = "Flying (unclosed reminder";
    let stripped = strip_reminder_text(s);
    assert!(stripped.contains("unclosed reminder"));
}

#[test]
fn t2_paren_02_stray_closing_paren_preserved() {
    let s = "Flying) haste";
    let stripped = strip_reminder_text(s);
    assert_eq!(stripped, "Flying) haste");
}

#[test]
fn t2_paren_03_nested_unbalanced_parens_do_not_panic() {
    let s = "Ward {2} (outer (inner) still unclosed";
    let stripped = strip_reminder_text(s);
    assert!(!stripped.is_empty());
    let card = make_raw("e2e-t2-par-03", "Paren Stress", "{2}", "Artifact", s);
    let mut interner = Interner::new();
    let _ = compile_card(&card, &mut interner).unwrap();
}

#[test]
fn t2_paren_04_multiple_adjacent_paren_blocks() {
    let s = "Flying (rem 1) (rem 2), trample (rem 3)";
    assert_eq!(strip_reminder_text(s), "Flying, trample");
}

#[test]
fn t2_paren_05_unclosed_quote_in_lexer_does_not_panic() {
    let s = "Say \"unclosed quote text";
    let toks = tokenize(s, 0);
    assert!(!toks.is_empty());
    for tok in &toks {
        assert_eq!(&s[tok.span.start as usize..tok.span.end as usize], tok.raw);
    }
}

// ============================================================================
// Boundary Group 3: Max-Size & Overflow Numbers (5 tests)
// ============================================================================

#[test]
fn t2_num_01_large_generic_mana_cost_fits_u32_and_saturates() {
    let mc = ManaCost::parse("{1000000}");
    assert_eq!(mc.generic, 1_000_000);
    assert_eq!(mc.mana_value(), 1_000_000);

    let sat = ManaCost::parse("{4294967295}{5}");
    assert_eq!(sat.generic, u32::MAX);
}

#[test]
fn t2_num_02_large_mana_symbol_in_lexer() {
    let toks = tokenize("{1000000}: Add {C}.", 0);
    assert!(matches!(
        toks[0].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::Generic(1_000_000))
    ));
}

#[test]
fn t2_num_03_extreme_numeric_word_and_u32_max_token() {
    let toks = tokenize("4294967295 999999999999999", 0);
    assert_eq!(toks[0].kind, TokenKind::Number(u32::MAX));
    // Overflowing u32 should not panic; lexer falls back cleanly (Word or saturated)
    assert!(!toks[1].raw.is_empty());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_num_04_large_activated_mana_cost_ability() {
    let card = make_raw(
        "e2e-t2-num-04",
        "Omega Beacon",
        "{5}",
        "Artifact",
        "{1000000}: Draw a card.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Mana { cost, .. } if cost.generic == 1_000_000))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_num_05_zero_generic_mana_cost_spell() {
    let card = make_raw(
        "e2e-t2-num-05",
        "Zero Pact",
        "{0}",
        "Instant",
        "You gain 1 life.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert_eq!(def.faces[0].characteristics.mana_cost.mana_value(), 0);
}

// ============================================================================
// Boundary Group 4: Zero, Negative, and CDA P/T / Loyalty / Defense (7 tests)
// ============================================================================

#[test]
fn t2_pt_01_zero_zero_creature() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Zero Construct".into(),
        type_line: "Artifact Creature \u{2014} Construct".into(),
        power: Some("0".into()),
        toughness: Some("0".into()),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert_eq!(
        ch.pt,
        Some(PrintedPt::Fixed {
            power: 0,
            toughness: 0
        })
    );
}

#[test]
fn t2_pt_02_negative_power_and_toughness() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Hollow Shade".into(),
        type_line: "Creature \u{2014} Shade".into(),
        power: Some("-1".into()),
        toughness: Some("3".into()),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert_eq!(
        ch.pt,
        Some(PrintedPt::Fixed {
            power: -1,
            toughness: 3
        })
    );
}

#[test]
fn t2_pt_03_cda_star_star_and_half_or_question() {
    let mut interner = Interner::new();
    let face1 = RawFaceInput {
        name: "Star Elemental".into(),
        type_line: "Creature \u{2014} Elemental".into(),
        power: Some("*".into()),
        toughness: Some("*".into()),
        ..Default::default()
    };
    let ch1 = build_characteristics(&face1, &mut interner);
    assert_eq!(
        ch1.pt,
        Some(PrintedPt::Cda {
            power: None,
            toughness: None
        })
    );

    let face2 = RawFaceInput {
        name: "Mystery Beast".into(),
        type_line: "Creature \u{2014} Beast".into(),
        power: Some("?".into()),
        toughness: Some("2".into()),
        ..Default::default()
    };
    let ch2 = build_characteristics(&face2, &mut interner);
    assert_eq!(
        ch2.pt,
        Some(PrintedPt::Cda {
            power: None,
            toughness: Some(2)
        })
    );
}

#[test]
fn t2_pt_04_zero_loyalty_planeswalker_and_zero_defense_battle() {
    let mut interner = Interner::new();
    let pw = RawFaceInput {
        name: "Null Planeswalker".into(),
        type_line: "Legendary Planeswalker \u{2014} Null".into(),
        loyalty: Some("0".into()),
        ..Default::default()
    };
    let ch_pw = build_characteristics(&pw, &mut interner);
    assert_eq!(ch_pw.loyalty, Some(0));

    let bt = RawFaceInput {
        name: "Null Siege".into(),
        type_line: "Battle \u{2014} Siege".into(),
        defense: Some("0".into()),
        ..Default::default()
    };
    let ch_bt = build_characteristics(&bt, &mut interner);
    assert_eq!(ch_bt.defense, Some(0));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_pt_05_zero_and_large_negative_loyalty_abilities() {
    let mut card = make_raw(
        "e2e-t2-pt-05",
        "Zero-Base Walker",
        "{2}{U}",
        "Legendary Planeswalker \u{2014} Synth",
        "0: Scry 1.\n\u{2212}12: Draw seven cards.",
    );
    card.loyalty = Some("0".into());
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert_eq!(def.faces[0].characteristics.loyalty, Some(0));
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { delta: 0, .. }))
    )));
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Activated(a)
        if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { delta: -12, .. }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_pt_06_zero_zero_with_enters_with_counters_cda_like() {
    let mut card = make_raw(
        "e2e-t2-pt-06",
        "Clockwork Zero",
        "{X}",
        "Artifact Creature \u{2014} Construct",
        "Clockwork Zero enters with X +1/+1 counters on it.",
    );
    card.power = Some("0".into());
    card.toughness = Some("0".into());
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert_eq!(
        def.faces[0].characteristics.pt,
        Some(PrintedPt::Fixed {
            power: 0,
            toughness: 0
        })
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_pt_07_negative_pt_pump_signed_pt_effect() {
    let card = make_raw(
        "e2e-t2-pt-07",
        "Wither Bolt",
        "{B}",
        "Instant",
        "Target creature gets -0/-2 until end of turn.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
}

// ============================================================================
// Boundary Group 5: Non-ASCII UTF-16 & Special Characters (6 tests)
// ============================================================================

#[test]
fn t2_utf_01_utf16_hash_distinguishes_em_dash_minus_and_bullets() {
    assert_ne!(hash_text("\u{2014}"), hash_text("-")); // em-dash vs ASCII hyphen
    assert_ne!(hash_text("\u{2212}2"), hash_text("-2")); // Unicode minus vs ASCII hyphen
    assert_ne!(hash_text("\u{2022} Draw"), hash_text("* Draw")); // bullet
    assert_ne!(hash_text("Sera\u{2019}s"), hash_text("Sera's")); // curly apostrophe
}

#[test]
fn t2_utf_02_utf16_surrogate_pair_hash_and_lexer_no_panic() {
    let s = "Symbol \u{1F525} \u{221E} \u{2014} \u{2212}3 \u{201C}quoted\u{201D}";
    let h = hash_text(s);
    assert_eq!(h.len(), 8);
    let toks = tokenize(s, 0);
    for tok in &toks {
        assert_eq!(&s[tok.span.start as usize..tok.span.end as usize], tok.raw);
    }
}

#[test]
fn t2_utf_03_unicode_minus_in_signed_pt_and_loyalty() {
    let s = "+2/\u{2212}2 and \u{2212}3 and \u{2212}X/+0";
    let toks = tokenize(s, 0);
    assert!(
        toks.iter()
            .any(|t| matches!(t.kind, TokenKind::SignedNumber(-3)))
    );
    for tok in &toks {
        assert_eq!(&s[tok.span.start as usize..tok.span.end as usize], tok.raw);
    }
}

#[test]
fn t2_utf_04_accented_card_name_self_reference_normalization() {
    let text = "When D\u{00E9}j\u{00E0}, Cloud Mage enters, D\u{00E9}j\u{00E0} draws a card.";
    let norm = replace_self_references(text, "D\u{00E9}j\u{00E0}, Cloud Mage", true, Some(text));
    assert_eq!(norm, "When ~ enters, ~ draws a card.");
}

#[test]
fn t2_utf_05_smart_quotes_in_quoted_span_and_sentence_split() {
    let text = "Target creature gains \u{201C}When ~ dies, draw a card.\u{201D} Scry 1.";
    let face = normalize_face("Quote Spirit", text, false);
    assert_eq!(face.lines.len(), 1);
    let toks = tokenize(&face.lines[0].text, 0);
    assert!(!toks.is_empty());
}

#[test]
fn t2_utf_06_infinity_symbol_token_and_span() {
    let s = "Set your hand size to \u{221E}.";
    let toks = tokenize(s, 0);
    assert!(toks.iter().any(|t| matches!(t.kind, TokenKind::Infinity)));
    for tok in &toks {
        assert_eq!(&s[tok.span.start as usize..tok.span.end as usize], tok.raw);
    }
}

// ============================================================================
// Boundary Group 6: Unknown Types, Supertypes, Layouts & Symbols (5 tests)
// ============================================================================

#[test]
fn t2_unk_01_unknown_layout_clamps_to_none_and_infers_fallback() {
    assert_eq!(parse_card_layout(Some("unknown_xyz")), None);
    let scry = ScryfallJsonCard {
        oracle_id: Some("e2e-t2-unk-01".into()),
        name: Some("Strange Frame".into()),
        layout: Some("planechase_weird".into()),
        ..Default::default()
    };
    let norm = normalize_scryfall_card(&scry);
    assert_eq!(norm.layout, None);
    assert_eq!(effective_layout(&norm), CardLayout::Normal);
}

#[test]
fn t2_unk_02_non_game_layouts_negative_checks_and_none() {
    assert!(!is_non_game_layout(None));
    assert!(!is_non_game_layout(Some("")));
    assert!(!is_non_game_layout(Some("TOKEN"))); // case-sensitive Scryfall keys
    assert!(is_non_game_layout(Some("vanguard")));
}

#[test]
fn t2_unk_03_unknown_card_type_words_and_tribal_kindred_alias() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Weird Card".into(),
        type_line: "Tribal UnknownType Creature \u{2014} OddSubtype".into(),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert!(
        ch.types
            .contains(CardTypeMask::KINDRED | CardTypeMask::CREATURE)
    );
    assert!(!ch.types.contains(CardTypeMask::CONSPIRACY));
    assert_eq!(interner.subtype_name(ch.subtypes[0]), Some("OddSubtype"));
}

#[test]
fn t2_unk_04_conspiracy_not_in_permanent_mask() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Plot Scheme".into(),
        type_line: "Conspiracy".into(),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert!(ch.types.contains(CardTypeMask::CONSPIRACY));
    assert!(!ch.types.intersects(CardTypeMask::PERMANENT));
}

#[test]
fn t2_unk_05_unknown_mana_symbol_preserved_in_raw_and_lexer() {
    let mc = ManaCost::parse("{Q}{T}{?}{2}{W}");
    assert_eq!(mc.generic, 2);
    assert_eq!(mc.mana_value(), 3);
    assert_eq!(mc.raw, "{Q}{T}{?}{2}{W}");

    let toks = tokenize("{?}{Z}", 0);
    assert!(matches!(
        toks[0].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::Unknown)
    ));
}

// ============================================================================
// Boundary Group 7: Malformed / Unsupported Abilities Graceful Fallback (6 tests)
// ============================================================================

#[test]
fn t2_unsup_01_gibberish_oracle_text_never_panics_returns_unsupported() {
    let card = make_raw(
        "e2e-t2-uns-01",
        "Gibberish Construct",
        "{2}",
        "Artifact",
        "Blargle zorp quux 42 onto the hyperstack.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .all(|ab| matches!(ab, AbilityDef::Unsupported(_)))
    );
    let rep = definition_support_report(&def);
    assert!(!rep.fully_supported);
    assert_eq!(rep.unsupported.len(), 1);
}

#[test]
fn t2_unsup_02_unrecognized_trigger_does_not_panic() {
    let card = make_raw(
        "e2e-t2-uns-02",
        "Weird Trigger",
        "{1}{U}",
        "Enchantment",
        "Whenever a player sneezes, draw a quantum card.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert!(matches!(
        def.faces[0].abilities[0],
        AbilityDef::Unsupported(_)
    ));
}

#[test]
fn t2_unsup_03_unrecognized_activated_cost_or_effect_no_panic() {
    let card = make_raw(
        "e2e-t2-uns-03",
        "Weird Activated",
        "{2}",
        "Artifact",
        "{T}, Dance a jig: Teleport to Saturn.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert!(matches!(
        def.faces[0].abilities[0],
        AbilityDef::Unsupported(_)
    ));
}

#[test]
fn t2_unsup_04_unrecognized_keyword_never_panics() {
    let card = make_raw(
        "e2e-t2-uns-04",
        "Weird Keyword",
        "{2}{G}",
        "Creature \u{2014} Beast",
        "Megamegahorsemanship",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert!(matches!(
        def.faces[0].abilities[0],
        AbilityDef::Unsupported(_)
    ));
}

#[test]
fn t2_unsup_05_partial_punctuation_only_lines_handled_cleanly() {
    let card = make_raw(
        "e2e-t2-uns-05",
        "Punctuation Stress",
        "{1}",
        "Artifact",
        "::: ;; ... --- ///",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
}

#[test]
fn t2_unsup_06_mixed_supported_basic_land_and_unsupported_line() {
    let card = make_raw(
        "e2e-t2-uns-06",
        "Mystic Island",
        "",
        "Land \u{2014} Island",
        "Whenever a comet passes overhead, scry 99.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert_eq!(def.faces[0].abilities.len(), 2);
    assert!(matches!(
        def.faces[0].abilities[0],
        AbilityDef::Activated(_)
    ));
    assert!(matches!(
        def.faces[0].abilities[1],
        AbilityDef::Unsupported(_)
    ));
}

// ============================================================================
// Boundary Group 8: Duplicate Names, Overrides & Codec Rejection (10 tests)
// ============================================================================

#[test]
fn t2_dup_01_duplicate_card_names_in_bundle_and_card_database() {
    let inputs = vec![
        make_raw(
            "e2e-dup-oid-2",
            "Shared Synth Name",
            "",
            "Basic Land \u{2014} Island",
            "",
        ),
        make_raw(
            "e2e-dup-oid-1",
            "Shared Synth Name",
            "",
            "Basic Land \u{2014} Plains",
            "",
        ),
    ];
    let bundle = compile_bundle(&inputs, vec![]).unwrap();
    // compile_bundle sorts by (oracle_id, name), so oid-1 is index 0
    assert_eq!(bundle.cards[0].oracle_id, "e2e-dup-oid-1");
    assert_eq!(bundle.cards[1].oracle_id, "e2e-dup-oid-2");

    let db = CardDatabase::from_bundle(bundle).unwrap();
    // by_name is first-insertion-wins (index 0)
    assert_eq!(db.id_by_name("Shared Synth Name").unwrap().0, 0);
    assert_eq!(db.id_by_oracle("e2e-dup-oid-2").unwrap().0, 1);
}

#[test]
fn t2_ovr_01_override_fallback_when_mana_cost_mutated() {
    let slot = &OVERRIDES[3];
    let entry = get_or_parse(slot).unwrap();
    let card = make_raw(
        slot.oracle_id,
        &entry.card.name,
        "{9}{W}{U}{B}{R}{G}",
        "Artifact",
        "Some text",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
    assert_ne!(def.text_hash, slot.expected_text_hash);
}

#[test]
fn t2_ovr_02_override_fallback_when_oracle_text_mutated() {
    let slot = &OVERRIDES[4];
    let entry = get_or_parse(slot).unwrap();
    let card = make_raw(
        slot.oracle_id,
        &entry.card.name,
        &entry.card.faces[0].characteristics.mana_cost.raw,
        "Sorcery",
        "Completely altered synthetic text that mismatches text_hash.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(!def.fully_supported);
}

#[test]
fn t2_ovr_03_override_unknown_oracle_id_returns_none() {
    let mut interner = Interner::new();
    let res = resolve_override(
        "00000000-0000-0000-0000-000000000000",
        "abcd1234",
        "No Match",
        &mut interner,
    )
    .unwrap();
    assert!(res.is_none());
}

#[test]
fn t2_codec_01_empty_bundle_roundtrip_zero_chunks() {
    let bundle = compile_bundle(&[], vec![]).unwrap();
    let db = CardDatabase::from_bundle(bundle).unwrap();
    assert_eq!(db.cards.len(), 0);
    let raw = db.encode_raw().unwrap();
    let zstd = db.encode_zstd(1).unwrap();
    assert_eq!(CardDatabase::decode_bytes(&raw).unwrap().cards.len(), 0);
    assert_eq!(CardDatabase::decode_bytes(&zstd).unwrap().cards.len(), 0);
}

#[test]
fn t2_codec_02_invalid_magic_and_version_rejected() {
    assert!(matches!(
        CardDatabase::decode_bytes(b"BAD!12345"),
        Err(IrCodecError::InvalidMagic)
    ));
    let mut bad_ver = b"MTGC".to_vec();
    bad_ver.extend_from_slice(&99u32.to_le_bytes());
    bad_ver.push(0);
    assert!(matches!(
        CardDatabase::decode_bytes(&bad_ver),
        Err(IrCodecError::UnsupportedVersion(99))
    ));
}

#[test]
fn t2_codec_03_deny_unknown_fields_and_filter_flags_bit53_rejected() {
    let bad_json = r#"{"oracle_id":"x","compiler_version":46,"text_hash":"00000000","name":"X","layout":"normal","faces":[],"fully_supported":true,"unexpected_extra":1}"#;
    assert!(serde_json::from_str::<mtg_ir::CardDef>(bad_json).is_err());

    let bad_flags = (1u64 << 53).to_string();
    assert!(serde_json::from_str::<FilterFlags>(&bad_flags).is_err());
}

#[test]
fn t2_norm_07_counter_stopwords_and_roman_numeral_chapter_guard() {
    assert_eq!(normalize_counter_name("the"), None);
    assert_eq!(normalize_counter_name("x"), None);
    assert_eq!(normalize_counter_name("creature"), None);
    assert_eq!(normalize_counter_name("+1/+1"), Some("+1/+1".into()));
    assert!(!is_ability_or_flavor_word("I, II, III"));
    assert!(!is_ability_or_flavor_word("IV"));
}

#[test]
fn t2_norm_08_templater_empty_subtypes_and_no_matches() {
    let t = build_templater::<[&str; 0], &str>([]);
    assert_eq!(t.apply("Draw a card."), "Draw a card.");
    assert_eq!(t.apply("Deal 3 damage."), "Deal N damage.");
}

#[test]
fn t2_char_07_supertype_and_color_indicator_edge_cases() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "World Snow Ongoing".into(),
        type_line: "Basic Legendary Snow World Ongoing Enchantment".into(),
        color_indicator: Some(vec!["W".into(), "G".into()]),
        ..Default::default()
    };
    let ch = build_characteristics(&face, &mut interner);
    assert!(ch.supertypes.contains(
        SupertypeMask::BASIC
            | SupertypeMask::LEGENDARY
            | SupertypeMask::SNOW
            | SupertypeMask::WORLD
            | SupertypeMask::ONGOING
    ));
    assert_eq!(
        ch.color_indicator,
        Some(mtg_ir::ColorMask::W | mtg_ir::ColorMask::G)
    );
}

// ============================================================================
// Boundary Group 9: Grammar Edge Cases (5 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t2_gram_01_x_cant_be_zero_restriction() {
    let card = make_raw(
        "e2e-t2-gr-01",
        "Strict Flare",
        "{X}{R}",
        "Sorcery",
        "Strict Flare deals X damage to target creature. X can't be 0.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(matches!(
        &def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.x_cant_be_zero
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_gram_02_up_to_zero_targets_allowed() {
    let card = make_raw(
        "e2e-t2-gr-02",
        "Optional Freeze",
        "{U}",
        "Instant",
        "Tap up to one target creature.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(matches!(
        &def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.targets[0].min == 0 && s.targets[0].max == 1
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_gram_03_optional_you_may_effect_step() {
    let mut card = make_raw(
        "e2e-t2-gr-03",
        "Careful Seer",
        "{1}{U}",
        "Creature \u{2014} Merfolk",
        "When Careful Seer enters, you may draw a card.",
    );
    card.power = Some("1".into());
    card.toughness = Some("1".into());
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Triggered(t) if t.effect[0].optional.is_some()
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_gram_04_each_player_non_targeting_effect() {
    let card = make_raw(
        "e2e-t2-gr-04",
        "Shared Vision",
        "{1}{U}",
        "Sorcery",
        "Each player draws a card.",
    );
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert!(matches!(
        &def.faces[0].abilities[0],
        AbilityDef::Spell(s) if s.targets.is_empty()
    ));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t2_gram_05_multi_line_reminder_stripped_before_parsing() {
    let mut card = make_raw(
        "e2e-t2-gr-05",
        "Reminder Heavy Bird",
        "{2}{W}",
        "Creature \u{2014} Bird",
        "Flying (This creature can't be blocked except by creatures with flying or reach.)\nVigilance (Attacking doesn't cause this creature to tap.)\nFirst strike (This creature deals combat damage before creatures without first strike.)",
    );
    card.power = Some("2".into());
    card.toughness = Some("2".into());
    let mut interner = Interner::new();
    let def = compile_card(&card, &mut interner).unwrap();
    assert!(def.fully_supported);
    assert_eq!(def.faces[0].abilities.len(), 3);
}
