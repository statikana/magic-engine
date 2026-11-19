//! `mtg-wasm`: Stateful WebAssembly handle over `mtg-engine`.
//!
//! `CardDatabase` and `GameState` stay pinned in WASM linear memory so JS only passes small
//! actions/queries across the boundary and reads back compact redacted `ClientViewModel`s.
//! Never embeds `mtg-compiler`.

use std::sync::Arc;

use mtg_engine::{Action, DeckSpec, GameConfig, MacroAction, MatchSession, PlayerId};
use mtg_ir::CardDatabase;

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub struct WasmCardDb {
    inner: Arc<CardDatabase>,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
impl WasmCardDb {
    /// Load a raw (uncompressed) `cards.bin` buffer into WASM linear memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<WasmCardDb, String> {
        let db = CardDatabase::decode_bytes(bytes).map_err(|e| e.to_string())?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    pub fn card_count(&self) -> usize {
        self.inner.cards.len()
    }
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub struct WasmMatch {
    session: MatchSession,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
impl WasmMatch {
    pub fn new(
        db: &WasmCardDb,
        config_json: &str,
        decks_json: &str,
        seed: u64,
    ) -> Result<WasmMatch, String> {
        let config: GameConfig = serde_json::from_str(config_json).map_err(|e| e.to_string())?;
        let decks: Vec<DeckSpec> = serde_json::from_str(decks_json).map_err(|e| e.to_string())?;
        let session = MatchSession::new(Arc::clone(&db.inner), config, decks, seed)
            .map_err(|e| e.to_string())?;
        Ok(Self { session })
    }

    pub fn view_model_json(&self, viewer: Option<u8>) -> Result<String, String> {
        let vm = self.session.view_model(viewer.map(PlayerId));
        serde_json::to_string(&vm).map_err(|e| e.to_string())
    }

    pub fn apply_action_json(&mut self, player: u8, action_json: &str) -> Result<String, String> {
        let action: Action = serde_json::from_str(action_json).map_err(|e| e.to_string())?;
        let out = self
            .session
            .apply_action(PlayerId(player), action)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    pub fn apply_macro_action_json(
        &mut self,
        player: u8,
        macro_json: &str,
    ) -> Result<String, String> {
        let act: MacroAction = serde_json::from_str(macro_json).map_err(|e| e.to_string())?;
        let out = self
            .session
            .apply_macro_action(PlayerId(player), act)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }
}
