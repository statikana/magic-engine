//! Release performance benchmarks for `mtg-engine` (F17).
//!
//! Zero external benchmark harness dependencies (`harness = false`); uses `std::time::Instant`
//! and `std::hint::black_box` exclusively outside `crates/mtg-engine/src/`.
//! All synthetic cards use `Synth *` / `synth_*` naming only.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use mtg_engine::{
    Action, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, GameConfig, MacroAction,
    MatchSession, ObjectId, PlayerId,
};
use mtg_ir::{
    AbilityDef, ActivatedAbility, Affected, Amount, CardDatabase, CardDef, CardDefId, CardLayout,
    CardTypeMask, Characteristics, ColorMask, CostDef, EffectOp, EffectStep, FaceDef, GamePhase,
    IR_V2_VERSION, InternTables, IrV2Bundle, Layer, LayerMod, ManaColor, ManaCost, ManaProduction,
    ObjectFilter, PrintedPt, RelativeControl, Selector, Span, StaticAbility, StaticBody, StaticDef,
    SubtypeId, SupertypeMask, Timing, Zone, ZoneMask,
};
use smallvec::SmallVec;

const BENCH_NAMES: &[&str] = &[
    "clone_4p_midgame",
    "clone_1v1_scaling",
    "stepping_4p_midgame",
    "layer_cache_eval",
    "snapshot_roundtrip",
];

fn make_basic_land(id: &str, name: &str, subtype: SubtypeId, color: ManaColor) -> CardDef {
    CardDef {
        oracle_id: id.to_string(),
        compiler_version: 46,
        text_hash: "00000000".to_string(),
        name: name.to_string(),
        layout: CardLayout::Normal,
        faces: vec![FaceDef {
            name: name.to_string(),
            characteristics: Characteristics {
                mana_cost: ManaCost::default(),
                types: CardTypeMask::LAND,
                subtypes: SmallVec::from_slice(&[subtype]),
                supertypes: SupertypeMask::BASIC,
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: None,
                loyalty: None,
                defense: None,
            },
            abilities: vec![AbilityDef::Activated(ActivatedAbility {
                id: "mana_tap".to_string(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![EffectStep {
                    op: EffectOp::AddMana {
                        production: ManaProduction::Fixed {
                            symbols: SmallVec::from_slice(&[color]),
                        },
                        restriction: None,
                        restriction_spec: None,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    subject: Selector::None,
                    actor: None,
                    amount: None,
                    optional: None,
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span: Span::default(),
                }],
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
            })],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }],
        fully_supported: true,
    }
}

fn make_vanilla_creature(
    id: &str,
    name: &str,
    subtype: SubtypeId,
    power: i32,
    toughness: i32,
) -> CardDef {
    CardDef {
        oracle_id: id.to_string(),
        compiler_version: 46,
        text_hash: "00000000".to_string(),
        name: name.to_string(),
        layout: CardLayout::Normal,
        faces: vec![FaceDef {
            name: name.to_string(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse("{0}"),
                types: CardTypeMask::CREATURE,
                subtypes: SmallVec::from_slice(&[subtype]),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: Some(PrintedPt::Fixed { power, toughness }),
                loyalty: None,
                defense: None,
            },
            abilities: vec![],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }],
        fully_supported: true,
    }
}

fn make_anthem_enchantment(id: &str, name: &str, p: i32, t: i32) -> CardDef {
    CardDef {
        oracle_id: id.to_string(),
        compiler_version: 46,
        text_hash: "00000000".to_string(),
        name: name.to_string(),
        layout: CardLayout::Normal,
        faces: vec![FaceDef {
            name: name.to_string(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse("{0}"),
                types: CardTypeMask::ENCHANTMENT,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: None,
                loyalty: None,
                defense: None,
            },
            abilities: vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: p }),
                            toughness: Box::new(Amount::Const { n: t }),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Span::default(),
            })],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }],
        fully_supported: true,
    }
}

struct BenchDeckFixture {
    db: Arc<CardDatabase>,
    land_id: CardDefId,
    bear_id: CardDefId,
    anthem_id: CardDefId,
    cmd_id: CardDefId,
}

fn build_fixture() -> BenchDeckFixture {
    let mut tables = InternTables::default();
    let plains_sub = SubtypeId(tables.subtypes.len() as u16);
    tables.subtypes.push("Plains".to_string());
    let beast_sub = SubtypeId(tables.subtypes.len() as u16);
    tables.subtypes.push("SynthBeast".to_string());

    let cards = vec![
        make_basic_land(
            "synth_bench_plains",
            "Synth Plains",
            plains_sub,
            ManaColor::W,
        ),
        make_vanilla_creature("synth_bench_bear", "Synth Bear", beast_sub, 2, 2),
        make_anthem_enchantment("synth_bench_anthem", "Synth Anthem", 1, 1),
        make_vanilla_creature("synth_bench_cmd", "Synth Cmd", beast_sub, 3, 3),
    ];
    let db = Arc::new(
        CardDatabase::from_bundle(IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables,
            cards,
            decks: vec![],
        })
        .expect("bench bundle must build CardDatabase"),
    );
    BenchDeckFixture {
        db,
        land_id: CardDefId(0),
        bear_id: CardDefId(1),
        anthem_id: CardDefId(2),
        cmd_id: CardDefId(3),
    }
}

fn build_4p_commander_decks(fix: &BenchDeckFixture) -> Vec<DeckSpec> {
    let mut main = Vec::with_capacity(99);
    main.extend(std::iter::repeat_n(fix.land_id, 39));
    main.extend(std::iter::repeat_n(fix.bear_id, 40));
    main.extend(std::iter::repeat_n(fix.anthem_id, 20));
    let deck = DeckSpec {
        commanders: vec![fix.cmd_id],
        mainboard: main,
        sideboard: vec![],
    };
    vec![deck.clone(), deck.clone(), deck.clone(), deck]
}

fn find_hand_card(session: &MatchSession, player: PlayerId, def: CardDefId) -> Option<ObjectId> {
    let vm = session.view_model(Some(player));
    vm.players
        .iter()
        .find(|p| p.id == player)?
        .hand
        .iter()
        .find(|o| o.card_def == def)
        .map(|o| o.id)
}

fn keep_all_hands(session: &mut MatchSession) {
    for _ in 0..64 {
        let vm = session.view_model(None);
        if !matches!(vm.decision, DecisionState::Mulligan) {
            break;
        }
        let actor = session.current_actor().expect("mulligan actor");
        session.apply_action(actor, Action::KeepHand).unwrap();
    }
}

fn total_battlefield_count(session: &MatchSession) -> usize {
    session
        .state()
        .players()
        .iter()
        .map(|p| p.battlefield.len())
        .sum()
}

fn populate_4p_midgame_session(fix: &BenchDeckFixture, seed: u64) -> (MatchSession, Vec<DeckSpec>) {
    let decks = build_4p_commander_decks(fix);
    let mut session = MatchSession::new(
        Arc::clone(&fix.db),
        GameConfig::commander(4),
        decks.clone(),
        seed,
    )
    .unwrap();
    keep_all_hands(&mut session);

    for _ in 0..500 {
        if total_battlefield_count(&session) >= 20 {
            break;
        }
        let actor = session.current_actor().expect("active actor");
        match session.decision_info().state_kind {
            mtg_engine::DecisionKind::Priority => {
                let s = session.state();
                if s.phase() == GamePhase::Main1
                    && s.active_player() == actor
                    && s.stack().is_empty()
                {
                    if let Some(anth) = find_hand_card(&session, actor, fix.anthem_id) {
                        session
                            .apply_macro_action(
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
                    if let Some(bear) = find_hand_card(&session, actor, fix.bear_id) {
                        session
                            .apply_macro_action(
                                actor,
                                MacroAction::CastSpell {
                                    object: bear,
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
                    if let Some(land) = find_hand_card(&session, actor, fix.land_id)
                        && session.legal_actions(actor).iter().any(
                            |a| matches!(a, Action::PlayLand { object, .. } if *object == land),
                        )
                    {
                        session
                            .apply_action(
                                actor,
                                Action::PlayLand {
                                    object: land,
                                    from_zone: Zone::Hand,
                                    face_index: 0,
                                },
                            )
                            .unwrap();
                        continue;
                    }
                }
                session.apply_action(actor, Action::PassPriority).unwrap();
            }
            mtg_engine::DecisionKind::Prompt => {
                if let DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            ..
                        },
                } = session.view_model(Some(actor)).decision
                {
                    session
                        .apply_action(
                            actor,
                            Action::Answer {
                                prompt_id,
                                answer: ChoiceAnswer::PickObject { id: candidates[0] },
                            },
                        )
                        .unwrap();
                } else {
                    let legal = session.legal_actions(actor);
                    let chosen = legal
                        .into_iter()
                        .find(|a| !matches!(a, Action::Concede))
                        .expect("non-concede prompt action");
                    session.apply_action(actor, chosen).unwrap();
                }
            }
            mtg_engine::DecisionKind::DeclareAttackers => {
                session
                    .apply_action(actor, Action::ConfirmAttackers)
                    .unwrap();
            }
            mtg_engine::DecisionKind::DeclareBlockers => {
                session
                    .apply_action(actor, Action::ConfirmBlockers)
                    .unwrap();
            }
            other => panic!("unexpected decision kind while populating midgame board: {other:?}"),
        }
    }

    let total_bf = total_battlefield_count(&session);
    assert!(
        total_bf >= 20,
        "4p midgame fixture must have >= 20 battlefield permanents (got {total_bf})"
    );
    (session, decks)
}

fn bench_clone_4p_midgame() {
    let fix = build_fixture();
    let (session, _) = populate_4p_midgame_session(&fix, 174);
    let state = session.state();
    let _ = black_box(state.evaluate_board().rev);

    let iters = 10_000usize;
    let batch_size = 100usize;
    let batches = iters / batch_size;
    let mut batch_us = Vec::with_capacity(batches);

    let t_total = Instant::now();
    let mut acc = 0u64;
    for _ in 0..batches {
        let t0 = Instant::now();
        for _ in 0..batch_size {
            let cloned = state.clone();
            acc = acc.wrapping_add(black_box(cloned.layer_rev()));
        }
        let us = (t0.elapsed().as_secs_f64() * 1_000_000.0) / (batch_size as f64);
        batch_us.push(us);
    }
    let elapsed = t_total.elapsed();
    assert!(acc > 0);

    batch_us.sort_by(|a, b| a.total_cmp(b));
    let mean_us = (elapsed.as_secs_f64() * 1_000_000.0) / (iters as f64);
    let p50 = batch_us[batches / 2];
    let p95 = batch_us[((batches * 95) / 100).min(batches - 1)];
    let max_us = *batch_us.last().unwrap_or(&mean_us);

    println!(
        "bench clone_4p_midgame: mean={mean_us:.2} us/clone p50={p50:.2} us p95={p95:.2} us max={max_us:.2} us (bf={})",
        total_battlefield_count(&session)
    );
    if !cfg!(debug_assertions) {
        assert!(
            mean_us < 20.0,
            "release GameState::clone() mean {mean_us:.2} us >= 20.0 us"
        );
    }
}

fn bench_clone_1v1_scaling() {
    let fix = build_fixture();
    let d10 = DeckSpec {
        commanders: vec![],
        mainboard: vec![fix.land_id; 10],
        sideboard: vec![],
    };
    let mut main_large = vec![fix.bear_id; 75];
    main_large.extend(std::iter::repeat_n(fix.anthem_id, 75));
    let d150_p0 = DeckSpec {
        commanders: vec![],
        mainboard: main_large,
        sideboard: vec![],
    };
    let d150_p1 = DeckSpec {
        commanders: vec![],
        mainboard: vec![fix.land_id; 150],
        sideboard: vec![],
    };

    let s_small = MatchSession::new(
        Arc::clone(&fix.db),
        GameConfig::standard_1v1(),
        vec![d10.clone(), d10],
        272,
    )
    .unwrap();
    let mut s_large = MatchSession::new(
        Arc::clone(&fix.db),
        GameConfig::standard_1v1(),
        vec![d150_p0, d150_p1],
        272,
    )
    .unwrap();
    keep_all_hands(&mut s_large);

    for _ in 0..300 {
        if s_large.state().players()[0].battlefield.len() >= 12 {
            break;
        }
        let actor = s_large.current_actor().unwrap();
        match s_large.decision_info().state_kind {
            mtg_engine::DecisionKind::Priority => {
                let st = s_large.state();
                if st.phase() == GamePhase::Main1
                    && st.active_player() == actor
                    && st.stack().is_empty()
                    && actor == PlayerId(0)
                    && let Some(obj) = find_hand_card(&s_large, actor, fix.bear_id)
                        .or_else(|| find_hand_card(&s_large, actor, fix.anthem_id))
                {
                    s_large
                        .apply_macro_action(
                            actor,
                            MacroAction::CastSpell {
                                object: obj,
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
                s_large.apply_action(actor, Action::PassPriority).unwrap();
            }
            mtg_engine::DecisionKind::DeclareAttackers => {
                s_large
                    .apply_action(actor, Action::ConfirmAttackers)
                    .unwrap();
            }
            mtg_engine::DecisionKind::DeclareBlockers => {
                s_large
                    .apply_action(actor, Action::ConfirmBlockers)
                    .unwrap();
            }
            mtg_engine::DecisionKind::Prompt => {
                let legal = s_large.legal_actions(actor);
                let chosen = legal
                    .into_iter()
                    .find(|a| !matches!(a, Action::Concede))
                    .unwrap();
                s_large.apply_action(actor, chosen).unwrap();
            }
            other => panic!("unexpected state in clone_1v1_scaling setup: {other:?}"),
        }
    }
    assert!(
        s_large.state().players()[0].battlefield.len() >= 12,
        "1v1 large board must have >= 12 permanents"
    );

    let iters = 1_000usize;
    let mut acc = 0u64;
    let t0 = Instant::now();
    for _ in 0..iters {
        let cloned = s_small.state().clone();
        acc = acc.wrapping_add(black_box(cloned.layer_rev()));
    }
    let d_small = t0.elapsed();

    let t1 = Instant::now();
    for _ in 0..iters {
        let cloned = s_large.state().clone();
        acc = acc.wrapping_add(black_box(cloned.layer_rev()));
    }
    let d_large = t1.elapsed();
    assert!(acc > 0);

    let small_us = (d_small.as_secs_f64() * 1_000_000.0) / (iters as f64);
    let large_us = (d_large.as_secs_f64() * 1_000_000.0) / (iters as f64);
    println!(
        "bench clone_1v1_scaling: small_10={small_us:.2} us/clone large_150={large_us:.2} us/clone"
    );
    if !cfg!(debug_assertions) {
        assert!(
            large_us < 20.0,
            "release 150-card clone {large_us:.2} us >= 20.0 us"
        );
        assert!(
            d_large.as_nanos()
                <= d_small
                    .as_nanos()
                    .saturating_mul(10)
                    .saturating_add(5_000_000),
            "1v1 clone scaling exceeded 10x + 5ms bound (small={d_small:?}, large={d_large:?})"
        );
    }
}

fn bench_stepping_4p_midgame() {
    let fix = build_fixture();
    let (mut session, decks) = populate_4p_midgame_session(&fix, 175);

    let target_steps = 10_000usize;
    let mut steps = 0usize;
    let t0 = Instant::now();
    while steps < target_steps {
        if matches!(session.state().decision(), DecisionState::GameOver { .. }) {
            session = MatchSession::new(
                Arc::clone(&fix.db),
                GameConfig::commander(4),
                decks.clone(),
                175,
            )
            .unwrap();
            keep_all_hands(&mut session);
        }
        let actor = session.current_actor().expect("step actor");
        let legal = session.legal_actions(actor);
        let chosen = legal
            .iter()
            .find(|a| {
                matches!(
                    a,
                    Action::PlayLand { .. }
                        | Action::BeginCast { .. }
                        | Action::BeginActivate { .. }
                        | Action::AssignAttacker { .. }
                        | Action::ConfirmAttackers
                        | Action::ConfirmBlockers
                )
            })
            .or_else(|| legal.iter().find(|a| !matches!(a, Action::Concede)))
            .unwrap_or(&legal[0])
            .clone();
        session.apply_action(actor, chosen).unwrap();
        steps += 1;
    }
    let elapsed = t0.elapsed();
    let steps_per_sec = (steps as f64) / elapsed.as_secs_f64().max(1e-9);
    assert!(
        session.state().turn_number() > 1,
        "10,000 steps must advance past turn 1 (turn={})",
        session.state().turn_number()
    );

    println!(
        "bench stepping_4p_midgame: {steps_per_sec:.1} steps/sec (turn={}, elapsed={elapsed:?})",
        session.state().turn_number()
    );
    if !cfg!(debug_assertions) {
        assert!(
            steps_per_sec > 10_000.0,
            "release stepping throughput {steps_per_sec:.1} steps/sec <= 10,000.0"
        );
    }
}

fn bench_layer_cache_eval() {
    let fix = build_fixture();
    let (session, _) = populate_4p_midgame_session(&fix, 176);
    let state = session.state();
    let _ = black_box(state.evaluate_board().rev);

    let warm_iters = 10_000usize;
    let mut acc = 0u64;
    let t_warm = Instant::now();
    for _ in 0..warm_iters {
        acc = acc.wrapping_add(black_box(state.evaluate_board().rev));
    }
    let warm_ns = (t_warm.elapsed().as_secs_f64() * 1_000_000_000.0) / (warm_iters as f64);

    let cold_iters = 1_000usize;
    let cold_states: Vec<_> = (0..cold_iters).map(|_| state.clone()).collect();
    let t_cold = Instant::now();
    for s in &cold_states {
        acc = acc.wrapping_add(black_box(s.evaluate_board().rev));
    }
    let cold_ns = (t_cold.elapsed().as_secs_f64() * 1_000_000_000.0) / (cold_iters as f64);
    assert!(acc > 0);

    println!(
        "bench layer_cache_eval: warm_hit={warm_ns:.1} ns/eval cold_recompute={cold_ns:.1} ns/eval"
    );
}

fn bench_snapshot_roundtrip() {
    let fix = build_fixture();
    let (session, _) = populate_4p_midgame_session(&fix, 177);

    let sample_bytes = session.snapshot_bytes().unwrap();
    let payload_len = sample_bytes.len();
    let iters = 500usize;
    let mut acc = 0usize;
    let t0 = Instant::now();
    for _ in 0..iters {
        let bytes = session.snapshot_bytes().unwrap();
        let restored = MatchSession::from_snapshot(Arc::clone(&fix.db), &bytes).unwrap();
        acc = acc.wrapping_add(black_box(restored.state().players().len()));
    }
    let us_per_rt = (t0.elapsed().as_secs_f64() * 1_000_000.0) / (iters as f64);
    assert!(acc > 0);

    println!("bench snapshot_roundtrip: {us_per_rt:.2} us/roundtrip payload_bytes={payload_len}");
}

fn run_bench(name: &str) {
    match name {
        "clone_4p_midgame" => bench_clone_4p_midgame(),
        "clone_1v1_scaling" => bench_clone_1v1_scaling(),
        "stepping_4p_midgame" => bench_stepping_4p_midgame(),
        "layer_cache_eval" => bench_layer_cache_eval(),
        "snapshot_roundtrip" => bench_snapshot_roundtrip(),
        other => panic!("unknown benchmark {other}"),
    }
}

fn main() {
    let mut list_only = false;
    let mut exact = false;
    let mut filters: Vec<String> = Vec::new();
    let mut skip_next = false;

    for arg in std::env::args().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "--list" => list_only = true,
            "--bench" | "--test" | "--nocapture" | "--show-output" | "--quiet" | "-q" => {}
            "--exact" => exact = true,
            "--format" | "--color" => {
                skip_next = true;
            }
            s if s.starts_with("--format=") || s.starts_with("--color=") => {}
            s if s.starts_with('-') => {}
            s => filters.push(s.to_string()),
        }
    }

    if list_only {
        for name in BENCH_NAMES {
            println!("{name}: bench");
        }
        return;
    }

    for &name in BENCH_NAMES {
        let matched = if filters.is_empty() {
            true
        } else if exact {
            filters.iter().any(|f| f == name)
        } else {
            filters.iter().any(|f| name.contains(f.as_str()))
        };
        if matched {
            run_bench(name);
        }
    }
}
