//! `mtg-wasm`: Stateful WebAssembly handle over `mtg-engine`.
//!
//! `CardDatabase` and `GameState` stay pinned in WASM linear memory so JS only passes small
//! actions/queries across the boundary and reads back compact redacted `ClientViewModel`s.
//! Never embeds `mtg-compiler`.

use std::sync::Arc;

use mtg_engine::{
    Action, CastRequest, DeckSpec, GameConfig, MacroAction, MatchSession, ObjectId, PlayerId,
    ReplayLog,
};
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

    pub fn validate_deck_json(&self, config_json: &str, deck_json: &str) -> Result<String, String> {
        let config: GameConfig = serde_json::from_str(config_json).map_err(|e| e.to_string())?;
        let deck: DeckSpec = serde_json::from_str(deck_json).map_err(|e| e.to_string())?;
        let report = MatchSession::validate_deck(&self.inner, &config, &deck);
        serde_json::to_string(&report).map_err(|e| e.to_string())
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

    pub fn decision_info_json(&self) -> Result<String, String> {
        let info = self.session.decision_info();
        serde_json::to_string(&info).map_err(|e| e.to_string())
    }

    pub fn legal_actions_json(&self, player: u8) -> Result<String, String> {
        let actions = self.session.legal_actions(PlayerId(player));
        serde_json::to_string(&actions).map_err(|e| e.to_string())
    }

    pub fn view_model_json(&self, viewer: Option<u8>) -> Result<String, String> {
        let vm = self.session.view_model(viewer.map(PlayerId));
        serde_json::to_string(&vm).map_err(|e| e.to_string())
    }

    pub fn hover_breakdown_json(
        &self,
        viewer: Option<u8>,
        object_index: u32,
        object_generation: u32,
    ) -> Result<String, String> {
        let oid = ObjectId {
            index: object_index,
            generation: object_generation,
        };
        let bd = self.session.hover_breakdown(viewer.map(PlayerId), oid);
        serde_json::to_string(&bd).map_err(|e| e.to_string())
    }

    pub fn preview_auto_tap_json(&self, player: u8, req_json: &str) -> Result<String, String> {
        let req: CastRequest = serde_json::from_str(req_json).map_err(|e| e.to_string())?;
        let preview = self.session.preview_auto_tap(PlayerId(player), &req);
        serde_json::to_string(&preview).map_err(|e| e.to_string())
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

    pub fn snapshot_bytes(&self) -> Result<Vec<u8>, String> {
        self.session.snapshot_bytes().map_err(|e| e.to_string())
    }

    pub fn from_snapshot(db: &WasmCardDb, bytes: &[u8]) -> Result<WasmMatch, String> {
        let session =
            MatchSession::from_snapshot(Arc::clone(&db.inner), bytes).map_err(|e| e.to_string())?;
        Ok(Self { session })
    }

    pub fn replay_log_json(&self) -> Result<String, String> {
        serde_json::to_string(self.session.replay_log()).map_err(|e| e.to_string())
    }

    pub fn from_replay(db: &WasmCardDb, log_json: &str) -> Result<WasmMatch, String> {
        let log: ReplayLog = serde_json::from_str(log_json).map_err(|e| e.to_string())?;
        let session =
            MatchSession::from_replay(Arc::clone(&db.inner), log).map_err(|e| e.to_string())?;
        Ok(Self { session })
    }
}

#[cfg(test)]
mod tests {
    use mtg_engine::{ClientViewModel, DecisionInfo, DeckValidationReport};
    use mtg_ir::{
        CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, PrintedPt, SupertypeMask,
    };

    use super::*;

    fn make_wasm_db_bytes() -> Vec<u8> {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards: vec![
                CardDef {
                    oracle_id: "synth_wasm_land".into(),
                    compiler_version: 46,
                    text_hash: "wasm_0".into(),
                    name: "Synth Plains".into(),
                    layout: CardLayout::Normal,
                    faces: vec![FaceDef {
                        name: "Synth Plains".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::default(),
                            types: CardTypeMask::LAND,
                            subtypes: Default::default(),
                            supertypes: SupertypeMask::BASIC,
                            colors: ColorMask::empty(),
                            color_indicator: None,
                            pt: None,
                            loyalty: None,
                            defense: None,
                        },
                        abilities: vec![],
                        has_read_ahead: false,
                        prototype_spec: None,
                        cleaved_spell_spec: None,
                        gift_promised_spell_spec: None,
                    }],
                    fully_supported: true,
                },
                CardDef {
                    oracle_id: "synth_wasm_bear".into(),
                    compiler_version: 46,
                    text_hash: "wasm_1".into(),
                    name: "Synth Bear".into(),
                    layout: CardLayout::Normal,
                    faces: vec![FaceDef {
                        name: "Synth Bear".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::parse("{1}{W}"),
                            types: CardTypeMask::CREATURE,
                            subtypes: Default::default(),
                            supertypes: SupertypeMask::empty(),
                            colors: ColorMask::W,
                            color_indicator: None,
                            pt: Some(PrintedPt::Fixed {
                                power: 2,
                                toughness: 2,
                            }),
                            loyalty: None,
                            defense: None,
                        },
                        abilities: vec![],
                        has_read_ahead: false,
                        prototype_spec: None,
                        cleaved_spell_spec: None,
                        gift_promised_spell_spec: None,
                    }],
                    fully_supported: true,
                },
            ],
            decks: vec![],
        };
        CardDatabase::from_bundle(bundle)
            .unwrap()
            .encode_raw()
            .unwrap()
    }

    #[test]
    fn wasm_db_and_match_json_snapshot_replay_round_trip() {
        let bytes = make_wasm_db_bytes();
        let db = WasmCardDb::from_bytes(&bytes).unwrap();
        assert_eq!(db.card_count(), 2);

        let config = GameConfig::standard_1v1();
        let config_json = serde_json::to_string(&config).unwrap();
        let mut mainboard = vec![CardDefId(0); 24];
        mainboard.extend(vec![CardDefId(1); 36]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        let deck_json = serde_json::to_string(&deck).unwrap();
        let report_json = db.validate_deck_json(&config_json, &deck_json).unwrap();
        let report: DeckValidationReport = serde_json::from_str(&report_json).unwrap();
        assert!(report.is_legal);

        let decks_json = serde_json::to_string(&vec![deck.clone(), deck]).unwrap();
        let mut m = WasmMatch::new(&db, &config_json, &decks_json, 42).unwrap();

        let info: DecisionInfo = serde_json::from_str(&m.decision_info_json().unwrap()).unwrap();
        assert_eq!(info.actor, Some(PlayerId(0)));

        let legal0: Vec<Action> = serde_json::from_str(&m.legal_actions_json(0).unwrap()).unwrap();
        assert!(legal0.contains(&Action::KeepHand));

        let vm_spec: ClientViewModel =
            serde_json::from_str(&m.view_model_json(None).unwrap()).unwrap();
        assert!(vm_spec.players.iter().all(|p| p.hand.is_empty()));

        let vm0: ClientViewModel =
            serde_json::from_str(&m.view_model_json(Some(0)).unwrap()).unwrap();
        let p0_card = vm0.players[0].hand[0].id;
        let p1_opp_vm: ClientViewModel =
            serde_json::from_str(&m.view_model_json(Some(1)).unwrap()).unwrap();
        let p1_card = p1_opp_vm.players[1].hand[0].id;

        // Invalid and hidden-hand hover_breakdown_json both serialize to JSON "null".
        assert_eq!(m.hover_breakdown_json(None, 9999, 1).unwrap(), "null");
        assert_eq!(
            m.hover_breakdown_json(Some(0), p1_card.index, p1_card.generation)
                .unwrap(),
            "null"
        );

        let req_json = serde_json::to_string(&CastRequest {
            object: p0_card,
            option_id: 0,
            x_value: None,
        })
        .unwrap();
        let preview_json = m.preview_auto_tap_json(0, &req_json).unwrap();
        assert!(preview_json.contains("can_pay"));

        let keep_json = serde_json::to_string(&Action::KeepHand).unwrap();
        m.apply_action_json(0, &keep_json).unwrap();
        m.apply_action_json(1, &keep_json).unwrap();

        let snap = m.snapshot_bytes().unwrap();
        let restored_snap = WasmMatch::from_snapshot(&db, &snap).unwrap();
        assert_eq!(
            restored_snap.view_model_json(None).unwrap(),
            m.view_model_json(None).unwrap()
        );

        let log_json = m.replay_log_json().unwrap();
        let restored_rep = WasmMatch::from_replay(&db, &log_json).unwrap();
        assert_eq!(
            restored_rep.view_model_json(None).unwrap(),
            m.view_model_json(None).unwrap()
        );
    }
}
