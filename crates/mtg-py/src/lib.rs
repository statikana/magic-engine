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
