# Known TypeScript vs. Rust Comprehensive Rules Divergences

This document records every intentional Comprehensive Rules (CR) improvement implemented in Rust `mtg-engine` (Milestones M2–M6) relative to the reference TypeScript engine (`peckhamr.com/src/lib/magic/`), as well as protocol/adapter normalizations applied by `mtg-engine::replay::run_ts_diff_test` and `mtg-cli diff-test`.

Each CR divergence below corresponds to one of the Day-1 CR debts documented in `peckhamr.com/src/lib/magic/docs/ENGINE_INVARIANTS.md` or a rules-accuracy refinement verified by unit and E2E tests.

---

## 1. Intentional CR Improvements in Rust `mtg-engine`

### 1.1 Single `CR 400.7` / `CR 400.7d` Zone Funnel Including `Zone::Stack` (M2)
- **CR Citations:** `CR 400.7`, `CR 400.7d`, `CR 107.3m`
- **TypeScript Behavior:** `ENGINE_INVARIANTS.md` §3.1 — TS mutates `state.stack` directly outside `zones.ts` rather than routing stack entry/exit through `moveObject()`.
- **Rust CR Behavior:** All 7 zones (including `Zone::Stack`) enter and leave exclusively through `zones::move_object_raw()` / `zones::move_object()` (`crates/mtg-engine/src/zones.rs`). Cross-zone moves automatically bump `ObjectId.generation`, perform LTB transient state resets, and capture LKI snapshots, while `Zone::Stack -> Zone::Battlefield` permanent resolution preserves the spell-to-permanent object continuity (`CR 400.7d`).
- **Verified Rust Tests:** `zones::tests::cr_400_7_and_400_7d_generation_bumps_and_ltb_resets`, `e2e/tests/t1_engine_features.rs` (`t1_f16_05`).

### 1.2 `CR 613.8` Continuous-Effect Dependency Ordering & `OnceLock<LayerCache>` (M3)
- **CR Citations:** `CR 613.8`, `CR 613.8a`, `CR 613.8b`, `CR 613.6`
- **TypeScript Behavior:** `ENGINE_INVARIANTS.md` §2.2 — TS used three hardcoded Layer 4 passes rather than general `CR 613.8` dependency graph evaluation.
- **Rust CR Behavior:** `crates/mtg-engine/src/layers/deps.rs` and `crates/mtg-engine/src/layers/mod.rs` implement full `CR 613.8a` dependency detection within layer subpasses with an O(1) fast path when effects do not interact, and `CR 613.8b` timestamp tie-breaking for circular dependencies. Board evaluation is memoized in `OnceLock<LayerCache>` with `DirtyMask`-selective invalidation and cross-checked by `--features verify-layer-cache`.
- **Verified Rust Tests:** `layers::mod::tests::cr_613_8a_urborg_and_blood_moon_both_timestamp_orders`, `layers::mod::tests::cr_613_8a_opalescence_and_humility_cr_613_6_persistence`, `layers::mod::tests::cr_613_8b_circular_dependency_loop_broken_by_timestamp`.

### 1.3 Signed Unclamped `Option<PowerToughness>` (`CR 107.1b` / `CR 208.3` / `CR 208.5`) (M3)
- **CR Citations:** `CR 107.1b`, `CR 208.3`, `CR 208.5`
- **TypeScript Behavior:** `ENGINE_INVARIANTS.md` §2.6 — TS clamped `effectiveToughness` at `0` in layer evaluation and emitted `[0, 0]` for non-creature permanents in step digests.
- **Rust CR Behavior:** `EvaluatedObject.pt` is `Option<PowerToughness>` (`None` for non-creatures) and stays signed and unclamped (`i32`) through Layer 7d (`CR 107.1b`), clamping with `.max(0)` strictly at combat/fight damage sites (`CR 120.3`). In `diff_ts_step_digest`, non-creature TS entries with `"pt": [0, 0]` are normalized to `None`.
- **Verified Rust Tests:** `layers::mod::tests::layer_ordering_2_to_7d_and_set_name_and_unclamped_pt`, `replay::tests::run_ts_diff_test_1v1_cast_and_noncreature_pt_normalization`, `t1_f16_04`, `t3_19`.

### 1.4 Unified `CR 614–616` Replacement Pipeline & `CR 616.1b` Control-Changing ETB Tier (M4)
- **CR Citations:** `CR 614.5`, `CR 615`, `CR 616.1`, `CR 616.1a`, `CR 616.1b`, `CR 616.1c`, `CR 616.1d`
- **TypeScript Behavior:** `ENGINE_INVARIANTS.md` §4.1 — TS had separate per-domain replacement helpers and lacked the explicit `CR 616.1b` control-changing ETB tier before `CR 616.1c` copy effects.
- **Rust CR Behavior:** `crates/mtg-engine/src/replace.rs` funnels all replaceable events (`ZoneChange`, `Draw`, `Damage`, `LifeGain`, `PutCounters`, `CreateToken`, `Untap`, `Proliferate`) through a single pipeline with `CR 614.5` applied-once tracking, `CR 616.1a–d` priority tiers (updating the affected controller after `616.1b` before `616.1c` copy choices), and serializable `PendingReplacementState` so replacement choice prompts survive `snapshot_bytes()`.
- **Verified Rust Tests:** `replace::tests::cr_616_1b_4p_enter_under_opponent_control_updates_clone_chooser_and_copy_mod_controller`.

### 1.5 `CR 704.8` Pre-Annihilation LKI for `+1/+1` / `-1/-1` Counter Annihilation (M5b)
- **CR Citations:** `CR 704.8`, `CR 704.5q`, `CR 704.5f`
- **TypeScript Behavior:** TS could lose pre-annihilation counter state when lethal damage or `<= 0` toughness and `+1/+1`/`-1/-1` counter annihilation occurred in the same SBA check.
- **Rust CR Behavior:** `crates/mtg-engine/src/sba.rs` captures pre-annihilation LKI snapshots before performing `+1/+1` / `-1/-1` counter cancellation (`CR 704.8`) so dies triggers observe the creature's last-known counters on the battlefield.
- **Verified Rust Tests:** `effects::mod::tests::synth_stack_trigger_meta_dies_lki_counter_and_defending_player`.

### 1.6 `CR 702.16e` Protection Damage Prevention & `UnpreventableDamage` Bypass (M5b)
- **CR Citations:** `CR 702.16e`, `CR 615.12`
- **TypeScript Behavior:** Protection prevention and "damage can't be prevented" interacted ad hoc across damage sites.
- **Rust CR Behavior:** `crates/mtg-engine/src/replace.rs` and `combat.rs` route `CR 702.16e` protection prevention through the unified damage replacement/prevention pipeline and honor `unpreventable_by_effect` (`CR 615.12`) across both combat and non-combat damage.
- **Verified Rust Tests:** `effects::mod::tests::synth_protection_damage_prevention_and_unpreventable_bypass`.

### 1.7 `CR 306.5b` / `CR 310.4b` / `CR 714.3a` Intrinsic ETB Counters via `PutCounters` Replacements (M5b)
- **CR Citations:** `CR 306.5b` (Planeswalker loyalty), `CR 310.4b` (Battle defense), `CR 714.3a` (Saga lore), `CR 122.6`
- **TypeScript Behavior:** Intrinsic ETB counters on planeswalkers/battles/sagas were applied directly without consistently participating in counter-doubling/replacement modifiers.
- **Rust CR Behavior:** `zones::compute_intrinsic_etb_counters` routes starting loyalty, defense, and lore counters through `PutCounters` replacement resolution (`PendingEtbCounterMove`) so counter multipliers and replacements apply cleanly per `CR 122.6`.
- **Verified Rust Tests:** `zones::tests::synth_planeswalker_battle_etb_multiplier_manifest_and_token_copy`.

### 1.8 `CR 611.2a` / `CR 800.4m` Multiplayer Turn-Serial Expiry (`UntilYourNextTurn` / `UntilEndOfYourNextTurn`) (M5b)
- **CR Citations:** `CR 611.2a`, `CR 800.4m`
- **TypeScript Behavior:** Multi-player turn-scoped durations could drift if turn ownership skipped or players were eliminated.
- **Rust CR Behavior:** `crates/mtg-engine/src/turn.rs` and `state.rs` track turn-serial and player-anchored expiry (`expires_on_player`, `turn_serial`) across 3+ player tables, including player elimination (`CR 800.4m`).
- **Verified Rust Tests:** `turn::tests::synth_3p_goad_and_become_copy_until_next_turn_and_until_end_of_next_turn`, `effects::mod::tests::synth_multiplayer_timed_expiry_and_protection_from_everything`.

---

## 2. Differential Replay Adapter Normalizations & Caveats

### 2.1 TS `prioritySeat` Freeze During `mulligan` and Interactive Prompts
- **Observed Behavior:** In `peckhamr.com/src/lib/magic/engine.ts`, `state.priorityPlayerIndex` remains frozen at the pre-prompt priority holder while `status === 'mulligan'` or `pendingPrompts[0]` is active (e.g., `prioritySeat: 0` across all 4 seats' mulligan steps, or keeping `prioritySeat` on the prior player while another seat resolves `scry_choice`, `commander_zone_choice`, `search_library`, or `chooseObjects`).
- **Rust Behavior:** `crates/mtg-engine/src/turn.rs` and `effects/library_hand.rs` advance `state.priority_player` to the active chooser (`current_actor`) whenever a prompt or mulligan decision shifts to another player.
- **Adapter Rule (`diff_ts_step_digest`):** When `actual.pending_prompt_kind.is_some()` AND `expected.pending_prompt_kind == actual.pending_prompt_kind`, `diff_ts_step_digest` allows `expected.priority_seat` to reflect TS's pre-prompt `priorityPlayerIndex`. Whenever `pending_prompt_kind` is `None`, strict `priority_seat` equality is enforced (verified by `t2_f16_05`).

### 2.2 `CR 103.8c` Multiplayer Turn 1 Draw (`firstTurnDraw: true` in TS 4p Commander)
- **CR Citations:** `CR 103.8c`
- **Observed Behavior:** `record-ts-replay.ts` sets `firstTurnDraw: true` in 4-player Commander mode, causing seat 0 to draw 1 card upon transitioning from mulligan into Turn 1 `Main1` (`hand: 8, library: 92` in `t1_f16_02`; `hand: 8, library: 91` in `t4_09` with 1 commander in command zone). Conversely, Rust `turn.rs` starts Turn 1 directly in `Main1` without drawing (`hand == 7`, asserted by `t1_f14_01`).
- **Adapter Rule (`replay.rs`):** Leaving `turn.rs` untouched preserves `t1_f14_01`, while `run_ts_diff_test` bridges TS 4p Commander traces (`trace.seats.len() > 2 && trace.rules.commander`) by drawing 1 card for `PlayerId(0)` via `zones::draw_one_raw` + `AtomicEventKind::DrawCard` when mulligan transitions to `DecisionState::Priority` on Turn 1 `Main1`.

### 2.3 `export-ir-v2.ts` `"defense"` Counter Interning Caveat for Scryfall Battles
- **CR Citations:** `CR 310.4b`, `CR 704.5v`
- **Observed Behavior:** In `peckhamr.com/scripts/export-ir-v2.ts`, `translateCharacteristics` extracts `loyalty` and `defense` onto `FaceDef.characteristics`, and `"loyalty"` happens to be interned by Planeswalker abilities in the corpus, whereas `"defense"` is NOT interned into `tables.counters` for the 39 Battle cards in `/tmp/scryfall.ir-v2.json` (the 41 precon decks contain 0 Battles).
- **Engine Impact:** `zones::compute_intrinsic_etb_counters` (`zones.rs`) and `sba.rs` look up `state.db().counter_id("defense")`. When `"defense"` is absent from `tables.counters` in a Scryfall-exported DB, Battles enter with 0 defense counters and immediately go to the graveyard via `CR 704.5v`. Adding `if (defense !== null) interner.counter('defense')` in `export-ir-v2.ts` resolves this with zero `crates/mtg-ir` schema changes.

### 2.4 TS `turnNumber` (Table Round Number) vs. Rust `turn_number` (`CR 500` Per-Turn Counter)
- **CR Citations:** `CR 500.1`, `CR 500.7`
- **Observed Behavior:** In `peckhamr.com/src/lib/magic/runtime/turnAndPrompts.ts`, `state.turnNumber` tracks table rounds (incrementing only on wrap-around `nextIdx <= baseIdx` or extra turns), while `state.turnSerial` tracks individual turns. In Rust `crates/mtg-engine/src/turn.rs`, `state.turn_number()` increments on every turn (matching `state.turn_serial()`).
- **Adapter Rule (`replay.rs`):** `compute_ts_step_digest` returns `state.turn_number()`, while `run_ts_diff_test` projects TS's round-number counter from `state.turn_serial()` / `state.active_player()` / `extra_turn_queue` transitions before calling `diff_ts_step_digest`.

### 2.5 Display Keyword Formatting & Identical-Permanent Auto-Tap Tie-Breaks
- **Observed Behavior:** TS `evaluatePermanent` emits title-cased parameterized display strings in `activeKeywords` (e.g., `"Equip {1}"`, `"Deathtouch"`), whereas Rust `CardDatabase::keyword_name` returns lowercase interned base names (`"equip"`, `"deathtouch"`). Additionally, TS `autoTapForCost` breaks ties between identical basic lands in battlefield order, whereas Rust `auto_tap.rs` breaks ties by `ObjectId.index`.
- **Adapter Rule (`diff_ts_step_digest`):** Keywords are normalized (lowercased, stripping trailing `" {...}"` parameter suffixes) before set comparison. When a player's battlefield differs strictly on `.tapped` across identical-key permanents (matching full canonical multisets including `tapped` state), `diff_ts_step_digest` permits the identical-permanent tie-break permutation while failing on any count or non-identical-permanent `tapped` mismatch.

### 2.6 TS Greedy `autoTap.ts` Over-Tapping vs. Rust Optimal Backtracking `auto_tap.rs` (M6)
- **Observed Behavior:** In `peckhamr.com/src/lib/magic/runtime/autoTap.ts`, `autoTapForCost` groups `Plains`/`Mountain` and `Sol Ring` in the same priority tier without sorting multi-mana producers (`producesAmount`) ahead of single-mana lands. Consequently, TS can greedily tap both a basic land and `Sol Ring` when paying a generic cost such as `{2}` (e.g. `4p` seed `1` step `151`: tapping `Plains` + `Sol Ring` for `Fellwar Stone`, leaving `{W}` floating in `manaPool`) or `{2}{U}` (e.g. `1v1` seed `77`/`1` step `182`: tapping `Mountain` + `Island` + `Sol Ring` for `Flux Channeler`, leaving `{R}` floating in `manaPool`).
- **Rust Engine Behavior:** `crates/mtg-engine/src/auto_tap.rs` sorts candidates by `b_out.cmp(&a_out)` and performs optimal backtracking to minimize total sources tapped (`Sol Ring` alone for `{2}`, `Island` + `Sol Ring` for `{2}{U}`), leaving zero wasted floating mana and keeping basic lands untapped.
- **Adapter Rule (`crates/mtg-engine/src/replay.rs`):** Rather than weakening `auto_tap.rs`, `replay::drive_casting_sequence` inspects `step.digest_after.players[actor_seat]` at `CastStep::PayCosts` for untapped mana-capable permanents that became `tapped: true` in TS and submits `Action::TapMana` for TS's tapped delta before `Action::AutoPayMana`. This preserves optimal auto-tap in the Rust engine while reproducing TS's exact `manaPool` and `battlefield[*].tapped` digests during differential replay.

