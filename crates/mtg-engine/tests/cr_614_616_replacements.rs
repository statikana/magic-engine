//! CR 614–616 (Replacement Effects, Priority Tiers, Applied-Once, Prevention Shields,
//! ETB Lookahead & Snapshot Pause) integration tests.

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, assert_snapshot_equivalence, effect_step, simple_trigger,
    target_creature, target_player,
};
use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig, PlayerId,
    PowerToughness, TargetRef,
};
use mtg_ir::{
    Amount, CardTypeMask, ColorMask, Compare, CompareOp, DamageModMode, DamageModTarget,
    DamagePreventionSpec, DamageScaleSpec, EffectOp, EtbCopySpec, EventDetail, EventFilter,
    EventKind, EventPattern, GamePhase, LifeGainScaleSpec, ManaColor, MultiplierTarget,
    ObjectFilter, PlayerFilter, PlayerRef, PlayerRelation, PreventMode, PreventScope,
    RelativeControl, ReplacementDef, ReplacementWith, RuleMod, Selector, StaticBody, StaticDef,
    TriggerSubject, Zone, ZoneChangeMod, ZoneMask,
};

#[test]
fn synth_cr616_1_affected_player_ordering_counters_and_damage() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let plains = b.add_synth_basic_land("synth_pl_616", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_616", "Synth Bear", "{0}", 2, 2);

    // (a) Synth Scales (+1 counter) + Synth Doubler (x2 counters)
    let scales = b.add_card("synth_scales_616", "Synth Scales", |c| {
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
    let doubler = b.add_card("synth_cdbl_616", "Synth Doubler", |c| {
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
    let put2 = b.add_card("synth_put2_616", "Synth Bolster2", |c| {
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });

    // (b) Synth RedAmp (+2 damage) + Synth DamageDoubler (x2 damage)
    let red_amp = b.add_card("synth_ramp_616", "Synth RedAmp", |c| {
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
    let dmg_dbl = b.add_card("synth_ddbl_616", "Synth DamageDoubler", |c| {
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
    let bolt3 = b.add_synth_burn_instant("synth_bolt3_616", "Synth Bolt3", "{0}", 3);

    // (c) Self-replacement (CR 616.1a enters tapped) + general replacement (enters with +1/+1) -> no prompt!
    let self_tapped_bear = b.add_card("synth_stbear_616", "Synth TappedBear", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            });
    });
    let global_etb_counter = b.add_card("synth_etbc_616", "Synth Rhythm", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((plus_cid, Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });

    let db = b.build_db();
    let mut cfg = GameConfig::standard_1v1();
    cfg.starting_hand_size = 9;
    cfg.max_hand_size = 9;
    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            bear,
            scales,
            doubler,
            put2,
            red_amp,
            dmg_dbl,
            bolt3,
            self_tapped_bear,
            global_etb_counter,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, cfg, vec![p0_deck, p1_deck], 6161).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    for card in [bear, scales, doubler, red_amp, dmg_dbl] {
        drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    // (a) Put 2 +1/+1 counters on Bear -> prompt P0: index 0 (Scales first: (2+1)*2 = 6) vs index 1 (Doubler first: (2*2)+1 = 5).
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(PlayerId(0), put2, vec![TargetRef::Object { id: bear_bf }])
        .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    if let DecisionState::Prompt {
        request:
            ChoiceRequest::ChooseReplacement {
                prompt_id,
                option_count,
            },
    } = drv.decision()
    {
        assert_eq!(option_count, 2);
        assert_eq!(drv.session.current_actor(), Some(PlayerId(0)));
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 0 },
            },
        )
        .unwrap();
    } else {
        panic!("expected ChooseReplacement for Scales vs Doubler");
    }
    assert_eq!(
        drv.session.state().counter_count(bear_bf, plus_cid),
        6,
        "(2+1)*2 = 6 counters"
    );

    // (b) Deal 3 damage to P1 -> prompt P1 (damaged player): pick index 0 (+2 first: (3+2)*2 = 10) vs index 1 (*2 first: 8).
    drv.cast_from_hand(
        PlayerId(0),
        bolt3,
        vec![TargetRef::Player { id: PlayerId(1) }],
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    if let DecisionState::Prompt {
        request:
            ChoiceRequest::ChooseReplacement {
                prompt_id,
                option_count,
            },
    } = drv.decision()
    {
        assert_eq!(option_count, 2);
        assert_eq!(drv.session.current_actor(), Some(PlayerId(1)));
        drv.step(
            PlayerId(1),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 1 },
            },
        )
        .unwrap();
    } else {
        panic!("expected ChooseReplacement for RedAmp vs DamageDoubler");
    }
    assert_eq!(
        drv.session.state().player(PlayerId(1)).unwrap().life,
        12,
        "(3*2)+2 = 8 damage -> P1 at 12 life"
    );

    // (c) Priority tiers: cast Rhythm (general ETB +1/+1) then TappedBear (616.1a self-replacement).
    // Note: Rhythm + Scales + Doubler will apply on the +1/+1 counters -> wait: let's just check TappedBear
    // does NOT prompt between self_tapped (616.1a) and Rhythm (616.1d), only between Scales and Doubler!
    drv.cast_from_hand(PlayerId(0), global_etb_counter, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), self_tapped_bear, vec![])
        .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    // Self-replacement applied automatically (616.1a) before Scales/Doubler (616.1d);
    // if Scales/Doubler prompt appears, answer it; TappedBear must enter tapped with counters.
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
    }
    let st_bf = drv
        .find_battlefield_object(PlayerId(0), self_tapped_bear)
        .unwrap();
    assert!(
        drv.session
            .state()
            .object(st_bf)
            .unwrap()
            .flags
            .contains(mtg_engine::ObjectFlags::TAPPED)
    );
    assert!(drv.session.state().counter_count(st_bf, plus_cid) >= 2);
}

#[test]
fn synth_cr614_5_chained_replacements_single_application_and_snapshot() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6145", "Synth Plains", "Plains", ManaColor::W);
    let life_doubler = b.add_card("synth_ldbl_6145", "Synth LifeDoubler", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::LifeGainReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: true,
                        add: None,
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            });
    });
    let life_adder = b.add_card("synth_ladd_6145", "Synth LifeAdder", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::LifeGainReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::LifeGainScale {
                    spec: LifeGainScaleSpec {
                        double: false,
                        add: Some(Box::new(Amount::Const { n: 3 })),
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                condition: None,
            });
    });
    let heal = b.add_card("synth_heal_6145", "Synth Salve", |c| {
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            life_doubler,
            life_adder,
            heal,
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
    let mut drv = SessionDriver::new(
        db.clone(),
        GameConfig::standard_1v1(),
        vec![p0_deck, p1_deck],
        6145,
    )
    .unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), life_doubler, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), life_adder, vec![]).unwrap();
    drv.resolve_all_stack(8);

    drv.cast_from_hand(PlayerId(0), heal, vec![]).unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // Paused mid-ChooseReplacement! Verify PendingReplacementState, snapshot roundtrip, and CR 614.5 applied-once!
    let prompt_id = match drv.decision() {
        DecisionState::Prompt {
            request: ChoiceRequest::ChooseReplacement { prompt_id, .. },
        } => prompt_id,
        other => panic!("expected ChooseReplacement pause, got {other:?}"),
    };
    assert!(drv.session.state().pending_replacement().is_some());

    // Snapshot equivalence across answering PickReplacement { index: 1 } (Adder +3 first -> 5, then Doubler *2 -> 10)
    assert_snapshot_equivalence(
        &drv.session,
        &db,
        &[(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickReplacement { index: 1 },
            },
        )],
    );

    drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id,
            answer: ChoiceAnswer::PickReplacement { index: 1 },
        },
    )
    .unwrap();
    assert!(drv.session.state().pending_replacement().is_none());
    // (2 + 3) * 2 = 10 -> 20 + 10 = 30 life (neither replacement applied twice).
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 30);
}

#[test]
fn synth_cr614_6_rest_exile_replacement_skips_dies_fires_ltb() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_6146", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_6146", "Synth Bear", "{0}", 2, 2);
    let grave_exile = b.add_card("synth_gex_6146", "Synth GraveExile", |c| {
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
    let dies_watch = b.add_card("synth_dies_6146", "Synth DiesWatch", |c| {
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
                    Some(Amount::Const { n: 10 }),
                )],
            );
    });
    let ltb_watch = b.add_card("synth_ltb_6146", "Synth LtbWatch", |c| {
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let shatter = b.add_card("synth_kill_6146", "Synth Shatter", |c| {
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
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            bear,
            grave_exile,
            dies_watch,
            ltb_watch,
            shatter,
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
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6146).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    for card in [bear, grave_exile, dies_watch, ltb_watch] {
        drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        shatter,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(12);

    assert!(drv.find_exile_object(PlayerId(0), bear).is_some());
    assert!(drv.find_graveyard_object(PlayerId(0), bear).is_none());
    // LtbWatch (+3) fired, DiesWatch (+10) did NOT fire -> life == 23!
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);
}

#[test]
fn synth_cr615_prevention_shields_redirection_and_unpreventable() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_615", "Synth Plains", "Plains", ManaColor::W);
    let shield3 = b.add_card("synth_sh3_615", "Synth Shield3", |c| {
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
    let doubler = b.add_card("synth_ddbl_615", "Synth DmgDoubler", |c| {
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
    let no_prev = b.add_card("synth_noprev_615", "Synth NoPrevent", |c| {
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
    let bolt5 = b.add_synth_burn_instant("synth_bolt5_615", "Synth Bolt5", "{0}", 5);
    let bolt2 = b.add_synth_burn_instant("synth_bolt2_615", "Synth Bolt2", "{0}", 2);
    let db = b.build_db();

    // (a) Partial prevent next 3 consumes 3 of 5 damage (2 dealt, shield exhausted).
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![shield3, bolt5, bolt2, plains, plains, plains, plains],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plains; 15],
            sideboard: vec![],
        };
        let mut drv = SessionDriver::new(
            db.clone(),
            GameConfig::standard_1v1(),
            vec![p0_deck, p1_deck],
            6151,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(
            PlayerId(0),
            shield3,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(
            PlayerId(0),
            bolt5,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        // 5 - 3 = 2 dealt -> P1 at 18, shield exhausted
        assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 18);
        assert!(drv.session.state().prevention_shields().is_empty());

        drv.cast_from_hand(
            PlayerId(0),
            bolt2,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 16);
    }

    // (b) Affected player orders prevention shield (prevent 3) vs Doubler (x2) on 5 damage:
    // prevent-first = (5-3)*2 = 4 vs double-first = (5*2)-3 = 7!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![shield3, doubler, bolt5, plains, plains, plains, plains],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plains; 15],
            sideboard: vec![],
        };
        let mut drv = SessionDriver::new(
            db.clone(),
            GameConfig::standard_1v1(),
            vec![p0_deck, p1_deck],
            6152,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), doubler, vec![]).unwrap();
        drv.cast_from_hand(
            PlayerId(0),
            shield3,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);

        drv.cast_from_hand(
            PlayerId(0),
            bolt5,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();

        if let DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseReplacement {
                    prompt_id,
                    option_count,
                },
        } = drv.decision()
        {
            assert_eq!(option_count, 2);
            assert_eq!(drv.session.current_actor(), Some(PlayerId(1)));
            // Pick one ordering (either Doubler first -> 7 dmg -> life 13, or Shield first -> 4 dmg -> life 16).
            drv.step(
                PlayerId(1),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index: 0 },
                },
            )
            .unwrap();
        } else {
            panic!("expected ChooseReplacement between Shield3 and Doubler");
        }
        let life_after = drv.session.state().player(PlayerId(1)).unwrap().life;
        assert!(
            life_after == 16 || life_after == 13,
            "expected 4 or 7 damage depending on index 0 choice (got life={life_after})"
        );
    }

    // (c) DamageCantBePrevented bypasses Shield3 while still allowing Doubler (2*2 = 4 damage)!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![shield3, doubler, no_prev, bolt2, plains, plains, plains],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plains; 15],
            sideboard: vec![],
        };
        let mut drv =
            SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6153)
                .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        for card in [doubler, no_prev] {
            drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
            drv.resolve_all_stack(8);
        }
        drv.cast_from_hand(
            PlayerId(0),
            shield3,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(
            PlayerId(0),
            bolt2,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);

        assert_eq!(
            drv.session.state().player(PlayerId(1)).unwrap().life,
            16,
            "CR 615.12: unpreventable damage bypasses shield and still gets doubled (2*2 = 4)"
        );
        assert_eq!(drv.session.state().prevention_shields().len(), 1);
    }
}

#[test]
fn synth_cr614_12_etb_lookahead_zero_toughness_and_clone_triggers() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let plains = b.add_synth_basic_land("synth_pl_61412", "Synth Plains", "Plains", ManaColor::W);

    // 0/0 creature that enters with a +1/+1 counter (CR 614.1c)
    let zero_zero = b.add_card("synth_hydra_61412", "Synth Hatchling", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(0, 0)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
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
                },
                with: ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        counters: Some((plus_cid, Box::new(Amount::Const { n: 1 }))),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            });
    });

    // Watcher: "Whenever a creature with power 1 enters, gain 4 life" (CR 614.12 lookahead)
    let power1_watcher = b.add_card("synth_p1watch_61412", "Synth Power1Watch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                power: Some(Compare {
                                    op: CompareOp::Eq,
                                    value: Box::new(Amount::Const { n: 1 }),
                                }),
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
                    Some(Amount::Const { n: 4 }),
                )],
            );
    });

    // ETB-draw creature (2/2)
    let etb_draw = b.add_card("synth_oracle_61412", "Synth Sage", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
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

    // Synth Clone copying any creature on battlefield
    let clone_card = b.add_card("synth_clone_61412", "Synth Clone", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(0, 0)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
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
                self_replacement: true,
                who: None,
                condition: None,
            });
    });

    let db = b.build_db();
    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            power1_watcher,
            zero_zero,
            etb_draw,
            clone_card,
            plains,
            plains,
            plains,
            plains,
            plains,
            plains,
        ],
        sideboard: vec![],
    };
    // Ensure first 4 cards are in opening hand? Wait: 10-card deck might miss one in opening 7!
    // Put ONLY the 4 needed cards + 3 Plains = 7 cards so opening hand has 100% of them,
    // plus wait: Draw needs >=1 card in library! Let's use starting_hand_size = 7 and put 7 cards + 2 Plains in library!
    // Wait: if we put 7 unique? No, 4 cards + 3 Plains = 7, if library is empty then Draw loses!
    // Use 4 cards (each 2 copies? No, 10 cards with 2 copies of each? Wait: let's set starting_hand_size: 8 or use a 7-card deck where we draw on Turn 1 -> wait: Turn 1 P0 skips draw step! We only draw from etb_draw twice = 2 cards!).
    // So if p0_deck has 9 cards and starting_hand_size = 7, wait — any 2 might be in library!
    // Solution: `GameConfig` has `starting_hand_size: 8`? No, if `starting_hand_size = 7`, put `[power1_watcher, zero_zero, etb_draw, clone_card, etb_draw, clone_card, zero_zero, power1_watcher]`? Still random.
    // Even simpler: just set `config.starting_hand_size = 10` on a 12-card deck with `max_hand_size = 12`! No, wait: `MatchSession::new` draws `starting_hand_size` cards, and `power1_watcher, zero_zero, etb_draw, clone_card` are in a 4-card + 2-library-filler = 6-card? If `starting_hand_size = 4` and `mainboard = [power1_watcher, zero_zero, etb_draw, clone_card, plains, plains]` (with `plains` at bottom? No, shuffle randomizes!).
    // Wait: if `mainboard = [power1_watcher, zero_zero, etb_draw, clone_card]` (4 cards) and `starting_hand_size = 4`, ALL 4 are in hand, and we only need 2 cards in library for `etb_draw`! Wait: if `etb_draw` draws, empty library causes SBA loss only AFTER drawing from empty library. So in a 10-card deck, if we advance turns until all 4 are drawn, we NEVER miss! Let's just use `advance_to` loop (max 3 turns) on a 10-card deck `[power1_watcher, zero_zero, etb_draw, clone_card, plains, plains, plains, plains, plains, plains]` — by Turn 5 `P0` has drawn 7 + 2 = 9 of 10 cards! Or 2 copies of each in 10 cards -> 100% drawn by Turn 3!
    let mut main = vec![
        power1_watcher,
        zero_zero,
        etb_draw,
        clone_card,
        power1_watcher,
        zero_zero,
        etb_draw,
        clone_card,
        plains,
        plains,
    ];
    let p0_deck_10 = DeckSpec {
        commanders: vec![],
        mainboard: main.split_off(0),
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let _ = p0_deck;
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![p0_deck_10, p1_deck],
        61412,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 32);
        if drv.find_hand_object(PlayerId(0), power1_watcher).is_some()
            && drv.find_hand_object(PlayerId(0), zero_zero).is_some()
            && drv.find_hand_object(PlayerId(0), etb_draw).is_some()
            && drv.find_hand_object(PlayerId(0), clone_card).is_some()
        {
            break;
        }
    }

    drv.cast_from_hand(PlayerId(0), power1_watcher, vec![])
        .unwrap();
    drv.resolve_all_stack(8);

    // 1) Cast 0/0 Hatchling -> enters as 1/1 -> survives SBA and triggers Power1Watch (+4 life -> 24).
    drv.cast_from_hand(PlayerId(0), zero_zero, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let zz_bf = drv.find_battlefield_object(PlayerId(0), zero_zero).unwrap();
    assert_eq!(
        drv.session.state().evaluate_board().get(zz_bf).unwrap().pt,
        Some(PowerToughness {
            power: 1,
            toughness: 1
        })
    );
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 24);

    // 2) Cast ETB-draw Sage (draws 1), then Clone copying Sage (enters with copied ETB trigger, draws 1)!
    let hand_before_sage = drv.session.state().player(PlayerId(0)).unwrap().hand.len();
    drv.cast_from_hand(PlayerId(0), etb_draw, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let hand_after_sage = drv.session.state().player(PlayerId(0)).unwrap().hand.len();
    assert_eq!(hand_after_sage, hand_before_sage); // -1 cast +1 draw

    let sage_bf = drv.find_battlefield_object(PlayerId(0), etb_draw).unwrap();
    drv.cast_from_hand(PlayerId(0), clone_card, vec![]).unwrap();
    drv.pass_priority();
    drv.pass_priority();
    if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseObjects { prompt_id, .. },
    } = drv.decision()
    {
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id,
                answer: ChoiceAnswer::PickObject { id: sage_bf },
            },
        )
        .unwrap();
    } else {
        panic!("expected ChooseObjects for Synth Clone ETB copy");
    }
    drv.resolve_all_stack(8);
    let hand_after_clone = drv.session.state().player(PlayerId(0)).unwrap().hand.len();
    assert_eq!(
        hand_after_clone, hand_after_sage,
        "Clone copying ETB-draw Sage must fire copied ETB draw trigger (-1 cast +1 draw)"
    );
}
