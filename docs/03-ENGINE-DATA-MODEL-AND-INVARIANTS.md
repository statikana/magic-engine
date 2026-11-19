# 03 — Engine Data Model, Memory Layout & CR Invariants (`mtg-engine`)

## 1. Core Architectural Shift: From JS Object Graph to Flat Arena

The TS engine embeds a full `ScryfallCard` inside every `CardInstance` and mutates nested objects freely across 65 runtime files. That causes two cascading problems measured in our V8 profile:
- **669 KB** serialized `GameState` (~20 ms just to clone/redact/serialize per HTTP request).
- **~50% of total CPU** spent in `dynamicFingerprintMatches` + `getBoardSnapshot` re-scanning the entire board on every layer query because mutations are untracked.

`mtg-engine` eliminates both bottlenecks structurally.

---

## 2. Memory Layout & Generational Arena (`GameState`)

```rust
// Conceptual layout (target: ~8-20 KB for a 4-player Commander mid-game board)
pub struct GameState {
    config: GameConfig,
    db: Arc<CardDatabase>,          // Immutable, O(1) Arc clone during MCTS rollouts!
    rng: RngState,                  // Mulberry32 (TS-compat) or ChaCha8/SmallRng

    // Generational object arena — ALL objects (library, hand, battlefield, gy, exile, command, stack, tokens)
    objects: ObjectArena,           // Vec<Slot<GameObject>> + free list; ObjectId { index: u32, gen: u32 }

    // Per-player state & ordered zone vectors of ObjectId (4-8 bytes each, not full card structs!)
    players: SmallVec<[PlayerState; 4]>,
    stack: Vec<StackEntry>,         // Routed through the SAME zone funnel as player zones!

    // Turn, phase, priority & combat state
    turn: TurnState,
    combat: CombatState,

    // Continuous effects, timed modifiers, delayed triggers & prevention shields
    continuous_effects: Vec<TimedContinuousEffect>,
    delayed_triggers: Vec<DelayedTrigger>,
    prevention_shields: Vec<DamagePreventionShield>,

    // Pausable execution frames (6-kind choice protocol, pending costs, pending replacements)
    pending_frame: Option<ResolutionFrame>,
    pending_triggers: Vec<PendingTrigger>,

    // O(1) Layer Cache Invalidation (private! bumped only by mutators)
    layer_rev: u64,
    dirty_since_eval: DirtyMask,    // Bitflags: TOPOLOGY | COUNTERS | COMBAT | LIFE | GRAVEYARD | TURN_HISTORY | ...
    layer_cache: Option<LayerCache>,// Recomputed lazily; skipped on Clone or cloned cleanly as needed
}
```

### Key Design Rules

1. **`ObjectId { index: u32, gen: u32 }` enforces `CR 400.7` automatically:**
   - Every genuine cross-zone move (except `Stack -> Battlefield` permanent spell resolution per `CR 400.7d / 107.3m`) bumps `gen` (or allocates a new generation) and resets transient battlefield fields (`tapped`, `damage_marked`, `deathtouch_damage`, `counters`, `attached_to`, `copiable_override`, `face_down`, `transformed`, `summoning_sick`, `goaded_by`, `used_loyalty_this_turn`, `playable_from_exile_by`) **inside one function**: `GameState::move_object(...)`.
   - Old `ObjectId` references held by spells on the stack, delayed triggers, or LKI records automatically detect stale generations (`arena.get(id) == None`) and fall back to recorded LKI (`CR 113.7a, 608.2h`)—making stale-target bugs impossible.
   - **Phasing (`CR 702.26d`)** toggles `phased_out` in place without changing `gen` or firing ETB/LTB triggers.
2. **Compact `GameObject` (~128–192 bytes instead of ~1.7 KB):**
   - Stores `card_def: CardDefId`, `owner: PlayerId`, `controller: PlayerId`, `zone: Zone`, `entered_timestamp: u64`, `flags: ObjectFlags` (bitflags for `tapped`, `summoning_sick`, `phased_out`, `is_token`, `is_commander`, `face_down`, `transformed`, `flipped`, `saddled`, `monstrous`, `suspected`, `prepared`, `renowned`, `solved`, `exerted`, `bestowed`, `prototyped`), `damage_marked: i32`, `counters: SmallVec<[(CounterId, u32); 2]>`, `attached_to: Option<ObjectId>`, and optional boxed rare overlays (`Box<RareObjectState>` for copy specs, designations like `chosen_color`/`chosen_subtype`/`chosen_name`, meld/mutate stacks, room unlocked doors, class level).
   - Keeping rare overlays behind `Option<Box<RareObjectState>>` (or a side `HashMap<ObjectId, RareObjectState>`) keeps common cards (lands, vanilla/typical creatures) tiny and cache-line friendly!
3. **Private fields + O(1) mutation tracking:**
   - All fields on `GameState` are private to the mutation layer (`state/mutators.rs`).
   - Every mutation method calls `self.mark_dirty(DirtyMask::...)`, which increments `self.layer_rev` and ORs the relevant category bit into `self.dirty_since_eval`.
   - `evaluate_board()` checks in **O(1)**:
     - If `cache.rev == self.layer_rev`, return cached result immediately.
     - Even if `layer_rev` changed, if `(self.dirty_since_eval & cache.reads_mask) == DirtyMask::empty()` (e.g., life changed, but no active continuous effect read life totals), bump `cache.rev = self.layer_rev` and skip recomputation!
   - **Invariant verifier (debug/test builds):** Retain the TS `MAGIC_LAYER_CACHE_CHECK=1` idea as a `#[cfg(feature = "verify-layer-cache")]` mode that recomputes layers from scratch and `assert_eq!`s against the cached evaluation on every query in tests.

---

## 3. Four CR Debts Fixed from Day 1

Compared to TS [`ENGINE_INVARIANTS.md`](file:///home/ryan/statikana/peckhamr.com/src/lib/magic/docs/ENGINE_INVARIANTS.md), `mtg-engine` closes four documented architectural gaps at the foundation:

### A. Real `CR 613.8` Continuous-Effect Dependency Resolution
- **TS gap:** Layer 4 used 3 hardcoded sub-passes (attached types → type overwrites → additive types/colors) then timestamps.
- **Rust v1 design:** Within each layer/sublayer (`1a, 1b, 2, 4, 5, 6, 7a, 7b, 7c, 7d`), collect the active continuous effects (`Vec<ActiveEffect>`, typically 0–20 items on a board).
  - If `active.len() <= 1` or no pair shares overlapping targets/types (fast path — 99% of boards!), apply directly in timestamp order (`CR 613.7`).
  - Otherwise, build a tiny bitset dependency graph (`[u64; N]`) using `CR 613.8a` rules: effect **B depends on A** iff both apply in the same layer/sublayer, neither is a CDA unless both are, and applying A changes the set of objects B applies to, what B does to any object, or whether B exists (e.g. *Blood Moon* vs *Urborg, Tomb of Yawgmoth*, *Humility* vs *Opalescence*).
  - Topologically apply independent effects in timestamp order; break dependency loops strictly by timestamp order (`CR 613.8b`).

### B. Unified `CR 614–616` Replacement-Effect Pipeline
- **TS gap:** Separate ad-hoc replacement helpers for zone changes, draws, counters, token multipliers, and inline prevention checks for damage/untap; no `CR 616.1b` control-changing ETB tier.
- **Rust v1 design:** Route **every** replaceable event (`ZoneChange`, `Draw`, `Damage`, `LifeGain`, `PutCounters`, `CreateToken`, `Untap`, `Proliferate`) through a single `resolve_replacements(state, event: ReplaceableEvent) -> ReplacementOutcome` pipeline:
  1. Maintain `applied: SmallVec<[ReplacementSourceId; 4]>` so each replacement applies **at most once** to a given event (`CR 614.5`).
  2. Collect all currently applicable `ReplacementDef`s + damage prevention shields (`CR 615`) matching `event`.
  3. Partition by **`CR 616.1` priority tiers**:
     - Tier 1 (`CR 616.1a`): Self-replacement effects (`self_replacement == true`).
     - Tier 2 (`CR 616.1b`): Control-changing ETB replacements.
     - Tier 3 (`CR 616.1c`): Copy effects (`Clone`, etc.).
     - Tier 4 (`CR 616.1d`): Other ETB/event replacements & preventions.
  4. Within the highest non-empty tier: if 1 effect applies, apply it and loop; if ≥2 apply, determine the affected player / controller of affected object (`CR 616.1`) and pause via `ChoiceRequest::ChooseReplacement` (never silently slicing!).

### C. Stack Managed Through the Single `CR 400.7` Zone Funnel
- **TS gap:** `state.stack` was mutated directly across 11 runtime files outside `zones.ts`.
- **Rust v1 design:** Stack spells (`ObjectId` with `zone == Zone::Stack`) enter and leave the stack exclusively through `GameState::move_object()`. Resolving spells remain on the stack until all `EffectStep`s and interactive prompts complete (`CR 608.2n`), so "End the turn" (`CR 725.1b`) cleanly exiles the resolving spell itself. Activated/triggered abilities on the stack are first-class `StackEntry::Ability` entries referencing their source `ObjectId` + LKI snapshot.

### D. Signed Unclamped `i32` P/T & `Option<PowerToughness>` (`CR 107.1b, 208.3, 208.5`)
- **TS gap:** `EvaluatedPermanent` clamped `effectiveToughness` to `0` and reported `0` P/T for non-creatures.
- **Rust v1 design:** `EvaluatedObject.pt: Option<PowerToughness>` where `PowerToughness { pub power: i32, pub toughness: i32 }` stays **signed and unclamped** throughout Layers 7a–7d (`1/3` with `-3/-0` then `+2/+0` evaluates to `power = 0`, not `2`). Non-creatures have `pt: None`. Clamping to `>= 0` (`val.max(0)`) occurs exclusively at action sites (combat damage assignment `CR 510.1a`, life gain/loss, draw count, token/counter counts).

*(Deferred per design decision: **Layer 3 text-changing (`CR 612`)** is omitted from v1, except `LayerMod::SetName` already present in Wave 12.)*

---

## 4. Format-Agnostic `GameConfig` & Optional Guided Fallback

```rust
pub struct GameConfig {
    pub player_count: u8,                 // 2..=8 (1v1, 3-player, 4-player Commander, etc.)
    pub starting_life: i32,               // 20 (Standard/Modern) or 40 (Commander)
    pub starting_hand_size: u8,           // 7
    pub mulligan: MulliganRule,           // London { free_first: bool }
    pub commander: Option<CommanderRules>,// CR 903: command zone, tax (+2), 21 combat dmg, color identity
    pub max_hand_size: u8,                // 7
    pub allow_guided_fallback: bool,      // true for casual web tables; false for strict/AI/competitive
    pub rng_mode: RngMode,                // Mulberry32Compat(u32) | Fast(u64)
}
```

- When `allow_guided_fallback == false` (AI training, tournaments, strict lobbies), decks containing any `CardDef` with `!fully_supported` fail validation up front.
- When `allow_guided_fallback == true`, unsupported abilities still pay their compiled costs and pause with a `ChoiceRequest::GuidedResolution` hook so casual human tables on `magic.peckhamr.com` keep full parity with the TS fallback behavior.

---

## 5. Determinism & `Mulberry32` Compatibility

Per `peckhamr.com/src/lib/magic/runtime/random.ts` (lines 58–74), the TS engine uses a 5-line 32-bit `mulberry32` PRNG and Fisher-Yates shuffle:

```rust
// Exact bit-for-bit equivalent of TS `nextRandom` in runtime/random.ts
pub struct Mulberry32(pub u32);

impl Mulberry32 {
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        t ^ (t >> 14)
    }

    /// Exact equivalent of TS `Math.floor(nextRandom(holder) * bound)`.
    #[inline]
    pub fn next_index(&mut self, bound: usize) -> usize {
        let f = (self.next_u32() as f64) / 4_294_967_296.0;
        (f * (bound as f64)).floor() as usize
    }
}
```

**Determinism rules:**
- **Zero wall-clock timestamps inside `GameState`:** Unlike TS (which wrote `Date.now()` into `GameLogEntry.timestamp` and `StackItem.createdAt`), `mtg-engine` uses monotonic integer sequence counters (`event_seq: u64`). Wall-clock times (rope timers, chat timestamps) belong exclusively in `magic-web`'s server layer!
- Consequently, two games with the same `(GameConfig, CardDatabase, decks, seed, actions)` produce **byte-identical** state snapshots and event streams across Linux, macOS, Windows, and WASM.
