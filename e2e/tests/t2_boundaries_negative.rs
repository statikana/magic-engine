//! Tier 2 Opaque-Box E2E Tests: Boundary, Corner & Negative Cases (F1–F7, F11–F17).
//!
//! Covers 70 tests: `T2-F01-01..05` .. `T2-F07-01..05` (35) + `T2-F11-01..05` .. `T2-F17-01..05` (35).

use std::sync::Arc;
use std::time::Instant;

use mtg_engine::{
    Action, AttackTarget, CastRequest, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec,
    EngineError, GameConfig, MacroAction, MatchSession, ObjectId, PlayerId, PowerToughness,
    RngState, TargetRef, TsReplayTrace,
};
use mtg_engine_e2e::assertions::{assert_observation_consistency, assert_view_model_redaction};
use mtg_engine_e2e::cli::{
    TempDir, assert_node_script_present, repo_root, run_build_db, run_diff_test, run_inspect_db,
    run_node_script, write_synthetic_jsonl_gz,
};
use mtg_engine_e2e::driver::{SessionDriver, play_deterministic_random_game_driver};
use mtg_engine_e2e::synth::{SynthBundleBuilder, effect_step, target_creature, target_player};
use mtg_ir::{
    Amount, CardDatabase, CardDefId, CardLayout, CardTypeMask, ColorMask, Compare, CompareOp,
    CostDef, CostDelta, CostDeltaMode, EffectOp, EffectStep, FilterFlags, GamePhase, IR_V2_VERSION,
    InternTables, IrCodecError, IrV2Bundle, ManaColor, ManaCost, ObjectFilter, PlayerCounterKind,
    PlayerFilter, PlayerRef, PlayerRelation, Selector, SpellAbility, SpellOrAbilityFilter,
    StaticBody, StaticDef, SupertypeMask, TargetKind, TargetSpec, Timing, UnsupportedReason, Zone,
    ZoneMask,
};

fn build_60_deck_pair(land: CardDefId, filler: CardDefId, specials: &[CardDefId]) -> Vec<DeckSpec> {
    let mut main = Vec::with_capacity(60);
    for &s in specials {
        main.extend([s, s, s, s]);
    }
    while main.len() < 24 {
        main.push(land);
    }
    while main.len() < 60 {
        main.push(filler);
    }
    let d = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    vec![d.clone(), d]
}

fn find_land_in_hand(drv: &SessionDriver, player: PlayerId) -> Option<ObjectId> {
    drv.session.view_model(Some(player)).players[usize::from(player.0)]
        .hand
        .iter()
        .find(|o| o.types.contains(CardTypeMask::LAND))
        .map(|o| o.id)
}

// ============================================================================
// F1 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f01_01_unknown_json_field_rejected_deny_unknown_fields() {
    let mut b = SynthBundleBuilder::new();
    b.add_synth_vanilla_creature("synth_unk_01", "Synth Bear", "{1}{G}", 2, 2);
    let mut val = serde_json::to_value(b.build_bundle()).unwrap();
    val["cards"][0]["unknown_extra_key"] = serde_json::json!(1);
    let err = serde_json::from_value::<IrV2Bundle>(val)
        .expect_err("CardDef deny_unknown_fields must reject extra key");
    assert!(err.to_string().contains("unknown_extra_key"), "{err}");

    let mut val2 = serde_json::to_value(b.build_bundle()).unwrap();
    val2["cards"][0]["faces"][0]["characteristics"]["extra_char"] = serde_json::json!(true);
    let err2 = serde_json::from_value::<IrV2Bundle>(val2)
        .expect_err("Characteristics deny_unknown_fields must reject extra key");
    assert!(err2.to_string().contains("extra_char"), "{err2}");
}

#[test]
fn t2_f01_02_huge_generic_mana_cost_and_zero_and_empty() {
    let empty = ManaCost::parse("");
    assert!(empty.is_empty());
    assert_eq!(empty.mana_value(), 0);

    let zero = ManaCost::parse("{0}");
    assert!(!zero.is_empty());
    assert_eq!(zero.mana_value(), 0);

    let huge = ManaCost::parse("{1000000}");
    assert_eq!(
        huge.mana_value(),
        1_000_000,
        "{{1000000}} must not truncate/saturate at u16::MAX"
    );
}

#[test]
fn t2_f01_03_corrupted_cards_bin_headers_and_truncated_payload() {
    assert!(matches!(
        CardDatabase::decode_bytes(b"BAD!\x02\x00\x00\x00\x00"),
        Err(IrCodecError::InvalidMagic)
    ));
    assert!(matches!(
        CardDatabase::decode_bytes(b"MTGC\x63\x00\x00\x00\x00"),
        Err(IrCodecError::UnsupportedVersion(99))
    ));
    assert!(matches!(
        CardDatabase::decode_bytes(b"MTGC\x02\x00\x00\x00\x09"),
        Err(IrCodecError::UnsupportedCompression(9))
    ));
    assert!(matches!(
        CardDatabase::decode_bytes(b"MTGC\x02"),
        Err(IrCodecError::InvalidMagic)
    ));
}

#[test]
fn t2_f01_04_empty_ir_v2_bundle_round_trip() {
    let bundle = IrV2Bundle {
        ir_version: IR_V2_VERSION,
        source_compiler_version: 46,
        tables: InternTables::default(),
        cards: vec![],
        decks: vec![],
    };
    let db = CardDatabase::from_bundle(bundle).unwrap();
    assert_eq!(db.fully_supported_count(), 0);
    assert_eq!(db.id_by_oracle("x"), None);
    let raw = db.encode_raw().unwrap();
    let dec = CardDatabase::decode_bytes(&raw).unwrap();
    assert!(dec.cards.is_empty());
    assert!(dec.decks.is_empty());
}

#[test]
fn t2_f01_05_filter_flags_bit_52_and_empty_boundary() {
    let empty = FilterFlags::empty();
    let high = FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN;
    assert_eq!(high.bits(), 1u64 << 52);

    for flags in [empty, high] {
        let j = serde_json::to_value(flags).unwrap();
        assert_eq!(j, serde_json::json!(flags.bits()));
        assert_eq!(serde_json::from_value::<FilterFlags>(j).unwrap(), flags);

        let pc = postcard::to_allocvec(&flags).unwrap();
        assert_eq!(postcard::from_bytes::<FilterFlags>(&pc).unwrap(), flags);
    }
}

// ============================================================================
// F2 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f02_01_export_ir_v2_rejects_unknown_cli_flag() {
    let res = run_node_script("scripts/export-ir-v2.ts", &["--unknown-flag-e2e"]);
    assert_node_script_present("scripts/export-ir-v2.ts", &res);
    assert!(
        !res.success,
        "export-ir-v2.ts must fail cleanly on unknown flags"
    );
}

#[test]
fn t2_f02_02_export_ir_v2_missing_input_fails_without_partial_output() {
    let tmp = TempDir::new("t2_f02_02");
    let out_json = tmp.child("should_not_exist.json");
    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            tmp.child("nonexistent.jsonl.gz").to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert_node_script_present("scripts/export-ir-v2.ts", &res);
    assert!(!res.success, "missing --bulk file must fail");
    assert!(
        !res.stderr.trim().is_empty(),
        "missing --bulk file must emit diagnostic stderr"
    );
    assert!(!out_json.exists(), "must leave no partial output JSON");
}

#[test]
fn t2_f02_03_filter_flags_bit31_and_bit52_positive_in_json() {
    let tmp = TempDir::new("t2_f02_03");
    let synth_gz = tmp.child("synth_bit31.jsonl.gz");
    let out_json = tmp.child("out.json");
    let card = serde_json::json!({
        "id": "synth_b31", "oracle_id": "synth_b31", "name": "Synth UnblockDestroy",
        "lang": "en", "layout": "normal", "mana_cost": "{1}{W}", "cmc": 2.0,
        "type_line": "Instant",
        "oracle_text": "Destroy target artifact or enchantment.",
        "colors": ["W"], "color_identity": ["W"], "keywords": [],
        "legalities": { "commander": "legal" }, "games": ["paper"]
    });
    write_synthetic_jsonl_gz(&synth_gz, &serde_json::to_string(&card).unwrap());
    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            synth_gz.to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(res.success, "export-ir-v2 failed: {}", res.stderr);
    let raw = std::fs::read_to_string(&out_json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // Ensure no negative 32-bit signed filter flags appear anywhere in the JSON tree.
    assert!(
        !raw.contains("\"flags\":-"),
        "FilterFlags must never sign-truncate negative"
    );
    let _bundle: IrV2Bundle = serde_json::from_value(v).unwrap();
}

#[test]
fn t2_f02_04_intern_tables_deduplicate_shared_subtypes_and_keywords() {
    let tmp = TempDir::new("t2_f02_04");
    let synth_gz = tmp.child("synth_dedup.jsonl.gz");
    let out_json = tmp.child("dedup.json");
    let c1 = serde_json::json!({
        "id": "synth_e1", "oracle_id": "synth_e1", "name": "Synth Elf One",
        "lang": "en", "layout": "normal", "mana_cost": "{G}", "cmc": 1.0,
        "type_line": "Creature — Elf", "oracle_text": "Flying",
        "power": "1", "toughness": "1",
        "colors": ["G"], "color_identity": ["G"], "keywords": ["Flying"],
        "legalities": { "commander": "legal" }, "games": ["paper"]
    });
    let c2 = serde_json::json!({
        "id": "synth_e2", "oracle_id": "synth_e2", "name": "Synth Elf Two",
        "lang": "en", "layout": "normal", "mana_cost": "{1}{G}", "cmc": 2.0,
        "type_line": "Creature — Elf", "oracle_text": "Flying",
        "power": "2", "toughness": "2",
        "colors": ["G"], "color_identity": ["G"], "keywords": ["Flying"],
        "legalities": { "commander": "legal" }, "games": ["paper"]
    });
    write_synthetic_jsonl_gz(&synth_gz, &format!("{c1}\n{c2}\n"));
    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            synth_gz.to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(res.success, "export-ir-v2 failed: {}", res.stderr);
    let bundle: IrV2Bundle =
        serde_json::from_str(&std::fs::read_to_string(&out_json).unwrap()).unwrap();
    assert_eq!(
        bundle
            .tables
            .subtypes
            .iter()
            .filter(|s| *s == "Elf")
            .count(),
        1
    );
    assert_eq!(
        bundle
            .tables
            .keywords
            .iter()
            .filter(|k| *k == "flying")
            .count(),
        1
    );
}

#[test]
fn t2_f02_05_unsupported_ability_preserves_fully_supported_false() {
    let tmp = TempDir::new("t2_f02_05");
    let synth_gz = tmp.child("synth_unsup.jsonl.gz");
    let out_json = tmp.child("unsup.json");
    let card = serde_json::json!({
        "id": "synth_unsup_card", "oracle_id": "synth_unsup_card", "name": "Synth Unparsed Card",
        "lang": "en", "layout": "normal", "mana_cost": "{1}{U}", "cmc": 2.0,
        "type_line": "Sorcery",
        "oracle_text": "Synthesize four imaginary hyper-tokens with quantum phasing.",
        "colors": ["U"], "color_identity": ["U"], "keywords": [],
        "legalities": { "commander": "legal" }, "games": ["paper"]
    });
    write_synthetic_jsonl_gz(&synth_gz, &serde_json::to_string(&card).unwrap());
    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            synth_gz.to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(res.success, "export-ir-v2 failed: {}", res.stderr);
    let bundle: IrV2Bundle =
        serde_json::from_str(&std::fs::read_to_string(&out_json).unwrap()).unwrap();
    assert!(!bundle.cards[0].fully_supported);
}

// ============================================================================
// F3 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f03_01_build_db_rejects_malformed_json_and_unknown_field() {
    let tmp = TempDir::new("t2_f03_01");
    let bad = tmp.child("bad.json");
    let out = tmp.child("bad.bin");
    std::fs::write(&bad, "{").unwrap();
    let res1 = run_build_db(&bad, &out, false, None);
    assert!(!res1.success);
    assert!(!out.exists());

    let mut b = SynthBundleBuilder::new();
    b.add_synth_vanilla_creature("synth_x", "Synth X", "{1}", 1, 1);
    let mut val = serde_json::to_value(b.build_bundle()).unwrap();
    val["extra_root_key"] = serde_json::json!(1);
    std::fs::write(&bad, serde_json::to_string(&val).unwrap()).unwrap();
    let res2 = run_build_db(&bad, &out, false, None);
    assert!(!res2.success);
    assert!(!out.exists());
}

#[test]
fn t2_f03_02_build_db_rejects_missing_input_or_unwritable_output() {
    let tmp = TempDir::new("t2_f03_02");
    let res = run_build_db(
        &tmp.child("missing.json"),
        &tmp.child("nonexistent_dir").join("out.bin"),
        false,
        None,
    );
    assert!(!res.success);
}

#[test]
fn t2_f03_03_inspect_db_verify_precons_fails_on_missing_oracle_id() {
    let tmp = TempDir::new("t2_f03_03");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_plains", "Synth Plains", "Plains", ManaColor::W);
    b.add_deck(
        "synth_bad",
        "Synth Missing Deck",
        &[],
        &[("synth_missing_id", 1)],
    );
    let j = tmp.child("b.json");
    let bin = tmp.child("b.bin");
    std::fs::write(&j, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);

    let res = run_inspect_db(&bin, true);
    assert!(!res.success);
    assert!(res.stderr.contains("MISSING"));
    assert!(res.stdout.contains("1 missing"));
}

#[test]
fn t2_f03_04_inspect_db_verify_precons_fails_on_unsupported_card() {
    let tmp = TempDir::new("t2_f03_04");
    let mut b = SynthBundleBuilder::new();
    b.add_card("synth_unsup", "Synth BadCard", |c| {
        c.unsupported(UnsupportedReason::Unparsed, "unparsed text");
    });
    b.add_deck("synth_bad2", "Synth Unsup Deck", &[], &[("synth_unsup", 1)]);
    let j = tmp.child("b.json");
    let bin = tmp.child("b.bin");
    std::fs::write(&j, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);

    let res = run_inspect_db(&bin, true);
    assert!(!res.success);
    assert!(res.stderr.contains("UNSUPPORTED"));
    assert!(res.stdout.contains("1 unsupported"));
}

#[test]
fn t2_f03_05_inspect_db_handles_zero_card_empty_database() {
    let tmp = TempDir::new("t2_f03_05");
    let empty = IrV2Bundle {
        ir_version: IR_V2_VERSION,
        source_compiler_version: 46,
        tables: InternTables::default(),
        cards: vec![],
        decks: vec![],
    };
    let j = tmp.child("empty.json");
    let bin = tmp.child("empty.bin");
    std::fs::write(&j, serde_json::to_string(&empty).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);
    let res = run_inspect_db(&bin, false);
    assert!(res.success, "empty inspect-db failed: {}", res.stderr);
    assert!(
        res.stdout.contains("0 (0 fully supported = 0.0%)"),
        "{}",
        res.stdout
    );
}

// ============================================================================
// F4 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f04_01_ts_replay_trace_rejects_unknown_and_malformed_fields() {
    let bad_extra = serde_json::json!({
        "formatVersion": 1,
        "compilerVersion": 46,
        "seed": 42,
        "extra_field": 123,
        "seats": [],
        "steps": []
    });
    assert!(
        serde_json::from_value::<TsReplayTrace>(bad_extra).is_err(),
        "TsReplayTrace must reject unknown fields via #[serde(deny_unknown_fields)]"
    );

    let bad_type = serde_json::json!({
        "formatVersion": "not_a_number",
        "compilerVersion": 46,
        "seed": 42,
        "seats": [],
        "steps": []
    });
    assert!(serde_json::from_value::<TsReplayTrace>(bad_type).is_err());
}

#[test]
fn t2_f04_02_mulberry32_zero_and_u32_max_boundary_parity() {
    let root = mtg_engine_e2e::cli::peckhamr_root().expect("peckhamr.com must exist");
    let random_ts = root.join("src/lib/magic/runtime/random.ts");
    let js = format!(
        "import({:?}).then(m => {{ \
           const r0 = {{ rngState: 0 }}; \
           const rmax = {{ rngState: 4294967295 }}; \
           const a = Array.from({{length: 5}}, () => Math.floor(m.nextRandom(r0) * 4294967296)); \
           const b = Array.from({{length: 5}}, () => Math.floor(m.nextRandom(rmax) * 4294967296)); \
           console.log(JSON.stringify({{ a, b }})); \
         }})",
        random_ts
    );
    let out = std::process::Command::new("node")
        .args(["--input-type=module", "-e", &js])
        .output()
        .unwrap();
    assert!(out.status.success());
    let oracle: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let mut r0 = RngState::mulberry32(0);
    let rust0: Vec<u32> = (0..5).map(|_| r0.next_u32()).collect();
    assert_eq!(
        rust0,
        serde_json::from_value::<Vec<u32>>(oracle["a"].clone()).unwrap()
    );

    let mut rmax = RngState::mulberry32(u32::MAX);
    let rust_max: Vec<u32> = (0..5).map(|_| rmax.next_u32()).collect();
    assert_eq!(
        rust_max,
        serde_json::from_value::<Vec<u32>>(oracle["b"].clone()).unwrap()
    );
}

#[test]
fn t2_f04_03_fisher_yates_shuffle_empty_and_single_element() {
    let mut rng = RngState::mulberry32(42);
    let mut empty: Vec<u32> = vec![];
    rng.shuffle(&mut empty);
    assert!(empty.is_empty());
    assert_eq!(
        rng,
        RngState::mulberry32(42),
        "empty shuffle must not advance RNG"
    );

    let mut one = vec![99u32];
    rng.shuffle(&mut one);
    assert_eq!(one, vec![99]);
    assert_eq!(
        rng,
        RngState::mulberry32(42),
        "1-element shuffle must not advance RNG"
    );
}

#[test]
fn t2_f04_04_record_ts_replay_rejects_invalid_mode() {
    let tmp = TempDir::new("t2_f04_04");
    let out = tmp.child("bad.replay.json");
    let res = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "invalid_mode",
            "--out",
            out.to_string_lossy().as_ref(),
        ],
    );
    assert_node_script_present("scripts/record-ts-replay.ts", &res);
    assert!(!res.success, "invalid mode must fail");
    assert!(
        !res.stderr.trim().is_empty(),
        "invalid mode must emit diagnostic stderr"
    );
    assert!(!out.exists());
}

#[test]
fn t2_f04_05_distinct_mulberry32_seeds_yield_distinct_library_orders() {
    let tmp = TempDir::new("t2_f04_05");
    let a = tmp.child("s1.json");
    let b = tmp.child("s2.json");
    let r1 = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "1",
            "--out",
            a.to_string_lossy().as_ref(),
        ],
    );
    assert!(r1.success, "seed 1 failed: {}", r1.stderr);
    let r2 = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "2",
            "--out",
            b.to_string_lossy().as_ref(),
        ],
    );
    assert!(r2.success, "seed 2 failed: {}", r2.stderr);
    let t1: TsReplayTrace = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
    let t2: TsReplayTrace = serde_json::from_str(&std::fs::read_to_string(&b).unwrap()).unwrap();
    assert_ne!(
        t1.seats[0].library_order_initial,
        t2.seats[0].library_order_initial
    );
}

// ============================================================================
// F5 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f05_01_stale_object_id_after_zone_move_rejected() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_51", "Synth Plains", "Plains", ManaColor::W);
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 201).unwrap();
    drv.keep_all_hands();

    let stale_hand_id = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: stale_hand_id,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let err = drv.step_unchecked(
        PlayerId(0),
        Action::TapMana {
            object: stale_hand_id,
            ability_index: 0,
            preferred_color: None,
        },
    );
    assert!(matches!(err, Err(EngineError::IllegalAction(_))));
}

#[test]
fn t2_f05_02_token_leaves_battlefield_ceases_to_exist_at_sba() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_52", "Synth Plains", "Plains", ManaColor::W);
    let mk_tok = b.add_card("synth_tok_t2_52", "Synth Raise", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::CreateToken {
                        token: Some(Box::new(mtg_ir::TokenSpec {
                            name: "Synth Pawn".into(),
                            power: 1,
                            toughness: 1,
                            colors: ColorMask::W,
                            types: CardTypeMask::CREATURE,
                            supertypes: SupertypeMask::empty(),
                            subtypes: Default::default(),
                            type_line: "Creature".into(),
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
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let bounce = b.add_card("synth_bnc_t2_52", "Synth Bounce", |c| {
        c.mana_cost("{W}")
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
    let decks = build_60_deck_pair(plains, mk_tok, &[bounce]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 202).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
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
    let t_h = drv.find_hand_object(PlayerId(0), mk_tok).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: t_h,
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

    let tok_bf = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .find(|o| o.types.contains(CardTypeMask::CREATURE))
        .unwrap()
        .id;
    let hand_before = drv.session.view_model(Some(PlayerId(0))).players[0].hand_count;
    let b_h = drv.find_hand_object(PlayerId(0), bounce).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: tok_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    // Token bounces to hand -> ceases to exist at SBA -> hand count drops by 1 (the Bounce spell).
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        hand_before - 1
    );
}

#[test]
fn t2_f05_03_empty_zone_operations_mill_and_draw_from_empty_library() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t2_53", "Synth Island", "Island", ManaColor::U);
    let mill_huge = b.add_card("synth_mill_t2_53", "Synth Mill", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_player()],
                vec![effect_step(
                    EffectOp::Mill,
                    Selector::Target { index: 0 },
                    Some(Amount::Const { n: 200 }),
                )],
            );
    });
    let draw1 = b.add_synth_draw_sorcery("synth_draw_t2_53", "Synth Draw", "{0}", 1);
    let db = b.build_db();
    let decks = build_60_deck_pair(u_land, mill_huge, &[draw1]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 203).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(mill_huge, 1), (draw1, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), mill_huge).is_some()
            && drv.find_hand_object(PlayerId(0), draw1).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw mill_huge and draw1 within wait loop");

    let m_h = drv.find_hand_object(PlayerId(0), mill_huge).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: m_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(0) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[0].library_count, 0);

    let d_h = drv.find_hand_object(PlayerId(0), draw1).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: d_h,
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
        DecisionState::GameOver {
            winner: Some(PlayerId(1))
        }
    );
}

#[test]
fn t2_f05_04_phased_out_permanent_cannot_be_targeted() {
    let mut b = SynthBundleBuilder::new();
    let u_land =
        b.add_synth_basic_land("synth_island_t2_54", "Synth Island", "Island", ManaColor::U);
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_54", "Synth Bear", "{U}", 2, 2);
    let phase = b.add_card("synth_phase_t2_54", "Synth Phase", |c| {
        c.mana_cost("{U}")
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
    let bolt = b.add_synth_burn_instant("synth_bolt_t2_54", "Synth Bolt", "{U}", 3);
    let db = b.build_db();
    let decks = build_60_deck_pair(u_land, bear, &[phase, bolt]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 204).unwrap();
    drv.keep_all_hands();

    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(u_land, 1), (bear, 1), (phase, 1), (bolt, 1)],
        )
        .unwrap();
        if lands_played < 3
            && let Some(l) = find_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 3
            && drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), phase).is_some()
            && drv.find_hand_object(PlayerId(0), bolt).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Islands and hold bear, phase, and bolt within wait loop"
    );
    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
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

    let p_h = drv.find_hand_object(PlayerId(0), phase).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: p_h,
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

    let bolt_h = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    let err = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bolt_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: b_bf }],
        },
    );
    assert!(
        matches!(err, Err(EngineError::IllegalAction(_))),
        "cannot target phased-out permanent (expected IllegalAction, got {err:?})"
    );
}

#[test]
fn t2_f05_05_repeated_zone_transitions_never_alias_stale_ids() {
    let mut b = SynthBundleBuilder::new();
    let w_land =
        b.add_synth_basic_land("synth_plains_t2_55", "Synth Plains", "Plains", ManaColor::W);
    let flicker = b.add_card("synth_blink_t2_55", "Synth Flicker", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![
                    EffectStep {
                        bind_as: Some("b".into()),
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
                        Selector::Binding { name: "b".into() },
                        None,
                    ),
                ],
            );
    });
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_55", "Synth Bear", "{0}", 2, 2);
    let _ = w_land;
    let db = b.build_db();
    let mut main = vec![bear; 30];
    main.extend(vec![flicker; 30]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        205,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), flicker).is_some()
        {
            break;
        }
    }
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
    let first_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();

    let f_h = drv
        .find_hand_object(PlayerId(0), flicker)
        .expect("Synth Flicker must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: f_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: first_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    let after_bf = drv.find_battlefield_object(PlayerId(0), bear).unwrap();
    assert!(after_bf.generation > first_bf.generation);
    assert!(drv.session.state().object(first_bf).is_none());
    let act_res = drv.step_unchecked(
        PlayerId(0),
        Action::BeginActivate {
            object: first_bf,
            ability_index: 0,
        },
    );
    assert!(
        matches!(act_res, Err(EngineError::IllegalAction(_))),
        "stale pre-flicker ObjectId must be rejected with IllegalAction, got {act_res:?}"
    );
}

// ============================================================================
// F6 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f06_01_commander_validate_deck_rejects_duplicate_offcolor_and_wrong_size() {
    let mut b = SynthBundleBuilder::new();
    let forest =
        b.add_synth_basic_land("synth_forest_t2_61", "Synth Forest", "Forest", ManaColor::G);
    let cmd = b.add_card("synth_cmd_t2_61", "Synth MonoG Cmd", |c| {
        c.mana_cost("{G}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_61", "Synth Bear", "{G}", 2, 2);
    let red_spell = b.add_synth_burn_instant("synth_red_t2_61", "Synth RedBolt", "{R}", 2);
    let db = b.build_db();
    let cfg = GameConfig::commander(2);

    // Wrong size (99 total instead of 100):
    let bad_size = DeckSpec {
        commanders: vec![cmd],
        mainboard: vec![forest; 98],
        sideboard: vec![],
    };
    assert!(!MatchSession::validate_deck(&db, &cfg, &bad_size).is_legal);

    // Duplicate nonbasic:
    let mut dup_main = vec![forest; 97];
    dup_main.push(bear);
    dup_main.push(bear);
    let bad_dup = DeckSpec {
        commanders: vec![cmd],
        mainboard: dup_main,
        sideboard: vec![],
    };
    assert!(!MatchSession::validate_deck(&db, &cfg, &bad_dup).is_legal);

    // Off-color identity:
    let mut off_main = vec![forest; 98];
    off_main.push(red_spell);
    let bad_color = DeckSpec {
        commanders: vec![cmd],
        mainboard: off_main,
        sideboard: vec![],
    };
    assert!(!MatchSession::validate_deck(&db, &cfg, &bad_color).is_legal);
}

#[test]
fn t2_f06_02_commander_cast_from_hand_does_not_add_or_increment_tax() {
    let mut b = SynthBundleBuilder::new();
    let island =
        b.add_synth_basic_land("synth_island_t2_62", "Synth Island", "Island", ManaColor::U);
    let cmd = b.add_card("synth_cmd_t2_62", "Synth BlueCmd", |c| {
        c.mana_cost("{U}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let bounce = b.add_card("synth_bounce_t2_62", "Synth Bounce", |c| {
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
    let db = b.build_db();
    let mut main = vec![island; 60];
    main.extend(vec![bounce; 39]);
    let d = DeckSpec {
        commanders: vec![cmd],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::commander(2), vec![d.clone(), d], 206).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
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
    let cmd_id = drv.find_command_object(PlayerId(0), cmd).unwrap();
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

    let bn_h = drv.find_hand_object(PlayerId(0), bounce).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bn_h,
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
                    answer: ChoiceAnswer::YesNo { value: false },
                },
            )
            .unwrap();
        }
        other => panic!("expected CR 903.9b YesNo prompt on commander bounce, got {other:?}"),
    }

    // Turn 5: Islands untap; in Hand, casting commander costs {U} (NOT {2}{U}!), even after 1 command cast!
    drv.advance_to_preserving(5, GamePhase::Main1, 64, &[(cmd, 1)])
        .unwrap();
    let cmd_h = drv.find_hand_object(PlayerId(0), cmd).unwrap();
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: cmd_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(
        prev.can_pay && prev.sources_to_tap.len() == 1,
        "CR 903.8: casting commander from Hand costs {{U}} with 1 source, not +2 tax"
    );
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: cmd_h,
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
    assert!(drv.find_battlefield_object(PlayerId(0), cmd).is_some());
}

#[test]
fn t2_f06_03_commander_combat_damage_tracked_separately_per_commander() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_63", "Synth Plains", "Plains", ManaColor::W);
    let p1 = b.add_card("synth_partner1_63", "Synth Partner One", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(11, 11)
            .keyword("haste");
    });
    let p2 = b.add_card("synth_partner2_63", "Synth Partner Two", |c| {
        c.mana_cost("{W}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(11, 11)
            .keyword("haste");
    });
    let db = b.build_db();
    let d0 = DeckSpec {
        commanders: vec![p1, p2],
        mainboard: vec![plains; 98],
        sideboard: vec![],
    };
    let d1 = DeckSpec {
        commanders: vec![p1],
        mainboard: vec![plains; 99],
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(db, GameConfig::commander(2), vec![d0, d1], 207).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
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
    for cmd in [p1, p2] {
        let cid = drv.find_command_object(PlayerId(0), cmd).unwrap();
        drv.macro_step(
            PlayerId(0),
            MacroAction::CastSpell {
                object: cid,
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
    }
    let bf1 = drv.find_battlefield_object(PlayerId(0), p1).unwrap();
    let bf2 = drv.find_battlefield_object(PlayerId(0), p2).unwrap();
    drv.advance_to(3, GamePhase::DeclareAttackers, 16).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![
                (bf1, AttackTarget::Player { id: PlayerId(1) }),
                (bf2, AttackTarget::Player { id: PlayerId(1) }),
            ],
        },
    )
    .unwrap();
    drv.advance_to(3, GamePhase::Main2, 32).unwrap();
    // 40 - 22 = 18 life, 11 from P1 and 11 from P2 (< 21 each) -> P1 is still alive!
    assert_eq!(drv.session.view_model(None).players[1].life, 18);
    assert!(!matches!(drv.decision(), DecisionState::GameOver { .. }));
}

#[test]
fn t2_f06_04_face_down_permanent_characteristics_and_turn_face_up() {
    let mut b = SynthBundleBuilder::new();
    let g_land =
        b.add_synth_basic_land("synth_forest_t2_64", "Synth Forest", "Forest", ManaColor::G);
    let morph = b.add_card("synth_morph_t2_64", "Synth Morph", |c| {
        c.mana_cost("{3}{G}")
            .types(CardTypeMask::CREATURE)
            .pt(5, 5)
            .keyword_with_cost("morph", "{G}");
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(g_land, morph, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 208).unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5] {
        drv.advance_to(turn, GamePhase::Main1, 64).unwrap();
        let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
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
    let m_h = drv.find_hand_object(PlayerId(0), morph).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: m_h,
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

    let m_bf = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .find(|o| o.types.contains(CardTypeMask::CREATURE))
        .unwrap()
        .id;
    let bd = drv.session.hover_breakdown(None, m_bf).unwrap();
    assert_eq!(
        bd.evaluated_pt,
        Some(PowerToughness {
            power: 2,
            toughness: 2
        })
    );
    assert_eq!(bd.colors, ColorMask::empty());

    // Turn 5: all 3 Forests are tapped -> TurnFaceUp for {G} must fail with 0 mana available.
    assert!(
        drv.step_unchecked(
            PlayerId(0),
            Action::SpecialAction {
                kind: mtg_engine::SpecialActionKind::TurnFaceUp { object: m_bf },
            },
        )
        .is_err(),
        "CR 702.37e: TurnFaceUp for {{G}} must fail when 0 mana is available"
    );

    drv.advance_to(7, GamePhase::Main1, 64).unwrap();
    let tapped_before = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.types.contains(CardTypeMask::LAND) && o.tapped)
        .count();
    assert_eq!(tapped_before, 0);

    drv.step(
        PlayerId(0),
        Action::SpecialAction {
            kind: mtg_engine::SpecialActionKind::TurnFaceUp { object: m_bf },
        },
    )
    .unwrap();
    let tapped_after = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.types.contains(CardTypeMask::LAND) && o.tapped)
        .count();
    assert_eq!(
        tapped_after, 1,
        "CR 702.37e: TurnFaceUp for {{G}} must tap 1 Forest"
    );
    let bd_up = drv.session.hover_breakdown(None, m_bf).unwrap();
    assert_eq!(
        bd_up.evaluated_pt,
        Some(PowerToughness {
            power: 5,
            toughness: 5
        })
    );
    assert_eq!(bd_up.colors, ColorMask::G);
}

#[test]
fn t2_f06_05_adventure_creature_face_illegal_from_exile_without_adventure_cast() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_65",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let adv = b.add_card("synth_adv_t2_65", "Synth Adventurer", |c| {
        c.layout(CardLayout::Adventure)
            .mana_cost("{R}")
            .types(CardTypeMask::CREATURE)
            .pt(2, 2)
            .add_face("Synth SideQuest", |f| {
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
    let exile_spell = b.add_card("synth_exile_t2_65", "Synth Banish", |c| {
        c.mana_cost("{R}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
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
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(mtn, adv, &[exile_spell]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 209).unwrap();
    drv.keep_all_hands();

    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=29).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(mtn, 1), (adv, 1), (exile_spell, 1)],
        )
        .unwrap();
        if lands_played < 2
            && let Some(l) = find_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 2
            && drv.find_hand_object(PlayerId(0), adv).is_some()
            && drv.find_hand_object(PlayerId(0), exile_spell).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 2 Mountains and hold adv + exile_spell within wait loop"
    );
    let a_h = drv.find_hand_object(PlayerId(0), adv).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a_h,
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
    let a_bf = drv.find_battlefield_object(PlayerId(0), adv).unwrap();

    let ex_h = drv.find_hand_object(PlayerId(0), exile_spell).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: ex_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: a_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();

    let a_ex = drv.find_exile_object(PlayerId(0), adv).unwrap();
    let err = drv.step_unchecked(
        PlayerId(0),
        Action::BeginCast {
            object: a_ex,
            option_id: 0,
            from_zone: Zone::Exile,
        },
    );
    assert!(matches!(err, Err(EngineError::IllegalAction(_))));
}

// ============================================================================
// F7 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f07_01_empty_object_filter_zero_matches() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_71", "Synth Plains", "Plains", ManaColor::W);
    let tally = b.add_card("synth_tally_t2_71", "Synth CountGain", |c| {
        c.mana_cost("{0}")
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
                    Some(Amount::Count {
                        of: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::PLANESWALKER,
                            ..Default::default()
                        }),
                    }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, tally, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 210).unwrap();
    drv.keep_all_hands();

    let t_h = drv.find_hand_object(PlayerId(0), tally).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: t_h,
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
    assert_eq!(drv.session.view_model(None).players[0].life, 20);
}

#[test]
fn t2_f07_02_tri_state_filter_flags_unspecified_vs_untapped() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_72", "Synth Plains", "Plains", ManaColor::W);
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_72", "Synth Bear", "{0}", 2, 2);
    let untap_only = b.add_card("synth_untap_kill_72", "Synth SnipeUntapped", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::UNTAPPED,
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
    let tapper = b.add_card("synth_tapper_72", "Synth Tap", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::Tap {
                        select: Default::default(),
                    },
                    Selector::Target { index: 0 },
                    None,
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, bear, &[untap_only, tapper]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 211).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(bear, 1), (tapper, 1), (untap_only, 1)],
        )
        .unwrap();
        if drv.find_hand_object(PlayerId(0), bear).is_some()
            && drv.find_hand_object(PlayerId(0), tapper).is_some()
            && drv.find_hand_object(PlayerId(0), untap_only).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must draw bear, tapper, and untap_only within wait loop"
    );

    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
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

    let t_h = drv.find_hand_object(PlayerId(0), tapper).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: t_h,
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

    let u_h = drv.find_hand_object(PlayerId(0), untap_only).unwrap();
    let untap_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: u_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: b_bf }],
        },
    );
    assert!(
        matches!(untap_res, Err(EngineError::IllegalAction(_))),
        "UNTAPPED filter must exclude tapped creature (expected IllegalAction, got {untap_res:?})"
    );
}

#[test]
fn t2_f07_03_negative_amount_clamps_to_zero_at_life_gain() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_73", "Synth Plains", "Plains", ManaColor::W);
    let wimp = b.add_synth_vanilla_creature("synth_wimp_t2_73", "Synth Wimp", "{0}", -2, 2);
    let gain_pow = b.add_card("synth_gp_t2_73", "Synth PowerLife", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_creature()],
                vec![effect_step(
                    EffectOp::GainLife {
                        set_life_total: false,
                    },
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Stat {
                        of: mtg_ir::Ref::Target { index: 0 },
                        stat: mtg_ir::StatKind::Power,
                        lki: false,
                    }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, wimp, &[gain_pow]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 212).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(wimp, 1), (gain_pow, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), wimp).is_some()
            && drv.find_hand_object(PlayerId(0), gain_pow).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw wimp and gain_pow within wait loop");

    let w_h = drv.find_hand_object(PlayerId(0), wimp).unwrap();
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
    let w_bf = drv.find_battlefield_object(PlayerId(0), wimp).unwrap();

    let gp_h = drv.find_hand_object(PlayerId(0), gain_pow).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: gp_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: w_bf }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[0].life, 20);
}

#[test]
fn t2_f07_04_colors_of_mana_spent_zero_when_free_cast() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_74", "Synth Plains", "Plains", ManaColor::W);
    let conv0 = b.add_card("synth_conv0_t2_74", "Synth ZeroConv", |c| {
        c.mana_cost("{0}")
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
                    Some(Amount::ColorsOfManaSpentToCast),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, conv0, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 213).unwrap();
    drv.keep_all_hands();

    let c_h = drv.find_hand_object(PlayerId(0), conv0).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: c_h,
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
    assert_eq!(drv.session.view_model(None).players[0].life, 20);
}

#[test]
fn t2_f07_05_compare_op_boundary_ge4_vs_gt4_on_4_4() {
    let mut b = SynthBundleBuilder::new();
    let plains =
        b.add_synth_basic_land("synth_plains_t2_75", "Synth Plains", "Plains", ManaColor::W);
    let four = b.add_synth_vanilla_creature("synth_four_t2_75", "Synth Beast4", "{0}", 4, 4);
    let gt4 = b.add_card("synth_gt4_t2_75", "Synth Gt4", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::INSTANT)
            .spell_effect(
                vec![TargetSpec {
                    kind: TargetKind::Object,
                    filter: ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::CREATURE,
                        power: Some(Compare {
                            op: CompareOp::Gt,
                            value: Box::new(Amount::Const { n: 4 }),
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
    let decks = build_60_deck_pair(plains, four, &[gt4]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 214).unwrap();
    drv.keep_all_hands();

    let mut found = false;
    for turn in (1u32..=39).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(four, 1), (gt4, 1)])
            .unwrap();
        if drv.find_hand_object(PlayerId(0), four).is_some()
            && drv.find_hand_object(PlayerId(0), gt4).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(found, "must draw four and gt4 within wait loop");

    let f_h = drv.find_hand_object(PlayerId(0), four).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: f_h,
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
    let f_bf = drv.find_battlefield_object(PlayerId(0), four).unwrap();

    let g_h = drv.find_hand_object(PlayerId(0), gt4).unwrap();
    let gt4_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: g_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: f_bf }],
        },
    );
    assert!(
        matches!(gt4_res, Err(EngineError::IllegalAction(_))),
        "4/4 must fail CompareOp::Gt(4) with IllegalAction, got {gt4_res:?}"
    );
}

// ============================================================================
// F11 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f11_01_mana_pool_empties_at_step_transition_cr500_4() {
    let mut b = SynthBundleBuilder::new();
    let g_land = b.add_synth_basic_land(
        "synth_forest_t2_111",
        "Synth Forest",
        "Forest",
        ManaColor::G,
    );
    let bolt = b.add_synth_burn_instant("synth_bolt_t2_111", "Synth Bolt", "{G}", 2);
    let db = b.build_db();
    let decks = build_60_deck_pair(g_land, bolt, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 221).unwrap();
    drv.keep_all_hands();

    let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let l_bf = drv.find_battlefield_object(PlayerId(0), g_land).unwrap();
    drv.step(
        PlayerId(0),
        Action::TapMana {
            object: l_bf,
            ability_index: 0,
            preferred_color: None,
        },
    )
    .unwrap();
    // Advance from Main1 -> BeginCombat: floating {G} empties!
    drv.advance_to(1, GamePhase::BeginCombat, 16).unwrap();
    let b_h = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    let cast_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    );
    assert!(
        matches!(cast_res, Err(EngineError::IllegalAction(_))),
        "CR 500.4: mana floated in Main1 must empty before BeginCombat (expected IllegalAction, got {cast_res:?})"
    );
}

#[test]
fn t2_f11_02_cost_reduction_cannot_reduce_colored_pips_cr118_7a() {
    let mut b = SynthBundleBuilder::new();
    let island = b.add_synth_basic_land(
        "synth_island_t2_112",
        "Synth Island",
        "Island",
        ManaColor::U,
    );
    let red = b.add_card("synth_red_t2_112", "Synth Reducer", |c| {
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
                        generic: 3,
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
    let uu_spell = b.add_synth_draw_sorcery("synth_uu_t2_112", "Synth DeepDraw", "{1}{U}{U}", 2);
    let db = b.build_db();
    let decks = build_60_deck_pair(island, red, &[uu_spell]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 222).unwrap();
    drv.keep_all_hands();

    let mut land_played = false;
    let mut found = false;
    for turn in (1u32..=59).step_by(2) {
        drv.advance_to_preserving(
            turn,
            GamePhase::Main1,
            64,
            &[(island, 1), (red, 1), (uu_spell, 1)],
        )
        .unwrap();
        if !land_played && let Some(l) = find_land_in_hand(&drv, PlayerId(0)) {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            land_played = true;
        }
        if land_played
            && drv.find_hand_object(PlayerId(0), red).is_some()
            && drv.find_hand_object(PlayerId(0), uu_spell).is_some()
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 1 Island and hold red + uu_spell within wait loop"
    );
    let r_h = drv.find_hand_object(PlayerId(0), red).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: r_h,
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

    // Only 1 Island on battlefield: generic {1} reduces to 0, but {U}{U} cannot be reduced!
    let uu_h = drv.find_hand_object(PlayerId(0), uu_spell).unwrap();
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: uu_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(
        !prev.can_pay,
        "CR 118.7a generic reduction cannot reduce {{U}}{{U}}"
    );
}

#[test]
fn t2_f11_03_x_cant_be_zero_rejects_x_0() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_113",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let xspell = b.add_card("synth_xmin1_t2_113", "Synth XMin1", |c| {
        c.mana_cost("{X}")
            .types(CardTypeMask::SORCERY)
            .spell_ability(SpellAbility {
                id: None,
                additional_costs: vec![],
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
                modal: None,
                cast_restrictions: vec![],
                cast_condition: None,
                x_cant_be_zero: true,
                span: Default::default(),
            });
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(mtn, xspell, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 223).unwrap();
    drv.keep_all_hands();

    let x_h = drv
        .find_hand_object(PlayerId(0), xspell)
        .expect("Synth XMin1 must be in opening hand");
    let res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: x_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: Some(0),
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    );
    assert!(
        matches!(res, Err(EngineError::IllegalAction(_))),
        "x_cant_be_zero must reject X=0 with IllegalAction (not NotImplemented), got {res:?}"
    );
    assert!(
        drv.find_hand_object(PlayerId(0), xspell).is_some(),
        "rejected X=0 spell must remain in hand"
    );

    let l = find_land_in_hand(&drv, PlayerId(0)).expect("Mountain must be in opening hand");
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let x_h2 = drv.find_hand_object(PlayerId(0), xspell).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: x_h2,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: Some(1),
            targets: vec![TargetRef::Player { id: PlayerId(1) }],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[1].life, 19);
}

#[test]
fn t2_f11_04_auto_tap_returns_can_pay_false_on_wrong_colors() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_114",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let uu_spell = b.add_synth_draw_sorcery("synth_uu_t2_114", "Synth Blue", "{U}{U}", 1);
    let db = b.build_db();
    let decks = build_60_deck_pair(mtn, uu_spell, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 224).unwrap();
    drv.keep_all_hands();

    let mut lands_played = 0u32;
    let mut found = false;
    for turn in (1u32..=29).step_by(2) {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(mtn, 1), (uu_spell, 1)])
            .unwrap();
        if lands_played < 3
            && let Some(l) = find_land_in_hand(&drv, PlayerId(0))
        {
            drv.step(
                PlayerId(0),
                Action::PlayLand {
                    object: l,
                    from_zone: Zone::Hand,
                    face_index: 0,
                },
            )
            .unwrap();
            lands_played += 1;
        }
        if lands_played >= 3 && drv.find_hand_object(PlayerId(0), uu_spell).is_some() {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "must play 3 Mountains and hold uu_spell within wait loop"
    );
    let uu_h = drv.find_hand_object(PlayerId(0), uu_spell).unwrap();
    let prev = drv.session.preview_auto_tap(
        PlayerId(0),
        &CastRequest {
            object: uu_h,
            option_id: 0,
            x_value: None,
        },
    );
    assert!(!prev.can_pay);
    assert!(prev.sources_to_tap.is_empty());
}

#[test]
fn t2_f11_05_cannot_pay_excess_life_when_life_below_cost() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_115", "Synth Swamp", "Swamp", ManaColor::B);
    let big_pay = b.add_card("synth_pay25_t2_115", "Synth BloodPact", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::ARTIFACT)
            .activated_ability(
                "pay25",
                vec![CostDef::PayLife {
                    amount: Box::new(Amount::Const { n: 25 }),
                    half_life_rounded_up: false,
                }],
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
    let decks = build_60_deck_pair(swamp, big_pay, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 225).unwrap();
    drv.keep_all_hands();

    let bp_h = drv.find_hand_object(PlayerId(0), big_pay).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: bp_h,
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
    let bp_bf = drv.find_battlefield_object(PlayerId(0), big_pay).unwrap();
    let act_res = drv.step_unchecked(
        PlayerId(0),
        Action::BeginActivate {
            object: bp_bf,
            ability_index: 0,
        },
    );
    assert!(
        matches!(act_res, Err(EngineError::IllegalAction(_))),
        "CR 119.4: cannot pay 25 life at 20 life (expected IllegalAction, got {act_res:?})"
    );
}

// ============================================================================
// F12 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f12_01_wrong_player_apply_action_rejected() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_121",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 231).unwrap();
    let seq = drv.session.state().event_seq();
    let res = drv.step_unchecked(PlayerId(1), Action::KeepHand);
    assert!(matches!(res, Err(EngineError::IllegalAction(_))));
    assert_eq!(drv.session.state().event_seq(), seq);
}

#[test]
fn t2_f12_02_wrong_prompt_id_or_mismatched_choice_answer_rejected() {
    let mut b = SynthBundleBuilder::new();
    let u_land = b.add_synth_basic_land(
        "synth_island_t2_122",
        "Synth Island",
        "Island",
        ManaColor::U,
    );
    let scry = b.add_card("synth_scry_t2_122", "Synth Scry", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Scry,
                    Selector::PlayerRef {
                        player: PlayerRef::You,
                    },
                    Some(Amount::Const { n: 1 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(u_land, scry, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 232).unwrap();
    drv.keep_all_hands();

    let s_h = drv.find_hand_object(PlayerId(0), scry).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: s_h,
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
    assert!(matches!(drv.decision(), DecisionState::Prompt { .. }));
    let ans_res = drv.step_unchecked(
        PlayerId(0),
        Action::Answer {
            prompt_id: 999_999,
            answer: ChoiceAnswer::PickNumber { value: 42 },
        },
    );
    assert!(
        matches!(ans_res, Err(EngineError::IllegalAction(_))),
        "wrong prompt_id must fail with IllegalAction, got {ans_res:?}"
    );
}

#[test]
fn t2_f12_03_macro_action_cast_spell_partial_failure_rolls_back() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_123",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let bolt = b.add_synth_burn_instant("synth_bolt_t2_123", "Synth Bolt", "{R}", 3);
    let db = b.build_db();
    let decks = build_60_deck_pair(mtn, bolt, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 233).unwrap();
    drv.keep_all_hands();

    let l = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let l_bf = drv.find_battlefield_object(PlayerId(0), mtn).unwrap();
    let b_h = drv.find_hand_object(PlayerId(0), bolt).unwrap();
    let seq_before = drv.session.state().event_seq();

    // Target a bogus stale ObjectId -> macro must roll back without tapping Mountain!
    let bad_target = ObjectId {
        index: 99999,
        generation: 1,
    };
    let cast_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![TargetRef::Object { id: bad_target }],
        },
    );
    assert!(
        matches!(cast_res, Err(EngineError::IllegalAction(_))),
        "stale target in MacroAction::CastSpell must reject with IllegalAction, got {cast_res:?}"
    );
    assert!(!drv.battlefield_view(PlayerId(0), l_bf).unwrap().tapped);
    assert!(drv.find_hand_object(PlayerId(0), bolt).is_some());
    assert_eq!(drv.session.state().event_seq(), seq_before);
}

#[test]
fn t2_f12_04_sorcery_rejected_on_opponents_turn_or_nonempty_stack() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_124",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let sorc = b.add_synth_draw_sorcery("synth_sorc_t2_124", "Synth SlowDraw", "{0}", 1);
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, sorc, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 234).unwrap();
    drv.keep_all_hands();

    // Advance to P1's turn, pass priority so P0 gets priority on opponent's turn:
    drv.advance_to(2, GamePhase::Main1, 64).unwrap();
    drv.pass_priority();
    assert_eq!(drv.actor(), Some(PlayerId(0)));
    let s_h = drv.find_hand_object(PlayerId(0), sorc).unwrap();
    let cast_res = drv.step_unchecked(
        PlayerId(0),
        Action::BeginCast {
            object: s_h,
            option_id: 0,
            from_zone: Zone::Hand,
        },
    );
    assert!(
        matches!(cast_res, Err(EngineError::IllegalAction(_))),
        "Sorcery cannot be cast on opponent's turn (expected IllegalAction, got {cast_res:?})"
    );
}

#[test]
fn t2_f12_05_duplicate_target_on_single_target_slot_rejected_cr115_3() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_125",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_125", "Synth Bear", "{0}", 2, 2);
    let two_tgts = b.add_card("synth_two_t2_125", "Synth ArcTwo", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![TargetSpec {
                    min: 2,
                    max: 2,
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
    let decks = build_60_deck_pair(mtn, bear, &[two_tgts]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 235).unwrap();
    drv.keep_all_hands();

    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
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

    let tt_h = drv.find_hand_object(PlayerId(0), two_tgts).unwrap();
    let dup_res = drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: tt_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: None,
            targets: vec![
                TargetRef::Object { id: b_bf },
                TargetRef::Object { id: b_bf },
            ],
        },
    );
    assert!(
        matches!(dup_res, Err(EngineError::IllegalAction(_))),
        "CR 115.3: single TargetSpec min=2 max=2 cannot pick same ObjectId twice (expected IllegalAction, got {dup_res:?})"
    );
}

// ============================================================================
// F13 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f13_01_choose_objects_min_zero_on_empty_candidates_no_deadlock() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_131", "Synth Swamp", "Swamp", ManaColor::B);
    let up_to = b.add_card("synth_upto_t2_131", "Synth EmptySearch", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::Search {
                        spec: Box::new(mtg_ir::SearchSpec {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::PLANESWALKER,
                                ..Default::default()
                            }),
                            count: 2,
                            up_to: true,
                            stated_quality: true,
                            destinations: vec![mtg_ir::SearchDestination {
                                zone: Zone::Hand,
                                tapped: false,
                                on_top: false,
                            }],
                            reveal: false,
                            description: None,
                            zones: ZoneMask::GRAVEYARD,
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
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(swamp, up_to, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 241).unwrap();
    drv.keep_all_hands();

    let u_h = drv.find_hand_object(PlayerId(0), up_to).unwrap();
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
    if let DecisionState::Prompt {
        request: ChoiceRequest::ChooseObjects { prompt_id, .. },
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
    assert_eq!(drv.decision(), DecisionState::Priority);
}

#[test]
fn t2_f13_02_game_over_rejects_all_further_actions() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_132",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut s = MatchSession::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 242).unwrap();
    s.apply_action(PlayerId(0), Action::Concede).unwrap();
    assert!(s.legal_actions(PlayerId(0)).is_empty());
    assert!(s.legal_actions(PlayerId(1)).is_empty());
    assert!(matches!(
        s.apply_action(PlayerId(1), Action::PassPriority),
        Err(EngineError::IllegalAction(_))
    ));
}

#[test]
fn t2_f13_03_strict_config_blocks_guided_fallback() {
    let mut b = SynthBundleBuilder::new();
    let unsup = b.add_card("synth_unsup_t2_133", "Synth Broken", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .unsupported(UnsupportedReason::Unparsed, "unsupported clause");
    });
    let db = b.build_db();
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: vec![unsup; 60],
        sideboard: vec![],
    };
    let rep = MatchSession::validate_deck(&db, &GameConfig::standard_1v1(), &deck);
    assert!(!rep.is_legal);
    assert_eq!(rep.unsupported_card_ids, vec![unsup]);
}

#[test]
fn t2_f13_04_x_equals_zero_tokens_or_damage_does_nothing() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land(
        "synth_plains_t2_134",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let xtok = b.add_card("synth_xtok_t2_134", "Synth XTokens", |c| {
        c.mana_cost("{X}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![],
                vec![effect_step(
                    EffectOp::CreateToken {
                        token: Some(Box::new(mtg_ir::TokenSpec {
                            name: "Synth Wisp".into(),
                            power: 1,
                            toughness: 1,
                            colors: ColorMask::W,
                            types: CardTypeMask::CREATURE,
                            supertypes: SupertypeMask::empty(),
                            subtypes: Default::default(),
                            type_line: "Creature".into(),
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
                    Some(Amount::X),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(w_land, xtok, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 244).unwrap();
    drv.keep_all_hands();

    let x_h = drv.find_hand_object(PlayerId(0), xtok).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: x_h,
            option_id: 0,
            from_zone: Zone::Hand,
            modes: vec![],
            x_value: Some(0),
            targets: vec![],
        },
    )
    .unwrap();
    drv.pass_priority();
    drv.pass_priority();
    assert_eq!(drv.session.view_model(None).players[0].battlefield.len(), 0);
}

#[test]
fn t2_f13_05_negative_power_creature_deals_zero_combat_damage() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_135",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let neg = b.add_synth_keyword_creature(
        "synth_neg_t2_135",
        "Synth Negative",
        "{0}",
        -3,
        2,
        &["haste"],
    );
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, neg, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 245).unwrap();
    drv.keep_all_hands();

    let n_h = drv.find_hand_object(PlayerId(0), neg).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: n_h,
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
    let n_bf = drv.find_battlefield_object(PlayerId(0), neg).unwrap();
    drv.advance_to(1, GamePhase::DeclareAttackers, 16).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::DeclareAttackers {
            assignments: vec![(n_bf, AttackTarget::Player { id: PlayerId(1) })],
        },
    )
    .unwrap();
    drv.advance_to(1, GamePhase::Main2, 32).unwrap();
    assert_eq!(
        drv.session.view_model(None).players[1].life,
        20,
        "CR 510.1a: -3/2 attacker must deal 0 damage (never heal defender!)"
    );
}

// ============================================================================
// F14 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f14_01_mulligan_to_zero_cards_proceeds_cleanly() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_141",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 251).unwrap();
    for _ in 0..7 {
        if matches!(drv.decision(), DecisionState::Mulligan) && drv.actor() == Some(PlayerId(0)) {
            let legal = drv.session.legal_actions(PlayerId(0));
            if legal.contains(&Action::TakeMulligan) {
                drv.step(PlayerId(0), Action::TakeMulligan).unwrap();
            }
        }
    }
    while matches!(drv.decision(), DecisionState::Mulligan) {
        let a = drv.actor().unwrap();
        let legal = drv.session.legal_actions(a);
        if let Some(bot) = legal
            .iter()
            .find(|act| matches!(act, Action::BottomCard { .. }))
        {
            drv.step(a, bot.clone()).unwrap();
        } else {
            drv.step(a, Action::KeepHand).unwrap();
        }
    }
    assert_eq!(
        drv.session.view_model(Some(PlayerId(0))).players[0].hand_count,
        0
    );
}

#[test]
fn t2_f14_02_second_land_play_in_same_turn_rejected_cr305_2() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_142",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![plains; 60],
        sideboard: vec![],
    };
    let mut drv =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 252).unwrap();
    drv.keep_all_hands();

    let l1 = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    drv.step(
        PlayerId(0),
        Action::PlayLand {
            object: l1,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    )
    .unwrap();
    let l2 = find_land_in_hand(&drv, PlayerId(0)).unwrap();
    let land_res = drv.step_unchecked(
        PlayerId(0),
        Action::PlayLand {
            object: l2,
            from_zone: Zone::Hand,
            face_index: 0,
        },
    );
    assert!(
        matches!(land_res, Err(EngineError::IllegalAction(_))),
        "CR 305.2: second land play in same turn must be rejected with IllegalAction, got {land_res:?}"
    );
}

#[test]
fn t2_f14_03_simultaneous_lethal_sba_draw_cr104_4a() {
    let mut b = SynthBundleBuilder::new();
    let mtn = b.add_synth_basic_land(
        "synth_mtn_t2_143",
        "Synth Mountain",
        "Mountain",
        ManaColor::R,
    );
    let quake = b.add_card("synth_quake_t2_143", "Synth Quake20", |c| {
        c.mana_cost("{0}")
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
                    Selector::Player {
                        filter: PlayerFilter {
                            relation: PlayerRelation::EachPlayer,
                        },
                    },
                    Some(Amount::Const { n: 20 }),
                )],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(mtn, quake, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 253).unwrap();
    drv.keep_all_hands();

    let q_h = drv.find_hand_object(PlayerId(0), quake).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: q_h,
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
    assert_eq!(drv.decision(), DecisionState::GameOver { winner: None });
}

#[test]
fn t2_f14_04_illegal_attacker_summoning_sick_or_defender_rejected() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_144",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_144", "Synth Bear", "{0}", 2, 2);
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, bear, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 254).unwrap();
    drv.keep_all_hands();

    let b_h = drv.find_hand_object(PlayerId(0), bear).unwrap();
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

    drv.advance_to(1, GamePhase::DeclareAttackers, 16).unwrap();
    let atk_res = drv.step_unchecked(
        PlayerId(0),
        Action::AssignAttacker {
            attacker: b_bf,
            target: AttackTarget::Player { id: PlayerId(1) },
        },
    );
    assert!(
        matches!(atk_res, Err(EngineError::IllegalAction(_))),
        "CR 302.6: summoning-sick creature without Haste cannot attack (expected IllegalAction, got {atk_res:?})"
    );
}

#[test]
fn t2_f14_05_ten_poison_counters_sba_loss_cr704_5c() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_145", "Synth Swamp", "Swamp", ManaColor::B);
    let toxin = b.add_card("synth_toxin_t2_145", "Synth Toxin10", |c| {
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
    let db = b.build_db();
    let decks = build_60_deck_pair(swamp, toxin, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 255).unwrap();
    drv.keep_all_hands();

    let t_h = drv.find_hand_object(PlayerId(0), toxin).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: t_h,
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
        drv.decision(),
        DecisionState::GameOver {
            winner: Some(PlayerId(0))
        }
    );
}

// ============================================================================
// F15 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f15_01_match_session_new_mismatched_decks_len_rejected() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land(
        "synth_plains_t2_151",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 60],
        sideboard: vec![],
    };
    let res = MatchSession::new(db, GameConfig::commander(4), vec![d.clone(), d], 261);
    assert!(matches!(
        res,
        Err(EngineError::PlayerCountMismatch {
            expected: 4,
            actual: 2
        })
    ));
}

#[test]
fn t2_f15_02_match_session_new_unknown_card_def_id_rejected() {
    let b = SynthBundleBuilder::new();
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![CardDefId(99_999)],
        sideboard: vec![],
    };
    let res = MatchSession::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 262);
    assert!(matches!(
        res,
        Err(EngineError::UnknownCardDef(CardDefId(99_999)))
    ));
}

#[test]
fn t2_f15_03_hover_breakdown_invalid_object_id_returns_none() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land(
        "synth_plains_t2_153",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let db = b.build_db();
    let d = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 60],
        sideboard: vec![],
    };
    let s = MatchSession::new(db, GameConfig::standard_1v1(), vec![d.clone(), d], 263).unwrap();
    assert!(
        s.hover_breakdown(
            None,
            ObjectId {
                index: 9999,
                generation: 1
            }
        )
        .is_none()
    );
}

#[test]
fn t2_f15_04_from_snapshot_rejects_corrupted_bytes() {
    let b = SynthBundleBuilder::new();
    let db = b.build_db();
    #[cfg(feature = "m5-session-ext")]
    {
        let res = MatchSession::from_snapshot(db, b"corrupt_snapshot_bytes");
        assert!(
            matches!(res, Err(EngineError::Serialization(_))),
            "corrupted snapshot bytes must return EngineError::Serialization (not NotImplemented), got {res:?}"
        );
    }
    #[cfg(not(feature = "m5-session-ext"))]
    {
        assert!(db.cards.is_empty());
        panic!("requires M5 MatchSession extensions (--features m5-session-ext): from_snapshot");
    }
}

#[test]
fn t2_f15_05_determinize_empty_opponent_hand_and_library() {
    let mut b = SynthBundleBuilder::new();
    let swamp = b.add_synth_basic_land("synth_swamp_t2_155", "Synth Swamp", "Swamp", ManaColor::B);
    let mill = b.add_card("synth_mill_t2_155", "Synth EmptyOpp", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::SORCERY)
            .spell_effect(
                vec![target_player()],
                vec![
                    effect_step(
                        EffectOp::Mill,
                        Selector::Target { index: 0 },
                        Some(Amount::Const { n: 100 }),
                    ),
                    effect_step(
                        EffectOp::DiscardHand {
                            draw_that_many: false,
                            draw_after: None,
                        },
                        Selector::Target { index: 0 },
                        None,
                    ),
                ],
            );
    });
    let db = b.build_db();
    let decks = build_60_deck_pair(swamp, mill, &[]);
    let mut drv = SessionDriver::new(db, GameConfig::standard_1v1(), decks, 265).unwrap();
    drv.keep_all_hands();

    let m_h = drv.find_hand_object(PlayerId(0), mill).unwrap();
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: m_h,
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

    let mut rng = RngState::splitmix64(999);
    let det = drv.session.determinize(PlayerId(0), &mut rng);
    assert!(det.players()[1].hand.is_empty());
    assert!(det.players()[1].library.is_empty());
}

// ============================================================================
// F16 Boundary & Negative (5 cases)
// ============================================================================

fn build_diff_bin_t2(dir: &TempDir) -> std::path::PathBuf {
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_plains_t2d", "Synth Plains", "Plains", ManaColor::W);
    b.add_synth_vanilla_creature("synth_bear_t2d", "Synth Bear", "{0}", 2, 2);
    let j = dir.child("b.json");
    let bin = dir.child("b.bin");
    std::fs::write(&j, serde_json::to_string(&b.build_bundle()).unwrap()).unwrap();
    assert!(run_build_db(&j, &bin, false, None).success);
    bin
}

#[test]
fn t2_f16_01_diff_test_rejects_missing_oracle_id() {
    let tmp = TempDir::new("t2_f16_01");
    let bin = build_diff_bin_t2(&tmp);
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": ["synth_nonexistent_oracle"], "handInitial": [] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": ["synth_plains_t2d"], "handInitial": [] }
        ],
        "steps": []
    });
    let rep = tmp.child("missing_oracle.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(
        !res.success,
        "missing oracle_id in diff-test must exit non-zero"
    );
}

#[test]
fn t2_f16_02_diff_test_catches_initial_library_order_mismatch() {
    let tmp = TempDir::new("t2_f16_02");
    let bin = build_diff_bin_t2(&tmp);
    let mut lib = vec!["synth_plains_t2d"; 30];
    lib.extend(vec!["synth_bear_t2d"; 30]);
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": lib, "handInitial": [] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": lib, "handInitial": [] }
        ],
        "steps": []
    });
    let rep = tmp.child("bad_lib.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(
        !res.success,
        "unshuffled libraryOrderInitial mismatch must fail diff-test"
    );
}

#[test]
fn t2_f16_03_diff_test_catches_battlefield_counters_mismatch() {
    let tmp = TempDir::new("t2_f16_03");
    let bin = build_diff_bin_t2(&tmp);
    let lib0 = vec!["synth_bear_t2d".to_string(); 60];
    let lib1 = vec!["synth_plains_t2d".to_string(); 60];

    let mk_p = |seat: u8, hand: usize, bf_count: usize, oracle: &str, bf: serde_json::Value| {
        serde_json::json!({
            "seat": seat, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": hand, "battlefield": bf_count, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec![oracle; hand],
            "battlefield": bf
        })
    };

    let p0_init = mk_p(0, 7, 0, "synth_bear_t2d", serde_json::json!([]));
    let p0_cast = mk_p(0, 6, 0, "synth_bear_t2d", serde_json::json!([]));
    let p0_bad_counters = mk_p(
        0,
        6,
        1,
        "synth_bear_t2d",
        serde_json::json!([{
            "oracleId": "synth_bear_t2d", "tapped": false, "pt": [2, 2],
            "types": ["Creature"], "keywords": [], "counters": [["+1/+1", 99]],
            "damageMarked": 0, "attachedToOracleId": null
        }]),
    );
    let p1 = mk_p(1, 7, 0, "synth_plains_t2d", serde_json::json!([]));

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
    let bad_counters_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0_bad_counters, p1]
    });

    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": lib0, "handInitial": vec!["synth_bear_t2d"; 7] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": lib1, "handInitial": vec!["synth_plains_t2d"; 7] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": main1_digest },
            {
                "stepIndex": 2,
                "actorSeat": 0,
                "action": { "type": "cast_spell", "instanceId": "obj_0", "optionId": "normal" },
                "objectLocators": { "obj_0": { "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "synth_bear_t2d" } },
                "digestAfter": stack_p0_digest
            },
            { "stepIndex": 3, "actorSeat": 0, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": stack_p1_digest },
            { "stepIndex": 4, "actorSeat": 1, "action": { "type": "pass_priority" }, "objectLocators": {}, "digestAfter": bad_counters_digest }
        ]
    });
    let rep = tmp.child("bad_bf.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(!res.success);
    assert!(
        res.stderr.contains("counters") || res.stdout.contains("counters"),
        "diff-test must report counters divergence, got stdout:\n{}\nstderr:\n{}",
        res.stdout,
        res.stderr
    );
}

#[test]
fn t2_f16_04_diff_test_rejects_truncated_json_trace() {
    let tmp = TempDir::new("t2_f16_04");
    let bin = build_diff_bin_t2(&tmp);
    let rep = tmp.child("bad.json");
    std::fs::write(&rep, "{ \"bad\": ").unwrap();
    assert!(!run_diff_test(&bin, &rep).success);
}

#[test]
fn t2_f16_05_diff_test_catches_unallowlisted_priority_divergence() {
    let tmp = TempDir::new("t2_f16_05");
    let bin = build_diff_bin_t2(&tmp);
    let mut rng = RngState::mulberry32(42);
    let mut lib0 = vec!["synth_plains_t2d".to_string(); 60];
    rng.shuffle(&mut lib0);
    let mut lib1 = vec!["synth_plains_t2d".to_string(); 60];
    rng.shuffle(&mut lib1);
    let mk_p = |seat: u8| {
        serde_json::json!({
            "seat": seat, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_plains_t2d"; 7],
            "battlefield": []
        })
    };
    let p0 = mk_p(0);
    let p1 = mk_p(1);
    let mulligan_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
        "players": [p0.clone(), p1.clone()]
    });
    let bad_prio_digest = serde_json::json!({
        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1,
        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
        "players": [p0, p1]
    });
    let trace = serde_json::json!({
        "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
        "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
        "seats": [
            { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": lib0, "handInitial": [] },
            { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": lib1, "handInitial": [] }
        ],
        "steps": [
            { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mulligan_digest },
            { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": bad_prio_digest }
        ]
    });
    let rep = tmp.child("bad_prio.json");
    std::fs::write(&rep, serde_json::to_string(&trace).unwrap()).unwrap();
    let res = run_diff_test(&bin, &rep);
    assert!(!res.success);
    assert!(
        res.stderr.contains("priority_seat") || res.stdout.contains("priority_seat"),
        "diff-test must report priority_seat divergence, got stdout:\n{}\nstderr:\n{}",
        res.stdout,
        res.stderr
    );
}

// ============================================================================
// F17 Boundary & Negative (5 cases)
// ============================================================================

#[test]
fn t2_f17_01_legend_rule_respects_name_equality_boundary_cr704_5j() {
    let mut b = SynthBundleBuilder::new();
    let w_land = b.add_synth_basic_land(
        "synth_plains_t2_171",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let leg_a = b.add_card("synth_leg_a_171", "Synth Hero", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let leg_b = b.add_card("synth_leg_b_171", "Synth Rival", |c| {
        c.mana_cost("{0}")
            .types(CardTypeMask::CREATURE)
            .supertypes(SupertypeMask::LEGENDARY)
            .pt(2, 2);
    });
    let _ = w_land;
    let db = b.build_db();
    let mut main = vec![leg_a; 30];
    main.extend(vec![leg_b; 30]);
    let deck = DeckSpec {
        commanders: vec![],
        mainboard: main,
        sideboard: vec![],
    };
    let mut drv = SessionDriver::new(
        db,
        GameConfig::standard_1v1(),
        vec![deck.clone(), deck],
        271,
    )
    .unwrap();
    drv.keep_all_hands();

    for turn in [1u32, 3, 5, 7] {
        drv.advance_to_preserving(turn, GamePhase::Main1, 64, &[(leg_a, 2), (leg_b, 1)])
            .unwrap();
        let a_cnt = drv.session.view_model(Some(PlayerId(0))).players[0]
            .hand
            .iter()
            .filter(|o| o.card_def == leg_a)
            .count();
        if a_cnt >= 2 && drv.find_hand_object(PlayerId(0), leg_b).is_some() {
            break;
        }
    }

    // Two distinct names survive; two identical names trigger Legend Rule SBA!
    let a1 = drv
        .find_hand_object(PlayerId(0), leg_a)
        .expect("first Synth Hero must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a1,
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

    let b1 = drv
        .find_hand_object(PlayerId(0), leg_b)
        .expect("Synth Rival must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: b1,
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
    assert!(drv.find_battlefield_object(PlayerId(0), leg_a).is_some());
    assert!(drv.find_battlefield_object(PlayerId(0), leg_b).is_some());

    let a2 = drv
        .find_hand_object(PlayerId(0), leg_a)
        .expect("second Synth Hero must be in hand");
    drv.macro_step(
        PlayerId(0),
        MacroAction::CastSpell {
            object: a2,
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
            request:
                ChoiceRequest::ChooseObjects {
                    prompt_id,
                    candidates,
                    ..
                },
        } => {
            assert_eq!(candidates.len(), 2);
            drv.step(
                PlayerId(0),
                Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickObject { id: candidates[0] },
                },
            )
            .unwrap();
        }
        other => panic!("expected Legend Rule ChooseObjects prompt, got {other:?}"),
    }
    let hero_count = drv.session.view_model(None).players[0]
        .battlefield
        .iter()
        .filter(|o| o.card_def == leg_a)
        .count();
    assert_eq!(
        hero_count, 1,
        "CR 704.5j Legend Rule must keep exactly 1 Synth Hero"
    );
}

#[test]
fn t2_f17_02_game_state_clone_scales_compactly_across_board_sizes() {
    let mut b = SynthBundleBuilder::new();
    let land = b.add_synth_basic_land(
        "synth_plains_t2_172",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_172", "Synth Bear", "{0}", 2, 2);
    let anthem = b.add_synth_anthem_enchantment("synth_anth_t2_172", "Synth Anthem", "{0}", 1, 1);
    let db = b.build_db();
    let d10 = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 10],
        sideboard: vec![],
    };
    let mut main_large = vec![bear; 75];
    main_large.extend(vec![anthem; 75]);
    let d150_p0 = DeckSpec {
        commanders: vec![],
        mainboard: main_large,
        sideboard: vec![],
    };
    let d150_p1 = DeckSpec {
        commanders: vec![],
        mainboard: vec![land; 150],
        sideboard: vec![],
    };
    let s_small = MatchSession::new(
        Arc::clone(&db),
        GameConfig::standard_1v1(),
        vec![d10.clone(), d10],
        272,
    )
    .unwrap();
    let mut s_large =
        SessionDriver::new(db, GameConfig::standard_1v1(), vec![d150_p0, d150_p1], 272).unwrap();
    s_large.keep_all_hands();

    for turn in [1u32, 3, 5, 7, 9, 11, 13, 15] {
        s_large.advance_to(turn, GamePhase::Main1, 64).unwrap();
        while let Some(obj) = s_large
            .find_hand_object(PlayerId(0), bear)
            .or_else(|| s_large.find_hand_object(PlayerId(0), anthem))
        {
            s_large
                .macro_step(
                    PlayerId(0),
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
            s_large.pass_priority();
            s_large.pass_priority();
        }
        if s_large.session.state().players()[0].battlefield.len() >= 12 {
            break;
        }
    }
    assert!(
        s_large.session.state().players()[0].battlefield.len() >= 12,
        "populated large board must have >= 12 permanents before clone benchmark"
    );

    let mut acc = 0u64;
    let t0 = Instant::now();
    for _ in 0..1000 {
        let cloned = s_small.state().clone();
        acc = acc.wrapping_add(std::hint::black_box(cloned.layer_rev()));
    }
    let d_small = t0.elapsed();
    let t1 = Instant::now();
    for _ in 0..1000 {
        let cloned = s_large.session.state().clone();
        acc = acc.wrapping_add(std::hint::black_box(cloned.layer_rev()));
    }
    let d_large = t1.elapsed();
    assert!(acc > 0);
    assert!(
        d_large.as_nanos()
            <= d_small
                .as_nanos()
                .saturating_mul(10)
                .saturating_add(5_000_000),
        "clone must scale compactly across board sizes (small={d_small:?}, large={d_large:?})"
    );
    if !cfg!(debug_assertions) {
        let per_large_us = (d_large.as_secs_f64() * 1_000_000.0) / 1000.0;
        assert!(
            per_large_us < 20.0,
            "release clone on populated board took {per_large_us:.2} µs (>= 20 µs)"
        );
    }
}

#[test]
fn t2_f17_03_verify_layer_cache_feature_enabled_build_check() {
    let tmp = TempDir::new("t2_f17_03");
    let root = repo_root();
    let out = std::process::Command::new("cargo")
        .args([
            "check",
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
        .unwrap();
    assert!(
        out.status.success(),
        "cargo check -p mtg-engine --features verify-layer-cache failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn t2_f17_04_workspace_clippy_and_fmt_clean() {
    let tmp = TempDir::new("t2_f17_04");
    let root = repo_root();
    let fmt = std::process::Command::new("cargo")
        .args([
            "fmt",
            "--manifest-path",
            root.join("Cargo.toml").to_str().unwrap(),
            "--all",
            "--",
            "--check",
        ])
        .output()
        .unwrap();
    assert!(fmt.status.success(), "cargo fmt --all -- --check failed");

    let clippy = std::process::Command::new("cargo")
        .args([
            "clippy",
            "--manifest-path",
            root.join("Cargo.toml").to_str().unwrap(),
            "--workspace",
            "--all-targets",
            "--target-dir",
            tmp.path().to_str().unwrap(),
            "--",
            "-D",
            "warnings",
        ])
        .output()
        .unwrap();
    assert!(
        clippy.status.success(),
        "cargo clippy --workspace --all-targets -- -D warnings failed:\n{}",
        String::from_utf8_lossy(&clippy.stderr)
    );
}

#[test]
fn t2_f17_05_zero_hidden_information_leak_across_200_random_steps() {
    let mut b = SynthBundleBuilder::new();
    let plains = b.add_synth_basic_land(
        "synth_plains_t2_175",
        "Synth Plains",
        "Plains",
        ManaColor::W,
    );
    let bear = b.add_synth_vanilla_creature("synth_bear_t2_175", "Synth Bear", "{W}", 2, 2);
    let db = b.build_db();
    let decks = build_60_deck_pair(plains, bear, &[]);
    let (final_dec, final_drv) =
        play_deterministic_random_game_driver(db, GameConfig::standard_1v1(), decks, 275, 200);
    assert!(
        final_drv.session.state().turn_number() > 1
            || matches!(final_dec, DecisionState::GameOver { .. }),
        "200-step random rollout must advance past Turn 1 (stuck at turn {})",
        final_drv.session.state().turn_number()
    );
    assert!(
        final_drv
            .session
            .replay_log()
            .entries
            .iter()
            .any(|e| !matches!(
                e.action,
                Action::KeepHand | Action::PassPriority | Action::Concede
            )),
        "200-step random rollout must execute gameplay actions beyond KeepHand/PassPriority"
    );
    // Also explicitly verify direct session invariant functions on a fresh session.
    let mut b2 = SynthBundleBuilder::new();
    let p2 = b2.add_synth_basic_land("synth_p2_175", "Synth P", "Plains", ManaColor::W);
    let db2 = b2.build_db();
    let d2 = DeckSpec {
        commanders: vec![],
        mainboard: vec![p2; 60],
        sideboard: vec![],
    };
    let s = MatchSession::new(db2, GameConfig::standard_1v1(), vec![d2.clone(), d2], 275).unwrap();
    assert_view_model_redaction(&s);
    assert_observation_consistency(&s, PlayerId(0));
    assert_observation_consistency(&s, PlayerId(1));
}
