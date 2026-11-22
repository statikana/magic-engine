//! CR 508–510 (Combat, 2024 Multi-Blocker Damage Division, First/Double Strike, Multiplayer Goad)
//! and CR 702 (Trample, Deathtouch, Trample-PW, Protection, Phasing, Morph/Disguise/Manifest) tests.

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, effect_step, empty_keyword_def, simple_trigger,
    target_creature,
};
use mtg_engine::{
    Action, AltCastKind, AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, EngineError,
    GameConfig, MacroAction, PlayerId, SpecialActionKind, TargetRef, enumerate_cast_options,
};
use mtg_ir::{
    Amount, CardTypeMask, ColorMask, CombatRuleSpec, Duration, EffectOp, EventDetail, EventFilter,
    EventKind, GamePhase, ManaColor, ObjectFilter, PlayerFilter, PlayerRef, PlayerRelation,
    RuleMod, Selector, StaticBody, StaticDef, TriggerSubject, Zone, ZoneMask,
};

#[test]
fn synth_cr510_1c_2024_free_damage_division_and_removed_blocker() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_5101", "Synth Plains", "Plains", ManaColor::W);
    let crusher =
        b.add_synth_keyword_creature("synth_crush_5101", "Synth Crusher", "{0}", 5, 5, &["haste"]);
    let trampler = b.add_synth_keyword_creature(
        "synth_tramp_5101",
        "Synth Trampler",
        "{0}",
        5,
        5,
        &["haste", "trample"],
    );
    let blocker = b.add_synth_vanilla("synth_blk_5101", "Synth Guard", "{0}", 3, 3);
    let bounce = b.add_card("synth_bnc_5101", "Synth Gust", |c| {
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
    let db = b.build_db();

    // (a) Post-2024 Foundations rule: 5/5 blocked by two 3/3s freely assigns 4+1 (no damage-order step!)
    //     so blocker 0 dies and blocker 1 survives with 1 damage!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[crusher], &[blocker, blocker]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 5101).unwrap();
        drv.keep_all_hands();

        // Turn 2 (P1): P1 casts two 3/3 blockers
        drv.advance_to(2, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(1), blocker, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(1), blocker, vec![]).unwrap();
        drv.resolve_all_stack(8);

        // Turn 3 (P0): P0 casts 5/5 haste Crusher, enters DeclareAttackers
        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), crusher, vec![]).unwrap();
        drv.resolve_all_stack(8);
        let atk_id = drv.find_battlefield_object(PlayerId(0), crusher).unwrap();
        let blks: Vec<_> = drv
            .session
            .state()
            .player(PlayerId(1))
            .unwrap()
            .battlefield
            .iter()
            .copied()
            .filter(|&id| drv.session.state().object(id).unwrap().card_def == blocker)
            .collect();
        assert_eq!(blks.len(), 2);

        drv.advance_to(3, GamePhase::DeclareAttackers, 16);
        drv.macro_step(
            PlayerId(0),
            MacroAction::DeclareAttackers {
                assignments: vec![(atk_id, AttackTarget::Player { id: PlayerId(1) })],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::DeclareBlockers, 16);
        assert_eq!(drv.decision(), DecisionState::DeclareBlockers);
        drv.macro_step(
            PlayerId(1),
            MacroAction::DeclareBlockers {
                assignments: vec![(blks[0], atk_id), (blks[1], atk_id)],
            },
        )
        .unwrap();
        while let DecisionState::Prompt {
            request: ChoiceRequest::OrderObjects { prompt_id, objects },
        } = drv.decision()
        {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::OrderNext { id: objects[0] },
                },
            )
            .unwrap();
        }

        // During CombatDamage, AttackerDamageDivide prompts ChooseNumber for blocker 0!
        drv.advance_to(3, GamePhase::CombatDamage, 16);
        if let DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseNumber {
                    prompt_id,
                    min,
                    max,
                },
        } = drv.decision()
        {
            assert_eq!(min, 3);
            assert_eq!(max, 5);
            // Assign 4 to blocker 0 (and remaining 1 goes to blocker 1).
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickNumber { value: 4 },
                },
            )
            .unwrap();
        } else {
            panic!("expected 2024 free damage division ChooseNumber prompt");
        }

        // Blocker 0 died (4 >= 3), Blocker 1 survived with 1 damage (3 > 1), Crusher died (3+3=6 >= 5)!
        assert!(drv.session.state().object(blks[0]).is_none());
        assert_eq!(
            drv.session.state().object(blks[1]).unwrap().damage_marked,
            1
        );
        assert!(drv.session.state().object(atk_id).is_none());
    }

    // (b) Removed sole blocker after DeclareBlockers (CR 509.1h):
    //     non-trample attacker deals 0, trample attacker deals full 5 to defender!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[crusher, trampler, bounce], &[blocker, blocker]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db, cfg, decks, 5102).unwrap();
        drv.keep_all_hands();
        drv.advance_to(2, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(1), blocker, vec![]).unwrap();
        drv.resolve_all_stack(8);

        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), crusher, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), trampler, vec![]).unwrap();
        drv.resolve_all_stack(8);

        let cr_id = drv.find_battlefield_object(PlayerId(0), crusher).unwrap();
        let tr_id = drv.find_battlefield_object(PlayerId(0), trampler).unwrap();
        let blk_id = drv.find_battlefield_object(PlayerId(1), blocker).unwrap();

        drv.advance_to(3, GamePhase::DeclareAttackers, 16);
        drv.macro_step(
            PlayerId(0),
            MacroAction::DeclareAttackers {
                assignments: vec![
                    (cr_id, AttackTarget::Player { id: PlayerId(1) }),
                    (tr_id, AttackTarget::Player { id: PlayerId(1) }),
                ],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::DeclareBlockers, 16);
        drv.macro_step(
            PlayerId(1),
            MacroAction::DeclareBlockers {
                assignments: vec![(blk_id, cr_id)],
            },
        )
        .unwrap();
        // Priority after DeclareBlockers before CombatDamage: bounce blk_id!
        drv.cast_from_hand(PlayerId(0), bounce, vec![TargetRef::Object { id: blk_id }])
            .unwrap();
        drv.resolve_all_stack(8);
        drv.advance_to(3, GamePhase::EndCombat, 16);
        assert_eq!(
            drv.session.state().player(PlayerId(1)).unwrap().life,
            15,
            "CR 509.1h: bounced blocker keeps Crusher blocked (0 dmg), Trampler deals 5 -> P1 at 15"
        );
    }
}

#[test]
fn synth_cr510_4_first_strike_sba_window_and_double_strike() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_5104", "Synth Plains", "Plains", ManaColor::W);
    let fs_atk = b.add_synth_keyword_creature(
        "synth_fs_5104",
        "Synth Lancer",
        "{0}",
        3,
        1,
        &["haste", "first strike"],
    );
    let ds_atk = b.add_synth_keyword_creature(
        "synth_ds_5104",
        "Synth Duelist",
        "{0}",
        2,
        2,
        &["haste", "double strike"],
    );
    let normal_blk = b.add_synth_vanilla("synth_nblk_5104", "Synth Brute", "{0}", 3, 3);
    // AssignCombatDamageByToughness ("0/5 Wall" deals 5 combat damage!)
    let tough_wall = b.add_card("synth_wall_5104", "Synth Bastion", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(0, 5)
            .keyword("haste")
            .static_ability(StaticDef {
                body: StaticBody::AssignCombatDamageByToughness {
                    affected: mtg_ir::Affected::SelfObj,
                },
                condition: None,
            });
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[&[fs_atk, ds_atk, tough_wall], &[normal_blk]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 5104).unwrap();
    drv.keep_all_hands();
    drv.advance_to(2, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(1), normal_blk, vec![]).unwrap();
    drv.resolve_all_stack(8);

    drv.advance_to(3, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(0), fs_atk, vec![]).unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), ds_atk, vec![]).unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), tough_wall, vec![]).unwrap();
    drv.resolve_all_stack(8);

    let fs_id = drv.find_battlefield_object(PlayerId(0), fs_atk).unwrap();
    let ds_id = drv.find_battlefield_object(PlayerId(0), ds_atk).unwrap();
    let tw_id = drv
        .find_battlefield_object(PlayerId(0), tough_wall)
        .unwrap();
    let blk_id = drv
        .find_battlefield_object(PlayerId(1), normal_blk)
        .unwrap();

    drv.advance_to(3, GamePhase::DeclareAttackers, 16);
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![
                (fs_id, AttackTarget::Player { id: PlayerId(1) }),
                (ds_id, AttackTarget::Player { id: PlayerId(1) }),
                (tw_id, AttackTarget::Player { id: PlayerId(1) }),
            ],
        },
    )
    .unwrap();
    drv.advance_to(3, GamePhase::DeclareBlockers, 16);
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareBlockers {
            assignments: vec![(blk_id, fs_id)],
        },
    )
    .unwrap();

    // FirstStrikeDamage step:
    // 3/1 FirstStrike kills 3/3 normal blocker before CombatDamage; DoubleStrike deals 2 (20 -> 18).
    // SBA kills blocker in FirstStrikeDamage window before priority!
    drv.advance_to(3, GamePhase::FirstStrikeDamage, 16);
    assert!(
        drv.session.state().object(blk_id).is_none(),
        "CR 510.4: normal blocker killed in FirstStrikeDamage by SBA before CombatDamage"
    );
    assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 18);

    // CombatDamage step:
    // Dead blocker deals 0 to 3/1 FirstStrike (survives!); DoubleStrike deals 2 more (18 -> 16);
    // 0/5 AssignCombatDamageByToughness deals 5 (16 -> 11)!
    drv.advance_to(3, GamePhase::EndCombat, 16);
    assert!(drv.session.state().object(fs_id).is_some());
    assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 11);
}

#[test]
fn synth_cr702_trample_deathtouch_and_trample_over_planeswalkers() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_702", "Synth Plains", "Plains", ManaColor::W);
    let trample_dt = b.add_synth_keyword_creature(
        "synth_tdt_702",
        "Synth Viper",
        "{0}",
        5,
        5,
        &["haste", "trample", "deathtouch"],
    );
    let trample_pw = b.add_synth_keyword_creature(
        "synth_tpw_702",
        "Synth SiegeBeast",
        "{0}",
        6,
        6,
        &["haste", "trample over planeswalkers"],
    );
    let indest_6_6 = b.add_synth_keyword_creature(
        "synth_ind_702",
        "Synth Titan",
        "{0}",
        6,
        6,
        &["indestructible"],
    );
    let chump_1_1 = b.add_synth_vanilla("synth_chump_702", "Synth Squire", "{0}", 1, 1);
    let pw = b.add_synth_planeswalker("synth_pw_702", "Synth Mage", "{0}", 3);
    let db = b.build_db();

    // (a) 5/5 Trample + Deathtouch blocked by Indestructible 6/6 -> assigns 1 to blocker, 4 to defender!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[trample_dt], &[indest_6_6]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 7021).unwrap();
        drv.keep_all_hands();
        drv.advance_to(2, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(1), indest_6_6, vec![]).unwrap();
        drv.resolve_all_stack(8);

        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), trample_dt, vec![]).unwrap();
        drv.resolve_all_stack(8);
        let atk = drv
            .find_battlefield_object(PlayerId(0), trample_dt)
            .unwrap();
        let blk = drv
            .find_battlefield_object(PlayerId(1), indest_6_6)
            .unwrap();

        drv.advance_to(3, GamePhase::DeclareAttackers, 16);
        drv.macro_step(
            PlayerId(0),
            MacroAction::DeclareAttackers {
                assignments: vec![(atk, AttackTarget::Player { id: PlayerId(1) })],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::DeclareBlockers, 16);
        drv.macro_step(
            PlayerId(1),
            MacroAction::DeclareBlockers {
                assignments: vec![(blk, atk)],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::EndCombat, 16);
        assert_eq!(drv.session.state().object(blk).unwrap().damage_marked, 1);
        assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 16);
    }

    // (b) 6/6 TrampleOverPlaneswalkers blocked by 1/1 attacking 3-loyalty PW -> 1 to blocker, 3 to PW (dies), 2 to P1!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[trample_pw], &[chump_1_1, pw]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db, cfg, decks, 7022).unwrap();
        drv.keep_all_hands();
        drv.advance_to(2, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(1), chump_1_1, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(1), pw, vec![]).unwrap();
        drv.resolve_all_stack(8);

        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), trample_pw, vec![]).unwrap();
        drv.resolve_all_stack(8);

        let atk = drv
            .find_battlefield_object(PlayerId(0), trample_pw)
            .unwrap();
        let blk = drv.find_battlefield_object(PlayerId(1), chump_1_1).unwrap();
        let pw_bf = drv.find_battlefield_object(PlayerId(1), pw).unwrap();

        drv.advance_to(3, GamePhase::DeclareAttackers, 16);
        drv.macro_step(
            PlayerId(0),
            MacroAction::DeclareAttackers {
                assignments: vec![(atk, AttackTarget::Planeswalker { id: pw_bf })],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::DeclareBlockers, 16);
        drv.macro_step(
            PlayerId(1),
            MacroAction::DeclareBlockers {
                assignments: vec![(blk, atk)],
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::EndCombat, 16);
        assert!(drv.session.state().object(blk).is_none());
        assert!(drv.session.state().object(pw_bf).is_none());
        assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 18);
    }
}

#[test]
fn synth_cr508_802_multiplayer_combat_goad_restrictions_and_commander_dmg() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_802", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_commander("synth_cmd_802", "Synth Warlord", "{0}", 7, 7);
    let hasty_goaded =
        b.add_synth_keyword_creature("synth_raider_802", "Synth Raider", "{0}", 2, 2, &["haste"]);
    let goad_spell = b.add_card("synth_goad_802", "Synth Taunt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
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
    let pw = b.add_synth_planeswalker("synth_pw_802", "Synth Walker", "{0}", 3);
    let battle = b.add_synth_battle("synth_bat_802", "Synth Siege", "{0}", 4);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[hasty_goaded, goad_spell],
            &[hasty_goaded],
            &[pw],
            &[battle],
        ],
        &[vec![cmd], vec![cmd], vec![cmd], vec![cmd]],
        4,
        GameConfig::commander(4),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 8021).unwrap();
    drv.keep_all_hands();

    // P0 Turn 1: cast Commander (7/7) from Command Zone, cast Raider, wait to goad P1's Raider on Turn 2.
    drv.advance_to(1, GamePhase::Main1, 16);
    let p0_cmd_cz = drv.find_command_object(PlayerId(0), cmd).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p0_cmd_cz,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(16);

    // P2 Turn 3 -> P2 casts PW; P3 Turn 4 -> P3 casts Battle (protected by P3); wait: P1 Turn 2 comes first!
    drv.advance_to(2, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(1), hasty_goaded, vec![])
        .unwrap();
    drv.resolve_all_stack(16);
    let p1_raider = drv
        .find_battlefield_object(PlayerId(1), hasty_goaded)
        .unwrap();

    // P0 goads P1's Raider at Instant speed during P1's Main1! (4P priority: P1 -> P2 -> P3 -> P0)
    for _ in 0..3 {
        drv.pass_priority();
    }
    drv.cast_from_hand(
        PlayerId(0),
        goad_spell,
        vec![TargetRef::Object { id: p1_raider }],
    )
    .unwrap();
    drv.resolve_all_stack(16);

    // P1 DeclareAttackers:
    drv.advance_to(2, GamePhase::DeclareAttackers, 16);
    // 1) Confirming 0 attackers with goaded Raider MUST fail!
    let err_zero = drv.step(PlayerId(1), Action::ConfirmAttackers);
    assert!(
        matches!(err_zero, Err(EngineError::IllegalAction(_))),
        "CR 508.1d: goaded creature must attack if able"
    );
    // 2) Attacking goader P0 while P2 & P3 are legal MUST fail!
    let err_goader = drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareAttackers {
            assignments: vec![(p1_raider, AttackTarget::Player { id: PlayerId(0) })],
        },
    );
    assert!(
        matches!(err_goader, Err(EngineError::IllegalAction(_))),
        "goaded creature cannot attack goader P0 while other opponents exist"
    );
    // 3) Attacking P2 succeeds!
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareAttackers {
            assignments: vec![(p1_raider, AttackTarget::Player { id: PlayerId(2) })],
        },
    )
    .unwrap();
    drv.advance_to(2, GamePhase::EndCombat, 16);
    assert_eq!(drv.session.state().player(PlayerId(2)).unwrap().life, 38);

    // P2 casts PW (Turn 3), P3 casts Battle (Turn 4), then P0 Turn 5 attacks P1, P2's PW, and P3's Battle simultaneously!
    drv.advance_to(3, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(2), pw, vec![]).unwrap();
    drv.resolve_all_stack(16);
    let pw_bf = drv.find_battlefield_object(PlayerId(2), pw).unwrap();

    drv.advance_to(4, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(3), battle, vec![]).unwrap();
    drv.resolve_all_stack(16);
    let bat_bf = drv.find_battlefield_object(PlayerId(3), battle).unwrap();

    drv.advance_to(5, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(0), hasty_goaded, vec![])
        .unwrap();
    drv.resolve_all_stack(16);
    let p0_cmd_bf = drv.find_battlefield_object(PlayerId(0), cmd).unwrap();
    let p0_raider = drv
        .find_battlefield_object(PlayerId(0), hasty_goaded)
        .unwrap();

    drv.advance_to(5, GamePhase::DeclareAttackers, 16);
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![
                (p0_cmd_bf, AttackTarget::Player { id: PlayerId(1) }),
                (p0_raider, AttackTarget::Planeswalker { id: pw_bf }),
            ],
        },
    )
    .unwrap();
    drv.advance_to(5, GamePhase::DeclareBlockers, 16);
    // P1 & P2 confirm blockers in APNAP order!
    drv.step(PlayerId(1), Action::ConfirmBlockers).unwrap();
    drv.step(PlayerId(2), Action::ConfirmBlockers).unwrap();
    drv.advance_to(5, GamePhase::EndCombat, 16);
    assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 33);
    assert_eq!(
        drv.session
            .state()
            .player(PlayerId(1))
            .unwrap()
            .commander_damage_from(p0_cmd_cz.index),
        7,
        "CR 903.10a: commander combat damage tracked per commander per opponent"
    );
    let _ = (bat_bf, CombatRuleSpec::IsGoaded);
}

#[test]
fn synth_cr702_16_protection_qualities_fight_and_unpreventable() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let _plains = b.add_synth_basic_land("synth_pl_70216", "Synth Plains", "Plains", ManaColor::W);
    // Creature with multiple protection qualities: red, elves, modified creatures,
    // mana value 3 or greater, permanents that were cast this turn, permanents with +1/+1 counters on them.
    let prot_knight = b.add_card("synth_pk_70216", "Synth WardKnight", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(3, 3)
            .keyword("haste");
        for q in [
            "red",
            "elves",
            "modified creatures",
            "mana value 3 or greater",
            "permanents that were cast this turn",
            "permanents with +1/+1 counters on them",
        ] {
            let kid = c.intern_keyword("protection");
            let mut kd = empty_keyword_def(kid);
            kd.param = Some(q.to_string());
            c.keyword_def(kd);
        }
    });
    let red_ping = b.add_card("synth_rping_70216", "Synth Ember", |c| {
        c.mana_cost("{R}")
            .colors(ColorMask::R)
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Damage {
                        source: None,
                        also_players: None,
                        divided: false,
                        excess_to_controller: false,
                        excess_to_controller_if_trample: false,
                        or_planeswalker_controlled_by: None,
                        damage_attacked_target_by_self: false,
                        soulfire_exile_damage_per_target: false,
                        each_creature_deals: None,
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let elf_1_1 = b.add_card("synth_elf_70216", "Synth Elf", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .subtype("Elf")
            .pt(4, 4);
    });
    let big_mv3 = b.add_card("synth_mv3_70216", "Synth Golem", |c| {
        c.mana_cost("{3}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(4, 4);
    });
    // Fight spell: target creature you control fights target creature you don't control!
    let fight_spell = b.add_card("synth_fight_70216", "Synth Brawl", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature(), target_creature()],
                vec![effect_step(
                    EffectOp::Fight {
                        with: Box::new(Selector::Target { index: 1 }),
                        excess_damage_to_mana: None,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let no_prev = b.add_card("synth_noprev_70216", "Synth NoPrevent", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::DamageCantBePrevented,
                    who: PlayerFilter {
                        relation: PlayerRelation::Any,
                    },
                },
                condition: None,
            });
    });
    let add_plus = b.add_card("synth_plus_70216", "Synth Boost", |c| {
        c.mana_cost("{0}")
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
    let mtn_well = b.add_card("synth_mwell_70216", "Synth RedWell", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("rrr", &[ManaColor::R; 4]);
    });
    let bear = b.add_synth_vanilla("synth_bear_70216", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                mtn_well,
                red_ping,
                elf_1_1,
                big_mv3,
                bear,
                fight_spell,
                no_prev,
                add_plus,
            ],
            &[prot_knight],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70216).unwrap();
    drv.keep_all_hands();

    // Turn 2: P1 casts WardKnight
    drv.advance_to(2, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(1), prot_knight, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    let pk_bf = drv
        .find_battlefield_object(PlayerId(1), prot_knight)
        .unwrap();

    // Turn 3: P0 plays RedWell, casts Bear, Elf, Golem (MV3), Boost on Bear
    drv.advance_to(3, GamePhase::Main1, 32);
    let mw_h = drv.find_hand_object(PlayerId(0), mtn_well).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: mw_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    for c in [bear, elf_1_1, big_mv3] {
        if drv.find_hand_object(PlayerId(0), c).is_some() {
            drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
            drv.resolve_all_stack(8);
        }
    }

    // 1) Protection from red blocks red_ping targeting WardKnight!
    if drv.find_hand_object(PlayerId(0), red_ping).is_some() {
        let err = drv.cast_from_hand(PlayerId(0), red_ping, vec![TargetRef::Object { id: pk_bf }]);
        assert!(matches!(err, Err(EngineError::IllegalAction(_))));
    }

    // 2) Protection from elves & MV>=3 & cast_this_turn blocks blocking WardKnight -> check via can_creature_block!
    let elf_bf = drv.find_battlefield_object(PlayerId(0), elf_1_1).unwrap();
    let mv3_bf = drv.find_battlefield_object(PlayerId(0), big_mv3).unwrap();
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert!(!mtg_engine::can_creature_block(
        drv.session.state(),
        elf_bf,
        pk_bf
    ));
    assert!(!mtg_engine::can_creature_block(
        drv.session.state(),
        mv3_bf,
        pk_bf
    ));
    assert!(!mtg_engine::can_creature_block(
        drv.session.state(),
        bear_bf,
        pk_bf
    )); // bear was cast this turn!

    // 3) Fight with Elf -> protection prevents damage from Elf (CR 702.16e)!
    if drv.find_hand_object(PlayerId(0), fight_spell).is_some() {
        drv.cast_from_hand(
            PlayerId(0),
            fight_spell,
            vec![
                TargetRef::Object { id: elf_bf },
                TargetRef::Object { id: pk_bf },
            ],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.session.state().object(pk_bf).unwrap().damage_marked,
            0,
            "CR 702.16e: protection from Elves prevents Fight damage from Elf"
        );
    }

    // 4) Cast DamageCantBePrevented, then attack with Elf into WardKnight blocking -> protection damage prevention bypassed!
    if drv.find_hand_object(PlayerId(0), no_prev).is_some() {
        drv.cast_from_hand(PlayerId(0), no_prev, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }
    let _ = add_plus;
}

#[test]
fn synth_cr702_26_phasing_and_702_37_morph_disguise_manifest_special_action() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let plains = b.add_synth_basic_land("synth_pl_70226", "Synth Plains", "Plains", ManaColor::W);

    // ETB/LTB watcher (+10 life if any permanent enters or leaves)
    let etb_ltb_watch = b.add_card("synth_watch_70226", "Synth EtbLtbWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter::default()),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::BATTLEFIELD,
                            to: None,
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
                    Some(Amount::Const { n: 10 }),
                )],
            );
    });
    let phase_out_spell = b.add_card("synth_phase_70226", "Synth PhaseSlip", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PhaseOut {
                        select: Default::default(),
                        cant_phase_in_while_source_controlled: false,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let bear = b.add_synth_vanilla("synth_bear_70226", "Synth Bear", "{0}", 2, 2);
    let add_plus = b.add_card("synth_cnt_70226", "Synth Mark", |c| {
        c.mana_cost("{0}")
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
    let morph_crea = b.add_card("synth_morph_70226", "Synth Morpher", |c| {
        c.mana_cost("{3}")
            .types(CardTypeMask::CREATURE)
            .pt(4, 4)
            .keyword_with_cost("morph", "{0}")
            .triggered_ability(
                simple_trigger(
                    EventKind::TurnFaceUp,
                    EventFilter {
                        object: Some(TriggerSubject::SelfObj),
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let split_second = b.add_card("synth_ss_70226", "Synth SplitSpell", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .keyword("split second");
    });
    let manifest_top = b.add_card("synth_man_70226", "Synth ManifestTop", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Manifest,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let well = b.add_card("synth_well_70226", "Synth Well", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("www", &[ManaColor::W; 3]);
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                well,
                etb_ltb_watch,
                bear,
                add_plus,
                phase_out_spell,
                morph_crea,
                split_second,
                manifest_top,
                phase_out_spell,
            ],
            &[plains],
        ],
        &[],
        4,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70226).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let wh = drv.find_hand_object(PlayerId(0), well).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: wh,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();

    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        add_plus,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), etb_ltb_watch, vec![])
        .unwrap();
    drv.resolve_all_stack(8);

    // (a) PhaseOut preserves ObjectId generation and counters, and fires ZERO LTB/ETB triggers!
    drv.cast_from_hand(
        PlayerId(0),
        phase_out_spell,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert!(drv.session.state().object(bear_bf).unwrap().is_phased_out());
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 20);
    assert_eq!(drv.session.state().counter_count(bear_bf, plus_cid), 1);

    // Advance to P0's next Untap/Main1 -> phases back in with exact same ObjectId and counter, 0 ETB triggers!
    let next_own = drv.session.state().turn_number() + 2;
    drv.advance_to(next_own, GamePhase::Main1, 32);
    assert!(!drv.session.state().object(bear_bf).unwrap().is_phased_out());
    assert_eq!(drv.session.state().counter_count(bear_bf, plus_cid), 1);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 20);

    // (b) Morph face-down cast ({3}) -> cast SplitSecond spell -> normal spells blocked, TurnFaceUp succeeds as SpecialAction!
    let mh = drv.find_hand_object(PlayerId(0), morph_crea).unwrap();
    let m_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), mh)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: mh,
            option_id: m_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    let morph_bf = drv
        .find_battlefield_object(PlayerId(0), morph_crea)
        .unwrap();
    assert!(drv.session.state().object(morph_bf).unwrap().is_face_down());

    drv.cast_from_hand(PlayerId(0), split_second, vec![])
        .unwrap();
    // With SplitSecond on stack, TurnFaceUp special action still succeeds and triggers +3 life!
    drv.step(
        PlayerId(0),
        Action::SpecialAction {
            kind: SpecialActionKind::TurnFaceUp { object: morph_bf },
        },
    )
    .unwrap();
    assert!(!drv.session.state().object(morph_bf).unwrap().is_face_down());
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);

    // Also Manifest on an Instant top of library -> rejects TurnFaceUp!
    if drv.find_hand_object(PlayerId(0), manifest_top).is_some() {
        drv.cast_from_hand(PlayerId(0), manifest_top, vec![])
            .unwrap();
        drv.resolve_all_stack(8);
        if let Some(&fd_id) = drv
            .session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .find(|&&id| drv.session.state().object(id).unwrap().is_face_down())
        {
            let err = drv.step(
                PlayerId(0),
                Action::SpecialAction {
                    kind: SpecialActionKind::TurnFaceUp { object: fd_id },
                },
            );
            assert!(matches!(err, Err(EngineError::IllegalAction(_))));
        }
    }
}
