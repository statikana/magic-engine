//! Comprehensive Rules `Equip` (`CR 702.6a`, `CR 701.3b`, `CR 301.5c`, `CR 613.7e`,
//! `CR 113.7a`, `CR 608.2b`, `CR 702.16n`, `CR 704.5n`) synthetic integration tests.
//!
//! Zero WotC/Scryfall IP: every synthetic card uses `Synth *` / `synth_*`.

mod common;

use std::sync::Arc;

use common::{
    SessionDriver, SynthBundleBuilder, SynthCardBuilder, effect_step, empty_keyword_def,
    guaranteed_decks, simple_trigger, target_creature,
};
use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig, GameState, ObjectId,
    PlayerId, RngState, TargetRef, TsReplayTrace, check_sbas, has_keyword,
    resolve_activated_abilities, run_ts_diff_test,
};
use mtg_ir::{
    AbilityDef, Affected, Amount, CardTypeMask, ColorMask, ConditionalCostReduction, CostDef,
    CostDelta, CostDeltaMode, Duration, EffectOp, EventFilter, EventKind, FilterFlags,
    FirstActivationZeroKind, GamePhase, KeywordAbility, Layer, LayerMod, ManaColor, ManaCost,
    ObjectFilter, PlayerFilter, PlayerRef, PlayerRelation, RelativeControl, Selector, Span,
    SpellOrAbilityFilter, StaticBody, StaticDef, SupertypeMask, TargetKind, TargetSpec,
    TriggerSubject, Zone, ZoneMask,
};
use smallvec::{SmallVec, smallvec};

fn add_equip_keyword(
    c: &mut SynthCardBuilder<'_>,
    id: Option<&str>,
    mana_cost: Option<&str>,
    equip_filter: Option<ObjectFilter>,
    allow_planeswalker: bool,
    cost_defs: Vec<CostDef>,
    cost_reduction: Option<ConditionalCostReduction>,
) {
    let kid = c.intern_keyword("equip");
    let mut kd = empty_keyword_def(kid);
    kd.cost = mana_cost.map(ManaCost::parse);
    kd.equip_filter = equip_filter.map(Box::new);
    kd.equip_allow_planeswalker = allow_planeswalker;
    kd.cost_defs = cost_defs;
    kd.equip_cost_reduction = cost_reduction;
    c.primary
        .abilities
        .push(AbilityDef::Keyword(KeywordAbility {
            keyword: kd,
            id: id.map(str::to_string),
            span: Span::default(),
        }));
}

fn add_attach_watchers(b: &mut SynthBundleBuilder) -> (mtg_ir::CardDefId, mtg_ir::CardDefId) {
    let attached_watcher = b.add_card("synth_attach_watch", "Synth AttachWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::Attached,
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
                    Some(Amount::Const { n: 3 }),
                )],
            );
    });
    let unattached_watcher = b.add_card("synth_unattach_watch", "Synth UnattachWatch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .triggered_ability(
                simple_trigger(
                    EventKind::Unattached,
                    EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter::default()),
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
                    Some(Amount::Const { n: 2 }),
                )],
            );
    });
    (attached_watcher, unattached_watcher)
}

fn equip_ability_index(state: &GameState, equipment: ObjectId) -> u16 {
    resolve_activated_abilities(state, equipment)
        .into_iter()
        .find(|ab| {
            ab.ability
                .cost_keyword
                .and_then(|kw| state.db().keyword_name(kw))
                .is_some_and(|name| name.eq_ignore_ascii_case("equip"))
        })
        .expect("equipment must expose synthesized Equip activated ability")
        .ability_index
}

fn activate_equip_to(
    drv: &mut SessionDriver,
    player: PlayerId,
    equipment: ObjectId,
    ability_index: u16,
    target: ObjectId,
) -> Result<(), mtg_engine::EngineError> {
    drv.step(
        player,
        Action::BeginActivate {
            object: equipment,
            ability_index,
        },
    )?;
    if matches!(drv.decision(), DecisionState::CastingSequence) {
        drv.step(
            player,
            Action::SelectTarget {
                target: TargetRef::Object { id: target },
            },
        )?;
    }
    if matches!(drv.decision(), DecisionState::CastingSequence) {
        drv.step(player, Action::AutoPayMana)?;
    }
    Ok(())
}

fn play_and_resolve_land(drv: &mut SessionDriver, player: PlayerId, land_def: mtg_ir::CardDefId) {
    let land = drv
        .find_hand_object(player, land_def)
        .expect("land in hand");
    drv.step(
        player,
        Action::PlayLand {
            object: land,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
}

fn cast_and_resolve(
    drv: &mut SessionDriver,
    player: PlayerId,
    card_def: mtg_ir::CardDefId,
    targets: Vec<TargetRef>,
) {
    drv.cast_from_hand(player, card_def, targets).unwrap();
    drv.resolve_all_stack(16);
}

fn resolve_all_stack_with_order(drv: &mut SessionDriver, max_steps: usize) {
    for _ in 0..max_steps {
        match drv.decision() {
            DecisionState::Priority => {
                if drv.session.state().stack().is_empty() {
                    return;
                }
                drv.pass_priority();
            }
            DecisionState::Prompt {
                request: ChoiceRequest::OrderObjects { prompt_id, objects },
            } => {
                let actor = drv.session.current_actor().expect("order actor");
                drv.step(
                    actor,
                    Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::OrderNext { id: objects[0] },
                    },
                )
                .unwrap();
            }
            other => panic!("unexpected decision while resolving stack: {other:?}"),
        }
    }
    assert!(drv.session.state().stack().is_empty());
}

#[test]
fn synth_cr702_6a_equip_lifecycle_and_layer6_grant() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_eq1", "Synth Plains", "Plains", ManaColor::W);
    let mystic = b.add_synth_vanilla("synth_mystic_eq1", "Synth Mystic", "{0}", 1, 1);
    let (attach_watch, _) = add_attach_watchers(&mut b);
    let boots = b.add_card("synth_boots_eq1", "Synth Boots", |c| {
        let hex_kw = c.intern_keyword("hexproof");
        let haste_kw = c.intern_keyword("haste");
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::AttachedTo,
                    modifier: LayerMod::AddKeywords {
                        keywords: smallvec![hex_kw, haste_kw],
                        protection_attachment_exemption: None,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
        add_equip_keyword(c, Some("equip_1"), Some("{1}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[&[plains, mystic, attach_watch, boots], &[plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70261).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    play_and_resolve_land(&mut drv, PlayerId(0), plains);
    cast_and_resolve(&mut drv, PlayerId(0), mystic, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), attach_watch, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);

    let mystic_id = drv.find_battlefield_object(PlayerId(0), mystic).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let ts_before = drv
        .session
        .state()
        .object(boots_id)
        .unwrap()
        .entered_timestamp;
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 20);
    assert!(!has_keyword(drv.session.state(), mystic_id, "hexproof"));
    assert!(!has_keyword(drv.session.state(), mystic_id, "haste"));

    let eq_idx = equip_ability_index(drv.session.state(), boots_id);
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, mystic_id).unwrap();
    drv.resolve_all_stack(16);

    let boots_obj = drv.session.state().object(boots_id).unwrap();
    assert_eq!(boots_obj.attached_to, Some(mystic_id));
    assert!(
        boots_obj.entered_timestamp > ts_before,
        "attaching Equip must update entered_timestamp (CR 613.7e)"
    );
    assert!(has_keyword(drv.session.state(), mystic_id, "hexproof"));
    assert!(has_keyword(drv.session.state(), mystic_id, "haste"));
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        23,
        "EventKind::Attached must trigger on Equip attachment"
    );
}

#[test]
fn synth_cr702_6a_equip_sorcery_speed_enforcement() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_eq2", "Synth Plains", "Plains", ManaColor::W);
    let mystic = b.add_synth_vanilla("synth_mystic_eq2", "Synth Mystic", "{0}", 1, 1);
    let instant_noop = b.add_synth_burn_instant("synth_spark_eq2", "Synth Spark", "{0}", 1);
    let boots = b.add_card("synth_boots_eq2", "Synth Boots", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[&[mystic, boots, instant_noop], &[plains]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70262).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    cast_and_resolve(&mut drv, PlayerId(0), mystic, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);

    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);
    let begin_act = Action::BeginActivate {
        object: boots_id,
        ability_index: eq_idx,
    };

    // 1. Legal during own main phase with empty stack.
    assert!(
        drv.session.legal_actions(PlayerId(0)).contains(&begin_act),
        "Equip must be legal during own Main1 with empty stack"
    );

    // 2. Illegal when stack is non-empty.
    drv.cast_from_hand(
        PlayerId(0),
        instant_noop,
        vec![TargetRef::Player { id: PlayerId(1) }],
    )
    .unwrap();
    assert!(!drv.session.legal_actions(PlayerId(0)).contains(&begin_act));
    assert!(drv.step(PlayerId(0), begin_act.clone()).is_err());
    drv.resolve_all_stack(16);

    // 3. Illegal during combat (BeginCombat).
    drv.advance_to(1, GamePhase::BeginCombat, 16);
    assert!(!drv.session.legal_actions(PlayerId(0)).contains(&begin_act));
    assert!(drv.step(PlayerId(0), begin_act.clone()).is_err());

    // 4. Illegal during opponent's turn (even in opponent's Main1 when P0 receives priority).
    drv.advance_to(2, GamePhase::Main1, 32);
    drv.pass_priority(); // P1 passes -> P0 has priority during P1's Main1
    assert_eq!(drv.session.current_actor(), Some(PlayerId(0)));
    assert!(!drv.session.legal_actions(PlayerId(0)).contains(&begin_act));
    assert!(drv.step(PlayerId(0), begin_act).is_err());
}

#[test]
fn synth_cr702_6a_equip_target_legality_and_protection_fizzle() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_eq3", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla("synth_bear_eq3", "Synth Bear", "{0}", 2, 2);
    let hex_guy =
        b.add_synth_keyword_creature("synth_hex_eq3", "Synth Hex", "{0}", 1, 1, &["hexproof"]);
    let shroud_guy =
        b.add_synth_keyword_creature("synth_shroud_eq3", "Synth Shroud", "{0}", 1, 1, &["shroud"]);
    let prot_guy = b.add_card("synth_prot_eq3", "Synth Prot", |c| {
        c.mana_cost("{0}").types(CardTypeMask::CREATURE).pt(2, 2);
        let kid = c.intern_keyword("protection");
        let mut kd = empty_keyword_def(kid);
        kd.param = Some("artifacts".to_string());
        c.keyword_def(kd);
    });
    let flash_prot_aura_field = b.add_card("synth_flash_prot_eq3", "Synth FlashProt", |c| {
        let prot_kid = c.intern_keyword("protection");
        let beast_sid = c.intern_subtype("SynthBeast");
        let mut prot_kd = empty_keyword_def(prot_kid);
        prot_kd.param = Some("artifacts".to_string());
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .keyword("flash")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            subtypes_any: smallvec![beast_sid],
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::GrantAbility {
                        ability: Box::new(AbilityDef::Keyword(KeywordAbility {
                            keyword: prot_kd,
                            id: None,
                            span: Span::default(),
                        })),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let boots = b.add_card("synth_boots_eq3", "Synth Boots", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[
            &[
                bear,
                hex_guy,
                shroud_guy,
                prot_guy,
                flash_prot_aura_field,
                boots,
            ],
            &[plains, bear],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70263).unwrap();
    drv.keep_all_hands();

    // Let P1 deploy an opponent's creature on Turn 2, then come back to Turn 3 P0 Main1.
    drv.advance_to(2, GamePhase::Main1, 32);
    cast_and_resolve(&mut drv, PlayerId(1), bear, vec![]);
    let opp_bear = drv.find_battlefield_object(PlayerId(1), bear).unwrap();

    drv.advance_to(3, GamePhase::Main1, 32);
    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), hex_guy, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), shroud_guy, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), prot_guy, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);

    let own_bear = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let own_hex = drv.find_battlefield_object(PlayerId(0), hex_guy).unwrap();
    let own_shroud = drv
        .find_battlefield_object(PlayerId(0), shroud_guy)
        .unwrap();
    let own_prot = drv.find_battlefield_object(PlayerId(0), prot_guy).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);

    // Opponent's creature, Shroud creature, and Protection-from-artifacts creature all rejected.
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: boots_id,
            ability_index: eq_idx,
        },
    )
    .unwrap();
    assert!(
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Object { id: opp_bear },
            },
        )
        .is_err()
    );
    assert!(
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Object { id: own_shroud },
            },
        )
        .is_err()
    );
    assert!(
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Object { id: own_prot },
            },
        )
        .is_err()
    );

    // Own Hexproof creature CAN be targeted and equipped!
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: own_hex },
        },
    )
    .unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        Some(own_hex)
    );

    // Now target own_bear, and while Equip is on the stack cast Flash enchantment granting
    // protection from artifacts -> Equip fizzles on resolution (CR 608.2b), staying on own_hex.
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, own_bear).unwrap();
    drv.cast_from_hand(PlayerId(0), flash_prot_aura_field, vec![])
        .unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        Some(own_hex),
        "gaining protection from artifacts on the stack must fizzle Equip (CR 608.2b)"
    );
}

#[test]
fn synth_cr701_3b_re_equip_same_host_noop_and_move_host_events() {
    let mut b = SynthBundleBuilder::new();
    let bear_a = b.add_synth_vanilla("synth_bear_a_eq4", "Synth Bear A", "{0}", 2, 2);
    let bear_b = b.add_synth_vanilla("synth_bear_b_eq4", "Synth Bear B", "{0}", 2, 2);
    let (attach_watch, unattach_watch) = add_attach_watchers(&mut b);
    let boots = b.add_card("synth_boots_eq4", "Synth Boots", |c| {
        let haste_kw = c.intern_keyword("haste");
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::AttachedTo,
                    modifier: LayerMod::AddKeywords {
                        keywords: smallvec![haste_kw],
                        protection_attachment_exemption: None,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[
            &[bear_a, bear_b, attach_watch, unattach_watch, boots],
            &[bear_a],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70264).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    cast_and_resolve(&mut drv, PlayerId(0), bear_a, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), bear_b, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), attach_watch, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), unattach_watch, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);

    let a_id = drv.find_battlefield_object(PlayerId(0), bear_a).unwrap();
    let b_id = drv.find_battlefield_object(PlayerId(0), bear_b).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);

    // 1. Equip A: fires Attached (+3 life -> 23), no Unattached.
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, a_id).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        Some(a_id)
    );
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 23);
    assert!(has_keyword(drv.session.state(), a_id, "haste"));
    assert!(!has_keyword(drv.session.state(), b_id, "haste"));

    let ts_after_first = drv
        .session
        .state()
        .object(boots_id)
        .unwrap()
        .entered_timestamp;

    // 2. Re-equip A: legal activation, resolves as CR 701.3b / 613.7e no-op (no timestamp bump, no Attached event).
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, a_id).unwrap();
    drv.resolve_all_stack(16);
    let boots_re = drv.session.state().object(boots_id).unwrap();
    assert_eq!(boots_re.attached_to, Some(a_id));
    assert_eq!(
        boots_re.entered_timestamp, ts_after_first,
        "re-equipping same host must NOT bump entered_timestamp (CR 613.7e)"
    );
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        23,
        "re-equipping same host must NOT fire Attached or Unattached"
    );

    // 3. Equip B: fires Unattached (-2 life) and Attached (+3 life) -> net 24 life, bumps timestamp, transfers haste.
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, b_id).unwrap();
    resolve_all_stack_with_order(&mut drv, 16);
    let boots_moved = drv.session.state().object(boots_id).unwrap();
    assert_eq!(boots_moved.attached_to, Some(b_id));
    assert!(boots_moved.entered_timestamp > ts_after_first);
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        24,
        "moving Equipment to new host must fire both Unattached (-2) and Attached (+3)"
    );
    assert!(!has_keyword(drv.session.state(), a_id, "haste"));
    assert!(has_keyword(drv.session.state(), b_id, "haste"));
}

#[test]
fn synth_cr608_2b_controller_change_and_cr113_7a_lki_resolution() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_eq5", "Synth Bear", "{0}", 2, 2);
    let steal_instant = b.add_card("synth_steal_eq5", "Synth Steal", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::GainControl {
                        duration: Some(Duration::EndOfTurn),
                        untap: false,
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let blank_flash = b.add_card("synth_blank_eq5", "Synth Blank", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .keyword("flash")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ARTIFACT,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
    });
    let boots = b.add_card("synth_boots_eq5", "Synth Boots", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[&[bear, boots, blank_flash], &[steal_instant]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70265).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);
    let bear_id = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);

    // Case A: Opponent steals target creature while Equip is on stack -> Equip fizzles (CR 608.2b).
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, bear_id).unwrap();
    drv.pass_priority(); // P0 passes -> P1 responds
    drv.cast_from_hand(
        PlayerId(1),
        steal_instant,
        vec![TargetRef::Object { id: bear_id }],
    )
    .unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session
            .state()
            .evaluate_board()
            .get(bear_id)
            .unwrap()
            .controller,
        PlayerId(1)
    );
    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        None,
        "Equip must fizzle if target creature changes controller before resolution"
    );

    // Deploy a fresh Bear for P0 to test Case C.
    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    let bear2_id = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .battlefield
        .iter()
        .copied()
        .find(|&id| {
            id != bear_id
                && drv
                    .session
                    .state()
                    .object(id)
                    .is_some_and(|o| o.card_def == bear)
        })
        .unwrap();

    // Case C: Equipment loses all innate abilities while Equip is on stack -> still resolves & attaches (CR 113.7a).
    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, bear2_id).unwrap();
    drv.cast_from_hand(PlayerId(0), blank_flash, vec![])
        .unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        Some(bear2_id),
        "CR 113.7a: Equip on stack must still resolve after source loses abilities"
    );
}

#[test]
fn synth_cr113_7a_destroyed_equipment_resolves_via_lki_cleanly() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_eq5b", "Synth Bear", "{0}", 2, 2);
    let shatter_instant = b.add_card("synth_shatter_eq5b", "Synth Shatter", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
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
    let boots = b.add_card("synth_boots_eq5b", "Synth Boots", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[&[bear, boots], &[shatter_instant]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 702652).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);
    let bear_id = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);

    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, bear_id).unwrap();
    drv.pass_priority(); // P0 passes -> P1 destroys boots while Equip is on stack
    drv.cast_from_hand(
        PlayerId(1),
        shatter_instant,
        vec![TargetRef::Object { id: boots_id }],
    )
    .unwrap();
    drv.resolve_all_stack(16);

    assert!(drv.find_battlefield_object(PlayerId(0), boots).is_none());
    assert!(drv.find_graveyard_object(PlayerId(0), boots).is_some());
}

#[test]
fn synth_cr301_5c_animated_equipment_cannot_attach() {
    let mut b = SynthBundleBuilder::new();
    let bear = b.add_synth_vanilla("synth_bear_eq6", "Synth Bear", "{0}", 2, 2);
    let (attach_watch, _) = add_attach_watchers(&mut b);
    let animate_flash = b.add_card("synth_anim_eq6", "Synth Animate", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .keyword("flash")
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
                            types_any: CardTypeMask::ARTIFACT,
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
    let boots = b.add_card("synth_boots_eq6", "Synth Boots", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(c, Some("equip_1"), Some("{0}"), None, false, vec![], None);
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[&[bear, attach_watch, animate_flash, boots], &[bear]],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70266).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), attach_watch, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), boots, vec![]);
    let bear_id = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let boots_id = drv.find_battlefield_object(PlayerId(0), boots).unwrap();
    let eq_idx = equip_ability_index(drv.session.state(), boots_id);

    activate_equip_to(&mut drv, PlayerId(0), boots_id, eq_idx, bear_id).unwrap();
    drv.cast_from_hand(PlayerId(0), animate_flash, vec![])
        .unwrap();
    drv.resolve_all_stack(16);

    assert_eq!(
        drv.session.state().object(boots_id).unwrap().attached_to,
        None,
        "CR 301.5c: animated creature Equipment without reconfigure must not attach"
    );
    assert_eq!(
        drv.session.state().player(PlayerId(0)).unwrap().life,
        20,
        "skipped attachment must not fire Attached trigger"
    );
}

#[test]
fn synth_cr702_6a_equip_filter_legendary_and_planeswalker() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_eq7", "Synth Plains", "Plains", ManaColor::W);
    let vanilla = b.add_synth_vanilla("synth_vanilla_eq7", "Synth Vanilla", "{0}", 2, 2);
    let legend = b.add_synth_legendary_creature("synth_legend_eq7", "Synth Legend", "{0}", 3, 3);
    let walker = b.add_synth_planeswalker("synth_walker_eq7", "Synth Walker", "{0}", 4);
    let legend_blade = b.add_card("synth_legblade_eq7", "Synth LegBlade", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(
            c,
            Some("equip_1"),
            Some("{2}"),
            Some(ObjectFilter {
                supertypes_any: SupertypeMask::LEGENDARY,
                ..Default::default()
            }),
            false,
            vec![],
            None,
        );
    });
    let pw_gem = b.add_card("synth_pwgem_eq7", "Synth PwGem", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(
            c,
            Some("equip_1"),
            Some("{1}"),
            Some(ObjectFilter {
                types_any: CardTypeMask::PLANESWALKER,
                ..Default::default()
            }),
            true,
            vec![],
            None,
        );
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[
            &[plains, vanilla, legend, walker, legend_blade, pw_gem],
            &[plains],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70267).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    play_and_resolve_land(&mut drv, PlayerId(0), plains);
    cast_and_resolve(&mut drv, PlayerId(0), vanilla, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), legend, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), walker, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), legend_blade, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), pw_gem, vec![]);

    // Add 1 more Plains on Turn 3 so we can afford {2} for legend_blade and {1} for pw_gem.
    drv.advance_to(3, GamePhase::Main1, 32);
    play_and_resolve_land(&mut drv, PlayerId(0), plains);

    let vanilla_id = drv.find_battlefield_object(PlayerId(0), vanilla).unwrap();
    let legend_id = drv.find_battlefield_object(PlayerId(0), legend).unwrap();
    let walker_id = drv.find_battlefield_object(PlayerId(0), walker).unwrap();
    let legblade_id = drv
        .find_battlefield_object(PlayerId(0), legend_blade)
        .unwrap();
    let pwgem_id = drv.find_battlefield_object(PlayerId(0), pw_gem).unwrap();

    // 1. Equip legendary creature {2}: rejects nonlegendary, accepts legendary.
    let leg_idx = equip_ability_index(drv.session.state(), legblade_id);
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: legblade_id,
            ability_index: leg_idx,
        },
    )
    .unwrap();
    assert!(
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Object { id: vanilla_id },
            },
        )
        .is_err()
    );
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: legend_id },
        },
    )
    .unwrap();
    drv.step(PlayerId(0), Action::AutoPayMana).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(legblade_id).unwrap().attached_to,
        Some(legend_id)
    );

    // 2. Equip planeswalker {1} (equip_allow_planeswalker: true): rejects non-PW creature, attaches to PW & survives SBAs.
    drv.advance_to(5, GamePhase::Main1, 32);
    let pw_idx = equip_ability_index(drv.session.state(), pwgem_id);
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: pwgem_id,
            ability_index: pw_idx,
        },
    )
    .unwrap();
    assert!(
        drv.step(
            PlayerId(0),
            Action::SelectTarget {
                target: TargetRef::Object { id: vanilla_id },
            },
        )
        .is_err()
    );
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: walker_id },
        },
    )
    .unwrap();
    drv.step(PlayerId(0), Action::AutoPayMana).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(pwgem_id).unwrap().attached_to,
        Some(walker_id)
    );

    // Explicit SBA check must keep pwgem attached to Planeswalker!
    let mut s = drv.session.state().clone();
    check_sbas(&mut s).unwrap();
    assert_eq!(
        s.object(pwgem_id).unwrap().attached_to,
        Some(walker_id),
        "CR 704.5n: equip_allow_planeswalker must keep Equipment attached to Planeswalker across SBAs"
    );
}

#[test]
fn synth_cr702_6a_equip_cost_reduction_first_zero_and_choice_cost() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land("synth_plains_eq8", "Synth Plains", "Plains", ManaColor::W);
    let knight = b.add_card("synth_knight_eq8", "Synth Knight", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .subtype("Knight")
            .pt(2, 2);
    });
    let zero_enabler = b.add_card("synth_zero_enabler_eq8", "Synth ZeroEnabler", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ENCHANTMENT)
            .static_ability(StaticDef {
                body: StaticBody::FirstActivationZeroCost {
                    ability_keyword: FirstActivationZeroKind::Equip,
                    during_your_turn_only: false,
                },
                condition: None,
            });
    });
    let knight_sid = b.intern_subtype("Knight");
    let knight_armor = b.add_card("synth_armor_eq8", "Synth Armor", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(
            c,
            Some("equip_1"),
            Some("{3}"),
            None,
            false,
            vec![],
            Some(ConditionalCostReduction {
                delta: CostDelta {
                    mode: CostDeltaMode::Reduce,
                    generic: 1,
                    per: Some(Box::new(Amount::Count {
                        of: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            subtypes_any: smallvec![knight_sid],
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    })),
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: None,
                },
                condition: None,
            }),
        );
    });
    let choice_gavel = b.add_card("synth_gavel_eq8", "Synth Gavel", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment");
        add_equip_keyword(
            c,
            Some("equip_3"),
            None,
            None,
            false,
            vec![CostDef::Choice {
                options: vec![
                    vec![CostDef::Mana {
                        cost: ManaCost::parse("{3}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                    vec![CostDef::PayLife {
                        amount: Box::new(Amount::Const { n: 2 }),
                        half_life_rounded_up: false,
                    }],
                ],
                forage: false,
            }],
            None,
        );
    });
    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[
            &[plains, knight, zero_enabler, knight_armor, choice_gavel],
            &[plains],
        ],
        &[],
        3,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 70268).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 16);

    play_and_resolve_land(&mut drv, PlayerId(0), plains);
    cast_and_resolve(&mut drv, PlayerId(0), knight, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), knight, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), zero_enabler, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), knight_armor, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), choice_gavel, vec![]);

    let knights: Vec<ObjectId> = drv
        .session
        .state()
        .player(PlayerId(0))
        .unwrap()
        .battlefield
        .iter()
        .copied()
        .filter(|&id| {
            drv.session
                .state()
                .object(id)
                .is_some_and(|o| o.card_def == knight)
        })
        .collect();
    assert_eq!(knights.len(), 2);
    let armor_id = drv
        .find_battlefield_object(PlayerId(0), knight_armor)
        .unwrap();
    let armor_idx = equip_ability_index(drv.session.state(), armor_id);

    // 1. First Equip activation uses FirstActivationZeroCost -> pays {0}, leaving Plains untapped!
    activate_equip_to(&mut drv, PlayerId(0), armor_id, armor_idx, knights[0]).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(armor_id).unwrap().attached_to,
        Some(knights[0])
    );
    let plains_id = drv.find_battlefield_object(PlayerId(0), plains).unwrap();
    assert!(
        !drv.session
            .state()
            .object(plains_id)
            .unwrap()
            .flags
            .contains(mtg_engine::ObjectFlags::TAPPED),
        "first Equip activation must be free via FirstActivationZeroCost"
    );
    assert_ne!(
        drv.session
            .state()
            .player(PlayerId(0))
            .unwrap()
            .used_first_activation_zero_mask
            & (1 << 0),
        0,
        "used_first_activation_zero_mask bit 0 must be set after first Equip activation"
    );

    // 2. Second Equip activation in the SAME turn pays {3} - {2} (2 Knights) = {1} via the single Plains!
    activate_equip_to(&mut drv, PlayerId(0), armor_id, armor_idx, knights[1]).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(armor_id).unwrap().attached_to,
        Some(knights[1])
    );
    assert!(
        drv.session
            .state()
            .object(plains_id)
            .unwrap()
            .flags
            .contains(mtg_engine::ObjectFlags::TAPPED),
        "second Equip activation must pay reduced {{1}} cost (Plains tapped)"
    );

    // 3. Non-mana CostDef::Choice Equip (`Gavel`-style): choose PayLife(2) branch (index 1).
    let gavel_id = drv
        .find_battlefield_object(PlayerId(0), choice_gavel)
        .unwrap();
    let gavel_idx = equip_ability_index(drv.session.state(), gavel_id);
    drv.step(
        PlayerId(0),
        Action::BeginActivate {
            object: gavel_id,
            ability_index: gavel_idx,
        },
    )
    .unwrap();
    drv.step(
        PlayerId(0),
        Action::SelectTarget {
            target: TargetRef::Object { id: knights[0] },
        },
    )
    .unwrap();
    let prompt_id = match drv.decision() {
        DecisionState::Prompt {
            request: ChoiceRequest::ChooseMode { prompt_id, .. },
        } => prompt_id,
        other => panic!("expected CostBranchChoice ChooseMode prompt, got {other:?}"),
    };
    drv.step(
        PlayerId(0),
        Action::Answer {
            prompt_id,
            answer: ChoiceAnswer::PickMode { index: 1 },
        },
    )
    .unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(gavel_id).unwrap().attached_to,
        Some(knights[0])
    );
    assert_eq!(drv.session.state().player(PlayerId(0)).unwrap().life, 18);
}

#[test]
fn synth_replay_match_activated_ability_index_equip_prefix() {
    let mut b = SynthBundleBuilder::new();
    let _plains =
        b.add_synth_basic_land("synth_plains_eq9", "Synth Plains", "Plains", ManaColor::W);
    let _jewel = b.add_card("synth_jewel_eq9", "Synth Jewel", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .mana_ability("mana_0", &[ManaColor::W]);
    });
    let _mystic = b.add_synth_vanilla("synth_mystic_eq9", "Synth Mystic", "{0}", 1, 1);
    let _multisword = b.add_card("synth_multisword_eq9", "Synth MultiSword", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment")
            .static_ability(StaticDef {
                body: StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::AttachedTo,
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 1 }),
                    },
                    active_in_zone: None,
                },
                condition: None,
            });
        // Multi-ability Equipment: [Static (idx 0), equip_1 {1} (idx 1), equip_2 {3} (idx 2)]
        add_equip_keyword(c, Some("equip_1"), Some("{1}"), None, false, vec![], None);
        add_equip_keyword(c, Some("equip_2"), Some("{3}"), None, false, vec![], None);
    });
    let db = b.build_db();

    // Find a deterministic Mulberry32Compat seed where P0's opening 7 contains >=1 mystic,
    // >=1 multisword, and >=2 jewels.
    let mut sorted_p0 = Vec::with_capacity(60);
    sorted_p0.extend(std::iter::repeat_n("synth_jewel_eq9", 20));
    sorted_p0.extend(std::iter::repeat_n("synth_multisword_eq9", 20));
    sorted_p0.extend(std::iter::repeat_n("synth_mystic_eq9", 20));
    let sorted_p1 = vec!["synth_plains_eq9"; 60];

    let mut chosen_seed = 0u32;
    let mut p0_lib_init = Vec::new();
    let mut p1_lib_init = Vec::new();
    for s in 1..500u32 {
        let mut rng = RngState::mulberry32(s);
        let mut l0 = sorted_p0.clone();
        rng.shuffle(&mut l0);
        let mut l1 = sorted_p1.clone();
        rng.shuffle(&mut l1);
        let h0 = &l0[0..7];
        if h0.iter().filter(|&&c| c == "synth_jewel_eq9").count() >= 2
            && h0.contains(&"synth_multisword_eq9")
            && h0.contains(&"synth_mystic_eq9")
        {
            chosen_seed = s;
            p0_lib_init = l0;
            p1_lib_init = l1;
            break;
        }
    }
    assert_ne!(
        chosen_seed, 0,
        "must find opening hand with mystic + sword + 2 jewels"
    );

    let mut p0_hand: Vec<&str> = p0_lib_init[0..7].to_vec();
    let p1_hand: Vec<&str> = p1_lib_init[0..7].to_vec();

    // Verify ability_index offsets directly on resolved abilities.
    let mut state_probe = GameState::new(
        Arc::clone(&db),
        GameConfig::standard_1v1(),
        &[
            DeckSpec {
                commanders: vec![],
                mainboard: vec![db.id_by_oracle("synth_multisword_eq9").unwrap(); 60],
                sideboard: vec![],
            },
            DeckSpec {
                commanders: vec![],
                mainboard: vec![db.id_by_oracle("synth_plains_eq9").unwrap(); 60],
                sideboard: vec![],
            },
        ],
        1,
    )
    .unwrap();
    let sword_oid = state_probe.player(PlayerId(0)).unwrap().hand[0];
    mtg_engine::move_object(
        &mut state_probe,
        sword_oid,
        Zone::Battlefield,
        Default::default(),
        None,
    )
    .unwrap();
    let bf_sword = state_probe.player(PlayerId(0)).unwrap().battlefield[0];
    let resolved = resolve_activated_abilities(&state_probe, bf_sword);
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].ability_index, 1);
    assert_eq!(resolved[0].ability.id, "equip_1");
    assert_eq!(resolved[1].ability_index, 2);
    assert_eq!(resolved[1].ability.id, "equip_2");

    // Build a TsReplayTrace testing BOTH `"equip_1"` and `"f0:equip_1"` via `run_ts_diff_test`.
    // Because only 2 mana sources exist (2x `synth_jewel_eq9`), `"f0:equip_1"` MUST match `equip_1`
    // (`{1}`) rather than falling back to `abilities[1]` (`equip_2` `{3}`, which cannot be paid).
    #[derive(Clone)]
    struct BfEntry {
        oracle: &'static str,
        tapped: bool,
        pt: Option<[i32; 2]>,
        types: Vec<&'static str>,
        keywords: Vec<&'static str>,
        attached_to: Option<&'static str>,
    }

    let make_p = |seat: u8, hand: &[&str], bf: &[BfEntry]| {
        let bf_json: Vec<serde_json::Value> = bf
            .iter()
            .map(|e| {
                serde_json::json!({
                    "oracleId": e.oracle,
                    "tapped": e.tapped,
                    "pt": e.pt,
                    "types": e.types,
                    "keywords": e.keywords,
                    "counters": [],
                    "damageMarked": 0,
                    "attachedToOracleId": e.attached_to
                })
            })
            .collect();
        serde_json::json!({
            "seat": seat,
            "life": 20,
            "lost": false,
            "poison": 0,
            "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": {
                "library": 53,
                "hand": hand.len() as u32,
                "battlefield": bf.len() as u32,
                "graveyard": 0,
                "exile": 0,
                "command": 0
            },
            "handOracleIds": hand,
            "battlefield": bf_json
        })
    };

    let p1_const = make_p(1, &p1_hand, &[]);
    let mut bf0: Vec<BfEntry> = Vec::new();
    let mut steps: Vec<serde_json::Value> = Vec::new();
    let mut step_idx = 0u32;

    let push_step = |steps: &mut Vec<serde_json::Value>,
                     step_idx: &mut u32,
                     actor: u8,
                     action: serde_json::Value,
                     locators: serde_json::Value,
                     priority_seat: u8,
                     stack_depth: u16,
                     prompt_kind: Option<&str>,
                     hand0: &[&str],
                     bf0: &[BfEntry]| {
        steps.push(serde_json::json!({
            "stepIndex": *step_idx,
            "actorSeat": actor,
            "action": action,
            "objectLocators": locators,
            "digestAfter": {
                "turnNumber": 1,
                "activeSeat": 0,
                "prioritySeat": priority_seat,
                "phase": "main1",
                "stackDepth": stack_depth,
                "pendingPromptKind": prompt_kind,
                "players": [make_p(0, hand0, bf0), p1_const.clone()]
            }
        }));
        *step_idx += 1;
    };

    push_step(
        &mut steps,
        &mut step_idx,
        0,
        serde_json::json!({ "type": "mulligan_keep" }),
        serde_json::json!({}),
        1,
        0,
        Some("mulligan"),
        &p0_hand,
        &bf0,
    );
    push_step(
        &mut steps,
        &mut step_idx,
        1,
        serde_json::json!({ "type": "mulligan_keep" }),
        serde_json::json!({}),
        0,
        0,
        None,
        &p0_hand,
        &bf0,
    );

    let mut cast_zero_permanent = |oracle: &'static str,
                                   inst: &str,
                                   pt: Option<[i32; 2]>,
                                   types: Vec<&'static str>,
                                   keywords: Vec<&'static str>| {
        let zidx = p0_hand.iter().position(|&c| c == oracle).unwrap();
        p0_hand.remove(zidx);
        push_step(
            &mut steps,
            &mut step_idx,
            0,
            serde_json::json!({ "type": "cast_spell", "instanceId": inst, "fromZone": "hand" }),
            serde_json::json!({
                inst: { "seat": 0, "zone": "hand", "zoneIndex": zidx, "oracleId": oracle }
            }),
            0,
            1,
            None,
            &p0_hand,
            &bf0,
        );
        push_step(
            &mut steps,
            &mut step_idx,
            0,
            serde_json::json!({ "type": "pass_priority" }),
            serde_json::json!({}),
            1,
            1,
            None,
            &p0_hand,
            &bf0,
        );
        bf0.push(BfEntry {
            oracle,
            tapped: false,
            pt,
            types,
            keywords,
            attached_to: None,
        });
        push_step(
            &mut steps,
            &mut step_idx,
            1,
            serde_json::json!({ "type": "pass_priority" }),
            serde_json::json!({}),
            0,
            0,
            None,
            &p0_hand,
            &bf0,
        );
    };

    cast_zero_permanent(
        "synth_mystic_eq9",
        "c_mystic",
        Some([1, 1]),
        vec!["Creature"],
        vec![],
    );
    cast_zero_permanent(
        "synth_multisword_eq9",
        "c_sword",
        None,
        vec!["Artifact"],
        vec!["equip"],
    );
    cast_zero_permanent(
        "synth_jewel_eq9",
        "c_jewel_0",
        None,
        vec!["Artifact"],
        vec![],
    );
    cast_zero_permanent(
        "synth_jewel_eq9",
        "c_jewel_1",
        None,
        vec!["Artifact"],
        vec![],
    );

    // Activation 1: "equip_1" -> taps c_jewel_0, attaches sword to mystic (+1/+1 -> 2/2).
    bf0[2].tapped = true;
    push_step(
        &mut steps,
        &mut step_idx,
        0,
        serde_json::json!({
            "type": "activate_ability",
            "instanceId": "c_sword",
            "abilityId": "equip_1",
            "targets": [{ "kind": "permanent", "id": "c_mystic" }]
        }),
        serde_json::json!({
            "c_sword": { "seat": 0, "zone": "battlefield", "zoneIndex": 1, "oracleId": "synth_multisword_eq9" },
            "c_mystic": { "seat": 0, "zone": "battlefield", "zoneIndex": 0, "oracleId": "synth_mystic_eq9" }
        }),
        0,
        1,
        None,
        &p0_hand,
        &bf0,
    );
    push_step(
        &mut steps,
        &mut step_idx,
        0,
        serde_json::json!({ "type": "pass_priority" }),
        serde_json::json!({}),
        1,
        1,
        None,
        &p0_hand,
        &bf0,
    );
    bf0[1].attached_to = Some("synth_mystic_eq9");
    bf0[0].pt = Some([2, 2]);
    push_step(
        &mut steps,
        &mut step_idx,
        1,
        serde_json::json!({ "type": "pass_priority" }),
        serde_json::json!({}),
        0,
        0,
        None,
        &p0_hand,
        &bf0,
    );

    // Activation 2: "f0:equip_1" -> must match `equip_1` ({1}), NOT positional index 1 (`equip_2` {3}).
    bf0[3].tapped = true;
    push_step(
        &mut steps,
        &mut step_idx,
        0,
        serde_json::json!({
            "type": "activate_ability",
            "instanceId": "c_sword",
            "abilityId": "f0:equip_1",
            "targets": [{ "kind": "permanent", "id": "c_mystic" }]
        }),
        serde_json::json!({
            "c_sword": { "seat": 0, "zone": "battlefield", "zoneIndex": 1, "oracleId": "synth_multisword_eq9" },
            "c_mystic": { "seat": 0, "zone": "battlefield", "zoneIndex": 0, "oracleId": "synth_mystic_eq9" }
        }),
        0,
        1,
        None,
        &p0_hand,
        &bf0,
    );
    push_step(
        &mut steps,
        &mut step_idx,
        0,
        serde_json::json!({ "type": "pass_priority" }),
        serde_json::json!({}),
        1,
        1,
        None,
        &p0_hand,
        &bf0,
    );
    push_step(
        &mut steps,
        &mut step_idx,
        1,
        serde_json::json!({ "type": "pass_priority" }),
        serde_json::json!({}),
        0,
        0,
        None,
        &p0_hand,
        &bf0,
    );

    let trace: TsReplayTrace = serde_json::from_value(serde_json::json!({
        "formatVersion": 1,
        "compilerVersion": 46,
        "seed": chosen_seed,
        "postInitRngState": chosen_seed,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            {
                "seat": 0,
                "deckId": "s0",
                "commanders": [],
                "libraryOrderInitial": p0_lib_init,
                "handInitial": p0_lib_init[0..7].to_vec()
            },
            {
                "seat": 1,
                "deckId": "s1",
                "commanders": [],
                "libraryOrderInitial": p1_lib_init,
                "handInitial": p1_lib_init[0..7].to_vec()
            }
        ],
        "steps": steps
    }))
    .unwrap();

    let report = run_ts_diff_test(db, &trace)
        .expect("TsReplayTrace with equip_1 and f0:equip_1 must pass run_ts_diff_test");
    assert_eq!(report.steps_replayed, steps.len());
}

#[test]
fn synth_cr702_6a_paired_equip_cost_reduction_and_static_cost_modifier_no_double_count() {
    let mut b = SynthBundleBuilder::new();
    let plains_1 = b.add_synth_basic_land(
        "synth_plains_1_eq10",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let plains_2 = b.add_synth_basic_land(
        "synth_plains_2_eq10",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let plains_3 = b.add_synth_basic_land(
        "synth_plains_3_eq10",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let bear = b.add_synth_vanilla("synth_bear_eq10", "Synth Bear", "{0}", 2, 2);

    let add_synth_boots = |b: &mut SynthBundleBuilder, oid: &str| {
        b.add_card(oid, "Synth Boots", |c| {
            c.mana_cost("{0}")
                .types(CardTypeMask::ARTIFACT)
                .subtype("Equipment");
            add_equip_keyword(c, Some("equip_1"), Some("{1}"), None, false, vec![], None);
        })
    };
    let boots_1 = add_synth_boots(&mut b, "synth_boots_1_eq10");
    let boots_2 = add_synth_boots(&mut b, "synth_boots_2_eq10");
    let boots_3 = add_synth_boots(&mut b, "synth_boots_3_eq10");

    let equip_sid = b.intern_subtype("Equipment");
    let equip_kid = b.intern_keyword("equip");
    let reduction_delta = CostDelta {
        mode: CostDeltaMode::Reduce,
        generic: 1,
        per: Some(Box::new(Amount::Count {
            of: Box::new(ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                subtypes_any: smallvec![equip_sid],
                controller: Some(RelativeControl::You),
                flags: FilterFlags::OTHER,
                ..Default::default()
            }),
        })),
        additional_costs: vec![],
        colored: SmallVec::new(),
        hybrid_colored: SmallVec::new(),
        colored_only: false,
        min_total_mana: None,
        max_reduction: None,
        per_target_life: None,
    };

    let plate = b.add_card("synth_plate_eq10", "Synth Plate", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .subtype("Equipment")
            .static_ability(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: Some(equip_kid),
                        filter: None,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: true,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta: reduction_delta.clone(),
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: None,
            });
        add_equip_keyword(
            c,
            Some("equip_1"),
            Some("{3}"),
            None,
            false,
            vec![],
            Some(ConditionalCostReduction {
                delta: reduction_delta,
                condition: None,
            }),
        );
    });

    let db = b.build_db();
    let (cfg, decks) = guaranteed_decks(
        &[
            &[
                plains_1, plains_2, plains_3, bear, plate, boots_1, boots_2, boots_3,
            ],
            &[plains_1],
        ],
        &[],
        6,
        GameConfig::standard_1v1(),
    );
    let mut drv = SessionDriver::new(db, cfg, decks, 702610).unwrap();
    drv.keep_all_hands();
    drv.advance_to(1, GamePhase::Main1, 64);
    play_and_resolve_land(&mut drv, PlayerId(0), plains_1);
    drv.advance_to(3, GamePhase::Main1, 128);
    play_and_resolve_land(&mut drv, PlayerId(0), plains_2);
    drv.advance_to(5, GamePhase::Main1, 128);
    play_and_resolve_land(&mut drv, PlayerId(0), plains_3);

    cast_and_resolve(&mut drv, PlayerId(0), bear, vec![]);
    cast_and_resolve(&mut drv, PlayerId(0), plate, vec![]);

    let bear_id = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    let plate_id = drv.find_battlefield_object(PlayerId(0), plate).unwrap();
    let plate_idx = equip_ability_index(drv.session.state(), plate_id);
    let plains_ids = [
        drv.find_battlefield_object(PlayerId(0), plains_1).unwrap(),
        drv.find_battlefield_object(PlayerId(0), plains_2).unwrap(),
        drv.find_battlefield_object(PlayerId(0), plains_3).unwrap(),
    ];
    let count_tapped_plains = |state: &GameState| {
        plains_ids
            .iter()
            .filter(|&&id| {
                state
                    .object(id)
                    .unwrap()
                    .flags
                    .contains(mtg_engine::ObjectFlags::TAPPED)
            })
            .count()
    };

    // 0 other Equipment: Equip {3} pays {3} -> 3 Plains tapped.
    assert_eq!(count_tapped_plains(drv.session.state()), 0);
    activate_equip_to(&mut drv, PlayerId(0), plate_id, plate_idx, bear_id).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        drv.session.state().object(plate_id).unwrap().attached_to,
        Some(bear_id)
    );
    assert_eq!(
        count_tapped_plains(drv.session.state()),
        3,
        "0 other Equipment must pay full Equip {{3}}"
    );

    // 1 other Equipment: Equip {3} - {1} = {2} -> 2 Plains tapped (1 remains untapped).
    drv.advance_to(7, GamePhase::Main1, 128);
    assert_eq!(count_tapped_plains(drv.session.state()), 0);
    cast_and_resolve(&mut drv, PlayerId(0), boots_1, vec![]);
    activate_equip_to(&mut drv, PlayerId(0), plate_id, plate_idx, bear_id).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        count_tapped_plains(drv.session.state()),
        2,
        "1 other Equipment must pay Equip {{2}} (not double-reduced to {{1}})"
    );

    // 2 other Equipment: Equip {3} - {2} = {1} -> 1 Plains tapped (2 remain untapped).
    drv.advance_to(9, GamePhase::Main1, 128);
    assert_eq!(count_tapped_plains(drv.session.state()), 0);
    cast_and_resolve(&mut drv, PlayerId(0), boots_2, vec![]);
    activate_equip_to(&mut drv, PlayerId(0), plate_id, plate_idx, bear_id).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        count_tapped_plains(drv.session.state()),
        1,
        "2 other Equipment must pay Equip {{1}}"
    );

    // 3 other Equipment: Equip {3} - {3} = {0} -> 0 Plains tapped (all 3 remain untapped).
    drv.advance_to(11, GamePhase::Main1, 128);
    assert_eq!(count_tapped_plains(drv.session.state()), 0);
    cast_and_resolve(&mut drv, PlayerId(0), boots_3, vec![]);
    activate_equip_to(&mut drv, PlayerId(0), plate_id, plate_idx, bear_id).unwrap();
    drv.resolve_all_stack(16);
    assert_eq!(
        count_tapped_plains(drv.session.state()),
        0,
        "3 other Equipment must pay Equip {{0}}"
    );
}
