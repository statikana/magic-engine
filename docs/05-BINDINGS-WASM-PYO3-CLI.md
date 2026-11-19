# 05 — External Bindings: Narrow Façade, WASM, PyO3 & CLI

## 1. The Cardinal Rule for Multi-Target Maintainability

Supporting Rust crates + WASM (`npm`) + Python (`PyO3`) + CLI + future C-FFI stays low-overhead **only if all bindings wrap one narrow, message-oriented façade (`MatchSession`)** instead of mirroring hundreds of internal Rust structs into JS/Python/C classes.

When you add a new MTG keyword, layer effect, or CR edge case inside `mtg-engine`, **zero lines of code should change** in `mtg-wasm`, `mtg-py`, or `mtg-cli`.

```text
                       ┌────────────────────────────┐
                       │   MatchSession Façade      │  (~12 methods, serde + flat buffers)
                       │  (inside crates/mtg-engine)│
                       └─────────────┬──────────────┘
          ┌──────────────────────────┼──────────────────────────┬───────────────────┐
          ▼                          ▼                          ▼                   ▼
   crates/mtg-wasm            crates/mtg-py              crates/mtg-cli      magic-web/server
  (stateful WasmMatch        (PyMatch + VecEnv          (build-db, diff,     (axum WS actors,
   in WASM linear mem)        + NumPy obs arrays)        tournaments)         ISMCTS on homelab)
```

### The `MatchSession` Façade Surface

| Method | Input | Output | Consumers |
|---|---|---|---|
| `new` | `Arc<CardDatabase>`, `GameConfig`, `Vec<DeckSpec>`, `seed` | `Result<MatchSession, InitError>` | All |
| `decision_info` | — | `DecisionInfo { actor, state_kind, legal_count }` | All |
| `legal_actions` | `PlayerId` | `Vec<Action>` | AI, WASM, Py, CLI |
| `apply_action` | `PlayerId`, `Action` | `Result<StepOutcome, ActionError>` | All |
| `apply_macro_action` | `PlayerId`, `MacroAction` | `Result<StepOutcome, ActionError>` | Web UI, WASM |
| `view_model` | `Option<PlayerId>` *(viewer; `None` = spectator)* | `ClientViewModel` | Web Server, WASM |
| `hover_breakdown` | `Option<PlayerId>`, `ObjectId` | `Option<ContinuousBreakdown>` | Web Server, WASM |
| `preview_auto_tap` | `PlayerId`, `CastRequest` | `AutoTapPreview` | Web Server, WASM |
| `observe` | `PlayerId` | `Observation` | AI, PyO3 |
| `validate_deck` | `&CardDatabase`, `&GameConfig`, `&DeckSpec` | `DeckValidationReport` | Web Server, WASM, CLI |
| `replay_log` / `from_replay` | `ReplayLog` | `ReplayLog` / `MatchSession` | Server, CLI, Tests |
| `snapshot_bytes` / `from_snapshot` | `&[u8]` (`postcard`) | `Vec<u8>` / `MatchSession` | Server, WASM |

---

## 2. `mtg-wasm` — Avoiding Both Classic WASM Traps

### Trap 1: JS ↔ WASM Serialization Overhead
- **Never** pass a full `GameState` from JS into WASM on every hover, render, or action.
- **Solution — Stateful WASM Handle:**
  ```rust
  #[wasm_bindgen]
  pub struct WasmCardDb(Arc<CardDatabase>);

  #[wasm_bindgen]
  pub struct WasmMatch {
      session: MatchSession,
  }
  ```
  1. Browser loads `WasmCardDb::from_bytes(&cards_bin_bytes)` **once** (either a deck-scoped pack of ≤400 cards or the full precompiled `cards.bin`). It stays pinned in WASM linear memory.
  2. `WasmMatch::new(&db, config_json, decks_json, seed)` allocates the match inside WASM linear memory (~20 KB).
  3. On each user click, Svelte sends a tiny `Action` or `MacroAction` JSON (~100 bytes) into WASM and receives a compact `ClientViewModel` (~5–20 KB for current visible board, **not** hidden libraries or full `ScryfallCard` blobs). Crossing the JS↔WASM boundary takes **<0.2 ms**.

### Trap 2: Binary Size & C Toolchain Friction
- **`mtg-compiler` is NEVER linked into `mtg-wasm`.** Only `mtg-ir` (`default-features = false`, pure Rust, no `zstd-sys`) + `mtg-engine` (+ optional single-threaded `mtg-ai` behind a cargo feature) are compiled into `.wasm`.
- Build with `[profile.wasm-release]` (`opt-level = "s"`, `lto = true`) + `wasm-opt -Os` via `wasm-pack build --target web`.
- **Target bundle budget:** **300 KB – 1.0 MB gzipped** (smaller than the current TS `/magic` client chunk of **1.22 MB gzipped**!), and **lazy-loaded** in `magic-web` only when a user opens offline solitaire/goldfish or deck inspection tools.
- **TypeScript definitions:** Generate `.d.ts` automatically via `tsify` / `wasm-bindgen` so external npm consumers get full strict TypeScript types for `ClientViewModel`, `Action`, `MacroAction`, and `DeckValidationReport`.

---

## 3. `mtg-py` (`PyO3` / `maturin`) — Built for High-Throughput AI Training

- Keep `pyo3` behind the optional `extension-module` feature in `crates/mtg-py/Cargo.toml` so `cargo test --workspace` works everywhere without requiring Python development headers installed.
- Expose two primary Python classes:
  1. **`PyCardDb(path)`:** Loads `cards.bin` once into `Arc<CardDatabase>` (shared across all environments).
  2. **`PyMatch`** (single match) and **`PyVecEnv(db, configs, num_envs, num_threads)`:**
     - `step_batch(actions)` releases the Python GIL (`py.allow_threads(...)`), steps all `N` environments in parallel with `rayon`, and writes dense observations + legal-action masks directly into pre-allocated contiguous buffers (zero per-step Python dict allocation!).
     - Built-in opponent stepping: `PyVecEnv` can step heuristic/ISMCTS opponents internally in Rust and only return control to Python when the learning agent has a decision!
- Packaging: `maturin` builds manylinux/macOS/Windows wheels in GitHub Actions and publishes to PyPI.

---

## 4. `mtg-cli` — Internal Workhorse & External Power Tool

`mtg-cli` serves both internal development and external users:

| Subcommand | Purpose | Replaces in `peckhamr.com` |
|---|---|---|
| `mtg-cli build-db <ir-v2.json \| oracle-cards.jsonl.gz> -o cards.bin [--raw \| --zstd]` | Validates (`deny_unknown_fields`) and compiles JSON IR (Phase 0–4) or Scryfall bulk data (Phase 5) into binary `cards.bin`. | New (AOT IR pipeline) |
| `mtg-cli inspect-db cards.bin [--verify-precons]` | Prints load latency, card count, fully-supported %, intern table sizes, and verifies every precon deck resolves cleanly. | [`scripts/magic-coverage.ts`](file:///home/ryan/statikana/peckhamr.com/scripts/magic-coverage.ts) |
| `mtg-cli coverage oracle-cards.jsonl.gz --out COVERAGE.md` *(Phase 5)* | Runs native `mtg-compiler` over the full Scryfall corpus and generates the coverage + top-unparsed-templates report. | [`scripts/magic-coverage.ts`](file:///home/ryan/statikana/peckhamr.com/scripts/magic-coverage.ts) |
| `mtg-cli diff-test <cards.bin> <ts-replay.json>` | Replays TS-recorded matches inside Rust `mtg-engine` (with `Mulberry32`), comparing per-step digests and pinpointing the first divergence. | New (Phase 0–2 oracle) |
| `mtg-cli tournament <cards.bin> --decks ... --agents heuristic,ismcts --games 100` | Multi-threaded headless match runner with win-rate, Elo, steps/sec, and crash/loop reporting. | New (Phase 3 AI bench) |

---

## 5. Future `mtg-ffi` (C/C++) Readiness (Deferred Post-v1)

Because `MatchSession` is stateful and message-oriented, adding `crates/mtg-ffi` after v1.0 stabilizes requires no engine changes—just ~300 lines exposing opaque pointers (`MtgCardDb*`, `MtgMatch*`), byte-slice inputs/outputs (JSON or `postcard` buffers), and `mtg_bytes_free(MtgBytes)`, with C/C++ headers generated by `cbindgen`.
