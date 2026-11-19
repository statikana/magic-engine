use mtg_ir::GamePhase;
use serde::{Deserialize, Serialize};

use crate::action::Action;
use crate::config::GameConfig;
use crate::id::PlayerId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub seq: u64,
    pub actor: PlayerId,
    pub action: Action,
}

/// Append-only deterministic replay stored by `magic-web` SQLite and used for bug repros / AI data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayLog {
    pub config: GameConfig,
    pub seed: u64,
    pub entries: Vec<ReplayEntry>,
}

/// Trace recorded by `peckhamr.com/scripts/record-ts-replay.ts` for Phase 0–2 differential testing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsReplayTrace {
    pub format_version: u32,
    pub compiler_version: u32,
    pub seed: u32,
    pub seats: Vec<TsReplaySeat>,
    pub steps: Vec<TsReplayStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsReplaySeat {
    pub seat: u8,
    pub deck_id: String,
    pub commanders: Vec<String>,
    pub library_order_initial: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsReplayStep {
    pub step_index: u32,
    pub actor_seat: u8,
    pub action: serde_json::Value,
    pub digest_after: TsStepDigest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsStepDigest {
    pub turn_number: u32,
    pub active_seat: u8,
    pub priority_seat: u8,
    pub phase: GamePhase,
    pub stack_depth: u16,
    pub players: Vec<TsPlayerDigest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TsPlayerDigest {
    pub seat: u8,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
}
