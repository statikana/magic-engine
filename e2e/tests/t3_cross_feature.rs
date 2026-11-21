//! Tier 3 Opaque-Box E2E Tests: Cross-Feature Combinations (`T3-01` .. `T3-20`).
//!
//! Covers 20 multi-feature / pairwise CR interaction scenarios.

use std::sync::Arc;

use mtg_engine::{
    Action, AttackTarget, CastRequest, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec,
    EngineError, GameConfig, MacroAction, MatchSession, PlayerId, PowerToughness, RngMode,
    RngState, TargetRef,
};
use mtg_engine_e2e::assertions::{
    assert_observation_consistency, assert_rename_invariance_with, assert_view_model_redaction,
};
use mtg_engine_e2e::cli::{
    TempDir, repo_root, run_build_db, run_diff_test, run_inspect_db, run_node_script,
};
use mtg_engine_e2e::driver::SessionDriver;
use mtg_engine_e2e::synth::{
    SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
};
use mtg_ir::{
    Affected, Amount, CardDatabase, CardDefId, CardTypeMask, ColorMask, Compare, CompareOp,
    CostDelta, CostDeltaMode, DamageModMode, DamageModTarget, DamagePreventionSpec,
    DamageScaleSpec, DrawScaleSpec, Duration, EffectOp, EffectStep, EtbCopySpec, EventDetail,
    EventFilter, EventKind, EventPattern, FilterFlags, GamePhase, Layer, LayerMod, ManaColor,
    ManaSpendingRestriction, ObjectFilter, PlainManaFilter, PlayerFilter, PlayerRef,
    PlayerRelation, PreventMode, PreventScope, ReplacementDef, ReplacementWith, Selector,
    SpellOrAbilityFilter, StaticBody, StaticDef, SupertypeMask, TargetKind, TargetSpec, Zone,
    ZoneChangeMod, ZoneMask,
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

fn find_land(drv: &SessionDriver, p: PlayerId) -> Option<mtg_engine::ObjectId> {
    drv.session.view_model(Some(p)).players[usize::from(p.0)]
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::LAND))
        .map(|o| o.id)
}

#[test]
fn t3_01_layers_plus_etb_counter_replacement_cr613_cr614_12() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t3_01", "Synth Plains", "Plains", ManaColor::W);
    let plus1 = b.intern_counter("+1/+1");
    let suppressor = b.add_card("synth_supp_t3_01", "Synth Suppressor", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                    active_in_zone: None,
                },
                condition: None,
            })
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
    let etb_counter_maker = b.add_card("synth_etb_cnt_t3_01", "Synth Forge", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((plus1, Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let flyer =
        b.add_synth_keyword_creature("synth_fly_t3_01", "Synth Angel", "{0}", 4, 4, &["flying"]);
    let db = b.build_db();
    let decks = build_60_pair(w_land, flyer, &[suppressor, etb_counter_maker]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 301).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(suppressor, 1), (etb_counter_maker, 1), (flyer, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), suppressor).is_some()
            && drv
                .find_hand_object(PlayerId(0), etb_counter_maker)
                .is_some()
            && drv.find_hand_object(PlayerId(0), flyer).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t3_01 combo pieces within wait loop");

    for card in [suppressor, etb_counter_maker, flyer] {
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
    let f_bf = drv.find_battlefield_object(PlayerId(0), flyer).unwrap();
    let view = drv.battlefield_view(PlayerId(0), f_bf).unwrap();
    assert_eq!(view.counters.iter().map(|(_, n)| *n).sum::<u32>(), 1);
    assert_eq!(
        drv.session
            .hover_breakdown(None, f_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
}

#[test]
fn t3_02_zone_funnel_pre_batch_lki_and_bind_as() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t3_02", "Synth Swamp", "Swamp", ManaColor::B);
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_02", "Synth Bear", "{0}", 2, 2);
    let lord = b.add_card("synth_lord_t3_02", "Synth BuffLord", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            flags: FilterFlags::OTHER,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 2 }),
                        toughness: Box::new(Amount::Const { n: 2 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let watcher = b.add_card("synth_watch_t3_02", "Synth BigDeathWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                power: Some(Compare {
                                    op: CompareOp::Ge,
                                    value: Box::new(Amount::Const { n: 4 }),
                                }),
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
                    Some(Amount::Const { n: 4 }),
                )],
            );
    });
    let wipe = b.add_card("synth_wipe_t3_02", "Synth Wipe", |c| {
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
    let decks = build_60_pair(swamp, bear, &[lord, watcher, wipe]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 302).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=49).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(watcher, 1), (bear, 1), (lord, 1), (wipe, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), watcher).is_some()
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), lord).is_some()
            && drv.find_hand_object(PlayerId(0), wipe).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw t3_02 combo pieces within wait loop");

    for card in [watcher, bear, lord, wipe] {
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
    drv.pass_priority();
    drv.pass_priority();
    // Bear had pre-batch LKI power 4 (2 + 2 from Lord), so Watcher triggers and gains 4 life!
    assert_eq!(drv.session.view_model(None).players[0].life, 24);
}

#[test]
fn t3_03_commander_21_damage_sba_fixpoint_and_layer_debuff() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t3_03", "Synth Swamp", "Swamp", ManaColor::B);
    let cmd = b.add_card("synth_cmd_t3_03", "Synth GiantCmd", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(21, 3)
            .keyword("haste");
    });
    let debuff = b.add_synth_anthem_enchantment("synth_debuff_t3_03", "Synth Curse", "{0}", 0, -2);
    let db = b.build_db();
    let mut main = vec![swamp; 20];
    main.extend(vec![debuff; 79]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![d.clone(), d.clone(), d.clone(), d],
        303,
    )
    .unwrap();
    drv.keep_all_hands();

    let cmd_id = drv.find_command_object(PlayerId(0), cmd).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cmd_id,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    for _ in 0..4 {
        drv.pass_priority();
    }
    let cmd_bf = drv.find_battlefield_object(PlayerId(0), cmd).unwrap();
    let d_h = drv
        .find_hand_object(PlayerId(0), debuff)
        .expect("Synth Curse must be in hand");
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
    for _ in 0..4 {
        drv.pass_priority();
    }
    assert_eq!(
        drv.session
            .hover_breakdown(None, cmd_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 21,
            toughness: 1
        })
    );

    drv.advance_to(1, GamePhase::DeclareAttackers, 16).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(cmd_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(1, GamePhase::Main2, 32).unwrap();
    assert!(drv.session.view_model(None).players[1].lost);
    assert!(!matches!(drv.decision(), DecisionState::GameOver { .. }));
}

#[test]
fn t3_04_restricted_mana_casting_sequence_and_target_fizzle() {
    let mut b = SynthBundleBuilder::new();
    let orchard = b.add_card("synth_orch_t3_04", "Synth Orchard", |c| {
        c.types(CardTypeMask::LAND).restricted_mana_ability(
            "tap_crea_g",
            &[ManaColor::G],
            Some(Box::new(ManaSpendingRestriction {
                allow_spells: Some(mtg_ir::AllowSpellsRestriction {
                    filter: Some(PlainManaFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        );
    });
    let draw_sorc = b.add_synth_draw_sorcery("synth_draw_t3_04", "Synth Draw", "{G}", 1);
    let bounce = b.add_card("synth_bounce_t3_04", "Synth Bounce", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Bounce {
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let target_bear = b.add_synth_vanilla_creature("synth_bear_t3_04", "Synth Bear", "{0}", 2, 2);
    let target_etb_crea = b.add_card("synth_tcrea_t3_04", "Synth Striker", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
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
                vec![target_creature()],
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
    let mut main0 = vec![orchard; 15];
    main0.extend(vec![draw_sorc; 15]);
    main0.extend(vec![target_etb_crea; 15]);
    main0.extend(vec![bounce; 15]);
    let mut main1 = vec![orchard; 20];
    main1.extend(vec![target_bear; 40]);
    let d0 = DeckSpec {
        commanders: vec![],
        mainboard: main0,
        sideboard: vec![],
    };
    let d1 = DeckSpec {
        commanders: vec![],
        mainboard: main1,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 304).unwrap();
    drv.keep_all_hands();

    // Turn 1: Play Orchard, confirm restricted {G} cannot pay for non-creature draw_sorc.
    let l = find_land(&drv, PlayerId(0)).expect("Synth Orchard must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let draw_h = drv
        .find_hand_object(PlayerId(0), draw_sorc)
        .expect("Synth Draw must be in hand");
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: draw_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(
        !prev.can_pay,
        "CR 106.6: restricted creature-only mana source cannot pay for sorcery"
    );
    let l_bf = drv.find_battlefield_object(PlayerId(0), orchard).unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: l_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    let restricted_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: draw_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(restricted_res, Err(EngineError::IllegalAction(_))),
        "CR 106.6: spending creature-restricted mana on sorcery must be rejected with IllegalAction, got {restricted_res:?}"
    );

    // Turn 2: P1 casts Synth Bear so P0 has an ETB target.
    drv.advance_to(2, GamePhase::Main1, 32).unwrap();
    let tb_h = drv
        .find_hand_object(PlayerId(1), target_bear)
        .expect("Synth Bear must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: tb_h,
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
    let tb_bf = drv
        .find_battlefield_object(PlayerId(1), target_bear)
        .unwrap();

    // Turn 3: P0 casts Synth Striker via BeginCast CastingSequence using restricted {G}.
    for turn in [3u32, 5, 7] {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(target_etb_crea, 1), (bounce, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), target_etb_crea).is_some()
            && drv.find_hand_object(PlayerId(0), bounce).is_some()
        {
            break;
        }
    }
    let c_h = drv
        .find_hand_object(PlayerId(0), target_etb_crea)
        .expect("Synth Striker must be in hand");
    drv.step(
        PlayerId(0),
        Action::BeginCast {
            object: c_h,
            option_id: 0,
            from_zone: Zone::Hand,
        },
    )
    .unwrap();
    let mut guard = 0;
    while matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        let actor = drv.actor().expect("casting substep must have actor");
        let legal = drv.session.legal_actions(actor);
        let act = legal
            .iter()
            .find(|a| matches!(a, Action::TapMana { .. }))
            .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
            .expect("casting substep must expose non-concede action")
            .clone();
        drv.step(actor, act).unwrap();
        guard += 1;
        assert!(guard <= 12, "BeginCast CastingSequence did not complete");
    }
    drv.pass_priority();
    drv.pass_priority();
    assert!(
        drv.find_battlefield_object(PlayerId(0), target_etb_crea)
            .is_some()
    );

    // Select tb_bf as target for Synth Striker's ETB trigger, then bounce tb_bf in response!
    let mut etb_guard = 0;
    while matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        let actor = drv.actor().expect("ETB target prompt must have actor");
        if let DecisionState::Prompt {
            request: ChoiceRequest::ChooseObjects { prompt_id, .. },
        } = drv.decision()
        {
            drv.step(
                actor,
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: tb_bf },
                },
            )
            .unwrap();
        } else {
            let legal = drv.session.legal_actions(actor);
            let act = legal
                .iter()
                .find(|a| {
                    !matches!(a, Action::Concede)
                        && format!("{a:?}").contains(&format!("{tb_bf:?}"))
                })
                .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
                .unwrap()
                .clone();
            drv.step(actor, act).unwrap();
        }
        etb_guard += 1;
        assert!(etb_guard <= 8, "ETB target selection did not complete");
    }
    assert_eq!(drv.session.state().stack().len(), 1);

    let bn_h = drv
        .find_hand_object(PlayerId(0), bounce)
        .expect("Synth Bounce must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bn_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: tb_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    // Bounce resolved; now resolve ETB trigger whose target became illegal -> CR 608.2b fizzle!
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.session.state().stack().is_empty());
    assert!(
        drv.find_battlefield_object(PlayerId(1), target_bear)
            .is_none()
    );
    assert!(
        drv.find_graveyard_object(PlayerId(1), target_bear)
            .is_none()
    );
    assert!(drv.find_hand_object(PlayerId(1), target_bear).is_some());
}

#[test]
fn t3_05_rename_invariance_layers_and_legend_rule() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t3_05", "Synth Plains", "Plains", ManaColor::W);
    let leg = b.add_card("synth_leg_t3_05", "Synth Legend", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let anthem = b.add_synth_anthem_enchantment("synth_anth_t3_05", "Synth Anthem", "{0}", 1, 1);
    let bundle_a = b.build_bundle();
    let bundle_b = b.clone_renamed("_renamed");
    let mut main = vec![land; 12];
    main.extend(vec![leg; 24]);
    main.extend(vec![anthem; 24]);
    let d = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let decks = vec![d.clone(), d];
    assert_rename_invariance_with(
        bundle_a,
        bundle_b,
        &GameConfig::standard_1v1(),
        &decks,
        &decks,
        305,
        |drv| {
            drv.keep_all_hands();
            for turn in [1u32, 3, 5, 7] {
                drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(leg, 2), (anthem, 1)])
                    .unwrap();
                let leg_cnt = drv.session.view_model(Some(PlayerId(0))).players[0]
                    .hand
                    .iter()
                    .filter(|o| o.card_def == leg)
                    .count();
                if leg_cnt >= 2 && drv.find_hand_object(PlayerId(0), anthem).is_some() {
                    break;
                }
            }
            let a_h = drv
                .find_hand_object(PlayerId(0), anthem)
                .expect("Synth Anthem must be in hand");
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

            for _ in 0..2 {
                let l_h = drv
                    .find_hand_object(PlayerId(0), leg)
                    .expect("Synth Legend must be in hand");
                drv.macro_step(
                    PlayerId(0),
                    MacroAction::CastSpell {
                        object: l_h,
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

            match drv.decision() {
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } => {
                    assert_eq!(candidates.len(), 2);
                    drv.step(
                        PlayerId(0),
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickObject { id: candidates[0] },
                        },
                    )
                    .unwrap();
                }
                other => panic!("expected Legend Rule ChooseObjects prompt, got {other:?}"),
            }
            let leg_bf = drv
                .find_battlefield_object(PlayerId(0), leg)
                .expect("one Synth Legend must remain on battlefield");
            assert_eq!(
                drv.session
                    .hover_breakdown(None, leg_bf)
                    .unwrap()
                    .evaluated_pt,
                Some(PowerToughness {
                    power: 3,
                    toughness: 3
                })
            );
            assert!(drv.find_graveyard_object(PlayerId(0), leg).is_some());
        },
    );
}

#[test]
fn t3_06_phasing_updates_layer_cache_and_restores_on_phase_in() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t3_06", "Synth Island", "Island", ManaColor::U);
    let phaser_lord = b.add_card("synth_plord_t3_06", "Synth PhaseLord", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword("phasing")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            flags: FilterFlags::OTHER,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 2 }),
                        toughness: Box::new(Amount::Const { n: 2 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_06", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();
    let decks = build_60_pair(u_land, bear, &[phaser_lord]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 306).unwrap();
    drv.keep_all_hands();

    let mut cast_turn = 1u32;
    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(bear, 1), (phaser_lord, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), phaser_lord).is_some()
        {
            cast_turn = turn;
            found = true;
            break;
        }
    }
    assert!(found, "must draw bear and phaser_lord within wait loop");

    for card in [bear, phaser_lord] {
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
            power: 4,
            toughness: 4
        })
    );
    // Next P0 turn: PhaseLord phases out -> Bear drops to 2/2. Following P0 turn: PhaseLord phases back in -> 4/4!
    drv.advance_to(cast_turn + 2, GamePhase::Main1, 64).unwrap();
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
    drv.advance_to(cast_turn + 4, GamePhase::Main1, 64).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, b_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 4,
            toughness: 4
        })
    );
}

#[test]
fn t3_07_auto_tap_cost_modifiers_preview_and_macro_cast() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t3_07", "Synth Island", "Island", ManaColor::U);
    let reducer = b.add_card("synth_red_t3_07", "Synth Prism", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(ObjectFilter::default()),
                        alt_cost_keyword: None,
                    },
                    delta: CostDelta {
                        mode: CostDeltaMode::Reduce,
                        generic: 2,
                        per: None,
                        additional_costs: vec![],
                        colored: Default::default(),
                        hybrid_colored: Default::default(),
                        colored_only: false,
                        min_total_mana: None,
                        max_reduction: None,
                        per_target_life: None,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::BATTLEFIELD,
                },
                condition: None,
            });
    });
    let spell = b.add_synth_draw_sorcery("synth_draw_t3_07", "Synth Insight", "{2}{U}", 2);
    let db = b.build_db();
    let decks = build_60_pair(u_land, reducer, &[spell]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 307).unwrap();
    drv.keep_all_hands();

    let l = find_land(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let r_h = drv.find_hand_object(PlayerId(0), reducer).unwrap();
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

    let s_h = drv.find_hand_object(PlayerId(0), spell).unwrap();
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: s_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(prev.can_pay);
    assert_eq!(prev.sources_to_tap.len(), 1);
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
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

#[test]
fn t3_08_pausable_resolution_frame_prompt_and_concede_clears_cleanly() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t3_08", "Synth Island", "Island", ManaColor::U);
    let scry = b.add_card("synth_scry_t3_08", "Synth Scry", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Scry,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(u_land, scry, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 308).unwrap();
    drv.keep_all_hands();

    let s_h = drv.find_hand_object(PlayerId(0), scry).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
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
    assert_eq!(drv.session.state().stack().len(), 1);
    assert!(matches!(drv.decision(), DecisionState::Prompt { .. }));
    drv.step(PlayerId(0), Action::Concede).unwrap();
    assert_eq!(
        drv.decision(),
        DecisionState::GameOver {
            winner: Some(PlayerId(1))
        }
    );
}

#[test]
fn t3_09_copy_control_etb_self_replacement_and_layer_1a_2() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t3_09", "Synth Island", "Island", ManaColor::U);
    let thief = b.add_card("synth_thief_t3_09", "Synth EtbSteal", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        enter_under_opponent_control: true,
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_09", "Synth Bear", "{0}", 4, 4);
    let clone = b.add_card("synth_clone_t3_09", "Synth Clone", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(0, 0)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        copy: Some(EtbCopySpec {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            optional: false,
                            from_graveyards: false,
                            tapped_if_copied: false,
                            modifications: None,
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let self_draw = b.add_card("synth_sdraw_t3_09", "Synth SelfDraw", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Draw {
                        up_to: false,
                        reveal_drawn: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            )
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: None,
                        delta: Some(2),
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: true,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            });
    });
    let db = b.build_db();
    let mut main = vec![u_land; 12];
    main.extend(vec![bear; 12]);
    main.extend(vec![thief; 12]);
    main.extend(vec![clone; 12]);
    main.extend(vec![self_draw; 12]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        309,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in (1u32..=15).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (thief, 1), (clone, 1), (self_draw, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), thief).is_some()
            && drv.find_hand_object(PlayerId(0), clone).is_some()
            && drv.find_hand_object(PlayerId(0), self_draw).is_some()
        {
            break;
        }
    }

    // 1. Cast Bear (4/4) under P0's control.
    let b_h = drv
        .find_hand_object(PlayerId(0), bear)
        .expect("Synth Bear must be in hand");
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

    // 2. Cast Thief (616.1b enter_under_opponent_control).
    let t_h = drv
        .find_hand_object(PlayerId(0), thief)
        .expect("Synth EtbSteal must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: t_h,
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

    // 3. Cast Clone: 616.1b steals to P1, then 616.1c copies Bear -> 4/4 under P1 (Layer 1a + Layer 2)!
    let cl_h = drv
        .find_hand_object(PlayerId(0), clone)
        .expect("Synth Clone must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cl_h,
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
            request: ChoiceRequest::ChooseObjects { prompt_id, .. },
        } => {
            drv.step(
                PlayerId(1),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: b_bf },
                },
            )
            .unwrap();
        }
        other => panic!("expected ETB copy ChooseObjects prompt for P1, got {other:?}"),
    }
    let cl_bf = drv
        .find_battlefield_object(PlayerId(1), clone)
        .expect("Clone must enter under P1's control");
    assert_eq!(
        drv.session
            .hover_breakdown(None, cl_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 4,
            toughness: 4
        })
    );

    // 4. Cast self_draw: CR 616.1a self_replacement adds +2 to Draw(1) -> draws 3 cards!
    let hand_before = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    let sd_h = drv
        .find_hand_object(PlayerId(0), self_draw)
        .expect("Synth SelfDraw must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sd_h,
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
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        hand_before + 2
    );
}

#[test]
fn t3_10_signed_unclamped_pt_combat_damage_sba_and_hover_breakdown() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t3_10", "Synth Swamp", "Swamp", ManaColor::B);
    let scout =
        b.add_synth_keyword_creature("synth_scout_t3_10", "Synth Scout", "{0}", 1, 2, &["haste"]);
    let blocker = b.add_synth_vanilla_creature("synth_blk_t3_10", "Synth Squire", "{0}", 1, 1);
    let shrink = b.add_card("synth_sh_t3_10", "Synth Shrink4", |c| {
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
                    Some(Amount::Const { n: -4 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(swamp, scout, &[blocker, shrink]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 310).unwrap();
    drv.keep_all_hands();

    let mut p1_turn = 2u32;
    let mut found_blk = false;
    for turn in (2u32..=48).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(blocker, 1), (scout, 1), (shrink, 1)],
        )
        .unwrap();
        if let Some(blk_h) = drv.find_hand_object(PlayerId(1), blocker) {
            drv.macro_step(
                PlayerId(1),
                MacroAction::CastSpell {
                    object: blk_h,
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
            p1_turn = turn;
            found_blk = true;
            break;
        }
    }
    assert!(found_blk, "P1 must draw blocker within wait loop");
    let blk_bf = drv.find_battlefield_object(PlayerId(1), blocker).unwrap();

    let mut atk_turn = p1_turn + 1;
    let mut found_p0 = false;
    for turn in ((p1_turn + 1)..=(p1_turn + 40)).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(scout, 1), (shrink, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), scout).is_some()
            && drv.find_hand_object(PlayerId(0), shrink).is_some()
        {
            atk_turn = turn;
            found_p0 = true;
            break;
        }
    }
    assert!(found_p0, "P0 must draw scout and shrink within wait loop");

    let sc_h = drv.find_hand_object(PlayerId(0), scout).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sc_h,
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
    let sc_bf = drv.find_battlefield_object(PlayerId(0), scout).unwrap();

    let sh_h = drv.find_hand_object(PlayerId(0), shrink).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sh_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: sc_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session
            .hover_breakdown(None, sc_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: -3,
            toughness: 2
        })
    );

    drv.advance_to(atk_turn, GamePhase::DeclareAttackers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(sc_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::DeclareBlockers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareBlockers {
            assignments: vec![(blk_bf, sc_bf)],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::Main2, 32).unwrap();
    // 1/1 blocker takes 0 damage from -3/2 and survives; -3/2 takes 1 damage and survives!
    assert!(drv.find_battlefield_object(PlayerId(1), blocker).is_some());
    assert!(drv.find_battlefield_object(PlayerId(0), scout).is_some());
}

#[test]
fn t3_11_prototype_mv_filter_stack_battlefield_vs_graveyard() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t3_11", "Synth Plains", "Plains", ManaColor::W);
    let proto = b.add_card("synth_proto_t3_11", "Synth Proto", |c| {
        c.mana_cost("{7}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(7, 7)
            .prototype_spec("{W}", 3, 3);
    });
    let reanim7 = b.add_card("synth_rean_t3_11", "Synth ReanimateBig", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::GRAVEYARD,
                        types_any: CardTypeMask::CREATURE,
                        mana_value: Some(Compare {
                            op: CompareOp::Ge,
                            value: Box::new(Amount::Const { n: 6 }),
                        }),
                        ..Default::default()
                    },
                    ..target_creature()
                }],
                vec![effect_step(
                    EffectOp::Move {
                        from: Some(Zone::Graveyard),
                        to: Some(Zone::Battlefield),
                        select: Default::default(),
                        tapped: false,
                        under_your_control: true,
                        topmost_only: false,
                        may_shuffle: false,
                        position: None,
                        from_top: None,
                        library_choice_by: None,
                        random_order: false,
                        entered_transformed: false,
                        entered_flipped: false,
                        enters_tapped_and_attacking: false,
                        as_enchantment_only: false,
                        unearth: false,
                        with_counters: None,
                        attached_to: None,
                        condition: None,
                        choose_at_random_from_exiled_with_self: false,
                        leave_chosen_in_graveyard_rest_to_hand: false,
                        split_exiled_cost_cards_to_bottom_and_battlefield: false,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let kill = b.add_synth_burn_instant("synth_kill_t3_11", "Synth Snipe", "{0}", 3);
    let db = b.build_db();
    let decks = build_60_pair(w_land, proto, &[reanim7, kill]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 311).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(w_land, 1), (proto, 1), (kill, 1), (reanim7, 1)],
        )
        .unwrap();
        if !land_played && let Some(l) = find_land(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played
            && drv.find_hand_object(PlayerId(0), proto).is_some()
            && drv.find_hand_object(PlayerId(0), kill).is_some()
            && drv.find_hand_object(PlayerId(0), reanim7).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must draw land and t3_11 combo pieces within wait loop"
    );
    let p_h = drv.find_hand_object(PlayerId(0), proto).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_h,
            option_id: 1,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let p_bf = drv.find_battlefield_object(PlayerId(0), proto).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, p_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 3,
            toughness: 3
        })
    );

    let k_h = drv.find_hand_object(PlayerId(0), kill).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: k_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: p_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // In Graveyard, Proto reverts to full MV 7 -> legal target for ReanimateBig (MV >= 6)!
    let p_gy = drv.find_graveyard_object(PlayerId(0), proto).unwrap();
    let r_h = drv.find_hand_object(PlayerId(0), reanim7).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: r_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: p_gy }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let p_rebf = drv.find_battlefield_object(PlayerId(0), proto).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, p_rebf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 7,
            toughness: 7
        })
    );
}

#[test]
fn t3_12_mulberry32_compat_replay_and_snapshot_equivalence() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t3_12", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();
    let mut cfg = GameConfig::standard_1v1();
    cfg.rng_mode = RngMode::Mulberry32Compat;
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 60],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(Arc::clone(&db), cfg, vec![d.clone(), d], 312).unwrap();
    drv.keep_all_hands();
    drv.advance_to(3, GamePhase::Main1, 32).unwrap();

    #[cfg(feature = "m5-session-ext")]
    {
        let snap = drv.session.snapshot_bytes().unwrap();
        let from_snap = MatchSession::from_snapshot(Arc::clone(&db), &snap).unwrap();
        let from_rep = MatchSession::from_replay(db, drv.session.replay_log().clone()).unwrap();
        assert_eq!(drv.session.view_model(None), from_snap.view_model(None));
        assert_eq!(drv.session.view_model(None), from_rep.view_model(None));
    }
    #[cfg(not(feature = "m5-session-ext"))]
    {
        assert!(!drv.session.replay_log().entries.is_empty());
        panic!("requires M5 MatchSession extensions (--features m5-session-ext)");
    }
}

#[test]
fn t3_13_determinize_and_observation_consistency_across_10_seeds() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t3_13", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_13", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();
    let decks = build_60_pair(land, bear, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 313).unwrap();
    drv.keep_all_hands();
    assert_view_model_redaction(&drv.session);
    assert_observation_consistency(&drv.session, PlayerId(0));

    let orig = drv.session.state();
    let orig_p0_hand = orig.players()[0].hand.clone();
    let orig_p0_lib_len = orig.players()[0].library.len();
    let orig_opp_hand_defs: Vec<_> = orig.players()[1]
        .hand
        .iter()
        .map(|&id| orig.object(id).unwrap().card_def)
        .collect();
    let orig_opp_lib_defs: Vec<_> = orig.players()[1]
        .library
        .iter()
        .map(|&id| orig.object(id).unwrap().card_def)
        .collect();

    let mut samples = Vec::with_capacity(10);
    for seed in 1u64..=10 {
        let mut rng = RngState::splitmix64(seed * 7919);
        let det = drv.session.determinize(PlayerId(0), &mut rng);
        assert_eq!(det.players()[0].hand, orig_p0_hand);
        assert_eq!(det.players()[0].library.len(), orig_p0_lib_len);
        assert_eq!(det.players()[1].hand.len(), 7);
        assert_eq!(det.players()[1].library.len(), 53);
        assert!(det.players()[0].battlefield.is_empty());
        assert!(det.players()[1].battlefield.is_empty());

        let opp_hand_defs: Vec<_> = det.players()[1]
            .hand
            .iter()
            .map(|&id| det.object(id).unwrap().card_def)
            .collect();
        let opp_lib_defs: Vec<_> = det.players()[1]
            .library
            .iter()
            .map(|&id| det.object(id).unwrap().card_def)
            .collect();
        let land_cnt = opp_hand_defs
            .iter()
            .chain(opp_lib_defs.iter())
            .filter(|&&cid| cid == land)
            .count();
        let bear_cnt = opp_hand_defs
            .iter()
            .chain(opp_lib_defs.iter())
            .filter(|&&cid| cid == bear)
            .count();
        assert_eq!((land_cnt, bear_cnt), (24, 36));
        samples.push((opp_hand_defs, opp_lib_defs));
    }
    assert!(
        samples.windows(2).any(|w| w[0] != w[1]),
        "determinize() must resample opponent hidden zones across 10 seeds"
    );
    assert!(
        samples
            .iter()
            .any(|(h, l)| h != &orig_opp_hand_defs || l != &orig_opp_lib_defs),
        "determinize() must differ from original unresampled opponent hidden zones"
    );
}

#[test]
fn t3_14_ir_v2_cli_build_decode_validate_commander_and_boot() {
    let tmp = TempDir::new("t3_14");
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t3_14", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_card("synth_cmd_t3_14", "Synth Commander", |c| {
        c.mana_cost("{1}{W}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(3, 3);
    });
    let j = tmp.child("cmd.json");
    let bin = tmp.child("cmd.bin");
    std::fs::write(&j, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);
    assert!(run_inspect_db(&bin, false).success);

    let db = Arc::new(CardDatabase::decode_bytes(&std::fs::read(&bin).unwrap()).unwrap());
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![plains; 99],
        sideboard: vec![],
    };
    let rep = MatchSession::validate_deck(&db, &GameConfig::commander(2), &d);
    assert!(rep.is_legal, "errors={:?}", rep.errors);
    let s = MatchSession::new(db, GameConfig::commander(2), vec![d.clone(), d], 314).unwrap();
    assert_eq!(s.view_model(None).players.len(), 2);
}

#[test]
fn t3_15_reflexive_and_delayed_triggers_with_bind_as() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t3_15", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_15", "Synth Bear", "{0}", 2, 2);
    let blink = b.add_card("synth_blink_t3_15", "Synth Ephemerate", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![
                    EffectStep {
                        bind_as: Some("ex".into()),
                        ..effect_step(
                            EffectOp::Exile {
                                select: Default::default(),
                                duration: None,
                                until_source_leaves: false,
                                until_opponent_becomes_monarch: false,
                                exile_all_except_bottom: false,
                                face_down: false,
                                look_only: false,
                                exile_with_counters: None,
                            },
                            Selector::Target { index: 0 },
                            None,
                        )
                    },
                    effect_step(
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
                                    EffectOp::Move {
                                        from: Some(Zone::Exile),
                                        to: Some(Zone::Battlefield),
                                        select: Default::default(),
                                        tapped: false,
                                        under_your_control: true,
                                        topmost_only: false,
                                        may_shuffle: false,
                                        position: None,
                                        from_top: None,
                                        library_choice_by: None,
                                        random_order: false,
                                        entered_transformed: false,
                                        entered_flipped: false,
                                        enters_tapped_and_attacking: false,
                                        as_enchantment_only: false,
                                        unearth: false,
                                        with_counters: None,
                                        attached_to: None,
                                        condition: None,
                                        choose_at_random_from_exiled_with_self: false,
                                        leave_chosen_in_graveyard_rest_to_hand: false,
                                        split_exiled_cost_cards_to_bottom_and_battlefield: false,
                                    },
                                    Selector::Binding { name: "ex".into() },
                                    None,
                                )],
                                targets: vec![],
                                next_turn: false,
                            }),
                            inline_delayed: None,
                            return_target_and_sacrificed_at_upkeep: false,
                        },
                        Selector::None,
                        None,
                    ),
                ],
            );
    });
    let db = b.build_db();
    let decks = build_60_pair(w_land, bear, &[blink]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 315).unwrap();
    drv.keep_all_hands();

    let mut cast_turn = 1u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(bear, 1), (blink, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), blink).is_some()
        {
            cast_turn = turn;
            found = true;
            break;
        }
    }
    assert!(found, "must draw bear and blink within wait loop");

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

    let bl_h = drv.find_hand_object(PlayerId(0), blink).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bl_h,
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
    assert!(drv.find_exile_object(PlayerId(0), bear).is_some());

    drv.advance_to(cast_turn + 1, GamePhase::Main1, 32).unwrap();
    let returned = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert!(returned.generation > b_bf.generation);
}

#[test]
fn t3_16_prevention_shield_plus_damage_doubler_and_lifelink() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t3_16",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let doubler = b.add_card("synth_dbl_t3_16", "Synth Furnace", |c| {
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
    let shield = b.add_card("synth_sh_t3_16", "Synth Shield2", |c| {
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let burn3 = b.add_synth_burn_instant("synth_b3_t3_16", "Synth Bolt", "{0}", 3);
    let lifelinker =
        b.add_synth_keyword_creature("synth_ll_t3_16", "Synth Cleric", "{0}", 3, 3, &["lifelink"]);
    let attacker =
        b.add_synth_keyword_creature("synth_atk_t3_16", "Synth Raider", "{0}", 4, 4, &["haste"]);
    let pay18 = b.add_card("synth_p18_t3_16", "Synth Drain18", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_player()],
                vec![effect_step(
                    EffectOp::LoseLife,
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 18 }),
                )],
            );
    });
    let _ = mtn;
    let db = b.build_db();
    let mut main0 = vec![pay18; 10];
    main0.extend(vec![attacker; 20]);
    main0.extend(vec![doubler; 10]);
    main0.extend(vec![shield; 10]);
    main0.extend(vec![burn3; 10]);
    let d0 = DeckSpec {
        commanders: vec![],
        mainboard: main0,
        sideboard: vec![],
    };
    let d1 = DeckSpec {
        commanders: vec![],
        mainboard: vec![lifelinker; 60],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 316).unwrap();
    drv.keep_all_hands();

    // Advance until P0 holds pay18, 2x attacker, doubler, shield, burn3.
    let mut base_turn = 1u32;
    let mut found_all = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (attacker, 2),
                (pay18, 1),
                (doubler, 1),
                (shield, 1),
                (burn3, 1),
            ],
        )
        .unwrap();
        let atk_cnt = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == attacker)
            .count();
        if atk_cnt >= 2
            && drv.find_hand_object(PlayerId(0), pay18).is_some()
            && drv.find_hand_object(PlayerId(0), doubler).is_some()
            && drv.find_hand_object(PlayerId(0), shield).is_some()
            && drv.find_hand_object(PlayerId(0), burn3).is_some()
        {
            base_turn = turn;
            found_all = true;
            break;
        }
    }
    assert!(
        found_all,
        "must draw all t3_16 combo pieces within wait loop"
    );

    // 1. CR 702.15b simultaneous Lifelink survival: drop P1 to 2 life, P1 casts 3/3 lifelink.
    let p18_h = drv
        .find_hand_object(PlayerId(0), pay18)
        .expect("Synth Drain18 must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p18_h,
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
    assert_eq!(drv.session.view_model(None).players[1].life, 2);

    drv.advance_to_preserving(
        base_turn + 1,
        GamePhase::Main1,
        64,
        &[(attacker, 2), (doubler, 1), (shield, 1), (burn3, 1)],
    )
    .unwrap();
    let ll_h = drv
        .find_hand_object(PlayerId(1), lifelinker)
        .expect("Synth Cleric must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: ll_h,
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
    let lk_bf = drv
        .find_battlefield_object(PlayerId(1), lifelinker)
        .unwrap();

    // P0 casts two 4/4 haste attackers and attacks P1; P1 blocks one with 3/3 lifelink!
    let atk_turn = base_turn + 2;
    drv.advance_to_preserving(
        atk_turn,
        GamePhase::Main1,
        64,
        &[(attacker, 2), (doubler, 1), (shield, 1), (burn3, 1)],
    )
    .unwrap();
    for _ in 0..2 {
        let a_h = drv
            .find_hand_object(PlayerId(0), attacker)
            .expect("Synth Raider must be in hand");
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
    }
    let atks: Vec<_> = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.card_def == attacker)
        .map(|o| o.id)
        .collect();
    assert_eq!(atks.len(), 2);

    drv.advance_to(atk_turn, GamePhase::DeclareAttackers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![
                (atks[0], AttackTarget::Player { id: PlayerId(1) }),
                (atks[1], AttackTarget::Player { id: PlayerId(1) }),
            ],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::DeclareBlockers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareBlockers {
            assignments: vec![(lk_bf, atks[0])],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::Main2, 32).unwrap();
    // Simultaneous CR 510.2 / 702.15b: 2 - 4 + 3 = 1 life, P1 survives SBAs!
    assert_eq!(drv.session.view_model(None).players[1].life, 1);
    assert!(!matches!(drv.decision(), DecisionState::GameOver { .. }));

    // 2. Prevent 2 + Damage Doubler + Bolt(3) on P0 (at 20 life).
    let d_h = drv
        .find_hand_object(PlayerId(0), doubler)
        .expect("Synth Furnace must be in hand");
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

    let s_h = drv
        .find_hand_object(PlayerId(0), shield)
        .expect("Synth Shield2 must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(0) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let b_h = drv
        .find_hand_object(PlayerId(0), burn3)
        .expect("Synth Bolt must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(0) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseReplacement { prompt_id, .. },
    } = drv.decision()
    {
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 0 },
            },
        )
        .unwrap();
    } else {
        panic!("expected ChooseReplacement between Prevent 2 and Double");
    }
    let life0 = drv.session.view_model(None).players[0].life;
    assert!(
        life0 == 18 || life0 == 16,
        "expected (3-2)*2=2 or 3*2-2=4 damage on P0 (life={life0})"
    );
}

#[test]
fn t3_17_4p_apnap_and_player_elimination_clears_objects_cr800_4() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t3_17", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t3_17", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![bear; 60],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![d.clone(), d.clone(), d.clone(), d],
        317,
    )
    .unwrap();
    drv.keep_all_hands();

    // Advance to Seat 2's turn, cast Bear, then Seat 2 concedes -> Seat 2's battlefield clears!
    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    let b2 = drv.find_hand_object(PlayerId(2), bear).unwrap();
    drv.macro_step(
        PlayerId(2),
        MacroAction::CastSpell {
            object: b2,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    for _ in 0..4 {
        drv.pass_priority();
    }
    assert!(drv.find_battlefield_object(PlayerId(2), bear).is_some());
    drv.step(PlayerId(2), Action::Concede).unwrap();
    let vm = drv.session.view_model(None);
    assert!(vm.players[2].lost);
    assert!(vm.players[2].battlefield.is_empty());
    let _ = plains;
}

#[test]
fn t3_18_cr613_8_dependency_and_combat_flying_reach_legality() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t3_18", "Synth Plains", "Plains", ManaColor::W);
    let construct_sub = b.intern_subtype("Construct");
    let bird_sub = b.intern_subtype("Bird");
    let flying_kw = b.intern_keyword("flying");
    let art_crea = b.add_card("synth_ac_t3_18", "Synth Automaton", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword("haste");
    });
    // Ts 1 (Layer 4): Constructs are Birds (depends on make_construct in SAME Layer 4!).
    let constructs_are_birds = b.add_card("synth_cab_t3_18", "Synth Aviary", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: smallvec::smallvec![construct_sub],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddTypes {
                        types: CardTypeMask::empty(),
                        subtypes: smallvec::smallvec![bird_sub],
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Ts 2 (Layer 4): Artifacts are Constructs.
    let make_construct = b.add_card("synth_mkc_t3_18", "Synth Foundry", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ARTIFACT,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddTypes {
                        types: CardTypeMask::empty(),
                        subtypes: smallvec::smallvec![construct_sub],
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Layer 6: Birds have flying.
    let birds_fly = b.add_card("synth_gfly_t3_18", "Synth SkyLattice", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: smallvec::smallvec![bird_sub],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddKeywords {
                        keywords: smallvec::smallvec![flying_kw],
                        protection_attachment_exemption: None,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let ground = b.add_synth_vanilla_creature("synth_grd_t3_18", "Synth Bear", "{0}", 2, 2);
    let _ = w_land;
    let db = b.build_db();
    let mut main0 = vec![art_crea; 15];
    main0.extend(vec![constructs_are_birds; 15]);
    main0.extend(vec![make_construct; 15]);
    main0.extend(vec![birds_fly; 15]);
    let d0 = DeckSpec {
        commanders: vec![],
        mainboard: main0,
        sideboard: vec![],
    };
    let d1 = DeckSpec {
        commanders: vec![],
        mainboard: vec![ground; 60],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 318).unwrap();
    drv.keep_all_hands();

    drv.advance_to(2, GamePhase::Main1, 32).unwrap();
    let g_h = drv
        .find_hand_object(PlayerId(1), ground)
        .expect("Synth Bear must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: g_h,
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
    let g_bf = drv.find_battlefield_object(PlayerId(1), ground).unwrap();

    let mut cast_turn = 3u32;
    for turn in (3u32..=15).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (constructs_are_birds, 1),
                (make_construct, 1),
                (birds_fly, 1),
                (art_crea, 1),
            ],
        )
        .unwrap();
        if drv
            .find_hand_object(PlayerId(0), constructs_are_birds)
            .is_some()
            && drv.find_hand_object(PlayerId(0), make_construct).is_some()
            && drv.find_hand_object(PlayerId(0), birds_fly).is_some()
            && drv.find_hand_object(PlayerId(0), art_crea).is_some()
        {
            cast_turn = turn;
            break;
        }
    }
    // Cast dependent constructs_are_birds (ts=1) BEFORE prerequisite make_construct (ts=2) in SAME Layer 4!
    for card in [constructs_are_birds, make_construct, birds_fly, art_crea] {
        let h = drv
            .find_hand_object(PlayerId(0), card)
            .expect("combo piece must be in P0 hand");
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
    let ac_bf = drv.find_battlefield_object(PlayerId(0), art_crea).unwrap();
    drv.advance_to(cast_turn, GamePhase::DeclareAttackers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(ac_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(cast_turn, GamePhase::DeclareBlockers, 16)
        .unwrap();
    let blk_res = drv.step_unchecked(
        PlayerId(1),
        Action::AssignBlocker {
            blocker: g_bf,
            attacker: ac_bf,
        },
    );
    assert!(
        matches!(blk_res, Err(EngineError::IllegalAction(_))),
        "CR 613.8a: ground Bear cannot block Artifact Creature that gained Construct -> Bird -> Flying (expected IllegalAction, got {blk_res:?})"
    );
}

#[test]
fn t3_19_ts_replay_diff_test_and_known_cr_divergences_allowlist() {
    let tmp = TempDir::new("t3_19");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_plains_t3_19", "Synth Plains", "Plains", ManaColor::W);
    b.add_card("synth_relic_t3_19", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let j = tmp.child("b.json");
    let bin = tmp.child("b.bin");
    std::fs::write(&j, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);

    let p0_init = serde_json::json!({
        "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
        "handOracleIds": vec!["synth_relic_t3_19"; 7],
        "battlefield": []
    });
    let p1_init = serde_json::json!({
        "seat": 1, "life": 20, "lost": false, "poison": 0, "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
        "handOracleIds": vec!["synth_plains_t3_19"; 7],
        "battlefield": []
    });
    let d0 = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": [p0_init.clone(), p1_init.clone()]
    });
    let d1 = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_init, p1_init.clone()]
    });
    let p0_cast = serde_json::json!({
        "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": { "library": 53, "hand": 6, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
        "handOracleIds": vec!["synth_relic_t3_19"; 6],
        "battlefield": []
    });
    let d2 = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 1, "pendingPromptKind": null,
        "players": [p0_cast.clone(), p1_init.clone()]
    });
    let d3 = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1,
        "phase": "main1", "stackDepth": 1, "pendingPromptKind": null,
        "players": [p0_cast, p1_init.clone()]
    });
    let p0_bf = serde_json::json!({
        "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": { "library": 53, "hand": 6, "battlefield": 1, "graveyard": 0, "exile": 0, "command": 0 },
        "handOracleIds": vec!["synth_relic_t3_19"; 6],
        "battlefield": [{
            "oracleId": "synth_relic_t3_19", "tapped": false, "pt": [0, 0],
            "types": ["Artifact"], "keywords": [], "counters": [],
            "damageMarked": 0, "attachedToOracleId": null
        }]
    });
    let d4 = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_bf, p1_init]
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": vec!["synth_relic_t3_19"; 60], "handInitial": vec!["synth_relic_t3_19"; 7] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": vec!["synth_plains_t3_19"; 60], "handInitial": vec!["synth_plains_t3_19"; 7] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": d0 },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": d1 },
            {
                "stepIndex": 2, "actorSeat": 0,
                "action": { "type": "cast_spell", "instanceId": "c_relic_0", "optionId": "normal" },
                "objectLocators": { "c_relic_0": { "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "synth_relic_t3_19" } },
                "digestAfter": d2
            },
            { "stepIndex": 3, "actorSeat": 0, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": d3 },
            { "stepIndex": 4, "actorSeat": 1, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": d4 }
        ]
    });
    let rep = tmp.child("trace.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(res.success, "diff-test failed: {}", res.stderr);
    assert!(!res.stdout.contains("activates in Phase 1"));
    assert!(repo_root().join("docs/KNOWN_TS_CR_DIVERGENCES.md").exists());
}

#[test]
fn t3_20_full_ir_bridge_smoke_pipeline() {
    let tmp = TempDir::new("t3_20");
    let ir_json = tmp.child("precons.ir-v2.json");
    let cards_bin = tmp.child("precons.cards.bin");
    let replay_json = tmp.child("1v1.replay.json");

    let exp = run_node_script(
        "scripts/export-ir-v2.ts",
        &["--precons", "--out", ir_json.to_string_lossy().as_ref()],
    );
    assert!(exp.success, "export-ir-v2 failed: {}", exp.stderr);
    assert!(run_build_db(&ir_json, &cards_bin, false, Some(3)).success);
    let ins = run_inspect_db(&cards_bin, true);
    assert!(
        ins.success,
        "inspect-db --verify-precons failed: {}",
        ins.stderr
    );

    let rec = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "42",
            "--out",
            replay_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(rec.success, "record-ts-replay failed: {}", rec.stderr);

    let diff = run_diff_test(&cards_bin, &replay_json);
    assert!(diff.success, "diff-test failed: {}", diff.stderr);
    assert!(!diff.stdout.contains("activates in Phase 1"));
}
