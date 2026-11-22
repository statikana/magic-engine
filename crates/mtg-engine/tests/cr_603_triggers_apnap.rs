//! CR 603 (Triggers, APNAP Stacking, Pre-Batch LKI, Intervening-If, Delayed, Reflexive, State)
//! and CR 605 (Triggered Mana Abilities & Trigger Multipliers) integration tests.

mod common;

use std::sync::Arc;

use common::{
    SessionDriver, SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
};
use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig, PlayerId, TargetRef,
};
use mtg_ir::{
    Affected, Amount, CardTypeMask, Compare, CompareOp, Condition, DelayedTriggerPayload, EffectOp,
    EventDetail, EventFilter, EventKind, EventPattern, GamePhase, Layer, LayerMod, ManaColor,
    ManaProduction, MultiplierTarget, ObjectFilter, PlayerRef, ReflexiveTriggerSpec,
    RelativeControl, Selector, StaticBody, StaticDef, TriggerDef, TriggerSubject, Zone, ZoneMask,
};
use smallvec::SmallVec;

#[test]
fn synth_cr603_3b_4p_apnap_stacking_and_same_player_order_prompt() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6033", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_6033", "Synth Bear", "{0}", 2, 2);

    // Triggered enchantments that react to any creature entering the battlefield:
    // P0 has 2 different enchantments (so P0 gets OrderObjects prompt), P1 has 1, P2 concedes, P3 has 1.
    let p0_trig_a = b.add_card("synth_t0a_6033", "Synth WatchA", |c| {
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
                            from: ZoneMask::empty(),
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
    let p0_trig_b = b.add_card("synth_t0b_6033", "Synth WatchB", |c| {
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
                            from: ZoneMask::empty(),
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
    // P1 & P3 watcher: deals 1 damage to P0 (so we can observe exact LIFO resolution order: P3 -> P1 -> P0!)
    let opp_ping_watch = b.add_card("synth_oppw_6033", "Synth OppWatch", |c| {
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
                            from: ZoneMask::empty(),
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
                vec![],
                vec![effect_step(
                    EffectOp::LoseLife,
                    Selector::PlayerRef {
                        player: PlayerRef::ActivePlayer,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let cmd = b.add_synth_commander("synth_cmd_6033", "Synth Cmd", "{0}", 3, 3);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[p0_trig_a, p0_trig_b, bear],
            &[opp_ping_watch],
            &[plains],
            &[opp_ping_watch],
        ],
        &[vec![cmd], vec![cmd], vec![cmd], vec![cmd]],
        3,
        GameConfig::commander(4),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 6033).unwrap();
    drv.keep_all_hands();

    // Turn 1: P0 casts WatchA & WatchB; Turn 2: P1 casts OppWatch; Turn 3: P2 concedes; Turn 4: P3 casts OppWatch.
    drv.advance_to(1, GamePhase::Main1, 16);
    drv.cast_from_hand(PlayerId(0), p0_trig_a, vec![]).unwrap();
    drv.resolve_all_stack(16);
    drv.cast_from_hand(PlayerId(0), p0_trig_b, vec![]).unwrap();
    drv.resolve_all_stack(16);

    drv.advance_to(2, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(1), opp_ping_watch, vec![])
        .unwrap();
    drv.resolve_all_stack(16);

    drv.advance_to(3, GamePhase::Main1, 32);
    drv.step(PlayerId(2), Action::Concede).unwrap();
    assert!(drv.session.state().player(PlayerId(2)).unwrap().lost);

    drv.advance_to(4, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(3), opp_ping_watch, vec![])
        .unwrap();
    drv.resolve_all_stack(16);

    // Advance back to P0 Turn 5 Main1 -> P0 casts Bear!
    drv.advance_to(5, GamePhase::Main1, 48);
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    // Resolve Bear (all 4 alive players pass? P2 is eliminated, so P0, P1, P3 pass).
    for _ in 0..3 {
        drv.pass_priority();
    }

    // Now Bear entered -> P0 has 2 simultaneous triggers -> OrderObjects prompt for P0!
    if let DecisionState::Prompt {
        request: ChoiceRequest::OrderObjects { prompt_id, objects },
    } = drv.decision()
    {
        assert_eq!(objects.len(), 2);
        assert_eq!(drv.session.current_actor(), Some(PlayerId(0)));
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::OrderNext { id: objects[0] },
            },
        )
        .unwrap();
    } else {
        panic!("expected CR 603.3b OrderObjects prompt for P0's 2 simultaneous triggers");
    }

    // Stack should have 4 triggers: bottom [P0, P0], middle [P1], top [P3].
    let stack = drv.session.state().stack_entries();
    assert_eq!(stack.len(), 4);
    assert_eq!(stack[0].controller, PlayerId(0));
    assert_eq!(stack[1].controller, PlayerId(0));
    assert_eq!(stack[2].controller, PlayerId(1));
    assert_eq!(stack[3].controller, PlayerId(3));

    // Resolve top 1 (P3) -> P0 loses 1 life (40 -> 39).
    for _ in 0..3 {
        drv.pass_priority();
    }
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 39);
    // Resolve next (P1) -> P0 loses 1 life (39 -> 38).
    for _ in 0..3 {
        drv.pass_priority();
    }
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 38);
    // Resolve remaining 2 (P0) -> P0 gains 2 + 3 = 5 life -> 43!
    drv.resolve_all_stack(16);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 43);
}

#[test]
fn synth_cr603_10a_simultaneous_wrath_pre_batch_lki() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_60310", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_60310", "Synth Bear", "{0}", 2, 2);

    // Synth DrainWatcher: whenever a creature dies, gain 1 life (if dying creature had power >= 3, gain 10!)
    let drain_watcher = b.add_card("synth_dwatch_60310", "Synth DrainWatcher", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .triggered_ability_if(
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
                Condition::EventObjectMatchedLki {
                    filter: Box::new(ObjectFilter {
                        power: Some(Compare {
                            op: CompareOp::Ge,
                            value: Box::new(Amount::Const { n: 3 }),
                        }),
                        ..Default::default()
                    }),
                },
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

    // Synth Lord: +1/+1 to other creatures you control (so Bears are 3/3 before wrath!)
    let lord = b.add_card("synth_lord_60310", "Synth Lord", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            extra: vec![mtg_ir::ObjectPredicate::NotSelf],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 1 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });

    // Simultaneous DestroyAll wrath
    let wrath = b.add_card("synth_wrath_60310", "Synth Wrath", |c| {
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

    // 7-card deck: DrainWatcher + Lord + 3 Bears + Wrath + Plains
    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![drain_watcher, lord, bear, bear, bear, wrath, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![p0_deck, p1_deck],
        60310,
    )
    .unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), drain_watcher, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), lord, vec![]).unwrap();
    drv.resolve_all_stack(8);
    for _ in 0..3 {
        drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    // Cast Wrath -> wipes Artist (2/2 w/ Lord), Lord (2/2), and 3 Bears (3/3 w/ Lord)!
    // CR 603.10a: Artist sees all 3 Bears as 3/3 in pre-batch LKI even though Lord died in the same batch!
    drv.cast_from_hand(PlayerId(0), wrath, vec![]).unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // Answer trigger ordering if 3 triggers queued
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
    assert_eq!(
        drv.session.state().stack().len(),
        3,
        "all 3 Bears must match power >= 3 via pre-batch LKI before Lord died"
    );
    drv.resolve_all_stack(16);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);
}

#[test]
fn synth_cr603_4_intervening_if_three_cases_and_once_per_turn() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6034", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_6034", "Synth Bear", "{0}", 2, 2);
    let artifact = b.add_card("synth_art_6034", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let shatter_art = b.add_card("synth_shart_6034", "Synth Smelt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![mtg_ir::TargetSpec {
                    kind: mtg_ir::TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ARTIFACT,
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
                    EffectOp::Destroy {
                        cant_be_regenerated: false,
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });

    // Trigger: "Whenever a creature enters, if you control an artifact, gain 5 life. Triggers only once each turn."
    let mut trig = simple_trigger(
        EventKind::ZoneChange,
        EventFilter {
            object: Some(TriggerSubject::Filter {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
            }),
            detail: Some(EventDetail::ZoneChange {
                from: ZoneMask::empty(),
                to: Some(Zone::Battlefield),
                from_anywhere: false,
                not_from_zone: None,
                not_to_zone: None,
                enters_untapped: false,
                dealt_damage_by_self_this_turn: false,
            }),
            ..Default::default()
        },
    );
    trig.only_once_per_turn = true;

    let watch = b.add_card("synth_watch_6034", "Synth Watch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability_if(
                trig,
                Condition::ControlsFilter {
                    filter: Box::new(ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ARTIFACT,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    }),
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
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![watch, bear, bear, bear, artifact, shatter_art, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6034).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), watch, vec![]).unwrap();
    drv.resolve_all_stack(8);

    // Case (a): False at event time (no artifact) -> never triggers AND does NOT consume once_per_turn!
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 20);

    // Case (b): Cast Artifact, cast 2nd Bear -> triggers onto stack; destroy Artifact in response -> fizzles silently!
    drv.cast_from_hand(PlayerId(0), artifact, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let art_bf = drv.find_battlefield_object(PlayerId(0), artifact).unwrap();

    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session.state().stack().len(),
        1,
        "intervening-if true at trigger time -> goes on stack"
    );
    drv.cast_from_hand(
        PlayerId(0),
        shatter_art,
        vec![TargetRef::Object { id: art_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(12);
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        20,
        "CR 603.4: intervening-if false on resolution -> fizzles silently"
    );

    // Case (c): Next turn (Turn 3), cast Artifact? Wait: we only had 1 Artifact, let's check Turn 1 case (a)+(c)
    // Wait: did Case (b) consume only_once_per_turn? Yes, it triggered once!
    // What if we test Case (a) followed directly by Case (c) in the SAME turn? Let's verify in a 2nd mini-session!
    let mut drv2 = SessionDriver::new(
        Arc::new(drv.session.state().db().clone()),
        GameConfig::standard_1v1(),
        vec![
            DeckSpec {
                commanders: vec![],
                mainboard: vec![watch, bear, bear, artifact, plains, plains, plains],
                sideboard: vec![],
            },
            DeckSpec {
                commanders: vec![],
                mainboard: vec![plains; 15],
                sideboard: vec![],
            },
        ],
        60340,
    )
    .unwrap();
    drv2.keep_all_hands();
    drv2.advance_to(1, GamePhase::Main1, 16);
    drv2.cast_from_hand(PlayerId(0), watch, vec![]).unwrap();
    drv2.resolve_all_stack(8);
    // Bear #1 without artifact -> intervening-if false -> does NOT consume once-per-turn!
    drv2.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv2.resolve_all_stack(8);
    assert_eq!(drv2.session.state().player(PlayerId(0)).unwrap().life, 20);
    // Cast artifact, then Bear #2 in the SAME turn -> triggers & resolves -> 25 life!
    drv2.cast_from_hand(PlayerId(0), artifact, vec![]).unwrap();
    drv2.resolve_all_stack(8);
    drv2.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv2.resolve_all_stack(8);
    assert_eq!(drv2.session.state().player(PlayerId(0)).unwrap().life, 25);
}

#[test]
fn synth_cr603_7_12_delayed_reflexive_and_cascading_lifo() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6037", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_6037", "Synth Bear", "{0}", 2, 2);

    // Delayed trigger spell: "Create a delayed trigger at beginning of next end step: P0 gains 4 life."
    let delayed_spell = b.add_card("synth_delay_6037", "Synth DelayedBlessing", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::DelayedTrigger {
                        delayed: Some(DelayedTriggerPayload {
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
                                EffectOp::GainLife {
                                    set_life_total: false,
                                },
                                Selector::PlayerRef {
                                    player: PlayerRef::You,
                                },
                                Some(Amount::Const { n: 4 }),
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

    // Reflexive trigger spell: "Gain 1 life. When you do, deal 2 damage to target creature or player."
    let mut ref_step = effect_step(
        EffectOp::GainLife {
            set_life_total: false,
        },
        Selector::PlayerRef {
            player: PlayerRef::You,
        },
        Some(Amount::Const { n: 1 }),
    );
    ref_step.reflexive_trigger = Some(Box::new(ReflexiveTriggerSpec {
        targets: vec![target_player()],
        effect: vec![effect_step(
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
        modal: None,
        text: None,
        event_filter: None,
        object_filter: None,
    }));
    let reflexive_spell = b.add_card("synth_refl_60312", "Synth Reflex", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(vec![], vec![ref_step]);
    });

    let db = b.build_db();
    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            bear,
            delayed_spell,
            reflexive_spell,
            plains,
            plains,
            plains,
            plains,
            plains,
            plains,
            plains,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let (cfg, decks) = common::guaranteed_decks(
        &[&[bear, delayed_spell, reflexive_spell], &[plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let _ = (p0_deck, p1_deck);
    let mut drv = SessionDriver::new(db, cfg, decks, 6037).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // 1) Cast delayed_spell -> resolves (moves to GY, source no longer exists), registers one-shot delayed trigger.
    drv.cast_from_hand(PlayerId(0), delayed_spell, vec![])
        .unwrap();
    drv.resolve_all_stack(8);

    // 2) Cast Bear (stays on bottom of stack), then cast Reflexive spell ABOVE it!
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.cast_from_hand(PlayerId(0), reflexive_spell, vec![])
        .unwrap();
    assert_eq!(drv.session.state().stack().len(), 2);

    // Resolve Reflexive spell -> gains 1 life (21) and queues reflexive trigger onto stack (enters CastingSequence to pick target_player)!
    drv.pass_priority();
    drv.pass_priority();
    if matches!(drv.decision(), DecisionState::CastingSequence) {
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        if matches!(drv.decision(), DecisionState::CastingSequence) {
            drv.step(PlayerId(0), Action::DoneTargets).unwrap();
        }
    } else if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseMode { prompt_id, .. },
    } = drv.decision()
    {
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickMode { index: 1 },
            },
        )
        .unwrap();
    }
    // Reflexive trigger must be on stack ABOVE Bear (stack.len() == 2)!
    assert_eq!(drv.session.state().stack().len(), 2);
    // Resolve reflexive trigger (LIFO, BEFORE Bear!) -> P1 takes 2 damage (18).
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 18);
    assert_eq!(drv.session.state().stack().len(), 1); // Bear still on stack!
    drv.resolve_all_stack(8);

    // 3) Advance to EndStep -> delayed trigger fires once (+4 life -> 25)!
    drv.advance_to(1, GamePhase::EndStep, 32);
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 25);

    // Advance to Turn 2 EndStep -> one-shot delayed trigger does NOT fire a second time!
    drv.advance_to(2, GamePhase::EndStep, 32);
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 25);
}

#[test]
fn synth_cr603_8_state_trigger_anti_loop_guard() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6038", "Synth Plains", "Plains", ManaColor::W);
    // State trigger: "When a player has 25 or more life, you lose 5 life."
    let state_ench = b.add_card("synth_state_6038", "Synth Balance", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                TriggerDef {
                    event: EventKind::StateTriggered,
                    filter: EventFilter {
                        detail: Some(EventDetail::State {
                            condition: Box::new(Condition::AnyPlayerLife {
                                op: CompareOp::Ge,
                                count: 25,
                            }),
                        }),
                        ..Default::default()
                    },
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: false,
                    do_this_once_per_turn: false,
                    is_state_trigger: true,
                    or: vec![],
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                },
                vec![],
                vec![effect_step(
                    EffectOp::LoseLife,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 5 }),
                )],
            );
    });
    let heal10 = b.add_card("synth_h10_6038", "Synth SurgeLife10", |c| {
        c.mana_cost("{0}")
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
                    Some(Amount::Const { n: 11 }),
                )],
            );
    });
    let ping1_self = b.add_card("synth_h1_6038", "Synth Pulse", |c| {
        c.mana_cost("{0}")
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
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            state_ench, heal10, ping1_self, plains, plains, plains, plains,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6038).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), state_ench, vec![]).unwrap();
    drv.resolve_all_stack(8);
    assert!(drv.session.state().stack().is_empty());

    // Gain 11 -> 31 life (>= 25): state trigger queues ONCE onto stack!
    drv.cast_from_hand(PlayerId(0), heal10, vec![]).unwrap();
    drv.resolve_all_stack(8); // Wait: heal10 resolves -> state trigger immediately queues!
    // Actually resolve_all_stack resolves heal10 AND the state trigger!
    // Let's trace carefully: 31 -> state trigger #1 resolves (-5 -> 26 >= 25) -> state trigger #2 queues and resolves (-5 -> 21 < 25)!
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        21,
        "CR 603.8: state trigger re-triggers after leaving stack while >= 25 (31 -> 26 -> 21) and stops below 25"
    );

    // Now test anti-loop guard while on stack: gain 11 -> 32, pass 2x to resolve heal10 ONLY (stack has 1 state trigger),
    // then cast ping1_self while state trigger is on stack -> stack len is 2 (not infinite!).
    let mut drv2 = SessionDriver::new(
        Arc::new(drv.session.state().db().clone()),
        GameConfig::standard_1v1(),
        vec![
            DeckSpec {
                commanders: vec![],
                mainboard: vec![
                    state_ench, heal10, ping1_self, plains, plains, plains, plains,
                ],
                sideboard: vec![],
            },
            DeckSpec {
                commanders: vec![],
                mainboard: vec![plains; 15],
                sideboard: vec![],
            },
        ],
        60381,
    )
    .unwrap();
    drv2.keep_all_hands();
    drv2.advance_to(1, GamePhase::Main1, 16);
    drv2.cast_from_hand(PlayerId(0), state_ench, vec![])
        .unwrap();
    drv2.resolve_all_stack(8);
    drv2.cast_from_hand(PlayerId(0), heal10, vec![]).unwrap();
    drv2.pass_priority();
    drv2.pass_priority();
    assert_eq!(
        drv2.session.state().stack().len(),
        1,
        "CR 603.8: exactly 1 state trigger queued on stack"
    );
    drv2.cast_from_hand(PlayerId(0), ping1_self, vec![])
        .unwrap();
    assert_eq!(
        drv2.session.state().stack().len(),
        2,
        "CR 603.8: state trigger must NOT re-queue while already on stack"
    );
    drv2.resolve_all_stack(16);
    assert_eq!(drv2.session.state().player(PlayerId(0)).unwrap().life, 22);
}

#[test]
fn synth_cr605_triggered_mana_abilities_and_multipliers() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_for_605", "Synth Forest", "Forest", ManaColor::G);
    // CR 605.1b triggered mana ability: whenever you tap a permanent for mana, add 1 extra mana of produced color (stackless!).
    let mana_flare = b.add_card("synth_flare_605", "Synth ManaFlare", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(EventKind::TappedForMana, EventFilter::default()),
                vec![],
                vec![effect_step(
                    EffectOp::AddMana {
                        production: ManaProduction::FromProducedEvent,
                        restriction: None,
                        restriction_spec: None,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    None,
                )],
            );
    });
    // Trigger multiplier (2x triggers) + ETB-only suppressor (0 triggers on ZoneChange, must NOT suppress TappedForMana!)
    let trig_doubler = b.add_card("synth_trigamp_605", "Synth TriggerAmplifier", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::Multiplier {
                    what: MultiplierTarget::Triggers,
                    factor: 2.0,
                    delta: 0,
                    filter: None,
                    counter: None,
                    cause: None,
                    cause_or: vec![],
                    round: None,
                    applies_to_players: false,
                    who_places: None,
                    who_creates: None,
                    extra_token: None,
                    extra_token_matching_count: false,
                    trigger_source_attached_to: false,
                },
                condition: None,
            });
    });
    let etb_suppressor = b.add_card("synth_etbsup_605", "Synth EtbHush", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::Multiplier {
                    what: MultiplierTarget::Triggers,
                    factor: 0.0,
                    delta: 0,
                    filter: None,
                    counter: None,
                    cause: Some(EventPattern {
                        event: EventKind::ZoneChange,
                        filter: EventFilter {
                            detail: Some(EventDetail::ZoneChange {
                                from: ZoneMask::empty(),
                                to: Some(Zone::Battlefield),
                                from_anywhere: false,
                                not_from_zone: None,
                                not_to_zone: None,
                                enters_untapped: false,
                                dealt_damage_by_self_this_turn: false,
                            }),
                            ..Default::default()
                        },
                    }),
                    cause_or: vec![],
                    round: None,
                    applies_to_players: false,
                    who_places: None,
                    who_creates: None,
                    extra_token: None,
                    extra_token_matching_count: false,
                    trigger_source_attached_to: false,
                },
                condition: None,
            });
    });
    let etb_heal = b.add_card("synth_etbh_605", "Synth EtbHealer", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::empty(),
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

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            forest,
            mana_flare,
            trig_doubler,
            etb_suppressor,
            etb_heal,
            forest,
            forest,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![forest; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6051).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let f_h = drv.find_hand_object(PlayerId(0), forest).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: f_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    for card in [mana_flare, trig_doubler, etb_suppressor] {
        drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    // EtbHealer's ETB trigger IS suppressed by EtbHush -> life remains 20.
    drv.cast_from_hand(PlayerId(0), etb_heal, vec![]).unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 20);

    // Tapping Forest for mana fires CR 605.1b triggered mana ability immediately (stack stays empty!)
    // AND scales with TriggerMultiplier (1 base + 2 from doubled ManaFlare = 3 Green mana in pool!).
    let f_bf = drv.find_battlefield_object(PlayerId(0), forest).unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: f_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    assert!(
        drv.session.state().stack().is_empty(),
        "CR 605.4a: triggered mana ability resolves immediately without using stack"
    );
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().mana_pool[4],
        3,
        "1 Forest + 2x TriggerMultiplier ManaFlare = 3 Green mana"
    );
}

#[test]
fn synth_cr603_7_delayed_trigger_preserves_bind_as_bindings() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_dtbind", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_dtbind", "Synth Bear", "{0}", 2, 2);
    let blink = b.add_card("synth_blink_dtbind", "Synth Delayed Blink", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![
                    mtg_ir::EffectStep {
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
                            delayed: Some(DelayedTriggerPayload {
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
    let (cfg, decks) = common::guaranteed_decks(
        &[&[bear, blink], &[plains]],
        &[],
        2,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 603701).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let b_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    drv.cast_from_hand(PlayerId(0), blink, vec![TargetRef::Object { id: b_bf }])
        .unwrap();
    drv.resolve_all_stack(8);
    assert!(
        drv.find_battlefield_object(PlayerId(0), bear).is_none(),
        "Bear must be exiled during Main1"
    );
    assert!(
        drv.session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .exile
            .iter()
            .any(|&id| drv.session.state().object(id).unwrap().card_def == bear),
        "Bear must be in exile"
    );

    // Advance to Turn 2 Main1 -> Turn 1 EndStep delayed trigger fires and returns Bear via binding "ex"!
    drv.advance_to(2, GamePhase::Main1, 32);
    let returned = drv
        .find_battlefield_object(PlayerId(0), bear)
        .expect("CR 603.7: delayed trigger must preserve bind_as binding and return exiled Bear");
    assert!(returned.generation > b_bf.generation);
}
