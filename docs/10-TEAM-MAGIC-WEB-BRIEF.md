# 10. Parallel Team Brief — `magic-web` (Server, Thin Svelte 5 Client & Homelab Ops)

> **Audience:** Autonomous agent team building `/home/ryan/statikana/magic-web` in parallel with the core `mtg-engine` (Milestones M3–M7) and `mtg-compiler` teams.
> **Target Repository:** `/home/ryan/statikana/magic-web` (private repository for `magic.peckhamr.com`).
> **Status:** Ready to start immediately (zero git or Cargo contention with `mtg-engine`).

---

## 1. Mission

Build `magic-web` (`/home/ryan/statikana/magic-web`), the private web application and Rust `axum` homelab backend for `magic.peckhamr.com`, decoupled from `/home/ryan/statikana/peckhamr.com` and designed to plug cleanly into `/home/ryan/statikana/mtg-engine`.

---

## 2. Context & Required Reading

Before writing code, read these documents from `/home/ryan/statikana/mtg-engine/docs/`:
1. [`01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md`](./01-HIGH-LEVEL-VISION-AND-TOPOLOGY.md) — 3-repo split & ownership boundaries
2. [`02-IR-V2-AND-CARD-DATABASE.md`](./02-IR-V2-AND-CARD-DATABASE.md) — `CardDatabase`, `cards.bin` (`MTGC` + `postcard`/`zstd`)
3. [`04-ACTION-PROTOCOL-AND-AI-DESIGN.md`](./04-ACTION-PROTOCOL-AND-AI-DESIGN.md) — sequential `Action` vs. UI `MacroAction` & 6-kind `ChoiceRequest`
4. [`05-BINDINGS-WASM-PYO3-CLI.md`](./05-BINDINGS-WASM-PYO3-CLI.md) — `MatchSession` façade & lazy `mtg-wasm` solitaire/deck model
5. [`08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md`](./08-MAGIC-WEB-MIGRATION-AND-CUTOVER.md) — exact `magic-web` layout, SQLite schema, API endpoints, Svelte 5 decoupling seams, and hosting/ops plan

Also inspect (**READ-ONLY**) the existing TS implementation in `/home/ryan/statikana/peckhamr.com`:
- **Backend / API / DB reference:**
  - `src/lib/magic/db.ts`
  - `src/routes/magic/api/{auth,decks,lobby,healthz}/+server.ts`
  - `server/magic-backend/main.ts`
  - `ops/magic-backend/*`
  - `ops/app-subdomains/README.md`
- **Svelte 5 UI to copy & adapt:**
  - `src/routes/magic/+page.svelte`
  - `src/lib/magic/ArenaTable.svelte`
  - `src/lib/magic/ui/**`
  - `src/lib/magic/{ManaSymbol.svelte,ManaText.svelte,vfx.ts,cardImages.ts,search.ts}`
- **Engine contract reference (READ-ONLY):**
  - `/home/ryan/statikana/mtg-engine/crates/mtg-engine/src/{session.rs,view.rs,action.rs,config.rs,replay.rs}`
  - `/home/ryan/statikana/mtg-engine/crates/mtg-ir/src/lib.rs`

---

## 3. Critical Parallel-Work Safety Rules

1. **NEVER edit files inside `/home/ryan/statikana/mtg-engine` or `/home/ryan/statikana/peckhamr.com`.** Another agent team is actively implementing Milestones M3–M7 inside `mtg-engine` right now. All writes belong strictly inside `/home/ryan/statikana/magic-web`.
2. **Isolate from transient M3–M5 churn in `mtg-engine`:**
   - `mtg-ir` is already **LOCKED** (M1a complete). The server may depend on `mtg-ir` directly (`path = "../../mtg-engine/crates/mtg-ir"` — or test against `cards.bin` generated into `/tmp/`).
   - For `MatchSession` (still being populated across M3–M5c), define a clean local trait `server/src/engine_bridge.rs` (`trait GameEngineSession`) mirroring `mtg_engine::MatchSession` (`new`, `legal_actions`, `apply_action`, `apply_macro_action`, `view_model`, `hover_breakdown`, `preview_auto_tap`, `replay_log`, `snapshot_bytes`), backed by a compiling `StubSession` for tests & lobby wiring right now, plus a feature-flagged `real-engine` adapter ready to flip on once M5c lands.
3. **IP & Data Hygiene:** Never commit Scryfall bulk data, `cards.bin`, or `.sqlite` databases into git.

---

## 4. Deliverables & Milestones

### Milestone W1 — Repo Scaffold, SQLite Layer & Data Migration Tool
- Initialize `/home/ryan/statikana/magic-web` with `server/`, `web/`, `ops/`, `docs/`, and `.gitignore`.
- Implement `server/src/db.rs` (`rusqlite` in WAL mode):
  - Schema preserving `MagicUsers`, `MagicSessions`, `MagicDecks`, `MagicChatMessages`, and `MagicCardImages` 1:1 with `peckhamr.com/src/lib/magic/db.ts`.
  - Event-sourced match tables: `MagicLobbiesV2`, `MagicGameEvents` (`postcard` action bytes), and `MagicGameSnapshots` (`postcard` snapshot bytes).
  - Write a tested migration tool/command importing an existing TS SQLite export (`MagicUsers`, `MagicSessions`, `MagicDecks`, `MagicChatMessages`, `MagicCardImages`) losslessly into the `magic-web` schema.

### Milestone W2 — Rust `axum` Server (Auth, Decks, Lobbies, WebSocket Actors)
- `auth.rs`: Exact endpoint parity with `src/routes/magic/api/auth/+server.ts`:
  - `me`, `login`, `callback` (Google + GitHub), `guest_login`, `dev_login` (behind dev flag), `logout`
  - `HttpOnly; SameSite=Lax` `magic_session` cookie, `X-Magic-Session` header, and constant-time `X-Magic-Proxy-Secret` verification for Cloudflare proxy requests.
- `decks.rs`: Exact parity with `src/routes/magic/api/decks/+server.ts`:
  - `GET` user decks + precons, `clone_precon`, `delete`, `update_card`, `add_card`, `search_scryfall`, `import`, `MagicCardImages` caching
  - **Real deck legality & support validation against `Arc<mtg_ir::CardDatabase>` loaded from `cards.bin`.**
- `lobby.rs` & `ws.rs`:
  - REST lobby endpoints (`create`, `join`, `leave`, `delete_lobby`, `transfer_host`, `select_deck`, `add_bot`, `remove_bot`, `update_rules`, `start_game`) + `/api/healthz` (`mode: "homelab-rust"`).
  - In-memory `LobbyActor` per active lobby (`tokio` task in `DashMap`), WebSocket endpoint (`/ws?code=...`) handling live chat, presence, reconnect state catch-up, rope timers, action dispatch via `GameEngineSession`, event-log persistence, and periodic snapshots.
- Full Rust integration test suite (`cargo test` covering auth, SQLite migration, deck validation against a fixture `cards.bin`, lobby lifecycle, WebSocket broadcast, and server-restart recovery from event log + snapshot).

### Milestone W3 — Rich `ClientViewModel` Contract & Svelte 5 Thin Client (`web/`)
- Author `docs/CLIENT_VIEW_MODEL_CONTRACT.md` defining the complete JSON `ClientViewModel` schema needed by `ArenaTable.svelte` (players, life/poison/energy/mana pools/commander damage & tax, evaluated permanents with P/T/types/keywords/counters/attachments/hover breakdowns, stack items, combat arrows/attackers/blockers, active decision/prompt payloads, legal cast options & default auto-tap highlights) so `mtg-engine` M5c can implement the exact same wire format.
- Scaffold `web/` as a static SvelteKit 3 + Svelte 5 SPA (`@sveltejs/adapter-static`, Vite 8) for Cloudflare Pages / Workers Static Assets.
- Copy and adapt the Svelte 5 UI from `peckhamr.com`:
  - Wire Auth, Lobby Hub, Active Pod, Chat, Deck Manager, Deck Import, and Deck Inspector directly to `server/` REST + `wsClient.ts` (zero HTTP polling for live lobbies).
  - Copy `ArenaTable.svelte` and `src/lib/magic/ui/*`, **stripping ALL direct TS `engine.ts` / `compiler/` / `runtime/*` imports** (decoupling the 5 seams documented in `docs/08` §3: `+page.svelte`, `boardViewModel.ts`, `autoTapPreview.ts`, `targetingController.ts` + `combatController.ts`, `DeckInspectorModal.svelte`) behind a single `web/src/lib/viewAdapter.ts` mapping `ClientViewModel` + `cardMeta.ts` to component props.
  - Add `web/src/lib/wasmSolo.ts` as a lazy dynamic-import shell for offline solitaire/goldfish (wired cleanly to load `mtg-wasm` once M5c/Phase 3 publishes it) and a clean offline banner when `/api/healthz` is unreachable.
- Verify `npm run check` and `npm run build` succeed cleanly and confirm initial client JS bundle is **`< 500 KB` uncompressed** (down from 5.1 MB).

### Milestone W4 — Homelab & Cloudflare Ops (`ops/`)
- Port and adapt `ops/magic-backend/*` from `peckhamr.com` for the Rust binary:
  - `magic-web.service` (systemd)
  - `deploy.sh` (atomic release symlink + `/api/healthz` check + auto-rollback)
  - `backup.sh` (`VACUUM INTO` + `PRAGMA quick_check`)
  - `cloudflared` config template
  - Minimal Cloudflare Worker static-assets + `/api/*` + `/ws` proxy template

---

## 5. Verification Protocol

- In `server/`: run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`.
- In `web/`: run `npm run check` and `npm run build`, verify the production JS bundle size (`< 500 KB` uncompressed initial load), and run live dev smoke tests against the local `axum` server.
- Make clean, milestone-scoped git commits inside `/home/ryan/statikana/magic-web`.
