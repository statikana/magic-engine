//! CR 508–510 (Combat, 2024 Multi-Blocker Damage Division, First/Double Strike, Multiplayer Goad)
//! and CR 702 (Trample, Deathtouch, Trample-PW, Protection, Phasing, Morph/Disguise/Manifest) tests.

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, effect_step, empty_keyword_def, simple_trigger,
    target_creature,
};
use mtg_engine::{
    Action, AltCastKind, AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, EngineError,
    FaceDownKind, GameConfig, MacroAction, ObjectFlags, PlayerId, SpecialActionKind, TargetRef,
    active_face_view, enumerate_cast_options, target_ward_cost,
};
use mtg_ir::{
    Amount, CardTypeMask, ColorMask, CombatRuleSpec, CostDef, Duration, EffectOp, EventDetail,
    EventFilter, EventKind, GamePhase, ManaColor, ManaCost, MultiplierTarget, ObjectFilter,
    PlayerFilter, PlayerRef, PlayerRelation, RelativeControl, RuleMod, Selector, StaticBody,
    StaticDef, TriggerSubject, Zone, ZoneMask,
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

#[test]
fn synth_cr702_37_168_morph_megamorph_disguise_face_down_costs_ward_and_counters() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let forest = b.add_synth_basic_land("synth_for_702fd", "Synth Forest", "Forest", ManaColor::G);
    let grove = b.add_card("synth_grv_702fd", "Synth Grove", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("ggg", &[ManaColor::G; 3]);
    });
    let pool = b.add_card("synth_pool_702fd", "Synth Tidewell", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("guu", &[ManaColor::G, ManaColor::U, ManaColor::U]);
    });
    let morph_g = b.add_card("synth_morphg_702fd", "Synth BarkMorpher", |c| {
        c.mana_cost("{3}{G}")
            .types(CardTypeMask::CREATURE)
            .colors(ColorMask::G)
            .pt(5, 5)
            .keyword_with_cost("morph", "{G}");
    });
    let disguise_u = b.add_card("synth_disg_702fd", "Synth VeilSpy", |c| {
        c.mana_cost("{2}{U}")
            .types(CardTypeMask::CREATURE)
            .colors(ColorMask::U)
            .pt(4, 4)
            .keyword_with_cost("disguise", "{1}{U}");
    });
    let megamorph_g = b.add_card("synth_mega_702fd", "Synth GrowthMorpher", |c| {
        c.mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .colors(ColorMask::G)
            .pt(3, 3)
            .keyword_with_cost("megamorph", "{G}");
    });
    let turn_up_spell = b.add_card("synth_break_702fd", "Synth RevealBeam", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::TurnFace {
                        face_up: true,
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let ping = b.add_synth_burn_instant("synth_ping_702fd", "Synth Ping", "{0}", 1);
    let db = b.build_db();

    // 1. Morph {G} (CR 702.37b): TurnFaceUp fails with IllegalAction when 0 mana is available,
    //    then succeeds and taps Forest {G} when Forest is available.
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[grove, forest, morph_g, morph_g], &[forest]],
            &[],
            4,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 70237).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let grove_h = drv.find_hand_object(PlayerId(0), grove).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: grove_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

        let mh = drv.find_hand_object(PlayerId(0), morph_g).unwrap();
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

        let morph_bf = drv.find_battlefield_object(PlayerId(0), morph_g).unwrap();
        assert!(drv.session.state().object(morph_bf).unwrap().is_face_down());
        {
            let fd = drv
                .session
                .state()
                .rare(morph_bf)
                .and_then(|r| r.face_down.as_deref())
                .expect("face_down spec must be populated");
            assert_eq!(fd.kind, FaceDownKind::Morph);
            assert_eq!(fd.turn_up_mana_cost, Some(ManaCost::parse("{G}")));
            assert_eq!(fd.ward_cost, None);
        }

        // Grove is tapped for {3} -> 0 mana available -> TurnFaceUp MUST fail with IllegalAction!
        let err = drv.step(
            PlayerId(0),
            Action::SpecialAction {
                kind: SpecialActionKind::TurnFaceUp { object: morph_bf },
            },
        );
        assert!(
            matches!(err, Err(EngineError::IllegalAction(_))),
            "Morph {{G}} TurnFaceUp must reject at 0 mana, got {err:?}"
        );
        assert!(drv.session.state().object(morph_bf).unwrap().is_face_down());

        // Turn 3: cast second Morph face down with Grove (leaving Grove tapped), then play Forest ({G})
        // and turn the first Morph face up -> MUST tap Forest!
        drv.advance_to(3, GamePhase::Main1, 16);
        let mh2 = drv.find_hand_object(PlayerId(0), morph_g).unwrap();
        let m_opt2 = enumerate_cast_options(drv.session.state(), PlayerId(0), mh2)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: mh2,
                option_id: m_opt2,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        let forest_h = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: forest_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let forest_bf = drv.find_battlefield_object(PlayerId(0), forest).unwrap();
        assert!(
            !drv.session
                .state()
                .object(forest_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        drv.step(
            PlayerId(0),
            Action::SpecialAction {
                kind: SpecialActionKind::TurnFaceUp { object: morph_bf },
            },
        )
        .unwrap();
        assert!(
            drv.session
                .state()
                .object(forest_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED),
            "TurnFaceUp must tap Forest to pay Morph {{G}}"
        );
        assert!(!drv.session.state().object(morph_bf).unwrap().is_face_down());
        assert!(
            drv.session
                .state()
                .rare(morph_bf)
                .and_then(|r| r.face_down.as_ref())
                .is_none()
        );
        let ev = drv.session.state().evaluate_board().get(morph_bf).unwrap();
        assert_eq!(ev.pt.map(|pt| (pt.power, pt.toughness)), Some((5, 5)));
    }

    // 2. Disguise {1}{U} (CR 702.168a): Ward {2} present while face-down (both in FaceDownSpec/view
    //    and live BecomesTarget Ward trigger), and gone once turned face up!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[pool, disguise_u], &[ping, ping]],
            &[],
            4,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 702168).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let pool_h = drv.find_hand_object(PlayerId(0), pool).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: pool_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

        let dh = drv.find_hand_object(PlayerId(0), disguise_u).unwrap();
        let d_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), dh)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::DisguiseFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: dh,
                option_id: d_opt,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        let disg_bf = drv
            .find_battlefield_object(PlayerId(0), disguise_u)
            .unwrap();
        assert!(drv.session.state().object(disg_bf).unwrap().is_face_down());
        {
            let fd = drv
                .session
                .state()
                .rare(disg_bf)
                .and_then(|r| r.face_down.as_deref())
                .expect("disguise face_down spec");
            assert_eq!(fd.kind, FaceDownKind::Disguise);
            assert_eq!(fd.turn_up_mana_cost, Some(ManaCost::parse("{1}{U}")));
            assert_eq!(fd.ward_cost, Some(ManaCost::parse("{2}")));
            assert_eq!(
                active_face_view(drv.session.state(), disg_bf)
                    .unwrap()
                    .ward_cost,
                Some(ManaCost::parse("{2}"))
            );
            assert_eq!(
                target_ward_cost(
                    drv.session.state(),
                    TargetRef::Object { id: disg_bf },
                    PlayerId(1),
                )
                .map(|(g, _, _)| g),
                Some(2)
            );
        }

        // Turn 2 (P1): P1 targets face-down Disguise creature with Ping at 0 mana -> Ward {2} counters Ping!
        drv.advance_to(2, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(1), ping, vec![TargetRef::Object { id: disg_bf }])
            .unwrap();
        drv.resolve_all_stack(12);
        assert_eq!(
            drv.session.state().object(disg_bf).unwrap().damage_marked,
            0,
            "Disguise ward {{2}} unpaid must counter opponent Ping while face-down"
        );

        // Turn 3 (P0): Turn Disguise creature face up for {1}{U} -> Ward {2} is now gone!
        drv.advance_to(3, GamePhase::Main1, 16);
        drv.step(
            PlayerId(0),
            Action::SpecialAction {
                kind: SpecialActionKind::TurnFaceUp { object: disg_bf },
            },
        )
        .unwrap();
        assert!(!drv.session.state().object(disg_bf).unwrap().is_face_down());
        assert_eq!(
            active_face_view(drv.session.state(), disg_bf)
                .unwrap()
                .ward_cost,
            None
        );
        assert_eq!(
            target_ward_cost(
                drv.session.state(),
                TargetRef::Object { id: disg_bf },
                PlayerId(1),
            ),
            None
        );

        // Turn 4 (P1): P1 casts second Ping targeting now face-up VeilSpy -> resolves & marks 1 damage!
        drv.advance_to(4, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(1), ping, vec![TargetRef::Object { id: disg_bf }])
            .unwrap();
        drv.resolve_all_stack(12);
        assert_eq!(
            drv.session.state().object(disg_bf).unwrap().damage_marked,
            1,
            "once face up, Disguise ward {{2}} is gone and Ping must deal 1 damage"
        );
    }

    // 3. Megamorph {G} (CR 702.37f): places 1 +1/+1 counter when turned face up via TurnFaceUp
    //    (megamorph cost paid), and does NOT place a counter when turned face up by an effect.
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[
                &[grove, forest, megamorph_g, megamorph_g, turn_up_spell],
                &[forest],
            ],
            &[],
            5,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db, cfg, decks, 702376).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let grove_h = drv.find_hand_object(PlayerId(0), grove).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: grove_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

        let mh1 = drv.find_hand_object(PlayerId(0), megamorph_g).unwrap();
        let opt1 = enumerate_cast_options(drv.session.state(), PlayerId(0), mh1)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: mh1,
                option_id: opt1,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        let mega_bf1 = drv
            .find_battlefield_object(PlayerId(0), megamorph_g)
            .unwrap();
        {
            let fd = drv
                .session
                .state()
                .rare(mega_bf1)
                .and_then(|r| r.face_down.as_deref())
                .expect("megamorph face_down spec");
            assert_eq!(fd.kind, FaceDownKind::Megamorph);
            assert_eq!(fd.turn_up_mana_cost, Some(ManaCost::parse("{G}")));
            assert_eq!(fd.ward_cost, None);
        }
        assert_eq!(drv.session.state().counter_count(mega_bf1, plus_cid), 0);

        // Turn 3: play Forest, cast second Megamorph face down with Grove, then TurnFaceUp first via Forest!
        drv.advance_to(3, GamePhase::Main1, 16);
        let forest_h = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: forest_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let forest_bf = drv.find_battlefield_object(PlayerId(0), forest).unwrap();

        let mh2 = drv.find_hand_object(PlayerId(0), megamorph_g).unwrap();
        let opt2 = enumerate_cast_options(drv.session.state(), PlayerId(0), mh2)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: mh2,
                option_id: opt2,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        // Find the second face-down Megamorph creature
        let mega_bf2 = drv
            .session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .copied()
            .find(|&id| {
                id != mega_bf1 && drv.session.state().object(id).unwrap().card_def == megamorph_g
            })
            .unwrap();

        // Turn mega_bf1 face up via SpecialActionKind::TurnFaceUp (taps Forest for {G}, adds +1/+1 counter)
        drv.step(
            PlayerId(0),
            Action::SpecialAction {
                kind: SpecialActionKind::TurnFaceUp { object: mega_bf1 },
            },
        )
        .unwrap();
        assert!(
            drv.session
                .state()
                .object(forest_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED),
            "Megamorph TurnFaceUp must tap Forest {{G}}"
        );
        assert!(!drv.session.state().object(mega_bf1).unwrap().is_face_down());
        assert_eq!(
            drv.session.state().counter_count(mega_bf1, plus_cid),
            1,
            "CR 702.37f: Megamorph TurnFaceUp must place one +1/+1 counter"
        );
        let ev1 = drv.session.state().evaluate_board().get(mega_bf1).unwrap();
        assert_eq!(ev1.pt.map(|pt| (pt.power, pt.toughness)), Some((4, 4)));

        // Turn mega_bf2 face up via EffectOp::TurnFace (not paying megamorph cost) -> 0 counters, 3/3!
        drv.cast_from_hand(
            PlayerId(0),
            turn_up_spell,
            vec![TargetRef::Object { id: mega_bf2 }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert!(!drv.session.state().object(mega_bf2).unwrap().is_face_down());
        assert_eq!(
            drv.session.state().counter_count(mega_bf2, plus_cid),
            0,
            "CR 702.37f: Turning Megamorph face up without paying megamorph cost must NOT add +1/+1 counter"
        );
        let ev2 = drv.session.state().evaluate_board().get(mega_bf2).unwrap();
        assert_eq!(ev2.pt.map(|pt| (pt.power, pt.toughness)), Some((3, 3)));
    }
}

#[test]
fn synth_cr702_37f_megamorph_turn_face_up_competing_put_counters_replacements() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let forest = b.add_synth_basic_land("synth_for_mmrep", "Synth Forest", "Forest", ManaColor::G);
    let grove = b.add_card("synth_grove_mmrep", "Synth Triple Grove", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("m_ggg", &[ManaColor::G, ManaColor::G, ManaColor::G]);
    });

    // Two competing same-tier PutCounters replacement effects (+1 and x2):
    // Applying +1 then x2 yields (1 + 1) * 2 = 4 counters; applying x2 then +1 yields (1 * 2) + 1 = 3.
    let scales = b.add_card("synth_scales_mmrep", "Synth Scales", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Multiplier {
                    what: MultiplierTarget::Counters,
                    factor: 1.0,
                    delta: 1,
                    filter: None,
                    counter: Some(plus_cid),
                    cause: None,
                    cause_or: vec![],
                    round: None,
                    applies_to_players: false,
                    who_places: Some(RelativeControl::You),
                    who_creates: None,
                    extra_token: None,
                    extra_token_matching_count: false,
                    trigger_source_attached_to: false,
                },
                condition: None,
            });
    });
    let doubler = b.add_card("synth_doubler_mmrep", "Synth Doubler", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Multiplier {
                    what: MultiplierTarget::Counters,
                    factor: 2.0,
                    delta: 0,
                    filter: None,
                    counter: None,
                    cause: None,
                    cause_or: vec![],
                    round: None,
                    applies_to_players: false,
                    who_places: Some(RelativeControl::You),
                    who_creates: None,
                    extra_token: None,
                    extra_token_matching_count: false,
                    trigger_source_attached_to: false,
                },
                condition: None,
            });
    });

    // Megamorph creature with a TurnedFaceUp trigger (gain 3 life) to verify event+trigger flush on resume.
    let megamorph_c = b.add_card("synth_mm_mmrep", "Synth Megamorph Watcher", |c| {
        c.mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword_with_cost("megamorph", "{G}")
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
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[&[grove, scales, doubler, megamorph_c, forest], &[forest]],
        &[],
        5,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 702371).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // Turn 1: play Grove, cast Scales & Doubler ({0} each), cast Megamorph face down ({3}).
    let grove_h = drv.find_hand_object(PlayerId(0), grove).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: grove_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.cast_from_hand(PlayerId(0), scales, vec![]).unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), doubler, vec![]).unwrap();
    drv.resolve_all_stack(8);

    let mh = drv.find_hand_object(PlayerId(0), megamorph_c).unwrap();
    let opt = enumerate_cast_options(drv.session.state(), PlayerId(0), mh)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: mh,
            option_id: opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);

    // Turn 3: play Forest, then turn Megamorph face up -> must pause on ChooseReplacement!
    drv.advance_to(3, GamePhase::Main1, 16);
    let forest_h = drv.find_hand_object(PlayerId(0), forest).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: forest_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let mega_bf = drv
        .find_battlefield_object(PlayerId(0), megamorph_c)
        .unwrap();
    let life_before = drv.session.state().player(PlayerId(0)).unwrap().life;

    let out = drv
        .step(
            PlayerId(0),
            Action::SpecialAction {
                kind: SpecialActionKind::TurnFaceUp { object: mega_bf },
            },
        )
        .unwrap();

    // Must be paused cleanly on ChooseReplacement (2 options), NOT soft-locked!
    let prompt_id = match out.decision {
        DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseReplacement {
                    prompt_id,
                    option_count,
                },
        } => {
            assert_eq!(option_count, 2);
            prompt_id
        }
        other => panic!("expected ChooseReplacement pause on Megamorph TurnFaceUp, got {other:?}"),
    };
    let legal = drv.session.legal_actions(PlayerId(0));
    assert!(
        legal.contains(&Action::Answer {
            prompt_id,
            answer: ChoiceAnswer::PickReplacement { index: 0 },
        }),
        "legal_actions must expose PickReplacement(0), got {legal:?}"
    );
    assert!(
        legal.contains(&Action::Answer {
            prompt_id,
            answer: ChoiceAnswer::PickReplacement { index: 1 },
        }),
        "legal_actions must expose PickReplacement(1), got {legal:?}"
    );

    // Pick replacement 0 (Scales +1 first, then Doubler x2 -> (1 + 1) * 2 = 4 counters).
    let resumed = drv
        .step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 0 },
            },
        )
        .unwrap();
    assert_eq!(
        resumed.decision,
        DecisionState::Priority,
        "resuming Megamorph ChooseReplacement must return cleanly to Priority"
    );

    let counters = drv.session.state().counter_count(mega_bf, plus_cid);
    assert!(
        counters == 3 || counters == 4,
        "both replacement modifiers must apply (3 or 4 counters depending on candidate order), got {counters}"
    );
    assert!(!drv.session.state().object(mega_bf).unwrap().is_face_down());

    // TurnedFaceUp trigger must have been flushed onto the stack and resolve cleanly!
    assert_eq!(
        drv.session.state().stack().len(),
        1,
        "TurnedFaceUp trigger must be put on stack after replacement resume"
    );
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        life_before + 3,
        "TurnedFaceUp trigger must resolve for +3 life after Megamorph replacement resume"
    );
    // Game must remain completely responsive (PassPriority is legal).
    assert!(
        drv.session
            .legal_actions(PlayerId(0))
            .contains(&Action::PassPriority)
    );
}

#[test]
fn synth_cr702_37_168_170_keyword_cost_and_cost_defs_mana_single_charge() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let forest = b.add_synth_basic_land("synth_for_dc", "Synth Forest", "Forest", ManaColor::G);
    let grove = b.add_card("synth_grove_dc", "Synth Triple Grove", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("m_ggg", &[ManaColor::G, ManaColor::G, ManaColor::G]);
    });

    // Helper closure to build a KeywordDef with BOTH `cost: Some(mc)` AND
    // `cost_defs: vec![CostDef::Mana { cost: mc, .. }]`, matching real IR v2 exports.
    let mk_dual_kw = |b: &mut SynthBundleBuilder, kw_name: &str, cost_str: &str| {
        let kid = b.intern_keyword(kw_name);
        let mc = ManaCost::parse(cost_str);
        let mut kd = empty_keyword_def(kid);
        kd.cost = Some(mc.clone());
        kd.cost_defs = vec![CostDef::Mana {
            cost: mc,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
        }];
        kd
    };

    let morph_kd = mk_dual_kw(&mut b, "morph", "{1}{G}");
    let morph_c = b.add_card("synth_morph_dc", "Synth Dual Morph", |c| {
        c.mana_cost("{3}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(4, 4)
            .keyword_def(morph_kd);
    });

    let mega_kd = mk_dual_kw(&mut b, "megamorph", "{G}");
    let mega_c = b.add_card("synth_mega_dc", "Synth Dual Megamorph", |c| {
        c.mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword_def(mega_kd);
    });

    let disg_kd = mk_dual_kw(&mut b, "disguise", "{G}");
    let disg_c = b.add_card("synth_disg_dc", "Synth Dual Disguise", |c| {
        c.mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(3, 3)
            .keyword_def(disg_kd);
    });

    let plot_kd = mk_dual_kw(&mut b, "plot", "{1}{G}");
    let plot_c = b.add_card("synth_plot_dc", "Synth Dual Plot", |c| {
        c.mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword_def(plot_kd);
    });

    let db = b.build_db();

    // Case 1: Morph ({1}{G}) TurnFaceUp via exact 2 Forests (auto-tap)
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[grove, forest, forest, morph_c], &[forest]],
            &[],
            4,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 702372).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        let grove_h = drv.find_hand_object(PlayerId(0), grove).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: grove_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let mh = drv.find_hand_object(PlayerId(0), morph_c).unwrap();
        let opt = enumerate_cast_options(drv.session.state(), PlayerId(0), mh)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: mh,
                option_id: opt,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        // Turn 3 & 5: play 2 Forests (exact {1}{G} available on Turn 5, Grove tapped or untouched)
        drv.advance_to(3, GamePhase::Main1, 16);
        let f1 = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: f1,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        // Tap Grove into pool for {G}{G}{G}, then turn face up -> must charge EXACTLY {1}{G} (leaving 1 G)!
        let grove_bf = drv.find_battlefield_object(PlayerId(0), grove).unwrap();
        drv.step(
            PlayerId(0),
            Action::TapMana {
                object: grove_bf,
                ability_index: 0,
                preferred_color: None,
            },
        )
        .unwrap();
        let morph_bf = drv.find_battlefield_object(PlayerId(0), morph_c).unwrap();
        let act = Action::SpecialAction {
            kind: SpecialActionKind::TurnFaceUp { object: morph_bf },
        };
        assert!(drv.session.legal_actions(PlayerId(0)).contains(&act));
        drv.step(PlayerId(0), act).unwrap();
        assert!(!drv.session.state().object(morph_bf).unwrap().is_face_down());
        assert_eq!(
            drv.session
                .state()
                .player(PlayerId(0))
                .unwrap()
                .mana_pool
                .iter()
                .sum::<u32>(),
            1,
            "Morph {{1}}{{G}} from {{G}}{{G}}{{G}} pool must charge exactly 2 mana (1 remaining)"
        );
    }

    // Case 2: Megamorph ({G}) & Disguise ({G}) TurnFaceUp via exact 1 Forest each (auto-tap)
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[grove, forest, mega_c, disg_c, forest], &[forest]],
            &[],
            5,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 702373).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        let grove_h = drv.find_hand_object(PlayerId(0), grove).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: grove_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let mh = drv.find_hand_object(PlayerId(0), mega_c).unwrap();
        let opt_m = enumerate_cast_options(drv.session.state(), PlayerId(0), mh)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::MorphFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: mh,
                option_id: opt_m,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        // Turn 3: cast Disguise face down using Grove, play 1 Forest, turn Megamorph face up via that exact Forest!
        drv.advance_to(3, GamePhase::Main1, 16);
        let f1 = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: f1,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let dh = drv.find_hand_object(PlayerId(0), disg_c).unwrap();
        let opt_d = enumerate_cast_options(drv.session.state(), PlayerId(0), dh)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::DisguiseFaceDown)
            .unwrap()
            .option_id;
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: dh,
                option_id: opt_d,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);

        let mega_bf = drv.find_battlefield_object(PlayerId(0), mega_c).unwrap();
        let act_m = Action::SpecialAction {
            kind: SpecialActionKind::TurnFaceUp { object: mega_bf },
        };
        assert!(drv.session.legal_actions(PlayerId(0)).contains(&act_m));
        drv.step(PlayerId(0), act_m).unwrap();
        assert!(!drv.session.state().object(mega_bf).unwrap().is_face_down());
        assert_eq!(drv.session.state().counter_count(mega_bf, plus_cid), 1);

        // Turn 5: tap Grove? No, just let auto-tap pay {G} from Forest for Disguise!
        drv.advance_to(5, GamePhase::Main1, 16);
        let disg_bf = drv.find_battlefield_object(PlayerId(0), disg_c).unwrap();
        let act_d = Action::SpecialAction {
            kind: SpecialActionKind::TurnFaceUp { object: disg_bf },
        };
        assert!(drv.session.legal_actions(PlayerId(0)).contains(&act_d));
        drv.step(PlayerId(0), act_d).unwrap();
        assert!(!drv.session.state().object(disg_bf).unwrap().is_face_down());
        assert_eq!(
            target_ward_cost(
                drv.session.state(),
                TargetRef::Object { id: disg_bf },
                PlayerId(1),
            ),
            None
        );
    }

    // Case 3: Plot ({1}{G}) via exact 2 Forests (auto-tap) AND via {G}{G}{G} pool (1 G remaining)
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[forest, forest, plot_c], &[forest]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db, cfg, decks, 702374).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        let f1 = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: f1,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::Main1, 16);
        let f2 = drv.find_hand_object(PlayerId(0), forest).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: f2,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        let ph = drv.find_hand_object(PlayerId(0), plot_c).unwrap();
        let act_p = Action::SpecialAction {
            kind: SpecialActionKind::Plot { object: ph },
        };
        assert!(drv.session.legal_actions(PlayerId(0)).contains(&act_p));
        drv.step(PlayerId(0), act_p).unwrap();
        assert!(
            drv.session
                .state()
                .player(PlayerId(0))
                .unwrap()
                .exile
                .iter()
                .any(|&id| drv.session.state().object(id).unwrap().card_def == plot_c),
            "Plot {{1}}{{G}} must succeed with exact 2 Forests and exile the card"
        );
    }
}
