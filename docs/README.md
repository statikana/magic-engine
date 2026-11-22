# TypeScript → Rust Transition Documentation (`docs/`)

This documentation suite is the canonical blueprint for transitioning the legacy TypeScript Magic: The Gathering engine (`peckhamr.com/src/lib/magic/`) into a standalone open-source Rust workspace (`mtg-engine`) and a decoupled web application (`magic-web`).

The documents are ordered strictly from **high level (strategy & repo topology)** down to **low level (byte-level codecs, file-by-file module maps, differential testing protocols, and production cutover checklists)**.

## Reading Order

| # | Document | Abstraction Level | What It Covers |
|---|---|---|---|
| **01** | [`01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md`](./01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md) | **Highest (System & Repos)** | Why Rust, locked decisions, 3-repo split (`peckhamr.com` / `mtg-engine` / `magic-web`), licensing & IP separation, Cargo workspace DAG, phased sequencing & exit gates. |
| **02** | [`02-IR-V2-AND-CARD-DATABASE.md`](./02-IR-V2-AND-CARD-DATABASE.md) | **High-Mid (Data & IR)** | The IR wall, structural cleanups from TS `ir.ts` v1 → Rust `mtg-ir` v2, bitmasks & pre-parsed `ManaCost`, `CardDatabase`, `cards.bin` binary format, and the temporary TS exporter bridge. |
| **03** | [`03-ENGINE-DATA-MODEL-AND-INVARIANTS.md`](./03-ENGINE-DATA-MODEL-AND-INVARIANTS.md) | **Mid (Core Engine)** | Arena `GameState`, generational `ObjectId`, mutation-tracked `O(1)` layer cache (eliminating TS's 50% CPU fingerprint hotspot), `CR 613.8` dependencies, unified `CR 614–616` replacements, stack-in-zone-funnel (`CR 400.7`), `Option<Pt>`, format-agnostic `GameConfig`, and `Mulberry32` compat RNG. |
| **04** | [`04-ACTION-PROTOCOL-AND-AI-DESIGN.md`](./04-ACTION-PROTOCOL-AND-AI-DESIGN.md) | **Mid (Actions & AI)** | Decomposing TS `GameActionPayload` into sequential low-branching `Action` steps + UI `MacroAction` adapters, the 6 canonical `ChoiceRequest` shapes, `determinize(viewer, rng)`, `Observation` tensors, parallel ISMCTS (`rayon`), and future neural/RL hooks. |
| **05** | [`05-BINDINGS-WASM-PYO3-CLI.md`](./05-BINDINGS-WASM-PYO3-CLI.md) | **Mid-Low (Bindings)** | The narrow message-oriented `MatchSession` façade shared by `mtg-wasm` (stateful handle avoiding serialization & compiler-size traps), `mtg-py` (`PyO3` `VecEnv`), `mtg-cli`, and future `mtg-ffi` (C/C++). |
| **06** | [`06-TS-TO-RUST-MODULE-MAP.md`](./06-TS-TO-RUST-MODULE-MAP.md) | **Low (Codebase Map)** | Complete file-by-file mapping of all 97 non-test TS modules (`runtime/`, `compiler/`, top-level) + 38 overrides + 1,920 tests in `peckhamr.com` to target Rust modules, plus exact IR v1 → v2 translation & zero-silent-drop rules. |
| **07** | [`07-DIFFERENTIAL-TESTING-AND-COMPILER.md`](./07-DIFFERENTIAL-TESTING-AND-COMPILER.md) | **Low (Verification & Compiler)** | Exact TS ↔ Rust differential testing protocol (replay schema, per-step state digests, triage rules, 41/41 precon parity gate) and Phase 5 native Rust `mtg-compiler` parser architecture (replacing 5,354 JS regexes). |
| **08** | [`08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md`](./08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md) | **Low (Web, DB & Ops)** | `magic-web` repo structure, `axum` WebSocket actors, append-only SQLite event log + snapshots, migrating `MagicUsers`/`MagicDecks`/`MagicSessions` intact, decoupling the 26k-LOC Svelte 5 UI (shrinking 5.1 MB JS → ~200 KB), Cloudflare + `cloudflared` ops, beta soak, and pruning Magic from `peckhamr.com`. |
| **09** | [`09-CHALLENGING-RULES-TEST-CATALOG.md`](./09-CHALLENGING-RULES-TEST-CATALOG.md) | **Low (CR Test Spec)** | Comprehensive 47-item challenging MTG rules catalog + 12 flagship stress tests (*Humility*+*Opalescence*, *Blood Moon*+*Urborg*, `CR 704.8` counter annihilation, `CR 903.9a/b` Commander split, `CR 510.1c` combat damage, phasing, copies, split/adventure, rule-modifying effects) mapped to milestones `M2–M7` with synthetic-card assertions. |

## Current Baseline Snapshot (Source TS Repo)

Reference numbers measured from the legacy TypeScript source (`COMPILER_VERSION = 46`, commit `aa82f16`):

- **Production TS engine/compiler/runtime:** ~213,639 LOC across 97 files (`runtime/`: 65 files / 126k LOC; `compiler/`: 19 files / 60k LOC with 5,354 regex literals; top-level: 13 files / 27k LOC).
- **UI:** ~26,234 LOC across 37 `.svelte`/`.ts` files (currently bundling 5.1 MB minified / 1.22 MB gzipped JS to browsers due to direct engine/compiler imports).
- **Tests:** 134 test files, 1,920 test cases (~144,261 LOC), plus 10 architecture ratchets all locked at `0`.
- **Card Coverage:** 41/41 Commander precon decks (`2,188/2,188 = 100.0%`, including 38 hash-gated JSON overrides) and `25,417/35,062 = 72.5%` of the full Scryfall game corpus (`76.6%` ignoring statics, `0` compiler errors).
- **Measured TS Bottlenecks (4-player Commander profile):** ~669 KB JSON `GameState` (~20 ms/request for parse + `JSON.parse(JSON.stringify)` redaction + stringify); ~50% of V8 CPU spent validating O(N) layer-cache fingerprints (`dynamicFingerprintMatches` + `sumNonStandardPtCountersInLayer` + `getBoardSnapshot`); ~14% re-parsing card faces/type lines; ~8.5 s for 400 warm bot steps on Turn 6.
