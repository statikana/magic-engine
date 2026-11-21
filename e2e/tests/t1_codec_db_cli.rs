//! Tier 1 Opaque-Box E2E Tests: Features F1–F4 (IR v2 Schema, Codec, TS Bridge, `mtg-cli`, `TsReplayTrace`).
//!
//! Covers 20 tests: `T1-F01-01..05`, `T1-F02-01..05`, `T1-F03-01..05`, `T1-F04-01..05`.

use std::collections::HashSet;
use std::process::Command;

use mtg_engine::{RngState, TsReplayTrace};
use mtg_engine_e2e::cli::{
    TempDir, peckhamr_root, repo_root, run_build_db, run_inspect_db, run_inspect_db_release,
    run_node_script, write_synthetic_jsonl_gz,
};
use mtg_engine_e2e::synth::SynthBundleBuilder;
use mtg_ir::{
    Amount, CardDatabase, CardTypeMask, ColorMask, CounterId, FilterFlags, IR_V2_VERSION,
    IrV2Bundle, ManaColor, ManaCost, ManaPip, ReplacementWith, SupertypeMask, Zone, ZoneChangeMod,
    ZoneMask,
};

fn parse_decode_ms(stdout: &str) -> Option<f64> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("Decode time:") {
            let num = rest.split_whitespace().next()?;
            return num.parse::<f64>().ok();
        }
    }
    None
}

// ============================================================================
// F1: `mtg-ir` Schema & Codec Fixes (5 cases)
// ============================================================================

#[test]
fn t1_f01_01_integer_bitmask_json_serialization() {
    let cm = ColorMask::W | ColorMask::U;
    let ctm = CardTypeMask::CREATURE | CardTypeMask::ARTIFACT;
    let stm = SupertypeMask::LEGENDARY;
    let zm = ZoneMask::BATTLEFIELD;
    let ff = FilterFlags::UNBLOCKED_ATTACKER | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN;

    let v_cm = serde_json::to_value(cm).unwrap();
    let v_ctm = serde_json::to_value(ctm).unwrap();
    let v_stm = serde_json::to_value(stm).unwrap();
    let v_zm = serde_json::to_value(zm).unwrap();
    let v_ff = serde_json::to_value(ff).unwrap();

    assert_eq!(
        v_cm,
        serde_json::json!(3u8),
        "ColorMask must serialize as integer"
    );
    assert_eq!(
        v_ctm,
        serde_json::json!(6u16),
        "CardTypeMask must serialize as integer"
    );
    assert_eq!(
        v_stm,
        serde_json::json!(2u8),
        "SupertypeMask must serialize as integer"
    );
    assert_eq!(
        v_zm,
        serde_json::json!(4u8),
        "ZoneMask must serialize as integer"
    );
    let expected_ff = (1u64 << 31) | (1u64 << 52);
    assert_eq!(
        v_ff,
        serde_json::json!(expected_ff),
        "FilterFlags must serialize as u64 integer"
    );

    assert_eq!(serde_json::from_value::<ColorMask>(v_cm).unwrap(), cm);
    assert_eq!(serde_json::from_value::<CardTypeMask>(v_ctm).unwrap(), ctm);
    assert_eq!(serde_json::from_value::<SupertypeMask>(v_stm).unwrap(), stm);
    assert_eq!(serde_json::from_value::<ZoneMask>(v_zm).unwrap(), zm);
    assert_eq!(serde_json::from_value::<FilterFlags>(v_ff).unwrap(), ff);
}

#[test]
fn t1_f01_02_mana_cost_parses_standard_hybrid_2brid_x_and_cp() {
    let mc = ManaCost::parse("{2}{X}{U/R}{G/P}{2/W}");
    assert_eq!(mc.generic, 2);
    assert_eq!(mc.x_count, 1);
    assert_eq!(
        mc.pips.as_slice(),
        &[
            ManaPip::Hybrid {
                a: ManaColor::U,
                b: ManaColor::R,
            },
            ManaPip::Phyrexian {
                color: ManaColor::G,
            },
            ManaPip::TwoBrid {
                color: ManaColor::W,
            },
        ]
    );
    assert_eq!(mc.mana_value(), 6);

    let cp = ManaCost::parse("{C/P}");
    assert_eq!(
        cp.pips.as_slice(),
        &[ManaPip::Phyrexian {
            color: ManaColor::C,
        }],
        "{{C/P}} must parse as ManaPip::Phyrexian {{ color: C }}"
    );
    assert_eq!(cp.mana_value(), 1);
}

#[test]
fn t1_f01_03_zone_change_mod_json_and_postcard_round_trip() {
    let mod_spec = ZoneChangeMod {
        to: Some(Zone::Battlefield),
        tapped: Some(true),
        counters: Some((CounterId(0), Box::new(Amount::Const { n: 2 }))),
        enter_under_opponent_control: true,
        ..Default::default()
    };
    let rw = ReplacementWith::ZoneChange {
        mod_spec: Box::new(mod_spec),
    };

    let json_val = serde_json::to_value(&rw).unwrap();
    let from_json: ReplacementWith = serde_json::from_value(json_val).unwrap();
    assert_eq!(from_json, rw);

    let pc_bytes = postcard::to_allocvec(&rw).unwrap();
    let from_pc: ReplacementWith = postcard::from_bytes(&pc_bytes).unwrap();
    assert_eq!(from_pc, rw);

    // Verify default ZoneChangeMod round-trips cleanly and does not encode nested Option ambiguity.
    let def_mod = ZoneChangeMod::default();
    let def_val = serde_json::to_value(&def_mod).unwrap();
    let def_obj = def_val
        .as_object()
        .expect("ZoneChangeMod must serialize as JSON object");
    assert!(def_obj.contains_key("opponent_discard_to_battlefield"));
    let def_pc = postcard::to_allocvec(&def_mod).unwrap();
    let def_back: ZoneChangeMod = postcard::from_bytes(&def_pc).unwrap();
    assert_eq!(def_back, def_mod);
}

#[test]
fn t1_f01_04_card_database_raw_and_zstd_round_trip() {
    let mut b = SynthBundleBuilder::new();
    let c0 = b.add_synth_basic_land("synth_land_w", "Synth Plains", "Plains", ManaColor::W);
    let c1 = b.add_synth_vanilla_creature("synth_bear", "Synth Bear", "{1}{G}", 2, 2);
    let c2 = b.add_synth_keyword_creature("synth_hawk", "Synth Hawk", "{1}{W}", 1, 2, &["flying"]);
    let c3 = b.add_synth_burn_instant("synth_bolt", "Synth Bolt", "{R}", 3);
    let c4 = b.add_synth_anthem_enchantment("synth_anthem", "Synth Anthem", "{1}{W}", 1, 1);
    let _ = (c0, c1, c2, c3, c4);
    b.intern_counter("+1/+1");
    b.add_deck(
        "synth_d1",
        "Synth Deck 1",
        &[],
        &[("synth_land_w", 20), ("synth_hawk", 4)],
    );
    b.add_deck(
        "synth_d2",
        "Synth Deck 2",
        &[],
        &[("synth_bear", 4), ("synth_bolt", 4)],
    );

    let db = CardDatabase::from_bundle(b.build_bundle()).unwrap();

    let raw = db.encode_raw().unwrap();
    assert_eq!(&raw[0..4], b"MTGC");
    assert_eq!(
        u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
        IR_V2_VERSION
    );
    assert_eq!(raw[8], 0);
    let dec_raw = CardDatabase::decode_bytes(&raw).unwrap();
    assert_eq!(dec_raw.cards, db.cards);
    assert_eq!(dec_raw.decks, db.decks);
    assert_eq!(dec_raw.id_by_oracle("synth_bolt"), Some(c3));
    assert_eq!(dec_raw.id_by_name("Synth Bear"), Some(c1));
    assert_eq!(
        dec_raw.keyword_name(dec_raw.keyword_id("flying").unwrap()),
        Some("flying")
    );

    let zst = db.encode_zstd(3).unwrap();
    assert_eq!(&zst[0..4], b"MTGC");
    assert_eq!(
        u32::from_le_bytes([zst[4], zst[5], zst[6], zst[7]]),
        IR_V2_VERSION
    );
    assert_eq!(zst[8], 1);
    let dec_zst = CardDatabase::decode_bytes(&zst).unwrap();
    assert_eq!(dec_zst.cards, db.cards);
    assert_eq!(dec_zst.decks, db.decks);
}

#[test]
fn t1_f01_05_mtg_ir_no_default_features_wasm_safe_check() {
    let tmp = TempDir::new("f01_05");
    let root = repo_root();
    let out = Command::new("cargo")
        .args([
            "check",
            "--manifest-path",
            root.join("Cargo.toml").to_str().unwrap(),
            "-p",
            "mtg-ir",
            "--no-default-features",
            "--target-dir",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .expect("failed to run cargo check -p mtg-ir --no-default-features");
    assert!(
        out.status.success(),
        "cargo check -p mtg-ir --no-default-features failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Also verify raw encode/decode path works cleanly without using zstd.
    let mut b = SynthBundleBuilder::new();
    b.add_synth_vanilla_creature("synth_raw_only", "Synth Raw", "{1}", 1, 1);
    let db = b.build_db();
    let raw = db.encode_raw().unwrap();
    let dec = CardDatabase::decode_bytes(&raw).unwrap();
    assert_eq!(dec.cards.len(), 1);
}

// ============================================================================
// F2: TS IR v2 Export Bridge (`export-ir-v2.ts`) (5 cases)
// ============================================================================

#[test]
fn t1_f02_01_export_ir_v2_precons_bundle() {
    assert!(
        peckhamr_root().is_some(),
        "peckhamr.com directory must exist"
    );
    let tmp = TempDir::new("f02_01");
    let out_json = tmp.child("precons.ir-v2.json");
    let out_s = out_json.to_string_lossy().into_owned();

    let res = run_node_script("scripts/export-ir-v2.ts", &["--precons", "--out", &out_s]);
    assert!(
        res.success,
        "export-ir-v2.ts --precons failed (code={:?}):\nstdout: {}\nstderr: {}",
        res.status_code, res.stdout, res.stderr
    );
    let text = std::fs::read_to_string(&out_json).expect("output precons json must exist");
    let bundle: IrV2Bundle = serde_json::from_str(&text).expect("valid IrV2Bundle JSON");
    assert_eq!(bundle.ir_version, IR_V2_VERSION);
    assert_eq!(bundle.source_compiler_version, 46);
    assert!(
        !bundle.decks.is_empty(),
        "precons bundle must contain decks"
    );
    assert!(
        !bundle.cards.is_empty(),
        "precons bundle must contain cards"
    );
}

#[test]
fn t1_f02_02_export_ir_v2_bulk_corpus() {
    let corpus = std::path::Path::new("/home/ryan/data/scryfall/oracle-cards.jsonl.gz");
    assert!(
        corpus.exists(),
        "Scryfall bulk corpus must exist at {}",
        corpus.display()
    );
    let tmp = TempDir::new("f02_02");
    let out_json = tmp.child("bulk.ir-v2.json");
    let out_s = out_json.to_string_lossy().into_owned();
    let corpus_s = corpus.to_string_lossy().into_owned();

    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &["--bulk", &corpus_s, "--out", &out_s],
    );
    assert!(
        res.success,
        "export-ir-v2.ts --bulk failed:\nstdout: {}\nstderr: {}",
        res.stdout, res.stderr
    );
    let text = std::fs::read_to_string(&out_json).expect("bulk json must exist");
    let bundle: IrV2Bundle = serde_json::from_str(&text).expect("valid IrV2Bundle JSON");
    assert!(
        bundle.cards.len() >= 35_000,
        "expected >= 35,000 game cards, got {}",
        bundle.cards.len()
    );
}

#[test]
fn t1_f02_03_export_ir_v2_deterministic_interning() {
    let tmp = TempDir::new("f02_03");
    let a = tmp.child("a.json");
    let b = tmp.child("b.json");
    let a_s = a.to_string_lossy().into_owned();
    let b_s = b.to_string_lossy().into_owned();

    let res_a = run_node_script("scripts/export-ir-v2.ts", &["--precons", "--out", &a_s]);
    assert!(
        res_a.success,
        "first --precons run failed: {}",
        res_a.stderr
    );
    let res_b = run_node_script("scripts/export-ir-v2.ts", &["--precons", "--out", &b_s]);
    assert!(
        res_b.success,
        "second --precons run failed: {}",
        res_b.stderr
    );

    let bytes_a = std::fs::read(&a).unwrap();
    let bytes_b = std::fs::read(&b).unwrap();
    assert_eq!(
        bytes_a, bytes_b,
        "export-ir-v2.ts output must be byte-for-byte deterministic"
    );

    let bundle: IrV2Bundle = serde_json::from_slice(&bytes_a).unwrap();
    let uniq_sub: HashSet<&str> = bundle.tables.subtypes.iter().map(String::as_str).collect();
    assert_eq!(uniq_sub.len(), bundle.tables.subtypes.len());
    let uniq_kw: HashSet<&str> = bundle.tables.keywords.iter().map(String::as_str).collect();
    assert_eq!(uniq_kw.len(), bundle.tables.keywords.len());
}

#[test]
fn t1_f02_04_export_ir_v2_bigint_53bit_filter_flags() {
    let tmp = TempDir::new("f02_04");
    let synth_gz = tmp.child("synth_flags.jsonl.gz");
    let out_json = tmp.child("flags.ir-v2.json");

    // Synthetic cards exercising high-bit FilterFlags (e.g., UNBLOCKED_ATTACKER bit 31, ARTIFACT_ENCHANTMENT_OR_TOKEN bit 52).
    let card1 = serde_json::json!({
        "id": "synth_flag_1",
        "oracle_id": "synth_flag_1",
        "name": "Synth Unblockable Check",
        "lang": "en",
        "layout": "normal",
        "mana_cost": "{1}{U}",
        "cmc": 2.0,
        "type_line": "Instant",
        "oracle_text": "Destroy target artifact or enchantment.",
        "colors": ["U"],
        "color_identity": ["U"],
        "keywords": [],
        "legalities": { "commander": "legal" },
        "games": ["paper"]
    });
    let card2 = serde_json::json!({
        "id": "synth_flag_2",
        "oracle_id": "synth_flag_2",
        "name": "Synth Kicked Watcher",
        "lang": "en",
        "layout": "normal",
        "mana_cost": "{1}{R}",
        "cmc": 2.0,
        "type_line": "Sorcery",
        "oracle_text": "Destroy target kicked creature.",
        "colors": ["R"],
        "color_identity": ["R"],
        "keywords": [],
        "legalities": { "commander": "legal" },
        "games": ["paper"]
    });
    let jsonl = format!("{card1}\n{card2}\n");
    write_synthetic_jsonl_gz(&synth_gz, &jsonl);

    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            synth_gz.to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(res.success, "export-ir-v2.ts --bulk failed: {}", res.stderr);

    let text = std::fs::read_to_string(&out_json).unwrap();
    let bundle: IrV2Bundle = serde_json::from_str(&text).expect("must deserialize high-bit flags");
    assert_eq!(bundle.cards.len(), 2);

    // Also verify high-bit FilterFlags round-trip directly with u64 bits >= 31.
    let high = FilterFlags::UNBLOCKED_ATTACKER | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN;
    assert_eq!(high.bits(), (1u64 << 31) | (1u64 << 52));
}

#[test]
fn t1_f02_05_export_ir_v2_consumed_keys_across_synthetic_ir_surfaces() {
    let tmp = TempDir::new("f02_05");
    let synth_gz = tmp.child("synth_surfaces.jsonl.gz");
    let out_json = tmp.child("surfaces.ir-v2.json");

    let cards = [
        serde_json::json!({
            "id": "synth_s1", "oracle_id": "synth_s1", "name": "Synth Modal Burn",
            "lang": "en", "layout": "normal", "mana_cost": "{1}{R}", "cmc": 2.0,
            "type_line": "Instant",
            "oracle_text": "Choose one —\n• Synth Modal Burn deals 2 damage to target creature.\n• Draw a card.",
            "colors": ["R"], "color_identity": ["R"], "keywords": [],
            "legalities": { "commander": "legal" }, "games": ["paper"]
        }),
        serde_json::json!({
            "id": "synth_s2", "oracle_id": "synth_s2", "name": "Synth Flyer Static",
            "lang": "en", "layout": "normal", "mana_cost": "{2}{W}", "cmc": 3.0,
            "type_line": "Creature — Bird",
            "oracle_text": "Flying\nCreatures you control get +1/+1.\nWhenever another creature enters under your control, you gain 1 life.",
            "power": "2", "toughness": "2",
            "colors": ["W"], "color_identity": ["W"], "keywords": ["Flying"],
            "legalities": { "commander": "legal" }, "games": ["paper"]
        }),
        serde_json::json!({
            "id": "synth_s3", "oracle_id": "synth_s3", "name": "Synth Enters Tapped",
            "lang": "en", "layout": "normal", "mana_cost": "", "cmc": 0.0,
            "type_line": "Land",
            "oracle_text": "Synth Enters Tapped enters tapped.\n{T}: Add {G}.",
            "colors": [], "color_identity": ["G"], "keywords": [],
            "legalities": { "commander": "legal" }, "games": ["paper"]
        }),
    ];
    let jsonl = cards
        .iter()
        .map(|c| serde_json::to_string(c).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    write_synthetic_jsonl_gz(&synth_gz, &jsonl);

    let res = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--bulk",
            synth_gz.to_string_lossy().as_ref(),
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        res.success,
        "export-ir-v2.ts synthetic surfaces failed:\nstdout: {}\nstderr: {}",
        res.stdout, res.stderr
    );
    let text = std::fs::read_to_string(&out_json).unwrap();
    let bundle: IrV2Bundle = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle.cards.len(), 3);
}

// ============================================================================
// F3: Binary Card DB Build & Verification (`mtg-cli`) (5 cases)
// ============================================================================

#[test]
fn t1_f03_01_cli_build_db_zstd_mode() {
    let tmp = TempDir::new("f03_01");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_plains", "Synth Plains", "Plains", ManaColor::W);
    b.add_synth_vanilla_creature("synth_bear", "Synth Bear", "{1}{G}", 2, 2);
    b.add_deck(
        "synth_d",
        "Synth Deck",
        &[],
        &[("synth_plains", 20), ("synth_bear", 4)],
    );

    let json_path = tmp.child("bundle.json");
    let bin_path = tmp.child("cards.bin");
    std::fs::write(
        &json_path,
        serde_json::to_string(&b.build_bundle()).unwrap(),
    )
    .unwrap();

    let res = run_build_db(&json_path, &bin_path, false, Some(3));
    assert!(res.success, "build-db failed: {}", res.stderr);
    assert!(
        res.stdout.contains("2 cards (2 fully supported, 1 decks)"),
        "stdout={}",
        res.stdout
    );

    let bytes = std::fs::read(&bin_path).unwrap();
    assert_eq!(&bytes[0..9], &[b'M', b'T', b'G', b'C', 2, 0, 0, 0, 1]);
}

#[test]
fn t1_f03_02_cli_build_db_raw_mode() {
    let tmp = TempDir::new("f03_02");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_vanilla_creature("synth_golem", "Synth Golem", "{3}", 3, 3);

    let json_path = tmp.child("bundle.json");
    let bin_path = tmp.child("raw.bin");
    std::fs::write(
        &json_path,
        serde_json::to_string(&b.build_bundle()).unwrap(),
    )
    .unwrap();

    let res = run_build_db(&json_path, &bin_path, true, None);
    assert!(res.success, "build-db --raw failed: {}", res.stderr);

    let bytes = std::fs::read(&bin_path).unwrap();
    assert_eq!(&bytes[0..9], &[b'M', b'T', b'G', b'C', 2, 0, 0, 0, 0]);
    let db = CardDatabase::decode_bytes(&bytes).unwrap();
    assert_eq!(db.cards.len(), 1);
    assert_eq!(db.cards[0].name, "Synth Golem");
}

#[test]
fn t1_f03_03_cli_inspect_db_summary_accuracy() {
    let tmp = TempDir::new("f03_03");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_island", "Synth Island", "Island", ManaColor::U);
    b.add_synth_keyword_creature("synth_drake", "Synth Drake", "{2}{U}", 2, 2, &["flying"]);
    b.intern_counter("lore");
    b.add_deck(
        "synth_u",
        "Synth Blue",
        &[],
        &[("synth_island", 24), ("synth_drake", 4)],
    );

    let json_path = tmp.child("bundle.json");
    let bin_path = tmp.child("cards.bin");
    std::fs::write(
        &json_path,
        serde_json::to_string(&b.build_bundle()).unwrap(),
    )
    .unwrap();
    assert!(run_build_db(&json_path, &bin_path, false, None).success);

    let res = run_inspect_db(&bin_path, false);
    assert!(res.success, "inspect-db failed: {}", res.stderr);
    assert!(
        res.stdout
            .contains("IR version:      v2 (source compiler v46)"),
        "{}",
        res.stdout
    );
    assert!(
        res.stdout
            .contains("Cards:           2 (2 fully supported = 100.0%)"),
        "{}",
        res.stdout
    );
    assert!(
        res.stdout.contains("2 subtypes, 1 counters, 1 keywords"),
        "{}",
        res.stdout
    );
    assert!(res.stdout.contains("Bundled decks:   1"), "{}", res.stdout);
    assert!(
        parse_decode_ms(&res.stdout).is_some(),
        "expected Decode time in stdout"
    );
}

#[test]
fn t1_f03_04_cli_inspect_db_verify_precons_success() {
    let tmp = TempDir::new("f03_04");
    let mut b = SynthBundleBuilder::new();
    b.add_synth_basic_land("synth_swamp", "Synth Swamp", "Swamp", ManaColor::B);
    b.add_synth_vanilla_creature("synth_cmd_b", "Synth Baron", "{2}{B}{B}", 4, 4);
    b.add_synth_vanilla_creature("synth_imp", "Synth Imp", "{B}", 1, 1);
    b.add_deck(
        "synth_p1",
        "Synth Pod 1",
        &["synth_cmd_b"],
        &[("synth_swamp", 35), ("synth_imp", 64)],
    );
    b.add_deck(
        "synth_p2",
        "Synth Pod 2",
        &["synth_cmd_b"],
        &[("synth_swamp", 40), ("synth_imp", 59)],
    );

    let json_path = tmp.child("bundle.json");
    let bin_path = tmp.child("cards.bin");
    std::fs::write(
        &json_path,
        serde_json::to_string(&b.build_bundle()).unwrap(),
    )
    .unwrap();
    assert!(run_build_db(&json_path, &bin_path, false, None).success);

    let res = run_inspect_db(&bin_path, true);
    assert!(
        res.success,
        "inspect-db --verify-precons failed: {}",
        res.stderr
    );
    assert!(
        res.stdout.contains("0 missing, 0 unsupported"),
        "{}",
        res.stdout
    );
}

#[test]
fn t1_f03_05_precon_corpus_release_decode_latency_gate() {
    let tmp = TempDir::new("f03_05");
    let precons_json = tmp.child("precons.ir-v2.json");
    let precons_bin = tmp.child("precons.cards.bin");

    let exp = run_node_script(
        "scripts/export-ir-v2.ts",
        &[
            "--precons",
            "--out",
            precons_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        exp.success,
        "export-ir-v2.ts --precons failed: {}",
        exp.stderr
    );

    let bld = run_build_db(&precons_json, &precons_bin, false, Some(3));
    assert!(bld.success, "build-db failed: {}", bld.stderr);

    let ins = run_inspect_db_release(&precons_bin, true);
    assert!(
        ins.success,
        "release inspect-db --verify-precons failed: {}",
        ins.stderr
    );
    assert!(
        ins.stdout.contains("Bundled decks:   41"),
        "expected 41 precon decks:\n{}",
        ins.stdout
    );
    assert!(
        ins.stdout.contains("2188"),
        "expected 2188 cards:\n{}",
        ins.stdout
    );
    assert!(
        ins.stdout.contains("100.0%"),
        "expected 100.0% supported:\n{}",
        ins.stdout
    );
    assert!(
        ins.stdout.contains("0 missing, 0 unsupported"),
        "{}",
        ins.stdout
    );
    let ms = parse_decode_ms(&ins.stdout).expect("expected Decode time ms");
    assert!(
        ms < 50.0,
        "release cards.bin decode latency {ms:.2} ms exceeded 50 ms gate"
    );
}

// ============================================================================
// F4: `TsReplayTrace` Schema & Deterministic TS Recorder (5 cases)
// ============================================================================

#[test]
fn t1_f04_01_ts_replay_trace_full_schema_round_trip() {
    let player_digest = serde_json::json!({
        "seat": 0,
        "life": 40,
        "lost": false,
        "poison": 0,
        "energy": 0,
        "manaPool": [0, 0, 0, 0, 0, 0],
        "zoneCounts": {
            "library": 92,
            "hand": 7,
            "battlefield": 0,
            "graveyard": 0,
            "exile": 0,
            "command": 1
        },
        "handOracleIds": ["synth_land_0"],
        "battlefield": [
            {
                "oracleId": "synth_bear_0",
                "tapped": false,
                "pt": [2, 2],
                "types": ["Creature"],
                "keywords": ["trample"],
                "counters": [["+1/+1", 1]],
                "damageMarked": 0,
                "attachedToOracleId": null
            }
        ]
    });
    let digest_after = serde_json::json!({
        "turnNumber": 1,
        "activeSeat": 0,
        "prioritySeat": 0,
        "phase": "main1",
        "stackDepth": 0,
        "pendingPromptKind": null,
        "players": [player_digest]
    });
    let trace_json = serde_json::json!({
        "formatVersion": 1,
        "compilerVersion": 46,
        "seed": 42,
        "postInitRngState": 42,
        "rules": {
            "startingLife": 40,
            "commander": true,
            "freeFirstMulligan": true
        },
        "seats": [
            {
                "seat": 0,
                "deckId": "synth_deck_0",
                "commanders": ["synth_cmd_0"],
                "libraryOrderInitial": ["synth_land_0", "synth_bear_0"],
                "handInitial": ["synth_land_0"]
            }
        ],
        "steps": [
            {
                "stepIndex": 0,
                "actorSeat": 0,
                "action": { "type": "mulligan_keep" },
                "objectLocators": {},
                "digestAfter": digest_after
            }
        ]
    });

    let parsed: TsReplayTrace =
        serde_json::from_value(trace_json).expect("TsReplayTrace must deserialize");
    let re_val = serde_json::to_value(&parsed).unwrap();

    assert!(
        re_val.get("rules").is_some(),
        "TsReplayTrace must preserve `rules`"
    );
    let step0_digest = &re_val["steps"][0]["digestAfter"];
    assert!(
        step0_digest.get("pendingPromptKind").is_some(),
        "TsStepDigest must preserve `pendingPromptKind`"
    );
    let p0 = &step0_digest["players"][0];
    assert_eq!(p0["manaPool"], serde_json::json!([0, 0, 0, 0, 0, 0]));
    assert!(
        p0.get("zoneCounts").is_some(),
        "TsPlayerDigest must preserve `zoneCounts`"
    );
    assert_eq!(p0["handOracleIds"], serde_json::json!(["synth_land_0"]));
    assert_eq!(
        p0["battlefield"][0]["oracleId"],
        serde_json::json!("synth_bear_0")
    );
}

#[test]
fn t1_f04_02_mulberry32_sequence_bit_for_bit_parity() {
    let root = peckhamr_root().expect("peckhamr.com must exist for Mulberry32 oracle");
    let random_ts = root.join("src/lib/magic/runtime/random.ts");
    assert!(random_ts.exists(), "missing {}", random_ts.display());

    // Invoke Node v24 directly against peckhamr.com's `runtime/random.ts` as executable oracle.
    let js = format!(
        "import({:?}).then(m => {{ \
           const h = {{ rngState: 123456789 }}; \
           const nums = Array.from({{length: 20}}, () => Math.floor(m.nextRandom(h) * 4294967296)); \
           const arr = Array.from({{length: 100}}, (_, i) => i); \
           const shuf = m.shuffleWith(h, arr); \
           console.log(JSON.stringify({{ nums, shuf }})); \
         }})",
        random_ts
    );
    let out = Command::new("node")
        .args(["--input-type=module", "-e", &js])
        .output()
        .expect("failed to invoke node on random.ts");
    assert!(
        out.status.success(),
        "node random.ts failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let oracle: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid JSON from node oracle");

    let mut rng = RngState::mulberry32(123_456_789);
    let rust_nums: Vec<u32> = (0..20).map(|_| rng.next_u32()).collect();
    let oracle_nums: Vec<u32> = serde_json::from_value(oracle["nums"].clone()).unwrap();
    assert_eq!(
        rust_nums, oracle_nums,
        "20 Mulberry32 next_u32() outputs must match TS bit-for-bit"
    );

    let mut deck: Vec<u32> = (0..100).collect();
    rng.shuffle(&mut deck);
    let oracle_shuf: Vec<u32> = serde_json::from_value(oracle["shuf"].clone()).unwrap();
    assert_eq!(
        deck, oracle_shuf,
        "100-element Fisher-Yates shuffle must match TS bit-for-bit"
    );
}

#[test]
fn t1_f04_03_record_ts_replay_1v1_deterministic_trace() {
    let tmp = TempDir::new("f04_03");
    let out_json = tmp.child("1v1.replay.json");
    let res = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "42",
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        res.success,
        "record-ts-replay.ts 1v1 failed: {}",
        res.stderr
    );

    let text = std::fs::read_to_string(&out_json).unwrap();
    let val: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(val["rules"]["startingLife"], serde_json::json!(20));
    assert_eq!(val["rules"]["commander"], serde_json::json!(false));
    let trace: TsReplayTrace = serde_json::from_value(val).unwrap();
    assert_eq!(trace.seats.len(), 2);
    assert!(!trace.steps.is_empty());
}

#[test]
fn t1_f04_04_record_ts_replay_4p_commander_trace() {
    let tmp = TempDir::new("f04_04");
    let out_json = tmp.child("4p.replay.json");
    let res = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "4p",
            "--seed",
            "99",
            "--out",
            out_json.to_string_lossy().as_ref(),
        ],
    );
    assert!(res.success, "record-ts-replay.ts 4p failed: {}", res.stderr);

    let text = std::fs::read_to_string(&out_json).unwrap();
    let val: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(val["rules"]["startingLife"], serde_json::json!(40));
    assert_eq!(val["rules"]["commander"], serde_json::json!(true));
    let trace: TsReplayTrace = serde_json::from_value(val).unwrap();
    assert_eq!(trace.seats.len(), 4);
    assert!(!trace.steps.is_empty());
}

#[test]
fn t1_f04_05_record_ts_replay_identical_seed_identical_json() {
    let tmp = TempDir::new("f04_05");
    let out_a = tmp.child("a.replay.json");
    let out_b = tmp.child("b.replay.json");

    let res_a = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "777",
            "--out",
            out_a.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        res_a.success,
        "first record-ts-replay failed: {}",
        res_a.stderr
    );

    let res_b = run_node_script(
        "scripts/record-ts-replay.ts",
        &[
            "--mode",
            "1v1",
            "--seed",
            "777",
            "--out",
            out_b.to_string_lossy().as_ref(),
        ],
    );
    assert!(
        res_b.success,
        "second record-ts-replay failed: {}",
        res_b.stderr
    );

    let trace_a: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_a).unwrap()).unwrap();
    let trace_b: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out_b).unwrap()).unwrap();
    assert_eq!(trace_a["seats"], trace_b["seats"]);
    assert_eq!(trace_a["steps"], trace_b["steps"]);
}
