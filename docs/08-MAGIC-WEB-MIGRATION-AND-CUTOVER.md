# 08 — `magic-web` Architecture, SQLite Migration, UI Decoupling & Production Cutover

This document covers Phase 4: building the private **`magic-web`** repository for `magic.peckhamr.com`, migrating existing users/decks from SQLite, adapting the 26k-LOC Svelte 5 frontend into a thin WebSocket client + lazy WASM solitaire/deck tool, and cleanly pruning all Magic code from `peckhamr.com`.

---

## 1. `magic-web` Repository Layout

```text
magic-web/
├── server/                      # Rust axum homelab backend
│   ├── Cargo.toml               # Depends on mtg-ir, mtg-engine, mtg-ai, mtg-compiler (or cards.bin)
│   └── src/
│       ├── main.rs              # CLI/env config, graceful shutdown, /healthz
│       ├── auth.rs              # Google + GitHub OAuth, guest login, HttpOnly magic_session cookies
│       ├── decks.rs             # CRUD + Scryfall import/search + precon cloning + MagicCardImages cache
│       ├── lobby.rs             # In-memory LobbyActor (tokio task per active lobby in DashMap)
│       ├── ws.rs                # WebSocket upgrade, reconnect catch-up, chat, presence, rope timers
│       ├── bot_runner.rs        # Spawns ISMCTS on rayon blocking pool with configurable botSpeed pacing
│       └── db.rs                # rusqlite (WAL mode): migrated user/deck tables + event-sourced game tables
├── web/                         # SvelteKit static frontend (deployed to Cloudflare Pages / Workers Assets)
│   ├── svelte.config.js         # @sveltejs/adapter-static (pure static SPA + lazy WASM)
│   └── src/
│       ├── routes/+page.svelte  # Adapted from peckhamr.com/src/routes/magic/+page.svelte
│       └── lib/
│           ├── wsClient.ts      # Reconnecting WebSocket + REST client (replaces HTTP polling!)
│           ├── wasmSolo.ts      # Lazy dynamic import of mtg-wasm for offline solitaire & deck checks
│           ├── cardMeta.ts      # UI-only card art/flavor/text metadata store (Scryfall cache)
│           └── ui/              # Adapted ArenaTable.svelte + all src/lib/magic/ui/*.svelte components
└── ops/
    ├── magic-web.service        # Systemd unit for Rust axum binary (homelab)
    ├── backup.sh                # Nightly SQLite VACUUM INTO + PRAGMA quick_check
    ├── deploy.sh                # Atomic release symlink + /healthz check + rollback
    └── cloudflared-config.yml   # Tunnel routing magic-api.peckhamr.com (or /api & /ws) -> 127.0.0.1:8787
```

---

## 2. Backend Server Architecture (`magic-web/server/`)

### 2.1 In-Memory Lobby Actors + WebSockets (Replacing HTTP Polling)
Today `peckhamr.com` polls `/api/lobby?code=...` via HTTP, deserializing 670 KB JSON from SQLite, cloning via `JSON.parse(JSON.stringify)`, and re-serializing on every request.

In `magic-web/server/`:
- Each active lobby runs as a lightweight `tokio` actor (`LobbyActor`) holding a live `MatchSession` in RAM (~20 KB per game) behind a `DashMap<LobbyCode, mpsc::Sender<LobbyCmd>>`.
- Connected clients hold a WebSocket (`/ws?code=ABCDEF`); auth uses the existing `magic_session` `HttpOnly` cookie or `X-Magic-Session` header.
- On every `Action` / `MacroAction`:
  1. `LobbyActor` validates and applies the action in-memory (<0.2 ms).
  2. Appends the compact serialized `Action` (~20–100 bytes) to `MagicGameEvents` in SQLite (WAL mode).
  3. Every `N` actions (e.g. every turn boundary or 50 actions), writes a compact binary `postcard` snapshot (~5–15 KB) to `MagicGameSnapshots` for instant restart recovery.
  4. Computes per-seat redacted `ClientViewModel` (~5–20 KB) and pushes it immediately over each connected player's WebSocket.
- **Bot turns:** When a bot has the decision, `LobbyActor` sends a cloned `GameState` to a bounded `rayon` pool (`bot_runner.rs`) for ISMCTS search, applies the chosen action when the pacing delay (`botSpeed`) elapses, and pushes updates in real time—never blocking other lobbies or WebSocket heartbeats!

### 2.2 SQLite Schema & Lossless Migration from TS

**Preserved tables (copied 1:1 from existing homelab/D1 SQLite via migration script):**
- `MagicUsers` (`id, provider, provider_id, email, username, avatar_url, stats_json, created_at, last_login_at`)
- `MagicSessions` (`token, user_id, expires_at, created_at`)
- `MagicDecks` (`id, user_id, name, description, commanders_json, cards_json, color_identity, card_count, is_legal, validation_errors_json, win_count, play_count, created_at, updated_at`)
- `MagicChatMessages` (`id, lobby_code, user_id, username, avatar_url, seat_idx, kind, message, created_at`)
- `MagicCardImages` (`key, image_url, art_crop_url, updated_at`)

**New event-sourced game tables (replacing `MagicLobbies.game_state_json`):**
```sql
CREATE TABLE IF NOT EXISTS MagicLobbiesV2 (
    code TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    host_user_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'waiting', -- waiting | active | finished
    is_private INTEGER NOT NULL DEFAULT 1,
    config_json TEXT NOT NULL,              -- GameConfig JSON
    seats_json TEXT NOT NULL,
    rng_seed INTEGER,                       -- u64 seed locked at game start
    decks_json TEXT,                        -- Resolved CardDefId / oracle_id decklists at game start
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS MagicGameEvents (
    lobby_code TEXT NOT NULL,
    seq INTEGER NOT NULL,                   -- 0, 1, 2, ...
    actor_seat INTEGER NOT NULL,
    action_bytes BLOB NOT NULL,             -- postcard-encoded Action
    created_at INTEGER NOT NULL,
    PRIMARY KEY (lobby_code, seq)
);

CREATE TABLE IF NOT EXISTS MagicGameSnapshots (
    lobby_code TEXT NOT NULL,
    seq INTEGER NOT NULL,
    state_bytes BLOB NOT NULL,              -- postcard-encoded GameState
    PRIMARY KEY (lobby_code, seq)
);
```

- **Crash/reboot recovery:** On boot, `server` loads `MagicLobbiesV2 WHERE status = 'active'`, restores the latest `MagicGameSnapshots` row, and replays any tail `MagicGameEvents WHERE seq > snapshot.seq` in <5 ms.
- **Free AI training dataset:** Completed games in `MagicLobbiesV2` + `MagicGameEvents` export directly into `ReplayLog` files consumable by `mtg-py` / `mtg-cli`!

### 2.3 API Endpoint Compatibility
Preserve the exact auth & deck flows from `peckhamr.com/src/routes/magic/api/`:
- `/api/healthz` → `{ ok, mode: "homelab-rust", engineVersion, irVersion, uptimeSec, dbOk, activeLobbies }`
- `/api/auth` → `me`, `login`, `callback` (Google + GitHub), `guest_login`, `dev_login` (dev only), `logout`
- `/api/decks` → `GET` list + precons, `POST` `clone_precon`, `delete`, `update_card`, `add_card`, `search_scryfall`, `import`
- `/api/lobby` → `GET` user lobbies, `POST` `create`, `join`, `leave`, `delete_lobby`, `transfer_host`, `select_deck`, `add_bot`, `remove_bot`, `update_rules`, `start_game`
- `/ws?code=...` → Live match actions (`MacroAction` / `Action`), `hover_breakdown`, `preview_auto_tap`, chat messages, presence, and real-time `ClientViewModel` pushes.

---

## 3. Decoupling the 26k-LOC Svelte 5 UI (`5.1 MB` → `~200 KB` Initial JS)

Today, `ArenaTable.svelte` and `src/lib/magic/ui/` bundle **5.1 MB minified (1.22 MB gzipped)** JS because 5 UI helper modules import the TS engine directly. When copying `src/lib/magic/ui/` and `ArenaTable.svelte` into `magic-web/web/`, decouple **only these 5 seams**:

| Current TS File in `peckhamr.com` | Current Engine Coupling | Thin-Client Replacement in `magic-web/web/` |
|---|---|---|
| `src/routes/magic/+page.svelte` | Imports `inspectCardContinuousBreakdown` from `engine.ts`; polls `/api/lobby` via `setInterval`. | Replace polling with `wsClient.ts` WebSocket stream; read `hoverBreakdown` directly from `ClientViewModel.permanents[id].breakdown` (zero engine call!). |
| `src/lib/magic/ui/boardViewModel.ts` (3,120 L) | Imports `compileCard`, `evaluatePermanent`, `withStableLayerInputs`, and ~15 `runtime/*` modules to compute evaluated P/T, castable cards, keywords, and stax badges. | **Delete all rules computation from the browser!** Server (`mtg-engine::view`) computes `ClientViewModel` directly; `boardViewModel.ts` shrinks to a ~300-line pure UI presentation mapper (joining `cardDefId` with UI image/art URLs from `cardMeta.ts`). |
| `src/lib/magic/ui/autoTapPreview.ts` (504 L) | Imports `evaluatePermanent`, `runtime/costs.ts`, `runtime/mana.ts`, `runtime/layers.ts`. | Read default auto-tap highlights precomputed per castable hand/board action inside `ClientViewModel` (and send a lightweight WS `preview_auto_tap` RPC only when user tweaks custom X/kicker sliders; in solo mode, call `wasmMatch.preview_auto_tap()`). |
| `src/lib/magic/ui/targetingController.ts` (1,634 L) & `combatController.ts` (509 L) | Import legality & target candidate helpers from `engine.ts`. | Legal targets, legal attackers/defenders, and legal blockers arrive precomputed inside `ClientViewModel.legalActions` / `decisionContext`; controllers become pure UI selection state machines. |
| `src/lib/magic/ui/lobby/DeckInspectorModal.svelte` & `DeckManagerPanel.svelte` | Import `deckUnsupportedCards` / `cardAbilityDisplayText` from `compiler/support.ts`. | Read support reports from `/api/decks` (server-validated via `CardDatabase`) or lazy-loaded `mtg-wasm` when offline. |

### Pure Presentation Components Reused Nearly As-Is
`ManaSymbol.svelte`, `ManaText.svelte`, `PermanentCard.svelte`, `HandTray.svelte`, `HudBar.svelte`, `OpponentPod.svelte`, `PlayerVitals.svelte`, `RightDock.svelte`, `CardInspector.svelte`, `AltHoverPreview.svelte`, `CastOptionsModal.svelte`, `ChoicePromptModal.svelte`, `ModePickerModal.svelte`, `XCostModal.svelte`, `XValueChooser.svelte`, `ZoneBrowserModal.svelte`, `AllocationStepperWidget.svelte`, `GuidedResolutionBuilder.svelte`, `SearchableOptionPicker.svelte`, `TargetBanner.svelte`, `VictoryModal.svelte`, `AbilityMenuModal.svelte`, `vfx.ts`, `stickyScroll.ts`, and `lobby/*` keep their Svelte 5 markup, CSS, and layout intact—only swapping their prop types to `ClientViewModel` types generated from Rust!

### Offline Solitaire / Goldfish Mode (Lazy `mtg-wasm`)
- Default multiplayer page load downloads **zero WASM** (~200 KB Svelte UI only).
- Clicking **"Solo / Goldfish"** (or falling back automatically when `/api/healthz` detects homelab maintenance/reboot) dynamically `import()`s `mtg-wasm` + a deck-scoped `cards.bin` pack and mounts the exact same `ArenaTable.svelte` against `WasmMatch` instead of `wsClient.ts`!

---

## 4. Hosting, Beta Soak & Cutover Runbook

### 4.1 Hosting Topology (`MAGIC_MODE=cloudflare` Retired)
- **Frontend (`magic.peckhamr.com`):** Deployed from `magic-web/web/` to Cloudflare Pages (or Cloudflare Workers Static Assets) with a tiny routing rule/Worker forwarding `/api/*` and `/ws` to the homelab `cloudflared` tunnel (`https://magic-backend.peckhamr.com`, carrying `X-Magic-Proxy-Secret` just like `src/hooks.server.ts` does today, plus native WebSocket pass-through).
- **Backend (`magic-backend.peckhamr.com`):** Single compiled Rust `axum` binary on homelab behind `systemd` + `cloudflared` + nightly SQLite backup timer (`VACUUM INTO`).
- **D1 fallback retired:** Once live, `magic.peckhamr.com` no longer maintains a duplicate D1/Workers backend. If the homelab reboots, Cloudflare still serves the Svelte shell with a clean offline banner + working offline WASM solitaire.

### 4.2 Beta Soak Checklist (`beta.magic.peckhamr.com`)
Before touching production `magic.peckhamr.com`:
1. Deploy `magic-web` to `beta.magic.peckhamr.com` backed by a snapshot copy of production `magic.sqlite` (migrated to include `MagicLobbiesV2` / `MagicGameEvents` / `MagicGameSnapshots`).
2. Add `https://beta.magic.peckhamr.com/api/auth?action=callback&provider=google` in Google OAuth console for live auth testing.
3. Verify:
   - Full 4-player Commander matches against 3 `Tactical`/`Strong` ISMCTS bots (check latency, RAM usage, CPU cores utilized).
   - Human-vs-human multiplayer across two browsers (verify instant WebSocket updates, hidden-zone redaction, chat, rope timer).
   - Server restart mid-game (`sudo systemctl restart magic-web`) → clients auto-reconnect and resume exact game state from snapshot + event log.
   - Offline WASM solitaire mode with homelab stopped (verify lazy WASM load, sub-ms UI previews, local bot turns).
   - Existing migrated decks open cleanly in `DeckInspectorModal` with `100%` precon support.

### 4.3 Production Cutover & `peckhamr.com` Cleanup
1. Freeze writes on TS backend (`MAGIC_READ_ONLY=1`), export final SQLite/D1 snapshot, run migration into `magic-web` homelab SQLite (`MagicUsers`, `MagicSessions`, `MagicDecks`, `MagicChatMessages`, `MagicCardImages`).
2. Update Cloudflare routing so `magic.peckhamr.com` points to `magic-web`'s Cloudflare Pages/Worker deployment (OAuth callback URLs `https://magic.peckhamr.com/api/auth?action=callback&provider=google|github` stay **unchanged**!).
3. Verify `https://magic.peckhamr.com/api/healthz` reports `mode: "homelab-rust"` and run smoke tests.
4. **Prune Magic from `peckhamr.com`** (in a clean dedicated PR once production has soaked cleanly for ~1 week):
   - Remove `{ pattern = "magic.peckhamr.com", custom_domain = true }` and `MAGIC_MODE` vars from `wrangler.toml`.
   - Remove `'magic'` from `APPS` in `src/lib/apps.ts` (keep redirect from `peckhamr.com/magic` → `https://magic.peckhamr.com` in `src/hooks.server.ts` so old bookmarks still work!).
   - Delete `src/lib/magic/`, `src/routes/magic/`, `server/magic-backend/`, `ops/magic-backend/`, `scripts/magic-*.ts`, `scripts/build-magic-backend.mjs`, `scripts/migrate-magic*`, `scripts/export-ir-v2.ts`, `scripts/record-ts-replay.ts`, and `.github/workflows/magic-engine.yml`.
   - Keep the portfolio link card on `peckhamr.com/+page.svelte` pointing to `https://magic.peckhamr.com` (and the public GitHub repo for `mtg-engine`).
