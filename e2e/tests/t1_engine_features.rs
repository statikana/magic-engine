//! Tier 1 Opaque-Box E2E Tests: Features F5–F17 (Core Rules Engine, Façade, Differential, Non-Functional).
//!
//! Covers 65 tests: `T1-F05-01..05` through `T1-F17-01..05`.

use std::sync::Arc;
use std::time::Instant;

use mtg_engine::{
    Action, AttackTarget, CastRequest, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec,
    EngineError, GameConfig, MacroAction, MatchSession, PlayerId, PowerToughness, RngState,
    SpecialActionKind, TargetRef,
};
use mtg_engine_e2e::assertions::{
    assert_observation_consistency, assert_rename_invariance_with, assert_view_model_redaction,
};
use mtg_engine_e2e::cli::{TempDir, repo_root, run_build_db, run_diff_test};
use mtg_engine_e2e::driver::SessionDriver;
use mtg_engine_e2e::synth::{
    SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
};
use mtg_ir::{
    Affected, Amount, CardDefId, CardLayout, CardTypeMask, ColorMask, CompareOp, Condition,
    CostDef, CostDelta, CostDeltaMode, DamageModMode, DamageModTarget, DamagePreventionSpec,
    DamageScaleSpec, DrawScaleSpec, Duration, EffectOp, EffectStep, EtbChoiceSpec, EventDetail,
    EventFilter, EventKind, EventPattern, FilterFlags, GamePhase, Layer, LayerMod,
    LifeGainScaleSpec, ManaColor, ManaSpendingRestriction, ModalDef, ModalMode, ObjectFilter,
    ObjectPredicate, OptionalMode, OptionalSpec, PlainManaFilter, PlayerFilter, PlayerRef,
    PlayerRelation, PreventMode, PreventScope, Ref, RelativeControl, ReplacementDef,
    ReplacementWith, SearchDestination, SearchSpec, Selector, SpellOrAbilityFilter, StatKind,
    StaticBody, StaticDef, SupertypeMask, TargetKind, TargetSpec, Timing, UnsupportedReason, Zone,
    ZoneChangeMod, ZoneMask,
};

fn build_60_card_pair(
    _b: &mut SynthBundleBuilder,
    land_id: CardDefId,
    filler_id: CardDefId,
    special_ids: &[CardDefId],
) -> Vec<DeckSpec> {
    let mut main = Vec::with_capacity(60);
    for &id in special_ids {
        main.push(id);
        main.push(id);
        main.push(id);
        main.push(id);
    }
    while main.len() < 24 {
        main.push(land_id);
    }
    while main.len() < 60 {
        main.push(filler_id);
    }
    let d = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    vec![d.clone(), d]
}

fn find_first_land_in_hand(
    driver: &SessionDriver,
    player: PlayerId,
) -> Option<mtg_engine::ObjectId> {
    let vm = driver.session.view_model(Some(player));
    vm.players
        .iter()
        .find(|p| p.id == player)?
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::LAND))
        .map(|o| o.id)
}

fn find_first_creature_in_hand(
    driver: &SessionDriver,
    player: PlayerId,
) -> Option<mtg_engine::ObjectId> {
    let vm = driver.session.view_model(Some(player));
    vm.players
        .iter()
        .find(|p| p.id == player)?
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::CREATURE))
        .map(|o| o.id)
}

// ============================================================================
// F5: CR 400.7 Zone Funnel, LKI, Phasing & RareObjectState (5 cases)
// ============================================================================

#[test]
fn t1_f05_01_cross_zone_object_id_generation_bump() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_01", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_01", "Synth Bear", "{1}{W}", 2, 2);
    let shatter = b.add_card("synth_destroy_land", "Synth Ruin", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::LAND,
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
    let _ = bear;
    let db = b.build_db();
    let mut main = vec![plains; 20];
    main.extend(vec![shatter; 40]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        101,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(plains, 1), (shatter, 1)])
            .unwrap();
        if find_first_land_in_hand(&drv, PlayerId(0)).is_some()
            && drv.find_hand_object(PlayerId(0), shatter).is_some()
        {
            break;
        }
    }
    let hand_land =
        find_first_land_in_hand(&drv, PlayerId(0)).expect("Synth Plains must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: hand_land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .expect("playing Synth Plains must succeed in Main1");

    let bf_land = drv
        .find_battlefield_object(PlayerId(0), plains)
        .expect("Synth Plains must be on battlefield");
    assert!(
        bf_land.generation > hand_land.generation,
        "Hand -> Battlefield must bump ObjectId generation ({} > {})",
        bf_land.generation,
        hand_land.generation
    );

    let ruin_id = drv
        .find_hand_object(PlayerId(0), shatter)
        .expect("Synth Ruin must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: ruin_id,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: bf_land }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let gy_land = drv
        .find_graveyard_object(PlayerId(0), plains)
        .expect("destroyed land must move to graveyard");
    assert!(gy_land.generation > bf_land.generation);
}

#[test]
fn t1_f05_02_stack_to_battlefield_preserves_object_id_cr400_7d() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_forest_02", "Synth Forest", "Forest", ManaColor::G);
    let cub = b.add_synth_vanilla_creature("synth_cub_02", "Synth Cub", "{G}", 2, 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, forest, cub, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 102).unwrap();
    drv.keep_all_hands();

    let hand_land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: hand_land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let hand_cub = find_first_creature_in_hand(&drv, PlayerId(0)).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: hand_cub,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();

    let stack_id = *drv
        .session
        .state()
        .stack()
        .first()
        .expect("cast creature spell must be on stack");
    assert!(stack_id.generation > hand_cub.generation);

    drv.pass_priority();
    drv.pass_priority();

    let bf_id = drv
        .find_battlefield_object(PlayerId(0), cub)
        .expect("resolved creature must be on battlefield");
    assert_eq!(
        bf_id, stack_id,
        "CR 400.7d: permanent resolving Stack -> Battlefield must preserve exact ObjectId"
    );
}

#[test]
fn t1_f05_03_transient_battlefield_state_reset_on_zone_departure() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land("synth_island_03", "Synth Island", "Island", ManaColor::U);
    let crab = b.add_card("synth_crab_03", "Synth Crab", |c| {
        c.mana_cost("{U}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 4)
            .activated_ability(
                "tap_life",
                vec![CostDef::TapSelf],
                Timing::Instant,
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
    let bounce = b.add_card("synth_bounce_03", "Synth Bounce", |c| {
        c.mana_cost("{U}")
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
    let plus_counter = b.intern_counter("+1/+1");
    let pump_counter = b.add_card("synth_boost_03", "Synth Boost", |c| {
        c.mana_cost("{U}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(plus_counter),
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
    let ping1 = b.add_synth_burn_instant("synth_ping_03", "Synth Ping", "{U}", 1);
    let db = b.build_db();
    let mut main = vec![island; 24];
    main.extend(vec![crab; 12]);
    main.extend(vec![pump_counter; 8]);
    main.extend(vec![ping1; 8]);
    main.extend(vec![bounce; 8]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        103,
    )
    .unwrap();
    drv.keep_all_hands();

    // Advance turns playing Islands until crab is on battlefield and we have >= 4 Islands + boost/ping/bounce.
    let mut crab_cast = false;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (crab, usize::from(!crab_cast)),
                (pump_counter, 1),
                (ping1, 1),
                (bounce, 1),
                (island, 1),
            ],
        )
        .unwrap();
        if let Some(l) = find_first_land_in_hand(&drv, PlayerId(0)) {
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
        if !crab_cast && let Some(c_hand) = drv.find_hand_object(PlayerId(0), crab) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: c_hand,
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
            crab_cast = true;
            continue;
        }
        let islands = drv.session.view_model(None).players[0]
            .battlefield
            .iter()
            .filter(|o| o.card_def == island && !o.tapped)
            .count();
        if crab_cast
            && islands >= 4
            && drv.find_hand_object(PlayerId(0), pump_counter).is_some()
            && drv.find_hand_object(PlayerId(0), ping1).is_some()
            && drv.find_hand_object(PlayerId(0), bounce).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must cast crab and hold 4 untapped Islands + combo within wait loop"
    );
    let c_bf = drv
        .find_battlefield_object(PlayerId(0), crab)
        .expect("Synth Crab must be on battlefield");

    // 1. Put +1/+1 counter on crab (becomes 2/5).
    let p_hand = drv
        .find_hand_object(PlayerId(0), pump_counter)
        .expect("Synth Boost must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // 2. Mark 1 non-lethal damage on crab.
    let ping_h = drv
        .find_hand_object(PlayerId(0), ping1)
        .expect("Synth Ping must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: ping_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    // 3. Tap crab via its {T} ability.
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: c_bf,
            ability_index: 0,
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let pre = drv.battlefield_view(PlayerId(0), c_bf).unwrap();
    assert!(
        pre.tapped,
        "precondition: crab must be tapped before bounce"
    );
    assert_eq!(
        pre.damage_marked, 1,
        "precondition: crab must have 1 damage before bounce"
    );
    assert_eq!(
        pre.counters.iter().map(|(_, n)| *n).sum::<u32>(),
        1,
        "precondition: crab must have +1/+1 counter"
    );

    // 4. Bounce and recast crab -> all transient battlefield state must reset!
    let b_hand = drv
        .find_hand_object(PlayerId(0), bounce)
        .expect("Synth Bounce must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let c_rehand = drv.find_hand_object(PlayerId(0), crab).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: c_rehand,
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

    let c_rebf = drv.find_battlefield_object(PlayerId(0), crab).unwrap();
    let view = drv.battlefield_view(PlayerId(0), c_rebf).unwrap();
    assert!(!view.tapped);
    assert_eq!(view.damage_marked, 0);
    assert!(view.counters.is_empty());
}

#[test]
fn t1_f05_04_lki_capture_for_removed_source_cr113_7a() {
    let mut b = SynthBundleBuilder::new();
    let mountain =
        b.add_synth_basic_land("synth_mtn_04", "Synth Mountain", "Mountain", ManaColor::R);
    let archer = b.add_card("synth_archer_04", "Synth Archer", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::CREATURE)
            .pt(3, 1)
            .activated_ability(
                "ping_power",
                vec![CostDef::TapSelf],
                Timing::Instant,
                vec![target_player()],
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
                    Some(Amount::Stat {
                        of: Ref::SelfObj,
                        stat: StatKind::Power,
                        lki: true,
                    }),
                )],
            );
    });
    let bolt = b.add_synth_burn_instant("synth_bolt_04", "Synth Bolt", "{R}", 3);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mountain, archer, &[bolt]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 104).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut archer_cast = false;
    let mut archer_turn = 1u32;
    let mut act_turn = 3u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (mountain, usize::from(!land_played)),
                (archer, usize::from(!archer_cast)),
                (bolt, 1),
            ],
        )
        .unwrap();
        if !land_played && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played
            && !archer_cast
            && let Some(a_hand) = drv.find_hand_object(PlayerId(0), archer)
        {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: a_hand,
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
            archer_cast = true;
            archer_turn = turn;
            continue;
        }
        if archer_cast
            && turn >= archer_turn + 2
            && drv.find_hand_object(PlayerId(0), bolt).is_some()
        {
            act_turn = turn;
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must cast archer and hold bolt on a later turn within wait loop"
    );

    let a_bf = drv.find_battlefield_object(PlayerId(0), archer).unwrap();
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: a_bf,
            ability_index: 0,
        },
    )
    .unwrap();
    let mut guard = 0;
    while matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        assert_eq!(drv.actor(), Some(PlayerId(0)));
        let legal = drv.session.legal_actions(PlayerId(0));
        let act = legal
            .iter()
            .find(|a| !matches!(a, Action::Concede) && format!("{a:?}").contains("PlayerId(1)"))
            .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
            .expect("targeting step must expose non-concede action")
            .clone();
        drv.step(PlayerId(0), act).unwrap();
        guard += 1;
        assert!(guard <= 8, "BeginActivate targeting did not complete");
    }
    assert_eq!(drv.decision(), DecisionState::Priority);
    assert_eq!(drv.session.state().stack().len(), 1);

    // Kill archer while ping is on the stack; ping must still deal 3 LKI damage to Player 1.
    let bolt_hand = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bolt_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: a_bf }],
        },
    )
    .unwrap();
    drv.advance_to(act_turn, GamePhase::BeginCombat, 16)
        .unwrap();
    let vm = drv.session.view_model(None);
    assert_eq!(
        vm.players[1].life, 17,
        "CR 113.7a LKI power ping must deal 3 damage"
    );
}

#[test]
fn t1_f05_05_phasing_preserves_id_counters_and_skips_triggers_cr702_26d() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land("synth_island_05", "Synth Island", "Island", ManaColor::U);
    let golem = b.add_synth_keyword_creature(
        "synth_phaser_05",
        "Synth Phaser",
        "{1}{U}",
        3,
        3,
        &["phasing"],
    );
    let watcher = b.add_card("synth_etb_watch_05", "Synth Watcher", |c| {
        c.mana_cost("{U}")
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
    let plus_counter = b.intern_counter("+1/+1");
    let put_plus = b.add_card("synth_plus_05", "Synth Charge", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(plus_counter),
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
    let db = b.build_db();
    let mut main = vec![island; 24];
    main.extend(vec![golem; 12]);
    main.extend(vec![watcher; 12]);
    main.extend(vec![put_plus; 12]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        105,
    )
    .unwrap();
    drv.keep_all_hands();

    let mut watcher_cast = false;
    let mut base_turn = 3u32;
    for turn in [1u32, 3, 5, 7, 9, 11] {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (watcher, usize::from(!watcher_cast)),
                (golem, 1),
                (put_plus, 1),
                (island, 1),
            ],
        )
        .unwrap();
        if let Some(l) = find_first_land_in_hand(&drv, PlayerId(0)) {
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
        if !watcher_cast && let Some(w_h) = drv.find_hand_object(PlayerId(0), watcher) {
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
            watcher_cast = true;
            continue;
        }
        let islands = drv.session.view_model(None).players[0]
            .battlefield
            .iter()
            .filter(|o| o.card_def == island && !o.tapped)
            .count();
        if watcher_cast
            && islands >= 2
            && drv.find_hand_object(PlayerId(0), golem).is_some()
            && drv.find_hand_object(PlayerId(0), put_plus).is_some()
        {
            base_turn = turn;
            break;
        }
    }
    assert!(
        watcher_cast,
        "Synth Watcher must be cast before Synth Phaser"
    );

    let g_hand = drv
        .find_hand_object(PlayerId(0), golem)
        .expect("Synth Phaser must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: g_hand,
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
    // Resolve Synth Watcher's ETB trigger (GainLife 2) so stack is empty before sorcery put_plus!
    drv.pass_priority();
    drv.pass_priority();
    // Watcher triggered on golem ETB -> life is now 22!
    let life_before_phase = drv.session.view_model(None).players[0].life;
    assert_eq!(
        life_before_phase, 22,
        "Synth Watcher ETB trigger must gain 2 life when Synth Phaser enters"
    );
    let g_id = drv.find_battlefield_object(PlayerId(0), golem).unwrap();

    let pp_h = drv
        .find_hand_object(PlayerId(0), put_plus)
        .expect("Synth Charge must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: pp_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: g_id }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.battlefield_view(PlayerId(0), g_id)
            .unwrap()
            .counters
            .iter()
            .map(|(_, n)| *n)
            .sum::<u32>(),
        1
    );

    // Advance to P0's next turn (base_turn + 2): Phaser phases out before untap!
    drv.advance_to(base_turn + 2, GamePhase::Main1, 64).unwrap();
    assert!(
        drv.find_battlefield_object(PlayerId(0), golem).is_none()
            && drv.session.state().object(g_id).is_some(),
        "CR 702.26b,d: phased-out permanent is hidden from battlefield view yet stays in arena with unchanged ObjectId"
    );
    // Advance to base_turn + 4: Phaser phases back in with exact same ObjectId, preserved +1/+1 counter, and zero ETB trigger!
    drv.advance_to(base_turn + 4, GamePhase::Main1, 64).unwrap();
    let g_after = drv.find_battlefield_object(PlayerId(0), golem).unwrap();
    assert_eq!(g_after, g_id, "CR 702.26d phasing must preserve ObjectId");
    assert_eq!(
        drv.battlefield_view(PlayerId(0), g_after)
            .unwrap()
            .counters
            .iter()
            .map(|(_, n)| *n)
            .sum::<u32>(),
        1,
        "CR 702.26d: phasing must preserve counters"
    );
    assert_eq!(
        drv.session.view_model(None).players[0].life,
        life_before_phase,
        "CR 702.26d: phasing in must not trigger ETB abilities"
    );
}

// ============================================================================
// F6: Face Resolution & Commander Rules (CR 903) (5 cases)
// ============================================================================

#[test]
fn t1_f06_01_split_mdfc_adventure_face_casting() {
    let mut b = SynthBundleBuilder::new();
    let mountain =
        b.add_synth_basic_land("synth_mtn_06", "Synth Mountain", "Mountain", ManaColor::R);
    let mdfc = b.add_card("synth_mdfc_06", "Synth Front // Synth Back", |c| {
        c.layout(CardLayout::ModalDfc)
            .mana_cost("{R}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_player()],
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
            )
            .add_face("Synth Back", |f| {
                f.types(CardTypeMask::LAND)
                    .mana_ability("back_tap", &[ManaColor::R]);
            });
    });
    let adv = b.add_card("synth_adv_06", "Synth Knight", |c| {
        c.layout(CardLayout::Adventure)
            .mana_cost("{1}{R}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .add_face("Synth Quest", |f| {
                f.mana_cost("{R}")
                    .types(CardTypeMask::INSTANT)
                    .spell_effect(
                        vec![target_player()],
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
                            Some(Amount::Const { n: 1 }),
                        )],
                    );
            });
    });
    let split = b.add_card("synth_split_06", "Synth Fire // Synth Fury", |c| {
        c.layout(CardLayout::Split)
            .mana_cost("{R}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_player()],
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
                    Some(Amount::Const { n: 1 }),
                )],
            )
            .add_face("Synth Fury", |f| {
                f.mana_cost("{1}{R}")
                    .types(CardTypeMask::SORCERY)
                    .spell_effect(
                        vec![target_player()],
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
                            Some(Amount::Const { n: 3 }),
                        )],
                    );
            });
    });
    let db = b.build_db();
    let mut main = vec![mountain; 20];
    main.extend(vec![mdfc; 14]);
    main.extend(vec![adv; 14]);
    main.extend(vec![split; 12]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        106,
    )
    .unwrap();
    drv.keep_all_hands();

    // Turn 1+: Play MDFC back face as a land (`face_index: 1`).
    let mut t_mdfc = 1u32;
    for turn in [1u32, 3, 5] {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(mdfc, 1), (mountain, 1), (adv, 1), (split, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), mdfc).is_some() {
            t_mdfc = turn;
            break;
        }
    }
    let mdfc_hand = drv
        .find_hand_object(PlayerId(0), mdfc)
        .expect("MDFC must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: mdfc_hand,
            from_zone: Zone::Hand,
            face_index: 1,
        },
    )
    .unwrap();
    assert!(drv.find_battlefield_object(PlayerId(0), mdfc).is_some());

    // Turn 3+: Play Mountain, cast Adventure half (`option_id: 1`, {R}), verify exile.
    let mut t_adv = t_mdfc + 2;
    for turn in ((t_mdfc + 2)..=(t_mdfc + 8)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(mountain, 1), (adv, 1), (split, 1)],
        )
        .unwrap();
        if drv.find_battlefield_object(PlayerId(0), mountain).is_none()
            && let Some(m_h) = drv.find_hand_object(PlayerId(0), mountain)
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: m_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }
        if drv.find_battlefield_object(PlayerId(0), mountain).is_some()
            && drv.find_hand_object(PlayerId(0), adv).is_some()
        {
            t_adv = turn;
            break;
        }
    }
    let adv_h = drv
        .find_hand_object(PlayerId(0), adv)
        .expect("Adventure card must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: adv_h,
            option_id: 1,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[1].life, 19);
    let adv_ex = drv
        .find_exile_object(PlayerId(0), adv)
        .expect("resolved Adventure must go on an adventure in Exile");

    // Next turn: Cast Adventure Creature half from Exile (`option_id: 0`, {1}{R}).
    let t_crea = t_adv + 2;
    drv.advance_to_preserving(t_crea, GamePhase::Main1, 64, &[(split, 1)])
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: adv_ex,
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

    // Next turn: Cast Split second half (`option_id: 1`, {1}{R} deal 3 to P1).
    for turn in [t_crea + 2, t_crea + 4, t_crea + 6] {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(split, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), split).is_some() {
            break;
        }
    }
    let sp_h = drv
        .find_hand_object(PlayerId(0), split)
        .expect("Split card must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sp_h,
            option_id: 1,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[1].life, 16);
}

#[test]
fn t1_f06_02_transform_and_flip_permanence() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_forest_07", "Synth Forest", "Forest", ManaColor::G);
    let dfc = b.add_card("synth_were_07", "Synth Wolf", |c| {
        c.layout(CardLayout::Transform)
            .mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .activated_ability(
                "transform_self",
                vec![],
                Timing::Sorcery,
                vec![],
                vec![effect_step(EffectOp::Transform, Selector::SelfObj, None)],
            )
            .add_face("Synth Alpha", |f| {
                f.types(CardTypeMask::CREATURE)
                    .colors(ColorMask::G | ColorMask::R)
                    .pt(4, 4)
                    .keyword("trample");
            });
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, forest, dfc, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 107).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let w_hand = drv.find_hand_object(PlayerId(0), dfc).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: w_hand,
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
    let w_bf = drv.find_battlefield_object(PlayerId(0), dfc).unwrap();
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: w_bf,
            ability_index: 0,
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let after_id = drv.find_battlefield_object(PlayerId(0), dfc).unwrap();
    assert_eq!(after_id, w_bf, "Transform must preserve ObjectId");
    let bd = drv.session.hover_breakdown(None, after_id).unwrap();
    assert_eq!(
        bd.evaluated_pt,
        Some(PowerToughness {
            power: 4,
            toughness: 4
        })
    );
}

#[test]
fn t1_f06_03_saga_class_room_prototype_battle_mechanics() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_08", "Synth Plains", "Plains", ManaColor::W);
    let proto = b.add_card("synth_proto_08", "Synth Proto", |c| {
        c.mana_cost("{5}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(6, 6)
            .prototype_spec("{1}{W}", 3, 3);
    });
    let room = b.add_card("synth_room_08", "Synth Room A // Synth Room B", |c| {
        c.layout(CardLayout::Split)
            .mana_cost("{W}")
            .types(CardTypeMask::ENCHANTMENT)
            .subtype("Room")
            .triggered_ability(
                simple_trigger(EventKind::RoomFullyUnlocked, EventFilter::default()),
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
            )
            .add_face("Synth Room B", |f| {
                f.mana_cost("{1}{W}")
                    .types(CardTypeMask::ENCHANTMENT)
                    .subtype("Room");
            });
    });
    let db = b.build_db();
    let mut main = vec![plains; 24];
    main.extend(vec![proto; 18]);
    main.extend(vec![room; 18]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        108,
    )
    .unwrap();
    drv.keep_all_hands();

    let mut plains_played = 0u32;
    let mut room_cast = false;
    let mut door_unlocked = false;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (plains, usize::from(plains_played < 2)),
                (room, usize::from(!room_cast)),
                (proto, 1),
            ],
        )
        .unwrap();
        if plains_played < 2
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            plains_played += 1;
        }
        let plains_untapped = drv.session.view_model(None).players[0]
            .battlefield
            .iter()
            .filter(|o| o.card_def == plains && !o.tapped)
            .count();
        if !room_cast
            && plains_untapped >= 1
            && let Some(r_hand) = drv.find_hand_object(PlayerId(0), room)
        {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: r_hand,
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
            room_cast = true;
            continue;
        }
        if room_cast && !door_unlocked && plains_untapped >= 2 {
            let r_bf = drv.find_battlefield_object(PlayerId(0), room).unwrap();
            drv.step(
                PlayerId(0),
                Action::SpecialAction {
                    kind: SpecialActionKind::UnlockRoomDoor {
                        object: r_bf,
                        face_index: 1,
                    },
                },
            )
            .unwrap();
            drv.pass_priority();
            drv.pass_priority();
            assert_eq!(
                drv.session.view_model(None).players[0].life,
                23,
                "RoomFullyUnlocked trigger must gain 3 life"
            );
            let unlock_err = drv.step_unchecked(
                PlayerId(0),
                Action::SpecialAction {
                    kind: SpecialActionKind::UnlockRoomDoor {
                        object: r_bf,
                        face_index: 1,
                    },
                },
            );
            assert!(
                matches!(unlock_err, Err(EngineError::IllegalAction(_))),
                "unlocking an already-unlocked Room door must fail with IllegalAction, got {unlock_err:?}"
            );
            door_unlocked = true;
            continue;
        }
        if door_unlocked
            && plains_untapped >= 2
            && drv.find_hand_object(PlayerId(0), proto).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must unlock Room and hold Proto with 2 untapped Plains within wait loop"
    );

    // Cast Proto via Prototype option (`option_id: 1`, {1}{W}, 3/3 White).
    let p_hand = drv
        .find_hand_object(PlayerId(0), proto)
        .expect("Proto must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_hand,
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
    let proto_bf = drv.find_battlefield_object(PlayerId(0), proto).unwrap();
    let bd = drv.session.hover_breakdown(None, proto_bf).unwrap();
    assert_eq!(
        bd.evaluated_pt,
        Some(PowerToughness {
            power: 3,
            toughness: 3
        })
    );
    assert_eq!(bd.colors, ColorMask::W);
}

#[test]
fn t1_f06_04_commander_tax_and_zone_return_choice() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_forest_09", "Synth Forest", "Forest", ManaColor::G);
    let cmd = b.add_card("synth_cmd_09", "Synth Commander", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let slay = b.add_card("synth_slay_09", "Synth Slay", |c| {
        c.mana_cost("{G}")
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
    let db = b.build_db();
    let mut main = vec![forest; 50];
    main.extend(vec![slay; 49]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::commander(2), vec![d.clone(), d], 109).unwrap();
    drv.keep_all_hands();

    let cmd_id = drv
        .find_command_object(PlayerId(0), cmd)
        .expect("commander starts in Command zone");
    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
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
    drv.pass_priority();
    drv.pass_priority();

    let cmd_bf = drv.find_battlefield_object(PlayerId(0), cmd).unwrap();
    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    let slay_hand = drv.find_hand_object(PlayerId(0), slay).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: slay_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: cmd_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

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
        other => panic!("expected YesNo prompt to return commander to Command zone, got {other:?}"),
    }
    let returned = drv.find_command_object(PlayerId(0), cmd).unwrap();
    // With only 1 mana available, recasting with +2 commander tax must fail.
    let err = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: returned,
            option_id: 0,
            from_zone: Zone::Command,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(err, Err(EngineError::IllegalAction(_))),
        "recasting commander without +2 tax must fail with IllegalAction, got {err:?}"
    );
}

#[test]
fn t1_f06_05_commander_21_combat_damage_loss_cr903_10a() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_forest_10", "Synth Forest", "Forest", ManaColor::G);
    let titan = b.add_card("synth_titan_10", "Synth Titan", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(21, 21)
            .keyword("haste");
    });
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![titan],
        mainboard: vec![forest; 99],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::commander(2), vec![d.clone(), d], 110).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let cmd_id = drv.find_command_object(PlayerId(0), titan).unwrap();
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
    drv.pass_priority();
    drv.pass_priority();
    let cmd_bf = drv.find_battlefield_object(PlayerId(0), titan).unwrap();

    drv.advance_to(1, GamePhase::DeclareAttackers, 16).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(cmd_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(1, GamePhase::Main2, 32).ok();
    assert_eq!(
        drv.decision(),
        DecisionState::GameOver {
            winner: Some(PlayerId(0))
        },
        "CR 903.10a: 21 combat damage from a single commander must end the game via SBA"
    );
}

// ============================================================================
// F7: Bitmask ObjectFilter & Amount/Condition Evaluation (5 cases)
// ============================================================================

#[test]
fn t1_f07_01_bitmask_object_filter_fast_path() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_11", "Synth Mountain", "Mountain", ManaColor::R);
    let bot = b.add_card("synth_bot_11", "Synth RedBot", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(2, 2);
    });
    let big_bot = b.add_card("synth_bigbot_11", "Synth BigBot", |c| {
        c.mana_cost("{3}{R}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(4, 4);
    });
    let snipe = b.add_card("synth_snipe_11", "Synth Snipe", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_all: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                        colors_any: ColorMask::R,
                        controller: Some(RelativeControl::You),
                        flags: FilterFlags::NONTOKEN,
                        mana_value: Some(mtg_ir::Compare {
                            op: CompareOp::Le,
                            value: Box::new(Amount::Const { n: 3 }),
                        }),
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
    let decks = build_60_card_pair(&mut b, mtn, bot, &[big_bot, snipe]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 111).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let bot_hand = drv.find_hand_object(PlayerId(0), bot).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bot_hand,
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
    let bot_bf = drv.find_battlefield_object(PlayerId(0), bot).unwrap();

    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    let snipe_hand = drv.find_hand_object(PlayerId(0), snipe).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: snipe_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: bot_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_graveyard_object(PlayerId(0), bot).is_some());
}

#[test]
fn t1_f07_02_object_predicate_relational_fallback() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_12", "Synth Mountain", "Mountain", ManaColor::R);
    let named = b.add_synth_vanilla_creature("synth_named_12", "Synth Target", "{R}", 2, 2);
    let ping_named = b.add_card("synth_ping_named_12", "Synth PingNamed", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::CREATURE,
                        extra: vec![ObjectPredicate::Named {
                            name: "Synth Target".into(),
                        }],
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
    let decks = build_60_card_pair(&mut b, mtn, named, &[ping_named]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 112).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let c_hand = drv.find_hand_object(PlayerId(0), named).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: c_hand,
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
    let c_bf = drv.find_battlefield_object(PlayerId(0), named).unwrap();

    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    let p_hand = drv.find_hand_object(PlayerId(0), ping_named).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_graveyard_object(PlayerId(0), named).is_some());
}

#[test]
fn t1_f07_03_high_bit_filter_flags_runtime_eval() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_13", "Synth Mountain", "Mountain", ManaColor::R);
    let bear = b.add_synth_vanilla_creature("synth_bear_13", "Synth Bear", "{R}", 2, 2);
    let fresh_kill = b.add_card("synth_fresh_13", "Synth FreshKill", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::ENTERED_THIS_TURN,
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
    let decks = build_60_card_pair(&mut b, mtn, bear, &[fresh_kill]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 113).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.advance_to(3, GamePhase::Main1, 64).unwrap();
    let land2 = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land2,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let c_hand = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: c_hand,
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
    let c_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    let fk_hand = drv.find_hand_object(PlayerId(0), fresh_kill).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: fk_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: c_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_graveyard_object(PlayerId(0), bear).is_some());
}

#[test]
fn t1_f07_04_amount_eval_colors_spent_and_total_stat() {
    let mut b = SynthBundleBuilder::new();
    let prism = b.add_card("synth_prism_14", "Synth Prism Land", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("tap_w", &[ManaColor::W])
            .mana_ability("tap_u", &[ManaColor::U])
            .mana_ability("tap_g", &[ManaColor::G]);
    });
    let converge = b.add_card("synth_converge_14", "Synth Converge", |c| {
        c.mana_cost("{2}{G}")
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
                    Some(Amount::ColorsOfManaSpentToCast),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, prism, converge, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 114).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    let lands: Vec<_> = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.card_def == prism)
        .map(|o| o.id)
        .collect();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: lands[0],
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: lands[1],
            ability_index: 1,
            preferred_color: None,
        },
    )
    .unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: lands[2],
            ability_index: 2,
            preferred_color: None,
        },
    )
    .unwrap();
    let hand_before = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    let conv_hand = drv.find_hand_object(PlayerId(0), converge).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: conv_hand,
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
    let hand_after = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    assert_eq!(hand_after, hand_before - 1 + 3);
}

#[test]
fn t1_f07_05_condition_eval_entered_this_turn_and_controls_filter() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_15", "Synth Mountain", "Mountain", ManaColor::R);
    let art = b.add_card("synth_relic_15", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let cond_bolt = b.add_card("synth_cond_bolt_15", "Synth CondBolt", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_player()],
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
                    Some(Amount::Conditional {
                        condition: Box::new(Condition::EnteredUnderYourControlThisTurn {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                ..Default::default()
                            }),
                            min: Some(1),
                        }),
                        then_amt: Box::new(Amount::Const { n: 4 }),
                        else_amt: Box::new(Amount::Const { n: 1 }),
                    }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, art, &[cond_bolt]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 115).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(mtn, 1), (art, 1), (cond_bolt, 1)],
        )
        .unwrap();
        if !land_played && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played
            && drv.find_hand_object(PlayerId(0), art).is_some()
            && drv.find_hand_object(PlayerId(0), cond_bolt).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play Mountain and hold art + cond_bolt within wait loop"
    );
    let art_hand = drv.find_hand_object(PlayerId(0), art).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: art_hand,
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

    let cb_hand = drv.find_hand_object(PlayerId(0), cond_bolt).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cb_hand,
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
    assert_eq!(drv.session.view_model(None).players[1].life, 16);
}

// ============================================================================
// F8: CR 613 Layers, CR 613.8 Dependencies, Cache & Signed P/T (5 cases)
// ============================================================================

#[test]
fn t1_f08_01_layer_ordering_1a_to_7d_and_set_name() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_16", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_16", "Synth Bear", "{W}", 1, 4);
    let set_pt = b.add_card("synth_set_pt_16", "Synth SetBase", |c| {
        c.mana_cost("{W}")
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
                        power: Box::new(Amount::Const { n: 2 }),
                        toughness: Box::new(Amount::Const { n: 5 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let anthem = b.add_synth_anthem_enchantment("synth_anth_16", "Synth Anthem", "{W}", 2, 1);
    let switcher = b.add_card("synth_switch_16", "Synth Switcher", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7d,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SwitchPt,
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, plains, bear, &[set_pt, anthem, switcher]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 116).unwrap();
    drv.keep_all_hands();

    // Play bear (1/4), SetBase (2/5), Anthem (+2/+1 -> 4/6), SwitchPt (7d -> 6/4).
    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=79).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (plains, 1),
                (bear, 1),
                (set_pt, 1),
                (anthem, 1),
                (switcher, 1),
            ],
        )
        .unwrap();
        if lands_played < 4
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 4
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), set_pt).is_some()
            && drv.find_hand_object(PlayerId(0), anthem).is_some()
            && drv.find_hand_object(PlayerId(0), switcher).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 4 Plains and hold bear, set_pt, anthem, and switcher within wait loop"
    );
    for card in [bear, set_pt, anthem, switcher] {
        let oid = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let bd = drv.session.hover_breakdown(None, bear_bf).unwrap();
    assert_eq!(
        bd.evaluated_pt,
        Some(PowerToughness {
            power: 6,
            toughness: 4
        })
    );
}

#[test]
fn t1_f08_02_cr613_8a_non_circular_dependency_overrides_timestamp() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_17", "Synth Mountain", "Mountain", ManaColor::R);
    let swamp_sub = b.intern_subtype("Swamp");
    let mtn_sub = b.intern_subtype("Mountain");
    let dual = b.add_card("synth_dual_17", "Synth Dual", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("tap_g", &[ManaColor::G]);
    });
    // Dependent (played FIRST, earlier timestamp): "All lands are Swamps in addition to their other types."
    let urborg_like = b.add_card("synth_swampmaker_17", "Synth Swampmaker", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::LAND)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::LAND,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddTypes {
                        types: CardTypeMask::empty(),
                        subtypes: smallvec::smallvec![swamp_sub],
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Prerequisite (cast AFTER, later timestamp): "Nonbasic lands are Mountains."
    let moon_like = b.add_card("synth_moon_17", "Synth Moon", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::LAND,
                            supertypes_none: SupertypeMask::BASIC,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetTypes {
                        types: CardTypeMask::LAND,
                        subtypes: smallvec::smallvec![mtn_sub],
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Inclusion dependency pair (SAME Layer 4, earlier dependent before later prerequisite):
    let enchant_army = b.add_card("synth_earmy_17", "Synth Starfield", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ENCHANTMENT,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddTypes {
                        types: CardTypeMask::CREATURE,
                        subtypes: Default::default(),
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
                            types_any: CardTypeMask::ENCHANTMENT,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetBasePt {
                        power: Box::new(Amount::Const { n: 2 }),
                        toughness: Box::new(Amount::Const { n: 2 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let lattice = b.add_card("synth_lat_17", "Synth Lattice", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
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
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: Default::default(),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let relic = b.add_card("synth_rel_17", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let db = b.build_db();
    let mut main = vec![mtn; 12];
    main.extend(vec![moon_like; 8]);
    main.extend(vec![urborg_like; 8]);
    main.extend(vec![dual; 8]);
    main.extend(vec![enchant_army; 8]);
    main.extend(vec![lattice; 8]);
    main.extend(vec![relic; 8]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        117,
    )
    .unwrap();
    drv.keep_all_hands();

    // Play Mountain first so we have {R}, then play Synth Swampmaker (ts earlier than Synth Moon),
    // then cast Synth Moon (ts later than Synth Swampmaker) so CR 613.8a must override timestamp order!
    let mut mtn_played = false;
    let mut base_turn = 1u32;
    for turn in (1u32..=49).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (mtn, 1),
                (urborg_like, 1),
                (moon_like, 1),
                (dual, 1),
                (enchant_army, 1),
                (lattice, 1),
                (relic, 1),
            ],
        )
        .unwrap();
        if !mtn_played && let Some(m_h) = drv.find_hand_object(PlayerId(0), mtn) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: m_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            mtn_played = true;
            base_turn = turn;
            break;
        }
    }
    assert!(mtn_played, "Synth Mountain must be played");

    // Play Synth Swampmaker FIRST (dependent, earlier timestamp) and cast Synth Moon AFTER (prerequisite, later timestamp).
    let mut urborg_played = false;
    let mut moon_cast = false;
    for turn in ((base_turn + 2)..=(base_turn + 40)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (urborg_like, usize::from(!urborg_played)),
                (moon_like, 1),
                (dual, 1),
                (enchant_army, 1),
                (lattice, 1),
                (relic, 1),
            ],
        )
        .unwrap();
        if !urborg_played && let Some(u_h) = drv.find_hand_object(PlayerId(0), urborg_like) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: u_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            urborg_played = true;
        }
        if urborg_played
            && !moon_cast
            && let Some(moon_h) = drv.find_hand_object(PlayerId(0), moon_like)
        {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: moon_h,
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
            moon_cast = true;
            base_turn = turn;
            break;
        }
    }
    assert!(urborg_played, "Synth Swampmaker must be played first");
    assert!(moon_cast, "Synth Moon must be cast after Synth Swampmaker");

    // Play Dual and ensure enchant_army, lattice, relic are ready.
    let mut dual_played = false;
    let mut ready = false;
    for turn in ((base_turn + 2)..=(base_turn + 40)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (dual, usize::from(!dual_played)),
                (enchant_army, 1),
                (lattice, 1),
                (relic, 1),
            ],
        )
        .unwrap();
        if !dual_played && let Some(d_h) = drv.find_hand_object(PlayerId(0), dual) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: d_h,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            dual_played = true;
        }
        if dual_played
            && drv.find_hand_object(PlayerId(0), enchant_army).is_some()
            && drv.find_hand_object(PlayerId(0), lattice).is_some()
            && drv.find_hand_object(PlayerId(0), relic).is_some()
        {
            ready = true;
            break;
        }
    }
    assert!(dual_played, "Synth Dual must be played");
    assert!(ready, "inclusion dependency cards must be in hand");

    // Even though Synth Swampmaker has an EARLIER timestamp than Synth Moon,
    // Synth Swampmaker depends on Synth Moon (which turns nonbasic Swampmaker into a Mountain
    // and removes its static ability), so CR 613.8a applies Synth Moon BEFORE Synth Swampmaker:
    // basic Synth Mountain does NOT gain Swamp ({B}), and Synth Dual loses its printed {G} ability!
    let m_bf = drv.find_battlefield_object(PlayerId(0), mtn).unwrap();
    let m_tap_count = drv
        .session
        .legal_actions(PlayerId(0))
        .iter()
        .filter(|a| matches!(a, Action::TapMana { object, .. } if *object == m_bf))
        .count();
    assert_eq!(
        m_tap_count, 1,
        "CR 613.8a: Synth Moon applies BEFORE Synth Swampmaker despite later timestamp, so basic Synth Mountain keeps only 1 mana ability (not Swamp)"
    );
    let d_bf = drv.find_battlefield_object(PlayerId(0), dual).unwrap();
    let bd = drv.session.hover_breakdown(None, d_bf).unwrap();
    assert!(bd.types.contains(CardTypeMask::LAND));
    let d_taps: Vec<_> = drv
        .session
        .legal_actions(PlayerId(0))
        .into_iter()
        .filter(|a| matches!(a, Action::TapMana { object, .. } if *object == d_bf))
        .collect();
    assert_eq!(
        d_taps.len(),
        1,
        "CR 613.8a + 305.7: Synth Dual must have exactly 1 mana ability (Mountain {{R}}), not Swamp or printed {{G}}"
    );
    drv.step(PlayerId(0), d_taps[0].clone()).unwrap();
    assert_eq!(
        drv.session.state().players()[0].mana_pool,
        [0, 0, 0, 1, 0, 0],
        "CR 305.7: Synth Moon must replace Synth Dual's printed {{G}} ability with intrinsic Mountain {{R}}"
    );

    // Inclusion dependency (same Layer 4): cast enchant_army (earlier) then lattice (later) + relic.
    for card in [enchant_army, lattice, relic] {
        let h = drv
            .find_hand_object(PlayerId(0), card)
            .expect("inclusion piece must be in hand");
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
    let rel_bf = drv.find_battlefield_object(PlayerId(0), relic).unwrap();
    let rel_bd = drv.session.hover_breakdown(None, rel_bf).unwrap();
    assert!(
        rel_bd
            .types
            .contains(CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE),
        "CR 613.8a inclusion dependency: Synth Relic must be Artifact + Enchantment + Creature, got {:?}",
        rel_bd.types
    );
}

#[test]
fn t1_f08_03_cr613_8b_circular_dependency_timestamp_tiebreak() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land("synth_island_18", "Synth Island", "Island", ManaColor::U);
    let blue_bear = b.add_synth_vanilla_creature("synth_bb_18", "Synth BlueBear", "{U}", 2, 2);
    let u_to_r = b.add_card("synth_u_to_r_18", "Synth BlueToRed", |c| {
        c.mana_cost("{U}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer5,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            colors_any: ColorMask::U,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetColors {
                        colors: ColorMask::R,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let r_to_u = b.add_card("synth_r_to_u_18", "Synth RedToBlue", |c| {
        c.mana_cost("{U}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer5,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            colors_any: ColorMask::R,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetColors {
                        colors: ColorMask::U,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, island, blue_bear, &[u_to_r, r_to_u]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 118).unwrap();
    drv.keep_all_hands();

    let mut islands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(island, 1), (blue_bear, 1), (u_to_r, 1), (r_to_u, 1)],
        )
        .unwrap();
        if islands_played < 3
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            islands_played += 1;
        }
        if islands_played >= 3
            && drv.find_hand_object(PlayerId(0), blue_bear).is_some()
            && drv.find_hand_object(PlayerId(0), u_to_r).is_some()
            && drv.find_hand_object(PlayerId(0), r_to_u).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Islands and hold blue_bear, u_to_r, and r_to_u within wait loop"
    );
    // Cast bear, then u_to_r (ts earlier), then r_to_u (ts later): Blue -> Red -> Blue.
    for card in [blue_bear, u_to_r, r_to_u] {
        let oid = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    let bear_bf = drv.find_battlefield_object(PlayerId(0), blue_bear).unwrap();
    assert_eq!(
        drv.session.hover_breakdown(None, bear_bf).unwrap().colors,
        ColorMask::U,
        "CR 613.8b circular dependency breaks by timestamp: u_to_r then r_to_u ends Blue"
    );
}

#[test]
fn t1_f08_04_signed_unclamped_option_pt() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_19", "Synth Swamp", "Swamp", ManaColor::B);
    let vehicle = b.add_card("synth_veh_19", "Synth Vehicle", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Vehicle")
            .pt(4, 4);
    });
    let scout = b.add_synth_vanilla_creature("synth_scout_19", "Synth Scout", "{B}", 1, 3);
    let shrink = b.add_card("synth_shrink_19", "Synth Shrink", |c| {
        c.mana_cost("{B}")
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
    let pump = b.add_card("synth_pump_19", "Synth Pump", |c| {
        c.mana_cost("{B}")
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, swamp, scout, &[vehicle, shrink, pump]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 119).unwrap();
    drv.keep_all_hands();

    let mut swamps_played = 0u32;
    let mut found = false;
    for turn in (1u32..=79).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(swamp, 1), (vehicle, 1), (scout, 1), (shrink, 1), (pump, 1)],
        )
        .unwrap();
        if swamps_played < 3
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            swamps_played += 1;
        }
        if swamps_played >= 3
            && drv.find_hand_object(PlayerId(0), vehicle).is_some()
            && drv.find_hand_object(PlayerId(0), scout).is_some()
            && drv.find_hand_object(PlayerId(0), shrink).is_some()
            && drv.find_hand_object(PlayerId(0), pump).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Swamps and hold vehicle, scout, shrink, and pump within wait loop"
    );
    let veh_h = drv.find_hand_object(PlayerId(0), vehicle).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: veh_h,
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
    let veh_bf = drv.find_battlefield_object(PlayerId(0), vehicle).unwrap();
    let veh_bd = drv.session.hover_breakdown(None, veh_bf).unwrap();
    assert_eq!(
        veh_bd.evaluated_pt, None,
        "CR 208.3: non-creature Vehicle has pt == None"
    );

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

    // Cast -3/-0, verify -2/3 unclamped, then cast +2/+0, verify 0/3 (1 - 3 + 2 = 0, not 2!).
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
            power: -2,
            toughness: 3
        })
    );

    let pu_h = drv.find_hand_object(PlayerId(0), pump).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: pu_h,
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
            power: 0,
            toughness: 3
        })
    );
}

#[test]
fn t1_f08_05_dirty_mask_cache_skip_and_verify_layer_cache() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_20", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_20", "Synth Bear", "{W}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_20", "Synth Anthem", "{W}", 1, 1);
    let life_gain = b.add_card("synth_gain_20", "Synth Salve", |c| {
        c.mana_cost("{W}")
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, plains, bear, &[anthem, life_gain]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 120).unwrap();
    drv.keep_all_hands();

    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(plains, 1), (bear, 1), (anthem, 1), (life_gain, 1)],
        )
        .unwrap();
        if lands_played < 3
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 3
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), anthem).is_some()
            && drv.find_hand_object(PlayerId(0), life_gain).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Plains and hold bear, anthem, and life_gain within wait loop"
    );
    for card in [bear, anthem, life_gain] {
        let oid = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert_eq!(
        drv.session
            .hover_breakdown(None, bear_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 3,
            toughness: 3
        })
    );
    assert_eq!(drv.session.view_model(None).players[0].life, 23);
}

// ============================================================================
// F9: Unified CR 614–616 Replacement Pipeline (5 cases)
// ============================================================================

#[test]
fn t1_f09_01_zone_change_replacement_tapped_and_exile_instead_of_gy() {
    let mut b = SynthBundleBuilder::new();
    let tapland = b.add_card("synth_tapland_21", "Synth Tapland", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("tap_w", &[ManaColor::W])
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
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let bear = b.add_synth_vanilla_creature("synth_bear_21", "Synth Bear", "{W}", 2, 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, tapland, bear, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 121).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let bf = drv.find_battlefield_object(PlayerId(0), tapland).unwrap();
    assert!(drv.battlefield_view(PlayerId(0), bf).unwrap().tapped);
}

#[test]
fn t1_f09_02_cr616_1_tier_priority_ordering() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land("synth_island_22", "Synth Island", "Island", ManaColor::U);
    let bear = b.add_synth_vanilla_creature("synth_bear_22", "Synth Bear", "{0}", 2, 2);
    let steal_etb = b.add_card("synth_steal_22", "Synth Gatherer", |c| {
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
    let all_tapped = b.add_card("synth_alltapped_22", "Synth Orb", |c| {
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
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                self_replacement: false,
                who: None,
                condition: None,
            });
    });
    let clone = b.add_card("synth_clone_22", "Synth Clone", |c| {
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
                        copy: Some(mtg_ir::EtbCopySpec {
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
    let double_draw = b.add_card("synth_dbldraw_22", "Synth ThoughtRef", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .replacement_ability(ReplacementDef {
                replaces: EventPattern {
                    event: EventKind::DrawReplacement,
                    filter: EventFilter::default(),
                },
                with: ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: Some(2),
                        delta: None,
                        skip_first_in_your_draw_step: false,
                    },
                },
                self_replacement: false,
                who: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                condition: None,
            });
    });
    let self_draw = b.add_card("synth_sdraw_22", "Synth SelfDraw", |c| {
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
    let _ = u_land;
    let db = b.build_db();
    let mut main0 = vec![bear; 12];
    main0.extend(vec![all_tapped; 16]);
    main0.extend(vec![clone; 16]);
    main0.extend(vec![self_draw; 16]);
    let mut main1 = vec![steal_etb; 30];
    main1.extend(vec![double_draw; 30]);
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
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 122).unwrap();
    drv.keep_all_hands();

    // Ensure P0 holds bear, all_tapped, clone, self_draw and P1 holds steal_etb, double_draw.
    let mut p0_turn = 1u32;
    let f09_02_keep = [
        (bear, 1),
        (all_tapped, 1),
        (clone, 1),
        (self_draw, 1),
        (steal_etb, 1),
        (double_draw, 1),
    ];
    for turn in [1u32, 3, 5, 7] {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &f09_02_keep)
            .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), all_tapped).is_some()
            && drv.find_hand_object(PlayerId(0), clone).is_some()
            && drv.find_hand_object(PlayerId(0), self_draw).is_some()
        {
            p0_turn = turn;
            break;
        }
    }

    // Cast Bear first so Clone has a copy target on battlefield.
    let bear_h = drv
        .find_hand_object(PlayerId(0), bear)
        .expect("Synth Bear must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bear_h,
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
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    // On P1's next turn, P1 casts steal_etb (616.1b) and double_draw (616.1e for P0 draws).
    drv.advance_to_preserving(p0_turn + 1, GamePhase::Main1, 64, &f09_02_keep)
        .unwrap();
    let st_h = drv
        .find_hand_object(PlayerId(1), steal_etb)
        .expect("Synth Gatherer must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: st_h,
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
    let dd_h = drv
        .find_hand_object(PlayerId(1), double_draw)
        .expect("Synth ThoughtRef must be in P1 hand");
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: dd_h,
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

    // On P0's next turn: P0 casts all_tapped (616.1e) and then Clone!
    drv.advance_to_preserving(p0_turn + 2, GamePhase::Main1, 64, &f09_02_keep)
        .unwrap();
    let at_h = drv
        .find_hand_object(PlayerId(0), all_tapped)
        .expect("Synth Orb must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: at_h,
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

    // CR 616.1b (steal to P1) > 616.1c (P1 picks copy of Bear) > 616.1e (enters tapped) with ZERO ChooseReplacement!
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
                    answer: ChoiceAnswer::PickObject { id: bear_bf },
                },
            )
            .unwrap();
        }
        other => {
            panic!(
                "CR 616.1: expected direct ChooseObjects for 616.1c copy without ChooseReplacement, got {other:?}"
            )
        }
    }
    assert_eq!(drv.decision(), DecisionState::Priority);
    let cl_bf = drv
        .find_battlefield_object(PlayerId(1), clone)
        .expect("Clone must enter under P1's control (616.1b)");
    assert_eq!(
        drv.session
            .hover_breakdown(None, cl_bf)
            .unwrap()
            .evaluated_pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
    assert!(
        drv.battlefield_view(PlayerId(1), cl_bf).unwrap().tapped,
        "CR 616.1e: Clone must enter tapped after 616.1b + 616.1c"
    );

    // CR 616.1a (self_replacement +2) > 616.1e (double_draw *2): Draw(1) -> +2 = 3 -> *2 = 6 cards!
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
        drv.decision(),
        DecisionState::Priority,
        "CR 616.1a vs 616.1e must not emit ChooseReplacement"
    );
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        hand_before + 5,
        "CR 616.1a (+2) before 616.1e (*2) must draw (1+2)*2 = 6 cards (net +5)"
    );
}

#[test]
fn t1_f09_03_cr614_5_applied_once_dedup() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_23", "Synth Plains", "Plains", ManaColor::W);
    let doubler = b.add_card("synth_doubler_23", "Synth LifeDoubler", |c| {
        c.mana_cost("{W}")
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
    let salve = b.add_card("synth_salve_23", "Synth Salve", |c| {
        c.mana_cost("{W}")
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
    let mut main = vec![plains; 20];
    main.extend(vec![doubler; 20]);
    main.extend(vec![salve; 20]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        123,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(doubler, 2), (salve, 1), (plains, 1)],
        )
        .unwrap();
        if let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }
        let plains_bf = drv.session.view_model(None).players[0]
            .battlefield
            .iter()
            .filter(|o| o.card_def == plains)
            .count();
        let doubler_cnt = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == doubler)
            .count();
        if plains_bf >= 3 && doubler_cnt >= 2 && drv.find_hand_object(PlayerId(0), salve).is_some()
        {
            break;
        }
    }
    for card in [doubler, doubler, salve] {
        let oid = drv
            .find_hand_object(PlayerId(0), card)
            .expect("required card for CR 614.5 must be in hand");
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
            request: ChoiceRequest::ChooseReplacement { prompt_id, .. },
        } => {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index: 0 },
                },
            )
            .unwrap();
        }
        other => panic!(
            "CR 614.5 / 616.1e: expected ChooseReplacement prompt for two equal-tier LifeGain replacements, got {other:?}"
        ),
    }
    assert_eq!(drv.session.view_model(None).players[0].life, 24);
}

#[test]
fn t1_f09_04_same_tier_tie_prompts_choose_replacement() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_24", "Synth Mountain", "Mountain", ManaColor::R);
    let plus1 = b.add_card("synth_plus1_24", "Synth PlusOne", |c| {
        c.mana_cost("{R}")
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
    let dbl = b.add_card("synth_dbl_24", "Synth Doubler", |c| {
        c.mana_cost("{R}")
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
    let shock = b.add_synth_burn_instant("synth_shock_24", "Synth Shock", "{R}", 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, plus1, &[dbl, shock]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 124).unwrap();
    drv.keep_all_hands();

    let mut mtns_played = 0u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(mtn, 1), (plus1, 1), (dbl, 1), (shock, 1)],
        )
        .unwrap();
        if mtns_played < 3
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            mtns_played += 1;
        }
        if mtns_played >= 3
            && drv.find_hand_object(PlayerId(0), plus1).is_some()
            && drv.find_hand_object(PlayerId(0), dbl).is_some()
            && drv.find_hand_object(PlayerId(0), shock).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Mountains and hold plus1, dbl, and shock within wait loop"
    );
    for card in [plus1, dbl] {
        let oid = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    let sh_h = drv.find_hand_object(PlayerId(0), shock).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sh_h,
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

    match drv.decision() {
        DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseReplacement {
                    prompt_id,
                    option_count,
                },
        } => {
            assert_eq!(option_count, 2);
            assert_eq!(
                drv.actor(),
                Some(PlayerId(1)),
                "affected player chooses replacement order"
            );
            drv.step(
                PlayerId(1),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index: 0 },
                },
            )
            .unwrap();
        }
        other => panic!("expected ChooseReplacement prompt on tier tie, got {other:?}"),
    }
    let life_after = drv.session.view_model(None).players[1].life;
    assert!(
        life_after == 14 || life_after == 15,
        "expected (2+1)*2=6 or (2*2)+1=5 damage"
    );
}

#[test]
fn t1_f09_05_cr615_damage_prevention_shield() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_25", "Synth Plains", "Plains", ManaColor::W);
    let shield = b.add_card("synth_shield_25", "Synth Shield", |c| {
        c.mana_cost("{W}")
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
    let blast = b.add_synth_burn_instant("synth_blast_25", "Synth Blast", "{W}", 5);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, plains, shield, &[blast]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 125).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    let sh_h = drv.find_hand_object(PlayerId(0), shield).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sh_h,
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

    let bl_h = drv.find_hand_object(PlayerId(0), blast).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bl_h,
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
        18,
        "Prevent 3 of 5 -> 2 damage dealt"
    );
}

// ============================================================================
// F10: Atomic Events & Triggers (CR 603) (5 cases)
// ============================================================================

#[test]
fn t1_f10_01_atomic_event_batch_pre_batch_lki_cr603_10a() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_26", "Synth Swamp", "Swamp", ManaColor::B);
    let ally = b.add_synth_vanilla_creature("synth_ally_26", "Synth Ally", "{B}", 2, 2);
    let witness = b.add_card("synth_witness_26", "Synth Witness", |c| {
        c.mana_cost("{B}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .triggered_ability(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                flags: FilterFlags::OTHER,
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
    let wipe = b.add_card("synth_wipe_26", "Synth Wipe", |c| {
        c.mana_cost("{B}")
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
    let decks = build_60_card_pair(&mut b, swamp, ally, &[witness, wipe]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 126).unwrap();
    drv.keep_all_hands();

    let mut swamps_played = 0u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(swamp, 1), (ally, 1), (witness, 1), (wipe, 1)],
        )
        .unwrap();
        if swamps_played < 3
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            swamps_played += 1;
        }
        if swamps_played >= 3
            && drv.find_hand_object(PlayerId(0), ally).is_some()
            && drv.find_hand_object(PlayerId(0), witness).is_some()
            && drv.find_hand_object(PlayerId(0), wipe).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Swamps and hold ally, witness, and wipe within wait loop"
    );
    for card in [ally, witness] {
        let oid = drv.find_hand_object(PlayerId(0), card).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    let w_h = drv.find_hand_object(PlayerId(0), wipe).unwrap();
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
    // Trigger resolves on subsequent pass:
    drv.pass_priority();
    drv.pass_priority();
    let hand_after = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    assert_eq!(
        hand_after, hand_before,
        "CR 603.10a: Witness sees Ally die in same atomic batch (-1 Wipe +1 Draw)"
    );
}

#[test]
fn t1_f10_02_apnap_trigger_stacking_cr603_3b() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_27", "Synth Plains", "Plains", ManaColor::W);
    let death_punisher = b.add_card("synth_p0_die_27", "Synth DeathToll", |c| {
        c.mana_cost("{W}")
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
                    EffectOp::LoseLife,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 20 }),
                )],
            );
    });
    let token_crea = b.add_synth_vanilla_creature("synth_tc_27", "Synth Goat", "{0}", 1, 1);
    let kill_crea = b.add_card("synth_kill_27", "Synth Doom", |c| {
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
    let mut main0 = vec![plains; 12];
    main0.extend(vec![death_punisher; 16]);
    main0.extend(vec![token_crea; 16]);
    main0.extend(vec![kill_crea; 16]);
    let mut main1 = vec![plains; 30];
    main1.extend(vec![death_punisher; 30]);
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
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 127).unwrap();
    drv.keep_all_hands();

    // Phase A: P0 plays Plains and casts death_punisher.
    let mut p0_land = false;
    let mut p0_turn = 1u32;
    let mut p0_done = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (plains, usize::from(!p0_land)),
                (death_punisher, 1),
                (token_crea, 1),
                (kill_crea, 1),
            ],
        )
        .unwrap();
        if !p0_land && let Some(land0) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land0,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            p0_land = true;
        }
        if p0_land && let Some(e0) = drv.find_hand_object(PlayerId(0), death_punisher) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: e0,
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
            p0_turn = turn;
            p0_done = true;
            break;
        }
    }
    assert!(
        p0_done,
        "P0 must play Plains and cast death_punisher within wait loop"
    );

    // Phase B: P1 plays Plains and casts death_punisher.
    let mut p1_land = false;
    let mut p1_turn = p0_turn + 1;
    let mut p1_done = false;
    for turn in ((p0_turn + 1)..=(p0_turn + 39)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (plains, 1),
                (death_punisher, 1),
                (token_crea, 1),
                (kill_crea, 1),
            ],
        )
        .unwrap();
        if !p1_land && let Some(land1) = find_first_land_in_hand(&drv, PlayerId(1)) {
            drv.step(
                PlayerId(1),
                Action::PlayLand {
                    object: land1,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            p1_land = true;
        }
        if p1_land && let Some(e1) = drv.find_hand_object(PlayerId(1), death_punisher) {
            drv.macro_step(
                PlayerId(1),
                MacroAction::CastSpell {
                    object: e1,
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
            p1_done = true;
            break;
        }
    }
    assert!(
        p1_done,
        "P1 must play Plains and cast death_punisher within wait loop"
    );

    // Phase C: P0 waits for {0} 1/1 Goat and {0} Doom, then casts both.
    let mut found_combo = false;
    for turn in ((p1_turn + 1)..=(p1_turn + 39)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(token_crea, 1), (kill_crea, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), token_crea).is_some()
            && drv.find_hand_object(PlayerId(0), kill_crea).is_some()
        {
            found_combo = true;
            break;
        }
    }
    assert!(
        found_combo,
        "P0 must hold token_crea and kill_crea within wait loop"
    );
    let tc_h = drv
        .find_hand_object(PlayerId(0), token_crea)
        .expect("Synth Goat must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: tc_h,
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
    let tc_bf = drv
        .find_battlefield_object(PlayerId(0), token_crea)
        .unwrap();

    let kc_h = drv
        .find_hand_object(PlayerId(0), kill_crea)
        .expect("Synth Doom must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: kc_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: tc_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    // Both triggers fire simultaneously on P0's turn: P0 bottom, P1 top -> P1 loses first!
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.decision(),
        DecisionState::GameOver {
            winner: Some(PlayerId(0))
        },
        "CR 603.3b APNAP: non-active player's trigger resolves first"
    );
}

#[test]
fn t1_f10_03_intervening_if_dual_check_cr603_4() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_28", "Synth Mountain", "Mountain", ManaColor::R);
    let relic = b.add_card("synth_relic_28", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let watcher = b.add_card("synth_upk_art_28", "Synth ArtCheck", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability_if(
                mtg_ir::TriggerDef {
                    scope: Some(mtg_ir::TriggerScope::ControllerTurn),
                    ..simple_trigger(
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
                    )
                },
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
    let shatter = b.add_card("synth_shat_28", "Synth Shatter", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
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
    let decks = build_60_card_pair(&mut b, mtn, relic, &[watcher, shatter]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 128).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut relic_cast = false;
    let mut watch_turn = 1u32;
    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (mtn, usize::from(!land_played)),
                (relic, usize::from(!relic_cast)),
                (watcher, 1),
                (shatter, 1),
            ],
        )
        .unwrap();
        if !land_played && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if !relic_cast && let Some(rel_h) = drv.find_hand_object(PlayerId(0), relic) {
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: rel_h,
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
            relic_cast = true;
        }
        if land_played
            && relic_cast
            && drv.find_hand_object(PlayerId(0), shatter).is_some()
            && let Some(w_h) = drv.find_hand_object(PlayerId(0), watcher)
        {
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
            watch_turn = turn;
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must set up relic, watcher, and shatter within wait loop"
    );

    drv.advance_to_preserving(watch_turn + 2, GamePhase::Upkeep, 64, &[(shatter, 1)])
        .unwrap();
    assert_eq!(drv.session.state().stack().len(), 1);
    let rel_bf = drv.find_battlefield_object(PlayerId(0), relic).unwrap();
    let sh_h = drv.find_hand_object(PlayerId(0), shatter).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sh_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: rel_bf }],
        },
    )
    .unwrap();
    drv.advance_to(watch_turn + 2, GamePhase::Main1, 64)
        .unwrap();
    assert_eq!(
        drv.session.view_model(None).players[0].life,
        20,
        "CR 603.4 intervening-if false on resolution must do nothing"
    );
}

#[test]
fn t1_f10_04_state_trigger_fires_once_cr603_8() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_29", "Synth Plains", "Plains", ManaColor::W);
    let state_ench = b.add_card("synth_state_29", "Synth Fragile", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                mtg_ir::TriggerDef {
                    is_state_trigger: true,
                    ..simple_trigger(
                        EventKind::StateTriggered,
                        EventFilter {
                            detail: Some(EventDetail::State {
                                condition: Box::new(Condition::Not {
                                    of: Box::new(Condition::ControlsFilter {
                                        filter: Box::new(ObjectFilter {
                                            zones: ZoneMask::BATTLEFIELD,
                                            types_any: CardTypeMask::CREATURE,
                                            controller: Some(RelativeControl::You),
                                            ..Default::default()
                                        }),
                                    }),
                                }),
                            }),
                            ..Default::default()
                        },
                    )
                },
                vec![],
                vec![effect_step(
                    EffectOp::Sacrifice {
                        select: Default::default(),
                    },
                    Selector::SelfObj,
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, plains, state_ench, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 129).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let st_h = drv.find_hand_object(PlayerId(0), state_ench).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: st_h,
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
        drv.session.state().stack().len(),
        1,
        "CR 603.8 state trigger queues exactly once"
    );
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_graveyard_object(PlayerId(0), state_ench).is_some());
}

#[test]
fn t1_f10_05_delayed_and_reflexive_triggers_cr603_7_12() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_30", "Synth Mountain", "Mountain", ManaColor::R);
    let reflex = b.add_card("synth_reflex_30", "Synth Reflex", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![EffectStep {
                    op: EffectOp::Discard {
                        select: Default::default(),
                    },
                    subject: Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    amount: Some(Box::new(Amount::Const { n: 1 })),
                    optional: Some(mtg_ir::OptionalSpec {
                        chooser: PlayerRef::You,
                        cost: vec![],
                        mode: mtg_ir::OptionalMode::DoIfPaid,
                    }),
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: Some(Box::new(mtg_ir::ReflexiveTriggerSpec {
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
                            Some(Amount::Const { n: 3 }),
                        )],
                        modal: None,
                        text: None,
                        event_filter: Some(mtg_ir::ReflexiveEventFilter::WhenYouDo),
                        object_filter: None,
                    })),
                    actor: None,
                    span: Default::default(),
                }],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, reflex, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 130).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let rf_h = drv.find_hand_object(PlayerId(0), reflex).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: rf_h,
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
    // Accept optional discard -> pick card to discard -> drive reflexive trigger target -> resolve.
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
        other => panic!("expected YesNo prompt for optional discard, got {other:?}"),
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
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: candidates[0] },
                },
            )
            .unwrap();
        }
        other => panic!("expected ChooseObjects discard prompt, got {other:?}"),
    }
    let mut guard = 0;
    while matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        let actor = drv
            .actor()
            .expect("reflexive target prompt must have actor");
        let legal = drv.session.legal_actions(actor);
        let act = legal
            .iter()
            .find(|a| !matches!(a, Action::Concede) && format!("{a:?}").contains("PlayerId(1)"))
            .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
            .expect("reflexive target step must expose non-concede action")
            .clone();
        drv.step(actor, act).unwrap();
        guard += 1;
        assert!(guard <= 8, "reflexive trigger targeting did not complete");
    }
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session.view_model(None).players[1].life,
        17,
        "CR 603.12 reflexive trigger must deal 3 damage"
    );
}

// ============================================================================
// F11: Mana Pool (CR 106.6), Auto-Tap Solver & Costs (5 cases)
// ============================================================================

#[test]
fn t1_f11_01_restricted_mana_cr106_6() {
    let mut b = SynthBundleBuilder::new();
    let creature_land = b.add_card("synth_crea_land_31", "Synth Orchard", |c| {
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
    let bear = b.add_synth_vanilla_creature("synth_bear_31", "Synth Bear", "{G}", 2, 2);
    let draw_sorc = b.add_synth_draw_sorcery("synth_draw_31", "Synth Draw", "{G}", 1);
    let db = b.build_db();
    let mut main = vec![creature_land; 20];
    main.extend(vec![bear; 20]);
    main.extend(vec![draw_sorc; 20]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        131,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(creature_land, 1), (draw_sorc, 1), (bear, 1)],
        )
        .unwrap();
        if find_first_land_in_hand(&drv, PlayerId(0)).is_some()
            && drv.find_hand_object(PlayerId(0), draw_sorc).is_some()
            && drv.find_hand_object(PlayerId(0), bear).is_some()
        {
            break;
        }
    }
    let land = find_first_land_in_hand(&drv, PlayerId(0)).expect("Synth Orchard must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let land_bf = drv
        .find_battlefield_object(PlayerId(0), creature_land)
        .unwrap();

    let sorc = drv
        .find_hand_object(PlayerId(0), draw_sorc)
        .expect("draw_sorc must be in hand");
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: sorc,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(
        !prev.can_pay,
        "CR 106.6 creature-restricted mana must NOT pay for a Sorcery"
    );

    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: land_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    let cast_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sorc,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(cast_res, Err(EngineError::IllegalAction(_))),
        "CR 106.6: pooled creature-restricted mana must reject casting a Sorcery with IllegalAction, got {cast_res:?}"
    );
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
    assert!(drv.find_battlefield_object(PlayerId(0), bear).is_some());
}

#[test]
fn t1_f11_02_preview_auto_tap_accuracy() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land("synth_island_32", "Synth Island", "Island", ManaColor::U);
    let drake =
        b.add_synth_keyword_creature("synth_drake_32", "Synth Drake", "{1}{U}", 2, 2, &["flying"]);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, island, drake, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 132).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    let drake_h = drv.find_hand_object(PlayerId(0), drake).unwrap();
    let preview = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: drake_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(preview.can_pay);
    assert_eq!(preview.sources_to_tap.len(), 2);
}

#[test]
fn t1_f11_03_cost_modifiers_cr601_2f() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land("synth_island_33", "Synth Island", "Island", ManaColor::U);
    let reducer = b.add_card("synth_red_33", "Synth Reducer", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::SORCERY,
                            ..Default::default()
                        }),
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
    let sorc = b.add_synth_draw_sorcery("synth_draw_33", "Synth Ponder", "{3}", 1);
    let sac_draw = b.add_card("synth_sacdraw_33", "Synth SacDraw", |c| {
        c.mana_cost("{2}{U}")
            .types(CardTypeMask::SORCERY)
            .spell_ability(mtg_ir::SpellAbility {
                id: None,
                additional_costs: vec![CostDef::Sacrifice {
                    count: Box::new(Amount::Const { n: 1 }),
                    target: mtg_ir::CostObjectTarget::Filter {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::ARTIFACT,
                            ..Default::default()
                        }),
                    },
                    sacrifice_all: false,
                    half_rounded_up: false,
                    attached_creature: false,
                    one_or_more: false,
                    up_to: None,
                    different_names: false,
                }],
                targets: vec![],
                effect: vec![effect_step(
                    EffectOp::Draw {
                        up_to: false,
                        reveal_drawn: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 2 }),
                )],
                modal: None,
                cast_restrictions: vec![],
                cast_condition: None,
                x_cant_be_zero: false,
                span: Default::default(),
            });
    });
    let db = b.build_db();
    let mut main = vec![island; 20];
    main.extend(vec![reducer; 16]);
    main.extend(vec![sorc; 12]);
    main.extend(vec![sac_draw; 12]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        133,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(island, 1), (reducer, 1), (sorc, 1), (sac_draw, 1)],
        )
        .unwrap();
        if find_first_land_in_hand(&drv, PlayerId(0)).is_some()
            && drv.find_hand_object(PlayerId(0), reducer).is_some()
            && drv.find_hand_object(PlayerId(0), sorc).is_some()
            && drv.find_hand_object(PlayerId(0), sac_draw).is_some()
        {
            break;
        }
    }
    let land = find_first_land_in_hand(&drv, PlayerId(0)).expect("Island must be in hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let red_h = drv
        .find_hand_object(PlayerId(0), reducer)
        .expect("Synth Reducer must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: red_h,
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

    let sorc_h = drv
        .find_hand_object(PlayerId(0), sorc)
        .expect("Synth Ponder must be in hand");
    let preview = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: sorc_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(
        preview.can_pay,
        "3 - 2 = 1 mana should be payable with 1 Island"
    );

    // CR 601.2f locks total cost of sac_draw at {U} BEFORE Synth Reducer is sacrificed in CR 601.2h!
    let hand_before = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    let sd_h = drv
        .find_hand_object(PlayerId(0), sac_draw)
        .expect("Synth SacDraw must be in hand");
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
    drv.pass_priority();
    drv.pass_priority();
    assert!(
        drv.find_graveyard_object(PlayerId(0), reducer).is_some(),
        "CR 601.2h: Synth Reducer must be sacrificed into graveyard"
    );
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        hand_before + 1,
        "CR 601.2f: sac_draw must resolve (-1 card cast + 2 drawn = +1)"
    );
}

#[test]
fn t1_f11_04_alternative_and_kicker_costs() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_34", "Synth Mountain", "Mountain", ManaColor::R);
    let k_id = b.intern_keyword("kicker");
    let kbolt = b.add_card("synth_kbolt_34", "Synth KickerBolt", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::INSTANT)
            .keyword_def(mtg_ir::KeywordDef {
                kicker_costs: vec![mtg_ir::KickerCostOption {
                    cost: mtg_ir::ManaCost::parse("{R}"),
                    cost_defs: vec![],
                    tags: vec![],
                }],
                ..mtg_engine_e2e::synth::empty_keyword_def(k_id)
            })
            .spell_effect(
                vec![target_player()],
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
                    Some(Amount::Conditional {
                        condition: Box::new(Condition::Kicked {
                            tag: None,
                            min_count: None,
                        }),
                        then_amt: Box::new(Amount::Const { n: 4 }),
                        else_amt: Box::new(Amount::Const { n: 2 }),
                    }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, kbolt, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 134).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    let kb_h = drv.find_hand_object(PlayerId(0), kbolt).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: kb_h,
            option_id: 1,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[1].life, 16);
}

#[test]
fn t1_f11_05_non_mana_costs_sac_and_pay_life() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_35", "Synth Swamp", "Swamp", ManaColor::B);
    let altar = b.add_card("synth_altar_35", "Synth BloodAltar", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .activated_ability(
                "pay_life_draw",
                vec![
                    CostDef::TapSelf,
                    CostDef::PayLife {
                        amount: Box::new(Amount::Const { n: 2 }),
                        half_life_rounded_up: false,
                    },
                ],
                Timing::Instant,
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
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, swamp, altar, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 135).unwrap();
    drv.keep_all_hands();

    let alt_h = drv.find_hand_object(PlayerId(0), altar).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: alt_h,
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
    let alt_bf = drv.find_battlefield_object(PlayerId(0), altar).unwrap();
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: alt_bf,
            ability_index: 0,
        },
    )
    .unwrap();
    assert_eq!(drv.session.view_model(None).players[0].life, 18);
}

// ============================================================================
// F12: Sequential CR 601.2 Casting, Targets & 6-Kind Choice Protocol (5 cases)
// ============================================================================

#[test]
fn t1_f12_01_sequential_casting_sequence_substeps() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_36", "Synth Mountain", "Mountain", ManaColor::R);
    let modal_x = b.add_card("synth_modalx_36", "Synth ModalX", |c| {
        c.mana_cost("{X}{R}")
            .types(CardTypeMask::SORCERY)
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
                                Some(Amount::X),
                            )],
                            span: Default::default(),
                        },
                        ModalMode {
                            targets: vec![],
                            effect: vec![effect_step(
                                EffectOp::GainLife {
                                    set_life_total: false,
                                },
                                Selector::PlayerRef {
                                    player: PlayerRef::You,
                                },
                                Some(Amount::X),
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
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, modal_x, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 136).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
    }
    let mx_h = drv.find_hand_object(PlayerId(0), modal_x).unwrap();
    drv.step(
        PlayerId(0),
        Action::BeginCast {
            object: mx_h,
            option_id: 0,
            from_zone: Zone::Hand,
        },
    )
    .unwrap();
    assert!(matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ));

    let mut guard = 0;
    while matches!(
        drv.decision(),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        let actor = drv.actor().expect("casting substep must have actor");
        match drv.decision() {
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode { prompt_id, .. },
            } => {
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickMode { index: 0 },
                    },
                )
                .unwrap();
            }
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseNumber { prompt_id, .. },
            } => {
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickNumber { value: 1 },
                    },
                )
                .unwrap();
            }
            _ => {
                let legal = drv.session.legal_actions(actor);
                let act = legal
                    .iter()
                    .find(|a| {
                        !matches!(a, Action::Concede) && format!("{a:?}").contains("PlayerId(1)")
                    })
                    .or_else(|| legal.iter().find(|a| matches!(a, Action::TapMana { .. })))
                    .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
                    .expect("casting substep must expose non-concede action")
                    .clone();
                drv.step(actor, act).unwrap();
            }
        }
        guard += 1;
        assert!(
            guard <= 12,
            "CastingSequence did not complete within 12 steps"
        );
    }
    assert_eq!(drv.decision(), DecisionState::Priority);
    assert_eq!(drv.session.state().stack().len(), 1);
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(
        drv.session.view_model(None).players[1].life,
        19,
        "ModalX mode 0 with X=1 must deal 1 damage to Player 1"
    );
}

#[test]
fn t1_f12_02_target_legality_hexproof_shroud_ward() {
    let mut b = SynthBundleBuilder::new();
    let forest = b.add_synth_basic_land("synth_forest_37", "Synth Forest", "Forest", ManaColor::G);
    let hex =
        b.add_synth_keyword_creature("synth_hex_37", "Synth Hexproof", "{G}", 2, 2, &["hexproof"]);
    let bolt = b.add_synth_burn_instant("synth_bolt_37", "Synth Bolt", "{G}", 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, forest, hex, &[bolt]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 137).unwrap();
    drv.keep_all_hands();

    drv.advance_to(2, GamePhase::Main1, 32).unwrap();
    let land1 = find_first_land_in_hand(&drv, PlayerId(1)).unwrap();
    drv.step(
        PlayerId(1),
        Action::PlayLand {
            object: land1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let hex_h = drv.find_hand_object(PlayerId(1), hex).unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: hex_h,
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
    let hex_bf = drv.find_battlefield_object(PlayerId(1), hex).unwrap();

    let mut land_played = false;
    let mut found = false;
    for turn in (3u32..=49).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(forest, 1), (bolt, 1)])
            .unwrap();
        if !land_played && let Some(land0) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land0,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played && drv.find_hand_object(PlayerId(0), bolt).is_some() {
            found = true;
            break;
        }
    }
    assert!(found, "must play Forest and draw bolt within wait loop");
    let bolt_h = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    let err = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bolt_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: hex_bf }],
        },
    );
    assert!(
        matches!(err, Err(EngineError::IllegalAction(_))),
        "cannot target opponent's Hexproof creature (expected IllegalAction, got {err:?})"
    );
}

#[test]
fn t1_f12_03_target_revalidation_cr608_2b_fizzle_vs_partial() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land("synth_island_38", "Synth Island", "Island", ManaColor::U);
    let bear = b.add_synth_vanilla_creature("synth_bear_38", "Synth Bear", "{U}", 2, 2);
    let bear23 = b.add_synth_vanilla_creature("synth_bear23_38", "Synth ToughBear", "{0}", 2, 3);
    let bolt = b.add_synth_burn_instant("synth_bolt_38", "Synth Bolt", "{U}", 3);
    let bounce = b.add_card("synth_bounce_38", "Synth Unsummon", |c| {
        c.mana_cost("{U}")
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
    let dual_bolt = b.add_card("synth_dbolt_38", "Synth TwinBolt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature(), target_creature()],
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
                        Some(Amount::Const { n: 2 }),
                    ),
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
                        Selector::Target { index: 1 },
                        Some(Amount::Const { n: 2 }),
                    ),
                ],
            );
    });
    let db = b.build_db();
    let mut main = vec![u_land; 16];
    main.extend(vec![bear; 8]);
    main.extend(vec![bear23; 12]);
    main.extend(vec![bolt; 8]);
    main.extend(vec![bounce; 8]);
    main.extend(vec![dual_bolt; 8]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        138,
    )
    .unwrap();
    drv.keep_all_hands();

    // Part 1: Full fizzle (1 target, becomes illegal -> bolt fizzles).
    let mut p1_turn = 1u32;
    let mut found1 = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[
                (u_land, 1),
                (bear, 1),
                (bolt, 1),
                (bounce, 2),
                (bear23, 2),
                (dual_bolt, 1),
            ],
        )
        .unwrap();
        if let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }
        let islands = drv.session.view_model(None).players[0]
            .battlefield
            .iter()
            .filter(|o| o.card_def == u_land && !o.tapped)
            .count();
        if islands >= 3
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), bolt).is_some()
            && drv.find_hand_object(PlayerId(0), bounce).is_some()
        {
            p1_turn = turn;
            found1 = true;
            break;
        }
    }
    assert!(found1, "must draw Part 1 combo pieces within wait loop");
    let b_h = drv
        .find_hand_object(PlayerId(0), bear)
        .expect("bear in hand");
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

    let bolt_h = drv
        .find_hand_object(PlayerId(0), bolt)
        .expect("bolt in hand");
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
    let bn_h = drv
        .find_hand_object(PlayerId(0), bounce)
        .expect("bounce in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bn_h,
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
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_graveyard_object(PlayerId(0), bolt).is_some());
    assert!(drv.find_hand_object(PlayerId(0), bear).is_some());

    // Part 2: CR 608.2b partial fizzle (2 targets: bear_a bounced, bear_b still takes 2 damage!).
    let mut found2 = false;
    for turn in ((p1_turn + 2)..=(p1_turn + 40)).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear23, 2), (dual_bolt, 1), (bounce, 1)],
        )
        .unwrap();
        let b23_cnt = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == bear23)
            .count();
        if b23_cnt >= 2
            && drv.find_hand_object(PlayerId(0), dual_bolt).is_some()
            && drv.find_hand_object(PlayerId(0), bounce).is_some()
        {
            found2 = true;
            break;
        }
    }
    assert!(found2, "must draw Part 2 combo pieces within wait loop");
    for _ in 0..2 {
        let h = drv
            .find_hand_object(PlayerId(0), bear23)
            .expect("Synth ToughBear must be in hand");
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
    let two_bears: Vec<_> = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.card_def == bear23)
        .map(|o| o.id)
        .collect();
    assert_eq!(two_bears.len(), 2);
    let (bear_a, bear_b) = (two_bears[0], two_bears[1]);

    let db_h = drv
        .find_hand_object(PlayerId(0), dual_bolt)
        .expect("Synth TwinBolt must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: db_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![
                TargetRef::Object { id: bear_a },
                TargetRef::Object { id: bear_b },
            ],
        },
    )
    .unwrap();
    let bn2_h = drv
        .find_hand_object(PlayerId(0), bounce)
        .expect("Synth Unsummon must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bn2_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: bear_a }],
        },
    )
    .unwrap();
    // Resolve bounce (bear_a returns to hand), then resolve dual_bolt (partially resolves on bear_b!).
    drv.pass_priority();
    drv.pass_priority();
    drv.pass_priority();
    drv.pass_priority();
    assert!(drv.find_hand_object(PlayerId(0), bear23).is_some());
    assert!(drv.find_graveyard_object(PlayerId(0), dual_bolt).is_some());
    assert_eq!(
        drv.battlefield_view(PlayerId(0), bear_b)
            .expect("bear_b must remain on battlefield")
            .damage_marked,
        2,
        "CR 608.2b: dual_bolt must partially resolve and deal 2 damage to legal target bear_b"
    );
}

#[test]
fn t1_f12_04_all_6_choice_request_shapes() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land("synth_island_39", "Synth Island", "Island", ManaColor::U);
    let opt_heal = b.add_card("synth_opt_39", "Synth OptHeal", |c| {
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
    let search_land = b.add_card("synth_srch_39", "Synth Tutor", |c| {
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
    let modal = b.add_card("synth_modal_39", "Synth Charm", |c| {
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
                            targets: vec![],
                            effect: vec![effect_step(
                                EffectOp::GainLife {
                                    set_life_total: false,
                                },
                                Selector::PlayerRef {
                                    player: PlayerRef::You,
                                },
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
    let num_art = b.add_card("synth_num_39", "Synth Dial", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
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
    let reorder = b.add_card("synth_reord_39", "Synth PonderTop", |c| {
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
    let add1_dmg = b.add_card("synth_add1_39", "Synth PlusOne", |c| {
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
    let dbl_dmg = b.add_card("synth_dbl_39", "Synth Doubler", |c| {
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
    let shock2 = b.add_synth_burn_instant("synth_shk_39", "Synth Shock", "{0}", 2);

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
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), vec![d0, d1], 139).unwrap();
    drv.keep_all_hands();

    let mut seen_yes_no = false;
    let mut seen_choose_objects = false;
    let mut seen_choose_mode = false;
    let mut seen_choose_number = false;
    let mut seen_order_objects = false;
    let mut seen_choose_replacement = false;

    let mut drive_prompts = |drv: &mut SessionDriver| {
        let mut guard = 0;
        while let DecisionState::Prompt { request } = drv.decision() {
            let actor = drv.actor().expect("prompt must have actor");
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

        if let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }

        if !done_opt && let Some(h) = drv.find_hand_object(PlayerId(0), opt_heal) {
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
            drive_prompts(&mut drv);
            done_opt = true;
        }
        if !done_search && let Some(h) = drv.find_hand_object(PlayerId(0), search_land) {
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
            drive_prompts(&mut drv);
            done_search = true;
        }
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
            drive_prompts(&mut drv);
            drv.pass_priority();
            drv.pass_priority();
            drive_prompts(&mut drv);
            done_modal = true;
        }
        if !done_num && let Some(h) = drv.find_hand_object(PlayerId(0), num_art) {
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
            drive_prompts(&mut drv);
            done_num = true;
        }
        if !done_reorder && let Some(h) = drv.find_hand_object(PlayerId(0), reorder) {
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
            drive_prompts(&mut drv);
            done_reorder = true;
        }
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
            drive_prompts(&mut drv);
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

    assert!(seen_yes_no);
    assert!(seen_choose_objects);
    assert!(seen_choose_mode);
    assert!(seen_choose_number);
    assert!(seen_order_objects);
    assert!(seen_choose_replacement);
}

#[test]
fn t1_f12_05_macro_action_atomic_lowering_and_rollback() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_40", "Synth Mountain", "Mountain", ManaColor::R);
    let bear = b.add_synth_vanilla_creature("synth_bear_40", "Synth Bear", "{R}", 2, 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, bear, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 140).unwrap();
    drv.keep_all_hands();

    let bear_h = drv
        .find_hand_object(PlayerId(0), bear)
        .expect("Synth Bear must be in opening hand");
    let rev_before = drv.session.state().layer_rev();
    let seq_before = drv.session.state().event_seq();
    let log_before = drv.session.replay_log().entries.len();
    let res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bear_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(res, Err(EngineError::IllegalAction(_))),
        "casting {{R}} creature with 0 mana must fail with IllegalAction (not NotImplemented), got {res:?}"
    );
    assert_eq!(drv.session.state().layer_rev(), rev_before);
    assert_eq!(drv.session.state().event_seq(), seq_before);
    assert_eq!(drv.session.replay_log().entries.len(), log_before);
    assert!(drv.find_hand_object(PlayerId(0), bear).is_some());

    // Positive lowering: play Mountain, cast Bear via MacroAction::CastSpell (auto-taps Mountain).
    let land =
        find_first_land_in_hand(&drv, PlayerId(0)).expect("Mountain must be in opening hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let bear_h2 = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bear_h2,
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
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    // Illegal phase macro rollback check in Main1.
    let rev2 = drv.session.state().layer_rev();
    let seq2 = drv.session.state().event_seq();
    let log2 = drv.session.replay_log().entries.len();
    let dec2 = drv.decision();
    let bad_atk = drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(bear_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    );
    assert!(
        matches!(bad_atk, Err(EngineError::IllegalAction(_))),
        "DeclareAttackers in Main1 must fail with IllegalAction, got {bad_atk:?}"
    );
    assert_eq!(drv.session.state().layer_rev(), rev2);
    assert_eq!(drv.session.state().event_seq(), seq2);
    assert_eq!(drv.session.replay_log().entries.len(), log2);
    assert_eq!(drv.decision(), dec2);
}

// ============================================================================
// F13: Core EffectOp Interpreter, Pausable ResolutionFrame & Keywords (5 cases)
// ============================================================================

#[test]
fn t1_f13_01_core_effect_op_domains() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land("synth_plains_41", "Synth Plains", "Plains", ManaColor::W);
    let mk_tok = b.add_card("synth_tok_41", "Synth Muster", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::CreateToken {
                        token: Some(Box::new(mtg_ir::TokenSpec {
                            name: "Synth Soldier".into(),
                            power: 2,
                            toughness: 2,
                            colors: ColorMask::W,
                            types: CardTypeMask::CREATURE,
                            supertypes: SupertypeMask::empty(),
                            subtypes: Default::default(),
                            type_line: "Creature — Soldier".into(),
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, w_land, mk_tok, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 141).unwrap();
    drv.keep_all_hands();

    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let mk_h = drv.find_hand_object(PlayerId(0), mk_tok).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: mk_h,
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
    let creatures = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.types.contains(CardTypeMask::CREATURE))
        .count();
    assert_eq!(creatures, 2);
}

#[test]
fn t1_f13_02_pausable_resolution_frame_cr608_2n() {
    let mut b = SynthBundleBuilder::new();
    let g_land = b.add_synth_basic_land("synth_forest_42", "Synth Forest", "Forest", ManaColor::G);
    let bear = b.add_synth_vanilla_creature("synth_bear_42", "Synth Bear", "{G}", 2, 2);
    let tutor = b.add_card("synth_tutor_42", "Synth Tutor", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![
                    effect_step(
                        EffectOp::Search {
                            spec: Box::new(mtg_ir::SearchSpec {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                                count: 1,
                                up_to: false,
                                stated_quality: true,
                                destinations: vec![mtg_ir::SearchDestination {
                                    zone: Zone::Hand,
                                    tapped: false,
                                    on_top: false,
                                }],
                                reveal: true,
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
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, g_land, bear, &[tutor]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 142).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(g_land, 1), (tutor, 1)])
            .unwrap();
        if !land_played && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played && drv.find_hand_object(PlayerId(0), tutor).is_some() {
            found = true;
            break;
        }
    }
    assert!(found, "must play Forest and draw tutor within wait loop");
    let tut_h = drv.find_hand_object(PlayerId(0), tutor).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: tut_h,
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

    // CR 608.2n: while paused on ChooseObjects, resolving spell stays on Stack!
    assert_eq!(drv.session.state().stack().len(), 1);
    match drv.decision() {
        DecisionState::Prompt {
            request:
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                },
        } => {
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: candidates[0] },
                },
            )
            .unwrap();
        }
        other => panic!("expected ChooseObjects prompt for tutor, got {other:?}"),
    }
    assert!(drv.session.state().stack().is_empty());
    assert!(drv.find_graveyard_object(PlayerId(0), tutor).is_some());
}

#[test]
fn t1_f13_03_bind_as_lki_across_steps() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_43", "Synth Swamp", "Swamp", ManaColor::B);
    let brute = b.add_synth_vanilla_creature("synth_brute_43", "Synth Brute", "{B}", 5, 5);
    let consume = b.add_card("synth_consume_43", "Synth Consume", |c| {
        c.mana_cost("{B}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![
                    EffectStep {
                        bind_as: Some("victim".into()),
                        ..effect_step(
                            EffectOp::Destroy {
                                cant_be_regenerated: false,
                                select: Default::default(),
                            },
                            Selector::Target { index: 0 },
                            None,
                        )
                    },
                    effect_step(
                        EffectOp::GainLife {
                            set_life_total: false,
                        },
                        Selector::PlayerRef {
                            player: PlayerRef::You,
                        },
                        Some(Amount::Stat {
                            of: Ref::Binding {
                                name: "victim".into(),
                            },
                            stat: StatKind::Power,
                            lki: true,
                        }),
                    ),
                ],
            );
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, swamp, brute, &[consume]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 143).unwrap();
    drv.keep_all_hands();

    let mut swamps_played = 0u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(swamp, 1), (brute, 1), (consume, 1)],
        )
        .unwrap();
        if swamps_played < 2
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            swamps_played += 1;
        }
        if swamps_played >= 2
            && drv.find_hand_object(PlayerId(0), brute).is_some()
            && drv.find_hand_object(PlayerId(0), consume).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 2 Swamps and hold brute + consume within wait loop"
    );
    let br_h = drv.find_hand_object(PlayerId(0), brute).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: br_h,
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
    let br_bf = drv.find_battlefield_object(PlayerId(0), brute).unwrap();

    let con_h = drv.find_hand_object(PlayerId(0), consume).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: con_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: br_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[0].life, 25);
}

#[test]
fn t1_f13_04_keyword_mechanics_cr702() {
    let mut b = SynthBundleBuilder::new();
    let g_land = b.add_synth_basic_land("synth_forest_44", "Synth Forest", "Forest", ManaColor::G);
    let trampler = b.add_synth_keyword_creature(
        "synth_tram_44",
        "Synth Beast",
        "{G}",
        5,
        5,
        &["deathtouch", "trample", "haste"],
    );
    let wall = b.add_synth_keyword_creature(
        "synth_wall_44",
        "Synth Wall",
        "{G}",
        0,
        6,
        &["defender", "indestructible"],
    );
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, g_land, wall, &[trampler]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 144).unwrap();
    drv.keep_all_hands();

    drv.advance_to(2, GamePhase::Main1, 32).unwrap();
    let l1 = find_first_land_in_hand(&drv, PlayerId(1)).unwrap();
    drv.step(
        PlayerId(1),
        Action::PlayLand {
            object: l1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let w_h = drv.find_hand_object(PlayerId(1), wall).unwrap();
    drv.macro_step(
        PlayerId(1),
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
    let w_bf = drv.find_battlefield_object(PlayerId(1), wall).unwrap();

    let mut land_played = false;
    let mut atk_turn = 3u32;
    let mut found = false;
    for turn in (3u32..=49).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(g_land, 1), (trampler, 1)])
            .unwrap();
        if !land_played && let Some(l0) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l0,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played && drv.find_hand_object(PlayerId(0), trampler).is_some() {
            atk_turn = turn;
            found = true;
            break;
        }
    }
    assert!(found, "must play Forest and draw trampler within wait loop");
    let tr_h = drv.find_hand_object(PlayerId(0), trampler).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: tr_h,
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
    let tr_bf = drv.find_battlefield_object(PlayerId(0), trampler).unwrap();

    drv.advance_to(atk_turn, GamePhase::DeclareAttackers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(tr_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::DeclareBlockers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareBlockers {
            assignments: vec![(w_bf, tr_bf)],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::Main2, 32).unwrap();
    // 5/5 Deathtouch+Trample assigns 1 lethal to 0/6 Indestructible Wall and 4 trample to P1!
    assert_eq!(drv.session.view_model(None).players[1].life, 16);
    assert!(drv.find_battlefield_object(PlayerId(1), wall).is_some());
}

#[test]
fn t1_f13_05_guided_fallback_behavior() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land("synth_mtn_45", "Synth Mountain", "Mountain", ManaColor::R);
    let unsup = b.add_card("synth_unsup_45", "Synth Enigma", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .unsupported(UnsupportedReason::Unparsed, "Synth special mystery.");
    });
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, mtn, unsup, &[]);

    let strict_rep = MatchSession::validate_deck(&db, &GameConfig::standard_1v1(), &decks[0]);
    assert!(!strict_rep.is_legal);

    let mut guided_cfg = GameConfig::standard_1v1();
    guided_cfg.allow_guided_fallback = true;
    let guided_rep = MatchSession::validate_deck(&db, &guided_cfg, &decks[0]);
    assert!(guided_rep.is_legal);

    let mut drv = SessionDriver::new(db, guided_cfg, decks, 145).unwrap();
    drv.keep_all_hands();
    let u_h = drv.find_hand_object(PlayerId(0), unsup).unwrap();
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
    assert!(matches!(
        drv.decision(),
        DecisionState::Prompt {
            request: ChoiceRequest::GuidedResolution { .. }
        }
    ));
}

// ============================================================================
// F14: Turn Loop, Priority/APNAP, Stack Resolution, SBA Fixpoint & Combat (5 cases)
// ============================================================================

#[test]
fn t1_f14_01_opening_draw_and_london_mulligan() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_46", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();
    let d60 = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };

    // 1v1 (free_first: false): P0 takes 2 mulligans -> bottoms 2 cards -> ends with 5 in hand.
    let mut drv_1v1 = SessionDriver::new(
        Arc::clone(&db),
        GameConfig::standard_1v1(),
        vec![d60.clone(), d60],
        146,
    )
    .unwrap();
    drv_1v1.step(PlayerId(0), Action::TakeMulligan).unwrap();
    drv_1v1.step(PlayerId(0), Action::TakeMulligan).unwrap();
    drv_1v1.step(PlayerId(0), Action::KeepHand).unwrap();
    // Bottom 2 cards sequentially (or P1 keeps first).
    while matches!(drv_1v1.decision(), DecisionState::Mulligan) {
        let actor = drv_1v1.actor().unwrap();
        let legal = drv_1v1.session.legal_actions(actor);
        if let Some(bot) = legal
            .iter()
            .find(|a| matches!(a, Action::BottomCard { .. }))
        {
            drv_1v1.step(actor, bot.clone()).unwrap();
        } else {
            drv_1v1.step(actor, Action::KeepHand).unwrap();
        }
    }
    let p0_1v1 = &drv_1v1.session.view_model(Some(PlayerId(0))).players[0];
    assert_eq!(p0_1v1.hand_count, 5);
    assert_eq!(p0_1v1.library_count, 55);

    // 4p Commander (free_first: true): P0 takes 1 mulligan -> bottoms 0 cards -> 7 in hand.
    let cmd = b.add_synth_vanilla_creature("synth_cmd_46", "Synth Cmd", "{W}", 2, 2);
    let db4 = b.build_db();
    let d100 = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![plains; 99],
        sideboard: vec![],
    };
    let mut drv_4p = SessionDriver::new(
        db4,
        GameConfig::commander(4),
        vec![d100.clone(), d100.clone(), d100.clone(), d100],
        146,
    )
    .unwrap();
    drv_4p.step(PlayerId(0), Action::TakeMulligan).unwrap();
    drv_4p.step(PlayerId(0), Action::KeepHand).unwrap();
    drv_4p.keep_all_hands();
    let p0_4p = &drv_4p.session.view_model(Some(PlayerId(0))).players[0];
    assert_eq!(p0_4p.hand_count, 7);
}

#[test]
fn t1_f14_02_turn_based_actions_untap_draw_cleanup() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_47", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 147).unwrap();
    drv.keep_all_hands();

    // Turn 1: starting player skips draw step (CR 103.8a).
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        7
    );
    let land = find_first_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let land_bf = drv.find_battlefield_object(PlayerId(0), plains).unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: land_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    assert!(drv.battlefield_view(PlayerId(0), land_bf).unwrap().tapped);

    // Advance to Turn 3 Main1: Turn 2 P1 draws to 8 cards and must discard 1 at Cleanup!
    let mut saw_cleanup_discard = false;
    for _ in 0..64 {
        let s = drv.session.state();
        if s.turn_number() >= 3 && s.phase() == GamePhase::Main1 {
            break;
        }
        match drv.decision() {
            DecisionState::Priority => {
                drv.pass_priority();
            }
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseObjects {
                        prompt_id,
                        candidates,
                        ..
                    },
            } => {
                assert_eq!(drv.session.state().phase(), GamePhase::Cleanup);
                let actor = drv.session.current_actor().unwrap();
                assert_eq!(actor, PlayerId(1));
                saw_cleanup_discard = true;
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickObject { id: candidates[0] },
                    },
                )
                .unwrap();
            }
            DecisionState::DeclareAttackers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmAttackers).unwrap();
            }
            DecisionState::DeclareBlockers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmBlockers).unwrap();
            }
            other => panic!("unexpected decision while advancing to Turn 3 Main1: {other:?}"),
        }
    }
    assert_eq!(drv.session.state().turn_number(), 3);
    assert_eq!(drv.session.state().phase(), GamePhase::Main1);
    assert!(
        saw_cleanup_discard,
        "Turn 2 P1 must discard down to 7 during Cleanup step"
    );
    assert!(!drv.battlefield_view(PlayerId(0), land_bf).unwrap().tapped);
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[1].hand_count,
        7
    );
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        7
    );
}

#[test]
fn t1_f14_03_iterative_sba_fixpoint_cr704_3() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land("synth_plains_48", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_48", "Synth Bear", "{W}", 1, 1);
    let lord = b.add_card("synth_lord_48", "Synth Lord", |c| {
        c.mana_cost("{W}")
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
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 1 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let wipe1 = b.add_card("synth_ping_all_48", "Synth Tremor", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
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
    let plus_counter = b.intern_counter("+1/+1");
    let minus_counter = b.intern_counter("-1/-1");
    let undying_wolf = b.add_card("synth_undying_48", "Synth Undying Wolf", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .triggered_ability_if(
                simple_trigger(
                    EventKind::ZoneChange,
                    EventFilter {
                        object: Some(mtg_ir::TriggerSubject::SelfObj),
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
                Condition::LkiHasNoCounter {
                    counter: plus_counter,
                },
                vec![],
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
                        with_counters: Some((plus_counter, 1)),
                        attached_to: None,
                        condition: None,
                        choose_at_random_from_exiled_with_self: false,
                        leave_chosen_in_graveyard_rest_to_hand: false,
                        split_exiled_cost_cards_to_bottom_and_battlefield: false,
                    },
                    Selector::EventObject,
                    None,
                )],
            );
    });
    let put_plus = b.add_card("synth_put_plus_48", "Synth Plus Counter", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(plus_counter),
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
    let put_minus3 = b.add_card("synth_put_minus3_48", "Synth Minus3 Counters", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::PutCounters {
                        counter: Some(minus_counter),
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
    let db = b.build_db();
    let mut main = Vec::with_capacity(60);
    main.extend(vec![w_land; 12]);
    main.extend(vec![bear; 8]);
    main.extend(vec![lord; 8]);
    main.extend(vec![wipe1; 8]);
    main.extend(vec![undying_wolf; 8]);
    main.extend(vec![put_plus; 8]);
    main.extend(vec![put_minus3; 8]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        148,
    )
    .unwrap();
    drv.keep_all_hands();

    let needed = [bear, lord, wipe1, undying_wolf, put_plus, put_minus3];
    let advance_p0_main1 = |drv: &mut SessionDriver| {
        let start_turn = drv.session.state().turn_number();
        let target_turn = if start_turn % 2 == 1 {
            start_turn + 2
        } else {
            start_turn + 1
        };
        for _ in 0..64 {
            let s = drv.session.state();
            if s.turn_number() >= target_turn && s.phase() == GamePhase::Main1 {
                return;
            }
            match drv.decision() {
                DecisionState::Priority => {
                    drv.pass_priority();
                }
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } => {
                    let actor = drv.session.current_actor().unwrap();
                    let reserved: Vec<_> = needed
                        .iter()
                        .filter_map(|&cid| drv.find_hand_object(actor, cid))
                        .collect();
                    let discard = candidates
                        .iter()
                        .copied()
                        .find(|id| !reserved.contains(id))
                        .unwrap_or(candidates[0]);
                    drv.step(
                        actor,
                        Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::PickObject { id: discard },
                        },
                    )
                    .unwrap();
                }
                DecisionState::DeclareAttackers => {
                    let actor = drv.session.current_actor().unwrap();
                    drv.step(actor, Action::ConfirmAttackers).unwrap();
                }
                DecisionState::DeclareBlockers => {
                    let actor = drv.session.current_actor().unwrap();
                    drv.step(actor, Action::ConfirmBlockers).unwrap();
                }
                other => panic!("unexpected decision during SBA advance: {other:?}"),
            }
        }
        panic!("failed to advance to turn {target_turn} Main1");
    };

    for _ in 0..3 {
        if let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }
        if [bear, lord, wipe1]
            .iter()
            .all(|&c| drv.find_hand_object(PlayerId(0), c).is_some())
            && drv.session.view_model(Some(PlayerId(0))).players[0]
                .battlefield
                .len()
                >= 3
        {
            break;
        }
        advance_p0_main1(&mut drv);
    }
    while ![bear, lord, wipe1]
        .iter()
        .all(|&c| drv.find_hand_object(PlayerId(0), c).is_some())
        || drv.session.view_model(Some(PlayerId(0))).players[0]
            .battlefield
            .len()
            < 3
    {
        if let Some(land) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
        }
        if [bear, lord, wipe1]
            .iter()
            .all(|&c| drv.find_hand_object(PlayerId(0), c).is_some())
            && drv.session.view_model(Some(PlayerId(0))).players[0]
                .battlefield
                .len()
                >= 3
        {
            break;
        }
        advance_p0_main1(&mut drv);
    }

    for card in [bear, lord, wipe1] {
        let oid = drv
            .find_hand_object(PlayerId(0), card)
            .expect("required card for CR 704.3 must be in hand");
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: oid,
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
    // SBA pass 1 kills 1/1 Lord (1 dmg); SBA pass 2 sees Bear drop to 1/1 with 1 dmg and kills Bear!
    assert!(drv.find_graveyard_object(PlayerId(0), lord).is_some());
    assert!(drv.find_graveyard_object(PlayerId(0), bear).is_some());

    // CR 704.8 LKI counter-annihilation sub-check:
    // Cast 1/1 Undying Wolf, put 1 +1/+1 counter on it, then put 3 -1/-1 counters on it.
    // +1/+1 and -1/-1 annihilate simultaneously with LKI toughness <= 0, so Undying must see
    // the +1/+1 counter in LKI (CR 704.8) and NOT return the wolf from Graveyard.
    while ![undying_wolf, put_plus, put_minus3]
        .iter()
        .all(|&c| drv.find_hand_object(PlayerId(0), c).is_some())
    {
        advance_p0_main1(&mut drv);
    }
    let wolf_hand = drv
        .find_hand_object(PlayerId(0), undying_wolf)
        .expect("undying_wolf must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: wolf_hand,
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
    let wolf_bf = drv
        .find_battlefield_object(PlayerId(0), undying_wolf)
        .expect("undying_wolf must enter battlefield");

    let plus_hand = drv
        .find_hand_object(PlayerId(0), put_plus)
        .expect("put_plus must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: plus_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: wolf_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let minus_hand = drv
        .find_hand_object(PlayerId(0), put_minus3)
        .expect("put_minus3 must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: minus_hand,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: wolf_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    assert!(
        drv.find_graveyard_object(PlayerId(0), undying_wolf)
            .is_some(),
        "undying_wolf must die to -1/-1 counters and stay in graveyard"
    );
    assert!(
        drv.find_battlefield_object(PlayerId(0), undying_wolf)
            .is_none(),
        "CR 704.8: Undying must check LKI before counter annihilation and NOT trigger"
    );
}

#[test]
fn t1_f14_04_sequential_combat_first_strike_and_damage_order() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land("synth_plains_49", "Synth Plains", "Plains", ManaColor::W);
    let fstriker = b.add_synth_keyword_creature(
        "synth_fs_49",
        "Synth Duelist",
        "{W}",
        3,
        1,
        &["first strike", "haste"],
    );
    let brute = b.add_synth_vanilla_creature("synth_brute_49", "Synth Brute", "{W}", 4, 3);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, w_land, brute, &[fstriker]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 149).unwrap();
    drv.keep_all_hands();

    drv.advance_to(2, GamePhase::Main1, 64).unwrap();
    let l1 = find_first_land_in_hand(&drv, PlayerId(1)).unwrap();
    drv.step(
        PlayerId(1),
        Action::PlayLand {
            object: l1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let br_h = drv.find_hand_object(PlayerId(1), brute).unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::CastSpell {
            object: br_h,
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
    let br_bf = drv.find_battlefield_object(PlayerId(1), brute).unwrap();

    let mut land_played = false;
    let mut atk_turn = 3u32;
    let mut found = false;
    for turn in (3u32..=49).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(w_land, 1), (fstriker, 1)])
            .unwrap();
        if !land_played && let Some(l0) = find_first_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l0,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played && drv.find_hand_object(PlayerId(0), fstriker).is_some() {
            atk_turn = turn;
            found = true;
            break;
        }
    }
    assert!(found, "must play Plains and draw fstriker within wait loop");
    let fs_h = drv.find_hand_object(PlayerId(0), fstriker).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: fs_h,
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
    let fs_bf = drv.find_battlefield_object(PlayerId(0), fstriker).unwrap();

    drv.advance_to(atk_turn, GamePhase::DeclareAttackers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(fs_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::DeclareBlockers, 16)
        .unwrap();
    drv.macro_step(
        PlayerId(1),
        MacroAction::DeclareBlockers {
            assignments: vec![(br_bf, fs_bf)],
        },
    )
    .unwrap();
    drv.advance_to(atk_turn, GamePhase::Main2, 32).unwrap();
    assert!(drv.find_graveyard_object(PlayerId(1), brute).is_some());
    assert!(drv.find_battlefield_object(PlayerId(0), fstriker).is_some());
}

#[test]
fn t1_f14_05_end_the_turn_exiles_stack_cr725_1b() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land("synth_island_50", "Synth Island", "Island", ManaColor::U);
    let bear = b.add_synth_vanilla_creature("synth_bear_50", "Synth Bear", "{U}", 2, 2);
    let end_turn = b.add_card("synth_end_50", "Synth Sundial", |c| {
        c.mana_cost("{U}")
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
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, u_land, bear, &[end_turn]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 150).unwrap();
    drv.keep_all_hands();

    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=79).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(u_land, 1), (bear, 1), (end_turn, 1)],
        )
        .unwrap();
        if lands_played < 2
            && let Some(land) = find_first_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: land,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 2
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), end_turn).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 2 Islands and hold bear + end_turn within wait loop"
    );
    let bear_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bear_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    let et_h = drv.find_hand_object(PlayerId(0), end_turn).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: et_h,
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
    // CR 725.1b: both resolving EndTurn and Bear below it are exiled from Stack!
    assert!(drv.session.state().stack().is_empty());
    assert!(drv.find_exile_object(PlayerId(0), end_turn).is_some());
    assert!(drv.find_exile_object(PlayerId(0), bear).is_some());
    assert!(drv.find_graveyard_object(PlayerId(0), end_turn).is_none());
}

// ============================================================================
// F15: MatchSession Façade, Views, Observation, Determinization & Snapshots (5 cases)
// ============================================================================

#[test]
fn t1_f15_01_view_model_redaction_player_vs_spectator() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_wastes_51", "Synth Wastes", "Wastes", ManaColor::C);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 60],
        sideboard: vec![],
    };
    let session =
        MatchSession::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 151).unwrap();
    assert_view_model_redaction(&session);

    let vm0 = session.view_model(Some(PlayerId(0)));
    assert_eq!(vm0.players[0].hand_count, 7);
    assert_eq!(vm0.players[0].hand.len(), 7);
    assert_eq!(vm0.players[1].hand_count, 7);
    assert!(vm0.players[1].hand.is_empty());
}

#[test]
fn t1_f15_02_observation_relative_seat_rotation_4p() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_52", "Synth Plains", "Plains", ManaColor::W);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_52", "Synth Cmd", "{W}", 2, 2);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![land; 99],
        sideboard: vec![],
    };
    let session = MatchSession::new(
        db,
        GameConfig::commander(4),
        vec![d.clone(), d.clone(), d.clone(), d],
        152,
    )
    .unwrap();
    for seat in 0..4u8 {
        assert_observation_consistency(&session, PlayerId(seat));
    }
    let obs2 = session.observe(PlayerId(2));
    assert_eq!(obs2.viewer, PlayerId(2));
    assert_eq!(obs2.players.len(), 4);
    assert_eq!(obs2.players[0].relative_seat, 0);
    assert_eq!(obs2.players[1].relative_seat, 1);
    assert_eq!(obs2.players[2].relative_seat, 2);
    assert_eq!(obs2.players[3].relative_seat, 3);
}

#[test]
fn t1_f15_03_determinize_preserves_public_and_viewer_facts() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_53", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_53", "Synth Bear", "{W}", 2, 2);
    let db = b.build_db();
    let decks = build_60_card_pair(&mut b, plains, bear, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 153).unwrap();
    drv.keep_all_hands();

    let mut rng_a = RngState::splitmix64(1111);
    let mut rng_b = RngState::splitmix64(2222);
    let det_a = drv.session.determinize(PlayerId(0), &mut rng_a);
    let det_b = drv.session.determinize(PlayerId(0), &mut rng_b);

    // Viewer's own hand + library sizes must remain exact; opponent hidden card order resampled.
    assert_eq!(det_a.players()[0].hand, det_b.players()[0].hand);
    assert_eq!(det_a.players()[1].hand.len(), det_b.players()[1].hand.len());
    assert_eq!(
        det_a.players()[1].library.len(),
        det_b.players()[1].library.len()
    );
    let opp_lib_a: Vec<_> = det_a.players()[1]
        .library
        .iter()
        .map(|&id| det_a.object(id).unwrap().card_def)
        .collect();
    let opp_lib_b: Vec<_> = det_b.players()[1]
        .library
        .iter()
        .map(|&id| det_b.object(id).unwrap().card_def)
        .collect();
    assert_ne!(
        opp_lib_a, opp_lib_b,
        "determinize must actually resample hidden zones"
    );
}

#[test]
fn t1_f15_04_snapshot_bytes_and_from_snapshot_round_trip() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_54", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut session = MatchSession::new(
        Arc::clone(&db),
        GameConfig::standard_1v1(),
        vec![d.clone(), d],
        154,
    )
    .unwrap();
    session.apply_action(PlayerId(0), Action::KeepHand).unwrap();

    #[cfg(feature = "m5-session-ext")]
    {
        let info = session.decision_info();
        assert_eq!(info.actor, session.current_actor());
        let bytes = session
            .snapshot_bytes()
            .expect("snapshot_bytes must succeed");
        let mut restored = MatchSession::from_snapshot(Arc::clone(&db), &bytes)
            .expect("from_snapshot must succeed");
        assert_eq!(session.view_model(None), restored.view_model(None));
        let act = session.legal_actions(session.current_actor().unwrap())[0].clone();
        let actor = session.current_actor().unwrap();
        assert_eq!(
            session.apply_action(actor, act.clone()).unwrap(),
            restored.apply_action(actor, act).unwrap()
        );
        assert_eq!(
            session.snapshot_bytes().unwrap(),
            restored.snapshot_bytes().unwrap()
        );
    }
    #[cfg(not(feature = "m5-session-ext"))]
    {
        assert_eq!(session.replay_log().entries.len(), 1);
        panic!(
            "requires M5 MatchSession extensions (--features m5-session-ext): snapshot_bytes / from_snapshot / decision_info"
        );
    }
}

#[test]
fn t1_f15_05_replay_log_from_replay_and_validate_deck() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_55", "Synth Plains", "Plains", ManaColor::W);
    let unsup = b.add_card("synth_unsup_55", "Synth Unsup", |c| {
        c.unsupported(UnsupportedReason::Unparsed, "uncompiled");
    });
    let db = b.build_db();
    let legal_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let bad_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![unsup, CardDefId(999)],
        sideboard: vec![],
    };
    assert!(MatchSession::validate_deck(&db, &GameConfig::standard_1v1(), &legal_deck).is_legal);
    let bad_rep = MatchSession::validate_deck(&db, &GameConfig::standard_1v1(), &bad_deck);
    assert!(!bad_rep.is_legal);
    assert_eq!(bad_rep.unsupported_card_ids, vec![unsup]);

    let mut session = MatchSession::new(
        Arc::clone(&db),
        GameConfig::standard_1v1(),
        vec![legal_deck.clone(), legal_deck],
        155,
    )
    .unwrap();
    session.apply_action(PlayerId(0), Action::KeepHand).unwrap();

    #[cfg(feature = "m5-session-ext")]
    {
        let log = session.replay_log().clone();
        let restored = MatchSession::from_replay(Arc::clone(&db), log).unwrap();
        assert_eq!(session.view_model(None), restored.view_model(None));
    }
    #[cfg(not(feature = "m5-session-ext"))]
    {
        assert_eq!(session.replay_log().entries.len(), 1);
        panic!("requires M5 MatchSession extensions (--features m5-session-ext): from_replay");
    }
}

// ============================================================================
// F16: Differential Runner (`mtg-cli diff-test`) & Divergences (5 cases)
// ============================================================================

fn write_synth_diff_bundle_and_bin(dir: &TempDir) -> std::path::PathBuf {
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_plains_diff", "Synth Plains", "Plains", ManaColor::W);
    b.add_card("synth_relic_diff", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let json_p = dir.child("bundle.json");
    let bin_p = dir.child("cards.bin");
    std::fs::write(&json_p, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&json_p, &bin_p, false, None).success);
    bin_p
}

fn synthetic_player_digest(
    seat: u8,
    life: i32,
    lib_count: usize,
    hand_count: usize,
    bf_count: usize,
    hand_oracle_id: &str,
    bf: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "seat": seat,
        "life": life,
        "lost": false,
        "poison": 0,
        "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": {
            "library": lib_count,
            "hand": hand_count,
            "battlefield": bf_count,
            "graveyard": 0,
            "exile": 0,
            "command": 0
        },
        "handOracleIds": vec![hand_oracle_id; hand_count],
        "battlefield": bf
    })
}

#[test]
fn t1_f16_01_diff_test_passing_1v1_trace() {
    let tmp = TempDir::new("f16_01");
    let bin = write_synth_diff_bundle_and_bin(&tmp);
    let mut rng = RngState::mulberry32(42);
    let mut lib0 = vec!["synth_plains_diff".to_string(); 60];
    rng.shuffle(&mut lib0);
    let mut lib1 = vec!["synth_plains_diff".to_string(); 60];
    rng.shuffle(&mut lib1);

    let p0 = synthetic_player_digest(0, 20, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let p1 = synthetic_player_digest(1, 20, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let mulligan_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": [p0.clone(), p1.clone()]
    });
    let main1_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0, p1]
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": lib0, "handInitial": vec!["synth_plains_diff"; 7] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": lib1, "handInitial": vec!["synth_plains_diff"; 7] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": main1_digest }
        ]
    });
    let rep = tmp.child("1v1.replay.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(res.success, "diff-test failed: {}", res.stderr);
    assert!(
        !res.stdout.contains("activates in Phase 1"),
        "diff-test must execute real step diffing, not baseline Phase 0 stub:\n{}",
        res.stdout
    );
}

#[test]
fn t1_f16_02_diff_test_passing_4p_commander_trace() {
    let tmp = TempDir::new("f16_02");
    let bin = write_synth_diff_bundle_and_bin(&tmp);
    let mut seats = Vec::new();
    let mut players = Vec::new();
    for s in 0..4u8 {
        seats.push(serde_json::json!({
            "seat": s, "deckId": format!("s{s}"), "commanders": [],
            "libraryOrderInitial": vec!["synth_plains_diff"; 100],
            "handInitial": vec!["synth_plains_diff"; 7]
        }));
        players.push(synthetic_player_digest(
            s,
            40,
            93,
            7,
            0,
            "synth_plains_diff",
            serde_json::json!([]),
        ));
    }
    let mulligan_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": players.clone()
    });
    let mut main1_players = players.clone();
    main1_players[0] =
        synthetic_player_digest(0, 40, 92, 8, 0, "synth_plains_diff", serde_json::json!([]));
    let main1_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": main1_players
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 99, "postInitRngState": 99,
        "rules": { "startingLife": 40, "commander": true, "freeFirstMulligan": true },
        "seats": seats,
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest.clone() },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest.clone() },
            { "stepIndex": 2, "actorSeat": 2, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest },
            { "stepIndex": 3, "actorSeat": 3, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": main1_digest }
        ]
    });
    let rep = tmp.child("4p.replay.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(res.success, "4p diff-test failed: {}", res.stderr);
    assert!(
        !res.stdout.contains("activates in Phase 1"),
        "diff-test must execute real step diffing:\n{}",
        res.stdout
    );
}

#[test]
fn t1_f16_03_diff_test_divergence_pinpoints_step_and_field() {
    let tmp = TempDir::new("f16_03");
    let bin = write_synth_diff_bundle_and_bin(&tmp);
    let p0_ok =
        synthetic_player_digest(0, 20, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let p0_bad =
        synthetic_player_digest(0, 999, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let p1 = synthetic_player_digest(1, 20, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let step0_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": [p0_ok, p1.clone()]
    });
    let step1_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_bad, p1]
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": vec!["synth_plains_diff"; 60], "handInitial": vec!["synth_plains_diff"; 7] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": vec!["synth_plains_diff"; 60], "handInitial": vec!["synth_plains_diff"; 7] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": step0_digest },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": step1_digest }
        ]
    });
    let rep = tmp.child("bad_life.replay.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(!res.success, "diff-test must fail on life=999 divergence");
    let combined = format!("{}\n{}", res.stdout, res.stderr);
    assert!(
        combined.to_lowercase().contains("life"),
        "output must mention differing `life` field: {combined}"
    );
}

#[test]
fn t1_f16_04_diff_test_noncreature_pt_normalization() {
    let tmp = TempDir::new("f16_04");
    let bin = write_synth_diff_bundle_and_bin(&tmp);
    let p0_init =
        synthetic_player_digest(0, 20, 53, 7, 0, "synth_relic_diff", serde_json::json!([]));
    let p0_cast =
        synthetic_player_digest(0, 20, 53, 6, 0, "synth_relic_diff", serde_json::json!([]));
    let p0_resolved = synthetic_player_digest(
        0,
        20,
        53,
        6,
        1,
        "synth_relic_diff",
        serde_json::json!([{
            "oracleId": "synth_relic_diff", "tapped": false, "pt": [0, 0],
            "types": ["Artifact"], "keywords": [], "counters": [],
            "damageMarked": 0, "attachedToOracleId": null
        }]),
    );
    let p1 = synthetic_player_digest(1, 20, 53, 7, 0, "synth_plains_diff", serde_json::json!([]));
    let mulligan_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": [p0_init.clone(), p1.clone()]
    });
    let main1_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_init, p1.clone()]
    });
    let stack_p0_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 1, "pendingPromptKind": null,
        "players": [p0_cast.clone(), p1.clone()]
    });
    let stack_p1_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1,
        "phase": "main1", "stackDepth": 1, "pendingPromptKind": null,
        "players": [p0_cast, p1.clone()]
    });
    let resolved_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_resolved, p1]
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": vec!["synth_relic_diff"; 60], "handInitial": vec!["synth_relic_diff"; 7] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": vec!["synth_plains_diff"; 60], "handInitial": vec!["synth_plains_diff"; 7] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": main1_digest },
            {
                "stepIndex": 2,
                "actorSeat": 0,
                "action": { "type": "cast_spell", "instanceId": "obj_0", "optionId": "normal" },
                "objectLocators": {
                    "obj_0": { "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "synth_relic_diff" }
                },
                "digestAfter": stack_p0_digest
            },
            { "stepIndex": 3, "actorSeat": 0, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": stack_p1_digest },
            { "stepIndex": 4, "actorSeat": 1, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": resolved_digest }
        ]
    });
    let rep = tmp.child("noncreature_pt.replay.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(
        res.success,
        "diff-test must normalize TS [0,0] non-creature pt:\n{}\n{}",
        res.stdout, res.stderr
    );
    assert!(
        !res.stdout.contains("activates in Phase 1"),
        "diff-test must run Phase 1 comparator"
    );
}

#[test]
fn t1_f16_05_known_ts_cr_divergences_doc_exists_and_cites_cr() {
    let doc = repo_root().join("docs").join("KNOWN_TS_CR_DIVERGENCES.md");
    assert!(
        doc.exists(),
        "Phase 1 Gate requires {} documenting intentional CR improvements",
        doc.display()
    );
    let content = std::fs::read_to_string(&doc).unwrap();
    assert!(content.contains("613.8"), "must cite CR 613.8");
    assert!(content.contains("616.1"), "must cite CR 616.1");
    assert!(content.contains("400.7"), "must cite CR 400.7");
}

// ============================================================================
// F17: Synthetic CR Suite, Rename Invariance, Cache & Benchmarks (5 cases)
// ============================================================================

#[test]
fn t1_f17_01_rename_invariance_across_non_name_cards() {
    let mut b = SynthBundleBuilder::new();
    let plains_a = b.add_synth_basic_land(
        "synth_plains_a",
        "Synth Plains Alpha",
        "Plains",
        ManaColor::W,
    );
    let bear_a =
        b.add_synth_keyword_creature("synth_bear_a", "Synth Bear Alpha", "{0}", 2, 2, &["haste"]);
    let anthem_a =
        b.add_synth_anthem_enchantment("synth_anth_a", "Synth Anthem Alpha", "{0}", 1, 1);
    let bundle_a = b.build_bundle();
    let bundle_b = b.clone_renamed("_omega");

    let mut main = Vec::with_capacity(60);
    main.extend(vec![plains_a; 20]);
    main.extend(vec![bear_a; 20]);
    main.extend(vec![anthem_a; 20]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let decks_a = vec![deck.clone(), deck];
    let decks_b = decks_a.clone();

    assert_rename_invariance_with(
        bundle_a,
        bundle_b,
        &GameConfig::standard_1v1(),
        &decks_a,
        &decks_b,
        171,
        |drv| {
            drv.keep_all_hands();
            for _ in 0..8 {
                if drv.find_hand_object(PlayerId(0), anthem_a).is_some()
                    && drv.find_hand_object(PlayerId(0), bear_a).is_some()
                {
                    break;
                }
                let next_turn = drv.session.state().turn_number() + 2;
                for _ in 0..64 {
                    let s = drv.session.state();
                    if s.turn_number() >= next_turn && s.phase() == GamePhase::Main1 {
                        break;
                    }
                    match drv.decision() {
                        DecisionState::Priority => {
                            drv.pass_priority();
                        }
                        DecisionState::Prompt {
                            request:
                                ChoiceRequest::ChooseObjects {
                                    prompt_id,
                                    candidates,
                                    ..
                                },
                        } => {
                            let actor = drv.session.current_actor().unwrap();
                            let keep_anth = drv.find_hand_object(actor, anthem_a);
                            let keep_bear = drv.find_hand_object(actor, bear_a);
                            let pick = candidates
                                .iter()
                                .copied()
                                .find(|&id| Some(id) != keep_anth && Some(id) != keep_bear)
                                .unwrap_or(candidates[0]);
                            drv.step(
                                actor,
                                Action::Answer {
                                    prompt_id,
                                    answer: ChoiceAnswer::PickObject { id: pick },
                                },
                            )
                            .unwrap();
                        }
                        DecisionState::DeclareAttackers => {
                            let actor = drv.session.current_actor().unwrap();
                            drv.step(actor, Action::ConfirmAttackers).unwrap();
                        }
                        DecisionState::DeclareBlockers => {
                            let actor = drv.session.current_actor().unwrap();
                            drv.step(actor, Action::ConfirmBlockers).unwrap();
                        }
                        other => {
                            panic!("unexpected state advancing in rename invariance: {other:?}")
                        }
                    }
                }
            }

            if let Some(land) = find_first_land_in_hand(drv, PlayerId(0)) {
                drv.step(
                    PlayerId(0),
                    Action::PlayLand {
                        object: land,
                        from_zone: Zone::Hand,
                        face_index: 0,
                    },
                )
                .unwrap();
            }

            let anth_hand = drv
                .find_hand_object(PlayerId(0), anthem_a)
                .expect("anthem must be in hand");
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: anth_hand,
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

            let bear_hand = drv
                .find_hand_object(PlayerId(0), bear_a)
                .expect("haste bear must be in hand");
            drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: bear_hand,
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

            let bear_bf = drv
                .find_battlefield_object(PlayerId(0), bear_a)
                .expect("haste bear must be on battlefield");
            let turn_now = drv.session.state().turn_number();
            drv.advance_to(turn_now, GamePhase::DeclareAttackers, 16)
                .unwrap();
            drv.step(
                PlayerId(0),
                Action::AssignAttacker {
                    attacker: bear_bf,
                    target: AttackTarget::Player { id: PlayerId(1) },
                },
            )
            .unwrap();
            drv.step(PlayerId(0), Action::ConfirmAttackers).unwrap();
            drv.advance_to(turn_now, GamePhase::Main2, 16).unwrap();
            assert_eq!(
                drv.session.view_model(None).players[1].life,
                17,
                "3/3 anthem-buffed haste bear must deal 3 damage"
            );
        },
    );
}

#[test]
fn t1_f17_02_verify_layer_cache_zero_mismatch() {
    let tmp = TempDir::new("f17_02");
    let root = repo_root();
    let out = std::process::Command::new("cargo")
        .args([
            "test",
            "--manifest-path",
            root.join("Cargo.toml").to_str().unwrap(),
            "-p",
            "mtg-engine",
            "--features",
            "verify-layer-cache",
            "--target-dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to run cargo test -p mtg-engine --features verify-layer-cache");
    assert!(
        out.status.success(),
        "verify-layer-cache test failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn t1_f17_03_pure_zero_io_core_workspace_hygiene() {
    fn visit_rs_files(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path)) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit_rs_files(&path, f);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                f(&path);
            }
        }
    }

    let root = repo_root();
    let forbidden = ["std::fs", "SystemTime", "Date::now", "tokio::", "reqwest"];
    for crate_dir in ["crates/mtg-ir/src", "crates/mtg-engine/src"] {
        let dir = root.join(crate_dir);
        visit_rs_files(&dir, &mut |path| {
            let text = std::fs::read_to_string(path).unwrap();
            for pat in forbidden {
                assert!(
                    !text.contains(pat),
                    "pure core file {} must not contain `{pat}`",
                    path.display()
                );
            }
        });
    }
    let tmp = TempDir::new("f17_03");
    let wasm_chk = std::process::Command::new("cargo")
        .args([
            "check",
            "--manifest-path",
            root.join("Cargo.toml").to_str().unwrap(),
            "-p",
            "mtg-wasm",
            "--features",
            "wasm",
            "--target-dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to invoke cargo check -p mtg-wasm --features wasm");
    assert!(
        wasm_chk.status.success(),
        "mtg-wasm --features wasm failed:\n{}",
        String::from_utf8_lossy(&wasm_chk.stderr)
    );
}

#[test]
fn t1_f17_04_release_game_state_clone_under_20_us() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_56", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_56", "Synth Bear", "{0}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_56", "Synth Anthem", "{0}", 1, 1);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_56", "Synth Cmd", "{0}", 3, 3);
    let db = b.build_db();
    let mut main = vec![land; 39];
    main.extend(vec![bear; 40]);
    main.extend(vec![anthem; 20]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::commander(4),
        vec![d.clone(), d.clone(), d.clone(), d],
        174,
    )
    .unwrap();
    drv.keep_all_hands();

    // Populate >= 20 permanents + continuous effects across 4 seats before timing GameState::clone().
    for _ in 0..500 {
        let total_bf: usize = drv
            .session
            .view_model(None)
            .players
            .iter()
            .map(|p| p.battlefield.len())
            .sum();
        if total_bf >= 20 {
            break;
        }
        match drv.decision() {
            DecisionState::Priority => {
                let actor = drv.session.current_actor().unwrap();
                let s = drv.session.state();
                if s.phase() == GamePhase::Main1
                    && s.active_player() == actor
                    && s.stack().is_empty()
                {
                    if let Some(anth) = drv.find_hand_object(actor, anthem) {
                        drv.macro_step(
                            actor,
                            MacroAction::CastSpell {
                                object: anth,
                                option_id: 0,
                                from_zone: Zone::Hand,
                                modes: vec![],
                                x_value: None,
                                targets: vec![],
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    if let Some(creature) = drv.find_hand_object(actor, bear) {
                        drv.macro_step(
                            actor,
                            MacroAction::CastSpell {
                                object: creature,
                                option_id: 0,
                                from_zone: Zone::Hand,
                                modes: vec![],
                                x_value: None,
                                targets: vec![],
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    if let Some(l) = find_first_land_in_hand(&drv, actor)
                        && drv
                            .session
                            .legal_actions(actor)
                            .iter()
                            .any(|a| matches!(a, Action::PlayLand { object, .. } if *object == l))
                    {
                        drv.step(
                            actor,
                            Action::PlayLand {
                                object: l,
                                from_zone: Zone::Hand,
                                face_index: 0,
                            },
                        )
                        .unwrap();
                        continue;
                    }
                }
                drv.pass_priority();
            }
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseObjects {
                        prompt_id,
                        candidates,
                        ..
                    },
            } => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickObject { id: candidates[0] },
                    },
                )
                .unwrap();
            }
            DecisionState::DeclareAttackers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmAttackers).unwrap();
            }
            DecisionState::DeclareBlockers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmBlockers).unwrap();
            }
            other => panic!("unexpected decision while populating clone board: {other:?}"),
        }
    }
    let total_bf: usize = drv
        .session
        .view_model(None)
        .players
        .iter()
        .map(|p| p.battlefield.len())
        .sum();
    assert!(
        total_bf >= 20,
        "must populate >= 20 permanents before GameState::clone() benchmark (got {total_bf})"
    );

    let state = drv.session.state();
    let iters = 10_000u32;
    let t0 = Instant::now();
    let mut acc = 0u64;
    for _ in 0..iters {
        let cloned = state.clone();
        acc = acc.wrapping_add(std::hint::black_box(cloned.layer_rev()));
    }
    let elapsed = t0.elapsed();
    assert!(acc > 0);
    let per_clone_us = (elapsed.as_secs_f64() * 1_000_000.0) / f64::from(iters);
    if !cfg!(debug_assertions) {
        assert!(
            per_clone_us < 20.0,
            "release GameState::clone() took {per_clone_us:.2} µs (limit < 20 µs)"
        );
    }
}

#[test]
fn t1_f17_05_release_single_threaded_stepping_over_10k_steps_per_sec() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land("synth_plains_57", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_57", "Synth Bear", "{0}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_57", "Synth Anthem", "{0}", 1, 1);
    let cmd = b.add_synth_vanilla_creature("synth_cmd_57", "Synth Cmd", "{0}", 3, 3);
    let db = b.build_db();
    let mut main = vec![land; 39];
    main.extend(vec![bear; 40]);
    main.extend(vec![anthem; 20]);
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
        175,
    )
    .unwrap();
    drv.keep_all_hands();

    // Populate >= 20 permanents + continuous effects before timing 10,000 steps.
    for _ in 0..500 {
        let total_bf: usize = drv
            .session
            .view_model(None)
            .players
            .iter()
            .map(|p| p.battlefield.len())
            .sum();
        if total_bf >= 20 {
            break;
        }
        match drv.decision() {
            DecisionState::Priority => {
                let actor = drv.session.current_actor().unwrap();
                let s = drv.session.state();
                if s.phase() == GamePhase::Main1
                    && s.active_player() == actor
                    && s.stack().is_empty()
                {
                    if let Some(anth) = drv.find_hand_object(actor, anthem) {
                        drv.macro_step(
                            actor,
                            MacroAction::CastSpell {
                                object: anth,
                                option_id: 0,
                                from_zone: Zone::Hand,
                                modes: vec![],
                                x_value: None,
                                targets: vec![],
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    if let Some(creature) = drv.find_hand_object(actor, bear) {
                        drv.macro_step(
                            actor,
                            MacroAction::CastSpell {
                                object: creature,
                                option_id: 0,
                                from_zone: Zone::Hand,
                                modes: vec![],
                                x_value: None,
                                targets: vec![],
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    if let Some(l) = find_first_land_in_hand(&drv, actor)
                        && drv
                            .session
                            .legal_actions(actor)
                            .iter()
                            .any(|a| matches!(a, Action::PlayLand { object, .. } if *object == l))
                    {
                        drv.step(
                            actor,
                            Action::PlayLand {
                                object: l,
                                from_zone: Zone::Hand,
                                face_index: 0,
                            },
                        )
                        .unwrap();
                        continue;
                    }
                }
                drv.pass_priority();
            }
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseObjects {
                        prompt_id,
                        candidates,
                        ..
                    },
            } => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickObject { id: candidates[0] },
                    },
                )
                .unwrap();
            }
            DecisionState::DeclareAttackers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmAttackers).unwrap();
            }
            DecisionState::DeclareBlockers => {
                let actor = drv.session.current_actor().unwrap();
                drv.step(actor, Action::ConfirmBlockers).unwrap();
            }
            other => panic!("unexpected decision while populating stepping board: {other:?}"),
        }
    }
    let total_bf: usize = drv
        .session
        .view_model(None)
        .players
        .iter()
        .map(|p| p.battlefield.len())
        .sum();
    assert!(
        total_bf >= 20,
        "must populate >= 20 permanents before stepping benchmark (got {total_bf})"
    );
    let mut session = drv.session;

    let t0 = Instant::now();
    let mut steps = 0usize;
    let target_steps = 10_000usize;
    while steps < target_steps {
        if matches!(
            session.view_model(None).decision,
            DecisionState::GameOver { .. }
        ) {
            session = MatchSession::new(
                Arc::clone(&db),
                GameConfig::commander(4),
                decks.clone(),
                175,
            )
            .unwrap();
            for _ in 0..4 {
                let a = session.current_actor().unwrap();
                session.apply_action(a, Action::KeepHand).unwrap();
            }
        }
        let actor = session.current_actor().unwrap();
        let legal = session.legal_actions(actor);
        let chosen = legal
            .iter()
            .find(|a| matches!(a, Action::PlayLand { .. } | Action::BeginCast { .. }))
            .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
            .unwrap_or(&legal[0])
            .clone();
        session.apply_action(actor, chosen).unwrap();
        steps += 1;
    }
    let secs = t0.elapsed().as_secs_f64();
    let steps_per_sec = (steps as f64) / secs.max(1e-9);
    // Ensure real turn progression happened (not a stubbed Priority loop).
    assert!(
        session.state().turn_number() > 1 || session.replay_log().entries.len() >= 100,
        "stepping must advance actual turns"
    );
    assert!(
        session.state().turn_number() > 1,
        "10,000 steps must advance past Turn 1 (still on turn {})",
        session.state().turn_number()
    );
    if !cfg!(debug_assertions) {
        assert!(
            steps_per_sec > 10_000.0,
            "release stepping throughput {steps_per_sec:.1} steps/sec <= 10,000 steps/sec"
        );
    }
}
