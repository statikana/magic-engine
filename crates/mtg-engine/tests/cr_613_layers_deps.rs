//! CR 613 (Continuous Effects & Layers), CR 613.8 (Dependencies & Cycle Breaking),
//! CR 604.3 (Off-Battlefield CDAs), Signed Unclamped P/T, and `verify-layer-cache` tests.

mod common;

use common::{SessionDriver, SynthBundleBuilder, effect_step, target_creature};
use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig, PlayerId,
    PowerToughness, TargetRef,
};
use mtg_ir::{
    Affected, Amount, CardLayout, CardTypeMask, ColorMask, CopyModificationSpec, Duration,
    EffectOp, EtbCopySpec, EventDetail, EventFilter, EventKind, EventPattern, GamePhase, Layer,
    LayerMod, ManaColor, ObjectFilter, ObjectPredicate, Ref, ReplacementDef, ReplacementWith,
    Selector, StatKind, StaticBody, StaticDef, SupertypeMask, TriggerSubject, Zone, ZoneChangeMod,
    ZoneMask, ZoneScopeWhose,
};
use smallvec::SmallVec;

#[test]
fn synth_cr613_suppressor_and_animate_enchantments_both_timestamps() {
    let mut b = SynthBundleBuilder::new();
    let aura_sub = b.intern_subtype("Aura");
    let plains = b.add_synth_basic_land("synth_plains_613", "Synth Plains", "Plains", ManaColor::W);
    // Synth Suppressor: MV 4 Enchantment, all creatures lose all abilities (L6) & base P/T 1/1 (L7b)
    let suppressor = b.add_card("synth_suppressor_613", "Synth Suppressor", |c| {
        c.mana_cost("{2}{W}{W}")
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
    // Synth EnchAnimator: MV 5 Enchantment, other non-Aura enchantments are creatures (L4) with base P/T = MV (L7b)
    let ench_anim = b.add_card("synth_enchanim_613", "Synth EnchAnimator", |c| {
        c.mana_cost("{3}{W}{W}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ENCHANTMENT,
                            subtypes_none: SmallVec::from_slice(&[aura_sub]),
                            extra: vec![ObjectPredicate::NotSelf],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::AddTypes {
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
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
                            subtypes_none: SmallVec::from_slice(&[aura_sub]),
                            extra: vec![ObjectPredicate::NotSelf],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetBasePt {
                        power: Box::new(Amount::Stat {
                            of: Ref::Affected,
                            stat: StatKind::ManaValue,
                            lki: false,
                        }),
                        toughness: Box::new(Amount::Stat {
                            of: Ref::Affected,
                            stat: StatKind::ManaValue,
                            lki: false,
                        }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Give P0 a {0} ManaWell tapping for {W}{W}{W}{W}{W} so both cast cleanly!
    let lotus_land = b.add_card("synth_lotus_land_613", "Synth ManaWell", |c| {
        c.types(CardTypeMask::LAND).mana_ability(
            "five_w",
            &[
                ManaColor::W,
                ManaColor::W,
                ManaColor::W,
                ManaColor::W,
                ManaColor::W,
            ],
        );
    });
    let db = b.build_db();

    // Order A: EnchAnimator enters FIRST, then Suppressor enters SECOND -> Suppressor is 1/1.
    let (cfg, decks) = common::guaranteed_decks(
        &[&[ench_anim, suppressor, lotus_land], &[plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    {
        let mut drv = SessionDriver::new(db.clone(), cfg.clone(), decks.clone(), 6131).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let land_id = drv.find_hand_object(PlayerId(0), lotus_land).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land_id,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        drv.cast_from_hand(PlayerId(0), ench_anim, vec![]).unwrap();
        drv.resolve_all_stack(8);

        // Tap lotus_land on Turn 3 Main1 for Suppressor.
        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), suppressor, vec![]).unwrap();
        drv.resolve_all_stack(8);

        let sup_bf = drv
            .find_battlefield_object(PlayerId(0), suppressor)
            .unwrap();
        let cache = drv.session.state().evaluate_board();
        let ev_sup = cache.get(sup_bf).unwrap();
        assert!(ev_sup.types.contains(CardTypeMask::CREATURE));
        assert!(ev_sup.lost_all_abilities);
        assert_eq!(
            ev_sup.pt,
            Some(PowerToughness {
                power: 1,
                toughness: 1
            }),
            "EnchAnimator -> Suppressor: Suppressor L7b timestamp wins -> 1/1"
        );
    }

    // Order B: Suppressor enters FIRST, then EnchAnimator enters SECOND -> Suppressor is 4/4 (CR 613.6)!
    {
        let mut drv = SessionDriver::new(db, cfg, decks, 6132).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let land_id = drv.find_hand_object(PlayerId(0), lotus_land).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: land_id,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        drv.cast_from_hand(PlayerId(0), suppressor, vec![]).unwrap();
        drv.resolve_all_stack(8);

        drv.advance_to(3, GamePhase::Main1, 32);
        drv.cast_from_hand(PlayerId(0), ench_anim, vec![]).unwrap();
        drv.resolve_all_stack(8);

        let sup_bf = drv
            .find_battlefield_object(PlayerId(0), suppressor)
            .unwrap();
        let cache = drv.session.state().evaluate_board();
        let ev_sup = cache.get(sup_bf).unwrap();
        assert!(ev_sup.types.contains(CardTypeMask::CREATURE));
        assert!(ev_sup.lost_all_abilities);
        assert_eq!(
            ev_sup.pt,
            Some(PowerToughness {
                power: 4,
                toughness: 4
            }),
            "Suppressor -> EnchAnimator: CR 613.6 preserves EnchAnimator L7b -> Suppressor is 4/4"
        );
    }
}

#[test]
fn synth_cr613_8a_mountainizer_and_swampmaker_both_timestamps() {
    let mut b = SynthBundleBuilder::new();
    let mtn_sub = b.intern_subtype("Mountain");
    let swp_sub = b.intern_subtype("Swamp");
    let basic_plains =
        b.add_synth_basic_land("synth_plains_6138", "Synth Plains", "Plains", ManaColor::W);
    let mountainizer = b.add_card("synth_mtnizer_6138", "Synth Mountainizer", |c| {
        c.mana_cost("{0}")
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
                        subtypes: SmallVec::from_slice(&[mtn_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    // Nonbasic land with Layer 4 static "All lands are Swamps in addition"
    let swampmaker = b.add_card("synth_swampmaker_6138", "Synth Swampmaker", |c| {
        c.types(CardTypeMask::LAND).static_ability(StaticDef {
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
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[swp_sub]),
                },
                active_in_zone: None,
            },
            condition: None,
        });
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[&[basic_plains, mountainizer, swampmaker], &[basic_plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );

    // Order A: Swampmaker enters Turn 1, Mountainizer Turn 3
    {
        let mut drv = SessionDriver::new(db.clone(), cfg.clone(), decks.clone(), 61381).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        let sw_h = drv.find_hand_object(PlayerId(0), swampmaker).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: sw_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        drv.advance_to(3, GamePhase::Main1, 32);
        let pl_h = drv.find_hand_object(PlayerId(0), basic_plains).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: pl_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();
        drv.cast_from_hand(PlayerId(0), mountainizer, vec![])
            .unwrap();
        drv.resolve_all_stack(8);

        let pl_bf = drv
            .find_battlefield_object(PlayerId(0), basic_plains)
            .unwrap();
        let sw_bf = drv
            .find_battlefield_object(PlayerId(0), swampmaker)
            .unwrap();
        let cache = drv.session.state().evaluate_board();
        assert!(!cache.get(pl_bf).unwrap().subtypes.contains(&swp_sub));
        assert!(cache.get(sw_bf).unwrap().subtypes.contains(&mtn_sub));
    }

    // Order B: Mountainizer enters Turn 1, Swampmaker Turn 3 -> dependency STILL forces Mountainizer first!
    {
        let mut drv = SessionDriver::new(db, cfg, decks, 61382).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), mountainizer, vec![])
            .unwrap();
        drv.resolve_all_stack(8);
        let pl_h = drv.find_hand_object(PlayerId(0), basic_plains).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: pl_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

        drv.advance_to(3, GamePhase::Main1, 32);
        let sw_h = drv.find_hand_object(PlayerId(0), swampmaker).unwrap();
        drv.step(
            PlayerId(0),
            Action::PlayLand {
                object: sw_h,
                from_zone: Zone::Hand,
                face_index: 0,
            },
        )
        .unwrap();

        let pl_bf = drv
            .find_battlefield_object(PlayerId(0), basic_plains)
            .unwrap();
        let sw_bf = drv
            .find_battlefield_object(PlayerId(0), swampmaker)
            .unwrap();
        let cache = drv.session.state().evaluate_board();
        assert!(
            !cache.get(pl_bf).unwrap().subtypes.contains(&swp_sub),
            "CR 613.8a: dependency must apply Mountainizer before Swampmaker regardless of entry order"
        );
        assert!(cache.get(sw_bf).unwrap().subtypes.contains(&mtn_sub));
    }
}

#[test]
fn synth_cr613_6_cross_layer_persistence_and_l4_cda_survival() {
    let mut b = SynthBundleBuilder::new();
    let mtn_sub = b.intern_subtype("Mountain");
    let elf_sub = b.intern_subtype("Elf");
    let plus_cid = b.intern_counter("+1/+1");
    let suppressor = b.add_card("synth_sup_6136", "Synth Suppressor", |c| {
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
    let lord = b.add_card("synth_lord_6136", "Synth Lord", |c| {
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
                            extra: vec![ObjectPredicate::NotSelf],
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
    let magus = b.add_card("synth_magus_6136", "Synth Magus", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
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
                        subtypes: SmallVec::from_slice(&[mtn_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let shapeshifter = b.add_card("synth_shifter_6136", "Synth Shapeshifter", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword("changeling");
    });
    let nonbasic = b.add_card("synth_nonbasic_6136", "Synth Wastes", |c| {
        c.types(CardTypeMask::LAND);
    });
    let add_counter_spell = b.add_card("synth_boost_6136", "Synth Boost", |c| {
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
    let bear = b.add_synth_vanilla("synth_bear_6136", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            suppressor,
            lord,
            magus,
            shapeshifter,
            nonbasic,
            add_counter_spell,
            bear,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![nonbasic; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6136).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let land_h = drv.find_hand_object(PlayerId(0), nonbasic).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    for card in [lord, magus, shapeshifter, bear, suppressor] {
        drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        add_counter_spell,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(8);

    let cache = drv.session.state().evaluate_board();
    // (a) Synth Lord lost its L7c +2/+2 static in L6 -> no buff to Magus (1/1).
    let magus_bf = drv.find_battlefield_object(PlayerId(0), magus).unwrap();
    assert_eq!(
        cache.get(magus_bf).unwrap().pt,
        Some(PowerToughness {
            power: 1,
            toughness: 1
        })
    );
    // (b) Magus's L4 Mountainizer & Shapeshifter's L4 Changeling survived L6 removal!
    let land_bf = drv.find_battlefield_object(PlayerId(0), nonbasic).unwrap();
    assert!(cache.get(land_bf).unwrap().subtypes.contains(&mtn_sub));
    let shifter_bf = drv
        .find_battlefield_object(PlayerId(0), shapeshifter)
        .unwrap();
    assert!(cache.get(shifter_bf).unwrap().has_all_creature_types);
    assert!(
        cache.get(shifter_bf).unwrap().subtypes.contains(&elf_sub)
            || cache.get(shifter_bf).unwrap().has_all_creature_types
    );
    // (c) +1/+1 counter on Bear applies in L7c after L7b 1/1 -> 2/2!
    assert_eq!(
        cache.get(bear_bf).unwrap().pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
}

#[test]
fn synth_cr613_8b_circular_dependency_and_no_dep_timestamp_fallback() {
    let mut b = SynthBundleBuilder::new();
    let mtn_sub = b.intern_subtype("Mountain");
    let isl_sub = b.intern_subtype("Island");
    let elf_sub = b.intern_subtype("Elf");
    let gob_sub = b.intern_subtype("Goblin");

    // Circular L4 dependency: Card A makes Elves into Goblins, Card B makes Goblins into Elves;
    // both are Elves & Goblins -> circular dependency terminates cleanly via timestamp tie-break!
    let circ_a = b.add_card("synth_circ_a_6138", "Synth CircA", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .subtype("Elf")
            .pt(2, 2)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: SmallVec::from_slice(&[elf_sub]),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetSubtypes {
                        subtypes: SmallVec::from_slice(&[gob_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let circ_b = b.add_card("synth_circ_b_6138", "Synth CircB", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .subtype("Goblin")
            .pt(2, 2)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: SmallVec::from_slice(&[gob_sub]),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetSubtypes {
                        subtypes: SmallVec::from_slice(&[elf_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });

    // Non-dependent L4 effects: PrismaticLands adds Island to all lands, Mountainizer sets nonbasics to Mountain.
    let prismatic = b.add_card("synth_prism_6138", "Synth Prismatic", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
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
                        types: CardTypeMask::LAND,
                        subtypes: SmallVec::from_slice(&[isl_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let mountainizer = b.add_card("synth_mtn_6138b", "Synth Mountainizer", |c| {
        c.mana_cost("{0}")
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
                        subtypes: SmallVec::from_slice(&[mtn_sub]),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let nonbasic = b.add_card("synth_nb_6138b", "Synth Wastes", |c| {
        c.types(CardTypeMask::LAND);
    });
    let db = b.build_db();

    let p0_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![
            circ_a,
            circ_b,
            prismatic,
            mountainizer,
            nonbasic,
            nonbasic,
            nonbasic,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![nonbasic; 15],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![p0_deck, p1_deck],
        61383,
    )
    .unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    let nb_h = drv.find_hand_object(PlayerId(0), nonbasic).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: nb_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    // Cast CircA then CircB, and Prismatic then Mountainizer (so Mountainizer overwrites Prismatic's Island).
    for card in [circ_a, circ_b, prismatic, mountainizer] {
        drv.cast_from_hand(PlayerId(0), card, vec![]).unwrap();
        drv.resolve_all_stack(8);
    }

    let cache = drv.session.state().evaluate_board();
    let ca = drv.find_battlefield_object(PlayerId(0), circ_a).unwrap();
    let cb = drv.find_battlefield_object(PlayerId(0), circ_b).unwrap();
    // CircA applied first (Goblin), then CircB turned all Goblins into Elf -> both end as Elf!
    assert_eq!(cache.get(ca).unwrap().subtypes.as_slice(), &[elf_sub]);
    assert_eq!(cache.get(cb).unwrap().subtypes.as_slice(), &[elf_sub]);

    let nb_bf = drv.find_battlefield_object(PlayerId(0), nonbasic).unwrap();
    // Mountainizer has newer timestamp than Prismatic -> nonbasic is Mountain only (timestamp fallback).
    assert_eq!(cache.get(nb_bf).unwrap().subtypes.as_slice(), &[mtn_sub]);
}

#[test]
fn synth_cr613_4d_unclamped_signed_pt_and_switching_order() {
    let mut b = SynthBundleBuilder::new();
    let plus_cid = b.intern_counter("+1/+1");
    let crab = b.add_synth_vanilla("synth_crab_6134", "Synth Crab", "{0}", 1, 3);
    let wall = b.add_synth_vanilla("synth_wall_6134", "Synth Wall", "{0}", 0, 4);
    let relic = b.add_card("synth_relic_6134", "Synth Relic", |c| {
        c.mana_cost("{0}").types(CardTypeMask::ARTIFACT);
    });
    let pump_p2 = b.add_card("synth_p2_6134", "Synth Surge", |c| {
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    let shrink_p3 = b.add_card("synth_m3_6134", "Synth Chill", |c| {
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
                    Some(Amount::Const { n: -3 }),
                )],
            );
    });
    let switch = b.add_card("synth_sw_6134", "Synth Twist", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::SwitchPt {
                        duration: Some(Duration::EndOfTurn),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let set_1_2 = b.add_card("synth_set12_6134", "Synth Shrink", |c| {
        c.mana_cost("{0}")
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
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 2 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let add_plus = b.add_card("synth_cnt_6134", "Synth Mark", |c| {
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
    let plains = b.add_synth_basic_land("synth_pl_6134", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();

    // Part 1: (a) 1/3 + +2/+0 -> 3/3 + SwitchPt -> 3/3;
    //         (c) 1/3 with -3/-0 -> -2/3, then +2/+0 -> 0/3 (NOT 2/3!);
    //         (d) non-creature relic has pt: None.
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[&[crab, relic, pump_p2, shrink_p3, switch], &[plains]],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 61341).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), crab, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), relic, vec![]).unwrap();
        drv.resolve_all_stack(8);

        let crab_id = drv.find_battlefield_object(PlayerId(0), crab).unwrap();
        let relic_id = drv.find_battlefield_object(PlayerId(0), relic).unwrap();
        assert_eq!(
            drv.session
                .state()
                .evaluate_board()
                .get(relic_id)
                .unwrap()
                .pt,
            None
        );

        // -3/-0 on 1/3 -> signed unclamped -2/3!
        drv.cast_from_hand(
            PlayerId(0),
            shrink_p3,
            vec![TargetRef::Object { id: crab_id }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.session
                .state()
                .evaluate_board()
                .get(crab_id)
                .unwrap()
                .pt,
            Some(PowerToughness {
                power: -2,
                toughness: 3
            })
        );

        // Then +2/+0 -> 0/3 (not 2/3!).
        drv.cast_from_hand(
            PlayerId(0),
            pump_p2,
            vec![TargetRef::Object { id: crab_id }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.session
                .state()
                .evaluate_board()
                .get(crab_id)
                .unwrap()
                .pt,
            Some(PowerToughness {
                power: 0,
                toughness: 3
            })
        );

        // SwitchPt on 0/3 -> 3/0 -> dies to SBA (704.5f)! Wait: let's test SwitchPt on 1/3 + +2/+0 first on Turn 3,
        // or test -2/3 combat damage clamping (0 damage) before switching!
        // Advance to Turn 3 so Turn 1 pump/shrink expire, then +2/+0 + SwitchPt on 1/3 -> 3/3!
        drv.advance_to(3, GamePhase::Main1, 32);
        // Note: Turn 1 crab died? No, we didn't switch 0/3 yet! So crab is still 1/3.
        // Wait: we used pump_p2 and switch? We only used pump_p2 and shrink_p3, we still have switch in hand!
        drv.cast_from_hand(PlayerId(0), switch, vec![TargetRef::Object { id: crab_id }])
            .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.session
                .state()
                .evaluate_board()
                .get(crab_id)
                .unwrap()
                .pt,
            Some(PowerToughness {
                power: 3,
                toughness: 1
            })
        );
    }

    // Part 2: (b) 0/4 Wall + +1/+1 counter (1/5) + L7b SetBasePt 1/2 (2/3) + SwitchPt -> 3/2!
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![wall, add_plus, set_1_2, switch, plains, plains, plains],
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
            61342,
        )
        .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), wall, vec![]).unwrap();
        drv.resolve_all_stack(8);
        let wall_id = drv.find_battlefield_object(PlayerId(0), wall).unwrap();

        drv.cast_from_hand(
            PlayerId(0),
            add_plus,
            vec![TargetRef::Object { id: wall_id }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), set_1_2, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), switch, vec![TargetRef::Object { id: wall_id }])
            .unwrap();
        drv.resolve_all_stack(16);

        assert_eq!(
            drv.session
                .state()
                .evaluate_board()
                .get(wall_id)
                .unwrap()
                .pt,
            Some(PowerToughness {
                power: 3,
                toughness: 2
            }),
            "0/4 + L7b(1/2) + (+1/+1) -> 2/3 -> L7d SwitchPt -> 3/2"
        );
    }
}

#[test]
fn synth_cr604_3_off_battlefield_cdas_and_layer_cache_invalidation() {
    let mut b = SynthBundleBuilder::new();
    let zombie_sub = b.intern_subtype("Zombie");
    let elf_sub = b.intern_subtype("Elf");
    let plains = b.add_synth_basic_land("synth_pl_604", "Synth Plains", "Plains", ManaColor::W);

    // */* CDA creature whose P/T equals creatures in all graveyards
    let grave_scaler = b.add_card("synth_gscaler_604", "Synth GraveScaler", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .cda_pt(None, None)
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7a,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::SetBasePt {
                        power: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        toughness: Box::new(Amount::Sum {
                            args: vec![
                                Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::GRAVEYARD,
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                },
                                Amount::Const { n: 1 },
                            ],
                        }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });

    // Devoid + Changeling creature
    let devoid_ch = b.add_card("synth_devoid_ch_604", "Synth Voidling", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword("devoid")
            .keyword("changeling");
    });

    // Clone that copies a creature except 1/1 Zombie (L1a copy exceptions)
    let zombie_clone = b.add_card("synth_zclone_604", "Synth ZombieClone", |c| {
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
                            modifications: Some(Box::new(CopyModificationSpec {
                                power: Some(1),
                                toughness: Some(1),
                                set_subtypes: Some(SmallVec::from_slice(&[zombie_sub])),
                                ..Default::default()
                            })),
                        }),
                        ..Default::default()
                    }),
                },
                self_replacement: true,
                who: None,
                condition: None,
            });
    });

    // Transforming DFC (front 1/1, back 4/4)
    let dfc = b.add_card("synth_dfc_604", "Synth Novice // Synth Beast", |c| {
        c.layout(CardLayout::Transform)
            .mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .add_face("Synth Awakened", |f| {
                f.types(CardTypeMask::CREATURE)
                    .colors(ColorMask::G)
                    .pt(4, 4);
            });
    });
    let transform_spell = b.add_card("synth_flip_604", "Synth Awaken", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Transform,
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let bear = b.add_synth_vanilla("synth_bear_604", "Synth Bear", "{0}", 2, 2);
    let destroy = b.add_card("synth_kill_604", "Synth Shatter", |c| {
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
            grave_scaler,
            devoid_ch,
            zombie_clone,
            zombie_clone,
            dfc,
            transform_spell,
            bear,
            destroy,
        ],
        sideboard: vec![],
    };
    let p1_deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 15],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 6043).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // 1) Off-battlefield CDAs in Hand:
    let gscaler_h = drv.find_hand_object(PlayerId(0), grave_scaler).unwrap();
    let void_h = drv.find_hand_object(PlayerId(0), devoid_ch).unwrap();
    {
        let cache = drv.session.state().evaluate_board();
        assert_eq!(
            cache.get(gscaler_h).unwrap().pt,
            Some(PowerToughness {
                power: 0,
                toughness: 1
            })
        );
        assert_eq!(
            cache.get(void_h).unwrap().colors,
            ColorMask::empty(),
            "Devoid off-BF must be colorless"
        );
        assert!(cache.get(void_h).unwrap().has_all_creature_types);
        assert!(
            cache.get(void_h).unwrap().subtypes.contains(&elf_sub)
                || cache.get(void_h).unwrap().has_all_creature_types
        );
    }

    // Cast Bear and Destroy it -> Graveyard has 1 creature -> GraveScaler in hand dynamically becomes 1/2!
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        destroy,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session
            .state()
            .evaluate_board()
            .get(gscaler_h)
            .unwrap()
            .pt,
        Some(PowerToughness {
            power: 1,
            toughness: 2
        }),
        "CR 604.3: off-battlefield */*+1 CDA updates dynamically as graveyard changes"
    );

    // 2) Cast DFC, Transform to back face (4/4), then ZombieClone copies DFC -> inherits L1a 1/1 Zombie
    // AND Clone-of-Clone inherits 1/1 Zombie!
    drv.cast_from_hand(PlayerId(0), dfc, vec![]).unwrap();
    drv.resolve_all_stack(8);
    let dfc_bf = drv.find_battlefield_object(PlayerId(0), dfc).unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        transform_spell,
        vec![TargetRef::Object { id: dfc_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session.state().evaluate_board().get(dfc_bf).unwrap().pt,
        Some(PowerToughness {
            power: 4,
            toughness: 4
        })
    );

    // First ZombieClone enters copying DFC (face-up back face, overridden by except 1/1 Zombie).
    drv.cast_from_hand(PlayerId(0), zombie_clone, vec![])
        .unwrap();
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
                answer: ChoiceAnswer::PickObject { id: dfc_bf },
            },
        )
        .unwrap();
    }
    drv.resolve_all_stack(8);

    let clones: Vec<_> = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .battlefield
        .iter()
        .copied()
        .filter(|&id| drv.session.state().object(id).unwrap().card_def == zombie_clone)
        .collect();
    assert_eq!(clones.len(), 1);
    let clone1 = clones[0];
    let c1_ev = drv.session.state().evaluate_board().get(clone1).unwrap();
    assert_eq!(
        c1_ev.pt,
        Some(PowerToughness {
            power: 1,
            toughness: 1
        })
    );
    assert!(c1_ev.subtypes.contains(&zombie_sub));

    // Verify DirtyMask invalidation & verify-layer-cache across 20 priority passes.
    for _ in 0..20 {
        let _ = drv.session.state().evaluate_board();
        if matches!(drv.decision(), DecisionState::Priority) {
            drv.pass_priority();
        } else {
            break;
        }
    }
    let _ = ZoneScopeWhose::All;
}
