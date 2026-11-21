//! Tier 2 Opaque-Box E2E Tests: CR 613 Layers, CR 614–616 Replacements & CR 603 Triggers (F8–F10).
//!
//! Covers 15 tests: `T2-F08-01..05`, `T2-F09-01..05`, `T2-F10-01..05`.

use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, EngineError, GameConfig,
    MacroAction, PlayerId, PowerToughness, TargetRef,
};
use mtg_engine_e2e::driver::SessionDriver;
use mtg_engine_e2e::synth::{
    SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
};
use mtg_ir::{
    Affected, Amount, CardDefId, CardTypeMask, ColorMask, CompareOp, Condition, DamageModMode,
    DamageModTarget, DamagePreventionSpec, DamageScaleSpec, Duration, EffectOp, EventDetail,
    EventFilter, EventKind, EventPattern, GamePhase, Layer, LayerMod, ManaColor, ObjectFilter,
    PlayerFilter, PlayerRef, PlayerRelation, PreventMode, PreventScope, RelativeControl,
    ReplacementDef, ReplacementWith, Selector, StaticBody, StaticDef, SupertypeMask, TargetKind,
    TargetSpec, Zone, ZoneMask,
};

fn build_60_pair(land: CardDefId, filler: CardDefId, specials: &[CardDefId]) -> Vec<DeckSpec> {
    let mut main = Vec::with_capacity(60);
    for &s in specials {
        main.extend([s, s, s, s]);
    }
    while main.len() < 24 {
        main.push(land);
    }
    while main.len() < 60 {
        main.push(filler);
    }
    let d = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    vec![d.clone(), d]
}

fn find_land_in_hand(drv: &SessionDriver, p: PlayerId) -> Option<mtg_engine::ObjectId> {
    drv.session.view_model(Some(p)).players[usize::from(p.0)]
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::LAND))
        .map(|o| o.id)
}

// ============================================================================
// F8 Boundary & CR 613 Cases (5 cases)
// ============================================================================

#[test]
fn t2_f08_01_negative_toughness_evaluates_unclamped_before_sba() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_81", "Synth Swamp", "Swamp", ManaColor::B);
    let imp = b.add_synth_vanilla_creature("synth_imp_t2_81", "Synth Imp", "{0}", 1, 1);
    let shrink3 = b.add_card("synth_sh3_t2_81", "Synth Wither", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Pump {
                        toughness: Some(Box::new(Amount::Const { n: -3 })),
                        duration: Some(Duration::EndOfTurn),
                        pump_choices: vec![],
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: -3 }),
                )],
            );
    });
    let watcher = b.add_card("synth_lki_watch_81", "Synth LkiWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability_if(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::BATTLEFIELD,
                            to: Some(Zone::Graveyard),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                ),
                Condition::LkiToughnessLessThan { max: -1 },
                vec![],
                vec![effect_step(
                    EffectOp::GainLife {
                        set_life_total: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 5 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(swamp, imp, &[shrink3, watcher]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 281).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(watcher, 1), (imp, 1), (shrink3, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), watcher).is_some()
            && drv.find_hand_object(PlayerId(0), imp).is_some()
            && drv.find_hand_object(PlayerId(0), shrink3).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f08_01 combo pieces within wait loop");

    for card in [watcher, imp] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    let imp_bf = drv.find_battlefield_object(PlayerId(0), imp).unwrap();
    let s_h = drv.find_hand_object(PlayerId(0), shrink3).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: imp_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    drv.pass_priority();
    drv.pass_priority();
    // Imp died as -2/-2 (not clamped 0/0), so LkiToughnessLessThan(-1) triggered!
    assert_eq!(drv.session.view_model(None).players[0].life, 25);
    assert!(drv.find_graveyard_object(PlayerId(0), imp).is_some());
}

#[test]
fn t2_f08_02_switch_pt_layer_7d_with_negative_power_causes_sba_death() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_82", "Synth Swamp", "Swamp", ManaColor::B);
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_82", "Synth Bear", "{0}", 1, 3);
    let shrink_p = b.add_card("synth_shp_t2_82", "Synth PowerDrain", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Pump {
                        toughness: Some(Box::new(Amount::Const { n: 0 })),
                        duration: Some(Duration::EndOfTurn),
                        pump_choices: vec![],
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: -3 }),
                )],
            );
    });
    let switch = b.add_card("synth_sw_t2_82", "Synth Twist", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::SwitchPt {
                        duration: Some(Duration::EndOfTurn),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(swamp, bear, &[shrink_p, switch]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 282).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (shrink_p, 1), (switch, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), shrink_p).is_some()
            && drv.find_hand_object(PlayerId(0), switch).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f08_02 combo pieces within wait loop");

    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let b_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    // 1/3 -> -2/3 -> SwitchPt (7d) -> 3/-2 -> dies to SBA CR 704.5f!
    let sh_h = drv.find_hand_object(PlayerId(0), shrink_p).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sh_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: b_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: -2,
            toughness: 3
        })
    );

    let sw_h = drv.find_hand_object(PlayerId(0), switch).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sw_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: b_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(
        drv.find_graveyard_object(PlayerId(0), bear).is_some(),
        "CR 704.5f: -2/3 switched to 3/-2 must die to SBA"
    );
}

#[test]
fn t2_f08_03_hover_breakdown_on_noncreature_land_and_artifact_is_none() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_83", "Synth Plains", "Plains", ManaColor::W);
    let relic = b.add_card("synth_relic_t2_83", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let db = b.build_db();
    let decks = build_60_pair(plains, relic, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 283).unwrap();
    drv.keep_all_hands();

    let l_h = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let r_h = drv.find_hand_object(PlayerId(0), relic).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: r_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let l_bf = drv.find_battlefield_object(PlayerId(0), plains).unwrap();
    let r_bf = drv.find_battlefield_object(PlayerId(0), relic).unwrap();
    for oid in [l_bf, r_bf] {
        let bd = drv.session.hover_breakdown(None, oid).unwrap();
        assert_eq!(bd.base_pt, None);
        assert_eq!(bd.evaluated_pt, None);
    }
}

#[test]
fn t2_f08_04_two_set_base_pt_effects_layer_7b_use_timestamp_order() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t2_84", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_84", "Synth Bear", "{0}", 2, 2);
    let set4 = b.add_card("synth_set4_t2_84", "Synth Set4", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7b,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetBasePt {
                        power: Box::new(Amount::Const { n: 4 }),
                        toughness: Box::new(Amount::Const { n: 4 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let set1 = b.add_card("synth_set1_t2_84", "Synth Set1", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7b,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetBasePt {
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 1 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let db = b.build_db();
    let decks = build_60_pair(w_land, bear, &[set4, set1]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 284).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=79).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (set4, 1), (set1, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), set4).is_some()
            && drv.find_hand_object(PlayerId(0), set1).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f08_04 combo pieces within wait loop");

    for card in [bear, set4, set1] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    let b_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 1,
            toughness: 1
        })
    );
}

#[test]
fn t2_f08_05_zero_effects_fast_path_and_rapid_add_remove_invalidation() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_85", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_85", "Synth Bear", "{0}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_t2_85", "Synth Anthem", "{0}", 1, 1);
    let wipe_ench = b.add_card("synth_wipe_ench_85", "Synth Purge", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Destroy {
                        cant_be_regenerated: false,
                        select: Default::default(),
                    },
                    Selector::All {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ENCHANTMENT,
                            ..Default::default()
                        }),
                    },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(plains, bear, &[anthem, wipe_ench]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 285).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (anthem, 1), (wipe_ench, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), anthem).is_some()
            && drv.find_hand_object(PlayerId(0), wipe_ench).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f08_05 combo pieces within wait loop");

    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let b_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );

    let rev0 = drv.session.state().layer_rev();
    let a_h = drv.find_hand_object(PlayerId(0), anthem).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.session.state().layer_rev() > rev0);
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 3,
            toughness: 3
        })
    );

    let w_h = drv.find_hand_object(PlayerId(0), wipe_ench).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: w_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
}

// ============================================================================
// F9 Boundary & CR 614–616 Cases (5 cases)
// ============================================================================

#[test]
fn t2_f09_01_replacement_condition_false_leaves_event_unmodified() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_91",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let cond_dbl = b.add_card("synth_cdbl_t2_91", "Synth CondFurnace", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DamageReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: DamageScaleSpec {
                        mode: DamageModMode::Double,
                        add: None,
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: Some(Box::new(Condition::ControlsFilter {
                    filter: Box::new(ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ARTIFACT,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    }),
                })),
            });
    });
    let shock = b.add_synth_burn_instant("synth_shock_t2_91", "Synth Shock", "{0}", 2);
    let db = b.build_db();
    let decks = build_60_pair(mtn, cond_dbl, &[shock]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 291).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(cond_dbl, 1), (shock, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), cond_dbl).is_some()
            && drv.find_hand_object(PlayerId(0), shock).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f09_01 combo pieces within wait loop");

    for card in [cond_dbl, shock] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        let tgts = if card == shock {
            vec![TargetRef::Player { id: PlayerId(1) }]
        } else {
            vec![]
        };
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: tgts,
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    assert_eq!(drv.session.view_model(None).players[1].life, 18);
}

#[test]
fn t2_f09_02_zero_damage_does_not_trigger_add_or_consume_shield_cr614_7a() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_92",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let plus2 = b.add_card("synth_p2_t2_92", "Synth Plus2", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DamageReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: DamageScaleSpec {
                        mode: DamageModMode::Add,
                        add: Some(Box::new(Amount::Const { n: 2 })),
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let zero_burn = b.add_synth_burn_instant("synth_zburn_t2_92", "Synth ZeroPing", "{0}", 0);
    let db = b.build_db();
    let decks = build_60_pair(mtn, plus2, &[zero_burn]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 292).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(plus2, 1), (zero_burn, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), plus2).is_some()
            && drv.find_hand_object(PlayerId(0), zero_burn).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f09_02 combo pieces within wait loop");

    let p_h = drv.find_hand_object(PlayerId(0), plus2).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let z_h = drv.find_hand_object(PlayerId(0), zero_burn).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: z_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session.view_model(None).players[1].life,
        20,
        "CR 614.7a / 120.8: 0 damage is not damage; +2 replacement must NOT apply"
    );
}

#[test]
fn t2_f09_03_choose_replacement_out_of_range_index_rejected() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_93",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let a1 = b.add_card("synth_a1_t2_93", "Synth Plus1", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DamageReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: DamageScaleSpec {
                        mode: DamageModMode::Add,
                        add: Some(Box::new(Amount::Const { n: 1 })),
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let a2 = b.add_card("synth_a2_t2_93", "Synth Doubler", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DamageReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DamageScale {
                    spec: DamageScaleSpec {
                        mode: DamageModMode::Double,
                        add: None,
                        noncombat_only: false,
                        combat_only: false,
                        source_filter: None,
                        target_filter: DamageModTarget::Any,
                    },
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let ping = b.add_synth_burn_instant("synth_ping_t2_93", "Synth Ping", "{0}", 2);
    let db = b.build_db();
    let decks = build_60_pair(mtn, a1, &[a2, ping]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 293).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(a1, 1), (a2, 1), (ping, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), a1).is_some()
            && drv.find_hand_object(PlayerId(0), a2).is_some()
            && drv.find_hand_object(PlayerId(0), ping).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f09_03 combo pieces within wait loop");

    for card in [a1, a2] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    let p_h = drv.find_hand_object(PlayerId(0), ping).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseReplacement { prompt_id, .. },
    } = drv.decision()
    {
        let err = drv.step_unchecked(
            PlayerId(1),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 5 },
            },
        );
        assert!(matches!(err, Err(EngineError::IllegalAction(_))));
    } else {
        panic!("expected ChooseReplacement prompt");
    }
}

#[test]
fn t2_f09_04_unpreventable_damage_does_not_deplete_finite_shield_cr615_12() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_94", "Synth Plains", "Plains", ManaColor::W);
    let no_prev = b.add_card("synth_noprev_t2_94", "Synth NoPrevent", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: mtg_ir::RuleMod::DamageCantBePrevented,
                    who: PlayerFilter {
                        relation: PlayerRelation::Any,
                    },
                },
                condition: None,
            });
    });
    let shield = b.add_card("synth_shield_t2_94", "Synth Shield3", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_player()],
                vec![effect_step(
                    EffectOp::PreventDamage {
                        spec: Box::new(DamagePreventionSpec {
                            mode: PreventMode::Next,
                            combat_only: false,
                            scope: PreventScope::Subject,
                            from_source: None,
                            also_prevent_dealt_by_subject: false,
                            choose_source: false,
                            next_damage_event_only: false,
                            on_prevented: None,
                            redirect_to: None,
                            redirect_source_color_filter: ColorMask::empty(),
                            redirect_source_kind: None,
                            min_life_floor: None,
                            all_damage_to_you_and_perms: false,
                            you_and_planeswalkers_you_control: false,
                            from_sources_you_dont_control: false,
                            from_source_colors: ColorMask::empty(),
                            from_source_not_subtypes: Default::default(),
                            on_prevented_creature: None,
                            on_prevented_noncreature: None,
                            on_prevented_to_damage_source_controller: false,
                        }),
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let bolt = b.add_synth_burn_instant("synth_bolt_t2_94", "Synth Bolt", "{0}", 3);
    let destroy = b.add_card("synth_dest_t2_94", "Synth Disenchant", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ENCHANTMENT,
                        ..Default::default()
                    },
                    ..target_creature()
                }],
                vec![effect_step(
                    EffectOp::Destroy {
                        cant_be_regenerated: false,
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(plains, shield, &[no_prev, bolt, destroy]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 294).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=49).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(shield, 1), (no_prev, 1), (bolt, 2), (destroy, 1)],
        )
        .unwrap();
        let bolt_count = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == bolt)
            .count();
        if bolt_count >= 2
            && drv.find_hand_object(PlayerId(0), shield).is_some()
            && drv.find_hand_object(PlayerId(0), no_prev).is_some()
            && drv.find_hand_object(PlayerId(0), destroy).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f09_04 combo pieces within wait loop");

    for card in [shield, no_prev] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        let tgts = if card == shield {
            vec![TargetRef::Player { id: PlayerId(1) }]
        } else {
            vec![]
        };
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: tgts,
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    // Deal 3 unpreventable damage -> P1 takes 3 (17 life); finite 3 shield stays intact!
    let b1 = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b1,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[1].life, 17);

    // Destroy NoPrevent enchantment -> subsequent Bolt 3 IS prevented by remaining 3-shield!
    let np_bf = drv.find_battlefield_object(PlayerId(0), no_prev).unwrap();
    let d_h = drv.find_hand_object(PlayerId(0), destroy).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: d_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: np_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let b2 = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b2,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session.view_model(None).players[1].life,
        17,
        "CR 615.12: unpreventable damage must NOT deplete finite prevention shield"
    );
}

#[test]
fn t2_f09_05_skip_draw_replacements_terminate_cleanly_without_prompt() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t2_95", "Synth Island", "Island", ManaColor::U);
    let skip_draw = b.add_card("synth_skip_t2_95", "Synth SkipDraw", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::Skip,
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            });
    });
    let draw1 = b.add_synth_draw_sorcery("synth_draw_t2_95", "Synth Draw", "{0}", 1);
    let db = b.build_db();
    let decks = build_60_pair(u_land, skip_draw, &[draw1]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 295).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(skip_draw, 2), (draw1, 1)])
            .unwrap();
        let skip_count = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == skip_draw)
            .count();
        if skip_count >= 2 && drv.find_hand_object(PlayerId(0), draw1).is_some() {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f09_05 combo pieces within wait loop");

    for _ in 0..2 {
        let h = drv.find_hand_object(PlayerId(0), skip_draw).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    let hand_before = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    let d_h = drv.find_hand_object(PlayerId(0), draw1).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: d_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    match drv.decision() {
        DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseReplacement {
                    prompt_id,
                    option_count,
                },
        } => {
            assert_eq!(option_count, 2);
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index: 0 },
                },
            )
            .unwrap();
        }
        other => {
            panic!(
                "expected single ChooseReplacement tie prompt for 2 Skip replacements, got {other:?}"
            )
        }
    }
    assert_eq!(
        drv.decision(),
        DecisionState::Priority,
        "CR 616.1f: skipped draw must terminate cleanly without a second replacement prompt"
    );
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        hand_before - 1
    );
}

// ============================================================================
// F10 Boundary & CR 603 Trigger Cases (5 cases)
// ============================================================================

#[test]
fn t2_f10_01_trigger_with_zero_legal_targets_removed_from_stack_cr603_3d() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_101",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let etb_ping_crea = b.add_card("synth_etb_crea_101", "Synth SparkElemental", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::ALL,
                            to: Some(Zone::Battlefield),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                ),
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::CREATURE,
                        controller: Some(RelativeControl::Opponent),
                        ..Default::default()
                    },
                    ..target_creature()
                }],
                vec![effect_step(
                    EffectOp::Destroy {
                        cant_be_regenerated: false,
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(mtn, etb_ping_crea, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 296).unwrap();
    drv.keep_all_hands();

    // Opponent has 0 creatures: trigger has 0 legal targets -> removed from stack cleanly!
    let c_h = drv.find_hand_object(PlayerId(0), etb_ping_crea).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: c_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.session.state().stack().is_empty());
    assert_eq!(drv.decision(), DecisionState::Priority);
}

#[test]
fn t2_f10_02_intervening_if_false_at_trigger_moment_never_queues_cr603_4() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land(
        "synth_plains_t2_102",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let cond_upk = b.add_card("synth_upk5_102", "Synth LowLifeCheck", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability_if(
                simple_trigger(
                    EventKind::PhaseBegin,
                    EventFilter {
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
                ),
                Condition::AnyPlayerLife {
                    op: CompareOp::Le,
                    count: 10,
                },
                vec![],
                vec![effect_step(
                    EffectOp::GainLife {
                        set_life_total: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 5 }),
                )],
            );
    });
    let pay_life = b.add_card("synth_pay15_102", "Synth Blood", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::LoseLife,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 15 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(w_land, cond_upk, &[pay_life]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 297).unwrap();
    drv.keep_all_hands();

    let u_h = drv.find_hand_object(PlayerId(0), cond_upk).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: u_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // At Turn 3 upkeep, life is 20 (> 10), so trigger never queues!
    drv.advance_to(3, GamePhase::Upkeep, 64).unwrap();
    assert!(drv.session.state().stack().is_empty());
}

#[test]
fn t2_f10_03_simultaneous_multi_object_trigger_queue_ordering() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_103", "Synth Swamp", "Swamp", ManaColor::B);
    let watcher = b.add_card("synth_watch_t2_103", "Synth Reaper", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::BATTLEFIELD,
                            to: Some(Zone::Graveyard),
                            from_anywhere: false,
                            not_from_zone: None,
                            not_to_zone: None,
                            enters_untapped: false,
                            dealt_damage_by_self_this_turn: false,
                        }),
                        ..Default::default()
                    },
                ),
                vec![],
                vec![effect_step(
                    EffectOp::GainLife {
                        set_life_total: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let tokens = b.add_card("synth_army_t2_103", "Synth Swarm", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::CreateToken {
                        token: Some(Box::new(mtg_ir::TokenSpec {
                            name: "Synth Bug".into(),
                            power: 1,
                            toughness: 1,
                            colors: ColorMask::B,
                            types: CardTypeMask::CREATURE,
                            supertypes: SupertypeMask::empty(),
                            subtypes: Default::default(),
                            type_line: "Creature".into(),
                            keywords: Default::default(),
                            tapped: false,
                            quoted_abilities: vec![],
                            legendary: false,
                        })),
                        dynamic_pt: None,
                        tapped_and_attacking: false,
                        per_opponent_tapped_attacking: None,
                        multiply_by_opponents_did_this_way: false,
                        benthic_anomaly_clone: false,
                    },
                    Selector::None,
                    Some(Amount::Const { n: 5 }),
                )],
            );
    });
    let wipe = b.add_card("synth_wipe_t2_103", "Synth Doom", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Destroy {
                        cant_be_regenerated: false,
                        select: Default::default(),
                    },
                    Selector::All {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(swamp, watcher, &[tokens, wipe]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 298).unwrap();
    drv.keep_all_hands();

    for card in [watcher, watcher, tokens, wipe] {
        let h = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
    }
    // Resolve all 10 queued triggers (2 watchers * 5 dying tokens):
    drv.advance_to(1, GamePhase::BeginCombat, 64).unwrap();
    assert_eq!(drv.session.view_model(None).players[0].life, 30);
}

#[test]
fn t2_f10_04_once_each_turn_trigger_suppression() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land(
        "synth_island_t2_104",
        "Synth Island",
        "Island",
        ManaColor::U,
    );
    let second_draw = b.add_card("synth_2nd_t2_104", "Synth SecondDraw", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                mtg_ir::TriggerDef {
                    only_once_per_turn: true,
                    ..simple_trigger(EventKind::DrawCard, EventFilter::default())
                },
                vec![],
                vec![effect_step(
                    EffectOp::GainLife {
                        set_life_total: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let draw1 = b.add_synth_draw_sorcery("synth_draw_t2_104", "Synth Opt", "{0}", 1);
    let db = b.build_db();
    let decks = build_60_pair(u_land, second_draw, &[draw1]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 299).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=79).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(second_draw, 1), (draw1, 2)])
            .unwrap();
        let draw_count = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == draw1)
            .count();
        if draw_count >= 2 && drv.find_hand_object(PlayerId(0), second_draw).is_some() {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t2_f10_04 combo pieces within wait loop");

    let e_h = drv.find_hand_object(PlayerId(0), second_draw).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: e_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // Draw twice in same turn: trigger fires ONLY ONCE (+2 life, not +4!).
    for _ in 0..2 {
        let d_h = drv.find_hand_object(PlayerId(0), draw1).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: d_h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
        if !drv.session.state().stack().is_empty() {
            drv.pass_priority();
            drv.pass_priority();
        }
    }
    assert_eq!(drv.session.view_model(None).players[0].life, 22);
}

#[test]
fn t2_f10_05_delayed_trigger_expires_after_one_firing_cr603_7() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_105",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let delayed_spell = b.add_card("synth_del_t2_105", "Synth EotBolt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::DelayedTrigger {
                        delayed: Some(mtg_ir::DelayedTriggerPayload {
                            trigger: Box::new(simple_trigger(
                                EventKind::PhaseBegin,
                                EventFilter {
                                    detail: Some(EventDetail::Phase {
                                        phase: Some(GamePhase::EndStep),
                                        any_main_phase: false,
                                        enchanted_controller_upkeep: false,
                                        enchanted_player_upkeep: false,
                                        enchanted_opponent_end_step: false,
                                        phase_chosen_player: false,
                                        turn_filter: None,
                                    }),
                                    ..Default::default()
                                },
                            )),
                            effect: vec![effect_step(
                                EffectOp::LoseLife,
                                Selector::PlayerRef {
                                    player: PlayerRef::You,
                                },
                                Some(Amount::Const { n: 3 }),
                            )],
                            targets: vec![],
                            next_turn: false,
                        }),
                        inline_delayed: None,
                        return_target_and_sacrificed_at_upkeep: false,
                    },
                    Selector::None,
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(mtn, delayed_spell, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 300).unwrap();
    drv.keep_all_hands();

    let h = drv.find_hand_object(PlayerId(0), delayed_spell).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // Turn 1 EndStep -> loses 3 life (17). Turn 2 EndStep -> delayed trigger expired (stays 17!).
    drv.advance_to(2, GamePhase::Main1, 64).unwrap();
    assert_eq!(drv.session.view_model(None).players[0].life, 17);
    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    assert_eq!(drv.session.view_model(None).players[0].life, 17);
}
