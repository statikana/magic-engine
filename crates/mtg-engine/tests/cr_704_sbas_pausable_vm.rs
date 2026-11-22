//! CR 704 (Iterative SBA Fixpoint, Legend Rule, CR 704.8 Pre-Annihilation LKI, Simultaneous Draw)
//! and Pausable ResolutionVM / 6-Kind ChoiceRequest integration tests.

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, assert_snapshot_equivalence, effect_step, simple_trigger,
    target_creature, target_player,
};
use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, EngineError, GameConfig,
    PlayerId, TargetRef,
};
use mtg_ir::{
    Affected, Amount, CardTypeMask, CostDef, DesignationKind, EffectOp, EventDetail, EventFilter,
    EventKind, EventPattern, GamePhase, Layer, LayerMod, ManaColor, MultiplierTarget, ObjectFilter,
    OptionalMode, OptionalSpec, PlayerCounterKind, PlayerFilter, PlayerRef, PlayerRelation,
    PunisherPlayersKind, PunisherSpec, Ref, RelativeControl, ReplacementDef, ReplacementWith,
    RuleMod, SearchDestination, SearchSpec, SelectionMod, Selector, StaticBody, StaticDef,
    SupertypeMask, TriggerSubject, Zone, ZoneChangeMod, ZoneMask,
};

#[test]
fn synth_cr704_3_iterative_fixpoint_and_cant_lose_legend_exempt_removal() {
    let mut b = SynthBundleBuilder::new();
    let minus_cid = b.intern_counter("-1/-1");
    let plains = b.add_synth_basic_land("synth_pl_7043", "Synth Plains", "Plains", ManaColor::W);
    let lord = b.add_card("synth_lord_7043", "Synth Lord", |c| {
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
    let zero_zero = b.add_synth_vanilla("synth_zz_7043", "Synth Germ", "{0}", 0, 0);
    let indest = b.add_synth_keyword_creature(
        "synth_ind_7043",
        "Synth Colossus",
        "{0}",
        2,
        2,
        &["indestructible"],
    );
    let mirror = b.add_card("synth_mir_7043", "Synth Gallery", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::LegendRuleExempt { filter: None },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let leg = b.add_synth_legendary_creature("synth_leg_7043", "Synth Hero", "{0}", 2, 2);
    let bolt3 = b.add_synth_burn_instant("synth_b3_7043", "Synth Bolt3", "{0}", 3);
    let put_minus3 = b.add_card("synth_m3_7043", "Synth Blight3", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(minus_cid),
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let destroy_art = b.add_card("synth_dart_7043", "Synth ShatterArt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![mtg_ir::TargetSpec {
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ARTIFACT,
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

    // 1) Iterative fixpoint (CR 704.3): lethal damage kills Lord in pass 1, then 0/0 Germ dies in pass 2;
    //    Indestructible survives 3 damage (704.5g) but dies to three -1/-1 counters (704.5f).
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![lord, zero_zero, indest, bolt3, bolt3, put_minus3, plains],
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
            7043,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        for c in [lord, zero_zero, indest] {
            drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
            drv.resolve_all_stack(8);
        }
        let lord_bf = drv.find_battlefield_object(PlayerId(0), lord).unwrap();
        let ind_bf = drv.find_battlefield_object(PlayerId(0), indest).unwrap();

        // 3 damage on 3/3 Indestructible (with Lord) -> survives even after Lord dies!
        drv.cast_from_hand(PlayerId(0), bolt3, vec![TargetRef::Object { id: ind_bf }])
            .unwrap();
        drv.resolve_all_stack(8);
        assert!(drv.find_battlefield_object(PlayerId(0), indest).is_some());

        // 3 damage on 2/2 Lord -> Pass 1 kills Lord, Pass 2 kills 0/0 Germ (CR 704.3 fixpoint)!
        drv.cast_from_hand(PlayerId(0), bolt3, vec![TargetRef::Object { id: lord_bf }])
            .unwrap();
        drv.resolve_all_stack(8);
        assert!(drv.find_battlefield_object(PlayerId(0), lord).is_none());
        assert!(
            drv.find_battlefield_object(PlayerId(0), zero_zero)
                .is_none()
        );
        // Indestructible (now 2/2 with 3 damage) STILL survives lethal damage (704.5g)!
        assert!(drv.find_battlefield_object(PlayerId(0), indest).is_some());

        // 3 -1/-1 counters on Indestructible -> toughness <= 0 -> dies to SBA 704.5f!
        drv.cast_from_hand(
            PlayerId(0),
            put_minus3,
            vec![TargetRef::Object { id: ind_bf }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert!(drv.find_battlefield_object(PlayerId(0), indest).is_none());
    }

    // 2) Removing LegendRuleExempt triggers immediate Legend Rule SBA on next check!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![mirror, leg, leg, destroy_art, plains, plains, plains],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plains; 15],
            sideboard: vec![],
        };
        let mut drv =
            SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 7044)
                .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        for c in [mirror, leg, leg] {
            drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
            drv.resolve_all_stack(8);
        }
        let leg_count = drv
            .session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .filter(|&&id| drv.session.state().object(id).unwrap().card_def == leg)
            .count();
        assert_eq!(leg_count, 2);

        let mir_bf = drv.find_battlefield_object(PlayerId(0), mirror).unwrap();
        drv.cast_from_hand(
            PlayerId(0),
            destroy_art,
            vec![TargetRef::Object { id: mir_bf }],
        )
        .unwrap();
        drv.pass_priority();
        drv.pass_priority();
        // Immediate Legend Rule prompt!
        if let DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                },
        } = drv.decision()
        {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: candidates[0] },
                },
            )
            .unwrap();
        } else {
            panic!("expected LegendRule ChooseObjects after LegendRuleExempt removed");
        }
    }
}

#[test]
fn synth_cr704_5j_legend_rule_prompt_not_destroy_not_sacrifice() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_7045", "Synth Plains", "Plains", ManaColor::W);
    let leg_indest = b.add_card("synth_legi_7045", "Synth ImmortalHero", |c| {
        c.mana_cost("{0}")
            .supertypes(SupertypeMask::LEGENDARY)
            .types(CardTypeMask::CREATURE)
            .pt(3, 3)
            .keyword("indestructible");
    });
    let sac_watch = b.add_card("synth_sacw_7045", "Synth SacWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(EventKind::Sacrificed, EventFilter::default()),
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
    let dies_watch = b.add_card("synth_diesw_7045", "Synth DiesWatch", |c| {
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let grave_exile = b.add_card("synth_gex_7045", "Synth GraveExile", |c| {
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

    // Part A: Without GraveExile -> unchosen Indestructible copy goes to GY, fires DiesWatch (+3), NOT SacWatch (+10)!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                sac_watch, dies_watch, leg_indest, leg_indest, plains, plains, plains,
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
            7045,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        for c in [sac_watch, dies_watch, leg_indest, leg_indest] {
            drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
            if c == leg_indest {
                drv.pass_priority();
                drv.pass_priority();
                if let DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } = drv.decision()
                {
                    drv.step(
                        PlayerId(0),
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickObject { id: candidates[0] },
                        },
                    )
                    .unwrap();
                }
            }
            drv.resolve_all_stack(8);
        }
        assert!(drv.find_graveyard_object(PlayerId(0), leg_indest).is_some());
        assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);
    }

    // Part B: With GraveExile -> unchosen copy routes through replacement to Exile!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                grave_exile,
                leg_indest,
                leg_indest,
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
            db,
            GameConfig::standard_1v1(),
            vec![p0_deck, p1_deck],
            70451,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), grave_exile, vec![])
            .unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), leg_indest, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), leg_indest, vec![]).unwrap();
        drv.pass_priority();
        drv.pass_priority();
        if let DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                },
        } = drv.decision()
        {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: candidates[0] },
                },
            )
            .unwrap();
        }
        assert!(drv.find_exile_object(PlayerId(0), leg_indest).is_some());
    }
}

#[test]
fn synth_cr704_8_pre_annihilation_lki_undying_and_simultaneous_draw() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let minus_cid = b.intern_counter("-1/-1");
    let plains = b.add_synth_basic_land("synth_pl_7048", "Synth Plains", "Plains", ManaColor::W);
    let undying_2_2 =
        b.add_synth_keyword_creature("synth_und_7048", "Synth Phoenix", "{0}", 2, 2, &["undying"]);
    let put_plus1 = b.add_card("synth_p1_7048", "Synth PlusOne", |c| {
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
    let put_minus3 = b.add_card("synth_m3_7048", "Synth MinusThree", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(minus_cid),
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let poison_10 = b.add_card("synth_tox_7048", "Synth Toxic", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_player()],
                vec![effect_step(
                    EffectOp::PutPlayerCounters {
                        counter: PlayerCounterKind::Poison,
                    },
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 10 }),
                )],
            );
    });
    let pw = b.add_synth_planeswalker("synth_pw_7048", "Synth Walker", "{0}", 2);
    let battle = b.add_synth_battle("synth_bat_7048", "Synth Siege", "{0}", 2);
    let nuke_all = b.add_card("synth_nuke_7048", "Synth BoardBurn", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Damage {
                        source: None,
                        also_players: Some(PlayerFilter {
                            relation: PlayerRelation::EachPlayer,
                        }),
                        divided: false,
                        excess_to_controller: false,
                        excess_to_controller_if_trample: false,
                        or_planeswalker_controlled_by: None,
                        damage_attacked_target_by_self: false,
                        soulfire_exile_damage_per_target: false,
                        each_creature_deals: None,
                    },
                    Selector::All {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE
                                | CardTypeMask::PLANESWALKER
                                | CardTypeMask::BATTLE,
                            ..Default::default()
                        }),
                    },
                    Some(Amount::Const { n: 20 }),
                )],
            );
    });
    let db = b.build_db();

    // 1) CR 704.8 pre-annihilation LKI: 2/2 Undying + +1/+1 counter receives three -1/-1 counters ->
    //    704.5f & 704.5q apply simultaneously -> Undying sees +1/+1 counter in pre-annihilation LKI -> stays in GY!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                undying_2_2,
                put_plus1,
                put_minus3,
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
            7048,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), undying_2_2, vec![])
            .unwrap();
        drv.resolve_all_stack(8);
        let u_bf = drv
            .find_battlefield_object(PlayerId(0), undying_2_2)
            .unwrap();
        drv.cast_from_hand(PlayerId(0), put_plus1, vec![TargetRef::Object { id: u_bf }])
            .unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(
            PlayerId(0),
            put_minus3,
            vec![TargetRef::Object { id: u_bf }],
        )
        .unwrap();
        drv.resolve_all_stack(8);

        assert!(
            drv.find_battlefield_object(PlayerId(0), undying_2_2)
                .is_none()
        );
        assert!(
            drv.find_graveyard_object(PlayerId(0), undying_2_2)
                .is_some(),
            "CR 704.8: Undying must NOT trigger when +1/+1 and -1/-1 counters annihilate simultaneously with 0 toughness"
        );
    }

    // 2) CR 704.5i PW 0 loyalty, Battle 0 defense, and CR 104.4a simultaneous all-players-lose -> GameOver { winner: None }!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![pw, battle, nuke_all, plains, plains, plains, plains],
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
            70481,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(0), pw, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), battle, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), nuke_all, vec![]).unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.decision(),
            DecisionState::GameOver { winner: None },
            "CR 104.4a: simultaneous lethal damage to all players is a draw"
        );
    }

    // 3) CR 704.5c poison >= 10 causes SBA loss
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![poison_10; 7],
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
            70482,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        drv.cast_from_hand(
            PlayerId(0),
            poison_10,
            vec![TargetRef::Player { id: PlayerId(1) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.decision(),
            DecisionState::GameOver {
                winner: Some(PlayerId(0))
            }
        );
    }
}

#[test]
fn synth_pausable_vm_scry_search_optional_and_bind_as_lki() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_vm1", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_vm1", "Synth Bear", "{0}", 4, 2);

    // Multi-step spell:
    // Step 0: Scry 2 (pauses for prompt!)
    // Step 1: Optional (YesNo): Search library for a Land to Hand (pauses for prompt!)
    // Step 2: Destroy target creature (bind_as = "victim") -> captures LKI
    // Step 3: Gain life equal to victim's LKI power (Amount::Stat { of: Ref::Binding("victim"), stat: Power, lki: true })
    let scry_step = effect_step(
        EffectOp::Scry,
        Selector::PlayerRef {
            player: PlayerRef::You,
        },
        Some(Amount::Const { n: 2 }),
    );
    let mut opt_search = effect_step(
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
                shuffle_only_if_library: false,
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
    );
    opt_search.optional = Some(OptionalSpec {
        chooser: PlayerRef::You,
        cost: vec![],
        mode: OptionalMode::DoIfPaid,
    });
    let mut destroy_bind = effect_step(
        EffectOp::Destroy {
            cant_be_regenerated: false,
            select: Default::default(),
        },
        Selector::Target { index: 0 },
        None,
    );
    destroy_bind.bind_as = Some("victim".to_string());
    let lki_heal = effect_step(
        EffectOp::GainLife {
            set_life_total: false,
        },
        Selector::PlayerRef {
            player: PlayerRef::You,
        },
        Some(Amount::Stat {
            of: Ref::Binding {
                name: "victim".to_string(),
            },
            stat: mtg_ir::StatKind::Power,
            lki: true,
        }),
    );

    let script_spell = b.add_card("synth_script_vm1", "Synth OmenScript", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![scry_step, opt_search, destroy_bind, lki_heal],
            );
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            bear,
            script_spell,
            plains,
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
    let mut drv = SessionDriver::new(
        db.clone(),
        GameConfig::standard_1v1(),
        vec![p0_deck, p1_deck],
        7101,
    )
    .unwrap();
    drv.keep_all_hands();
    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 32);
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), script_spell).is_some()
        {
            break;
        }
    }

    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let hand_before = drv.session.state().player(PlayerId(0)).unwrap().hand.len();

    drv.cast_from_hand(
        PlayerId(0),
        script_spell,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // Answer all interactive prompts (Scry, Optional YesNo, Search PickObject) & verify mid-frame snapshot parity!
    let mut saw_prompt = false;
    for step_idx in 0..16 {
        let dec = drv.decision();
        if !matches!(dec, DecisionState::Prompt { .. }) {
            break;
        }
        saw_prompt = true;
        if step_idx == 0 {
            assert_snapshot_equivalence(&drv.session, &db, &[]);
        }
        let legal = drv.session.legal_actions(PlayerId(0));
        let chosen = legal
            .into_iter()
            .find(|a| {
                matches!(
                    a,
                    Action::Answer {
                        answer: ChoiceAnswer::YesNo { value: true }
                            | ChoiceAnswer::PickObject { .. }
                            | ChoiceAnswer::OrderNext { .. },
                        ..
                    }
                )
            })
            .unwrap_or_else(|| {
                drv.session
                    .legal_actions(PlayerId(0))
                    .into_iter()
                    .find(|a| !matches!(a, Action::Concede))
                    .unwrap()
            });
        drv.step(PlayerId(0), chosen).unwrap();
    }
    assert!(saw_prompt);
    assert!(drv.find_battlefield_object(PlayerId(0), bear).is_none());
    // Bear had power 4 -> LKI GainLife gained 4 -> 24 life, and searched 1 Plains into hand!
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 24);
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().hand.len(),
        hand_before // -1 spell +1 searched Plains
    );
}

#[test]
fn synth_pausable_vm_selection_pick_and_batch_replacement_continuations() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let plains = b.add_synth_basic_land("synth_pl_vm2", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_vm2", "Synth Bear", "{0}", 2, 2);

    // Two competing counter multipliers so PutCounters & Proliferate trigger ChooseReplacement pauses!
    let scales = b.add_card("synth_scales_vm2", "Synth Scales", |c| {
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
    let doubler = b.add_card("synth_cdbl_vm2", "Synth Doubler", |c| {
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
    // Spell 1: SelectionPick PutCounters ("Choose a creature you control, put 1 +1/+1 counter on it")
    let pick_counter = b.add_card("synth_pickc_vm2", "Synth SelectCounter", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(plus_cid),
                        multi_counters: vec![],
                        select: SelectionMod {
                            choice_count: Some(Box::new(Amount::Const { n: 1 })),
                            up_to: false,
                            at_random: false,
                            filter: Some(Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            })),
                            chooser: None,
                        },
                        divided: false,
                        allow_exile_counters: false,
                        remove_instead_of_add: false,
                        copy_counters_from: None,
                        choose_counter_on_subject_and_add_one: false,
                        conditional_counter_on_type: None,
                        cast_bolas_bonus_loyalty: false,
                    },
                    Selector::All {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    // Spell 2: Proliferate (multi-object + player energy counter) under competing multipliers!
    let give_energy = b.add_card("synth_en_vm2", "Synth Spark", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::GainEnergy,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let prolif = b.add_card("synth_prol_vm2", "Synth Proliferate", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(EffectOp::Proliferate, Selector::None, None)],
            );
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            bear,
            bear,
            scales,
            doubler,
            pick_counter,
            give_energy,
            prolif,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 7102).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    for c in [bear, bear, scales, doubler, give_energy] {
        drv.cast_from_hand(PlayerId(0), c, vec![]).unwrap();
        drv.resolve_all_stack(8);
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
    assert_eq!(bears.len(), 2);

    // 1) SelectionPick PutCounters pauses for ChooseObjects, then ChooseReplacement (Scales+Doubler)!
    drv.cast_from_hand(PlayerId(0), pick_counter, vec![])
        .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let pid_sel = match drv.decision() {
        DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                },
        } => {
            assert_eq!(candidates.len(), 2);
            prompt_id
        }
        other => panic!("expected SelectionPick ChooseObjects, got {other:?}"),
    };
    drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id: pid_sel,
            answer: ChoiceAnswer::PickObject { id: bears[0] },
        },
    )
    .unwrap();
    // ChooseReplacement pause inside SelectionPick continuation!
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
        panic!("expected ChooseReplacement during SelectionPick PutCounters");
    }
    assert_eq!(
        drv.session.state().counter_count(bears[0], plus_cid),
        4,
        "(1+1)*2 = 4 counters on picked bear"
    );
    assert_eq!(drv.session.state().counter_count(bears[1], plus_cid), 0);

    // 2) Proliferate pauses for ChooseObjects (bear0 + P0), then ChooseReplacement on bear0, then completes energy!
    drv.cast_from_hand(PlayerId(0), prolif, vec![]).unwrap();
    drv.pass_priority();
    drv.pass_priority();
    while let DecisionState::Prompt { request } = drv.decision() {
        match request {
            ChoiceRequest::ChooseObjects {
                prompt_id,
                candidates,
                ..
            } => {
                for &cid in &candidates {
                    if let DecisionState::Prompt {
                        request: ChoiceRequest::ChooseObjects { prompt_id: cur, .. },
                    } = drv.decision()
                        && cur == prompt_id
                    {
                        drv.step(
                            PlayerId(0),
                            Action::Answer {
                                prompt_id,
                                answer: ChoiceAnswer::PickObject { id: cid },
                            },
                        )
                        .unwrap();
                    }
                }
                if let DecisionState::Prompt {
                    request: ChoiceRequest::ChooseObjects { prompt_id: cur, .. },
                } = drv.decision()
                    && cur == prompt_id
                {
                    drv.step(
                        PlayerId(0),
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::DonePicking,
                        },
                    )
                    .unwrap();
                }
            }
            ChoiceRequest::ChooseMode { prompt_id, .. } => {
                drv.step(
                    PlayerId(0),
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickMode { index: 0 },
                    },
                )
                .unwrap();
            }
            ChoiceRequest::ChooseReplacement { prompt_id, .. } => {
                drv.step(
                    PlayerId(0),
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickReplacement { index: 0 },
                    },
                )
                .unwrap();
            }
            other => panic!("unexpected prompt in Proliferate: {other:?}"),
        }
    }
    assert!(drv.session.state().counter_count(bears[0], plus_cid) > 4);
    assert!(drv.session.state().player(PlayerId(0)).unwrap().energy >= 2);
}

#[test]
fn synth_pausable_vm_choose_designation_color_number_and_pay_or_discard() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_vm3", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_vm3", "Synth Bear", "{0}", 2, 2);

    // Spell with ChooseDesignation (Color) + PunisherChoice ("Pay 3 life or discard 1 card")
    let desig_spell = b.add_card("synth_desig_vm3", "Synth Decree", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![
                    effect_step(
                        EffectOp::ChooseDesignation {
                            kind: DesignationKind::Color,
                        },
                        Selector::None,
                        None,
                    ),
                    effect_step(
                        EffectOp::PunisherChoice {
                            spec: PunisherSpec {
                                players: Some(PunisherPlayersKind::SubjectPlayer),
                                default_steps: vec![effect_step(
                                    EffectOp::Discard {
                                        select: Default::default(),
                                    },
                                    Selector::PlayerRef {
                                        player: PlayerRef::You,
                                    },
                                    Some(Amount::Const { n: 1 }),
                                )],
                                unless_cost: vec![CostDef::PayLife {
                                    amount: Box::new(Amount::Const { n: 3 }),
                                    half_life_rounded_up: false,
                                }],
                                unless_effect: vec![],
                                unless_who: PlayerRef::Target { index: 0 },
                                repeat_count: None,
                                may_first: false,
                                mandatory_if_able: false,
                                track_not_done_kind: None,
                                track_done_kind: None,
                            },
                        },
                        Selector::PlayerRef {
                            player: PlayerRef::You,
                        },
                        None,
                    ),
                ],
            );
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![desig_spell, bear, plains, plains, plains, plains, plains],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 7103).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    drv.cast_from_hand(PlayerId(0), desig_spell, vec![])
        .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // 1) ChooseDesignation (Color) -> ChooseMode prompt! Verify out-of-range PickMode(99) fails cleanly!
    let pid_c = match drv.decision() {
        DecisionState::Prompt {
            request: ChoiceRequest::ChooseMode { prompt_id, .. },
        } => prompt_id,
        other => panic!("expected ChooseMode for ChooseDesignation(Color), got {other:?}"),
    };
    let err = drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id: pid_c,
            answer: ChoiceAnswer::PickMode { index: 99 },
        },
    );
    assert!(matches!(err, Err(EngineError::IllegalAction(_))));
    assert_eq!(
        drv.decision(),
        DecisionState::Prompt {
            request: ChoiceRequest::ChooseMode {
                prompt_id: pid_c,
                option_count: 5,
                min: 1,
                max: 1
            }
        }
    );
    drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id: pid_c,
            answer: ChoiceAnswer::PickMode { index: 0 },
        },
    )
    .unwrap();

    // 2) PunisherChoice -> ChooseMode / YesNo prompt! Answer to pay 3 life -> life drops to 17!
    while let DecisionState::Prompt { .. } = drv.decision() {
        let legal = drv.session.legal_actions(PlayerId(0));
        let pick = legal
            .into_iter()
            .find(|a| !matches!(a, Action::Concede))
            .unwrap();
        drv.step(PlayerId(0), pick).unwrap();
    }
    assert!(
        drv.session.state().player(PlayerId(0)).unwrap().life == 17
            || drv
                .session
                .state()
                .player(PlayerId(0))
                .unwrap()
                .graveyard
                .len()
                >= 2,
        "PunisherChoice resolved either PayLife(3) or Discard(1)"
    );
}
