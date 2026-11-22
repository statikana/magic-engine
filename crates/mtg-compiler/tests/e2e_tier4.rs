use mtg_compiler::{
    Interner, RawCardInput, RawFaceInput, compile_bundle, compile_card, definition_support_report,
    derive_colors_from_mana_cost,
};
use mtg_ir::{AbilityDef, CardDatabase, CardLayout, CardTypeMask, CostDef, DeckDef, DeckEntry};

fn mk_card(id: &str, name: &str, mana: &str, type_line: &str, text: &str) -> RawCardInput {
    RawCardInput {
        oracle_id: id.into(),
        name: name.into(),
        layout: Some("normal".into()),
        mana_cost: mana.into(),
        type_line: type_line.into(),
        oracle_text: text.into(),
        colors: derive_colors_from_mana_cost(mana),
        ..Default::default()
    }
}

fn mk_creature(
    id: &str,
    name: &str,
    mana: &str,
    subtypes: &str,
    p: &str,
    t: &str,
    text: &str,
) -> RawCardInput {
    let mut c = mk_card(
        id,
        name,
        mana,
        &format!("Creature \u{2014} {subtypes}"),
        text,
    );
    c.power = Some(p.into());
    c.toughness = Some(t.into());
    c
}

fn verify_arch(card: &RawCardInput) -> mtg_ir::CardDef {
    let mut interner = Interner::new();
    let def = compile_card(card, &mut interner).expect("compile_card must succeed");
    assert!(
        def.fully_supported,
        "archetype `{}` failed support: {:?}",
        card.name,
        definition_support_report(&def)
    );
    def
}

// ============================================================================
// Tier 4: Real-World Synthetic Archetype Scenarios (16 tests, M5-gated)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_01_planeswalker_three_loyalty_abilities() {
    let mut pw = mk_card(
        "e2e-t4-01",
        "Kaelith, Astral Conduit",
        "{2}{U}{W}",
        "Legendary Planeswalker \u{2014} Kaelith",
        "+1: Scry 1, then draw a card.\n\u{2212}2: Exile target tapped creature.\n\u{2212}6: You get an emblem with \"Creatures you control get +2/+2 and have flying.\"",
    );
    pw.loyalty = Some("4".into());
    let def = verify_arch(&pw);
    assert_eq!(def.faces[0].characteristics.loyalty, Some(4));
    assert_eq!(def.faces[0].abilities.len(), 3);
    assert!(def.faces[0].abilities.iter().all(|ab| matches!(
        ab,
        AbilityDef::Activated(a) if a.cost.iter().any(|c| matches!(c, CostDef::Loyalty { .. }))
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_02_saga_three_chapters_token_counters_draw() {
    let saga = mk_card(
        "e2e-t4-02",
        "Ballad of the Dawnforge",
        "{2}{W}",
        "Enchantment \u{2014} Saga",
        "I \u{2014} Create two 1/1 white Soldier creature tokens.\nII \u{2014} Put a +1/+1 counter on each creature you control.\nIII \u{2014} Draw two cards.",
    );
    let def = verify_arch(&saga);
    let chapters: Vec<_> = def.faces[0]
        .abilities
        .iter()
        .filter_map(|ab| match ab {
            AbilityDef::Triggered(t) if t.trigger.is_chapter_ability => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(chapters.len(), 3);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_03_read_ahead_saga() {
    let saga = mk_card(
        "e2e-t4-03",
        "Chronicle of the Sky",
        "{1}{U}",
        "Enchantment \u{2014} Saga",
        "Read ahead\nI \u{2014} Scry 2.\nII \u{2014} Tap target creature.\nIII \u{2014} Draw two cards.",
    );
    let def = verify_arch(&saga);
    assert!(def.faces[0].has_read_ahead);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_04_class_enchantment_three_levels() {
    let class = mk_card(
        "e2e-t4-04",
        "Scholar Class",
        "{1}{U}",
        "Enchantment \u{2014} Class",
        "You have no maximum hand size.\n{2}{U}: Level 2\nWhenever you cast an instant or sorcery spell, scry 1.\n{4}{U}: Level 3\nAt the beginning of your upkeep, draw a card.",
    );
    let def = verify_arch(&class);
    assert!(!def.faces[0].abilities.is_empty());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_05_leveler_creature_striations() {
    let leveler = mk_creature(
        "e2e-t4-05",
        "Tidecaller Initiate",
        "{U}",
        "Merfolk Wizard",
        "1",
        "2",
        "Level up {1}{U}\nLEVEL 2-4\n3/3\nFlying\nLEVEL 5+\n5/5\nFlying, ward {2}",
    );
    let def = verify_arch(&leveler);
    assert!(!def.faces[0].abilities.is_empty());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_06_station_spacecraft_striations() {
    let mut station = mk_card(
        "e2e-t4-06",
        "Starlight Cruiser",
        "{3}",
        "Artifact \u{2014} Spacecraft",
        "Station\n4+\nFlying\n8+\nWhenever Starlight Cruiser attacks, draw two cards.",
    );
    station.power = Some("5".into());
    station.toughness = Some("5".into());
    let def = verify_arch(&station);
    assert!(!def.faces[0].abilities.is_empty());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_07_split_fuse_style_dual_spell() {
    let split = RawCardInput {
        oracle_id: "e2e-t4-07".into(),
        name: "Strike // Shelter".into(),
        layout: Some("split".into()),
        faces: vec![
            RawFaceInput {
                name: "Strike".into(),
                mana_cost: "{1}{R}".into(),
                type_line: "Instant".into(),
                oracle_text: "Strike deals 3 damage to target creature.".into(),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Shelter".into(),
                mana_cost: "{1}{W}".into(),
                type_line: "Instant".into(),
                oracle_text: "Target creature gains indestructible until end of turn.".into(),
                colors: vec!["W".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let def = verify_arch(&split);
    assert_eq!(def.layout, CardLayout::Split);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_08_adventure_knight_and_quest() {
    let adv = RawCardInput {
        oracle_id: "e2e-t4-08".into(),
        name: "Solstice Paladin".into(),
        layout: Some("adventure".into()),
        faces: vec![
            RawFaceInput {
                name: "Solstice Paladin".into(),
                mana_cost: "{2}{W}".into(),
                type_line: "Creature \u{2014} Human Knight".into(),
                oracle_text: "First strike, lifelink".into(),
                power: Some("3".into()),
                toughness: Some("2".into()),
                colors: vec!["W".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Dawn Crusade".into(),
                mana_cost: "{1}{W}".into(),
                type_line: "Sorcery \u{2014} Adventure".into(),
                oracle_text: "Create two 1/1 white Soldier creature tokens.".into(),
                colors: vec!["W".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let def = verify_arch(&adv);
    assert_eq!(def.layout, CardLayout::Adventure);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_09_transform_dfc_creature_to_planeswalker() {
    let dfc = RawCardInput {
        oracle_id: "e2e-t4-09".into(),
        name: "Lyra, Aspiring Spark".into(),
        layout: Some("transform".into()),
        faces: vec![
            RawFaceInput {
                name: "Lyra, Aspiring Spark".into(),
                mana_cost: "{1}{W}".into(),
                type_line: "Legendary Creature \u{2014} Human Wizard".into(),
                oracle_text: "Lifelink\nWhen Lyra, Aspiring Spark enters, you gain 2 life.".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["W".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Lyra, Awakened Beacon".into(),
                mana_cost: "".into(),
                type_line: "Legendary Planeswalker \u{2014} Lyra".into(),
                oracle_text: "+1: Put a +1/+1 counter on target creature.\n\u{2212}3: Exile target nonland permanent.".into(),
                loyalty: Some("4".into()),
                colors: vec!["W".into()],
                color_indicator: Some(vec!["W".into()]),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let def = verify_arch(&dfc);
    assert_eq!(def.layout, CardLayout::Transform);
    assert_eq!(def.faces[1].characteristics.loyalty, Some(4));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_10_modal_dfc_spell_and_bolt_land() {
    let mdfc = RawCardInput {
        oracle_id: "e2e-t4-10".into(),
        name: "Ember Storm".into(),
        layout: Some("modal_dfc".into()),
        faces: vec![
            RawFaceInput {
                name: "Ember Storm".into(),
                mana_cost: "{2}{R}{R}".into(),
                type_line: "Sorcery".into(),
                oracle_text: "Ember Storm deals 3 damage to each creature.".into(),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Ember Spire".into(),
                mana_cost: "".into(),
                type_line: "Land".into(),
                oracle_text: "As Ember Spire enters, you may pay 3 life. If you don't, it enters tapped.\n{T}: Add {R}.".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let def = verify_arch(&mdfc);
    assert_eq!(def.layout, CardLayout::ModalDfc);
    assert_eq!(def.faces.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_11_spree_modal_spell_with_per_mode_costs() {
    let spree = mk_card(
        "e2e-t4-11",
        "Arcane Triplicate",
        "{U}",
        "Instant",
        "Spree\n+ {1} \u{2014} Counter target spell unless its controller pays {2}.\n+ {2} \u{2014} Return target creature to its owner's hand.\n+ {1}{U} \u{2014} Draw two cards.",
    );
    let def = verify_arch(&spree);
    assert!(def.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Spell(s) if s.modal.as_ref().is_some_and(|m| !m.per_mode_costs.is_empty())
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_12_cleave_and_gift_spells() {
    let cleave = mk_card(
        "e2e-t4-12a",
        "Severing Gale",
        "{1}{W}",
        "Instant",
        "Cleave {3}{W}{W}\nDestroy target creature [with power 4 or greater].",
    );
    let d1 = verify_arch(&cleave);
    assert!(d1.faces[0].cleaved_spell_spec.is_some());

    let gift = mk_card(
        "e2e-t4-12b",
        "Bountiful Offering",
        "{1}{W}",
        "Instant",
        "Gift a card\nExile target creature. If the gift was promised, its controller creates a 1/1 white Bird creature token with flying.",
    );
    let d2 = verify_arch(&gift);
    assert!(d2.faces[0].abilities.iter().any(|ab| matches!(
        ab,
        AbilityDef::Spell(s) if s.effect.iter().any(|st| matches!(
            &st.gate,
            Some(mtg_ir::StepGate::Condition { condition }) if matches!(condition.as_ref(), mtg_ir::Condition::GiftPromised)
        ))
    )));

    // CR 702.174b replacement-style Gift spell populates `gift_promised_spell_spec`
    let gift_replace = mk_card(
        "e2e-t4-12c",
        "Bountiful Banishment",
        "{1}{W}",
        "Instant",
        "Gift a card\nDestroy target creature. If the gift was promised, exile target creature instead.",
    );
    let d3 = verify_arch(&gift_replace);
    assert!(d3.faces[0].gift_promised_spell_spec.is_some());
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_13_graveyard_reanimator_engine() {
    let reanimator = mk_creature(
        "e2e-t4-13",
        "Cryptbinder Lich",
        "{3}{B}{B}",
        "Zombie Wizard",
        "4",
        "4",
        "Deathtouch\nWhen Cryptbinder Lich enters, target player mills four cards.\n{2}{B}, {T}: Return target creature card from your graveyard to the battlefield.",
    );
    let def = verify_arch(&reanimator);
    assert_eq!(def.faces[0].abilities.len(), 3);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_14_token_and_counter_synergy_engine() {
    let engine = mk_creature(
        "e2e-t4-14",
        "Verdant Sovereign",
        "{2}{G}{W}",
        "Elf Noble",
        "3",
        "4",
        "Vigilance\nWhenever another creature enters under your control, put a +1/+1 counter on it.\n{3}{G}{W}, {T}: Proliferate.",
    );
    let def = verify_arch(&engine);
    assert_eq!(def.faces[0].abilities.len(), 3);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_15_spellslinger_storm_engine() {
    let storm_mage = mk_creature(
        "e2e-t4-15",
        "Tempest Weaver",
        "{1}{U}{R}",
        "Human Wizard",
        "2",
        "3",
        "Prowess\nInstant and sorcery spells you cast cost {1} less to cast.\nWhenever you cast an instant or sorcery spell, Tempest Weaver deals 1 damage to each opponent.",
    );
    let def = verify_arch(&storm_mage);
    assert_eq!(def.faces[0].abilities.len(), 3);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t4_arch_16_full_100_card_synthetic_commander_deck_pipeline() {
    // Construct a complete 100-card synthetic Commander deck (singleton nonbasics + basics)
    let mut cards: Vec<RawCardInput> = Vec::new();
    let mut entries: Vec<DeckEntry> = Vec::new();

    // 1. Commander
    let cmdr = mk_creature(
        "e2e-cmdr-000",
        "Aurelius, Starforge Strategist",
        "{1}{W}{U}{G}",
        "Legendary Creature \u{2014} Human Knight",
        "3",
        "4",
        "Flying, vigilance\nWhenever Aurelius, Starforge Strategist attacks, draw a card and put a +1/+1 counter on target creature you control.",
    );
    let mut cmdr = cmdr;
    cmdr.type_line = "Legendary Creature \u{2014} Human Knight".into();
    cards.push(cmdr);
    entries.push(DeckEntry {
        oracle_id: "e2e-cmdr-000".into(),
        count: 1,
    });

    // 2. Basic lands (3 types totaling 30 cards)
    for (oid, name, sub, count) in [
        ("e2e-cmdr-land-w", "Synthetic Plains", "Plains", 10u16),
        ("e2e-cmdr-land-u", "Synthetic Island", "Island", 10u16),
        ("e2e-cmdr-land-g", "Synthetic Forest", "Forest", 10u16),
    ] {
        cards.push(mk_card(
            oid,
            name,
            "",
            &format!("Basic Land \u{2014} {sub}"),
            "",
        ));
        entries.push(DeckEntry {
            oracle_id: oid.into(),
            count,
        });
    }

    // 3. 6 Utility dual/tapped lands (6 cards)
    for i in 1..=6 {
        let oid = format!("e2e-cmdr-util-land-{i:02}");
        cards.push(mk_card(
            &oid,
            &format!("Synthetic Sanctuary {i}"),
            "",
            "Land",
            &format!("Synthetic Sanctuary {i} enters tapped.\n{{T}}: Add {{W}} or {{U}}."),
        ));
        entries.push(DeckEntry {
            oracle_id: oid,
            count: 1,
        });
    }

    // 4. 63 Unique synthetic nonland singleton spells/permanents (total = 1 + 30 + 6 + 63 = 100)
    let templates = [
        (
            "{1}{W}",
            "Creature \u{2014} Soldier",
            Some(("2", "2")),
            "Vigilance",
        ),
        (
            "{2}{U}",
            "Creature \u{2014} Drake",
            Some(("2", "2")),
            "Flying",
        ),
        (
            "{1}{G}",
            "Creature \u{2014} Elf Druid",
            Some(("1", "1")),
            "{T}: Add {G}.",
        ),
        ("{1}{W}", "Instant", None, "Exile target creature."),
        ("{1}{U}", "Instant", None, "Counter target spell."),
        (
            "{2}{G}",
            "Sorcery",
            None,
            "Search your library for a basic land card, put it onto the battlefield tapped, then shuffle.",
        ),
        ("{2}", "Artifact", None, "{T}: Add {C}."),
        (
            "{2}{W}",
            "Enchantment",
            None,
            "Creatures you control get +1/+1.",
        ),
        ("{3}{U}", "Sorcery", None, "Draw two cards."),
    ];

    for i in 1..=63 {
        let oid = format!("e2e-cmdr-card-{i:02}");
        let (mana, tline, pt, text) = templates[(i - 1) % templates.len()];
        let name = format!("Synthetic Archetype Card {i}");
        let mut c = mk_card(&oid, &name, mana, tline, text);
        if let Some((p, t)) = pt {
            c.power = Some(p.into());
            c.toughness = Some(t.into());
        }
        cards.push(c);
        entries.push(DeckEntry {
            oracle_id: oid,
            count: 1,
        });
    }

    let total_cards_in_deck: u16 = entries.iter().map(|e| e.count).sum();
    assert_eq!(total_cards_in_deck, 100);

    let deck = DeckDef {
        id: "e2e-synth-cmdr-deck".into(),
        name: "Bant Synthetic Starforge".into(),
        commanders: vec!["e2e-cmdr-000".into()],
        cards: entries,
    };

    let bundle = compile_bundle(&cards, vec![deck]).expect("compile_bundle must succeed");
    assert_eq!(bundle.cards.len(), cards.len());
    for c in &bundle.cards {
        assert!(
            c.fully_supported,
            "deck card `{}` not fully supported: {:?}",
            c.name,
            definition_support_report(c)
        );
    }

    let db = CardDatabase::from_bundle(bundle).expect("CardDatabase::from_bundle must succeed");
    assert_eq!(db.fully_supported_count(), cards.len());
    assert_eq!(db.decks.len(), 1);
    assert_eq!(db.decks[0].commanders, vec!["e2e-cmdr-000"]);

    let zstd_bytes = db.encode_zstd(3).expect("encode_zstd must succeed");
    let raw_bytes = db.encode_raw().expect("encode_raw must succeed");
    assert!(zstd_bytes.len() < raw_bytes.len());

    let decoded = CardDatabase::decode_bytes(&zstd_bytes).expect("decode_bytes must succeed");
    assert_eq!(decoded.cards.len(), db.cards.len());
    assert_eq!(decoded.tables, db.tables);
    assert_eq!(decoded.decks, db.decks);

    // Query every deck entry in the decoded database
    let mut resolved_total: u16 = 0;
    for entry in &decoded.decks[0].cards {
        let id = decoded
            .id_by_oracle(&entry.oracle_id)
            .expect("every deck oracle_id must resolve in CardDatabase");
        let card_def = decoded.get(id).unwrap();
        assert!(card_def.fully_supported);
        assert_eq!(decoded.id_by_name(&card_def.name), Some(id));
        resolved_total += entry.count;
    }
    assert_eq!(resolved_total, 100);
    assert!(decoded.tables.subtypes.iter().any(|s| s == "Plains"));
    assert!(
        decoded
            .get(decoded.id_by_oracle("e2e-cmdr-000").unwrap())
            .unwrap()
            .faces[0]
            .characteristics
            .types
            .contains(CardTypeMask::CREATURE)
    );
}
