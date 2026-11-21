//! Standalone opaque-box E2E test harness for `mtg-ir`, `mtg-engine`, and `mtg-cli`.
//!
//! All cards, oracle IDs, and decks constructed by this crate are 100% synthetic (`Synth *` /
//! `synth_*`) with zero WotC/Scryfall intellectual property.

pub mod assertions;
pub mod cli;
pub mod driver;
pub mod synth;

pub const SYNTH_COMPILER_VERSION: u32 = 46;
pub const SYNTH_TEXT_HASH: &str = "synth_0000";
