# 06 — Low-Level TS → Rust Module Map & IR v1 → v2 Translation Spec

This document maps **every production subsystem** in `/home/ryan/statikana/peckhamr.com/src/lib/magic/` (97 non-test files, ~214k LOC + 38 overrides + 1,920 tests) to its destination in `mtg-engine` or `magic-web`, and gives the exact rules for translating TS `ir.ts` v1 into Rust `mtg-ir` v2 with **zero silent drops**.

---

## 1. Runtime Module Consolidation Map (`src/lib/magic/runtime/` → `crates/mtg-engine/src/`)

During Waves 1–13 in TS, features were added incrementally into wave-suffixed files (`effectsWave5..11.ts`, `staticsWave6..9.ts`, `triggersWave6..9.ts`, `costsWave6..7.ts`, `frames*Wave5..8.ts`, `keywordsWave3..6.ts`) to stay under the 6,000-line file ceiling. **In Rust, consolidate by MTG Comprehensive Rules domain—never carry over `waveN` filenames.**

| Rust Module (`crates/mtg-engine/src/`) | Source TS Files in `peckhamr.com/src/lib/magic/` | Key Consolidation Notes |
|---|---|---|
| `id.rs`, `arena.rs`, `state.rs` | `types.ts` (`GameState`, `PlayerGameState`, `CardInstance`, `StackItem`), `runtime/engineHost.ts` | Replace embedded `ScryfallCard` with `CardDefId(u32)` + `Arc<CardDatabase>`; generational `ObjectId(u32, u32)`; private fields with `layer_rev`/`DirtyMask`. |
| `rng.rs` | `runtime/random.ts` | Exact `Mulberry32` compat PRNG + Fisher-Yates shuffle + fast PRNG for MCTS. Remove wall-clock `Date.now()` from `GameState`. |
| `config.rs`, `commander.rs` | `types.ts` (`LobbyRulesConfig`), `engine.ts` (`createInitialGameState`, commander tax/damage/zone checks) | Format-agnostic `GameConfig` + opt-in `CR 903` Commander module. |
| `zones.rs` | `runtime/zones.ts` (4.9k L) + stack mutations scattered across 11 TS files | **Single `move_object()` funnel for ALL 7 zones including `Stack`**. `CR 400.7` generation bump + LKI capture + transient resets automatic by default. |
| `faces.rs` | `runtime/faces.ts`, `runtime/striatedFrames.ts`, `runtime/framesBattlesAndKeywordsWave5.ts`, `runtime/framesModalsWave6.ts`, `runtime/framesWave7.ts`, `runtime/framesWave8.ts`, `scryfall.ts` (characteristic helpers only) | Pre-parsed `Characteristics` & `CardLayout` on `CardDef` eliminate the 14% TS CPU spent re-parsing type lines! Covers Transform, MDFC, Split, Flip, Adventure, Meld, Rooms, Sagas, Classes, Cases, Battles, Prototype, Leveler, Station, Face-Down. |
| `layers/mod.rs`, `layers/deps.rs`, `layers/cache.rs`, `layers/predicates.rs` | `runtime/layers.ts` (5.4k), `runtime/layersPredicates.ts` (3.2k), `runtime/layerCache.ts`, `runtime/staticsLayersAndRestrictions.ts`, `runtime/staticsWave6..9.ts` (continuous parts), `ruleTree.ts` | O(1) `layer_rev`/`DirtyMask` validation (deletes `dynamicFingerprintMatches` & `sumNonStandardPtCountersInLayer`). Adds real `CR 613.8` dependency graph. Unclamped `Option<PowerToughness>`. |
| `filter.rs`, `eval.rs` | `runtime/objectFilter.ts` (1.8k), `runtime/effectsConditionsAndAmounts.ts` (1.8k) | Bitmask fast-path `matches_filter` + `evaluate_amount` + `evaluate_condition` over all flattened `Amount` & `Condition` variants. |
| `replace.rs` | `runtime/replace.ts` (2.4k), `runtime/replaceWave9.ts`, `runtime/staticsWave6..9.ts` (replacement parts), `runtime/copyPreventionAndEffects.ts` (prevention shields) | **Unified `CR 614–616` pipeline** with `applied` dedup (`CR 614.5`) and `CR 616.1a–d` priority tiers covering zone changes, damage, draws, life gain, counters, tokens, untap, proliferate. |
| `events.rs`, `triggers.rs` | `runtime/events.ts`, `runtime/triggers.ts` (5.3k), `runtime/triggersFiltersAndQueue.ts` (2.6k), `runtime/triggerStacking.ts` (2.2k), `runtime/triggersWave6..9.ts`, `runtime/reflexiveManaAndActions.ts` (reflexive part) | Atomic event batches, pre-batch LKI (`CR 603.10a`), APNAP trigger stacking (`CR 603.3b`), intervening-if (`CR 603.4`), state triggers (`CR 603.8`), delayed triggers (`CR 603.7`), reflexive triggers (`CR 603.12`). |
| `mana.rs`, `auto_tap.rs`, `costs.rs` | `runtime/mana.ts`, `runtime/manaCountersWave11.ts`, `runtime/autoTap.ts` (1.7k), `runtime/costs.ts` (3.9k), `runtime/castingAndCosts.ts` (3.7k), `runtime/costsWave6.ts`, `runtime/costsWave7.ts`, `runtime/staticsDynamicAndCosts.ts`, `runtime/staticsAndCostsWave5.ts`, `runtime/kicker.ts`, `oracle.ts` (mana math only) | Pre-parsed `ManaCost` arithmetic, restricted mana buckets (`CR 106.6`), fast backtracking auto-tap solver, cost modifiers (`CR 601.2f`), alternative/additional/kicker/optional costs. |
| `casting.rs`, `legality.rs`, `targets.rs` | `runtime/legalityAndTargets.ts` (3.9k), `runtime/castingAndCosts.ts` (cast options), `runtime/tuckFreezeChosen.ts` (protection/hexproof) | Sequential `CR 601.2` casting state machine + legal target candidate generation (`CR 115`) + target re-validation on resolution (`CR 608.2b`). |
| `effects/` *(split by domain: `zones.rs`, `combat_pt.rs`, `counters.rs`, `mana_draw.rs`, `tokens_copy.rs`, `keyword_actions.rs`, `control_flow.rs`)* | `runtime/effects.ts` (5.4k), `runtime/effectsSteps.ts` (4.6k), `runtime/effectsDamageAndMana.ts` (1.6k), `runtime/effectsWave5..11.ts`, `runtime/keywordActions.ts`, `runtime/reflexiveManaAndActions.ts` (actions), `runtime/copyPreventionAndEffects.ts` | Exhaustive `match` over strongly-typed `EffectOp`; pausable `ResolutionFrame` stack for mid-step choices; LKI bindings (`bind_as`). |
| `keywords.rs` | `keywords.ts`, `runtime/orthogonalKeywords.ts`, `runtime/keywordsWave3.ts`, `runtime/keywordsWave4.ts`, `runtime/keywordsWave6.ts`, `runtime/framesBattlesAndKeywordsWave5.ts` (keyword parts) | Unified `KeywordSpec` table covering all 214+ implemented `CR 702` keywords. |
| `combat.rs` | `runtime/combat.ts` (2.8k), `runtime/combatWave10.ts` (1.3k) | Sequential attacker/blocker declaration + combat rules/evasion/taxes/goad/banding/first-strike/trample-over-planeswalkers/lifelink/deathtouch/infect/wither (`CR 506–511`). |
| `sba.rs` | `runtime/sba.ts` (1.5k) + SBA hooks in `framesBattlesAndKeywordsWave5.ts`, `keywordsWave6.ts`, `framesWave7.ts` | Iterative `CR 704.3` fixpoint loop with pre-annihilation LKI (`CR 704.8`) and **zero** per-permanent cache re-validation! |
| `stack.rs`, `turn.rs`, `choices.rs` | `runtime/stackResolution.ts` (4.0k), `runtime/turnAndPrompts.ts` (3.8k), `runtime/choices.ts` (842 L), `engine.ts` (4.0k) | Turn-based actions (`CR 703.3`), priority/APNAP (`CR 117, 500–514`), pausable stack resolution (`CR 608.2`), unified 6-kind `ChoiceRequest` protocol. |
| `view.rs`, `observe.rs`, `determinize.rs` | `runtime/viewerRedaction.ts` (319 L), `ui/boardViewModel.ts` (3.1k — rules-derived view data), `ui/autoTapPreview.ts` (504 L), `ui/targetingController.ts` (legality queries) | Produces redacted `ClientViewModel` (replacing full-state JSON clone redaction!), ML `Observation`, and `determinize(viewer, rng)`. |
| `session.rs`, `replay.rs` | `engine.ts` public façade | Narrow `MatchSession` API + `MacroAction` lowering + deterministic `ReplayLog` & `TsDiffDigest`. |

---

## 2. Compiler, Overrides & Non-Engine Module Map

| Source in `peckhamr.com` | Destination | Notes |
|---|---|---|
| `src/lib/magic/ir.ts` (4.3k L) | `mtg-engine/crates/mtg-ir/src/*.rs` | Redesigned as IR v2 (`crates/mtg-ir` already holds the reference schema). |
| `src/lib/magic/compiler/*.ts` (19 files, 60k L) | **Phase 0–4:** stays in `peckhamr.com` feeding `scripts/export-ir-v2.ts`. **Phase 5:** replaced by clean lexer + recursive-descent parser in `mtg-engine/crates/mtg-compiler/`. | Do **not** port the 5,354 JS regexes to Rust. See [`07-DIFFERENTIAL-TESTING-AND-COMPILER.md`](./07-DIFFERENTIAL-TESTING-AND-COMPILER.md). |
| `src/lib/magic/overrides/*.json` (38 files) + `overrides/index.ts` | Exported into `cards.bin` during Phase 0–4; migrated to IR v2 hash-gated override files in `mtg-compiler` during Phase 5. | Keep strict override restraint rule (`<5%`, no unsupported abilities inside overrides). |
| `src/lib/magic/ai.ts` (2.0k L) | Replaced by `mtg-engine/crates/mtg-ai/` | Heuristic evaluator + parallel ISMCTS (`rayon`), benchmarked against TS 1-ply logic. |
| `src/lib/magic/precons.ts` & `precons.archidekt.json` | **Never committed to public `mtg-engine`!** Exported locally as `DeckDef` entries inside `cards.bin` for testing and served by `magic-web`. | Keeps WotC/Scryfall IP out of the open-source crates. |
| `src/lib/magic/db.ts`, `server/magic-backend/*`, `src/routes/magic/api/*`, `ops/magic-backend/*` | Replaced by `magic-web/server/` (Rust `axum` + `rusqlite` + WS actors) & `magic-web/ops/` | See [`08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md`](./08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md). |
| `src/lib/magic/ArenaTable.svelte`, `src/lib/magic/ui/*`, `src/routes/magic/+page.svelte`, `ManaSymbol.svelte`, `ManaText.svelte`, `vfx.ts`, `cardImages.ts`, `search.ts` | Moved & adapted into `magic-web/web/` | Strip all direct `engine.ts`/`compiler/`/`runtime/` imports; consume server `ClientViewModel` + lazy `mtg-wasm`. |
| `src/lib/magic/**/*.test.ts` (134 files, 1,920 tests) | Used as **behavioral spec** during Phases 1–2; port key CR regression tests into Rust `#[test]` suites using synthetic/renamed cards, and validate precon integration via differential testing. | Keep `architecture.test.ts` invariants enforced structurally by Rust visibility & crate boundaries. |

---

## 3. Exact TS `ir.ts` v1 → Rust `mtg-ir` v2 Translation Rules (`scripts/export-ir-v2.ts`)

When implementing `scripts/export-ir-v2.ts` in `peckhamr.com`, follow these exact mappings and enforce strict consumed-key assertions on every translated node:

### 3.1 Interning & Bitmasks
- **Interners:** Maintain three deterministic `Map<string, number>` tables:
  - `subtypes`: preserve canonical title case (e.g. `"Elf"`, `"Forest"`).
  - `counters`: lowercase normalized (e.g. `"+1/+1"`, `"-1/-1"`, `"loyalty"`, `"stun"`).
  - `keywords`: lowercase normalized (e.g. `"flying"`, `"ward"`, `"equip"`).
- **Bitmasks (serialized as raw integer `u8`/`u16`/`u64` via `bitflags` `#[serde(transparent)]`):**
  - `ColorMask` (`u8`): `W=1, U=2, B=4, R=8, G=16, C=32`.
  - `CardTypeMask` (`u16`): `Land=1, Creature=2, Artifact=4, Enchantment=8, Planeswalker=16, Instant=32, Sorcery=64, Battle=128, Kindred/Tribal=256, Dungeon=512, Emblem=1024, Permanent=Land|Creature|Artifact|Enchantment|Planeswalker|Battle`.
  - `SupertypeMask` (`u8`): `Basic=1, Legendary=2, Snow=4, World=8, Ongoing=16`.
  - `ZoneMask` (`u8`): `library=1, hand=2, battlefield=4, graveyard=8, exile=16, command=32, stack=64`.
- **Serde enum representation (important for `postcard`):** All Rust enums in `mtg-ir` and `mtg-engine` use Rust's default **externally tagged** `serde` representation (`#[serde(rename_all = "camelCase")]`, *not* `#[serde(tag = "k")]`) so `cards.bin`, `ReplayEntry`, and `GameState` snapshots round-trip losslessly in non-self-describing `postcard` binary format as well as JSON. In JSON, unit variants serialize as `"tapSelf"` and struct variants serialize as `{ "fixed": { "power": 2, "toughness": 2 } }`.
- **`Characteristics.pt`:**
  - If `ptDefinedByAbility` is true → `{ "cda": { "power": power ?? null, "toughness": toughness ?? null } }`.
  - Else if `power !== undefined && toughness !== undefined` → `{ "fixed": { "power": power, "toughness": toughness } }`.
  - Else → `null`.
- **`ManaCost`:** Parse every `{...}` mana cost string using the exact rules in `crates/mtg-ir/src/types.rs` (`ManaCost::parse`), preserving `raw` for display.

### 3.2 `ObjectFilter` Splitting
- Map `zone` (`ZoneType | ZoneType[]`) → `zones: ZoneMask`.
- Map `types`, `allTypes`, `notTypes`, `supertypes`, `notSupertypes`, `colors`, `allColors`, `notColors` → corresponding bitmasks.
- Map `subtypes`, `allSubtypes`, `notSubtypes` → interned `SubtypeId[]`.
- Map all boolean properties (`other`, `tapped`, `token`, `attacking`, `blocking`, `isCommander`, `multicolored`, `monocolored`, `isColored`, `faceDown`, `historic`, `modified`, `outlaw`, `equipped`, `enchanted`, `enchantedOrEquipped`, `hasAnyCounter`, `attackingOrBlocking`, `unblockedAttacker`, `enteredThisTurn`, `notEnteredThisTurn`, `dealtDamageThisTurn`, `saddled`, `monstrous`, `suspected`, `isPrepared`, `renowned`, `goaded`, `hasAdventure`, `hasXInManaCost`, `kickedSpell`, `bargainedSpell`, `wasNotCast`, `castNotFromHand`/`castFromOutsideHand`, `withCyclingAbility`, `artifactEnchantmentOrToken`) → `flags: FilterFlags` (`u64` — note JS `BigInt` when setting bits `>= 31`, serialized as JS `Number` since max bit is `52 <= MAX_SAFE_INTEGER`!).
  > **Precision warning:** Because `FilterFlags` currently uses bits `0..=52` (max `2^53 - 1`), never use 32-bit JS bitwise operators (`|`, `<<`) for bits `>= 31`—compute with `BigInt` (`1n << 52n`) and convert via `Number(bits)` (safe up to bit 52, or serialize as string/u64 if bits ever exceed 52).
- Map `manaValue` / `manaValueCompare` → `mana_value`; `power`, `toughness` → `power`, `toughness`.
- Map every remaining field on TS `ObjectFilter` → an explicit `ObjectPredicate` entry in `extra[]` (e.g. `castFromZone` / `castFromZoneFilter` / `castFromZones` all unify into `ObjectPredicate::CastFromZones { zones }`).

### 3.3 `Amount` & `Condition` Flattening
- Unwrap TS `p17bMetric` (`metric` + optional `filter`/`counter`) directly into its flat `Amount` variant (e.g., `metric: 'distinctCreatureTypesControlled'` → `"distinctCreatureTypesControlled"`; `exiledCostCardCount` → `{ "costPaidCount": { "kind": "exiled" } }`).
- Unwrap TS `p20bCostPaidCount` → `{ k: 'costPaidCount', kind }`.
- Unify duplicate TS aliases: `totalStatAmong` and `totalStatOf` → `Amount::TotalStatAmong`; `colorsOfManaSpentToCast` and `colorsManaSpentToCast` → `Amount::ColorsOfManaSpentToCast`; `hasMaxSpeed`/`maxSpeed` → `Condition::HasMaxSpeed`; `hasCitysBlessing`/`citysBlessing` → `Condition::HasCitysBlessing`; `firstCombatPhaseOfTurn`/`isFirstCombatPhaseOfTurn` → `Condition::FirstCombatPhaseOfTurn`; `enteredUnderYourControlThisTurn`/`enteredBattlefieldUnderYourControlThisTurn`/`enteredUnderYourControlThisTurnCount` → `Condition::EnteredUnderYourControlThisTurn`; `spellCastFromHand`/`Graveyard`/`Exile` → `Condition::SpellCastFromZone { zone }`.
- Unwrap TS `p17bCondition` and `p20bCondition` into their first-class `Condition` variants.

### 3.4 `EffectStep` → `EffectOp` Dispatch
- Extract step-level fields onto `EffectStep`: `subject`, `actor`, `amount`, `optional`, `gate` (combining `ifPrecedingAccepted`, `ifPrecedingDeclined`, `condition`), `bind_as`, `do_this_once_per_turn`, `reflexive_trigger` (pulled out of `params.reflexiveTrigger`), `span`.
- Extract shared selection fields (`choiceCount`, `upTo`, `atRandom`, `moveFilter`, `chooserOverride`) into `SelectionMod` for verbs that accept them (`destroy`, `exile`, `bounce`, `move`, `copyPermanent`, `putCounters`, `attach`, `phaseOut`, `sacrifice`, `discard`, `tap`, `untap`, `turnFace`, `grantPlayFromExile`, `reveal`).
- Dispatch each of the 127 TS `Verb` values to its typed `EffectOp` variant in `crates/mtg-ir/src/effect.rs`.
- **Empirical completeness check:** Assert `consumedKeys.size === definedKeys.length` on `step.params` for every single step of every single card.

### 3.5 `StaticDef`, `RuleMod`, `EventFilter` & `ReplacementWith`
- **`StaticDef`:** Wrap `{ body: StaticBody, condition }` (lifting `condition` once to the outer struct).
- **`RuleMod`:** Dispatch `st.rule` + `st.params` into the typed `RuleMod` enum in `crates/mtg-ir/src/static_def.rs` (all 16 parameterized rules were empirically verified against all 35,062 Scryfall cards).
- **`EventFilter`:** Place shared fields (`object`, `orSelf`, `orAttachedToSelf`, `orCommander`, `player`, `oneOrMore`, `duringTurn`, `duringStep`, `duringCombat`, `duringPhase`, `firstTimeEachTurn`, `nthThisTurn`, `nthEventThisTurn`, `attachedFilter`, `watchedBoundObject`) on `EventFilter`, and route event-specific fields into the matching `EventDetail` variant.
- **`ReplacementDef.with`:**
  - `Array.isArray(w)` → `ReplacementWith::Execute { steps }`
  - `w.k === 'skip'` → `ReplacementWith::Skip`
  - `w.k === 'modify'` → dispatch by domain key (`damageMod`/`chosenPlayerDamageMod` → `DamageScale`, `noncombatDamageToOpponentCreatureAsMinusCounters` → `DamageToMinusCounters`, `damageToSelfAsPlusCounters` → `DamageToSelfAsCounters`, `lifeGainMod` → `LifeGainScale`, `drawMod` → `DrawScale`, `stealOpponentExtraDraws` → `StealOpponentExtraDraws`, `abundanceDrawChoice` → `AbundanceDrawChoice`, `emptyLibraryGraveyardReanimate` → `EmptyLibraryGraveyardReanimate`, `proliferateMultiplier` → `ProliferateMultiplier`, `regenerateSelfOnDestroy` → `RegenerateSelfOnDestroy`, and all zone-change / ETB choice / ETB copy / ETB counter keys → `ZoneChange { mod_spec }`).
