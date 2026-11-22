use std::collections::HashSet;

use mtg_compiler::overrides::{OVERRIDES, get_or_parse};
use mtg_compiler::{
    COMPILER_VERSION, IdRemapper, Interner, ManaSymbolKind, Punct, RawCardInput, RawFaceInput,
    ScryfallJsonCard, ScryfallJsonFace, SignedPart, TokenKind, UnsupportedAbilityKind,
    build_characteristics, build_templater, compile_bundle, compile_card,
    definition_support_report, derive_colors_from_mana_cost, effective_layout, hash_text,
    is_ability_or_flavor_word, is_non_game_layout, normalize_counter_name, normalize_face,
    normalize_scryfall_card, remap_card_ids, replace_self_references, resolve_override, self_names,
    source_text, split_quoted_sentence_endings, split_sentences, strip_reminder_text, tokenize,
    unsupported_kind_label,
};
use mtg_ir::{
    AbilityDef, Amount, CardDatabase, CardLayout, CardTypeMask, ColorMask, Condition, CostDef,
    CounterTarget, EffectOp, EffectStep, IR_V2_VERSION, ManaColor, ObjectFilter, ObjectPredicate,
    PrintedPt, Selector, Span, SubtypeId, SupertypeMask, UnsupportedAbility, UnsupportedReason,
};
use smallvec::smallvec;

#[test]
fn test_utf16_hash_text_and_source_text() {
    // Verify FNV-1a over UTF-16 code units matches JS charCodeAt behavior on non-ASCII chars.
    let ascii_hash = hash_text("Landfall — Whenever a land enters");
    assert_eq!(ascii_hash.len(), 8);
    assert!(
        ascii_hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    // Two strings differing only in non-ASCII codepoints produce distinct hashes
    assert_ne!(hash_text("—"), hash_text("-"));
    assert_ne!(hash_text("• Gain 1 life"), hash_text("* Gain 1 life"));
    assert_ne!(hash_text("∞"), hash_text("inf"));

    // Verify source_text omits top-level defense and joins faces with \u{0001}/\u{0002}
    let mut card = RawCardInput {
        oracle_id: "syn-1".into(),
        name: "Synthetic Card".into(),
        layout: Some("normal".into()),
        mana_cost: "{1}{U}".into(),
        type_line: "Creature — Wizard".into(),
        oracle_text: "Flying".into(),
        power: Some("2".into()),
        toughness: Some("1".into()),
        loyalty: None,
        defense: Some("5".into()),
        colors: vec!["U".into()],
        color_indicator: None,
        faces: vec![RawFaceInput {
            name: "Synthetic Card".into(),
            mana_cost: "{1}{U}".into(),
            type_line: "Creature — Wizard".into(),
            oracle_text: "Flying".into(),
            power: Some("2".into()),
            toughness: Some("1".into()),
            loyalty: None,
            defense: Some("5".into()),
            colors: vec!["U".into()],
            color_indicator: None,
            keywords: vec!["Flying".into()],
            cmc: Some(2.0),
        }],
        back_face: None,
    };
    let h1 = hash_text(&source_text(&card));
    card.defense = Some("9".into()); // top-level defense intentionally omitted in TS sourceText
    let h2 = hash_text(&source_text(&card));
    assert_eq!(h1, h2);

    card.faces[0].defense = Some("9".into()); // face defense IS included
    let h3 = hash_text(&source_text(&card));
    assert_ne!(h1, h3);
}

#[test]
fn test_scryfall_normalization_and_layout_clamp() {
    assert!(is_non_game_layout(Some("token")));
    assert!(is_non_game_layout(Some("art_series")));
    assert!(!is_non_game_layout(Some("saga")));
    assert!(!is_non_game_layout(Some("normal")));

    let raw = ScryfallJsonCard {
        oracle_id: Some("syn-saga-id".into()),
        name: Some("Synthetic Tale".into()),
        layout: Some("saga".into()), // must clamp to None in RawCardInput!
        mana_cost: Some("{2}{G}".into()),
        type_line: Some("Enchantment — Saga".into()),
        oracle_text: Some("I — Draw a card.".into()),
        colors: Some(vec!["G".into()]),
        card_faces: Some(vec![ScryfallJsonFace {
            name: Some("Synthetic Tale".into()),
            mana_cost: Some("{2}{G}".into()),
            type_line: Some("Enchantment — Saga".into()),
            oracle_text: Some("I — Draw a card.".into()),
            colors: None, // tier-2 fallback -> color_indicator -> tier-3 derive from mana_cost
            color_indicator: None,
            ..Default::default()
        }]),
        ..Default::default()
    };
    let norm = normalize_scryfall_card(&raw);
    assert_eq!(norm.layout, None);
    assert_eq!(effective_layout(&norm), CardLayout::Normal);
    assert_eq!(norm.faces[0].colors, vec!["G"]);
    assert_eq!(
        derive_colors_from_mana_cost("{2/W}{U/B}{G/P}"),
        vec!["W", "U", "B", "G"]
    );

    // Verify 3-tier color fallback preserves explicit empty colors []
    let raw_colorless = ScryfallJsonCard {
        name: Some("Colorless DFC".into()),
        layout: Some("transform".into()),
        card_faces: Some(vec![
            ScryfallJsonFace {
                name: Some("Front Face".into()),
                mana_cost: Some("{2}{U}".into()),
                colors: Some(vec![]), // explicitly empty (Devoid-like)
                ..Default::default()
            },
            ScryfallJsonFace {
                name: Some("Back Face".into()),
                mana_cost: Some("".into()),
                colors: None,
                color_indicator: Some(vec!["B".into()]),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let norm_dfc = normalize_scryfall_card(&raw_colorless);
    assert_eq!(norm_dfc.name, "Front Face");
    assert_eq!(norm_dfc.faces[0].colors, Vec::<String>::new());
    assert_eq!(norm_dfc.faces[1].colors, vec!["B"]);
    assert_eq!(effective_layout(&norm_dfc), CardLayout::Transform);
}

#[test]
fn test_build_characteristics_and_printed_pt() {
    let mut interner = Interner::new();
    let face = RawFaceInput {
        name: "Synthetic Golem".into(),
        mana_cost: "{3}{W}{U}".into(),
        type_line: "Legendary Artifact Creature — Golem Wizard".into(),
        oracle_text: String::new(),
        power: Some("*".into()),
        toughness: Some("4".into()),
        loyalty: None,
        defense: None,
        colors: vec!["W".into(), "U".into()],
        color_indicator: Some(vec![]),
        keywords: vec![],
        cmc: Some(5.0),
    };
    let ch = build_characteristics(&face, &mut interner);
    assert_eq!(ch.mana_cost.generic, 3);
    assert_eq!(ch.mana_cost.mana_value(), 5);
    assert!(ch.supertypes.contains(SupertypeMask::LEGENDARY));
    assert!(
        ch.types
            .contains(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
    );
    assert_eq!(ch.subtypes.len(), 2);
    assert_eq!(interner.subtype_name(ch.subtypes[0]), Some("Golem"));
    assert_eq!(interner.subtype_name(ch.subtypes[1]), Some("Wizard"));
    assert_eq!(ch.colors, ColorMask::W | ColorMask::U);
    assert_eq!(ch.color_indicator, None); // empty [] does NOT set color_indicator
    assert_eq!(
        ch.pt,
        Some(PrintedPt::Cda {
            power: None,
            toughness: Some(4)
        })
    );
}

#[test]
fn test_strip_reminder_text() {
    assert_eq!(
        strip_reminder_text(
            "Flying (This creature can't be blocked except by creatures with flying or reach.), haste"
        ),
        "Flying, haste"
    );
    assert_eq!(
        strip_reminder_text(
            "Ward {2} (Whenever this creature becomes the target (see rule 115), counter it unless paid.)"
        ),
        "Ward {2}"
    );
    assert_eq!(
        strip_reminder_text("Stray (unclosed text"),
        "Stray (unclosed text"
    );
}

#[test]
fn test_self_names_and_pronoun_normalization() {
    let names = self_names("Kaelen, Skybound Explorer", true, None);
    assert_eq!(names, vec!["Kaelen, Skybound Explorer", "Kaelen"]);

    let nick_names = self_names("Alexios von \"Iron\" Vane", false, None);
    assert!(nick_names.contains(&"Alexios Vane".to_string()));

    let rank_names = self_names("Captain Mira Vance", true, None);
    assert!(rank_names.contains(&"Captain Vance".to_string()));

    let roman_names = self_names("Seraphina II", true, None);
    assert!(roman_names.contains(&"Seraphina".to_string()));

    let title_non_char = self_names("The Wanderer II", true, None);
    assert!(!title_non_char.contains(&"The Wanderer".to_string()));

    // `named` guard & token-creation guard & pronoun normalization outside quotes
    let line = "When Kaelen enters, he creates a token named Kaelen and says \"He wins.\" Her power becomes 4 and exile her to hand.";
    let norm = replace_self_references(line, "Kaelen, Skybound Explorer", true, Some(line));
    assert_eq!(
        norm,
        "When ~ enters, it creates a token named Kaelen and says \"He wins.\" Its power becomes 4 and exile it to hand."
    );
}

#[test]
fn test_ability_words_and_sentence_splitting() {
    assert!(is_ability_or_flavor_word("Landfall"));
    assert!(is_ability_or_flavor_word("Keen Sight"));
    assert!(!is_ability_or_flavor_word("I, II"));
    assert!(!is_ability_or_flavor_word("Flying"));
    assert!(!is_ability_or_flavor_word("Ward {2}"));
    assert!(!is_ability_or_flavor_word("Basic landcycling {2}"));

    let face = normalize_face(
        "Synthetic Explorer",
        "Landfall — Whenever a land enters, this creature gains \"When ~ attacks, draw a card.\" Scry 1.",
        false,
    );
    assert_eq!(face.lines.len(), 1);
    assert_eq!(face.lines[0].ability_word.as_deref(), Some("Landfall"));
    let split = split_quoted_sentence_endings(&face.lines[0].sentences);
    assert_eq!(split.len(), 2);
    assert_eq!(
        split[0].text,
        "Whenever a land enters, ~ gains \"When ~ attacks, draw a card.\""
    );
    assert_eq!(split[1].text, "Scry 1.");
    for s in &split {
        assert_eq!(
            &face.text[s.span.start as usize..s.span.end as usize],
            s.text
        );
    }

    let sents = split_sentences("Draw a card! Then discard a card.", 0);
    assert_eq!(sents.len(), 2);
    assert_eq!(sents[0].text, "Draw a card!");
    assert_eq!(sents[1].text, "Then discard a card.");
}

#[test]
fn test_rename_invariance_synthetic_card() {
    let make_card = |name: &str, text: &str| RawCardInput {
        oracle_id: "syn-rename-test".into(),
        name: name.into(),
        layout: Some("normal".into()),
        mana_cost: "{1}{W}".into(),
        type_line: "Legendary Creature — Human Knight".into(),
        oracle_text: text.into(),
        power: Some("2".into()),
        toughness: Some("2".into()),
        colors: vec!["W".into()],
        ..Default::default()
    };

    let c1 = make_card(
        "Aldric, Dawnblade",
        "When Aldric, Dawnblade enters, Aldric gains +1/+1. This creature has vigilance.",
    );
    let c2 = make_card(
        "Beren, Starshield",
        "When Beren, Starshield enters, Beren gains +1/+1. This creature has vigilance.",
    );
    let mut i1 = Interner::new();
    let mut i2 = Interner::new();
    let mut d1 = compile_card(&c1, &mut i1).unwrap();
    let mut d2 = compile_card(&c2, &mut i2).unwrap();
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

#[test]
fn test_lexer_tokens_and_byte_spans() {
    let text = "{T}, {Q}, {E}, {P}, {2/W}, {W/U/P}, {C/B}, {1000000}: ~'s +2/+2 vs 2/2, 0/+2, +X/-X, −2, twenty, ∞, \"Draw a card.\"";
    let tokens = tokenize(text, 0);
    for tok in &tokens {
        assert_eq!(
            &text[tok.span.start as usize..tok.span.end as usize],
            tok.raw,
            "byte span mismatch for {:?}",
            tok.kind
        );
    }
    assert!(matches!(tokens[0].kind, TokenKind::Tap));
    assert!(matches!(tokens[2].kind, TokenKind::Untap));
    assert!(matches!(tokens[4].kind, TokenKind::Energy));
    assert!(matches!(tokens[6].kind, TokenKind::Pawprint));
    assert!(matches!(
        tokens[8].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::TwoBrid(ManaColor::W))
    ));
    assert!(matches!(
        tokens[10].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::HybridPhyrexian(ManaColor::W, ManaColor::U))
    ));
    assert!(matches!(
        tokens[12].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::ColorlessHybrid(ManaColor::B))
    ));
    assert!(matches!(
        tokens[14].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::Generic(1_000_000))
    ));
    assert!(matches!(tokens[16].kind, TokenKind::SelfRef));
    assert_eq!(tokens[17].kind, TokenKind::Word("'s"));
    assert_eq!(
        tokens[18].kind,
        TokenKind::SignedPt {
            power: SignedPart::Int(2),
            toughness: SignedPart::Int(2)
        }
    );
    // 2/2 must NOT be SignedPt
    assert_eq!(tokens[20].kind, TokenKind::Number(2));
    assert!(tokens[21].is_punct(Punct::Slash));
    assert_eq!(tokens[22].kind, TokenKind::Number(2));

    assert_eq!(
        tokens[24].kind,
        TokenKind::SignedPt {
            power: SignedPart::Int(0),
            toughness: SignedPart::Int(2)
        }
    );
    assert_eq!(
        tokens[26].kind,
        TokenKind::SignedPt {
            power: SignedPart::PlusX,
            toughness: SignedPart::MinusX
        }
    );
    assert_eq!(tokens[28].kind, TokenKind::SignedNumber(-2));
    assert_eq!(tokens[30].kind, TokenKind::NumberWord(20));
    assert!(matches!(tokens[32].kind, TokenKind::Infinity));
    assert_eq!(tokens[34].kind, TokenKind::QuotedSpan("Draw a card."));
}

#[test]
fn test_all_38_overrides_validate_and_remap() {
    assert_eq!(OVERRIDES.len(), 38);
    let mut seen_oids = HashSet::new();

    for slot in &OVERRIDES {
        assert!(
            seen_oids.insert(slot.oracle_id),
            "duplicate override oracle_id {}",
            slot.oracle_id
        );
        let entry = get_or_parse(slot).unwrap();
        assert_eq!(entry.card.oracle_id, slot.oracle_id);
        assert_eq!(entry.card.text_hash, slot.expected_text_hash);
        assert!(entry.card.fully_supported);
        assert!(!entry.card.faces.is_empty());

        // Tables must be sorted and deduplicated
        let check_sorted_dedup = |v: &[String], kind: &str| {
            for w in v.windows(2) {
                assert!(
                    w[0] < w[1],
                    "unsorted/dup {kind} in {}: {:?}",
                    slot.oracle_id,
                    v
                );
            }
        };
        check_sorted_dedup(&entry.tables.subtypes, "subtype");
        check_sorted_dedup(&entry.tables.counters, "counter");
        check_sorted_dedup(&entry.tables.keywords, "keyword");

        let report = definition_support_report(&entry.card);
        assert!(report.fully_supported);
        assert!(report.unsupported.is_empty());

        for face in &entry.card.faces {
            assert!(!face.abilities.is_empty());
            for ab in &face.abilities {
                assert!(!matches!(ab, AbilityDef::Unsupported(_)));
            }
        }

        // Interner independence: empty vs pre-populated interner
        let mut empty_interner = Interner::new();
        let resolved_empty = resolve_override(
            slot.oracle_id,
            slot.expected_text_hash,
            &entry.card.name,
            &mut empty_interner,
        )
        .unwrap()
        .expect("override should resolve");
        assert_eq!(resolved_empty.compiler_version, COMPILER_VERSION);

        let mut dirty_interner = Interner::new();
        dirty_interner.intern_subtype("ZebraDummy");
        dirty_interner.intern_counter("zz_dummy");
        dirty_interner.intern_keyword("zz_kw");
        let resolved_dirty = resolve_override(
            slot.oracle_id,
            slot.expected_text_hash,
            "Renamed Synthetic",
            &mut dirty_interner,
        )
        .unwrap()
        .expect("override should resolve into dirty interner");
        assert_eq!(resolved_dirty.name, "Renamed Synthetic");
        if resolved_dirty.faces.len() == 1 {
            assert_eq!(resolved_dirty.faces[0].name, "Renamed Synthetic");
        }
        let dirty_tables = dirty_interner.tables();
        for st in &entry.tables.subtypes {
            assert!(dirty_tables.subtypes.contains(st));
        }
        for ct in &entry.tables.counters {
            assert!(dirty_tables.counters.contains(ct));
        }
        for kw in &entry.tables.keywords {
            assert!(dirty_tables.keywords.contains(kw));
        }

        // Mismatched hash triggers fallback Ok(None)
        let bad_hash = if slot.expected_text_hash == "00000000" {
            "ffffffff"
        } else {
            "00000000"
        };
        let miss = resolve_override(
            slot.oracle_id,
            bad_hash,
            &entry.card.name,
            &mut empty_interner,
        )
        .unwrap();
        assert!(miss.is_none());
    }

    // Verify compile_card falls back to normal compilation when card text is mutated
    let slot0 = &OVERRIDES[0];
    let mutated = RawCardInput {
        oracle_id: slot0.oracle_id.into(),
        name: "Mutated Override Test".into(),
        layout: Some("normal".into()),
        mana_cost: "{2}{B}".into(),
        type_line: "Creature — Bat".into(),
        oracle_text: "Mutated oracle text.".into(),
        ..Default::default()
    };
    let mut interner = Interner::new();
    let compiled = compile_card(&mutated, &mut interner).unwrap();
    assert!(!compiled.fully_supported);
    assert!(matches!(
        compiled.faces[0].abilities[0],
        AbilityDef::Unsupported(_)
    ));
}

#[test]
fn test_interner_counter_norm_and_bundle_codec_roundtrip() {
    assert_eq!(normalize_counter_name("+1/+1"), Some("+1/+1".into()));
    assert_eq!(normalize_counter_name("-0/-1"), Some("-0/-1".into()));
    assert_eq!(
        normalize_counter_name("First strike counters"),
        Some("first strike".into())
    );
    assert_eq!(
        normalize_counter_name("charge counters"),
        Some("charge".into())
    );
    assert_eq!(normalize_counter_name("the"), None);
    assert_eq!(normalize_counter_name("creature"), None);
    assert_eq!(normalize_counter_name("x"), None);

    // Exhaustive remap check + CardDatabase binary roundtrip with all 38 overrides + synthetic cards
    let mut cards = Vec::new();
    let mut interner = Interner::new();
    for slot in &OVERRIDES {
        let entry = get_or_parse(slot).unwrap();
        let resolved = resolve_override(
            slot.oracle_id,
            slot.expected_text_hash,
            &entry.card.name,
            &mut interner,
        )
        .unwrap()
        .unwrap();
        cards.push(resolved);
    }

    // Add a synthetic basic land compiled from RawCardInput
    let syn_inputs = vec![
        RawCardInput {
            oracle_id: "syn-forest-1".into(),
            name: "Synthetic Forest".into(),
            layout: Some("normal".into()),
            type_line: "Basic Land — Forest".into(),
            ..Default::default()
        },
        RawCardInput {
            oracle_id: "syn-creature-1".into(),
            name: "Synthetic Bear".into(),
            layout: Some("normal".into()),
            mana_cost: "{1}{G}".into(),
            type_line: "Creature — Bear".into(),
            power: Some("2".into()),
            toughness: Some("2".into()),
            colors: vec!["G".into()],
            ..Default::default()
        },
    ];
    let mut bundle = compile_bundle(&syn_inputs, vec![]).unwrap();
    assert_eq!(bundle.ir_version, IR_V2_VERSION);
    assert_eq!(bundle.source_compiler_version, COMPILER_VERSION);
    assert_eq!(bundle.cards.len(), 2);
    assert!(bundle.cards.iter().all(|c| c.fully_supported));

    // Merge the 38 override cards into a bundle as well
    let (tables, remap) = interner.into_sorted_tables_and_remap().unwrap();
    for c in &mut cards {
        remap_card_ids(c, &mut remap.clone()).unwrap();
    }
    cards.sort_by(|a, b| {
        a.oracle_id
            .cmp(&b.oracle_id)
            .then_with(|| a.name.cmp(&b.name))
    });
    bundle = mtg_ir::IrV2Bundle {
        ir_version: IR_V2_VERSION,
        source_compiler_version: COMPILER_VERSION,
        tables,
        cards,
        decks: vec![],
    };

    let db = CardDatabase::from_bundle(bundle).unwrap();
    let raw_bytes = db.encode_raw().unwrap();
    let zstd_bytes = db.encode_zstd(1).unwrap();
    let dec_raw = CardDatabase::decode_bytes(&raw_bytes).unwrap();
    let dec_zstd = CardDatabase::decode_bytes(&zstd_bytes).unwrap();
    assert_eq!(dec_raw.cards, db.cards);
    assert_eq!(dec_zstd.cards, db.cards);
    assert_eq!(dec_raw.tables, db.tables);
    assert_eq!(dec_zstd.tables, db.tables);
}

#[test]
fn test_support_report_and_templater() {
    let templater = build_templater(["Elf", "Dwarf", "Wolf", "Bush"]);
    assert_eq!(
        templater.apply("{2}{G}: Create two 1/1 green Elf tokens."),
        "{COST}: Create N N/N green <SUBTYPE> tokens."
    );
    assert_eq!(
        templater.apply("Elves and Dwarves and Wolves and Bushes get +1/+1."),
        "<SUBTYPE> and <SUBTYPE> and <SUBTYPE> and <SUBTYPE> get N/N."
    );

    let card = mtg_ir::CardDef {
        oracle_id: "syn-sup".into(),
        compiler_version: COMPILER_VERSION,
        text_hash: "00000000".into(),
        name: "Synthetic".into(),
        layout: CardLayout::Normal,
        faces: vec![mtg_ir::FaceDef {
            name: "Synthetic".into(),
            characteristics: mtg_ir::Characteristics {
                mana_cost: Default::default(),
                types: CardTypeMask::CREATURE,
                subtypes: smallvec![],
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: None,
                loyalty: None,
                defense: None,
            },
            abilities: vec![
                AbilityDef::Unsupported(UnsupportedAbility {
                    reason: UnsupportedReason::Unparsed,
                    text: "{T}: Add {G}.".into(),
                    span: Span { start: 0, end: 12 },
                    unparsed: vec![],
                    id: None,
                    cost: vec![],
                    cost_complete: None,
                    cost_keyword: None,
                    timing: None,
                    is_mana: false,
                    effect: vec![],
                    targets: vec![],
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    trigger: None,
                    condition: None,
                    grant_condition: None,
                }),
                AbilityDef::Unsupported(UnsupportedAbility {
                    reason: UnsupportedReason::Unparsed,
                    text: "Whenever ~ attacks, draw a card.".into(),
                    span: Span { start: 13, end: 45 },
                    unparsed: vec![],
                    id: None,
                    cost: vec![],
                    cost_complete: None,
                    cost_keyword: None,
                    timing: None,
                    is_mana: false,
                    effect: vec![],
                    targets: vec![],
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    trigger: None,
                    condition: None,
                    grant_condition: None,
                }),
            ],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }],
        fully_supported: false,
    };

    let rep = definition_support_report(&card);
    assert!(!rep.fully_supported);
    assert_eq!(rep.unsupported.len(), 2);
    assert_eq!(rep.unsupported[0].kind, UnsupportedAbilityKind::Mana);
    assert_eq!(unsupported_kind_label(rep.unsupported[0].kind), "Mana");
    assert_eq!(
        rep.unsupported[1].kind,
        UnsupportedAbilityKind::TriggerUnrecognised
    );

    // Silence unused import checks if any
    let _ = (
        Amount::Const { n: 1 },
        Condition::IsYourTurn,
        CostDef::TapSelf,
        CounterTarget::Any,
        EffectStep {
            op: EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
            subject: Selector::None,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        },
        ObjectFilter::default(),
        ObjectPredicate::NotSelf,
        SubtypeId(0),
    );
    struct NoopRemap;
    impl IdRemapper for NoopRemap {
        fn remap_subtype(
            &mut self,
            id: SubtypeId,
        ) -> Result<SubtypeId, mtg_compiler::CompileError> {
            Ok(id)
        }
        fn remap_counter(
            &mut self,
            id: mtg_ir::CounterId,
        ) -> Result<mtg_ir::CounterId, mtg_compiler::CompileError> {
            Ok(id)
        }
        fn remap_keyword(
            &mut self,
            id: mtg_ir::KeywordId,
        ) -> Result<mtg_ir::KeywordId, mtg_compiler::CompileError> {
            Ok(id)
        }
    }
    let mut c_copy = card.clone();
    remap_card_ids(&mut c_copy, &mut NoopRemap).unwrap();
}
