use mtg_compiler::overrides::{OVERRIDES, get_or_parse};
use mtg_compiler::{
    Interner, RawCardInput, RawFaceInput, compile_bundle, compile_card, definition_support_report,
    derive_colors_from_mana_cost, resolve_override,
};
use mtg_ir::{
    AbilityDef, CardDatabase, CardLayout, CardTypeMask, CostDef, DeckDef, DeckEntry, EffectOp,
    EventKind, LayerMod, ReplacementWith, RestrictionKind, StaticBody,
};

fn creature(
    id: &str,
    name: &str,
    mana: &str,
    subtypes: &str,
    p: &str,
    t: &str,
    text: &str,
) -> RawCardInput {
    RawCardInput {
        oracle_id: id.into(),
        name: name.into(),
        layout: Some("normal".into()),
        mana_cost: mana.into(),
        type_line: format!("Creature \u{2014} {subtypes}"),
        oracle_text: text.into(),
        power: Some(p.into()),
        toughness: Some(t.into()),
        colors: derive_colors_from_mana_cost(mana),
        ..Default::default()
    }
}

fn spell_or_perm(id: &str, name: &str, mana: &str, type_line: &str, text: &str) -> RawCardInput {
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

fn assert_supported(card: &RawCardInput) -> (mtg_ir::CardDef, Interner) {
    let mut interner = Interner::new();
    let def = compile_card(card, &mut interner).expect("compile_card must not fail");
    assert!(
        def.fully_supported,
        "expected `{}` fully supported, got {:?}",
        card.name,
        definition_support_report(&def)
    );
    (def, interner)
}

// ============================================================================
// Pairwise Group A: Layout x Characteristics x Abilities (6 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_01_split_two_faces_with_different_targets_and_effects() {
    let card = RawCardInput {
        oracle_id: "e2e-t3-01".into(),
        name: "Shatter // Rebuild".into(),
        layout: Some("split".into()),
        faces: vec![
            RawFaceInput {
                name: "Shatter".into(),
                mana_cost: "{1}{R}".into(),
                type_line: "Instant".into(),
                oracle_text: "Destroy target artifact.".into(),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Rebuild".into(),
                mana_cost: "{2}{W}".into(),
                type_line: "Sorcery".into(),
                oracle_text: "Return target creature card from your graveyard to your hand.".into(),
                colors: vec!["W".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&card);
    assert_eq!(def.layout, CardLayout::Split);
    assert_eq!(def.faces.len(), 2);
    assert!(matches!(&def.faces[0].abilities[0], AbilityDef::Spell(_)));
    assert!(matches!(&def.faces[1].abilities[0], AbilityDef::Spell(_)));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_02_adventure_creature_keyword_plus_instant_spell() {
    let card = RawCardInput {
        oracle_id: "e2e-t3-02".into(),
        name: "Skyrider Falcon".into(),
        layout: Some("adventure".into()),
        faces: vec![
            RawFaceInput {
                name: "Skyrider Falcon".into(),
                mana_cost: "{2}{W}".into(),
                type_line: "Creature \u{2014} Bird".into(),
                oracle_text: "Flying, vigilance".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["W".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Sudden Gust".into(),
                mana_cost: "{1}{U}".into(),
                type_line: "Instant \u{2014} Adventure".into(),
                oracle_text: "Tap target creature. Draw a card.".into(),
                colors: vec!["U".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&card);
    assert_eq!(def.layout, CardLayout::Adventure);
    assert_eq!(def.faces[0].abilities.len(), 2);
    assert!(matches!(&def.faces[1].abilities[0], AbilityDef::Spell(s) if s.effect.len() == 2));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_03_transform_dfc_etb_trigger_plus_back_static_and_keywords() {
    let card = RawCardInput {
        oracle_id: "e2e-t3-03".into(),
        name: "Moonlit Tracker".into(),
        layout: Some("transform".into()),
        faces: vec![
            RawFaceInput {
                name: "Moonlit Tracker".into(),
                mana_cost: "{1}{G}".into(),
                type_line: "Creature \u{2014} Human Scout".into(),
                oracle_text: "When Moonlit Tracker enters, you gain 2 life.".into(),
                power: Some("2".into()),
                toughness: Some("2".into()),
                colors: vec!["G".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Lunar Ravager".into(),
                mana_cost: "".into(),
                type_line: "Creature \u{2014} Werewolf".into(),
                oracle_text: "Trample\nOther Werewolves you control get +1/+1.".into(),
                power: Some("4".into()),
                toughness: Some("4".into()),
                colors: vec!["G".into()],
                color_indicator: Some(vec!["G".into()]),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&card);
    assert_eq!(def.layout, CardLayout::Transform);
    assert!(matches!(
        &def.faces[0].abilities[0],
        AbilityDef::Triggered(_)
    ));
    assert!(
        def.faces[1]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Static(_)))
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_04_modal_dfc_spell_front_plus_enters_tapped_mana_land_back() {
    let card = RawCardInput {
        oracle_id: "e2e-t3-04".into(),
        name: "Tidecall Surge".into(),
        layout: Some("modal_dfc".into()),
        faces: vec![
            RawFaceInput {
                name: "Tidecall Surge".into(),
                mana_cost: "{2}{U}".into(),
                type_line: "Instant".into(),
                oracle_text: "Return target creature to its owner's hand. Scry 1.".into(),
                colors: vec!["U".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Tidecall Reef".into(),
                mana_cost: "".into(),
                type_line: "Land".into(),
                oracle_text: "Tidecall Reef enters tapped.\n{T}: Add {U}.".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&card);
    assert_eq!(def.layout, CardLayout::ModalDfc);
    assert!(
        def.faces[1]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Replacement(_)))
    );
    assert!(
        def.faces[1]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Activated(act) if act.is_mana))
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_05_flip_creature_trigger_plus_legendary_back_keywords() {
    let card = RawCardInput {
        oracle_id: "e2e-t3-05".into(),
        name: "Spark Apprentice".into(),
        layout: Some("flip".into()),
        faces: vec![
            RawFaceInput {
                name: "Spark Apprentice".into(),
                mana_cost: "{1}{R}".into(),
                type_line: "Creature \u{2014} Human Shaman".into(),
                oracle_text: "Whenever you cast an instant or sorcery spell, Spark Apprentice deals 1 damage to each opponent.".into(),
                power: Some("1".into()),
                toughness: Some("2".into()),
                colors: vec!["R".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Pyromaster Sage".into(),
                mana_cost: "".into(),
                type_line: "Legendary Creature \u{2014} Human Shaman".into(),
                oracle_text: "Haste\nInstant and sorcery spells you cast cost {1} less to cast.".into(),
                power: Some("3".into()),
                toughness: Some("3".into()),
                colors: vec!["R".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&card);
    assert_eq!(def.layout, CardLayout::Flip);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_06_omen_and_prepare_dual_face_combinations() {
    let omen = RawCardInput {
        oracle_id: "e2e-t3-06".into(),
        name: "Stormwing Serpent".into(),
        layout: Some("omen".into()),
        faces: vec![
            RawFaceInput {
                name: "Stormwing Serpent".into(),
                mana_cost: "{4}{U}{U}".into(),
                type_line: "Creature \u{2014} Serpent".into(),
                oracle_text: "Flying, ward {2}".into(),
                power: Some("5".into()),
                toughness: Some("5".into()),
                colors: vec!["U".into()],
                ..Default::default()
            },
            RawFaceInput {
                name: "Tide Omen".into(),
                mana_cost: "{1}{U}".into(),
                type_line: "Sorcery \u{2014} Omen".into(),
                oracle_text: "Draw two cards, then discard a card.".into(),
                colors: vec!["U".into()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let (def, _) = assert_supported(&omen);
    assert_eq!(def.layout, CardLayout::Omen);
    assert_eq!(def.faces.len(), 2);
}

// ============================================================================
// Pairwise Group B: Keywords x Costs x Statics (6 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_07_prototype_plus_flying_plus_etb_trigger() {
    let card = creature(
        "e2e-t3-07",
        "Aether Automaton",
        "{6}",
        "Construct",
        "5",
        "4",
        "Prototype {2}{W} \u{2014} 2/2\nFlying\nWhen Aether Automaton enters, you gain 3 life.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].prototype_spec.is_some());
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Keyword(_)))
    );
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Triggered(_)))
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_08_ward_plus_anthem_static_on_same_creature() {
    let card = creature(
        "e2e-t3-08",
        "Suncrest Captain",
        "{2}{W}{W}",
        "Angel Knight",
        "3",
        "4",
        "Flying, ward {2}\nOther creatures you control get +1/+1.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Static(s) if matches!(s.static_def.body, StaticBody::Continuous { modifier: LayerMod::Pt { .. }, .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_09_equip_cost_plus_equipped_creature_pt_and_keyword_grant() {
    let card = spell_or_perm(
        "e2e-t3-09",
        "Vanguard Glaive",
        "{2}",
        "Artifact \u{2014} Equipment",
        "Equipped creature gets +2/+1 and has first strike.\nEquip {2}",
    );
    let (def, _) = assert_supported(&card);
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Static(_)))
    );
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Keyword(_)))
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_10_enchant_aura_plus_restriction_and_trigger() {
    let card = spell_or_perm(
        "e2e-t3-10",
        "Frost Binding",
        "{1}{U}",
        "Enchantment \u{2014} Aura",
        "Enchant creature\nWhen Frost Binding enters, tap enchanted creature.\nEnchanted creature doesn't untap during its controller's untap step.",
    );
    let (def, _) = assert_supported(&card);
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Keyword(_)))
    );
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Triggered(_)))
    );
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Static(s)
        if matches!(s.static_def.body, StaticBody::Restriction { restriction: RestrictionKind::DoesntUntap, .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_11_kicker_plus_kicked_etb_intervening_if_trigger() {
    let card = creature(
        "e2e-t3-11",
        "Grove Sentinel",
        "{2}{G}",
        "Elf Warrior",
        "3",
        "2",
        "Kicker {1}{G}\nWhen Grove Sentinel enters, if it was kicked, put two +1/+1 counters on it.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.condition.is_some()
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_12_cycling_plus_when_you_cycle_triggered_ability() {
    let card = creature(
        "e2e-t3-12",
        "Sandstorm Drake",
        "{3}{U}",
        "Drake",
        "3",
        "3",
        "Flying\nCycling {2}\nWhen you cycle Sandstorm Drake, draw a card.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::Cycle
    )));
}

// ============================================================================
// Pairwise Group C: Costs x Filters x Effects (6 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_13_tap_and_mana_cost_to_create_filtered_token() {
    let card = spell_or_perm(
        "e2e-t3-13",
        "Token Foundry",
        "{3}",
        "Artifact",
        "{2}, {T}: Create a 1/1 colorless Servo artifact creature token.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Activated(act)
        if act.cost.len() == 2 && matches!(act.effect[0].op, EffectOp::CreateToken { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_14_sacrifice_another_creature_for_targeted_removal() {
    let card = creature(
        "e2e-t3-14",
        "Dread Executioner",
        "{2}{B}",
        "Human Assassin",
        "2",
        "2",
        "{1}{B}, Sacrifice another creature: Destroy target creature with mana value 3 or less.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Activated(act)
        if act.targets.len() == 1
            && act.targets[0].filter.mana_value.is_some()
            && matches!(act.effect[0].op, EffectOp::Destroy { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_15_pay_life_and_tap_to_draw_and_lose_life() {
    let card = spell_or_perm(
        "e2e-t3-15",
        "Abyssal Tome",
        "{2}{B}",
        "Artifact",
        "{T}, Pay 2 life: Draw a card.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Activated(act)
        if act.cost.contains(&CostDef::TapSelf)
            && act.cost.iter().any(|c| matches!(c, CostDef::PayLife { .. }))
            && matches!(act.effect[0].op, EffectOp::Draw { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_16_discard_card_cost_to_create_treasure_token() {
    let card = creature(
        "e2e-t3-16",
        "Vault Smuggler",
        "{1}{R}",
        "Goblin Pirate",
        "2",
        "1",
        "{T}, Discard a card: Create a Treasure token.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Activated(act)
        if act.cost.iter().any(|c| matches!(c, CostDef::Discard { .. }))
            && matches!(act.effect[0].op, EffectOp::CreateToken { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_17_remove_counter_cost_to_put_counter_on_target_creature() {
    let card = creature(
        "e2e-t3-17",
        "Spore Weaver",
        "{2}{G}",
        "Fungus",
        "0",
        "0",
        "Spore Weaver enters with two +1/+1 counters on it.\nRemove a +1/+1 counter from Spore Weaver: Put a +1/+1 counter on target creature.",
    );
    let (def, _) = assert_supported(&card);
    assert_eq!(def.faces[0].abilities.len(), 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_18_exile_from_graveyard_cost_to_deal_damage() {
    let card = creature(
        "e2e-t3-18",
        "Cinder Necromancer",
        "{1}{B}{R}",
        "Human Wizard",
        "2",
        "2",
        "{T}, Exile a creature card from your graveyard: Cinder Necromancer deals 2 damage to each opponent.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Activated(act)
        if act.cost.iter().any(|c| matches!(c, CostDef::ExileFrom { .. }))
            && matches!(act.effect[0].op, EffectOp::Damage { .. })
    )));
}

// ============================================================================
// Pairwise Group D: Triggers x Conditions x Values (6 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_19_etb_intervening_if_controls_subtype_with_dynamic_count() {
    let card = creature(
        "e2e-t3-19",
        "Elf Marshal",
        "{2}{G}",
        "Elf Warrior",
        "2",
        "2",
        "When Elf Marshal enters, if you control another Elf, you gain life equal to the number of Elves you control.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.condition.is_some() && !t.effect.is_empty()
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_20_landfall_with_scry_and_conditional_draw() {
    let card = creature(
        "e2e-t3-20",
        "Horizon Mystic",
        "{2}{U}{G}",
        "Merfolk Druid",
        "2",
        "4",
        "Landfall \u{2014} Whenever a land enters under your control, scry 1, then draw a card.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::ZoneChange && t.effect.len() == 2
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_21_attacks_trigger_pumping_attacking_creatures() {
    let card = creature(
        "e2e-t3-21",
        "Banner Vanguard",
        "{3}{W}",
        "Human Soldier",
        "3",
        "3",
        "Whenever Banner Vanguard attacks, attacking creatures get +1/+1 until end of turn.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t)
        if t.trigger.event == EventKind::Attacks
            && matches!(t.effect[0].op, EffectOp::Pump { .. })
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_22_dies_trigger_scaling_life_loss_with_power() {
    let card = creature(
        "e2e-t3-22",
        "Vengeful Wraith",
        "{2}{B}{B}",
        "Wraith",
        "4",
        "1",
        "When Vengeful Wraith dies, target opponent loses life equal to its power.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if !t.targets.is_empty() && !t.effect.is_empty()
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_23_cast_spell_trigger_with_domain_damage() {
    let card = creature(
        "e2e-t3-23",
        "Prism Archmage",
        "{2}{U}{R}",
        "Human Wizard",
        "3",
        "3",
        "Domain \u{2014} Whenever you cast an instant or sorcery spell, Prism Archmage deals damage to each opponent equal to the number of basic land types among lands you control.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::Cast
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_24_upkeep_trigger_with_life_condition_and_token() {
    let card = spell_or_perm(
        "e2e-t3-24",
        "Sanctum Beacon",
        "{3}{W}",
        "Enchantment",
        "At the beginning of your upkeep, if you have 30 or more life, create a 4/4 white Angel creature token with flying.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.condition.is_some()
    )));
}

// ============================================================================
// Pairwise Group E: Modals x Bindings x Delayed / Reflexive Triggers (4 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_25_modal_spell_with_per_mode_targets_and_mixed_effects() {
    let card = spell_or_perm(
        "e2e-t3-25",
        "Verdant Confluence Synth",
        "{3}{G}{G}",
        "Sorcery",
        "Choose two \u{2014}\n\u{2022} Put two +1/+1 counters on target creature.\n\u{2022} Return target permanent card from your graveyard to your hand.\n\u{2022} Destroy target artifact or enchantment.",
    );
    let (def, _) = assert_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            let m = s.modal.as_ref().unwrap();
            assert_eq!(m.modes.len(), 3);
            assert!(m.modes.iter().all(|mode| !mode.targets.is_empty()));
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_26_reflexive_when_you_do_with_target_and_counter_placement() {
    let card = creature(
        "e2e-t3-26",
        "Forge Benefactor",
        "{2}{W}{R}",
        "Dwarf Artificer",
        "3",
        "3",
        "Whenever Forge Benefactor attacks, you may discard a card. When you do, put a +1/+1 counter on target creature and it gains first strike until end of turn.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.effect.iter().any(|e| e.reflexive_trigger.is_some())
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_27_delayed_end_step_sacrifice_with_token_creation() {
    let card = spell_or_perm(
        "e2e-t3-27",
        "Fleeting Mirage",
        "{1}{R}",
        "Instant",
        "Create a 3/1 red Elemental creature token with haste. Sacrifice it at the beginning of the next end step.",
    );
    let (def, _) = assert_supported(&card);
    match &def.faces[0].abilities[0] {
        AbilityDef::Spell(s) => {
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::CreateToken { .. }))
            );
            assert!(
                s.effect
                    .iter()
                    .any(|e| matches!(e.op, EffectOp::DelayedTrigger { .. }))
            );
        }
        other => panic!("expected spell, got {other:?}"),
    }
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_28_exile_top_of_library_binding_with_play_from_exile() {
    let card = creature(
        "e2e-t3-28",
        "Spark Visionary",
        "{1}{R}",
        "Human Shaman",
        "2",
        "1",
        "When Spark Visionary enters, exile the top card of your library. Until end of turn, you may play that card.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t)
        if t.effect.iter().any(|e| matches!(e.op, EffectOp::GrantPlayFromExile { .. }))
    )));
}

// ============================================================================
// Pairwise Group F: Statics x Replacements x Bundle Codec (5 tests)
// ============================================================================

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_29_enters_tapped_unless_condition_plus_dual_land_mana() {
    let card = spell_or_perm(
        "e2e-t3-29",
        "Sunlit Cove",
        "",
        "Land \u{2014} Plains Island",
        "Sunlit Cove enters tapped unless you control two or more other lands.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Replacement(r) if matches!(r.replacement.with, ReplacementWith::ZoneChange { .. })
    )));
    let mana_count = def.faces[0]
        .abilities
        .iter()
        .filter(|a| matches!(a, AbilityDef::Activated(act) if act.is_mana))
        .count();
    assert_eq!(mana_count, 2);
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_30_enters_with_counters_replacement_plus_counter_static() {
    let card = creature(
        "e2e-t3-30",
        "Crystaline Behemoth",
        "{3}{G}{G}",
        "Beast",
        "0",
        "0",
        "Crystaline Behemoth enters with four +1/+1 counters on it.\nEach creature you control with a +1/+1 counter on it has trample.",
    );
    let (def, _) = assert_supported(&card);
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Replacement(_)))
    );
    assert!(
        def.faces[0]
            .abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Static(_)))
    );
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_31_cost_reduction_static_plus_cast_trigger() {
    let card = creature(
        "e2e-t3-31",
        "Spellforge Mystic",
        "{2}{U}{R}",
        "Human Wizard",
        "2",
        "3",
        "Instant and sorcery spells you cast cost {1} less to cast.\nWhenever you cast an instant or sorcery spell, scry 1.",
    );
    let (def, _) = assert_supported(&card);
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Static(s) if matches!(s.static_def.body, StaticBody::CostModifier { .. })
    )));
    assert!(def.faces[0].abilities.iter().any(|a| matches!(
        a,
        AbilityDef::Triggered(t) if t.trigger.event == EventKind::Cast
    )));
}

#[test]
#[ignore = "E2E M5 gate"]
fn t3_pair_32_heterogeneous_bundle_compile_zstd_roundtrip_and_queries() {
    let cards = vec![
        spell_or_perm(
            "e2e-t3-b1",
            "Synth Plains",
            "",
            "Basic Land \u{2014} Plains",
            "",
        ),
        creature(
            "e2e-t3-b2",
            "Sky Knight",
            "{1}{W}",
            "Human Knight",
            "2",
            "2",
            "Flying, vigilance",
        ),
        spell_or_perm(
            "e2e-t3-b3",
            "Star Bolt",
            "{R}",
            "Instant",
            "Star Bolt deals 3 damage to any target.",
        ),
        spell_or_perm(
            "e2e-t3-b4",
            "Valor Anthem",
            "{2}{W}",
            "Enchantment",
            "Creatures you control get +1/+1.",
        ),
    ];
    let deck = DeckDef {
        id: "e2e-t3-deck".into(),
        name: "Synthetic Pairwise Deck".into(),
        commanders: vec!["e2e-t3-b2".into()],
        cards: vec![
            DeckEntry {
                oracle_id: "e2e-t3-b1".into(),
                count: 20,
            },
            DeckEntry {
                oracle_id: "e2e-t3-b2".into(),
                count: 1,
            },
            DeckEntry {
                oracle_id: "e2e-t3-b3".into(),
                count: 4,
            },
            DeckEntry {
                oracle_id: "e2e-t3-b4".into(),
                count: 4,
            },
        ],
    };
    let bundle = compile_bundle(&cards, vec![deck]).unwrap();
    assert!(bundle.cards.iter().all(|c| c.fully_supported));
    let db = CardDatabase::from_bundle(bundle).unwrap();
    assert_eq!(db.fully_supported_count(), 4);
    assert!(db.keyword_id("flying").is_some());
    assert!(db.subtype_id("Knight").is_some());
    let zstd = db.encode_zstd(1).unwrap();
    let decoded = CardDatabase::decode_bytes(&zstd).unwrap();
    assert_eq!(decoded.cards, db.cards);
    assert_eq!(decoded.decks, db.decks);
}

#[test]
fn t3_pair_33_m1_pipeline_overrides_plus_basic_lands_bundle_codec() {
    // M1-active pairwise test: overrides + synthetic basic lands + bundle sorting + zstd/raw codec
    let mut cards = Vec::new();
    let mut interner = Interner::new();
    for slot in OVERRIDES.iter().take(5) {
        let entry = get_or_parse(slot).unwrap();
        let resolved = resolve_override(
            slot.oracle_id,
            slot.expected_text_hash,
            &entry.card.name,
            &mut interner,
        )
        .unwrap()
        .unwrap();
        assert!(resolved.fully_supported);
        cards.push(resolved);
    }

    let synth_lands = vec![
        spell_or_perm(
            "e2e-t3-m1-1",
            "Synth Plains",
            "",
            "Basic Land \u{2014} Plains",
            "",
        ),
        spell_or_perm(
            "e2e-t3-m1-2",
            "Synth Forest",
            "",
            "Basic Land \u{2014} Forest",
            "",
        ),
    ];
    let land_bundle = compile_bundle(&synth_lands, vec![]).unwrap();
    assert_eq!(land_bundle.cards.len(), 2);
    assert!(land_bundle.cards.iter().all(|c| c.fully_supported));
    assert!(
        land_bundle.cards[0].faces[0]
            .characteristics
            .types
            .contains(CardTypeMask::LAND)
    );

    let db = CardDatabase::from_bundle(land_bundle).unwrap();
    let raw = db.encode_raw().unwrap();
    let zstd = db.encode_zstd(1).unwrap();
    assert_eq!(CardDatabase::decode_bytes(&raw).unwrap().cards, db.cards);
    assert_eq!(CardDatabase::decode_bytes(&zstd).unwrap().cards, db.cards);
}
