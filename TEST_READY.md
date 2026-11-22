# E2E Test Suite Ready

> **Status:** Verified by E2E Testing Track Round-4 Gate (2× Reviewer `APPROVE`, 2× Challenger `APPROVE`, Forensic Auditor `CLEAN`).

## Test Runner
- Command:
  - Baseline / incremental verification: `cargo test --manifest-path e2e/Cargo.toml`
  - Full M5/M6 verification (after M3 `verify-layer-cache` and M5c snapshot/replay land): `cargo test --manifest-path e2e/Cargo.toml --features m5-session-ext,verify-layer-cache`
  - Release performance & stress gate: `cargo test --manifest-path e2e/Cargo.toml --release -- t1_f17_04 t1_f17_05 t4_10`
- Expected: All 200 tests pass with exit code `0` upon completion of Phase 1 (`M7` Final Milestone). Zero WotC/Scryfall IP inside `e2e/` (`Synth *` / `synth_*` only).

## Coverage Summary
| Tier | Count | Description |
|------|------:|-------------|
| 1. Feature Coverage | 85 | 5 tests per feature across F1–F17 (`t1_codec_db_cli.rs`: 20, `t1_engine_features.rs`: 65) |
| 2. Boundary & Corner | 85 | 5 tests per feature (`t2_boundaries_negative.rs`: 70, `t2_layers_replace_cr.rs`: 15) |
| 3. Cross-Feature | 20 | Pairwise CR & multi-feature interactions (`t3_cross_feature.rs`: 20) |
| 4. Real-World Application | 10 | Multi-turn 1v1, 4p Commander, CLI `diff-test`, MCTS, stress (`t4_scenarios.rs`: 10) |
| **Total** | **200** | |

## Feature Checklist
| Feature | Tier 1 | Tier 2 | Tier 3 | Tier 4 |
|---------|:------:|:------:|:------:|:------:|
| **F1** `mtg-ir` Schema & Codec | 5 | 5 | ✓ (`t3_14,t3_20`) | ✓ (`t4_04,t4_09`) |
| **F2** `export-ir-v2.ts` Bridge | 5 | 5 | ✓ (`t3_20`) | ✓ (`t4_09`) |
| **F3** `build-db` & `inspect-db` | 5 | 5 | ✓ (`t3_14,t3_20`) | ✓ (`t4_04,t4_09`) |
| **F4** `TsReplayTrace` & TS Recorder | 5 | 5 | ✓ (`t3_12,t3_19`) | ✓ (`t4_05,t4_06,t4_09`) |
| **F5** `CR 400.7` Zone Funnel, LKI, Phasing | 5 | 5 | ✓ (`t3_02,t3_06,t3_08,t3_11,t3_13,t3_15`) | ✓ (`t4_01..03,t4_07,t4_08`) |
| **F6** Faces & Commander `CR 903` | 5 | 5 | ✓ (`t3_03,t3_09,t3_11,t3_14,t3_17`) | ✓ (`t4_02,t4_04,t4_08`) |
| **F7** `ObjectFilter` & `Amount`/`Condition` | 5 | 5 | ✓ (`t3_11`) | ✓ (`t4_01`) |
| **F8** `CR 613`/`613.8` Layers, Cache, Signed P/T | 5 | 5 | ✓ (`t3_01,t3_03,t3_05,t3_06,t3_09,t3_10,t3_18,t3_19`) | ✓ (`t4_01..03,t4_10`) |
| **F9** `CR 614–616` Replacements & Prevention | 5 | 5 | ✓ (`t3_01,t3_09,t3_16`) | ✓ (`t4_02,t4_03,t4_07`) |
| **F10** Atomic Events & `CR 603` Triggers | 5 | 5 | ✓ (`t3_02,t3_06,t3_15,t3_17`) | ✓ (`t4_01..03`) |
| **F11** Mana `CR 106.6`, Auto-Tap, Costs | 5 | 5 | ✓ (`t3_04,t3_07`) | ✓ (`t4_01,t4_02,t4_08`) |
| **F12** Casting `CR 601.2`, Targets, 6 Choices | 5 | 5 | ✓ (`t3_04,t3_07,t3_08`) | ✓ (`t4_01..03,t4_07,t4_08`) |
| **F13** `EffectOp`, Frames, `bind_as`, Keywords | 5 | 5 | ✓ (`t3_02,t3_04,t3_08,t3_13,t3_15,t3_16`) | ✓ (`t4_01..03,t4_07,t4_08`) |
| **F14** Turn Loop, Stack, SBAs, Combat | 5 | 5 | ✓ (`t3_03,t3_05,t3_10,t3_12,t3_16,t3_17,t3_18`) | ✓ (`t4_01..03,t4_05,t4_06,t4_10`) |
| **F15** `MatchSession` Façade, Views, Obs, Det. | 5 | 5 | ✓ (`t3_07,t3_10,t3_12,t3_13,t3_14`) | ✓ (`t4_01..06,t4_10`) |
| **F16** `mtg-cli diff-test` & Divergences | 5 | 5 | ✓ (`t3_19,t3_20`) | ✓ (`t4_09`) |
| **F17** CR Suite, Rename Inv., Cache & Bench | 5 | 5 | ✓ (`t3_05,t3_18`) | ✓ (`t4_06,t4_10`) |
