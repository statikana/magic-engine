//! `mtg-compiler`: Native Rust Oracle-text tokenizer, recursive-descent parser, and
//! hash-gated override registry targeting `mtg-ir` v2.
//!
//! Populated in Phase 5 (during Phases 0–4, `cards.bin` is bootstrapped from the TS compiler via
//! `peckhamr.com/scripts/export-ir-v2.ts`).

use mtg_ir::{CardDef, CardLayout, InternTables};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("native Rust Oracle compiler lands in Phase 5")]
    NotImplemented,
    #[error("invalid override entry: {0}")]
    InvalidOverride(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawCardFace {
    pub name: String,
    pub mana_cost: String,
    pub type_line: String,
    pub oracle_text: String,
    pub colors: Vec<String>,
    pub power: Option<String>,
    pub toughness: Option<String>,
    pub loyalty: Option<String>,
    pub defense: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawCardInput {
    pub oracle_id: String,
    pub name: String,
    pub layout: CardLayout,
    pub faces: Vec<RawCardFace>,
}

#[derive(Debug, Clone, Default)]
pub struct OverrideStore {
    by_oracle_and_hash: HashMap<(String, String), CardDef>,
}

impl OverrideStore {
    pub fn insert(&mut self, def: CardDef) {
        self.by_oracle_and_hash
            .insert((def.oracle_id.clone(), def.text_hash.clone()), def);
    }

    pub fn resolve(&self, oracle_id: &str, text_hash: &str) -> Option<&CardDef> {
        self.by_oracle_and_hash
            .get(&(oracle_id.to_string(), text_hash.to_string()))
    }

    pub fn len(&self) -> usize {
        self.by_oracle_and_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_oracle_and_hash.is_empty()
    }
}

pub trait CardCompiler {
    fn compile_card(
        &self,
        input: &RawCardInput,
        tables: &mut InternTables,
        overrides: &OverrideStore,
    ) -> Result<CardDef, CompilerError>;
}
