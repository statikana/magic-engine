//! `mtg-py`: PyO3 Python bindings and batched `VecEnv` stepping for `mtg-engine`.
//!
//! Build the Python extension with `--features extension-module` (via `maturin`).
//! Keeping `pyo3` optional ensures `cargo test --workspace` succeeds without Python headers.

use mtg_engine::{DeckSpec, GameConfig, MatchSession, Observation, PlayerId};
use mtg_ir::CardDatabase;
use std::sync::Arc;

/// Native handle backing both the Rust tests and the optional PyO3 `PyVecEnv` class.
pub struct VecEnvCore {
    envs: Vec<MatchSession>,
}

impl VecEnvCore {
    pub fn new(
        db: Arc<CardDatabase>,
        config: GameConfig,
        decks: Vec<DeckSpec>,
        seeds: &[u64],
    ) -> Result<Self, mtg_engine::EngineError> {
        let mut envs = Vec::with_capacity(seeds.len());
        for &seed in seeds {
            envs.push(MatchSession::new(
                Arc::clone(&db),
                config.clone(),
                decks.clone(),
                seed,
            )?);
        }
        Ok(Self { envs })
    }

    pub fn len(&self) -> usize {
        self.envs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.envs.is_empty()
    }

    pub fn observe_all(&self, viewer: PlayerId) -> Vec<Observation> {
        self.envs.iter().map(|env| env.observe(viewer)).collect()
    }

    pub fn decision_infos(&self) -> Vec<mtg_engine::DecisionInfo> {
        self.envs.iter().map(MatchSession::decision_info).collect()
    }

    pub fn legal_actions_all(&self, viewer: PlayerId) -> Vec<Vec<mtg_engine::Action>> {
        self.envs
            .iter()
            .map(|env| env.legal_actions(viewer))
            .collect()
    }

    pub fn step_all(
        &mut self,
        actions: &[(PlayerId, mtg_engine::Action)],
    ) -> Result<Vec<mtg_engine::StepOutcome>, mtg_engine::EngineError> {
        if actions.len() != self.envs.len() {
            return Err(mtg_engine::EngineError::IllegalAction(format!(
                "step_all expected {} actions, got {}",
                self.envs.len(),
                actions.len()
            )));
        }
        let mut outcomes = Vec::with_capacity(self.envs.len());
        for (env, (player, action)) in self.envs.iter_mut().zip(actions.iter()) {
            outcomes.push(env.apply_action(*player, action.clone())?);
        }
        Ok(outcomes)
    }

    pub fn observe_flat_all(&self, viewer: PlayerId) -> Vec<mtg_engine::FlatObservation> {
        self.envs
            .iter()
            .map(|env| mtg_engine::encode_flat_observation(&env.observe(viewer)))
            .collect()
    }

    pub fn determinize_all(
        &self,
        viewer: PlayerId,
        rng: &mut mtg_engine::RngState,
    ) -> Vec<mtg_engine::GameState> {
        self.envs
            .iter()
            .map(|env| env.determinize(viewer, rng))
            .collect()
    }
}

#[cfg(feature = "extension-module")]
mod py_bindings {
    use super::*;
    use pyo3::exceptions::PyValueError;
    use pyo3::prelude::*;

    #[pyclass]
    pub struct PyCardDb {
        inner: Arc<CardDatabase>,
    }

    #[pymethods]
    impl PyCardDb {
        #[staticmethod]
        fn from_bytes(bytes: &[u8]) -> PyResult<Self> {
            let db = CardDatabase::decode_bytes(bytes)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Self {
                inner: Arc::new(db),
            })
        }

        fn card_count(&self) -> usize {
            self.inner.cards.len()
        }
    }

    #[pymodule]
    fn mtg_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PyCardDb>()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mtg_engine::{Action, RngState};
    use mtg_ir::{
        CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, PrintedPt, SupertypeMask, Zone,
    };

    use super::*;

    fn make_synth_db() -> Arc<CardDatabase> {
        let make_card =
            |id: u32, name: &str, types: CardTypeMask, pt: Option<(i32, i32)>| CardDef {
                oracle_id: format!("synth_py_{id}"),
                compiler_version: 46,
                text_hash: format!("py_{id}"),
                name: name.into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: name.into(),
                    characteristics: Characteristics {
                        mana_cost: if types.contains(CardTypeMask::LAND) {
                            ManaCost::default()
                        } else {
                            ManaCost::parse("{1}{R}")
                        },
                        types,
                        subtypes: Default::default(),
                        supertypes: SupertypeMask::empty(),
                        colors: if types.contains(CardTypeMask::LAND) {
                            ColorMask::empty()
                        } else {
                            ColorMask::R
                        },
                        color_indicator: None,
                        pt: pt.map(|(power, toughness)| PrintedPt::Fixed { power, toughness }),
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
            };
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    make_card(0, "Synth Mountain", CardTypeMask::LAND, None),
                    make_card(1, "Synth Goblin", CardTypeMask::CREATURE, Some((2, 1))),
                ],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn vec_env_core_batch_observe_step_and_determinize() {
        let db = make_synth_db();
        let mut mainboard = vec![CardDefId(0); 24];
        mainboard.extend(vec![CardDefId(1); 36]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        let mut vec_env = VecEnvCore::new(
            db,
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            &[10, 20, 30],
        )
        .unwrap();
        assert_eq!(vec_env.len(), 3);
        assert!(!vec_env.is_empty());

        let infos = vec_env.decision_infos();
        assert_eq!(infos.len(), 3);
        assert!(infos.iter().all(|i| i.actor == Some(PlayerId(0))));

        let legal_all = vec_env.legal_actions_all(PlayerId(0));
        assert_eq!(legal_all.len(), 3);
        assert!(legal_all.iter().all(|l| l.contains(&Action::KeepHand)));

        let obs_all = vec_env.observe_all(PlayerId(0));
        let flat_all = vec_env.observe_flat_all(PlayerId(0));
        assert_eq!(obs_all.len(), 3);
        assert_eq!(flat_all.len(), 3);
        for (obs, flat) in obs_all.iter().zip(flat_all.iter()) {
            for ent in &obs.entities {
                assert_ne!(ent.zone, Zone::Library);
                if ent.zone == Zone::Hand {
                    assert_eq!(ent.owner_rel, 0);
                }
            }
            assert_eq!(flat.player_features.len(), 2 * 8);
            assert!(flat.global_features.iter().all(|v| v.is_finite()));
        }

        // Action count mismatch must fail cleanly.
        assert!(
            vec_env
                .step_all(&[(PlayerId(0), Action::KeepHand)])
                .is_err()
        );

        let outcomes = vec_env
            .step_all(&[
                (PlayerId(0), Action::KeepHand),
                (PlayerId(0), Action::KeepHand),
                (PlayerId(0), Action::KeepHand),
            ])
            .unwrap();
        assert_eq!(outcomes.len(), 3);

        let mut rng = RngState::splitmix64(555);
        let dets = vec_env.determinize_all(PlayerId(0), &mut rng);
        assert_eq!(dets.len(), 3);
    }
}
