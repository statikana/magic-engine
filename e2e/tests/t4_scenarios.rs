//! Tier 4 Opaque-Box E2E Tests: Full Multi-Turn & End-to-End Application Scenarios (`T4-01` .. `T4-10`).
//!
//! Covers 10 full end-to-end workflows using synthetic cards and `/tmp/e2e_*` scratch directories.

use std::sync::Arc;
use std::time::Instant;

use mtg_engine::{
    Action, AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, EngineError,
    GameConfig, MacroAction, MatchSession, PlayerId, PowerToughness, RngMode, RngState, TargetRef,
};
use mtg_engine_e2e::assertions::{
    assert_observation_consistency, assert_rename_invariance_with, assert_view_model_redaction,
};
use mtg_engine_e2e::cli::{
    TempDir, run_build_db, run_diff_test, run_inspect_db_release, run_node_script,
};
use mtg_engine_e2e::driver::{SessionDriver, play_deterministic_random_game};
use mtg_engine_e2e::synth::{
    SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
};
use mtg_ir::{
    Affected, Amount, CardDatabase, CardDefId, CardLayout, CardTypeMask, DamageModMode,
    DamageModTarget, DamageScaleSpec, Duration, EffectOp, EffectStep, EtbChoiceSpec, EventFilter,
    EventKind, EventPattern, GamePhase, Layer, LayerMod, ManaColor, ModalDef, ModalMode,
    ObjectFilter, OptionalMode, OptionalSpec, PlayerRef, ReplacementDef, ReplacementWith,
    SearchDestination, SearchSpec, Selector, SupertypeMask, Zone, ZoneChangeMod, ZoneMask,
};

fn find_land(drv: &SessionDriver, p: PlayerId) -> Option<mtg_engine::ObjectId> {
    drv.session.view_model(Some(p)).players[usize::from(p.0)]
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::LAND))
        .map(|o| o.id)
}

#[test]
fn t4_01_full_1v1_aggro_vs_control_match_to_lethal() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t4_01",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let island = b.add_synth_basic_land("synth_isl_t4_01", "Synth Island", "Island", ManaColor::U);
    let rusher =
        b.add_synth_keyword_creature("synth_rush_t4_01", "Synth Rusher", "{R}", 4, 2, &["haste"]);
    let bolt = b.add_synth_burn_instant("synth_bolt_t4_01", "Synth Bolt", "{R}", 4);
    let draw = b.add_synth_draw_sorcery("synth_draw_t4_01", "Synth Ponder", "{U}", 2);
    let db = b.build_db();

    let mut d0 = vec![mtn; 24];
    d0.extend(vec![rusher; 20]);
    d0.extend(vec![bolt; 16]);
    let mut d1 = vec![island; 30];
    d1.extend(vec![draw; 30]);
    let decks = vec![
        DeckSpec {
            commanders: vec![],
            mainboard: d0,
            sideboard: vec![],
        },
        DeckSpec {
            commanders: vec![],
            mainboard: d1,
            sideboard: vec![],
        },
    ];
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 401).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5, 7, 9, 11] {
        if matches!(drv.decision(), DecisionState::GameOver { .. }) {
            break;
        }
        if let Err(msg) = drv.advance_to(turn, GamePhase::Main1, 64) {
            if matches!(drv.decision(), DecisionState::GameOver { .. }) {
                break;
            }
            panic!("{msg}");
        }
        if let Some(l) = find_land(&drv, PlayerId(0)) {
            let _ = drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            );
        }
        while let Some(r_h) = drv.find_hand_object(PlayerId(0), rusher) {
            if drv
                .macro_step(
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
                .is_err()
            {
                break;
            }
            drv.pass_priority();
            drv.pass_priority();
        }
        while let Some(b_h) = drv.find_hand_object(PlayerId(0), bolt) {
            if drv
                .macro_step(
                    PlayerId(0),
                    MacroAction::CastSpell {
                        object: b_h,
                        option_id: 0,
                        from_zone: Zone::Hand,
                        modes: vec![],
                        x_value: None,
                        targets: vec![TargetRef::Player { id: PlayerId(1) }],
                    },
                )
                .is_err()
            {
                break;
            }
            drv.pass_priority();
            drv.pass_priority();
            if matches!(drv.decision(), DecisionState::GameOver { .. }) {
                break;
            }
        }
        if matches!(drv.decision(), DecisionState::GameOver { .. }) {
            break;
        }
        if drv
            .advance_to(turn, GamePhase::DeclareAttackers, 16)
            .is_ok()
        {
            let attackers: Vec<_> = drv.session.view_model(None).players[0]
                .battlefield
                .iter()
                .filter(|o| o.types.contains(CardTypeMask::CREATURE) && !o.tapped)
                .map(|o| (o.id, AttackTarget::Player { id: PlayerId(1) }))
                .collect();
            if !attackers.is_empty() {
                let _ = drv.macro_step(
                    PlayerId(0),
                    MacroAction::DeclareAttackers {
                        assignments: attackers,
                    },
                );
            }
        }
    }
    if !matches!(drv.decision(), DecisionState::GameOver { .. }) {
        let _ = drv.advance_to(11, GamePhase::Main2, 32);
    }
    assert_eq!(
        drv.decision(),
        DecisionState::GameOver {
            winner: Some(PlayerId(0))
        }
    );
}

#[test]
fn t4_02_full_4p_commander_pod_elimination_and_tax() {
    let mut b = SynthBundleBuilder::new();
    let forest =
        b.add_synth_basic_land("synth_forest_t4_02", "Synth Forest", "Forest", ManaColor::G);
    let cmd = b.add_card("synth_cmd_t4_02", "Synth PodLeader", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(7, 7)
            .keyword("haste");
    });
    let slay = b.add_card("synth_slay_t4_02", "Synth Naturalize", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
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
    let goad_spell = b.add_card("synth_goad_t4_02", "Synth Taunt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Goad {
                        duration: Some(Duration::UntilYourNextTurn),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let mut main0 = vec![forest; 60];
    main0.extend(vec![slay; 39]);
    let d0 = DeckSpec {
        commanders: vec![cmd],
        mainboard: main0,
        sideboard: vec![],
    };
    let mut main1 = vec![forest; 59];
    main1.extend(vec![goad_spell; 40]);
    let d1 = DeckSpec {
        commanders: vec![cmd],
        mainboard: main1,
        sideboard: vec![],
    };
    let d_other = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![forest; 99],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![d0, d1, d_other.clone(), d_other],
        402,
    )
    .unwrap();
    drv.keep_all_hands();

    // Turn 1: Play 1st Forest, cast cmd ({G}), destroy it, return to Command (+2 tax).
    drv.advance_to(1, GamePhase::Main1, 64).unwrap();
    let l1 = find_land(&drv, PlayerId(0)).expect("Forest 1 must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let cid = drv.find_command_object(PlayerId(0), cmd).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cid,
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
    let c_bf = drv.find_battlefield_object(PlayerId(0), cmd).unwrap();
    let s_h = drv
        .find_hand_object(PlayerId(0), slay)
        .expect("Synth Naturalize must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    for _ in 0..4 {
        drv.pass_priority();
    }
    match drv.decision() {
        DecisionState::Prompt {
            request: ChoiceRequest::YesNo { prompt_id, .. },
        } => {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::YesNo { value: true },
                },
            )
            .unwrap();
        }
        other => panic!("expected Commander zone return YesNo prompt, got {other:?}"),
    }

    // Turn 5: Play 2nd Forest (2 mana total). Recasting with +2 tax ({2}{G} = 3) MUST fail!
    drv.advance_to_preserving(5, GamePhase::Main1, 256, &[(goad_spell, 1), (forest, 1)])
        .unwrap();
    let l2 = find_land(&drv, PlayerId(0)).expect("Forest 2 must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l2,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let cid_taxed = drv.find_command_object(PlayerId(0), cmd).unwrap();
    let tax_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cid_taxed,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(tax_res, Err(EngineError::IllegalAction(_))),
        "CR 903.8: recasting with 2 lands when tax brings cost to {{2}}{{G}} must fail with IllegalAction, got {tax_res:?}"
    );

    // Turn 9: Play 3rd Forest (3 mana total), recast cmd ({2}{G}), deal 7 cmd damage to Seat 2!
    drv.advance_to_preserving(9, GamePhase::Main1, 256, &[(goad_spell, 1), (forest, 1)])
        .unwrap();
    let l3 = find_land(&drv, PlayerId(0)).expect("Forest 3 must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l3,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let cid_recast = drv.find_command_object(PlayerId(0), cmd).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cid_recast,
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
    let c_bf = drv.find_battlefield_object(PlayerId(0), cmd).unwrap();
    drv.advance_to(9, GamePhase::DeclareAttackers, 16).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(c_bf, AttackTarget::Player { id: PlayerId(2) })],
        },
    )
    .unwrap();
    drv.advance_to(9, GamePhase::Main2, 32).unwrap();

    // Turn 10 (Seat 1): P1 casts Goad targeting Seat 0's commander (CR 701.15)!
    drv.advance_to_preserving(10, GamePhase::Main1, 64, &[(goad_spell, 1)])
        .unwrap();
    let g_h = drv
        .find_hand_object(PlayerId(1), goad_spell)
        .expect("Synth Taunt must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: g_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    for _ in 0..4 {
        drv.pass_priority();
    }

    // Turn 13: Goaded commander MUST attack and CANNOT attack goader P1 while P2/P3 exist!
    drv.advance_to(13, GamePhase::DeclareAttackers, 256)
        .unwrap();
    let goad_empty = drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![],
        },
    );
    assert!(
        matches!(goad_empty, Err(EngineError::IllegalAction(_))),
        "CR 701.15: goaded creature must attack if able (expected IllegalAction, got {goad_empty:?})"
    );
    let goad_wrong_target = drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(c_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    );
    assert!(
        matches!(goad_wrong_target, Err(EngineError::IllegalAction(_))),
        "CR 701.15: goaded creature cannot attack goading player P1 while other opponents exist (expected IllegalAction, got {goad_wrong_target:?})"
    );
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(c_bf, AttackTarget::Player { id: PlayerId(2) })],
        },
    )
    .unwrap();
    drv.advance_to(13, GamePhase::Main2, 32).unwrap();

    // Turn 17: 3rd attack reaches 21 commander damage -> Seat 2 loses!
    drv.advance_to(17, GamePhase::DeclareAttackers, 256)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(c_bf, AttackTarget::Player { id: PlayerId(2) })],
        },
    )
    .unwrap();
    let _ = drv.advance_to(17, GamePhase::Main2, 32);
    assert!(drv.session.view_model(None).players[2].lost);
}

#[test]
fn t4_03_chaos_board_layers_replacements_and_sba_fixpoint() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t4_03",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let bear = b.add_synth_vanilla_creature("synth_bear_t4_03", "Synth Bear", "{0}", 1, 3);
    let doubler = b.add_card("synth_dbl_t4_03", "Synth Doubler", |c| {
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
    let anthem = b.add_synth_anthem_enchantment("synth_anth_t4_03", "Synth Anthem", "{0}", 1, 1);
    let bolt2 = b.add_synth_burn_instant("synth_bolt_t4_03", "Synth Bolt", "{0}", 2);
    let db = b.build_db();
    let mut main = vec![mtn; 12];
    main.extend(vec![bear; 12]);
    main.extend(vec![doubler; 12]);
    main.extend(vec![anthem; 12]);
    main.extend(vec![bolt2; 12]);
    let d = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 403).unwrap();
    drv.keep_all_hands();

    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (anthem, 1), (doubler, 1), (bolt2, 1)],
        )
        .unwrap();
        if [bear, anthem, doubler, bolt2]
            .iter()
            .all(|&c| drv.find_hand_object(PlayerId(0), c).is_some())
        {
            break;
        }
    }

    for card in [bear, anthem, doubler] {
        let h = drv
            .find_hand_object(PlayerId(0), card)
            .expect("combo piece must be in hand");
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
            power: 2,
            toughness: 4
        })
    );

    // Cast 2-damage Synth Bolt targeting 2/4 Synth Bear: Synth Doubler doubles to 4 -> lethal SBA!
    let bolt_h = drv
        .find_hand_object(PlayerId(0), bolt2)
        .expect("Synth Bolt must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bolt_h,
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
        drv.find_battlefield_object(PlayerId(0), bear).is_none()
            && drv.find_graveyard_object(PlayerId(0), bear).is_some(),
        "CR 614 + 704.5g: doubled 2->4 damage on 2/4 Synth Bear must be lethal via SBA"
    );
}

#[test]
fn t4_04_cli_end_to_end_card_db_lifecycle_and_match_session() {
    let tmp = TempDir::new("t4_04");
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t4_04", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_card("synth_cmd_t4_04", "Synth Sovereign", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(3, 3);
    });
    let mdfc = b.add_card("synth_mdfc_t4_04", "Synth Dual // Synth Land", |c| {
        c.layout(CardLayout::ModalDfc)
            .mana_cost("{W}")
            .types(CardTypeMask::SORCERY)
            .add_face("Synth Land", |f| {
                f.types(CardTypeMask::LAND)
                    .mana_ability("tap_w", &[ManaColor::W]);
            });
    });
    b.add_deck(
        "synth_cli_deck",
        "Synth CLI Precon",
        &["synth_cmd_t4_04"],
        &[("synth_plains_t4_04", 98), ("synth_mdfc_t4_04", 1)],
    );

    let json_p = tmp.child("synth.ir-v2.json");
    let zst_p = tmp.child("cards.bin");
    let raw_p = tmp.child("raw.bin");
    std::fs::write(&json_p, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();

    assert!(run_build_db(&json_p, &zst_p, false, Some(3)).success);
    assert!(run_build_db(&json_p, &raw_p, true, None).success);
    let ins = run_inspect_db_release(&zst_p, true);
    assert!(ins.success);
    assert!(ins.stdout.contains("0 missing, 0 unsupported"));

    let db = Arc::new(CardDatabase::decode_bytes(&std::fs::read(&zst_p).unwrap()).unwrap());
    let mut main = vec![plains; 98];
    main.push(mdfc);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    assert!(MatchSession::validate_deck(&db, &GameConfig::commander(2), &d).is_legal);
    let mut drv =
        SessionDriver::new(db, GameConfig::commander(2), vec![d.clone(), d], 404).unwrap();
    drv.keep_all_hands();
    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    assert_eq!(drv.session.state().turn_number(), 3);
}

#[test]
fn t4_05_snapshot_persistence_and_replay_recovery_simulation() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t4_05", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_t4_05", "Synth Cmd", "{W}", 2, 2);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![land; 99],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        Arc::clone(&db),
        GameConfig::commander(4),
        vec![d.clone(), d.clone(), d.clone(), d],
        405,
    )
    .unwrap();
    drv.keep_all_hands();
    drv.advance_to(4, GamePhase::Main1, 256).unwrap();

    #[cfg(feature = "m5-session-ext")]
    {
        let tmp = TempDir::new("t4_05");
        let snap_path = tmp.child("match.snap");
        let snap = drv.session.snapshot_bytes().unwrap();
        std::fs::write(&snap_path, &snap).unwrap();
        let rep = drv.session.replay_log().clone();

        let mut from_snap =
            MatchSession::from_snapshot(Arc::clone(&db), &std::fs::read(&snap_path).unwrap())
                .unwrap();
        let mut from_rep = MatchSession::from_replay(Arc::clone(&db), rep).unwrap();
        for _ in 0..8 {
            let a = drv.actor().unwrap();
            drv.step(a, Action::PassPriority).unwrap();
            from_snap.apply_action(a, Action::PassPriority).unwrap();
            from_rep.apply_action(a, Action::PassPriority).unwrap();
        }
        assert_eq!(drv.session.view_model(None), from_snap.view_model(None));
        assert_eq!(drv.session.view_model(None), from_rep.view_model(None));
    }
    #[cfg(not(feature = "m5-session-ext"))]
    {
        assert!(drv.session.state().turn_number() >= 4);
        panic!("requires M5 MatchSession extensions (--features m5-session-ext)");
    }
}

#[test]
fn t4_06_ismcts_determinization_and_observation_rollout_loop() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t4_06", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t4_06", "Synth Bear", "{0}", 2, 2);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_t4_06", "Synth Cmd", "{W}", 3, 3);
    let db = b.build_db();
    let mut main = vec![land; 60];
    main.extend(vec![bear; 39]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let decks = vec![d.clone(), d.clone(), d.clone(), d];
    let mut drv = SessionDriver::new(
        Arc::clone(&db),
        GameConfig::commander(4),
        decks.clone(),
        406,
    )
    .unwrap();
    drv.keep_all_hands();
    assert_observation_consistency(&drv.session, PlayerId(0));

    let mut opp_libs: Vec<Vec<CardDefId>> = Vec::with_capacity(20);
    for i in 0..20u64 {
        let mut rng = RngState::splitmix64(1000 + i);
        let det = drv.session.determinize(PlayerId(0), &mut rng);
        assert_eq!(det.players()[0].hand, drv.session.state().players()[0].hand);
        assert_eq!(
            det.players()[1].hand.len(),
            drv.session.state().players()[1].hand.len()
        );
        assert_eq!(
            det.players()[1].library.len(),
            drv.session.state().players()[1].library.len()
        );
        opp_libs.push(
            det.players()[1]
                .library
                .iter()
                .map(|&id| det.object(id).unwrap().card_def)
                .collect(),
        );
        let final_dec = play_deterministic_random_game(
            Arc::clone(&db),
            GameConfig::commander(4),
            decks.clone(),
            406 + i,
            30,
        );
        let _ = final_dec;
    }
    assert!(
        opp_libs.windows(2).any(|w| w[0] != w[1]),
        "determinize() must actually resample opponent hidden zones across seeds"
    );
    assert_view_model_redaction(&drv.session);
}

#[test]
fn t4_07_six_kind_interactive_choice_and_pausable_resolution_gauntlet() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t4_07", "Synth Island", "Island", ManaColor::U);
    let opt_heal = b.add_card("synth_opt_t4_07", "Synth OptHeal", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![EffectStep {
                    optional: Some(OptionalSpec {
                        chooser: PlayerRef::You,
                        cost: vec![],
                        mode: OptionalMode::DoIfPaid,
                    }),
                    ..effect_step(
                        EffectOp::GainLife {
                            set_life_total: false,
                        },
                        Selector::PlayerRef {
                            player: PlayerRef::You,
                        },
                        Some(Amount::Const { n: 2 }),
                    )
                }],
            );
    });
    let search_land = b.add_card("synth_srch_t4_07", "Synth Tutor", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Search {
                        spec: Box::new(SearchSpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::LAND,
                                ..Default::default()
                            }),
                            count: 1,
                            up_to: true,
                            stated_quality: false,
                            destinations: vec![SearchDestination {
                                zone: Zone::Hand,
                                tapped: false,
                                on_top: false,
                            }],
                            reveal: false,
                            description: None,
                            zones: ZoneMask::LIBRARY,
                            different_names: false,
                            shuffle_only_if_library: true,
                            searched_player: None,
                            cast_free: false,
                            target_controller_as_actor: false,
                        }),
                        tapped: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    None,
                )],
            );
    });
    let modal = b.add_card("synth_modal_t4_07", "Synth Charm", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_ability(mtg_ir::SpellAbility {
                id: None,
                additional_costs: vec![],
                targets: vec![],
                effect: vec![],
                modal: Some(ModalDef {
                    min: 1,
                    max: 1,
                    allow_repeat: false,
                    once_per_turn_unique: false,
                    up_to: false,
                    conditional_choose_all: None,
                    modes: vec![
                        ModalMode {
                            targets: vec![target_player()],
                            effect: vec![effect_step(
                                EffectOp::Draw {
                                    up_to: false,
                                    reveal_drawn: false,
                                },
                                Selector::Target { index: 0 },
                                Some(Amount::Const { n: 1 }),
                            )],
                            span: Default::default(),
                        },
                        ModalMode {
                            targets: vec![],
                            effect: vec![effect_step(
                                EffectOp::Scry,
                                Selector::PlayerRef {
                                    player: PlayerRef::You,
                                },
                                Some(Amount::Const { n: 2 }),
                            )],
                            span: Default::default(),
                        },
                    ],
                    per_mode_costs: vec![],
                    unchosen_permanent: false,
                    opponent_chooses: None,
                    pawprint_budget: None,
                    pawprint_costs: vec![],
                    dynamic_max: None,
                    random_mode: false,
                    prefix_effect: vec![],
                    suffix_effect: vec![],
                    conditional_choose_range: None,
                    conditional_not_random: None,
                    different_player_targets_per_mode: false,
                }),
                cast_restrictions: vec![],
                cast_condition: None,
                x_cant_be_zero: false,
                span: Default::default(),
            });
    });
    let num_art = b.add_card("synth_num_t4_07", "Synth Dial", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(mtg_ir::TriggerSubject::SelfObj),
                        detail: Some(mtg_ir::EventDetail::ZoneChange {
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
                        etb_choices: vec![EtbChoiceSpec::Number {
                            min: Some(1),
                            max: Some(3),
                        }],
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let reorder = b.add_card("synth_reord_t4_07", "Synth PonderTop", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::LookAndReorder { may_shuffle: false },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let add1_dmg = b.add_card("synth_add1_t4_07", "Synth PlusOne", |c| {
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
    let dbl_dmg = b.add_card("synth_dbl_t4_07", "Synth Doubler", |c| {
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
    let shock2 = b.add_synth_burn_instant("synth_shk_t4_07", "Synth Shock", "{0}", 2);

    let db = b.build_db();
    let mut main0 = vec![u_land; 10];
    main0.extend(vec![opt_heal; 7]);
    main0.extend(vec![search_land; 7]);
    main0.extend(vec![modal; 7]);
    main0.extend(vec![num_art; 7]);
    main0.extend(vec![reorder; 7]);
    main0.extend(vec![add1_dmg; 5]);
    main0.extend(vec![dbl_dmg; 5]);
    main0.extend(vec![shock2; 5]);
    let d0 = DeckSpec {
        commanders: vec![],
        mainboard: main0,
        sideboard: vec![],
    };
    let d1 = DeckSpec {
        commanders: vec![],
        mainboard: vec![u_land; 60],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 407).unwrap();
    drv.keep_all_hands();

    let mut seen_yes_no = false;
    let mut seen_choose_objects = false;
    let mut seen_choose_mode = false;
    let mut seen_choose_number = false;
    let mut seen_order_objects = false;
    let mut seen_choose_replacement = false;

    let mut drive_prompts = |drv: &mut SessionDriver, expect_stack_frame: bool| {
        let mut guard = 0;
        while let DecisionState::Prompt { request } = drv.decision() {
            let actor = drv.actor().expect("prompt must have actor");
            if expect_stack_frame {
                assert_eq!(drv.session.state().stack().len(), 1);
            }
            match request {
                ChoiceRequest::YesNo { prompt_id, .. } => {
                    seen_yes_no = true;
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::YesNo { value: true },
                        },
                    )
                    .unwrap();
                }
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                } => {
                    seen_choose_objects = true;
                    let answer = if let Some(&id) = candidates.first() {
                        ChoiceAnswer::PickObject { id }
                    } else {
                        ChoiceAnswer::DonePicking
                    };
                    drv.step(actor, Action::Answer { prompt_id, answer })
                        .unwrap();
                }
                ChoiceRequest::ChooseMode { prompt_id, .. } => {
                    seen_choose_mode = true;
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickMode { index: 1 },
                        },
                    )
                    .unwrap();
                }
                ChoiceRequest::ChooseNumber { prompt_id, min, .. } => {
                    seen_choose_number = true;
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickNumber { value: min },
                        },
                    )
                    .unwrap();
                }
                ChoiceRequest::OrderObjects { prompt_id, objects } => {
                    seen_order_objects = true;
                    let id = *objects.first().expect("OrderObjects must not be empty");
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::OrderNext { id },
                        },
                    )
                    .unwrap();
                }
                ChoiceRequest::ChooseReplacement { prompt_id, .. } => {
                    seen_choose_replacement = true;
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickReplacement { index: 0 },
                        },
                    )
                    .unwrap();
                }
                ChoiceRequest::GuidedResolution { .. } => {
                    panic!("unexpected GuidedResolution on synthetic cards");
                }
            }
            guard += 1;
            assert!(guard <= 16, "prompt loop exceeded 16 answers");
        }
    };

    let mut done_opt = false;
    let mut done_search = false;
    let mut done_modal = false;
    let mut done_num = false;
    let mut done_reorder = false;
    let mut done_add1 = false;
    let mut done_dbl = false;
    let mut done_shock = false;

    for turn in (1u32..=79).step_by(2) {
        let mut keep = Vec::new();
        if !done_opt {
            keep.push((opt_heal, 1));
        }
        if !done_search {
            keep.push((search_land, 1));
        }
        if !done_modal {
            keep.push((modal, 1));
        }
        if !done_num {
            keep.push((num_art, 1));
        }
        if !done_reorder {
            keep.push((reorder, 1));
        }
        if !done_add1 {
            keep.push((add1_dmg, 1));
        }
        if !done_dbl {
            keep.push((dbl_dmg, 1));
        }
        if !done_shock {
            keep.push((shock2, 1));
        }
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &keep)
            .unwrap();

        if let Some(l) = find_land(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }

        // 1. YesNo (inside ResolutionFrame)
        if !done_opt && let Some(oh) = drv.find_hand_object(PlayerId(0), opt_heal) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: oh,
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
            drive_prompts(&mut drv, true);
            done_opt = true;
        }
        // 2. ChooseObjects (Search inside ResolutionFrame)
        if !done_search && let Some(sh) = drv.find_hand_object(PlayerId(0), search_land) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: sh,
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
            drive_prompts(&mut drv, true);
            done_search = true;
        }
        // 3. ChooseMode (during BeginCast) + Scry OrderObjects (inside ResolutionFrame)
        if !done_modal && let Some(mh) = drv.find_hand_object(PlayerId(0), modal) {
            drv.step(
                PlayerId(0),
                Action::BeginCast {
                    object: mh,
                    option_id: 0,
                    from_zone: Zone::Hand,
                },
            )
            .unwrap();
            drive_prompts(&mut drv, false);
            drv.pass_priority();
            drv.pass_priority();
            drive_prompts(&mut drv, true);
            done_modal = true;
        }
        // 4. ChooseNumber (ETB choice on artifact)
        if !done_num && let Some(nh) = drv.find_hand_object(PlayerId(0), num_art) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: nh,
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
            drive_prompts(&mut drv, false);
            done_num = true;
        }
        // 5. OrderObjects (LookAndReorder inside ResolutionFrame)
        if !done_reorder && let Some(rh) = drv.find_hand_object(PlayerId(0), reorder) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: rh,
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
            drive_prompts(&mut drv, true);
            done_reorder = true;
        }
        // 6a. Replacement enchantments
        if !done_add1 && let Some(h) = drv.find_hand_object(PlayerId(0), add1_dmg) {
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
            done_add1 = true;
        }
        if !done_dbl && let Some(h) = drv.find_hand_object(PlayerId(0), dbl_dmg) {
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
            done_dbl = true;
        }
        // 6b. ChooseReplacement (two same-tier 616.1e damage replacements + Shock)
        if done_add1
            && done_dbl
            && !done_shock
            && let Some(skh) = drv.find_hand_object(PlayerId(0), shock2)
        {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: skh,
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
            drive_prompts(&mut drv, false);
            done_shock = true;
        }

        if done_opt
            && done_search
            && done_modal
            && done_num
            && done_reorder
            && done_add1
            && done_dbl
            && done_shock
        {
            break;
        }
    }

    assert!(done_opt, "opt_heal must be cast");
    assert!(done_search, "search_land must be cast");
    assert!(done_modal, "modal must be cast");
    assert!(done_num, "num_art must be cast");
    assert!(done_reorder, "reorder must be cast");
    assert!(
        done_add1 && done_dbl && done_shock,
        "replacement combo must resolve"
    );

    assert!(seen_yes_no, "ChoiceRequest::YesNo must be exercised");
    assert!(
        seen_choose_objects,
        "ChoiceRequest::ChooseObjects must be exercised"
    );
    assert!(
        seen_choose_mode,
        "ChoiceRequest::ChooseMode must be exercised"
    );
    assert!(
        seen_choose_number,
        "ChoiceRequest::ChooseNumber must be exercised"
    );
    assert!(
        seen_order_objects,
        "ChoiceRequest::OrderObjects must be exercised"
    );
    assert!(
        seen_choose_replacement,
        "ChoiceRequest::ChooseReplacement must be exercised"
    );
}

#[test]
fn t4_08_multiface_and_alternate_casting_mechanics_showcase() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t4_08", "Synth Plains", "Plains", ManaColor::W);
    let adv = b.add_card("synth_adv_t4_08", "Synth Traveler", |c| {
        c.layout(CardLayout::Adventure)
            .mana_cost("{W}")
            .types(CardTypeMask::CREATURE)
            .pt(3, 3)
            .add_face("Synth Voyage", |f| {
                f.mana_cost("{W}")
                    .types(CardTypeMask::INSTANT)
                    .spell_effect(
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
    });
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![w_land; 24].into_iter().chain(vec![adv; 36]).collect(),
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 408).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
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
    }
    // Cast Adventure half (`option_id: 1`) -> resolves to Exile -> cast Creature half from Exile!
    let a_h = drv.find_hand_object(PlayerId(0), adv).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a_h,
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
    assert_eq!(drv.session.view_model(None).players[0].life, 22);
    let a_ex = drv.find_exile_object(PlayerId(0), adv).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a_ex,
            option_id: 0,
            from_zone: Zone::Exile,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_battlefield_object(PlayerId(0), adv).is_some());
}

#[test]
fn t4_09_full_ts_bridge_to_rust_diff_test_pipeline() {
    let tmp = TempDir::new("t4_09");
    let precons_json = tmp.child("precons.ir-v2.json");
    let precons_bin = tmp.child("precons.cards.bin");
    let r_1v1 = tmp.child("1v1.replay.json");
    let r_4p = tmp.child("4p.replay.json");

    let exp = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--precons",
            "--out",
            precons_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(exp.success, "export-ir-v2 --precons failed: {}", exp.stderr);
    assert!(run_build_db(&precons_json, &precons_bin, false, Some(3)).success);
    let ins = run_inspect_db_release(&precons_bin, true);
    assert!(ins.success);
    assert!(ins.stdout.contains("Bundled decks:   41"));
    assert!(ins.stdout.contains("0 missing, 0 unsupported"));

    assert!(
        run_node_script(
            "scripts/record-ts-replay.ts",
            &[
                "--mode",
                "1v1",
                "--seed",
                "42",
                "--out",
                r_1v1.to_string_lossy().as_ref()
            ]
        )
        .success
    );
    assert!(
        run_node_script(
            "scripts/record-ts-replay.ts",
            &[
                "--mode",
                "4p",
                "--seed",
                "99",
                "--out",
                r_4p.to_string_lossy().as_ref()
            ]
        )
        .success
    );

    let d1 = run_diff_test(&precons_bin, &r_1v1);
    assert!(d1.success && !d1.stdout.contains("activates in Phase 1"));
    let d4 = run_diff_test(&precons_bin, &r_4p);
    assert!(d4.success && !d4.stdout.contains("activates in Phase 1"));
}

#[test]
fn t4_10_release_performance_and_rename_invariance_stress_harness() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_t4_10", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t4_10", "Synth Bear", "{0}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_t4_10", "Synth Anthem", "{0}", 1, 1);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_t4_10", "Synth Cmd", "{W}", 3, 3);
    let bundle_a = b.build_bundle();
    let bundle_b = b.clone_renamed("_stress");
    let db = Arc::new(CardDatabase::from_bundle(bundle_a.clone()).unwrap());

    let mut main = vec![land; 20];
    main.extend(vec![bear; 40]);
    main.extend(vec![anthem; 39]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let decks = vec![d.clone(), d.clone(), d.clone(), d];
    let mut drv = SessionDriver::new(
        Arc::clone(&db),
        GameConfig::commander(4),
        decks.clone(),
        410,
    )
    .unwrap();
    drv.keep_all_hands();

    // Populate a mid-game 4-player Commander board with >= 20 permanents + active continuous effects.
    for turn in 1u32..=8 {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let active = drv.session.state().active_player();
        while let Some(obj) = drv
            .find_hand_object(active, bear)
            .or_else(|| drv.find_hand_object(active, anthem))
        {
            drv.macro_step(
                active,
                MacroAction::CastSpell {
                    object: obj,
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
        }
        let total_bf: usize = drv
            .session
            .state()
            .players()
            .iter()
            .map(|p| p.battlefield.len())
            .sum();
        if total_bf >= 20 {
            break;
        }
    }
    let total_bf: usize = drv
        .session
        .state()
        .players()
        .iter()
        .map(|p| p.battlefield.len())
        .sum();
    assert!(
        total_bf >= 20,
        "stress benchmark requires >= 20 permanents on battlefield, got {total_bf}"
    );

    let t0 = Instant::now();
    let mut rev_sum = 0u64;
    for _ in 0..10_000 {
        let cloned = drv.session.state().clone();
        rev_sum = rev_sum.wrapping_add(std::hint::black_box(cloned.layer_rev()));
    }
    assert!(rev_sum > 0);
    let clone_us = (t0.elapsed().as_secs_f64() * 1_000_000.0) / 10_000.0;
    if !cfg!(debug_assertions) {
        assert!(clone_us < 20.0, "clone took {clone_us:.2} µs");
    }

    assert_rename_invariance_with(
        bundle_a,
        bundle_b,
        &GameConfig::commander(4),
        &decks,
        &decks,
        410,
        |rdrv| {
            rdrv.keep_all_hands();
            for turn in [1u32, 5, 9] {
                rdrv.advance_to_preserving(
                    turn,
                    GamePhase::Main1,
                    256,
                    &[(land, 1), (bear, 1), (anthem, 1)],
                )
                .unwrap();
                if find_land(rdrv, PlayerId(0)).is_some()
                    && rdrv.find_hand_object(PlayerId(0), bear).is_some()
                    && rdrv.find_hand_object(PlayerId(0), anthem).is_some()
                {
                    break;
                }
            }
            let l_h = find_land(rdrv, PlayerId(0)).expect("Synth Plains must be in hand");
            rdrv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            let a_h = rdrv
                .find_hand_object(PlayerId(0), anthem)
                .expect("Synth Anthem must be in hand");
            rdrv.macro_step(
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
            for _ in 0..4 {
                rdrv.pass_priority();
            }
            let b_h = rdrv
                .find_hand_object(PlayerId(0), bear)
                .expect("Synth Bear must be in hand");
            rdrv.macro_step(
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
            for _ in 0..4 {
                rdrv.pass_priority();
            }
            let b_bf = rdrv.find_battlefield_object(PlayerId(0), bear).unwrap();
            assert_eq!(
                rdrv.session
                    .hover_breakdown(None, b_bf)
                    .unwrap()
                    .evaluated_pt,
                Some(PowerToughness {
                    power: 3,
                    toughness: 3
                })
            );
        },
    );
    let _ = (
        RngMode::Mulberry32Compat,
        simple_trigger(EventKind::ZoneChange, EventFilter::default()),
        target_creature(),
        target_player(),
    );
    let _ = (Layer::Layer7c, Affected::SelfObj, LayerMod::SwitchPt);
}
