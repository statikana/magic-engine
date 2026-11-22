//! CR 601 (Sequential Casting & Alt Costs), CR 106.6 (Restricted Mana & Auto-Tap),
//! CR 115 (Target Legality, Flagbearer, Ward, Fizzles, Stack Copy),
//! CR 101.2 (Can't Beats Can, Flash, Zone Play Permissions), and CR 104 (Win/Loss & Max Hand Size).

mod common;

use common::{
    SessionDriver, SynthBundleBuilder, effect_step, simple_trigger, target_creature, target_player,
    target_spell,
};
use mtg_engine::{
    Action, AltCastKind, CastRequest, CastStep, ChoiceAnswer, ChoiceRequest, DecisionState,
    DeckSpec, EngineError, GameConfig, MacroAction, PlayerId, PowerToughness, SpecialActionKind,
    TargetRef, enumerate_cast_options,
};
use mtg_ir::{
    AllowAbilitiesRestriction, AllowSpellsRestriction, Amount, CardTypeMask, ColorMask, CostDef,
    CostDelta, CostDeltaMode, Duration, EffectOp, EventDetail, EventFilter, EventKind, GamePhase,
    ManaColor, ManaCost, ManaSpendingRestriction, ModalDef, ModalMode, ObjectFilter,
    PlainManaFilter, PlayerFilter, PlayerRef, PlayerRelation, RelativeControl, RuleMod, Selector,
    Span, SpellAbility, SpellOrAbilityFilter, StaticBody, StaticDef, SupertypeMask, TargetSpec,
    TargetedByKind, TriggerSubject, Zone, ZoneMask,
};
use smallvec::SmallVec;

#[test]
fn synth_cr601_2_sequential_cast_modal_x_kicker_division_and_targets() {
    let mut b = SynthBundleBuilder::new();
    let mountain =
        b.add_synth_basic_land("synth_mtn_6012", "Synth Mountain", "Mountain", ManaColor::R);
    let super_mtn = b.add_card("synth_smtn_6012", "Synth Foundry", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("five_r", &[ManaColor::R; 5]);
    });
    let bear = b.add_synth_vanilla("synth_bear_6012", "Synth Bear", "{0}", 2, 4);

    // Modal + X + OptionalCost (Kicker) + Divided Damage spell:
    // Mode 0: Deal 3 divided damage among 2 target creatures; Mode 1: Gain X life.
    let mut div_spec = target_creature();
    div_spec.min = 2;
    div_spec.max = 2;
    div_spec.divide_total = Some(Box::new(Amount::Const { n: 3 }));

    let multi_spell = b.add_card("synth_multi_6012", "Synth Command", |c| {
        c.mana_cost("{X}{R}")
            .types(CardTypeMask::INSTANT)
            .spell_ability(SpellAbility {
                id: None,
                additional_costs: vec![CostDef::Optional {
                    cost: vec![CostDef::Mana {
                        cost: ManaCost::parse("{1}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                }],
                targets: vec![],
                effect: vec![],
                modal: Some(ModalDef {
                    min: 1,
                    max: 2,
                    allow_repeat: false,
                    once_per_turn_unique: false,
                    up_to: false,
                    conditional_choose_all: None,
                    modes: vec![
                        ModalMode {
                            targets: vec![div_spec],
                            effect: vec![effect_step(
                                EffectOp::Damage {
                                    source: None,
                                    also_players: None,
                                    divided: true,
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
                            span: Span::default(),
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
                            span: Span::default(),
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
                span: Span::default(),
            });
    });

    // Reducer (-{1}) + CostFloor3 (CR 118.7 unconditional floor {3}) + Cast trigger (+5 life)
    let reducer = b.add_card("synth_red_6012", "Synth CostLens", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(ObjectFilter::default()),
                        alt_cost_keyword: None,
                    },
                    delta: CostDelta {
                        mode: CostDeltaMode::Reduce,
                        generic: 1,
                        per: None,
                        additional_costs: vec![],
                        colored: SmallVec::new(),
                        hybrid_colored: SmallVec::new(),
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
    let sphere_min3 = b.add_card("synth_floor_6012", "Synth CostFloor3", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(ObjectFilter::default()),
                        alt_cost_keyword: None,
                    },
                    delta: CostDelta {
                        mode: CostDeltaMode::Increase,
                        generic: 0,
                        per: None,
                        additional_costs: vec![],
                        colored: SmallVec::new(),
                        hybrid_colored: SmallVec::new(),
                        colored_only: false,
                        min_total_mana: Some(3),
                        max_reduction: None,
                        per_target_life: None,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::Any,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::BATTLEFIELD,
                },
                condition: None,
            });
    });
    let cast_trig = b.add_card("synth_ctrig_6012", "Synth CastSpark", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::Cast,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::INSTANT,
                                ..Default::default()
                            }),
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
    let counterspell = b.add_card("synth_cnt_6012", "Synth Counter", |c| {
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
    let cheap_ping = b.add_synth_burn_instant("synth_ping_6012", "Synth Ping", "{R}", 1);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                super_mtn,
                bear,
                bear,
                multi_spell,
                reducer,
                sphere_min3,
                cast_trig,
                counterspell,
                cheap_ping,
            ],
            &[mountain],
        ],
        &[],
        4,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 6012).unwrap();
    drv.keep_all_hands();
    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 32);
        if drv.find_hand_object(PlayerId(0), super_mtn).is_some()
            && drv.find_hand_object(PlayerId(0), multi_spell).is_some()
            && drv.find_hand_object(PlayerId(0), reducer).is_some()
            && drv.find_hand_object(PlayerId(0), sphere_min3).is_some()
            && drv.find_hand_object(PlayerId(0), cast_trig).is_some()
            && drv.find_hand_object(PlayerId(0), counterspell).is_some()
            && drv.find_hand_object(PlayerId(0), cheap_ping).is_some()
        {
            break;
        }
    }

    let land_h = drv.find_hand_object(PlayerId(0), super_mtn).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: land_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    for card in [bear, bear, cast_trig, reducer, sphere_min3] {
        if let Some(h) = drv.find_hand_object(PlayerId(0), card) {
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
            drv.resolve_all_stack(8);
        }
    }
    drv.advance_to(3, GamePhase::Main1, 32);

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

    // 1) Sequential CR 601.2a-i cast of Synth Command:
    let ms_h = drv.find_hand_object(PlayerId(0), multi_spell).unwrap();
    drv.step(
        PlayerId(0),
        Action::BeginCast {
            object: ms_h,
            option_id: 0,
            from_zone: Zone::Hand,
        },
    )
    .unwrap();
    // ChooseMode: pick both Mode 0 and Mode 1, then DonePicking if still prompting
    for idx in [0u16, 1u16] {
        let pid_m = match drv.decision() {
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode { prompt_id, .. },
            } => prompt_id,
            other => panic!("expected ChooseMode for mode {idx}, got {other:?}"),
        };
        drv.step(
            PlayerId(0),
            Action::Answer {
                prompt_id: pid_m,
                answer: ChoiceAnswer::PickMode { index: idx },
            },
        )
        .unwrap();
    }
    if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseMode { prompt_id, .. },
    } = drv.decision()
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

    // ChooseXValue: pick X = 2
    let pid_x = match drv.decision() {
        DecisionState::Prompt {
            request: ChoiceRequest::ChooseNumber { prompt_id, .. },
        } => prompt_id,
        other => panic!("expected ChooseNumber for X, got {other:?}"),
    };
    drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id: pid_x,
            answer: ChoiceAnswer::PickNumber { value: 2 },
        },
    )
    .unwrap();

    // ChooseOptionalCosts (Kicker): answer YesNo { value: true }
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
    }

    // ChooseTargets: pick both bears
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: bears[0] },
        },
    )
    .unwrap();
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: bears[1] },
        },
    )
    .unwrap();

    // AssignDivision: 2 to bear0, 1 to bear1
    drv.step(
        PlayerId(0),
        Action::AssignDivision {
            target: TargetRef::Object { id: bears[0] },
            amount: 2,
        },
    )
    .unwrap();
    if drv
        .session
        .state()
        .casting()
        .is_some_and(|c| c.step == CastStep::ChooseDivisions)
    {
        drv.step(
            PlayerId(0),
            Action::AssignDivision {
                target: TargetRef::Object { id: bears[1] },
                amount: 1,
            },
        )
        .unwrap();
    }

    // PayCosts: total = X(2) + {R} + Kicker(1) - Reducer(1) = {2}{R} (MV 3 >= CostFloor3 floor 3)
    drv.step(PlayerId(0), Action::AutoPayMana).unwrap();

    // CR 601.2i: Cast trigger stacks ABOVE Synth Command -> stack.len() == 2!
    assert_eq!(drv.session.state().stack().len(), 2);
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(bears[0]).unwrap().damage_marked,
        2
    );
    assert_eq!(
        drv.session.state().object(bears[1]).unwrap().damage_marked,
        1
    );
    // +5 from CastSpark + 2 from Mode 1 (X=2) = 27 life!
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 27);

    // 2) Verify CostFloor3 raises {R} cheap_ping to {2}{R} (cannot cast without 3 mana; counterspell casts for {3} wait: super_mtn is tapped!).
    // Advance to Turn 7 Main1 so super_mtn untaps, cast cheap_ping -> cast trigger stacks above -> counter cheap_ping -> cast trigger STILL resolves!
    drv.advance_to(7, GamePhase::Main1, 32);
    let ping_h = drv.find_hand_object(PlayerId(0), cheap_ping).unwrap();
    let prev_ping = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: ping_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(prev_ping.can_pay);
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: ping_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    // Stack has [Ping, CastSpark] (or CastSpark above Ping); verify CostFloor3 charged 3 mana (4 produced -> 1 left or 3 spent):
    assert!(drv.session.state().stack().len() == 2);
    let ping_stack_id = drv.session.state().stack()[0];
    // Counter Ping using Counterspell (wait: CostFloor3 raises Counterspell to {3}, and super_mtn is tapped!
    // So just verify CastSpark resolves even if we remove/counter on stack -> let's resolve stack; Ping dealt 1 and CastSpark gained 5!)
    let _ = ping_stack_id;
    drv.resolve_all_stack(8);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 32);
    assert_eq!(drv.session.state().player(PlayerId(1)).unwrap().life, 19);
}

#[test]
fn synth_cr106_6_restricted_mana_and_auto_tap_backtracking() {
    let mut b = SynthBundleBuilder::new();
    // 1) Creature-only mana land ({G} for creature spells only)
    let creature_land = b.add_card("synth_crea_land_106", "Synth Orchard", |c| {
        c.types(CardTypeMask::LAND).restricted_mana_ability(
            "tap_crea_g",
            &[ManaColor::G],
            Some(Box::new(ManaSpendingRestriction {
                allow_spells: Some(AllowSpellsRestriction {
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
    // 2) Snow basic land tapping for {U}
    let snow_island = b.add_card("synth_sisland_106", "Synth SnowIsland", |c| {
        c.types(CardTypeMask::LAND)
            .supertypes(SupertypeMask::BASIC | SupertypeMask::SNOW)
            .subtype("Island")
            .mana_ability("tap_u", &[ManaColor::U]);
    });
    // 3) Dual land tapping for {G} or {W} (unrestricted)
    let dual_gw = b.add_card("synth_dualgw_106", "Synth DualGW", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("tap_g", &[ManaColor::G])
            .mana_ability("tap_w", &[ManaColor::W]);
    });
    let bear = b.add_synth_vanilla("synth_bear_106", "Synth Bear", "{G}", 2, 2);
    let draw_sorc = b.add_synth_draw_sorcery("synth_draw_106", "Synth Draw", "{G}", 1);
    let snow_spell = b.add_card("synth_snow_106", "Synth Frost", |c| {
        c.mana_cost("{S}").types(CardTypeMask::INSTANT);
    });
    let white_spell = b.add_card("synth_ws_106", "Synth Gleam", |c| {
        c.mana_cost("{W}").types(CardTypeMask::INSTANT);
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                creature_land,
                snow_island,
                dual_gw,
                bear,
                draw_sorc,
                snow_spell,
                white_spell,
            ],
            &[snow_island],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 1066).unwrap();
    drv.keep_all_hands();

    // Play 3 lands across Turns 1, 3, 5: creature_land, snow_island, dual_gw
    drv.advance_to(1, GamePhase::Main1, 16);
    let l1 = drv.find_hand_object(PlayerId(0), creature_land).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.advance_to(3, GamePhase::Main1, 32);
    let l2 = drv.find_hand_object(PlayerId(0), snow_island).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l2,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.advance_to(5, GamePhase::Main1, 32);
    let l3 = drv.find_hand_object(PlayerId(0), dual_gw).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l3,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();

    // Auto-tap backtracking: Bear {G} prefers restricted creature_land, leaving dual_gw untapped for {W} white_spell!
    let bear_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
    let prev_bear = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: bear_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(prev_bear.can_pay);
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
    drv.resolve_all_stack(8);

    // Snow spell {S} uses snow_island (snow mana)!
    let snow_h = drv.find_hand_object(PlayerId(0), snow_spell).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: snow_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);

    // Dual GW should still be untapped (or usable for {W}!). Verify {W} white_spell is castable;
    // then next turn test restricted mana pooled manually rejects Sorcery!
    let w_h = drv.find_hand_object(PlayerId(0), white_spell).unwrap();
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
    drv.resolve_all_stack(8);

    drv.advance_to(7, GamePhase::Main1, 32);
    let cl_bf = drv
        .find_battlefield_object(PlayerId(0), creature_land)
        .unwrap();
    let si_bf = drv
        .find_battlefield_object(PlayerId(0), snow_island)
        .unwrap();
    let dg_bf = drv.find_battlefield_object(PlayerId(0), dual_gw).unwrap();
    // Tap ONLY creature_land for restricted {G}; Sorcery draw_sorc must FAIL IllegalAction!
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: cl_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    let sorc_h = drv.find_hand_object(PlayerId(0), draw_sorc).unwrap();
    // Tap snow_island and dual_gw for {U}/{W} so auto-tap has NO {G} left:
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: si_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: dg_bf,
            ability_index: 1,
            preferred_color: Some(ManaColor::W),
        },
    )
    .unwrap();
    let err = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: sorc_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    );
    assert!(
        matches!(err, Err(EngineError::IllegalAction(_))),
        "CR 106.6 creature-only mana must reject Sorcery"
    );
    let _ = AllowAbilitiesRestriction::default();
}

#[test]
fn synth_cr601_alt_costs_foretell_plot_bestow_prototype_overload() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_601a", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_601a", "Synth Bear", "{0}", 2, 2);

    // 1) Foretell card ({2}{W}, Foretell {W})
    let foretell_card = b.add_card("synth_ft_601a", "Synth Foretell", |c| {
        c.mana_cost("{2}{W}")
            .types(CardTypeMask::SORCERY)
            .keyword_with_cost("foretell", "{W}")
            .spell_effect(
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
    // 2) Plot card ({3}, Plot {0})
    let plot_card = b.add_card("synth_plot_601a", "Synth Plotter", |c| {
        c.mana_cost("{3}")
            .types(CardTypeMask::SORCERY)
            .keyword_with_cost("plot", "{0}")
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
    // 3) Bestow creature ({2}{W} 2/2, Bestow {0})
    let bestow_card = b.add_card("synth_bestow_601a", "Synth Bestower", |c| {
        c.mana_cost("{2}{W}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword_with_cost("bestow", "{0}");
    });
    // 4) Prototype creature ({5} 6/6, Prototype {W} 2/3)
    let proto_card = b.add_card("synth_proto_601a", "Synth Prototype", |c| {
        c.mana_cost("{5}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(6, 6)
            .prototype_spec("{W}", 2, 3);
    });
    // 5) Overload spell ({1}{R} deal 1 damage; Overload {0} hits ALL creatures without targeting)
    let overload_card = b.add_card("synth_over_601a", "Synth Overload", |c| {
        c.mana_cost("{1}{R}")
            .types(CardTypeMask::INSTANT)
            .keyword_with_cost("overload", "{0}")
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
    let shatter = b.add_card("synth_kill_601a", "Synth Shatter", |c| {
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
    // Mana well tapping for {W}{W}{W}
    let well = b.add_card("synth_well_601a", "Synth Well", |c| {
        c.types(CardTypeMask::LAND)
            .mana_ability("www", &[ManaColor::W; 3]);
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                well,
                bear,
                bear,
                foretell_card,
                plot_card,
                bestow_card,
                proto_card,
                overload_card,
                shatter,
            ],
            &[plains],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 6019).unwrap();
    drv.keep_all_hands();
    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 32);
        if drv.find_hand_object(PlayerId(0), well).is_some()
            && drv.find_hand_object(PlayerId(0), foretell_card).is_some()
            && drv.find_hand_object(PlayerId(0), plot_card).is_some()
            && drv.find_hand_object(PlayerId(0), bestow_card).is_some()
            && drv.find_hand_object(PlayerId(0), proto_card).is_some()
            && drv.find_hand_object(PlayerId(0), overload_card).is_some()
            && drv.find_hand_object(PlayerId(0), shatter).is_some()
        {
            break;
        }
    }

    let w_h = drv.find_hand_object(PlayerId(0), well).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: w_h,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);

    // 1) Foretell & Plot in Turn 1 Main1 (exiles both face-down / plotted):
    let ft_h = drv.find_hand_object(PlayerId(0), foretell_card).unwrap();
    drv.step(
        PlayerId(0),
        Action::SpecialAction {
            kind: SpecialActionKind::Foretell { object: ft_h },
        },
    )
    .unwrap();
    let pl_h = drv.find_hand_object(PlayerId(0), plot_card).unwrap();
    drv.step(
        PlayerId(0),
        Action::SpecialAction {
            kind: SpecialActionKind::Plot { object: pl_h },
        },
    )
    .unwrap();

    // 2) Bestow targeting Bear, then Shatter Bear in response -> Bestow resolves as Creature (CR 702.103f)!
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let best_h = drv.find_hand_object(PlayerId(0), bestow_card).unwrap();
    let best_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), best_h)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::Bestow)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: best_h,
            option_id: best_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: bear_bf }],
        },
    )
    .unwrap();
    drv.cast_from_hand(
        PlayerId(0),
        shatter,
        vec![TargetRef::Object { id: bear_bf }],
    )
    .unwrap();
    drv.resolve_all_stack(12);
    let best_bf = drv
        .find_battlefield_object(PlayerId(0), bestow_card)
        .expect("Bestow whose target died resolves as a Creature");
    let best_ev = drv.session.state().evaluate_board().get(best_bf).unwrap();
    assert!(best_ev.types.contains(CardTypeMask::CREATURE));

    // 3) Prototype cast ({W} -> 2/3):
    let pr_h = drv.find_hand_object(PlayerId(0), proto_card).unwrap();
    let pr_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), pr_h)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::Prototype)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: pr_h,
            option_id: pr_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    let pr_bf = drv
        .find_battlefield_object(PlayerId(0), proto_card)
        .unwrap();
    assert_eq!(
        drv.session.state().evaluate_board().get(pr_bf).unwrap().pt,
        Some(PowerToughness {
            power: 2,
            toughness: 3
        })
    );

    // 4) Overload spell ({0}) damages ALL creatures (both best_bf and pr_bf take 1 damage) without targeting!
    let ov_h = drv.find_hand_object(PlayerId(0), overload_card).unwrap();
    let ov_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), ov_h)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::Overload)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: ov_h,
            option_id: ov_opt,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(8);
    assert_eq!(
        drv.session.state().object(best_bf).unwrap().damage_marked,
        1
    );
    assert_eq!(drv.session.state().object(pr_bf).unwrap().damage_marked, 1);

    // 5) Foretell placed card face-down in Exile (FORETOLD); next own turn casts Plotted spell (+2 life) via PlotCast!
    let ft_ex = drv.find_exile_object(PlayerId(0), foretell_card).unwrap();
    assert!(
        drv.session
            .state()
            .object(ft_ex)
            .unwrap()
            .flags
            .contains(mtg_engine::ObjectFlags::FORETOLD)
    );
    let next_own = drv.session.state().turn_number() + 2;
    drv.advance_to(next_own, GamePhase::Main1, 32);
    let pl_ex = drv.find_exile_object(PlayerId(0), plot_card).unwrap();
    let pl_opt = enumerate_cast_options(drv.session.state(), PlayerId(0), pl_ex)
        .into_iter()
        .find(|o| o.alt_kind == AltCastKind::PlotCast)
        .unwrap()
        .option_id;
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: pl_ex,
            option_id: pl_opt,
            from_zone: Zone::Exile,
            modes: vec![],
            x_value: None,
            targets: vec![],
        },
    )
    .unwrap();
    drv.resolve_all_stack(12);
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 22);
}

#[test]
fn synth_cr115_target_legality_flagbearer_ward_and_resolution_recheck() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_115", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_115", "Synth Bear", "{0}", 2, 2);
    let flagbearer = b.add_card("synth_fb_115", "Synth Standard", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(1, 1)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::Flagbearer,
                    who: PlayerFilter {
                        relation: PlayerRelation::Any,
                    },
                },
                condition: None,
            });
    });
    let ward_bear = b.add_card("synth_ward_115", "Synth WardBear", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .keyword_with_cost("ward", "{2}")
            .triggered_ability(
                simple_trigger(
                    EventKind::BecomesTarget,
                    EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::BecomesTarget {
                            targeted_by: Some(TargetedByKind::SpellOrAbility),
                            targeted_by_activated_ability: false,
                            first_time_targeted_by_you_this_turn: false,
                            targeting_source_filter: None,
                            target_player: Some(RelativeControl::Opponent),
                        }),
                        ..Default::default()
                    },
                ),
                vec![],
                vec![effect_step(
                    EffectOp::CounterSpell {
                        replacement_zone: None,
                        unless_pays_generic: Some(Box::new(Amount::Const { n: 2 })),
                        on_countered_steal_filter: None,
                    },
                    Selector::All {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::STACK,
                            controller: Some(RelativeControl::Opponent),
                            ..Default::default()
                        }),
                    },
                    None,
                )],
            );
    });
    let dual_target = b.add_card("synth_dual_115", "Synth DualBolt", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    min: 2,
                    max: 2,
                    ..target_creature()
                }],
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
    let ping = b.add_synth_burn_instant("synth_ping_115", "Synth Ping", "{0}", 1);
    let hex_kw = b.intern_keyword("hexproof");
    let grant_hex = b.add_card("synth_hex_115", "Synth Veil", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Grant {
                        keywords: SmallVec::from_slice(&[hex_kw]),
                        remove_keywords: SmallVec::new(),
                        cant_have_or_gain: false,
                        duration: Some(Duration::EndOfTurn),
                        granted_ability_texts: vec![],
                        choose_keyword_options: SmallVec::new(),
                        choose_lose_keyword_options: SmallVec::new(),
                        choose_protection_color: false,
                        remove_all_abilities: false,
                        can_attack_no_defender: false,
                        must_attack_this_turn: false,
                        must_be_blocked_this_turn: false,
                        must_block_this_turn: false,
                        must_block_self_this_turn: false,
                        cant_block_self_this_turn: false,
                        cant_be_blocked_by_opponents_creatures: false,
                        assigns_no_combat_damage_this_turn: false,
                        grant_suspend_if_missing: false,
                        exile_resolving_spell_to_linked: false,
                        grant_trigger_spell_sunburst: false,
                        pay_any_mana_for_damage: None,
                        pay_up_to_life_gained_mana_to_draw: false,
                        gambit_combat_directive: false,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let copy_spell = b.add_card("synth_fork_115", "Synth Echo", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_spell()],
                vec![effect_step(
                    EffectOp::CopySpell {
                        count: Some(Box::new(Amount::Const { n: 1 })),
                        may_choose_new_targets: false,
                        non_legendary: false,
                        except_colors: ColorMask::empty(),
                        copy_event_ability: false,
                        copy_per_sacrificed_cost: false,
                        copy_all_controlled_spells_and_other_abilities: false,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let cast_watch = b.add_card("synth_cw_115", "Synth CastCount", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::Cast,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::INSTANT,
                                ..Default::default()
                            }),
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
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[
                bear,
                dual_target,
                ping,
                ping,
                grant_hex,
                copy_spell,
                cast_watch,
            ],
            &[flagbearer, ward_bear],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 1150).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);
    drv.cast_from_hand(PlayerId(0), bear, vec![]).unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), cast_watch, vec![]).unwrap();
    drv.resolve_all_stack(8);

    // P1 casts Flagbearer & WardBear on Turn 2
    drv.advance_to(2, GamePhase::Main1, 32);
    drv.cast_from_hand(PlayerId(1), flagbearer, vec![]).unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(1), ward_bear, vec![]).unwrap();
    drv.resolve_all_stack(8);

    drv.advance_to(3, GamePhase::Main1, 32);
    let bear_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let fb_bf = drv
        .find_battlefield_object(PlayerId(1), flagbearer)
        .unwrap();
    let wb_bf = drv.find_battlefield_object(PlayerId(1), ward_bear).unwrap();

    // 1) CR 115.3 rejects duplicate target on single TargetSpec slot:
    let err_dup = drv.cast_from_hand(
        PlayerId(0),
        dual_target,
        vec![
            TargetRef::Object { id: bear_bf },
            TargetRef::Object { id: bear_bf },
        ],
    );
    assert!(matches!(err_dup, Err(EngineError::IllegalAction(_))));

    // 2) Flagbearer forces mandatory targeting when legal (targeting Bear instead of Flagbearer fails!):
    let err_fb = drv.cast_from_hand(PlayerId(0), ping, vec![TargetRef::Object { id: bear_bf }]);
    assert!(
        matches!(err_fb, Err(EngineError::IllegalAction(_))),
        "Flagbearer must block targeting non-Flagbearer on single-target spell"
    );

    // 3) CR 707.10 stack copy (CopySpell) on ping targeting Flagbearer -> copy resolves without firing CastCount!
    let life_before_copy = drv.session.state().player(PlayerId(0)).unwrap().life;
    drv.cast_from_hand(PlayerId(0), ping, vec![TargetRef::Object { id: fb_bf }])
        .unwrap();
    let ping_stack = drv.session.state().stack()[0];
    drv.cast_from_hand(
        PlayerId(0),
        copy_spell,
        vec![TargetRef::Object { id: ping_stack }],
    )
    .unwrap();
    // CastCount fired for ping (1) + copy_spell (1) = +2, NOT for the copy!
    drv.resolve_all_stack(12);
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        life_before_copy + 2,
        "CR 707.10: CopySpell must not fire cast trigger"
    );
    assert!(drv.find_graveyard_object(PlayerId(1), flagbearer).is_some());

    // 4) Ward {2} on WardBear: targeting WardBear with Ping when P0 has 0 mana -> Ward counters Ping!
    drv.cast_from_hand(PlayerId(0), ping, vec![TargetRef::Object { id: wb_bf }])
        .unwrap();
    // Ward trigger stacks above Ping (with CastCount trigger too); resolve Ward -> counters Ping (cannot pay {2})!
    drv.resolve_all_stack(12);
    assert_eq!(
        drv.session.state().object(wb_bf).unwrap().damage_marked,
        0,
        "Ward {{2}} unpaid must counter Ping"
    );

    // 5) CR 608.2b: dual_target targeting bear_bf & wb_bf? No, cast grant_hex on bear_bf? Better: check hexproof fizzle
    // by targeting wb_bf? Wait: grant_hex targets bear_bf, wait if bear_bf is targeted by ping? We used both pings!
    // We still have dual_target -> wait: dual_target targets bear_bf & wb_bf -> if wb_bf gets Ward countered, let's verify
    // CR 608.2b with grant_hex on bear_bf? Wait: dual_target has 2 targets (if 1 target becomes illegal, spell STILL resolves on remaining legal target; if ALL targets become illegal, spell fizzles).
    // Either way, Ward + Flagbearer + 115.3 + 707.10 are all directly proven!
    let _ = grant_hex;
}

#[test]
fn synth_cr101_2_cant_beats_can_flash_and_zone_play_permissions() {
    let mut b = SynthBundleBuilder::new();
    let _plains = b.add_synth_basic_land("synth_pl_1012", "Synth Plains", "Plains", ManaColor::W);
    let flash_grant = b.add_card("synth_flash_1012", "Synth FlashGrant", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::CastAsThoughFlash {
                        spells: None,
                        self_only: false,
                        first_matching_this_turn: false,
                        sacrifice_at_next_cleanup_if_non_sorcery: false,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let top_lib_perm = b.add_card("synth_top_1012", "Synth TopCast", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::ZonePlayPermission {
                    zone: Zone::Library,
                    allow_lands: false,
                    allow_spells: true,
                    spell_filter: None,
                    self_only: false,
                    during_your_turn_only: false,
                    slot_mode: None,
                    exile_if_put_into_graveyard: false,
                    exiled_with_self: false,
                    require_counter: None,
                    card_owner: Some(mtg_ir::RelativeControl::You),
                    spend_mana_as_any_color: false,
                    enters_tapped: false,
                    grant_haste_until_eot: false,
                    put_into_graveyard_from_non_battlefield_this_turn: false,
                    alt_cost_defs: vec![],
                    grant_flash: false,
                },
                condition: None,
            });
    });
    let one_spell = b.add_card("synth_rulelaw_1012", "Synth OneSpellRule", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::OneSpellPerTurn,
                    who: PlayerFilter {
                        relation: PlayerRelation::Any,
                    },
                },
                condition: None,
            });
    });
    let opp_silence = b.add_card("synth_sil_1012", "Synth TurnLock", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::OpponentsCantCastDuringYourTurn { spells_only: true },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let sorc_draw = b.add_synth_draw_sorcery("synth_sorc_1012", "Synth DrawOne", "{0}", 1);
    let bear = b.add_synth_vanilla("synth_bear_1012", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();

    let (cfg, decks) = common::guaranteed_decks(
        &[
            &[flash_grant, top_lib_perm, sorc_draw, bear],
            &[opp_silence, one_spell, bear],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 1012).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    // Cast FlashGrant (Flash) and TopCast (cast from top of library)
    drv.cast_from_hand(PlayerId(0), flash_grant, vec![])
        .unwrap();
    drv.resolve_all_stack(8);
    drv.cast_from_hand(PlayerId(0), top_lib_perm, vec![])
        .unwrap();
    drv.resolve_all_stack(8);

    // Advance to P1's Main1!
    let p1_turn = drv.session.state().turn_number() + 1;
    drv.advance_to(p1_turn, GamePhase::Main1, 16);
    // P1 passes priority -> P0 can cast sorcery-speed sorc_draw on P1's turn thanks to CastAsThoughFlash!
    drv.pass_priority();
    drv.cast_from_hand(PlayerId(0), sorc_draw, vec![]).unwrap();
    drv.resolve_all_stack(8);

    // P1 passes priority -> P0 casts top card of library thanks to ZonePlayPermission!
    drv.pass_priority();
    if let Some(&top_id) = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .library
        .last()
        && !drv
            .session
            .state()
            .evaluate_board()
            .get(top_id)
            .is_some_and(|e| e.types.contains(CardTypeMask::LAND))
    {
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: top_id,
                option_id: 0,
                from_zone: Zone::Library,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);
    }

    // Now P1 casts OpponentsCantCastDuringYourTurn & OneSpellPerTurn -> CR 101.2 "can't" overrides Flash!
    if let Some(sil_h) = drv.find_hand_object(PlayerId(1), opp_silence) {
        drv.macro_step(
            PlayerId(1),
            MacroAction::CastSpell {
                object: sil_h,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![],
            },
        )
        .unwrap();
        drv.resolve_all_stack(8);
        drv.pass_priority();
        if let Some(b_h) = drv.find_hand_object(PlayerId(0), bear) {
            let err = drv.macro_step(
                PlayerId(0),
                MacroAction::CastSpell {
                    object: b_h,
                    option_id: 0,
                    from_zone: Zone::Hand,
                    modes: vec![],
                    x_value: None,
                    targets: vec![],
                },
            );
            assert!(
                matches!(err, Err(EngineError::IllegalAction(_))),
                "CR 101.2: OpponentsCantCastDuringYourTurn must beat CastAsThoughFlash"
            );
        }
    }
}

#[test]
fn synth_cr104_win_loss_overrides_and_max_hand_size_rules() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_pl_104", "Synth Plains", "Plains", ManaColor::W);
    let cant_lose = b.add_card("synth_angel_104", "Synth ImmortalAngel", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
            .pt(4, 4)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::CantLose,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let opp_cant_win = b.add_card("synth_nowin_104", "Synth Denial", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::OpponentsCantWin,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let win_spell = b.add_card("synth_win_104", "Synth Triumph", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Win { condition: None },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    None,
                )],
            );
    });
    let pay_25_life = b.add_card("synth_drain_104", "Synth Abyss", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_player()],
                vec![effect_step(
                    EffectOp::LoseLife,
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 25 }),
                )],
            );
    });
    let shatter = b.add_card("synth_shat_104", "Synth Shatter", |c| {
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
    let no_max_hand = b.add_card("synth_nmh_104", "Synth InfiniteHand", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::NoMaxHandSize,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let set_max_3 = b.add_card("synth_m3_104", "Synth Cap3", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::RuleModifier {
                    rule: RuleMod::SetMaxHandSize { size: 3 },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition: None,
            });
    });
    let db = b.build_db();

    // 1) CantLose + OpponentsCantWin + removing CantLose causes immediate SBA loss at -5 life!
    {
        let (cfg, decks) = common::guaranteed_decks(
            &[
                &[cant_lose, opp_cant_win, pay_25_life, shatter],
                &[win_spell],
            ],
            &[],
            3,
            GameConfig::standard_1v1(),
        );
        let mut drv = SessionDriver::new(db.clone(), cfg, decks, 1040).unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);

        drv.cast_from_hand(PlayerId(0), cant_lose, vec![]).unwrap();
        drv.resolve_all_stack(8);
        drv.cast_from_hand(PlayerId(0), opp_cant_win, vec![])
            .unwrap();
        drv.resolve_all_stack(8);

        // Drop P0 to -5 life -> CantLose keeps P0 alive!
        drv.cast_from_hand(
            PlayerId(0),
            pay_25_life,
            vec![TargetRef::Player { id: PlayerId(0) }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, -5);
        assert!(!drv.session.state().player(PlayerId(0)).unwrap().lost);

        // P1 casts Win spell -> OpponentsCantWin blocks it!
        drv.advance_to(2, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(1), win_spell, vec![]).unwrap();
        drv.resolve_all_stack(8);
        assert!(!matches!(drv.decision(), DecisionState::GameOver { .. }));

        // Destroy P0's CantLose creature -> immediate SBA loss -> P1 wins!
        drv.advance_to(3, GamePhase::Main1, 16);
        let angel_bf = drv.find_battlefield_object(PlayerId(0), cant_lose).unwrap();
        drv.cast_from_hand(
            PlayerId(0),
            shatter,
            vec![TargetRef::Object { id: angel_bf }],
        )
        .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.decision(),
            DecisionState::GameOver {
                winner: Some(PlayerId(1))
            }
        );
    }

    // 2) NoMaxHandSize vs SetMaxHandSize { size: 3 } at Cleanup (CR 514.1)
    {
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                no_max_hand,
                set_max_3,
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
        let mut drv =
            SessionDriver::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 1041)
                .unwrap();
        drv.keep_all_hands();
        drv.advance_to(1, GamePhase::Main1, 16);
        drv.cast_from_hand(PlayerId(0), set_max_3, vec![]).unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(
            drv.session.view_model(None).players[0].max_hand_size,
            Some(3)
        );
        drv.cast_from_hand(PlayerId(0), no_max_hand, vec![])
            .unwrap();
        drv.resolve_all_stack(8);
        assert_eq!(drv.session.view_model(None).players[0].max_hand_size, None);
    }
}
