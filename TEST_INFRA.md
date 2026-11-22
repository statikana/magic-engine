# `mtg-engine` E2E Test Infrastructure (`TEST_INFRA.md`)

> **Track:** E2E Testing Track (opaque-box integration tests, isolated from `crates/`).  
> **IP & Boundary Rules:**  
> 1. Zero WotC/Scryfall card data, precon decklists, or artwork URLs anywhere in `e2e/`. All cards, oracle IDs, and decks are **100% synthetic/renamed** (`Synth *` / `synth_*`).  
> 2. Opaque-box isolation: `e2e/` depends solely on the public APIs of `mtg-ir`, `mtg-engine`, and the `mtg-cli` binary (plus standalone `peckhamr.com/scripts/{export-ir-v2,record-ts-replay}.ts` invocations writing strictly to `/tmp/`). Never read internal/private engine fields or bypass `MatchSession`.  
> 3. Workspace isolation: `e2e/` carries an empty `[workspace]` table so `cargo test --manifest-path e2e/Cargo.toml` compiles and runs independently of the root Cargo workspace with zero extra non-workspace crates.

---

## 1. Directory & Package Layout

```text
/home/ryan/statikana/mtg-engine/
├── TEST_INFRA.md                        # This infrastructure & Tier 1–4 specification
├── TEST_READY.md                        # Published by orchestrator after suite verification
└── e2e/
    ├── Cargo.toml                       # Standalone manifest with `[workspace]` isolation
    ├── src/
    │   ├── lib.rs                       # Shared constants & harness re-exports
    │   ├── synth.rs                     # SynthBundleBuilder, SynthCardBuilder & preset archetypes
    │   ├── driver.rs                    # Opaque-box SessionDriver & invariant-checked step runner
    │   ├── assertions.rs                # Redaction, observation seat rotation, determinism & rename invariance
    │   └── cli.rs                       # OnceLock `mtg-cli` builder, Node TS bridge & RAII /tmp/e2e_* TempDir
    └── tests/
        ├── t1_codec_db_cli.rs           # Tier 1 (20 tests): F1–F4 (IR v2 codec, export-ir-v2, mtg-cli, TsReplayTrace)
        ├── t1_engine_features.rs        # Tier 1 (65 tests): F5–F17 (Core rules, MatchSession, diff-test, bench/hygiene)
        ├── t2_boundaries_negative.rs    # Tier 2 (70 tests): F1–F7 & F11–F17 boundaries, stale IDs, invalid inputs
        ├── t2_layers_replace_cr.rs      # Tier 2 (15 tests): F8–F10 CR 613/613.8, CR 614–616 tiers, CR 603 triggers
        ├── t3_cross_feature.rs          # Tier 3 (20 tests): Cross-feature & multi-CR interactions (T3-01..T3-20)
        └── t4_scenarios.rs              # Tier 4 (10 tests): Multi-turn 1v1, 4p Commander, CLI, MCTS, perf stress (T4-01..T4-10)
```

### `e2e/Cargo.toml` & Optional Feature Gates

- `[workspace]` empty table isolates `e2e/` from the root Cargo workspace.
- Only root workspace dependencies are used (`mtg-ir`, `mtg-engine`, `postcard`, `serde`, `serde_json`, `smallvec`).
- Optional feature flags:
  - `m5-session-ext = []`: gates calls to Milestone M2/M5c `MatchSession` methods not present on baseline (`decision_info`, `from_replay`, `snapshot_bytes`, `from_snapshot`) so `e2e/` compiles cleanly on baseline and exercises the full persistence API once M2/M5c lands.
  - `verify-layer-cache = []`: placeholder feature forwarding to `mtg-engine/verify-layer-cache` once Milestone M3 adds it to `crates/mtg-engine/Cargo.toml`.

---

## 2. Verified & Pinned Public API Contract Surface

All `e2e/tests/*.rs` suites interact strictly with:

### 2.1 `mtg-ir` (`crates/mtg-ir/src/`)
- `IR_V2_VERSION: u32` (`= 2`), `CARDS_BIN_MAGIC: [u8; 4]` (`= *b"MTGC"`)
- `InternTables { subtypes, counters, keywords }`, `IrV2Bundle { ir_version, source_compiler_version, tables, cards, decks }` (`#[serde(deny_unknown_fields)]`)
- `CardDatabase`: `from_bundle`, `to_bundle`, `get`, `id_by_oracle`, `id_by_name`, `subtype_id`/`subtype_name`, `counter_id`/`counter_name`, `keyword_id`/`keyword_name`, `fully_supported_count`, `encode_raw`, `encode_zstd`, `decode_bytes`
- `ManaCost::parse`, `ManaCost::mana_value`, `ManaCost::is_empty` (M1a adds `{C/P}` Phyrexian colorless & `generic: u32` for `{1000000}`)
- Bitmasks (unsigned integer JSON & fixed-width `postcard`): `ColorMask(u8)`, `CardTypeMask(u16)`, `SupertypeMask(u8)`, `ZoneMask(u8)`, `FilterFlags(u64)` (**frozen at bits `0..=52`**)
- `ZoneChangeMod::opponent_discard_to_battlefield` (M1a replaces `Option<Option<...>>` with unambiguous struct/enum)

### 2.2 `mtg-engine` (`crates/mtg-engine/src/`)
- `MatchSession`:
  - Baseline: `new`, `state`, `replay_log`, `current_actor`, `legal_actions`, `apply_action`, `apply_macro_action`, `view_model`, `hover_breakdown`, `preview_auto_tap`, `observe`, `determinize`, `validate_deck`
  - Frozen in M2 / implemented in M5c (`#[cfg(feature = "m5-session-ext")]`): `decision_info`, `from_replay`, `snapshot_bytes`, `from_snapshot` (where `GameStateSnapshot` excludes `Arc<CardDatabase>` and `OnceLock<LayerCache>`)
- `GameConfig::{standard_1v1, commander}`, `MulliganRule::London { free_first }`, `RngMode::{Mulberry32Compat, Fast}`
- Library-top convention: `PlayerState.library.last()` is the top of library (reversed after TS-compat descending Fisher-Yates shuffle so `library.last()` matches TS `library[0]`).
- `RngState::{mulberry32, splitmix64}`
- `TsReplayTrace` (`format_version`, `compiler_version`, `seed`, `post_init_rng_state`, `rules`, `seats` with `hand_initial` & `library_order_initial`, `steps` with `object_locators` & full `TsStepDigest`).

### 2.3 `mtg-cli` Binary & External TS Bridge
- `mtg-cli build-db <INPUT_IR_V2_JSON> --out <OUT_CARDS_BIN> [--raw] [--level <1..=19>]`
- `mtg-cli inspect-db <CARDS_BIN> [--verify-precons]`
- `mtg-cli diff-test <CARDS_BIN> <TS_REPLAY_JSON>`
- External TS scripts (invoked via Node v24 into `/tmp/e2e_*`):
  - `peckhamr.com/scripts/export-ir-v2.ts --precons | --bulk <path> --out <path>`
  - `peckhamr.com/scripts/record-ts-replay.ts --mode <1v1|4p> --seed <u32> --out <path>`

---

## 3. Harness Components (`e2e/src/`)

- **`synth.rs`:** `SynthBundleBuilder` + `SynthCardBuilder` + `SynthFaceBuilder`, `.clone_renamed(suffix)` for rename-invariance testing, and preset builders (`add_synth_basic_land`, `add_synth_vanilla_creature`, `add_synth_keyword_creature`, `add_synth_burn_instant`, `add_synth_draw_sorcery`, `add_synth_anthem_enchantment`, `deck_spec`).
- **`driver.rs`:** `SessionDriver` enforcing pre-step actor/legality assertions, `replay_log` monotonicity, `keep_all_hands`, `advance_to`, `macro_step`, and `play_deterministic_random_game`.
- **`assertions.rs`:** `assert_view_model_redaction`, `assert_observation_consistency`, `assert_determinism_identical`, `assert_rename_invariance`, `assert_active_has_legal_actions`.
- **`cli.rs`:** RAII `TempDir` under `/tmp/e2e_*`, `OnceLock` debug/release `mtg-cli` builder, `run_build_db`, `run_inspect_db`, `run_inspect_db_release`, `run_diff_test`, `run_node_script`, and `write_synthetic_jsonl_gz`.

---

## 4. Full Tier 1–4 Coverage Matrix (200 Tests Across F1–F17)

| Feature | Tier 1 (5/feat = 85) | Tier 2 (5/feat = 85) | Tier 3 (20 cross-feature) | Tier 4 (10 E2E scenarios) |
|---|---|---|---|---|
| **F1** `mtg-ir` Schema & Codec | `t1_f01_01..05` (`t1_codec_db_cli.rs`) | `t2_f01_01..05` (`t2_boundaries_negative.rs`) | `t3_14`, `t3_20` | `t4_04`, `t4_09` |
| **F2** `export-ir-v2.ts` Bridge | `t1_f02_01..05` (`t1_codec_db_cli.rs`) | `t2_f02_01..05` (`t2_boundaries_negative.rs`) | `t3_20` | `t4_09` |
| **F3** `build-db` & `inspect-db` | `t1_f03_01..05` (`t1_codec_db_cli.rs`) | `t2_f03_01..05` (`t2_boundaries_negative.rs`) | `t3_14`, `t3_20` | `t4_04`, `t4_09` |
| **F4** `TsReplayTrace` & TS Recorder | `t1_f04_01..05` (`t1_codec_db_cli.rs`) | `t2_f04_01..05` (`t2_boundaries_negative.rs`) | `t3_12`, `t3_19` | `t4_05`, `t4_06`, `t4_09` |
| **F5** `CR 400.7` Zone Funnel, LKI, Phasing | `t1_f05_01..05` (`t1_engine_features.rs`) | `t2_f05_01..05` (`t2_boundaries_negative.rs`) | `t3_02`, `t3_06`, `t3_08`, `t3_11`, `t3_13`, `t3_15` | `t4_01`, `t4_02`, `t4_03`, `t4_07`, `t4_08` |
| **F6** Faces & Commander `CR 903` | `t1_f06_01..05` (`t1_engine_features.rs`) | `t2_f06_01..05` (`t2_boundaries_negative.rs`) | `t3_03`, `t3_09`, `t3_11`, `t3_14`, `t3_17` | `t4_02`, `t4_04`, `t4_08` |
| **F7** `ObjectFilter` & `Amount`/`Condition` | `t1_f07_01..05` (`t1_engine_features.rs`) | `t2_f07_01..05` (`t2_boundaries_negative.rs`) | `t3_11` | `t4_01` |
| **F8** `CR 613`/`613.8` Layers, Cache, Signed P/T | `t1_f08_01..05` (`t1_engine_features.rs`) | `t2_f08_01..05` (`t2_layers_replace_cr.rs`) | `t3_01`, `t3_03`, `t3_05`, `t3_06`, `t3_09`, `t3_10`, `t3_18`, `t3_19` | `t4_01`, `t4_02`, `t4_03`, `t4_10` |
| **F9** `CR 614–616` Replacements & Prevention | `t1_f09_01..05` (`t1_engine_features.rs`) | `t2_f09_01..05` (`t2_layers_replace_cr.rs`) | `t3_01`, `t3_09`, `t3_16` | `t4_02`, `t4_03`, `t4_07` |
| **F10** Atomic Events & `CR 603` Triggers | `t1_f10_01..05` (`t1_engine_features.rs`) | `t2_f10_01..05` (`t2_layers_replace_cr.rs`) | `t3_02`, `t3_06`, `t3_15`, `t3_17` | `t4_01`, `t4_02`, `t4_03` |
| **F11** Mana `CR 106.6`, Auto-Tap, Costs | `t1_f11_01..05` (`t1_engine_features.rs`) | `t2_f11_01..05` (`t2_boundaries_negative.rs`) | `t3_04`, `t3_07` | `t4_01`, `t4_02`, `t4_08` |
| **F12** Casting `CR 601.2`, Targets, 6 Choices | `t1_f12_01..05` (`t1_engine_features.rs`) | `t2_f12_01..05` (`t2_boundaries_negative.rs`) | `t3_04`, `t3_07`, `t3_08` | `t4_01`, `t4_02`, `t4_03`, `t4_07`, `t4_08` |
| **F13** `EffectOp`, Frames, `bind_as`, Keywords *(F13+F14 unified in M5b)* | `t1_f13_01..05` (`t1_engine_features.rs`) | `t2_f13_01..05` (`t2_boundaries_negative.rs`) | `t3_02`, `t3_04`, `t3_08`, `t3_13`, `t3_15`, `t3_16` | `t4_01`, `t4_02`, `t4_03`, `t4_07`, `t4_08` |
| **F14** Turn Loop, Stack, SBAs, Combat *(merged into F13 in `PROJECT.md`)* | `t1_f14_01..05` (`t1_engine_features.rs`) | `t2_f14_01..05` (`t2_boundaries_negative.rs`) | `t3_03`, `t3_05`, `t3_10`, `t3_12`, `t3_16`, `t3_17`, `t3_18` | `t4_01`, `t4_02`, `t4_03`, `t4_05`, `t4_06`, `t4_10` |
| **F15** `MatchSession` Façade, Views, Obs, Det. | `t1_f15_01..05` (`t1_engine_features.rs`) | `t2_f15_01..05` (`t2_boundaries_negative.rs`) | `t3_07`, `t3_10`, `t3_12`, `t3_13`, `t3_14` | `t4_01..06`, `t4_10` |
| **F16** `mtg-cli diff-test` & Divergences | `t1_f16_01..05` (`t1_engine_features.rs`) | `t2_f16_01..05` (`t2_boundaries_negative.rs`) | `t3_19`, `t3_20` | `t4_09` |
| **F17** CR Suite, Rename Inv., Cache & Bench | `t1_f17_01..05` (`t1_engine_features.rs`) | `t2_f17_01..05` (`t2_boundaries_negative.rs`) | `t3_05`, `t3_18` | `t4_06`, `t4_10` |
| **Totals** | **85** | **85** | **20** | **10** |

---

## 5. Standard Execution Commands

> **Baseline vs. Milestone-Gated Classification:** On the pre-implementation baseline (before M1a–M6 land), only baseline-supported schema, synthetic CLI, RNG parity, and stub-surface checks pass (`t1_f01_*`, `t1_f03_01..04`, `t1_f04_02`, `t1_f15_02`, `t1_f17_03`, `t2_f01_*`, `t2_f03_*`, `t2_f04_02..03`, `t2_f12_01`, `t2_f13_02`, `t2_f15_01..03`, `t2_f16_04`, `t2_f17_04`, `t3_14`), plus `t2_f02_01..02` once `export-ir-v2.ts` is present. In particular, `t1_f15_01` asserts `hand_count == 7` (`CR 103.4` opening draw) and therefore requires M5b `turn.rs`, and `t2_f04_04`, `t2_f15_04..05`, `t2_f17_02`, `t2_f17_05` fail cleanly until their corresponding milestones land. Also note `--features m5-session-ext` activates calls to M5 `MatchSession` methods (`snapshot_bytes`, `from_snapshot`, `from_replay`, `decision_info`) and only compiles once M5 lands.

```bash
# Check formatting & clippy on standalone E2E suite
cargo fmt --manifest-path e2e/Cargo.toml -- --check
cargo clippy --manifest-path e2e/Cargo.toml --all-targets -- -D warnings

# Compile all 200 E2E tests
cargo test --manifest-path e2e/Cargo.toml --no-run

# Run Tier 1 codec/DB/CLI slice
cargo test --manifest-path e2e/Cargo.toml --test t1_codec_db_cli

# Run full E2E suite (after M5/M6 land, enable m5-session-ext & verify-layer-cache)
cargo test --manifest-path e2e/Cargo.toml --features m5-session-ext,verify-layer-cache
cargo test --manifest-path e2e/Cargo.toml --release -- t1_f17_04 t1_f17_05 t4_10
```
