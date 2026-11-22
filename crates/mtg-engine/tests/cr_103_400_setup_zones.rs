//! CR 103 (Setup / Mulligans), CR 903 (Commander), CR 400.7 (Zone Funnel & LKI),
//! CR 608.2n (End the Turn), and CR 709 / 715 (Split/Fuse & Adventure) integration tests.

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_spell,
};
use mtg_engine::{
    Action, AltCastKind, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig,
    MacroAction, PlayerId, TargetRef, enumerate_cast_options,
};
use mtg_ir::{
    Amount, CardLayout, CardTypeMask, ColorMask, EffectOp, EffectStep, EventDetail, EventFilter,
    EventKind, EventPattern, GamePhase, ManaColor, ObjectFilter, PlayerRef, ReplacementDef,
    ReplacementWith, Selector, StaticBody, StaticDef, TriggerSubject, Zone, ZoneChangeMod,
    ZoneMask,
};

fn blink_effect_steps() -> Vec<EffectStep> {
    let mut step1 = effect_step(
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
    );
    step1.bind_as = Some("blinked".to_string());

    let step2 = effect_step(
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
        Selector::Binding {
            name: "blinked".to_string(),
        },
        None,
    );
    vec![step1, step2]
}

#[test]
fn synth_cr103_london_mulligan_and_pregame_battlefield() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_103", "Synth Plains", "Plains", ManaColor::W);
    let leyline = b.add_card("synth_leyline_103", "Synth Leyline", |c| {
        c.mana_cost("{2}{W}{W}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::PregameBeginOnBattlefield,
                condition: None,
            });
    });
    let watcher = b.add_card("synth_cast_watcher_103", "Synth CastWatcher", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::Cast,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter::default()),
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
                    Some(Amount::Const { n: 5 }),
                )],
            );
    });
    let cmd_id = b.add_synth_commander("synth_cmd_103", "Synth Cmd", "{0}", 3, 3);
    let db = b.build_db();

    let mut main = vec![leyline; 20];
    main.extend(std::iter::repeat_n(watcher, 20));
    main.extend(std::iter::repeat_n(plains, 59));
    let deck = DeckSpec {
        commanders: vec![cmd_id],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![deck.clone(), deck.clone(), deck.clone(), deck],
        1031,
    )
    .unwrap();

    // P0 takes 1st mulligan (free in Commander) and keeps -> 0 bottomed.
    drv.step(PlayerId(0), Action::TakeMulligan).unwrap();
    drv.step(PlayerId(0), Action::KeepHand).unwrap();
    assert_eq!(
        drv.session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .mulligan_count,
        1
    );
    assert_eq!(
        drv.session.state().mulligan_bottom_remaining(PlayerId(0)),
        0
    );
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().hand.len(),
        7
    );

    // P1 takes 2 mulligans -> must bottom 1 card after all seats keep (CR 103.5).
    drv.step(PlayerId(1), Action::TakeMulligan).unwrap();
    drv.step(PlayerId(1), Action::TakeMulligan).unwrap();
    drv.step(PlayerId(1), Action::KeepHand).unwrap();
    assert_eq!(
        drv.session
            .state()
            .player(PlayerId(1))
            .unwrap()
            .mulligan_count,
        2
    );
    assert_eq!(
        drv.session.state().mulligan_bottom_remaining(PlayerId(1)),
        1
    );

    // P2 and P3 keep opening 7 before Stage 2 bottoming returns to P1.
    drv.step(PlayerId(2), Action::KeepHand).unwrap();
    drv.step(PlayerId(3), Action::KeepHand).unwrap();
    assert_eq!(drv.session.current_actor(), Some(PlayerId(1)));

    let bottom_pick = drv.session.state().player(PlayerId(1)).unwrap().hand[0];
    let bottom_def = drv.session.state().object(bottom_pick).unwrap().card_def;
    drv.step(
        PlayerId(1),
        Action::BottomCard {
            object: bottom_pick,
        },
    )
    .unwrap();
    let p1_leylines_on_bf = drv
        .session
        .state()
        .player(PlayerId(1))
        .unwrap()
        .battlefield
        .iter()
        .filter(|&&id| drv.session.state().object(id).unwrap().card_def == leyline)
        .count();
    assert_eq!(
        drv.session.state().player(PlayerId(1)).unwrap().hand.len() + p1_leylines_on_bf,
        6
    );
    let p1_lib = &drv.session.state().player(PlayerId(1)).unwrap().library;
    assert_eq!(
        drv.session.state().object(p1_lib[0]).unwrap().card_def,
        bottom_def,
        "London mulligan bottomed card must land at library[0] (bottom)"
    );

    // PregameBeginOnBattlefield places Synth Leyline onto battlefield without cast triggers.
    let total_leylines_on_bf: usize = drv
        .session
        .state()
        .players()
        .iter()
        .flat_map(|p| p.battlefield.iter())
        .filter(|&&id| drv.session.state().object(id).unwrap().card_def == leyline)
        .count();
    assert!(
        total_leylines_on_bf >= 1,
        "at least one Synth Leyline should begin on the battlefield pregame"
    );
    for p in drv.session.state().players() {
        assert_eq!(p.life, 40);
    }
}

#[test]
fn synth_cr903_commander_tax_sba_vs_hand_library_replacement() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_903", "Synth Plains", "Plains", ManaColor::W);
    let general = b.add_synth_commander("synth_general_903", "Synth General", "{0}", 3, 3);
    let dies_watcher = b.add_card("synth_dies_watcher_903", "Synth DiesWatcher", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let destroy_spell = b.add_card("synth_shatter_903", "Synth Shatter", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
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
    let bounce_spell = b.add_card("synth_gust_903", "Synth Gust", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
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
    let grave_exile = b.add_card("synth_grave_exile_903", "Synth GraveExile", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
                            to: Some(Zone::Graveyard),
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
                        to: Some(Zone::Exile),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });

    let db = b.build_db();
    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                dies_watcher,
                destroy_spell,
                destroy_spell,
                bounce_spell,
                grave_exile,
                plains,
                plains,
            ],
            &[plains],
        ],
        &[vec![general], vec![general]],
        3,
        GameConfig::commander(2),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 9031).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // 1) Cast DiesWatcher, play 2 Plains? Wait: 1 land/turn! We only need mana on later turns or
    // we can verify commander tax on turn 1 -> turn 3 after playing a second Plains.
    drv.cast_from_hand(PlayerId(0), dies_watcher, vec![])
        .unwrap();
    drv.resolve_all_stack(8);

    let cmd_id = drv.find_command_object(PlayerId(0), general).unwrap();
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
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .commander_cast_count(cmd_id.index),
        1
    );

    let bf_cmd = drv.find_battlefield_object(PlayerId(0), general).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        destroy_spell,
        vec![TargetRef::Object { id: bf_cmd }],
    )
    .unwrap();
    // Resolve destroy_spell -> commander dies -> SBA prompts CommanderReturn (YesNo) -> DiesWatcher triggers!
    drv.pass_priority();
    drv.pass_priority();

    if let DecisionState::Prompt {
        request: ChoiceRequest::YesNo { prompt_id, .. },
    } = drv.decision()
    {
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::YesNo { value: true },
            },
        )
        .unwrap();
    } else {
        panic!("expected CR 903.9a CommanderReturn prompt after commander died");
    }
    drv.resolve_all_stack(8);
    // DiesWatcher trigger fired on death and gained 2 life -> 42.
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 42);
    assert!(
        drv.find_command_object(PlayerId(0), general).is_some(),
        "commander must return to Command zone"
    );

    // Play Plains on Turn 1, advance to Turn 3 Main1, play 2nd Plains for {2} commander tax!
    let land1 = drv.find_hand_object(PlayerId(0), plains).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.advance_to(3, GamePhase::Main1, 32);
    let land2 = drv.find_hand_object(PlayerId(0), plains).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land2,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();

    // Casting Synth General from Command Zone without mana fails due to +2 commander tax;
    // with 2 Plains auto-tapped it succeeds!
    let cmd_in_cz = drv.find_command_object(PlayerId(0), general).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cmd_in_cz,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .commander_cast_count(cmd_in_cz.index),
        2
    );

    // 2) Under Synth GraveExile (exiles instead of graveyard), CR 903.9a SBA still offers Command Zone!
    drv.cast_from_hand(PlayerId(0), grave_exile, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    let bf_cmd2 = drv.find_battlefield_object(PlayerId(0), general).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        destroy_spell,
        vec![TargetRef::Object { id: bf_cmd2 }],
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    if let DecisionState::Prompt {
        request: ChoiceRequest::YesNo { prompt_id, .. },
    } = drv.decision()
    {
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::YesNo { value: true },
            },
        )
        .unwrap();
    } else {
        panic!("expected CR 903.9a CommanderReturn prompt from Exile");
    }
    drv.resolve_all_stack(8);
    assert!(drv.find_command_object(PlayerId(0), general).is_some());
    // Graveyard dies trigger did NOT fire under GraveExile (life remains 42).
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 42);

    // 3) Advance to Turn 7 with 4 Plains? Even simpler: verify bounce replacement CR 903.9b on P1's commander
    // or test CR 903.9b by using P1's commander if cast, or tapping 2 Plains + wait: we have 2 Plains,
    // let's check bounce on P1's {0} commander! First let P1 cast their commander on Turn 4.
    drv.advance_to(4, GamePhase::Main1, 32);
    let p1_cmd = drv.find_command_object(PlayerId(1), general).unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: p1_cmd,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);

    drv.advance_to(5, GamePhase::Main1, 32);
    let p1_bf = drv.find_battlefield_object(PlayerId(1), general).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        bounce_spell,
        vec![TargetRef::Object { id: p1_bf }],
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // CR 903.9b replacement prompts P1 immediately before entering hand!
    if let DecisionState::Prompt {
        request: ChoiceRequest::YesNo { prompt_id, .. },
    } = drv.decision()
    {
        assert_eq!(drv.session.current_actor(), Some(PlayerId(1)));
        drv.step(
            PlayerId(1),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::YesNo { value: true },
            },
        )
        .unwrap();
    } else {
        panic!("expected CR 903.9b bounce-to-command replacement prompt");
    }
    assert!(
        drv.find_command_object(PlayerId(1), general).is_some(),
        "CR 903.9b must redirect bounce directly to Command zone without hitting hand"
    );
    assert!(drv.find_hand_object(PlayerId(1), general).is_none());
}

#[test]
fn synth_cr400_7_generation_bumps_and_stack_permanent_exception() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_400", "Synth Bear", "{0}", 2, 2);
    let blink = b.add_card("synth_blink_400", "Synth Blink", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(vec![target_creature()], blink_effect_steps());
    });
    let plains = b.add_synth_basic_land("synth_plains_400", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![bear, blink, plains, plains, plains, plains, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 4007).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let hand_bear = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    let stack_bear = *drv.session.state().stack().last().unwrap();
    assert!(
        stack_bear.generation > hand_bear.generation,
        "CR 400.7: Hand -> Stack must bump generation"
    );
    assert!(drv.session.state().object(hand_bear).is_none());

    drv.resolve_all_stack(8);
    let bf_bear = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert_eq!(
        bf_bear, stack_bear,
        "CR 400.7d / 107.3m: Stack -> Battlefield permanent resolution preserves ObjectId"
    );

    drv.cast_from_hand(PlayerId(0), blink, vec![TargetRef::Object { id: bf_bear }])
        .unwrap();
    drv.resolve_all_stack(8);

    let blinked_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert!(
        blinked_bf.generation > bf_bear.generation,
        "CR 400.7: Battlefield -> Exile -> Battlefield must bump generation"
    );
    assert!(
        drv.session.state().object(bf_bear).is_none(),
        "stale pre-blink ObjectId must be invalidated"
    );
}

#[test]
fn synth_cr400_7_blink_target_fizzle_no_secondary_draw() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_fizz", "Synth Bear", "{0}", 2, 2);
    let bolt_draw = b.add_card("synth_boltdraw_fizz", "Synth BoltDraw", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![
                    effect_step(
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
                        Some(Amount::Const { n: 3 }),
                    ),
                    effect_step(
                        EffectOp::Draw {
                            up_to: false,
                            reveal_drawn: false,
                        },
                        Selector::PlayerRef {
                            player: PlayerRef::You,
                        },
                        Some(Amount::Const { n: 1 }),
                    ),
                ],
            );
    });
    let blink = b.add_card("synth_blink_fizz", "Synth Blink", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(vec![target_creature()], blink_effect_steps());
    });
    let plains =
        b.add_synth_basic_land("synth_plains_fizz", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![bear, bolt_draw, blink, plains, plains, plains, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 4008).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let bear_id = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    // Cast BoltDraw targeting Synth Bear, then respond with Synth Blink on Synth Bear.
    drv.cast_from_hand(
        PlayerId(0),
        bolt_draw,
        vec![TargetRef::Object { id: bear_id }],
    )
    .unwrap();
    let hand_before_fizzle = drv.session.state().player(PlayerId(0)).unwrap().hand.len();

    drv.cast_from_hand(PlayerId(0), blink, vec![TargetRef::Object { id: bear_id }])
        .unwrap();
    let hand_after_blink_cast = hand_before_fizzle - 1;
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().hand.len(),
        hand_after_blink_cast
    );

    drv.resolve_all_stack(16);

    // Blink resolved first -> Bear returned with new generation -> BoltDraw fizzled (CR 608.2b):
    // Bear is still alive on battlefield AND controller did NOT draw a card!
    let new_bear = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert!(new_bear.generation > bear_id.generation);
    assert_eq!(
        drv.session.state().object(new_bear).unwrap().damage_marked,
        0
    );
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().hand.len(),
        hand_after_blink_cast,
        "CR 608.2b: fizzled BoltDraw must NOT execute secondary Draw step"
    );
}

#[test]
fn synth_cr608_2n_end_the_turn_exiles_resolving_spell_from_stack() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_608n", "Synth Bear", "{0}", 2, 2);
    let time_stop = b.add_card("synth_timestop_608n", "Synth TimeStop", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::EndTurn {
                        active_player_may: false,
                    },
                    Selector::None,
                    None,
                )],
            );
    });
    let plains =
        b.add_synth_basic_land("synth_plains_608n", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![bear, time_stop, plains, plains, plains, plains, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6082).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // Put Synth Bear on stack, then cast Synth TimeStop on top of it.
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.cast_from_hand(PlayerId(0), time_stop, vec![]).unwrap();
    assert_eq!(drv.session.state().stack().len(), 2);

    // Resolve top of stack (Synth TimeStop).
    drv.pass_priority();
    drv.pass_priority();

    // CR 608.2n / 725.1b: Stack is empty, BOTH Synth TimeStop and Synth Bear are in Exile (not Graveyard),
    // and the turn advanced to Turn 2.
    assert!(drv.session.state().stack().is_empty());
    assert!(
        drv.find_exile_object(PlayerId(0), time_stop).is_some(),
        "resolving TimeStop must be exiled from stack, not put into graveyard"
    );
    assert!(
        drv.find_graveyard_object(PlayerId(0), time_stop).is_none(),
        "resolving TimeStop must NOT go to graveyard"
    );
    assert!(
        drv.find_exile_object(PlayerId(0), bear).is_some(),
        "other spell on stack must be exiled"
    );
    assert_eq!(drv.session.state().turn_number(), 2);
}

#[test]
fn synth_cr709_715_split_fuse_and_adventure_zone_characteristics() {
    let mut b = SynthBundleBuilder::new();
    let fuse_card = b.add_card("synth_fuse_709", "Synth Blaze // Synth Frost", |c| {
        c.layout(CardLayout::Split)
            .mana_cost("{1}{R}")
            .types(CardTypeMask::INSTANT)
            .keyword("fuse")
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
            )
            .add_face("Synth Frost", |f| {
                f.mana_cost("{2}{U}")
                    .types(CardTypeMask::INSTANT)
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
                    );
            });
    });
    let quest_knight = b.add_card("synth_quest_715", "Synth QuestKnight", |c| {
        c.layout(CardLayout::Adventure)
            .mana_cost("{2}{G}")
            .types(CardTypeMask::CREATURE)
            .subtype("Knight")
            .pt(3, 3)
            .add_face("Synth Quest", |f| {
                f.mana_cost("{G}")
                    .types(CardTypeMask::INSTANT)
                    .subtype("Adventure")
                    .spell_effect(
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
    });
    let counterspell = b.add_card("synth_cancel_715", "Synth Cancel", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_spell()],
                vec![effect_step(
                    EffectOp::CounterSpell {
                        replacement_zone: None,
                        unless_pays_generic: None,
                        on_countered_steal_filter: None,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let mountain =
        b.add_synth_basic_land("synth_mtn_709", "Synth Mountain", "Mountain", ManaColor::R);
    let island = b.add_synth_basic_land("synth_isl_709", "Synth Island", "Island", ManaColor::U);
    let forest = b.add_synth_basic_land("synth_for_709", "Synth Forest", "Forest", ManaColor::G);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                fuse_card,
                quest_knight,
                quest_knight,
                counterspell,
                mountain,
                island,
                forest,
            ],
            &[forest],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 7094).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // Check off-stack characteristics in hand:
    // 1) SplitFuse has combined MV = 2 + 3 = 5 and union of colors R | U.
    let fuse_hand = drv.find_hand_object(PlayerId(0), fuse_card).unwrap();
    let vm_hand = drv.session.view_model(Some(PlayerId(0)));
    let fuse_view = vm_hand.players[0]
        .hand
        .iter()
        .find(|o| o.id == fuse_hand)
        .unwrap();
    assert_eq!(fuse_view.mana_value, 5);
    assert_eq!(fuse_view.colors, ColorMask::R | ColorMask::U);

    // 2) Adventure card off-stack only has primary creature characteristics (MV 3, G, Creature).
    let adv_hand = drv.find_hand_object(PlayerId(0), quest_knight).unwrap();
    let adv_view = vm_hand.players[0]
        .hand
        .iter()
        .find(|o| o.id == adv_hand)
        .unwrap();
    assert_eq!(adv_view.mana_value, 3);
    assert!(adv_view.types.contains(CardTypeMask::CREATURE));
    assert!(!adv_view.types.contains(CardTypeMask::INSTANT));

    // Play Forest on Turn 1, cast Adventure instant half (Synth Quest {G}).
    let for1 = drv.find_hand_object(PlayerId(0), forest).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: for1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let adv_opts = enumerate_cast_options(drv.session.state(), PlayerId(0), adv_hand);
    let adv_half_opt = adv_opts
        .iter()
        .find(|o| o.alt_kind == AltCastKind::AdventureHalf)
        .expect("AdventureHalf cast option must exist")
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: adv_hand,
            option_id: adv_half_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);
    let adv_exile = drv
        .find_exile_object(PlayerId(0), quest_knight)
        .expect("resolved Adventure must go on an adventure in Exile");
    assert!(
        drv.session
            .state()
            .rare(adv_exile)
            .is_some_and(|r| r.on_an_adventure)
    );

    // Second copy of QuestKnight: cast Adventure half and COUNTER it -> goes to Graveyard (not Exile).
    // Play Mountain on Turn 3, Island on Turn 5, etc., or wait: tap Forest on Turn 3!
    drv.advance_to(3, GamePhase::Main1, 32);
    let adv2_hand = drv.find_hand_object(PlayerId(0), quest_knight).unwrap();
    let adv2_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), adv2_hand)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::AdventureHalf)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: adv2_hand,
            option_id: adv2_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    let stack_adv2 = *drv.session.state().stack().last().unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        counterspell,
        vec![TargetRef::Object { id: stack_adv2 }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert!(
        drv.find_graveyard_object(PlayerId(0), quest_knight)
            .is_some(),
        "countered Adventure must go to Graveyard without on_an_adventure"
    );

    // Play Mountain + Island across turns 3 and 5 so we can cast Fuse from hand ({1}{R} + {2}{U} = {3}{U}{R})
    // Wait: we only have 3 lands (Forest, Mountain, Island) -> let's test Split left half ({1}{R}, MV 2 on stack)
    // and check Fuse cast option exists when enough mana is available!
    let mtn1 = drv.find_hand_object(PlayerId(0), mountain).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: mtn1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();

    // Advance to Turn 5 so Forest + Mountain untap, play Island, then cast Split left half ({1}{R}, MV 2 on stack).
    drv.advance_to(5, GamePhase::Main1, 64);
    let isl1 = drv.find_hand_object(PlayerId(0), island).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: isl1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let fuse_opts = enumerate_cast_options(drv.session.state(), PlayerId(0), fuse_hand);
    let left_opt = fuse_opts
        .iter()
        .find(|o| o.alt_kind == AltCastKind::Normal)
        .unwrap()
        .option_id;
    assert!(
        fuse_opts.iter().any(|o| o.alt_kind == AltCastKind::Fuse),
        "Fuse option must be enumerated from Hand"
    );
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: fuse_hand,
            option_id: left_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    let stack_fuse = *drv.session.state().stack().last().unwrap();
    assert_eq!(
        drv.session
            .state()
            .evaluate_board()
            .get(stack_fuse)
            .unwrap()
            .mana_value,
        2,
        "CR 709.4b: non-fused left half on stack has MV 2"
    );
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 25);

    // Advance to Turn 7 Main1 so all 3 lands untap, then cast QuestKnight creature half from Exile!
    drv.advance_to(7, GamePhase::Main1, 64);
    let creature_exile_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), adv_exile)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::Normal && o.face_idx == 0)
        .expect("on_an_adventure creature half must be castable from Exile")
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: adv_exile,
            option_id: creature_exile_opt,
            from_zone: Zone::Exile,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert!(
        drv.find_battlefield_object(PlayerId(0), quest_knight)
            .is_some(),
        "QuestKnight must resolve onto battlefield from Exile"
    );
}

#[test]
fn synth_cr307_4_mdfc_sorcery_front_land_back_enters_battlefield() {
    use mtg_engine::EngineError;
    use mtg_ir::{
        AbilityDef, ActivatedAbility, CostDef, ManaProduction, Span, TargetKind, TargetSpec, Timing,
    };

    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_3074", "Synth Plains", "Plains", ManaColor::W);
    let mdfc = b.add_card("synth_mdfc_3074", "Synth Flare // Synth Crag", |c| {
        c.layout(CardLayout::ModalDfc)
            .mana_cost("{R}")
            .types(CardTypeMask::SORCERY)
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
            )
            .add_face("Synth Crag", |f| {
                f.types(CardTypeMask::LAND);
                f.draft
                    .abilities
                    .push(AbilityDef::Activated(ActivatedAbility {
                        id: "mdfc_mana_r".to_string(),
                        cost: vec![CostDef::TapSelf],
                        targets: vec![],
                        effect: vec![effect_step(
                            EffectOp::AddMana {
                                production: ManaProduction::Fixed {
                                    symbols: smallvec::smallvec![ManaColor::R],
                                },
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                                delayed_rider: None,
                            },
                            Selector::None,
                            None,
                        )],
                        modal: None,
                        timing: Timing::Mana,
                        is_mana: true,
                        activation_restrictions: vec![],
                        activation_condition: None,
                        activator_who: None,
                        power_up_reduction: false,
                        x_cant_be_zero: false,
                        activation_cost_reduction: None,
                        activation_zones: ZoneMask::BATTLEFIELD,
                        cost_keyword: None,
                        intrinsic: true,
                        text: None,
                        grant_condition: None,
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        span: Span::default(),
                    }));
            });
    });

    // Spell attempting to move any card from Graveyard to Battlefield (CR 307.4 must block Sorcery-front MDFC).
    let reanimate_any = b.add_card("synth_reanim_3074", "Synth Reclaim", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::GRAVEYARD,
                        ..Default::default()
                    },
                    player: None,
                    min: 1,
                    max: 1,
                    same_owner: false,
                    condition: None,
                    other_target: false,
                    divide_total: None,
                    with_single_target: false,
                    dynamic_count: None,
                    dynamic_up_to: false,
                    max_total_mana_value: None,
                    also_spells: false,
                    per_opponent: false,
                    different_controllers: false,
                    chosen_by_event_player: false,
                    shares_card_type_with_target: None,
                    zones: ZoneMask::empty(),
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
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[&[mdfc, mdfc, reanimate_any], &[plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 3074).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let mdfc_hand = drv.find_hand_object(PlayerId(0), mdfc).unwrap();
    // Playing face 0 (Sorcery) as a land must be rejected.
    let err = drv
        .step(
            PlayerId(0),
            Action::PlayLand {
                object: mdfc_hand,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::IllegalAction(_)));

    // Playing face 1 (Land) must move the MDFC onto the battlefield with Land types.
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: mdfc_hand,
            from_zone: Zone::Hand,
            face_index: 1,
        },
    )
    .unwrap();
    let mdfc_bf = drv
        .find_battlefield_object(PlayerId(0), mdfc)
        .expect("MDFC back-face Land must enter battlefield");
    assert!(drv.find_hand_object(PlayerId(0), mdfc).is_some()); // second copy still in hand
    assert_eq!(
        drv.session
            .state()
            .object(mdfc_bf)
            .unwrap()
            .active_face_index(),
        1
    );
    let bf_eval = drv.session.state().evaluate_board().get(mdfc_bf).unwrap();
    assert!(bf_eval.types.contains(CardTypeMask::LAND));
    assert!(!bf_eval.types.contains(CardTypeMask::SORCERY));

    // Tap the MDFC land for {R} to cast the second copy's face-0 Sorcery half.
    let mdfc_hand2 = drv.find_hand_object(PlayerId(0), mdfc).unwrap();
    drv.cast_from_hand(PlayerId(0), mdfc, vec![]).unwrap();
    drv.resolve_all_stack(8);
    assert!(drv.session.state().object(mdfc_hand2).is_none());
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 22);

    // The cast copy is in Graveyard with face 0 (Sorcery) active; CR 307.4 must keep it in Graveyard.
    let mdfc_gy = drv
        .find_graveyard_object(PlayerId(0), mdfc)
        .expect("resolved MDFC sorcery must be in graveyard");
    drv.cast_from_hand(
        PlayerId(0),
        reanimate_any,
        vec![TargetRef::Object { id: mdfc_gy }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert!(
        drv.find_graveyard_object(PlayerId(0), mdfc).is_some(),
        "CR 307.4: Sorcery-front MDFC must not enter battlefield from graveyard"
    );
    // Only the original MDFC land copy is on the battlefield.
    let bf_count = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .battlefield
        .iter()
        .filter(|&&id| {
            drv.session
                .state()
                .object(id)
                .is_some_and(|o| o.card_def == mdfc)
        })
        .count();
    assert_eq!(bf_count, 1);
}
