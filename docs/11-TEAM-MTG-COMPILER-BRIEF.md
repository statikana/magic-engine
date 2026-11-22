# 11. Parallel Team Brief — Native Rust `mtg-compiler` (Lexer, Parser & 35k-Card Diff Harness)

> **Audience:** Autonomous agent team building the native Rust `mtg-compiler` crate in parallel with the core `mtg-engine` (Milestones M3–M7) and `magic-web` teams.
> **Target Working Directory:** `/home/ryan/statikana/mtg-compiler-work` (isolated workspace structured to drop cleanly into `/home/ryan/statikana/mtg-engine/crates/mtg-compiler/` once M7 merges).
> **Status:** Ready to start immediately (depends **only** on locked `mtg-ir` from M1a).

---

## 1. Mission

Build `mtg-compiler`, the native Rust Magic: The Gathering Oracle-text compiler targeting `mtg-ir` v2, replacing the 60k-LOC / 5,354-regex TypeScript compiler in `/home/ryan/statikana/peckhamr.com/src/lib/magic/compiler/` with a fast, deterministic single-pass lexer + recursive-descent parser verified against the TS IR v2 oracle across all 41 Commander precons and 35,062 Scryfall cards.

---

## 2. Context & Required Reading

Read these documents and files before writing code:
1. [`02-IR-V2-AND-CARD-DATABASE.md`](./02-IR-V2-AND-CARD-DATABASE.md) — IR wall invariants, `mtg-ir` v2 schema, `CardDatabase`, `InternTables`
2. [`06-TS-TO-RUST-MODULE-MAP.md`](./06-TS-TO-RUST-MODULE-MAP.md) — IR v1 → v2 semantics & `compiler/` inventory
3. [`07-DIFFERENTIAL-TESTING-AND-COMPILER.md`](./07-DIFFERENTIAL-TESTING-AND-COMPILER.md) §2 — native Rust compiler architecture (`normalize.rs` → `lexer.rs` → recursive-descent parser → `overrides.rs`) and exact corpus verification bar
4. `/home/ryan/statikana/mtg-engine/crates/mtg-ir/src/*.rs` — the **LOCKED** IR v2 target types (M1a complete)
5. Reference TS compiler & ground-truth oracle (**READ-ONLY**):
   - `/home/ryan/statikana/peckhamr.com/src/lib/magic/compiler/*.ts`
   - `/home/ryan/statikana/peckhamr.com/src/lib/magic/keywords.ts`
   - `/home/ryan/statikana/peckhamr.com/src/lib/magic/overrides/*.json` (38 hash-gated overrides)
   - `/home/ryan/statikana/peckhamr.com/scripts/export-ir-v2.ts` (ground-truth IR v2 generator for all 35,062 cards)
   - `/home/ryan/data/scryfall/oracle-cards.jsonl.gz` (local Scryfall bulk corpus — **NEVER commit to git**)

---

## 3. Critical Parallel-Work Safety Rules

1. **NEVER edit files inside `/home/ryan/statikana/mtg-engine` or `/home/ryan/statikana/peckhamr.com`.** Another agent team is actively working on Milestones M3–M7 in `/home/ryan/statikana/mtg-engine` with uncommitted changes.
2. **Isolated Workspace:** Work inside `/home/ryan/statikana/mtg-compiler-work` (structured identically to `crates/mtg-compiler/` so it drops cleanly into `mtg-engine/crates/mtg-compiler/` once M7 merges). Reference `/home/ryan/statikana/mtg-engine/crates/mtg-ir` as a read-only path dependency (or snapshot its locked `Cargo.toml` + `src/` if needed to avoid any `Cargo.lock`/`target/` contention).
3. **NO Regex Porting:** **DO NOT port the 5,354 JS regexes to Rust `regex` / `fancy-regex`!** Build a proper **single-pass tokenizer (`lexer.rs`) + recursive-descent / combinator parser (`winnow` or hand-written RD)** with zero lookaround/backreference regexes.
4. **Uphold the IR Wall, Rename Invariance & IP Hygiene:**
   - Never branch on card names anywhere in the parser (only `normalize.rs` replaces self-references with `~`).
   - Every non-reminder character of Oracle text must be either consumed into an IR node with an accurate `Span` or emitted as `AbilityDef::Unsupported` (zero silent drops).
   - Hash-gated overrides (`overrides.rs`) must preserve `< 5%` restraint, `fully_supported == true`, zero `UnsupportedAbility` inside overrides, and fall back automatically on `text_hash` mismatch.
   - Never commit Scryfall bulk files, precon decklists, or artwork into git; use synthetic/renamed cards in unit tests.

---

## 4. Target Pipeline Architecture

```text
Raw Scryfall Face
  -> normalize.rs   (balanced reminder-text stripping, ~ self-ref & short-name norm, text_hash, paragraph/bullet/Saga/Class splitting)
  -> overrides.rs   (hash-gated IR v2 override lookup before lexing)
  -> lexer.rs       (spanned token stream: ManaSymbol, {T}/{Q}/{E}/{P}, numbers/number-words, +/- P/T, ~, keywords, card types, subtypes, zones, colors, punctuation, quotes)
  -> parser/
       ├── cost.rs        (CostDef, CostDelta, ManaSpendingRestriction)
       ├── filter.rs      (ObjectFilter bitmasks + ObjectPredicate, PlayerFilter, TargetSpec)
       ├── value.rs       (Amount, Condition, Ref, PlayerRef)
       ├── effect.rs      (EffectStep + typed EffectOp, bindings, delayed & reflexive triggers)
       ├── trigger.rs     (TriggerDef, EventFilter + EventDetail, intervening-if)
       ├── static_def.rs  (StaticDef, LayerMod, RuleMod, CombatRuleSpec, ReplacementDef)
       ├── keyword.rs     (declarative KeywordSpec table covering all 214+ implemented CR 702 keywords)
       └── card.rs        (FaceDef, CardDef, multi-face/Split/DFC/Adventure/Room/Saga/Class/Case/Battle/Prototype/Cleave/Gift)
  -> support.rs     (SupportReport + top unparsed template clustering)
```

---

## 5. Deliverables & Milestones

### Milestone C1 — Baseline Diff Harness, Normalizer, Lexer & Overrides
1. Generate the ground-truth TS IR v2 reference files in `/tmp/` using `node /home/ryan/statikana/peckhamr.com/scripts/export-ir-v2.ts`:
   - Precons → `/tmp/ts-precons.ir-v2.json`
   - Full Scryfall bulk (`/home/ryan/data/scryfall/oracle-cards.jsonl.gz`) → `/tmp/ts-scryfall.ir-v2.json`
2. Build a fast Rust binary `ir-diff` in `mtg-compiler-work` that runs `mtg-compiler` over the same Scryfall bulk JSONL + precon cards and diffs every compiled `CardDef` against the TS IR v2 oracle, reporting:
   - `identical_ir`, `rust_supported`, `ts_supported`, `ts_only_supported` (regressions to close), `rust_newly_supported`, `compile_errors`, total wall time, and top 50 unparsed template clusters.
3. Implement `normalize.rs`, `lexer.rs`, `support.rs`, and migrate the 38 hash-gated overrides into `overrides.rs` (verifying `text_hash` parity and rename-invariance fallback).

### Milestone C2 — Core Grammars (Characteristics, Keywords, Costs, Filters, Values)
- Implement `card.rs` face/characteristics/layout extraction (all 10 `CardLayout`s + `PrintedPt` + `ManaCost` + intrinsic basic-land mana abilities), `keyword.rs` (all 214+ keywords), `cost.rs`, `filter.rs`, and `value.rs`.
- Verify unit tests with synthetic/renamed cards and track `ir-diff` climb on precons.

### Milestone C3 — Effects, Triggers, Statics & Replacements → 41/41 Precon Parity
- Implement `effect.rs`, `trigger.rs`, and `static_def.rs` (including replacements & modals), iterating systematically against `ir-diff` on the 41 precon decks first until **`2,188/2,188 = 100.0%` precon support** with `0` errors and exact/equivalent IR parity against TS.

### Milestone C4 — Full 35,062-Card Scryfall Corpus Parity & Speed Gate
- Close remaining top unparsed clusters in `ir-diff` against the full Scryfall corpus until all exit criteria pass:
  - **Precon decks:** `2,188/2,188 (100.0%)`
  - **Full Scryfall corpus:** `>= 25,417/35,062 (>= 72.5%)` fully supported, `0` compiler errors
  - **Performance:** All 35,062 Scryfall cards compile in **`< 2.0 s`** in `--release` (and 400 cards cold in `< 15 ms`)
  - **Codec check:** Output `IrV2Bundle` builds cleanly into `cards.bin` via `CardDatabase::from_bundle` + `encode_zstd` / `encode_raw` with `#[serde(deny_unknown_fields)]`.

---

## 6. Verification Protocol

- Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` after every slice.
- Verify rename invariance on every grammar module (renaming a synthetic card + its self-references yields identical `CardDef` modulo `name`).
- Keep commits small, focused, and accompanied by `ir-diff` progress metrics in commit messages.
