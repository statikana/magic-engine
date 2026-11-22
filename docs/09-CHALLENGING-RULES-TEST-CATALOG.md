# 09 — Challenging Rules & Adversarial Test Catalog (`M2–M7`)

This document is the canonical **hard-rules test checklist** for `crates/mtg-engine/tests/` (Milestone **M6 / F17**) and the opaque-box E2E / Challenger suites (Milestone **M7 / F18**). Every item below represents a classic Magic: The Gathering Comprehensive Rules edge case that breaks naive callback- or modifier-list-based engines.

> **Synthetic-Card & IP Hygiene Rule:** All Rust tests in `mtg-engine` MUST instantiate these scenarios using **synthetic/renamed `CardDef`s** (never committing WotC card names, flavor text, or Scryfall dumps to `mtg-engine`, upholding rename invariance from `ENGINE_INVARIANTS.md` §1.3). Archetype card names below (e.g., *Humility*, *Blood Moon*) are listed strictly as recognizable CR reference shorthand for test authors.

> **Comprehensive Rules Reference:** All CR citations below are verified against `peckhamr.com/src/lib/rules.txt` (effective 2026-11-06), including the **2020 Commander SBA split (`CR 903.9a` vs `903.9b`)** and the **2024 Foundations combat damage assignment update (`CR 510.1c–d`)**.

---

## Priority Stress-Test Matrix (Implement First!)

If an engine passes these 12 flagship stress tests **with `--features verify-layer-cache` enabled**, its core abstractions (objects, zones, events, replacements, triggers, layers, SBAs, legality, priority) are structurally sound:

| # | Flagship Interaction (Synthetic Equivalents) | Primary CR Rules | What It Exposes | Target Milestone |
|---|---|---|---|---|
| **S1** | **Humility + Opalescence** (both timestamp orders!) | `CR 613.1d, 613.1f, 613.4b, 613.6, 613.7, 613.8` | Layer 4/6/7b ordering, cross-layer persistence (`613.6`), timestamp tie-breaking (`Opalescence` first → `Humility` is `1/1`; `Humility` first → `Humility` is `4/4`; both lose abilities in Layer 6 yet both Layer 4 & 7b effects still apply!). | **M3 / M6** |
| **S2** | **Blood Moon + Urborg, Tomb of Yawgmoth** (both timestamp orders!) | `CR 305.7, 613.1d, 613.8a–b` | True Layer 4 dependency overriding timestamps: regardless of entry order, *Urborg* depends on *Blood Moon*, so *Blood Moon* applies first, turns *Urborg* into a Mountain (removing printed text per `305.7`), and lands do **not** become Swamps. | **M3 / M6** |
| **S3** | **Humility + Elesh Norn / Magus of the Moon / Changeling** | `CR 613.1d, 613.1f, 613.4c, 613.6, 702.73a` | Cross-layer persistence: *Elesh Norn*'s Layer 7c static is removed in Layer 6 (`Humility` wins); *Magus of the Moon*'s Layer 4 static and *Changeling*'s Layer 4 CDA already applied before Layer 6 and survive ability loss! | **M3 / M6** |
| **S4** | **Clone + Counters + Anthem + Copy-of-Copy** | `CR 707.2, 613.1a, 613.4c` | Layer 1a copiable values vs. noncopiable state (counters, damage, tapped, Layer 4–7 continuous effects); copying a transformed DFC copies the current face-up characteristics (`CR 707.2 / 712.8`). | **M2 / M3 / M6** |
| **S5** | **First Strike + Deathtouch + Trample + Multi-Block** | `CR 510.1c, 510.2, 702.2c, 702.7, 702.19b` | Two combat damage steps with SBA + trigger window between them; `1` damage from deathtouch counts as lethal for trample spillover; post-2024 free damage division among blockers (`510.1c`). | **M5 / M6** |
| **S6** | **Simultaneous Wrath + Death Triggers + APNAP** | `CR 101.4, 603.3b, 603.10a, 704.3` | Atomic simultaneous destruction, look-back-in-time LKI (`603.10a`: dying creatures see each other die), and APNAP stack ordering (`603.3b`: active player's triggers go on stack first and resolve **last**). | **M4 / M5 / M6** |
| **S7** | **Undying/Persist + `+1/+1`/`-1/-1` Counter Annihilation** | `CR 704.3, 704.5f, 704.5q, 704.8` | Simultaneous SBA evaluation + `CR 704.8` pre-annihilation LKI: a `2/2` with a `+1/+1` counter given two `-1/-1` counters dies with `LKI` still showing the `+1/+1` counter, so Undying does **not** trigger. | **M4 / M5 / M6** |
| **S8** | **Rest in Peace + Replacement Ordering + Chaining** | `CR 614.5, 614.6, 616.1a–d` | Replacement vs. triggers (replaced death never triggers "dies"), affected-player ordering (*Hardened Scales* + *Doubling Season*, *Torbran* + *Furnace of Rath*), and chained replacements without double-applying any single effect (`614.5`). | **M4 / M6** |
| **S9** | **Phasing + Attached Equipment/Auras + Counters/Tokens** | `CR 702.26d, 702.26g` | Phasing is **not** a zone change: `ObjectId` generation unchanged, no ETB/LTB triggers fire, tokens do **not** cease to exist, counters remain, and attached Equipment/Auras phase out/in indirectly together. | **M2 / M5 / M6** |
| **S10** | **Split Cards + Adventure + Alternate Costs** | `CR 709.4b–c, 715.3–4, 118.9` | Context-dependent characteristics: Split card MV = sum of halves off-stack (`709.4b`) and cast half on-stack unless fused (`709.4c`); Adventure resolves to exile as castable creature (`715.3d`). | **M2 / M5 / M6** |
| **S11** | **Commander Graveyard/Exile SBA vs. Hand/Library Replacement** | `CR 903.9a, 903.9b, 614.5` | Commander to graveyard triggers "dies" then offers `CR 903.9a` SBA move to command zone; *Rest in Peace* sends it to exile and `903.9a` still offers command zone; bounce/tuck uses `CR 903.9b` replacement (exempt from `614.5`). | **M2 / M4 / M5 / M6** |
| **S12** | **Blink/Flicker Target Invalidation + End-the-Turn Stack Exile** | `CR 400.7, 608.2b, 608.2n, 725.1b` | Flickering a creature bumps its `ObjectId` generation so targeting spells fizzle completely (`608.2b`, no secondary "draw a card"); resolving spell stays on `Zone::Stack` so "end the turn" exiles the resolving spell itself (`608.2n / 725.1b`). | **M2 / M5 / M6** |

---

## Complete 47-Item Challenging Rules Test Catalog

Every item below must either have dedicated synthetic-card unit/E2E tests in its target milestone or carry an explicit `Deferred` / `N/A` disposition.

### 1. Layers and Continuous Effects (`CR 613`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T01** | **Layer system ordering** | `CR 613.1–613.7` | Layer 6 ability loss + Layer 6 ability grant (timestamp order decides); Layer 4 type change vs. Layer 6 ability loss (`CR 613.6`); Layer 2 control change precedes Layer 4/6/7 filters checking "creatures you control". Verify cache equivalence under `--features verify-layer-cache`. | M3, M6 |
| **T02** | **Dependencies between continuous effects** | `CR 613.8a–b` | (a) *Blood Moon* + *Urborg* in **both** timestamp orders (dependency overrides timestamp). (b) *Dryad of the Ilysian Grove* + *Blood Moon* (no dependency on creature → timestamp decides). (c) Dependency loop falls back cleanly to timestamp order (`613.8b`) with zero infinite recursion. | M3, M6 |
| **T03** | **Humility interactions** | `CR 613.1d, 613.1f, 613.4b, 613.6` | (a) *Humility* + *Opalescence* in both timestamp orders. (b) *Humility* + *Elesh Norn* (Layer 6 strips Layer 7c static). (c) *Humility* + *Magus of the Moon* (Layer 4 survives Layer 6). (d) *Humility* + *Changeling* (Layer 4 CDA survives Layer 6). (e) *Humility* + `+1/+1` counters (counters apply in Layer 7c **after** Layer 7b `1/1` setting → `2/2`!). | M3, M6 |
| **T04** | **Power/toughness switching & signed unclamped P/T** | `CR 613.4a–d, 107.1b, 208.3, 208.5` | (a) `1/3` + `+2/+0` (`3/3`) + *Inside Out* switch (`3/3`, not `3/1`). (b) `0/4` + `+1/+1` counter (`1/5`) + `setBasePT(1, 2)` (`2/3`) + switch (`3/2`). (c) Negative P/T stays unclamped through Layer 7c: `1/3` with `-3/-0` then `+2/+0` evaluates to `0/3`, **not** `2/3`; combat damage clamps negative power to `0` (`510.1a`). (d) Non-creature permanent has `pt: None` (`208.3`). | M3, M6 |

### 2. State-Based Actions and Timing (`CR 704`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T05** | **Iterative SBA fixpoint loop** | `CR 704.3` | Loop repeats until a full pass performs zero actions: e.g., an Aura-buffed `0/0` loses its lord/anthem when the lord dies to lethal damage in Pass 1 → the `0/0` dies in Pass 2; waiting triggers flush onto the stack **only after** the SBA loop stabilizes (`704.3`). | M5, M6 |
| **T06** | **Legend rule** | `CR 704.5j` | Controlling two legendary permanents with the same evaluated name prompts human via `ChoiceRequest::ChooseObjects` (never silent slice); unchosen copy moves to owner's graveyard—**not** destruction (ignores Indestructible) and **not** sacrifice (does not trigger "whenever you sacrifice"), yet **does** trigger "dies" and interacts with graveyard replacement effects (*Rest in Peace*). | M5, M6 |
| **T07** | **Zero-toughness creatures** | `CR 704.5f` | Creature with evaluated toughness `<= 0` (via `-X/-X` or `-1/-1` counters) is put into owner's graveyard even if it has **Indestructible** or a **Regeneration** shield. | M5, M6 |
| **T08** | **Damage vs. toughness reduction** | `CR 120.6, 704.5g` | `4/4` with `2` marked damage survives; later `-2/-2` makes it a `2/2` with `2` marked damage → dies to lethal-damage SBA (`704.5g`), which **is** destruction (so **Indestructible** DOES save a `2/2` with `2` damage, unlike T07!). | M5, M6 |
| **T09** | **Simultaneous SBAs & pre-annihilation LKI** | `CR 704.3, 704.5q, 704.8` | (a) Two creatures dying simultaneously in one SBA pass see each other die (`603.10a`). (b) Creature with `+1/+1` counter + Undying receiving lethal `-1/-1` counters: `704.5f` and `704.5q` apply simultaneously, and `704.8` captures LKI **before** counter annihilation so Undying sees the `+1/+1` counter and does not return it. (c) Simultaneous player losses (`life <= 0`) result in a draw (`104.4a`). | M5, M6 |

### 3. Replacement and Prevention Effects (`CR 614–616`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T10** | **Affected-player replacement ordering** | `CR 616.1a–d` | (a) *Hardened Scales* (`+1`) + *Doubling Season* (`×2`) on counters: controller chooses `(N+1)*2` vs `N*2+1` via `ChoiceRequest::ChooseReplacement`. (b) *Torbran* (`+2`) + *Furnace of Rath* (`×2`) on damage: affected player / damaged permanent's controller chooses `(D+2)*2` vs `D*2+2`. (c) Priority tiers enforced automatically: self-replacement (`616.1a`) → control-change ETB (`616.1b`) → copy (`616.1c`) → general (`616.1d`). | M4, M6 |
| **T11** | **Chained replacements & single-application rule** | `CR 614.5` | Replacement A modifies event E → E' (making Replacement B newly applicable); B modifies E' → E''; A **cannot** apply a second time to E'' (`614.5`). Verify `PendingReplacementState` preserves the `applied` set across interactive `ChooseReplacement` pauses and `snapshot_bytes()` round-trips. | M4, M6 |
| **T12** | **"Instead" vs. triggered abilities** | `CR 614.6` | *Rest in Peace* ("if a card would be put into a graveyard, exile it instead") prevents a dying creature from ever entering the graveyard → "Whenever a creature dies" triggers do **not** fire; "Whenever a permanent leaves the battlefield" triggers **do** fire. | M4, M5, M6 |
| **T13** | **Multiple damage prevention, redirection & unpreventable damage** | `CR 615.1–615.12, 616.1` | (a) Affected player orders prevention shields (`prevent next N`, `prevent all`, redirection) alongside damage doublers/adders (`616.1`). (b) "Damage can't be prevented" (`615.12`) suppresses prevention shields while still allowing non-prevention damage replacement effects. (c) Partial prevention consumes remaining shield amount accurately. | M4, M5, M6 |
| **T14** | **ETB replacement effects & ETB triggers** | `CR 614.1c, 614.12` | Permanent entering tapped, with counters, as a copy, with chosen color/subtype, or under an opponent's control applies all ETB replacements **before** entering the battlefield (`614.12`): e.g., a `0/0` entering with a `+1/+1` counter never dies to SBAs and enters as a `1/1` for "whenever a creature with power 1 enters" triggers; a *Clone* copying an ETB-trigger creature fires the copied creature's ETB trigger! | M4, M5, M6 |

### 4. Copying and Object Identity (`CR 707` & `CR 400.7`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T15** | **Copying copies** | `CR 707.2, 707.9` | *Clone A* copies *Bear* with an exception ("except it's a Zombie" or "except it's a 1/1"). *Clone B* copying *Clone A* inherits *Clone A*'s modified Layer 1a copiable values (the Zombie subtype / `1/1` exception), **not** non-copy effects. | M2, M3, M6 |
| **T16** | **Copying a transformed permanent** | `CR 707.2, 712.8` | Copying a transformed DFC copies the currently face-up back face's copiable values (and mana value rules per `712.8d` / `202.3b`); a single-faced copy cannot transform (`701.27c`). | M2, M3, M6 |
| **T17** | **Copying spells on the stack** | `CR 707.10` | Copying a modal X-spell with kicker copies chosen modes, `X`, kicker status, and targets (`707.10`); "you may choose new targets" allows retargeting without changing modes/`X`; copying a spell does **not** trigger "whenever you cast a spell" (`707.10`). | M5, M6 |
| **T18** | **Copiable vs. noncopiable values** | `CR 707.2` | Copying a tapped, damaged, enchanted, `+1/+1`-countered creature boosted by an anthem produces an untapped, undamaged, unattached copy with 0 counters and base printed P/T (affected only by anthems applying to the copy itself). | M3, M6 |
| **T19** | **Token copies** | `CR 111.3, 111.7, 707.2` | A token copy of a nontoken permanent acquires the permanent's copiable characteristics (including mana cost / mana value for devotion!) while keeping `ObjectFlags::IS_TOKEN` (still matches `token` filters and ceases to exist as an SBA upon leaving the battlefield per `704.5d`). | M2, M5, M6 |
| **T20** | **Copying a permanent affected by Layer 4–7 effects** | `CR 613.1a, 707.2` | Copying an animated land (animated by a Layer 4/7b effect) yields a **non-animated** land; copying a creature under *Humility* yields a creature whose Layer 1a copiable values still have its printed abilities and P/T (which *Humility* then modifies independently in Layers 6/7b, and which persist if *Humility* leaves!). | M3, M6 |

### 5. Triggered Abilities and the Stack (`CR 603` & `CR 608`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T21** | **APNAP ordering** | `CR 101.4, 603.3b` | Simultaneous triggers controlled by Active Player (P0) and Non-Active Players (P1, P2, P3): P0 stacks theirs first (bottom), then P1, P2, P3 (top) → **P3 resolves first, P0 resolves last**. When one player controls multiple simultaneous triggers, that player chooses their relative order (`ChoiceRequest::OrderObjects`). | M4, M5, M6 |
| **T22** | **Cascading triggers (triggers triggering triggers)** | `CR 603.3, 117.5` | Resolving trigger A causes event B which queues trigger C; C is placed on top of the stack *above* any older unresolved triggers on the stack and resolves before them (LIFO), always after SBA stabilization. | M4, M5, M6 |
| **T23** | **Intervening-if clauses** | `CR 603.4` | Test all 3 cases: (a) False when event occurs → **never triggers** (does not go on stack even if condition becomes true later). (b) True at trigger time, becomes false before resolution → goes on stack, fizzles silently on resolution. (c) True at both times → resolves normally. | M4, M5, M6 |
| **T24** | **Cast triggers vs. resolution** | `CR 601.2i, 603.2` | Spell with "When you cast this spell, draw a card" puts its cast trigger on the stack **above** the spell; if the spell is countered, the cast trigger still resolves. | M5, M6 |
| **T25** | **Delayed triggered abilities** | `CR 603.7` | Spell creates a delayed trigger ("At the beginning of the next end step, sacrifice that token" / "When that creature dies this turn, return it"). Survives after the originating spell leaves the stack and originating permanent leaves the battlefield; respects `one-shot` consumption (`603.7b`) and LKI/generation tracking. | M4, M5, M6 |
| **T26** | **Missed triggers** | *MTR (Paper Policy)* | **Out of Scope / N/A for rules engine.** Digital rules engines automatically enforce all mandatory triggers and prompt players for explicit `"you may"` optional triggers (`OptionalSpec`). Never auto-skip mandatory triggers. | N/A |

### 6. Combat Rules (`CR 506–511`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T27** | **Multi-blocker & multi-attacker damage assignment** | `CR 510.1c–d` | *(Post-2024 CR update: no damage assignment order step!)* A `5/5` blocked by two `3/3`s can freely divide damage (`3+2`, `4+1`, `5+0`, etc.) via sequential/allocation choice; a `4/4` blocking two `2/2`s can divide `2+2` freely. (For Trample, see T30.) | M5, M6 |
| **T28** | **First strike & double strike** | `CR 510.4, 702.4, 702.7` | (a) First-strike attacker kills normal blocker in `FirstStrikeDamage` step → SBA kills blocker before `CombatDamage` step → dead blocker deals 0 damage; attacker remains blocked and deals no normal damage unless it has Trample (`510.1c`). (b) Double-strike attacker deals damage in both steps; losing first strike after `FirstStrikeDamage` still skips normal step only if creature already dealt first-strike damage without double strike (`702.7c`). (c) Priority + triggers execute between the two combat damage steps! | M5, M6 |
| **T29** | **Creatures leaving combat / blocked with no blockers** | `CR 506.4, 509.1h, 510.1c` | (a) Attacker bounced/killed/phased-out/turned-noncreature before damage deals no combat damage. (b) Blocker removed after `DeclareBlockers` leaves the attacker **still blocked** (`509.1h`): without Trample it assigns 0 damage (`510.1c`); with Trample it assigns full damage to the defender (`702.19d`). | M5, M6 |
| **T30** | **Trample + Deathtouch (+ Indestructible / Protection / Damage Prevention)** | `CR 702.2c, 702.19b` | `5/5` with Trample + Deathtouch blocked by an Indestructible `6/6` (or Protection-from-attacker `4/4`) needs to assign only **1 damage** (deathtouch lethal!) to the blocker and can assign **4 damage** to the defending player/planeswalker/battle (`702.19b` ignores prevention/indestructible/protection when computing lethal assignment!). | M5, M6 |
| **T31** | **Global attack/block requirements & restrictions** | `CR 508.1c–d, 509.1b–c` | Whole-set legality validation (not just per-creature): (a) Never violate a restriction ("can't attack alone", "at most 1 creature can attack", "can't attack you"). (b) Maximize satisfied requirements ("attacks each combat if able", Goad, "must be blocked by all creatures able to block it") without violating any restriction or forcing optional tax payment (`508.1d`). | M5, M6 |
| **T32** | **Multiplayer combat (multiple opponents, planeswalkers, battles)** | `CR 508.1b, 802` | In a 4-player Commander game, Attackers A, B, C simultaneously attack Opponent 1, Opponent 2's Planeswalker, and a Battle protected by Opponent 3; defending players declare blockers in APNAP order (`509.1`), and combat damage resolves simultaneously (`510.2`) tracking commander combat damage per commander per opponent (`903.10a`). | M5, M6 |

### 7. Weird Zone and Turn Mechanics (`CR 702.26, 708, 709, 715, 500, 903`)

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T33** | **Phasing** | `CR 702.26a–g` | Phasing out/in preserves `ObjectId` generation, counters, damage/status rules, and token existence; fires **zero** ETB/LTB triggers; attached Auras/Equipment phase out indirectly with host and return attached (`702.26g`); phased-out permanents are invisible to filters, layers, and board wipes. | M2, M5, M6 |
| **T34** | **Face-down permanents & hidden information** | `CR 708.2a, 708.5` | Face-down creature is a `2/2` colorless, nameless, typeless creature with no abilities and mana value `0`; its controller may view its true card identity while opponents' `ClientViewModel` and `determinize(opp, rng)` receive **zero** leakage of the underlying `CardDefId`. | M2, M3, M5, M6 |
| **T35** | **Morph, Disguise & Manifest** | `CR 702.37, 702.168, 701.34, 708.8` | Casting face-down for `{3}` (Ward `{2}` for Disguise); turning face-up is a **special action** (doesn't use stack, cannot be responded to, works through Split Second!); triggers "When ~ is turned face up" (`708.8`); manifested instants/sorceries cannot turn face-up (`701.34b`). | M2, M5, M6 |
| **T36** | **Adventure cards** | `CR 715.2–715.4` | Off-stack (hand/library/graveyard/battlefield) an Adventure card has **only** its creature face characteristics (`715.4`). Casting its Adventure half puts instant/sorcery characteristics on stack; on resolution it exiles itself (`715.3d`) and grants permission to cast the creature half from exile. If countered, it goes to graveyard (not "on an adventure"). | M2, M5, M6 |
| **T37** | **Split cards (& Fuse)** | `CR 709.4a–d` | In hand/library/graveyard/exile, mana value equals the **sum** of both halves and colors/types are the **union** (`709.4b`). On the stack, only the cast half exists (unless cast fused from hand per `702.102`, where both halves exist and resolve left-to-right). Alternate costs ("cast without paying mana cost" / Cascade) evaluate MV per `709.4b`! | M2, M5, M6 |
| **T38** | **Commander zone-change rules** | `CR 903.9a–b, 903.8` | (a) Commander dying → hits graveyard (fires "dies" triggers!) → `CR 903.9a` SBA prompts owner to move to command zone. (b) Commander exiled (or replaced to exile by *Rest in Peace*) → `CR 903.9a` SBA prompts owner. (c) Commander bounced to hand or tucked to library → `CR 903.9b` **replacement effect** (exempt from `614.5`) before zone change. (d) Commander tax (`+2` per prior cast from command zone, `903.8`) applies even on alternative costs, and does not count casts from hand/graveyard. | M2, M4, M5, M6 |
| **T39** | **Extra turns, skipped turns & skipped phases** | `CR 500.7, 500.11` | Extra turns execute in **LIFO** order (`500.7`: most recently created extra turn happens first); skipped turns/phases (`skipDrawStep`, `skipCombatPhase`, `skipExtraTurns`) cancel the matching step/turn cleanly and resume normal turn order afterward. | M5, M6 |
| **T40** | **Taking control of another player's turn** | `CR 718` (*Mindslaver*) | **Deferred (post-Phase 1/2).** Architectural readiness check: keep `controller` (of objects), `owner` (of cards), and `decision_actor` (who answers `DecisionState` in `MatchSession`) conceptually distinct in `state.rs`/`session.rs` so adding `turn_controller: Option<PlayerId>` later requires no rewrite. | Deferred |

### 8. Rules That Modify the Rules (`CR 101.2` Golden Rule: "Can't" Beats "Can")

| ID | Rule / Mechanic | CR Citation | Required Test Assertions (Synthetic Cards) | Milestone |
|---|---|---|---|---|
| **T41** | **"Play as though it had flash" & zone-play permissions** | `CR 601.3b, 601.3e` | Static/temporary `CastAsThoughFlash` (e.g., *Vedalken Orrery*, *Leyline of Anticipation*) permits casting sorceries/creatures on opponents' turns and with non-empty stack; `ZonePlayPermission` permits playing lands/spells from top of library, graveyard, or exile while respecting per-turn slot limits. | M5, M6 |
| **T42** | **Centralized "can't" restrictions (`CR 101.2`)** | `CR 101.2` | `RuleMod::OneSpellPerTurn`, `CantCastMatchingSpells`, `OpponentsCantCastDuringYourTurn`, `CantGainLife`, `CantDrawExtraCards`, `CantSearchLibraries`, `CantActivateAbilities` override permissive grants ("can't" always beats "can": e.g., *Rule of Law* overrides *Flash* / free-cast grants!). | M5, M6 |
| **T43** | **"You can't lose the game" / "Opponents can't win"** | `CR 104.3b, 104.3h` | Player with `RuleMod::CantLose` (*Platinum Angel*) survives `0` life, `10` poison, `21` commander damage, and drawing from an empty library; the moment the permanent leaves the battlefield, the very next SBA pass (`704.5a/c`) causes them to lose immediately! | M5, M6 |
| **T44** | **Alternative win/loss conditions** | `CR 104.2b, 104.3` | Effect-based `EffectOp::Win { condition }` and `EffectOp::Lose` (e.g., *Laboratory Maniac* draw replacement, *Thassa's Oracle* resolution check, *Approach*-style win) evaluated cleanly alongside SBA losses and suppressed by `OpponentsCantWin` / `CantLose`. | M5, M6 |
| **T45** | **Configurable starting conditions & pregame actions** | `CR 103.6` | `GameConfig` supports arbitrary `starting_life` (`20`, `40`, custom), `starting_hand_size`, `player_count` (`2..=8`), and `StaticBody::PregameBeginOnBattlefield` (*Leyline* opening-hand pregame action). | M2, M5, M6 |
| **T46** | **Rule overrides (Legend-rule exemption, max hand size, combat damage by toughness)** | `CR 101.2, 704.5j, 510.1a` | (a) `RuleMod::LegendRuleExempt` (*Mirror Box*) disables `704.5j` SBA—and removing it immediately triggers the Legend Rule SBA on the next check! (b) `NoMaxHandSize` / `SetMaxHandSize` / `MaxHandSizeDelta` override cleanup discard (`514.1`). (c) `AssignCombatDamageByToughness` (*Arcades* / *Doran*) overrides `510.1a`. | M5, M6 |
| **T47** | **Ante and subgames** | `CR 407, 728` (*Shahrazad*) | **Out of Scope / N/A.** Banned in all formats; explicitly excluded. | N/A |

---

## Architectural Alignment Check (Mapping the 9 Core Abstractions)

Every one of the 9 recommended architectural abstractions maps directly onto `crates/mtg-engine/src/`:

| Abstraction | `mtg-engine` Module(s) | Core Invariant Enforced |
|---|---|---|
| 1. **Game objects & identity** | `id.rs`, `state.rs`, `faces.rs` | Separate immutable base `CardDef` (`Arc<CardDatabase>`) + generational `ObjectId { index, generation }` from evaluated characteristics (`EvaluatedObject`). |
| 2. **Zones** | `zones.rs` | Single `move_object()` funnel for **all 7 zones including `Stack`** (`CR 400.7`). |
| 3. **Events** | `events.rs` | Atomic batch emission with pre-batch LKI (`CR 603.10a`). |
| 4. **Replacement effects** | `replace.rs` | Unified `CR 614–616` pipeline (`614.5` dedup + `616.1a–d` tiers + serializable `PendingReplacementState`). |
| 5. **Triggered abilities** | `triggers.rs` | Event-driven queue, intervening-if (`603.4`), APNAP (`603.3b`), state/delayed/reflexive triggers. |
| 6. **Continuous effects** | `layers/{mod,deps,cache,predicates}.rs` | `CR 613` layers + `CR 613.8` dependency graph + O(1) `DirtyMask`/`layer_rev` cache verified by `--features verify-layer-cache`. |
| 7. **State-based actions** | `sba.rs` | Iterative fixpoint loop (`704.3`) with pre-annihilation LKI (`704.8`) stabilizing before trigger flush. |
| 8. **Action legality** | `legality.rs`, `targets.rs`, `costs.rs` | Centralized `CR 101.2` ("can't" beats "can") checks driving both `legal_actions()` and `apply_action()`. |
| 9. **Priority & turn structure** | `turn.rs`, `stack.rs`, `combat.rs`, `choices.rs` | Explicit turn scheduler, APNAP priority passes, pausable `ResolutionFrame` stack, and 6-kind `ChoiceRequest` protocol. |
