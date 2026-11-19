# mtg-engine

A deterministic, AI-ready, format-agnostic Magic: The Gathering rules engine, card IR, compiler, and search/AI toolkit written in Rust.

## Transition & Architecture Documentation

Start at **[`docs/README.md`](docs/README.md)** for the complete high-to-low-level guide to transitioning from the TypeScript engine in `peckhamr.com` to this Rust workspace and `magic-web`:

1. [`docs/01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md`](docs/01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md) — Goals, 3-repo split, crate DAG, phased milestones & exit gates
2. [`docs/02-IR-V2-AND-CARD-DATABASE.md`](docs/02-IR-V2-AND-CARD-DATABASE.md) — Card IR v2 design, `cards.bin` binary format, temporary TS exporter bridge
3. [`docs/03-ENGINE-DATA-MODEL-AND-INVARIANTS.md`](docs/03-ENGINE-DATA-MODEL-AND-INVARIANTS.md) — Arena `GameState`, O(1) layer invalidation, CR 613.8 / 614–616 / 400.7 foundations, `Mulberry32` compat RNG
4. [`docs/04-ACTION-PROTOCOL-AND-AI-DESIGN.md`](docs/04-ACTION-PROTOCOL-AND-AI-DESIGN.md) — Sequential low-branching `Action`s, `MacroAction`s, `determinize`, `Observation`, parallel ISMCTS & future neural hooks
5. [`docs/05-BINDINGS-WASM-PYO3-CLI.md`](docs/05-BINDINGS-WASM-PYO3-CLI.md) — `MatchSession` façade, stateful `mtg-wasm`, `mtg-py` `VecEnv`, `mtg-cli`, deferred C-FFI
6. [`docs/06-TS-TO-RUST-MODULE-MAP.md`](docs/06-TS-TO-RUST-MODULE-MAP.md) — File-by-file map of all 97 TS files + exact IR v1 → v2 translation spec
7. [`docs/07-DIFFERENTIAL-TESTING-AND-COMPILER.md`](docs/07-DIFFERENTIAL-TESTING-AND-COMPILER.md) — TS↔Rust differential replay protocol (41/41 precon parity) & native Rust `mtg-compiler` parser plan
8. [`docs/08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md`](docs/08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md) — `magic-web` `axum` WS actors, SQLite event log migration, Svelte 5 UI decoupling, beta soak & `peckhamr.com` cleanup

## Workspace Layout

| Crate | Purpose |
|---|---|
| `crates/mtg-ir` | Pure v2 Card IR types, bitmasks, pre-parsed `ManaCost`, `CardDatabase`, and `postcard`+`zstd` binary codec (`cards.bin`). Zero regex, zero IO. |
| `crates/mtg-engine` | Deterministic, pure-Rust rules engine (arena-indexed `GameState`, mutation-tracked `CR 613` layers with `CR 613.8` dependencies, unified `CR 614–616` replacements, single `CR 400.7` zone funnel including stack, sequential decision-point `Action` API, `determinize`, `Observation`, `ClientViewModel`). |
| `crates/mtg-ai` | Heuristic evaluation + parallel ISMCTS (`rayon`) with pluggable value/policy traits for future neural/RL inference. |
| `crates/mtg-compiler` | Oracle text tokenizer + recursive-descent parser targeting `mtg-ir` v2, plus hash-gated override loading. |
| `crates/mtg-cli` | CLI for building `cards.bin`, inspecting coverage, differential replay verification, and headless AI tournaments. |
| `crates/mtg-wasm` | Stateful `wasm-bindgen` wrapper over `mtg-engine` (no compiler embedded) for browser/npm consumers. |
| `crates/mtg-py` | `PyO3` Python bindings + batched `VecEnv` observations for ML/RL training. |

## Legal / Card Data Separation

This repository contains **no Wizards of the Coast or Scryfall card data, artwork, or flavor text**. Card databases (`cards.bin`) are built locally from user-supplied Scryfall bulk data via `mtg-cli build-db`.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
