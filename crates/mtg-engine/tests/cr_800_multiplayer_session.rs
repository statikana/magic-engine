//! CR 800.4a (Multiplayer Concede), CR 500.7/611.2a (UntilYourNextTurn vs UntilEndOfYourNextTurn),
//! Mulberry32 Determinism, Rename Invariance, MatchSession Snapshots / FromReplay,
//! Observation Redaction / Flat Encoding, Determinization, and `verify-layer-cache` 100-Step Walk.

mod common;

use std::sync::Arc;

use common::{
    SessionDriver, SynthBundleBuilder, assert_rename_invariance, assert_snapshot_equivalence,
    effect_step, target_creature,
};
use mtg_engine::{
    Action, DecisionState, DeckSpec, EngineError, GameConfig, MacroAction, MatchSession, PlayerId,
    PowerToughness, RngMode, RngState, TargetRef, encode_flat_observation,
};
use mtg_ir::{
    Amount, CardTypeMask, Duration, EffectOp, GamePhase, ManaColor, PlayerRef, PlayerRuleKind,
    RelativeControl, Selector, StaticBody, StaticDef, Zone,
};

#[test]
fn synth_multiplayer_until_next_turn_vs_until_end_of_next_turn() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_8001", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_commander("synth_cmd_8001", "Synth Cmd", "{0}", 3, 3);
    let bear = b.add_synth_vanilla("synth_bear_8001", "Synth Bear", "{0}", 2, 2);
    let giant = b.add_synth_vanilla("synth_giant_8001", "Synth Giant", "{0}", 5, 5);

    // Spell 1: BecomeCopy(UntilYourNextTurn) + Goad(UntilYourNextTurn) + ProtectionFromEverything(UntilYourNextTurn)
    let mut step_copy_next = effect_step(
        EffectOp::BecomeCopy {
            copy_source: Box::new(Selector::Target { index: 1 }),
            duration: Some(Duration::UntilYourNextTurn),
            copy_mod: None,
        },
        Selector::Target { index: 0 },
        None,
    );
    step_copy_next.subject = Selector::Target { index: 0 };
    let goad_next = effect_step(
        EffectOp::Goad {
            duration: Some(Duration::UntilYourNextTurn),
        },
        Selector::Target { index: 1 },
        None,
    );
    let prot_next = effect_step(
        EffectOp::GrantPlayer {
            rules: vec![PlayerRuleKind::ProtectionFromEverything],
            duration: Some(Duration::UntilYourNextTurn),
            extra_land_plays: None,
            next_spell_types: None,
            spend_mana_as_any_color_top_of_library: false,
            steal_opponent_entering_creatures_until_eot: false,
            opponents_spells_cost_more_until_next_turn: None,
            attack_tax_until_your_next_turn: None,
            next_spell_cost_reduction_generic: None,
            next_creature_enters_with_plus_counters: None,
            next_main_phase_add_mana: None,
            double_tokens_this_turn: false,
            flash_cast_condition: None,
            next_spell_cascade: None,
            extra_loyalty_activations_this_turn: None,
            planar_action: None,
        },
        Selector::PlayerRef {
            player: PlayerRef::You,
        },
        None,
    );
    let spell_until_next = b.add_card("synth_unt_8001", "Synth NextTurnSpell", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature(), target_creature()],
                vec![step_copy_next, goad_next, prot_next],
            );
    });

    // Spell 2: BecomeCopy(UntilEndOfYourNextTurn)
    let spell_until_eoynt = b.add_card("synth_ueoynt_8001", "Synth EndNextTurnSpell", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature(), target_creature()],
                vec![effect_step(
                    EffectOp::BecomeCopy {
                        copy_source: Box::new(Selector::Target { index: 1 }),
                        duration: Some(Duration::UntilEndOfYourNextTurn),
                        copy_mod: None,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[bear, bear, giant, spell_until_next, spell_until_eoynt],
            &[plains],
            &[plains],
            &[plains],
        ],
        &[vec![cmd], vec![cmd], vec![cmd], vec![cmd]],
        3,
        GameConfig::commander(4),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 8001).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    for c in [bear, bear, giant] {
        drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
        drv.resolve_all_stack(12);
    }
    let bears: Vec<_> = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .battlefield
        .iter()
        .copied()
        .filter(|&id| drv.session.state().object(id).unwrap().card_def == bear)
        .collect();
    let giant_bf = drv.find_battlefield_object(PlayerId(0), giant).unwrap();

    drv.cast_from_hand(
        PlayerId(0),
        spell_until_next,
        vec![
            TargetRef::Object { id: bears[0] },
            TargetRef::Object { id: giant_bf },
        ],
    )
    .unwrap();
    drv.resolve_all_stack(16);
    drv.cast_from_hand(
        PlayerId(0),
        spell_until_eoynt,
        vec![
            TargetRef::Object { id: bears[1] },
            TargetRef::Object { id: giant_bf },
        ],
    )
    .unwrap();
    drv.resolve_all_stack(16);

    // Persist through P1 (Turn 2), P2 (Turn 3), P3 (Turn 4)!
    for turn in [2u32, 3, 4] {
        drv.advance_to(turn, GamePhase::Main1, 32);
        let cache = drv.session.state().evaluate_board();
        assert_eq!(
            cache.get(bears[0]).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );
        assert_eq!(
            cache.get(bears[1]).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );
        assert!(
            cache
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );
    }

    // Turn 5 (P0's next turn Main1):
    // - UntilYourNextTurn (bears[0] BecomeCopy, giant Goad, P0 ProtectionFromEverything) expired at Turn 5 Untap!
    // - UntilEndOfYourNextTurn (bears[1] BecomeCopy) is STILL 5/5 during P0 Turn 5 Main1!
    drv.advance_to(5, GamePhase::Main1, 32);
    {
        let cache = drv.session.state().evaluate_board();
        assert_eq!(
            cache.get(bears[0]).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
        assert!(
            !cache
                .player(PlayerId(0))
                .unwrap()
                .protection_from_everything
        );
        assert_eq!(
            cache.get(bears[1]).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );
    }

    // Turn 6 (after P0 Turn 5 Cleanup): UntilEndOfYourNextTurn expired at P0 Turn 5 Cleanup -> bears[1] back to 2/2!
    drv.advance_to(6, GamePhase::Main1, 48);
    assert_eq!(
        drv.session
            .state()
            .evaluate_board()
            .get(bears[1])
            .unwrap()
            .pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
}

#[test]
fn synth_cr800_4a_multiplayer_concede_cleanup_mulligan_stack_and_prompt() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_8004", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_commander("synth_cmd_8004", "Synth Cmd", "{0}", 3, 3);
    let bear = b.add_synth_vanilla("synth_bear_8004", "Synth Bear", "{0}", 2, 2);
    let steal = b.add_card("synth_steal_8004", "Synth Annex", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::GainControl {
                        duration: Some(Duration::Permanent),
                        untap: true,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let opt_spell = b.add_card("synth_opt_8004", "Synth Inquiry", |c| {
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

    // (a) P0 concedes during opening mulligan -> advances mulligan to P1!
    {
        let d = DeckSpec {
            commanders: vec![cmd],
            mainboard: vec![plains; 10],
            sideboard: vec![],
        };
        let mut drv = SessionDriver::new(
            db.clone(),
            GameConfig::commander(4),
            vec![d.clone(), d.clone(), d.clone(), d],
            80041,
        )
        .unwrap();
        assert_eq!(drv.session.current_actor(), Some(PlayerId(0)));
        drv.step(PlayerId(0), Action::Concede).unwrap();
        assert!(drv.session.state().player(PlayerId(0)).unwrap().lost);
        assert_eq!(drv.session.current_actor(), Some(PlayerId(1)));
        drv.keep_all_hands();
        assert_eq!(drv.session.state().active_player(), PlayerId(1));
    }

    // (b) P0 concedes during Main1 with spells on stack & stolen P1 permanent ->
    //     exiles P0's stack items, cleans up P0 objects, returns P1's stolen permanent, advances to P1!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[bear, steal, opt_spell], &[bear], &[plains], &[plains]],
            &[vec![cmd], vec![cmd], vec![cmd], vec![cmd]],
            4,
            GameConfig::commander(4),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 80042).unwrap();
        drv.keep_all_hands();

        // P1 casts Bear on Turn 2; P0 steals it on Turn 5, puts another Bear on stack, then P0 concedes!
        drv.advance_to(2, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(1), bear, vec![]).unwrap();
        drv.resolve_all_stack(16);
        let p1_bear = drv.find_battlefield_object(PlayerId(1), bear).unwrap();

        drv.advance_to(5, GamePhase::Main1, 48);
        drv.cast_from_hand(PlayerId(0), steal, vec![TargetRef::Object { id: p1_bear }])
            .unwrap();
        drv.resolve_all_stack(16);
        assert_eq!(
            drv.session.state().object(p1_bear).unwrap().controller,
            PlayerId(0)
        );

        drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
        assert_eq!(drv.session.state().stack().len(), 1);
        drv.step(PlayerId(0), Action::Concede).unwrap();
        assert!(drv.session.state().player(PlayerId(0)).unwrap().lost);
        assert!(drv.session.state().stack().is_empty());
        assert!(
            drv.session
                .state()
                .player(PlayerId(0))
                .unwrap()
                .battlefield
                .is_empty()
        );
        assert!(drv.find_battlefield_object(PlayerId(0), bear).is_none());
        assert!(
            drv.find_battlefield_object(PlayerId(1), bear).is_some()
                || drv.find_exile_object(PlayerId(1), bear).is_some(),
            "stolen P1 Bear must either return to P1 battlefield or be exiled per CR 800.4a"
        );
        // Priority passes away from conceded P0 immediately, and next turn advances to P1!
        assert_ne!(drv.session.current_actor(), Some(PlayerId(0)));
        let next_t = drv.session.state().turn_number() + 1;
        drv.advance_to(next_t, GamePhase::Main1, 32);
        assert_ne!(
            drv.session.state().active_player(),
            PlayerId(0),
            "active turn must advance away from conceded P0"
        );
    }

    // (c) Player concedes while holding an interactive prompt -> game recovers without deadlock!
    {
        let p0_deck = DeckSpec {
            commanders: vec![cmd],
            mainboard: vec![opt_spell; 15],
            sideboard: vec![],
        };
        let other = DeckSpec {
            commanders: vec![cmd],
            mainboard: vec![plains; 15],
            sideboard: vec![],
        };
        let mut drv = SessionDriver::new(
            db,
            GameConfig::commander(3),
            vec![p0_deck, other.clone(), other],
            80043,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(0), opt_spell, vec![]).unwrap();
        for _ in 0..3 {
            drv.pass_priority();
        }
        assert!(matches!(drv.decision(), DecisionState::Prompt { .. }));
        drv.step(PlayerId(0), Action::Concede).unwrap();
        assert!(drv.session.current_actor().is_some());
        assert!(!matches!(drv.decision(), DecisionState::Prompt { .. }));
    }
}

#[test]
fn synth_determinism_mulberry32_and_rename_invariance() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_det", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_det", "Synth Bear", "{W}", 2, 2);
    let anthem = b.add_synth_anthem("synth_anth_det", "Synth Anthem", "{W}", 1, 1);
    let bolt = b.add_synth_burn_instant("synth_bolt_det", "Synth Bolt", "{W}", 2);
    let cmd = b.add_synth_commander("synth_cmd_det", "Synth Cmd", "{0}", 3, 3);
    let db = b.build_db();

    let mut main = vec![plains; 12];
    main.extend(std::iter::repeat_n(bear, 8));
    main.extend(std::iter::repeat_n(anthem, 4));
    main.extend(std::iter::repeat_n(bolt, 4));
    let deck = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };

    // 1) Mulberry32Compat determinism: two identical sessions produce bit-identical logs & views.
    let mut cfg = GameConfig::commander(4);
    cfg.rng_mode = RngMode::Mulberry32Compat;
    let decks4 = vec![deck.clone(), deck.clone(), deck.clone(), deck];
    let mut s1 = MatchSession::new(Arc::clone(&db), cfg.clone(), decks4.clone(), 4242).unwrap();
    let mut s2 = MatchSession::new(db, cfg.clone(), decks4.clone(), 4242).unwrap();
    let drive = |s: &mut MatchSession| {
        for _ in 0..40 {
            if matches!(
                s.decision_info().state_kind,
                mtg_engine::DecisionKind::GameOver
            ) {
                break;
            }
            let actor = s.current_actor().unwrap();
            let legal = s.legal_actions(actor);
            let act = legal
                .into_iter()
                .find(|a| !matches!(a, Action::Concede | Action::TakeMulligan))
                .unwrap();
            s.apply_action(actor, act).unwrap();
        }
    };
    drive(&mut s1);
    drive(&mut s2);
    assert_eq!(s1.replay_log(), s2.replay_log());
    assert_eq!(s1.view_model(None), s2.view_model(None));

    // 2) Rename invariance across original vs `_alt` bundle!
    assert_rename_invariance(&b, cfg, decks4, 4242, drive);
}

#[test]
fn synth_match_session_snapshot_and_from_replay_mid_game_parity() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_snap", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_snap", "Synth Bear", "{W}", 2, 2);
    let scry_spell = b.add_card("synth_scry_snap", "Synth ScrySpell", |c| {
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

    let (cfg, decks) = common::guaranteed_decks(
        &[&[bear, scry_spell, plains], &[plains]],
        &[],
        5,
        GameConfig::standard_1v1(),
    );
    let mut session = MatchSession::new(Arc::clone(&db), cfg, decks, 5555).unwrap();

    // Invalid / truncated snapshot bytes must return Err(EngineError::Serialization(_))
    assert!(matches!(
        MatchSession::from_snapshot(Arc::clone(&db), b"BAD!"),
        Err(EngineError::Serialization(_))
    ));
    assert!(matches!(
        MatchSession::from_snapshot(Arc::clone(&db), b"MTGS\x01"),
        Err(EngineError::Serialization(_))
    ));

    // Snapshot 1: Mulligan
    assert_snapshot_equivalence(&session, &db, &[]);
    session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
    session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

    // Snapshot 2: Priority
    while session.state().phase() != GamePhase::Main1 {
        let a = session.current_actor().unwrap();
        session.apply_action(a, Action::PassPriority).unwrap();
    }
    assert_snapshot_equivalence(&session, &db, &[(PlayerId(0), Action::PassPriority)]);

    // Play Plains so CastingSequence stops at PayCosts for {W} Bear
    let plains_h = session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .hand
        .iter()
        .copied()
        .find(|&id| session.state().object(id).unwrap().card_def == plains)
        .unwrap();
    session
        .apply_action(
            PlayerId(0),
            Action::PlayLand {
                object: plains_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

    // Snapshot 3: CastingSequence (BeginCast)
    let bear_h = session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .hand
        .iter()
        .copied()
        .find(|&id| session.state().object(id).unwrap().card_def == bear)
        .unwrap();
    session
        .apply_action(
            PlayerId(0),
            Action::BeginCast {
                object: bear_h,
                option_id: 0,
                from_zone: Zone::Hand,
            },
        )
        .unwrap();
    assert_snapshot_equivalence(&session, &db, &[(PlayerId(0), Action::AutoPayMana)]);
    session
        .apply_action(PlayerId(0), Action::AutoPayMana)
        .unwrap();

    // Resolve bear, then Snapshot 4: mid-ResolutionFrame Prompt (Scry)!
    session
        .apply_action(PlayerId(0), Action::PassPriority)
        .unwrap();
    session
        .apply_action(PlayerId(1), Action::PassPriority)
        .unwrap();
    if let Some(scry_h) = session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .hand
        .iter()
        .copied()
        .find(|&id| session.state().object(id).unwrap().card_def == scry_spell)
    {
        session
            .apply_macro_action(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: scry_h,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![],
                },
            )
            .unwrap();
        session
            .apply_action(PlayerId(0), Action::PassPriority)
            .unwrap();
        session
            .apply_action(PlayerId(1), Action::PassPriority)
            .unwrap();
        assert!(matches!(
            session.view_model(None).decision,
            DecisionState::Prompt { .. }
        ));
        assert_snapshot_equivalence(&session, &db, &[]);
        while matches!(
            session.view_model(None).decision,
            DecisionState::Prompt { .. }
        ) {
            let act = session
                .legal_actions(PlayerId(0))
                .into_iter()
                .find(|a| !matches!(a, Action::Concede))
                .unwrap();
            session.apply_action(PlayerId(0), act).unwrap();
        }
    }

    // Snapshot 5: Advance to DeclareAttackers -> snapshot across 20 subsequent actions + from_replay parity!
    while session.state().phase() != GamePhase::DeclareAttackers {
        let a = session.current_actor().unwrap();
        session.apply_action(a, Action::PassPriority).unwrap();
    }
    let bytes = session.snapshot_bytes().unwrap();
    let mut restored = MatchSession::from_snapshot(Arc::clone(&db), &bytes).unwrap();
    for _ in 0..20 {
        if matches!(
            session.view_model(None).decision,
            DecisionState::GameOver { .. }
        ) {
            break;
        }
        let a1 = session.current_actor().unwrap();
        let a2 = restored.current_actor().unwrap();
        assert_eq!(a1, a2);
        let act = session
            .legal_actions(a1)
            .into_iter()
            .find(|a| !matches!(a, Action::Concede))
            .unwrap();
        let o1 = session.apply_action(a1, act.clone()).unwrap();
        let o2 = restored.apply_action(a2, act).unwrap();
        assert_eq!(o1, o2);
    }

    // MatchSession::from_replay parity!
    let replayed = MatchSession::from_replay(db, session.replay_log().clone()).unwrap();
    assert_eq!(replayed.view_model(None), session.view_model(None));
    assert_eq!(replayed.observe(PlayerId(0)), session.observe(PlayerId(0)));
}

#[test]
fn synth_observe_redaction_seat_rotation_and_flat_encoding() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_obs", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_commander("synth_cmd_obs", "Synth Cmd", "{0}", 3, 3);
    // Card that reveals opponents' hands and lets controller look at top of library
    let hand_viewer = b.add_card("synth_tel_obs", "Synth Lens", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::HandVisibility {
                    players: RelativeControl::Opponent,
                },
                condition: None,
            })
            .static_ability(StaticDef {
                body: StaticBody::TopOfLibraryVisibility {
                    reveal_to_all: false,
                    all_players: false,
                },
                condition: None,
            });
    });
    let bear = b.add_synth_vanilla("synth_bear_obs", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![
            hand_viewer,
            bear,
            plains,
            plains,
            plains,
            plains,
            plains,
            plains,
        ],
        sideboard: vec![],
    };
    let other_deck = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![bear, plains, plains, plains, plains, plains, plains, plains],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![p0_deck, other_deck.clone(), other_deck.clone(), other_deck],
        8005,
    )
    .unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // Before HandViewer: spectator view sees ZERO hands; P1's observe sees ZERO library cards & ZERO opponent hands!
    let spec_vm = drv.session.view_model(None);
    for p in &spec_vm.players {
        assert!(p.hand.is_empty(), "spectator view must never expose hands");
        assert!(p.revealed_hand.is_empty());
    }
    let obs_p1 = drv.session.observe(PlayerId(1));
    assert!(!obs_p1.entities.iter().any(|e| e.zone == Zone::Library));
    // P1's own seat has owner_rel == 0; P2 has owner_rel == 1; P0 has owner_rel == 3!
    assert_eq!(obs_p1.players[0].relative_seat, 0);
    assert_eq!(obs_p1.players[1].relative_seat, 1);

    // Cast HandViewer -> P0 sees opponents' revealed_hand & top_of_library!
    drv.cast_from_hand(PlayerId(0), hand_viewer, vec![])
        .unwrap();
    drv.resolve_all_stack(16);

    let vm_p0 = drv.session.view_model(Some(PlayerId(0)));
    assert!(vm_p0.players[1].hand_revealed_to_viewer);
    assert!(!vm_p0.players[1].revealed_hand.is_empty());
    assert!(
        vm_p0.players[0]
            .top_of_library
            .as_ref()
            .unwrap()
            .top_card
            .is_some()
    );

    // Flat observation encoding is deterministic & non-empty.
    let obs_p0 = drv.session.observe(PlayerId(0));
    let flat1 = encode_flat_observation(&obs_p0);
    let flat2 = encode_flat_observation(&obs_p0);
    assert_eq!(flat1, flat2);
    assert!(!flat1.player_features.is_empty());
}

#[test]
fn synth_determinize_zero_hidden_leak_and_face_down_recomputation() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_detz", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_detz", "Synth Bear", "{0}", 2, 2);
    let morph_a = b.add_card("synth_ma_detz", "Synth MorphA", |c| {
        c.mana_cost("{2}")
            .types(CardTypeMask::CREATURE)
            .pt(3, 3)
            .keyword_with_cost("morph", "{1}");
    });
    let morph_b = b.add_card("synth_mb_detz", "Synth MorphB", |c| {
        c.mana_cost("{4}")
            .types(CardTypeMask::CREATURE)
            .pt(5, 5)
            .keyword_with_cost("morph", "{3}");
    });
    let top_peek = b.add_card("synth_peek_detz", "Synth OracleLens", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::TopOfLibraryVisibility {
                    reveal_to_all: false,
                    all_players: false,
                },
                condition: None,
            });
    });
    let well = b.add_card("synth_well_detz", "Synth Well", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("www", &[ManaColor::W; 3]);
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            top_peek, bear, bear, plains, plains, plains, plains, plains, plains, plains,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            well, well, morph_a, morph_a, morph_b, morph_b, plains, plains, plains, plains,
        ],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 8006).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);
    if drv.find_hand_object(PlayerId(0), top_peek).is_some() {
        drv.cast_from_hand(PlayerId(0), top_peek, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    // P1 Turn 2: play Well, cast two face-down Morphs if drawn!
    drv.advance_to(2, GamePhase::Main1, 16);
    if let Some(wh) = drv.find_hand_object(PlayerId(1), well) {
        drv.step(
            PlayerId(1),
            Action::PlayLand {
                object: wh,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    for card in [morph_a, morph_b] {
        if let Some(mh) = drv.find_hand_object(PlayerId(1), card)
            && let Some(opt) =
                mtg_engine::enumerate_cast_options(drv.session.state(), PlayerId(1), mh)
                    .into_iter()
                    .find(|o| o.alt_kind == mtg_engine::AltCastKind::MorphFaceDown)
            && drv
                .macro_step(
                    PlayerId(1),
                    MacroAction::CastSpell {
                        object: mh,
                        option_id: opt.option_id,
                        from_zone: Zone::Hand,
                        modes: vec![],
                        x_value: None,
                        targets: vec![],
                    },
                )
                .is_ok()
        {
            drv.resolve_all_stack(8);
        }
    }

    // Viewer = P0: P0's hand and visible top-of-library MUST remain invariant across 20 seeds;
    // P1's hidden cards permute across seeds; FaceDownSpec turn-up cost matches reassigned CardDefId!
    let orig_p0_hand: Vec<_> = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .hand
        .iter()
        .map(|&id| drv.session.state().object(id).unwrap().card_def)
        .collect();
    let orig_p0_top = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .library
        .last()
        .map(|&id| drv.session.state().object(id).unwrap().card_def);

    let mut saw_p1_perm = false;
    let orig_p1_hand: Vec<_> = drv
        .session
        .state()
        .player(PlayerId(1))
        .unwrap()
        .hand
        .iter()
        .map(|&id| drv.session.state().object(id).unwrap().card_def)
        .collect();

    for seed in 1u64..=20 {
        let mut rng = RngState::splitmix64(seed * 7919);
        let det = drv.session.determinize(PlayerId(0), &mut rng);

        let det_p0_hand: Vec<_> = det
            .player(PlayerId(0))
            .unwrap()
            .hand
            .iter()
            .map(|&id| det.object(id).unwrap().card_def)
            .collect();
        assert_eq!(
            det_p0_hand, orig_p0_hand,
            "viewer hand must never be shuffled"
        );
        if drv.find_battlefield_object(PlayerId(0), top_peek).is_some() {
            let det_top = det
                .player(PlayerId(0))
                .unwrap()
                .library
                .last()
                .map(|&id| det.object(id).unwrap().card_def);
            assert_eq!(
                det_top, orig_p0_top,
                "visible top-of-library must be pinned"
            );
        }

        let det_p1_hand: Vec<_> = det
            .player(PlayerId(1))
            .unwrap()
            .hand
            .iter()
            .map(|&id| det.object(id).unwrap().card_def)
            .collect();
        if det_p1_hand != orig_p1_hand {
            saw_p1_perm = true;
        }

        // Verify FaceDownSpec turn_up_mana_cost matches reassigned CardDefId on every face-down BF creature!
        for &id in &det.player(PlayerId(1)).unwrap().battlefield {
            if det.object(id).unwrap().is_face_down()
                && let Some(fd) = det.rare(id).and_then(|r| r.face_down.as_deref())
            {
                let cdef = det.object(id).unwrap().card_def;
                if cdef == morph_a {
                    assert_eq!(fd.turn_up_mana_cost.as_ref().unwrap().generic, 1);
                } else if cdef == morph_b {
                    assert_eq!(fd.turn_up_mana_cost.as_ref().unwrap().generic, 3);
                }
            }
        }
        // Check LKI / event_log leak suppression
        assert!(det.event_log().is_empty());
    }
    assert!(
        saw_p1_perm,
        "opponent hidden cards should permute across determinization seeds"
    );
}

#[test]
fn synth_verify_layer_cache_random_walk_100_steps() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let plains = b.add_synth_basic_land("synth_pl_walk", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_walk", "Synth Bear", "{W}", 2, 2);
    let anthem = b.add_synth_anthem("synth_anth_walk", "Synth Anthem", "{W}", 1, 1);
    let boost = b.add_card("synth_boost_walk", "Synth Boost", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(plus_cid),
                        multi_counters: vec![],
                        select: Default::default(),
                        divided: false,
                        allow_exile_counters: false,
                        remove_instead_of_add: false,
                        copy_counters_from: None,
                        choose_counter_on_subject_and_add_one: false,
                        conditional_counter_on_type: None,
                        cast_bolas_bonus_loyalty: false,
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let cmd = b.add_synth_commander("synth_cmd_walk", "Synth Cmd", "{W}", 3, 3);
    let db = b.build_db();

    let mut main = vec![plains; 40];
    main.extend(std::iter::repeat_n(bear, 30));
    main.extend(std::iter::repeat_n(anthem, 15));
    main.extend(std::iter::repeat_n(boost, 14));
    let deck = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };

    let mut session = MatchSession::new(
        db,
        GameConfig::commander(4),
        vec![deck.clone(), deck.clone(), deck.clone(), deck],
        9001,
    )
    .unwrap();
    let mut rng = RngState::splitmix64(9001);

    for step_idx in 0..140 {
        if matches!(
            session.view_model(None).decision,
            DecisionState::GameOver { .. }
        ) {
            break;
        }
        let actor = session.current_actor().expect("active actor");
        let legal = session.legal_actions(actor);
        assert!(
            !legal.is_empty(),
            "legal_actions must be non-empty while active (step {step_idx})"
        );

        let non_concede: Vec<_> = legal
            .iter()
            .filter(|a| !matches!(a, Action::Concede | Action::TakeMulligan))
            .cloned()
            .collect();
        let chosen = if let Some(gameplay) = non_concede.iter().find(|a| {
            matches!(
                a,
                Action::PlayLand { .. }
                    | Action::BeginCast { .. }
                    | Action::AutoPayMana
                    | Action::SelectTarget { .. }
                    | Action::DoneTargets
                    | Action::AssignAttacker { .. }
                    | Action::ConfirmAttackers
                    | Action::ConfirmBlockers
            )
        }) {
            gameplay.clone()
        } else {
            let idx = (rng.next_u32() as usize) % non_concede.len().max(1);
            non_concede
                .get(idx)
                .cloned()
                .unwrap_or_else(|| legal[0].clone())
        };

        session
            .apply_action(actor, chosen)
            .expect("legal_actions must succeed");

        // Under --features verify-layer-cache, every evaluate_board() cross-checks cached vs uncached!
        let _ = session.state().evaluate_board();
        let obs = session.observe(actor);
        assert!(!obs.entities.iter().any(|e| e.zone == Zone::Library));
        let vm_other = session.view_model(Some(PlayerId((actor.0 + 1) % 4)));
        assert!(vm_other.players[usize::from(actor.0)].hand.is_empty());
    }
    assert!(session.state().turn_number() > 1);
}
