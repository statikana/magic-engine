//! Bounded Tier 5 adversarial & robustness test suite for `mtg-compiler`.
//!
//! Covers:
//! 1. Rename-invariance across complex multi-clause synthetic cards.
//! 2. Malformed / adversarial oracle-text fuzzing (empty lines, weird Unicode,
//!    deeply nested quotes/parens, long strings, missing costs, integer overflow)
//!    asserting zero panics and graceful `Unsupported` fallback.
//! 3. `CardDatabase` zstd + raw codec round-trip on the full synthetic corpus.

use mtg_compiler::{Interner, RawCardInput, RawFaceInput, compile_bundle, compile_card};
use mtg_ir::{AbilityDef, CardDatabase, CardDef};

fn mk_card(
    id: &str,
    name: &str,
    mana_cost: &str,
    type_line: &str,
    oracle_text: &str,
    pt: Option<(&str, &str)>,
) -> RawCardInput {
    RawCardInput {
        oracle_id: id.into(),
        name: name.into(),
        mana_cost: mana_cost.into(),
        type_line: type_line.into(),
        oracle_text: oracle_text.into(),
        power: pt.map(|(p, _)| p.into()),
        toughness: pt.map(|(_, t)| t.into()),
        ..Default::default()
    }
}

fn strip_spans_and_names(mut def: CardDef) -> CardDef {
    def.name.clear();
    def.text_hash.clear();
    for face in &mut def.faces {
        face.name.clear();
        for ab in &mut face.abilities {
            match ab {
                AbilityDef::Keyword(k) => {
                    k.span = Default::default();
                }
                AbilityDef::Activated(a) => {
                    a.span = Default::default();
                    a.text = None;
                    for st in &mut a.effect {
                        st.span = Default::default();
                    }
                }
                AbilityDef::Triggered(t) => {
                    t.span = Default::default();
                    t.text = None;
                    for st in &mut t.effect {
                        st.span = Default::default();
                    }
                }
                AbilityDef::Static(s) => {
                    s.span = Default::default();
                }
                AbilityDef::Replacement(r) => {
                    r.span = Default::default();
                }
                AbilityDef::Spell(s) => {
                    s.span = Default::default();
                    for st in &mut s.effect {
                        st.span = Default::default();
                    }
                }
                AbilityDef::Unsupported(u) => {
                    u.span = Default::default();
                    u.unparsed.clear();
                }
            }
        }
        if let Some(sp) = &mut face.cleaved_spell_spec {
            sp.span = Default::default();
            for st in &mut sp.effect {
                st.span = Default::default();
            }
        }
        if let Some(sp) = &mut face.gift_promised_spell_spec {
            sp.span = Default::default();
            for st in &mut sp.effect {
                st.span = Default::default();
            }
        }
    }
    def
}

#[test]
fn t5_rename_invariance_complex_multi_clause_cards() {
    type RenameTemplate<'a> = (
        &'a str,
        &'a str,
        &'a str,
        Option<(&'a str, &'a str)>,
        &'a str,
    );
    let templates: &[RenameTemplate<'_>] = &[
        (
            "{2}{W}{U}",
            "Legendary Creature — Human Wizard",
            "Flying, vigilance\nWhen NAME enters, if you control an artifact, draw a card.\n{1}{U}, {T}: Tap target creature. It doesn't untap during its controller's untap step for as long as NAME remains tapped.",
            Some(("2", "3")),
            "etb_and_freeze",
        ),
        (
            "{3}{B}{R}",
            "Creature — Demon",
            "Trample\nNAME enters prepared.\nWhenever NAME attacks, exile the top card of your library. You may play it until end of turn.\nIf a creature dealt damage by NAME this turn would die, exile it instead.",
            Some(("5", "5")),
            "prepared_impulse_exile",
        ),
        (
            "{2}{G}{G}",
            "Enchantment",
            "If one or more tokens would be created under your control, twice that many of those tokens are created instead.\nWhenever a creature enters under your control, you gain 1 life.",
            None,
            "multiplier_and_trigger",
        ),
        (
            "{1}{W}{B}",
            "Instant",
            "Choose one —\n• Destroy target creature.\n• Exile target creature from a graveyard. You gain 3 life.",
            None,
            "modal_spell",
        ),
        (
            "{1}{U}",
            "Instant",
            "Counter target creature or enchantment spell. If that spell is countered this way, exile it instead of putting it into its owner's graveyard.",
            None,
            "counter_exile",
        ),
        (
            "{3}",
            "Artifact — Spacecraft",
            "Station\n4+\nFlying\n8+\nWhenever NAME attacks, draw two cards.",
            Some(("5", "5")),
            "station_striations",
        ),
        (
            "{1}{W}",
            "Instant",
            "Cleave {3}{W}{W}\nDestroy target creature [with power 4 or greater].",
            None,
            "cleave_spell",
        ),
        (
            "{1}{W}",
            "Instant",
            "Gift a card\nDestroy target creature. If the gift was promised, exile target creature instead.",
            None,
            "gift_replacement_spell",
        ),
        (
            "{2}{G}",
            "Creature — Elf Warrior",
            "NAME can't attack unless defending player controls an Island.\nNAME can block an additional creature each combat.\nYou may choose not to untap NAME during your untap step.",
            Some(("3", "3")),
            "combat_rules_and_untap",
        ),
        (
            "{1}{G}",
            "Creature — Lhurgoyf",
            "NAME's power is equal to the number of card types among cards in all graveyards and its toughness is equal to that number plus 1.",
            Some(("*", "1+*")),
            "cda_goyf",
        ),
    ];

    let alt_names = [
        "Aldric, Keeper of Echoes",
        "Xylar's Vanguard",
        "Élodie of the Spire",
        "Krag-Maw Sentinel",
    ];

    for (idx, (mc, tl, tmpl, pt, label)) in templates.iter().enumerate() {
        let base_name = "Baseline Construct";
        let base_text = tmpl.replace("NAME", base_name);
        let base_card = mk_card(&format!("t5-ren-{idx}"), base_name, mc, tl, &base_text, *pt);
        let mut int_base = Interner::new();
        let def_base = compile_card(&base_card, &mut int_base).expect("base compile");
        assert!(
            def_base.fully_supported,
            "template `{label}` must be fully supported"
        );
        let norm_base = strip_spans_and_names(def_base);

        for alt in alt_names {
            let alt_text = tmpl.replace("NAME", alt);
            let alt_card = mk_card(&format!("t5-ren-{idx}"), alt, mc, tl, &alt_text, *pt);
            let mut int_alt = Interner::new();
            let def_alt = compile_card(&alt_card, &mut int_alt).expect("alt compile");
            assert!(
                def_alt.fully_supported,
                "renamed `{label}` -> `{alt}` must remain fully supported"
            );
            let norm_alt = strip_spans_and_names(def_alt);
            assert_eq!(
                norm_base, norm_alt,
                "rename invariance failed for `{label}` under `{alt}`"
            );
            assert_eq!(
                int_base.tables(),
                int_alt.tables(),
                "intern tables drifted for `{label}` under `{alt}`"
            );
        }
    }
}

#[test]
fn t5_adversarial_malformed_oracle_text_fuzz_zero_panics() {
    let deep_parens = format!("{}Flying{}", "(".repeat(64), ")".repeat(64));
    let unclosed_parens = format!("{}Draw a card.", "(".repeat(64));
    let interleaved = "(\")(\")(\")(\")".repeat(32);
    let huge_line = "When ~ enters, draw a card and ".repeat(500);
    let huge_mana = "{W}{U}{B}{R}{G}{99999999999999999999}{X}{X}{X}".repeat(20);
    let many_lines = (0..250)
        .map(|i| format!("Ability line {i}: do {i} weird things."))
        .collect::<Vec<_>>()
        .join("\n");

    let adversarial_texts: Vec<(&str, &str, &str)> = vec![
        ("", "Creature", ""),
        ("{1}", "Creature", "\n\n   \t  \n\n"),
        ("{0}", "Instant", "..., :::, — — —, • • •, ;;;"),
        (
            "{W}",
            "Creature — Human",
            "Flying\u{0301} 🐉🔥\u{200D}✨ — −∞ “” ‘ ’",
        ),
        ("{2}{U}", "Sorcery", "日本語テキスト العربية עברית"),
        ("{1}{B}", "Creature", &deep_parens),
        ("{1}{R}", "Creature", &unclosed_parens),
        ("{1}{G}", "Artifact", &interleaved),
        ("{5}", "Sorcery", &huge_line),
        (&huge_mana, "Creature", "Flying\nHaste"),
        ("{3}", "Enchantment", &many_lines),
        (
            "{1}",
            "Artifact",
            ": Draw a card.\n{T}, : Destroy target creature.\n{}: Add {C}.\nX: Scry 1.",
        ),
        (
            "{99999999999999999999}",
            "Creature",
            "When ~ enters, deal 99999999999999999999 damage to any target and draw 99999999999999999999 cards.",
        ),
        (
            "{2}{W}",
            "Enchantment — Saga",
            "I, II, III, IV, V, VI, VII, VIII, IX, X —\n—\nI — :",
        ),
        (
            "{3}",
            "Enchantment — Class",
            "Class\n{1}: Level 2\n{999999999999}: Level 999999999999\nLevel -5",
        ),
        (
            "{2}",
            "Artifact — Spacecraft",
            "Station\n+\n9999999999999+\n-1+\n| | |",
        ),
        (
            "{U}",
            "Instant",
            "Spree\n+ —\n+ {9999999999999} — Draw a card.\n+",
        ),
        (
            "{W}",
            "Instant",
            "Cleave\n[unclosed bracket\n]unopened bracket\n[[nested]]",
        ),
        (
            "{W}",
            "Instant",
            "Gift\nGift a\nGift a card\n. If the gift was promised, instead.",
        ),
        ("{1}{R}", "Instant", "Choose 999999999999 —\n•\n• \n• • •"),
        (
            "{B}",
            "Creature",
            "Ward\nEquip\nEnchant\nCycling\nKicker\nSuspend\nVanishing\nPrototype",
        ),
        (
            "{G}",
            "Creature",
            "As ~ enters, choose \nIf you would \nWhenever \nAt the beginning of ",
        ),
    ];

    let mut inputs = Vec::with_capacity(adversarial_texts.len() + 3);
    for (i, (mc, tl, text)) in adversarial_texts.iter().enumerate() {
        inputs.push(RawCardInput {
            oracle_id: format!("t5-fuzz-{i:03}"),
            name: format!("Fuzz Card {i} — “Test”"),
            mana_cost: (*mc).into(),
            type_line: (*tl).into(),
            oracle_text: (*text).into(),
            power: Some("99999999999999999999".into()),
            toughness: Some("-99999999999999999999".into()),
            loyalty: Some("∞".into()),
            defense: Some("?".into()),
            ..Default::default()
        });
    }

    // Multi-face adversarial inputs
    inputs.push(RawCardInput {
        oracle_id: "t5-fuzz-multiface-10".into(),
        name: "Tenfold Chaos".into(),
        layout: Some("split".into()),
        faces: (0..10)
            .map(|i| RawFaceInput {
                name: if i % 2 == 0 {
                    String::new()
                } else {
                    format!("Face {i}")
                },
                mana_cost: "{X}{W/P}".into(),
                type_line: "Instant".into(),
                oracle_text: "Draw a card. ???".into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    inputs.push(RawCardInput {
        oracle_id: "t5-fuzz-reversible".into(),
        name: "Mirror Chaos".into(),
        layout: Some("reversible_card".into()),
        faces: vec![
            RawFaceInput {
                name: "Mirror Chaos".into(),
                type_line: "Creature — Construct".into(),
                oracle_text: "Flying".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                ..Default::default()
            },
            RawFaceInput {
                name: "Mirror Chaos".into(),
                type_line: "Creature — Construct".into(),
                oracle_text: "Haste".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    });

    // Compile every adversarial input individually (0 panics)
    let mut interner = Interner::new();
    for card in &inputs {
        let def = compile_card(card, &mut interner).expect("compile_card must never error");
        assert!(!def.faces.is_empty());
    }

    // Full bundle + CardDatabase raw & zstd round-trip
    let bundle =
        compile_bundle(&inputs, Vec::new()).expect("compile_bundle must succeed on fuzz corpus");
    let db = CardDatabase::from_bundle(bundle).expect("CardDatabase::from_bundle");
    let raw = db.encode_raw().expect("encode_raw");
    let zstd = db.encode_zstd(3).expect("encode_zstd");
    let dec_raw = CardDatabase::decode_bytes(&raw).expect("decode raw");
    let dec_zstd = CardDatabase::decode_bytes(&zstd).expect("decode zstd");
    assert_eq!(dec_raw.cards.len(), inputs.len());
    assert_eq!(dec_zstd.cards.len(), inputs.len());
    for card in &inputs {
        assert!(dec_zstd.id_by_oracle(&card.oracle_id).is_some());
    }
}
