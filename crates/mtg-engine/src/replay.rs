use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use mtg_ir::{CardDatabase, CardTypeMask, ColorMask, CostDef, GamePhase, ManaColor, Zone};
use serde::{Deserialize, Serialize};

use crate::action::{
    Action, AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, DeckSpec, TargetRef,
};
use crate::casting::{
    apply_casting_action, apply_play_land, apply_special_action, begin_activate_ability,
    begin_cast_spell,
};
use crate::choices::{
    apply_choice_answer, current_actor, current_decision_state, is_prompt_active,
};
use crate::combat::apply_combat_action;
use crate::config::{CommanderRules, GameConfig, MulliganRule, RngMode};
use crate::events::{AtomicEventKind, begin_action, emit_event, end_action};
use crate::id::{ObjectId, PlayerId};
use crate::legality::{enumerate_cast_options, resolve_activated_abilities};
use crate::mana::{
    ManaAbilityCandidate, activate_mana_ability, color_to_index, colors_from_mask,
    enumerate_mana_abilities,
};
use crate::sba::check_sbas;
use crate::session::MatchSession;
use crate::state::{
    AltCastKind, CastKind, CastStep, CastingPromptKind, EngineError, GameState, LayerCache,
    ObjectFlags, PendingReplacementPrompt, PromptOwner, SbaPromptKind, StackEntryKind,
    StepPauseState, TOKEN_CARD_DEF_ID, TurnPromptKind, YesNoPausePurpose,
};
use crate::turn::{apply_concede, apply_mulligan_action, begin_opening_mulligans, pass_priority};
use crate::zones::draw_one_raw;

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
    #[serde(default)]
    pub decks: Vec<DeckSpec>,
    pub seed: u64,
    pub entries: Vec<ReplayEntry>,
}

/// Trace recorded by `peckhamr.com/scripts/record-ts-replay.ts` for Phase 0–2 differential testing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayTrace {
    pub format_version: u32,
    pub compiler_version: u32,
    pub seed: u32,
    pub post_init_rng_state: u32,
    pub rules: TsReplayRules,
    pub seats: Vec<TsReplaySeat>,
    pub steps: Vec<TsReplayStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayRules {
    pub starting_life: i32,
    pub commander: bool,
    pub free_first_mulligan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplaySeat {
    pub seat: u8,
    pub deck_id: String,
    pub commanders: Vec<String>,
    pub library_order_initial: Vec<String>,
    pub hand_initial: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayStep {
    pub step_index: u32,
    pub actor_seat: u8,
    pub action: serde_json::Value,
    pub object_locators: BTreeMap<String, ObjectLocator>,
    pub digest_after: TsStepDigest,
}

/// Canonical object locator mapping an opaque TS `instanceId` to a Rust game object.
///
/// Covers all 7 zones (including [`Zone::Stack`]) via `ZoneSlot` / `OwnerOrdinal` and
/// tracks card/token creation identities via `InitialCard` / `Token`, while rejecting
/// empty objects and unknown fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ObjectLocator {
    ZoneSlot(ZoneSlotLocator),
    OwnerOrdinal(OwnerOrdinalLocator),
    InitialCard(InitialCardLocator),
    Token(TokenLocator),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ZoneSlotLocator {
    pub seat: u8,
    pub zone: Zone,
    pub zone_index: usize,
    pub oracle_id: String,
    #[serde(default)]
    pub initial_card_ordinal: Option<u32>,
    #[serde(default)]
    pub token_seq: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerOrdinalLocator {
    pub owner_seat: u8,
    pub zone: Zone,
    pub ordinal: usize,
    pub oracle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitialCardLocator {
    pub initial_card_ordinal: u32,
    #[serde(default)]
    pub token_seq: Option<u32>,
    #[serde(default)]
    pub seat: Option<u8>,
    #[serde(default)]
    pub oracle_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenLocator {
    pub token_seq: u32,
    #[serde(default)]
    pub initial_card_ordinal: Option<u32>,
    #[serde(default)]
    pub seat: Option<u8>,
    #[serde(default)]
    pub oracle_id: Option<String>,
}

/// Phase reported in [`TsStepDigest`], extending [`mtg_ir::GamePhase`] with `mulligan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsReplayPhase {
    Mulligan,
    Untap,
    Upkeep,
    Draw,
    Main1,
    BeginCombat,
    DeclareAttackers,
    DeclareBlockers,
    FirstStrikeDamage,
    CombatDamage,
    EndCombat,
    Main2,
    EndStep,
    Cleanup,
}

impl TsReplayPhase {
    pub fn as_game_phase(self) -> Option<GamePhase> {
        match self {
            Self::Mulligan => None,
            Self::Untap => Some(GamePhase::Untap),
            Self::Upkeep => Some(GamePhase::Upkeep),
            Self::Draw => Some(GamePhase::Draw),
            Self::Main1 => Some(GamePhase::Main1),
            Self::BeginCombat => Some(GamePhase::BeginCombat),
            Self::DeclareAttackers => Some(GamePhase::DeclareAttackers),
            Self::DeclareBlockers => Some(GamePhase::DeclareBlockers),
            Self::FirstStrikeDamage => Some(GamePhase::FirstStrikeDamage),
            Self::CombatDamage => Some(GamePhase::CombatDamage),
            Self::EndCombat => Some(GamePhase::EndCombat),
            Self::Main2 => Some(GamePhase::Main2),
            Self::EndStep => Some(GamePhase::EndStep),
            Self::Cleanup => Some(GamePhase::Cleanup),
        }
    }
}

impl From<GamePhase> for TsReplayPhase {
    fn from(phase: GamePhase) -> Self {
        match phase {
            GamePhase::Untap => Self::Untap,
            GamePhase::Upkeep => Self::Upkeep,
            GamePhase::Draw => Self::Draw,
            GamePhase::Main1 => Self::Main1,
            GamePhase::BeginCombat => Self::BeginCombat,
            GamePhase::DeclareAttackers => Self::DeclareAttackers,
            GamePhase::DeclareBlockers => Self::DeclareBlockers,
            GamePhase::FirstStrikeDamage => Self::FirstStrikeDamage,
            GamePhase::CombatDamage => Self::CombatDamage,
            GamePhase::EndCombat => Self::EndCombat,
            GamePhase::Main2 => Self::Main2,
            GamePhase::EndStep => Self::EndStep,
            GamePhase::Cleanup => Self::Cleanup,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsStepDigest {
    pub turn_number: u32,
    pub active_seat: u8,
    pub priority_seat: u8,
    pub phase: TsReplayPhase,
    pub stack_depth: u16,
    pub pending_prompt_kind: Option<String>,
    pub players: Vec<TsPlayerDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsPlayerDigest {
    pub seat: u8,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub mana_pool: [u32; 6],
    pub zone_counts: TsZoneCounts,
    pub hand_oracle_ids: Vec<String>,
    pub battlefield: Vec<TsBattlefieldEntryDigest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsZoneCounts {
    pub library: u32,
    pub hand: u32,
    pub battlefield: u32,
    pub graveyard: u32,
    pub exile: u32,
    pub command: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsBattlefieldEntryDigest {
    pub oracle_id: String,
    pub tapped: bool,
    pub pt: Option<[i32; 2]>,
    pub types: Vec<String>,
    pub keywords: Vec<String>,
    pub counters: Vec<(String, u32)>,
    pub damage_marked: i32,
    pub attached_to_oracle_id: Option<String>,
}

/// Summary returned by [`run_ts_diff_test`] when every step in a TS replay trace matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffTestReport {
    pub seed: u32,
    pub seat_count: usize,
    pub steps_replayed: usize,
    pub final_turn: u32,
    pub final_phase: TsReplayPhase,
}

/// Category of failure reported by [`run_ts_diff_test`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffFailureKind {
    InvalidTrace,
    UnknownOracleId,
    InitialStateMismatch,
    LocatorResolution,
    ActionLowering,
    EngineError,
    DigestMismatch,
}

/// Exact field-level mismatch between an expected TS [`TsStepDigest`] and actual Rust state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFieldMismatch {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

/// Diagnostic error returned by [`run_ts_diff_test`] and [`init_session_from_ts_trace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffTestFailure {
    pub step_index: Option<u32>,
    pub actor_seat: Option<u8>,
    pub action_type: Option<String>,
    pub kind: DiffFailureKind,
    pub message: String,
    pub diffs: Vec<DiffFieldMismatch>,
}

impl fmt::Display for DiffTestFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (
            self.step_index,
            self.actor_seat,
            self.action_type.as_deref(),
        ) {
            (Some(step), Some(seat), Some(act)) => write!(
                f,
                "[{:?}] step {step} (seat {seat}, action '{act}'): {}",
                self.kind, self.message
            )?,
            (Some(step), Some(seat), None) => write!(
                f,
                "[{:?}] step {step} (seat {seat}): {}",
                self.kind, self.message
            )?,
            _ => write!(f, "[{:?}] {}", self.kind, self.message)?,
        }
        for diff in &self.diffs {
            write!(
                f,
                "\n  - {}: expected {}, got {}",
                diff.path, diff.expected, diff.actual
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for DiffTestFailure {}

fn init_failure(
    kind: DiffFailureKind,
    message: impl Into<String>,
    diffs: Vec<DiffFieldMismatch>,
) -> DiffTestFailure {
    DiffTestFailure {
        step_index: None,
        actor_seat: None,
        action_type: None,
        kind,
        message: message.into(),
        diffs,
    }
}

fn step_failure(
    step: &TsReplayStep,
    kind: DiffFailureKind,
    message: impl Into<String>,
    diffs: Vec<DiffFieldMismatch>,
) -> DiffTestFailure {
    let action_type = step
        .action
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    DiffTestFailure {
        step_index: Some(step.step_index),
        actor_seat: Some(step.actor_seat),
        action_type,
        kind,
        message: message.into(),
        diffs,
    }
}

fn build_ts_config_and_decks(
    db: &CardDatabase,
    trace: &TsReplayTrace,
) -> Result<(GameConfig, Vec<DeckSpec>), DiffTestFailure> {
    if trace.format_version != 1 {
        return Err(init_failure(
            DiffFailureKind::InvalidTrace,
            format!(
                "unsupported formatVersion {} (expected 1)",
                trace.format_version
            ),
            Vec::new(),
        ));
    }
    if trace.seats.is_empty() || trace.seats.len() > 8 {
        return Err(init_failure(
            DiffFailureKind::InvalidTrace,
            format!("invalid seat count {} (expected 1..=8)", trace.seats.len()),
            Vec::new(),
        ));
    }

    let mut decks = Vec::with_capacity(trace.seats.len());
    for (idx, seat) in trace.seats.iter().enumerate() {
        if usize::from(seat.seat) != idx {
            return Err(init_failure(
                DiffFailureKind::InvalidTrace,
                format!("seat index mismatch at position {idx}: got {}", seat.seat),
                Vec::new(),
            ));
        }

        let mut commanders = Vec::with_capacity(seat.commanders.len());
        for oid in &seat.commanders {
            let cid = db.id_by_oracle(oid).ok_or_else(|| {
                init_failure(
                    DiffFailureKind::UnknownOracleId,
                    format!("seat {}: unknown commander oracleId '{oid}'", seat.seat),
                    Vec::new(),
                )
            })?;
            commanders.push(cid);
        }

        for oid in &seat.hand_initial {
            if db.id_by_oracle(oid).is_none() {
                return Err(init_failure(
                    DiffFailureKind::UnknownOracleId,
                    format!("seat {}: unknown handInitial oracleId '{oid}'", seat.seat),
                    Vec::new(),
                ));
            }
        }

        let mut sorted_oracles: Vec<&str> = seat
            .library_order_initial
            .iter()
            .map(String::as_str)
            .collect();
        sorted_oracles.sort_unstable();

        let mut mainboard = Vec::with_capacity(sorted_oracles.len());
        for oid in sorted_oracles {
            let cid = db.id_by_oracle(oid).ok_or_else(|| {
                init_failure(
                    DiffFailureKind::UnknownOracleId,
                    format!(
                        "seat {}: unknown libraryOrderInitial oracleId '{oid}'",
                        seat.seat
                    ),
                    Vec::new(),
                )
            })?;
            mainboard.push(cid);
        }

        decks.push(DeckSpec {
            commanders,
            mainboard,
            sideboard: Vec::new(),
        });
    }

    let config = GameConfig {
        player_count: trace.seats.len() as u8,
        starting_life: trace.rules.starting_life,
        starting_hand_size: 7,
        max_hand_size: 7,
        mulligan: MulliganRule::London {
            free_first: trace.rules.free_first_mulligan,
        },
        commander: trace.rules.commander.then(CommanderRules::default),
        allow_guided_fallback: true,
        rng_mode: RngMode::Mulberry32Compat,
    };

    Ok((config, decks))
}

fn object_oracle_id(state: &GameState, id: ObjectId) -> Option<String> {
    let obj = state.object(id)?;
    if obj.card_def == TOKEN_CARD_DEF_ID {
        let name = state
            .rare(id)
            .and_then(|r| r.token_spec.as_deref())
            .map_or("token", |t| t.name.as_str());
        return Some(format!("token:{name}"));
    }
    state
        .db()
        .get(obj.card_def)
        .map(|card| card.oracle_id.clone())
}

fn verify_initial_state(state: &GameState, trace: &TsReplayTrace) -> Result<(), DiffTestFailure> {
    for (idx, seat) in trace.seats.iter().enumerate() {
        let p = &state.players()[idx];

        let mut actual_order: Vec<String> = Vec::with_capacity(p.hand.len() + p.library.len());
        for &oid in p.hand.iter().chain(p.library.iter().rev()) {
            let oracle = object_oracle_id(state, oid).unwrap_or_else(|| "<unknown>".to_string());
            actual_order.push(oracle);
        }

        if actual_order != seat.library_order_initial {
            let first_diff = actual_order
                .iter()
                .zip(seat.library_order_initial.iter())
                .position(|(a, e)| a != e)
                .unwrap_or(actual_order.len().min(seat.library_order_initial.len()));
            let exp_at = seat
                .library_order_initial
                .get(first_diff)
                .map_or("<none>", String::as_str);
            let act_at = actual_order
                .get(first_diff)
                .map_or("<none>", String::as_str);
            return Err(init_failure(
                DiffFailureKind::InitialStateMismatch,
                format!(
                    "seat {} libraryOrderInitial mismatch after Mulberry32 shuffle (seed={})",
                    seat.seat, trace.seed
                ),
                vec![DiffFieldMismatch {
                    path: format!("seats[{}].libraryOrderInitial[{first_diff}]", seat.seat),
                    expected: format!("{exp_at} (len={})", seat.library_order_initial.len()),
                    actual: format!("{act_at} (len={})", actual_order.len()),
                }],
            ));
        }

        if !seat.hand_initial.is_empty() {
            let actual_hand: Vec<String> = p
                .hand
                .iter()
                .map(|&oid| object_oracle_id(state, oid).unwrap_or_else(|| "<unknown>".to_string()))
                .collect();
            if actual_hand != seat.hand_initial {
                return Err(init_failure(
                    DiffFailureKind::InitialStateMismatch,
                    format!("seat {} handInitial mismatch", seat.seat),
                    vec![DiffFieldMismatch {
                        path: format!("seats[{}].handInitial", seat.seat),
                        expected: format!("{:?}", seat.hand_initial),
                        actual: format!("{actual_hand:?}"),
                    }],
                ));
            }
        }
    }
    Ok(())
}

/// Initializes a [`MatchSession`] from a [`TsReplayTrace`] using `RngMode::Mulberry32Compat`
/// and verifies initial shuffled library order and opening hands.
pub fn init_session_from_ts_trace(
    db: Arc<CardDatabase>,
    trace: &TsReplayTrace,
) -> Result<MatchSession, DiffTestFailure> {
    let (config, decks) = build_ts_config_and_decks(&db, trace)?;
    let session = MatchSession::new(db, config, decks, u64::from(trace.seed)).map_err(|e| {
        init_failure(
            DiffFailureKind::EngineError,
            format!("MatchSession::new failed: {e}"),
            Vec::new(),
        )
    })?;
    verify_initial_state(session.state(), trace)?;
    Ok(session)
}

#[derive(Debug, Default)]
struct TsReplayTracker {
    initial_ordinal_to_index: Vec<u32>,
    token_seq_to_index: BTreeMap<u32, u32>,
    seen_object_ids: BTreeSet<ObjectId>,
    ts_id_to_index: BTreeMap<String, u32>,
    next_token_seq: u32,
}

impl TsReplayTracker {
    fn from_initial_state(state: &GameState) -> Self {
        let mut tracker = Self::default();
        for p in state.players() {
            for &id in p
                .command
                .iter()
                .chain(p.hand.iter())
                .chain(p.library.iter().rev())
            {
                tracker.initial_ordinal_to_index.push(id.index);
                tracker.seen_object_ids.insert(id);
            }
        }
        tracker
    }

    fn refresh_after_step(&mut self, state: &GameState) {
        for p in state.players() {
            for &id in p
                .library
                .iter()
                .rev()
                .chain(p.hand.iter())
                .chain(p.battlefield.iter())
                .chain(p.graveyard.iter())
                .chain(p.exile.iter())
                .chain(p.command.iter())
            {
                if self.seen_object_ids.insert(id) {
                    let is_initial = self.initial_ordinal_to_index.contains(&id.index);
                    if !is_initial || state.object(id).is_some_and(|o| o.is_token()) {
                        let seq = self.next_token_seq;
                        self.next_token_seq = self.next_token_seq.saturating_add(1);
                        self.token_seq_to_index.insert(seq, id.index);
                    }
                }
            }
        }
        for entry in state.stack_entries() {
            if let StackEntryKind::Spell { object } = entry.kind
                && self.seen_object_ids.insert(object)
            {
                let is_initial = self.initial_ordinal_to_index.contains(&object.index);
                if !is_initial || state.object(object).is_some_and(|o| o.is_token()) {
                    let seq = self.next_token_seq;
                    self.next_token_seq = self.next_token_seq.saturating_add(1);
                    self.token_seq_to_index.insert(seq, object.index);
                }
            }
        }
    }

    fn live_id_for_index(state: &GameState, index: u32) -> Option<ObjectId> {
        state
            .objects()
            .iter()
            .find(|(id, _)| id.index == index)
            .map(|(id, _)| id)
    }

    fn oracle_matches(state: &GameState, id: ObjectId, expected_oracle: &str) -> bool {
        if let Some(obj) = state.object(id)
            && obj.is_token()
            && (expected_oracle.starts_with("tok_") || expected_oracle.starts_with("token:"))
        {
            return true;
        }
        object_oracle_id(state, id).is_some_and(|actual| actual == expected_oracle)
    }

    fn resolve_zone_slice(p: &crate::state::PlayerState, zone: Zone) -> Vec<ObjectId> {
        match zone {
            Zone::Library => p.library.iter().copied().rev().collect(),
            Zone::Hand => p.hand.clone(),
            Zone::Battlefield => p.battlefield.clone(),
            Zone::Graveyard => p.graveyard.clone(),
            Zone::Exile => p.exile.clone(),
            Zone::Command => p.command.clone(),
            Zone::Stack => Vec::new(),
        }
    }

    fn resolve_instance_id(
        &mut self,
        state: &GameState,
        step: &TsReplayStep,
        ts_id: &str,
    ) -> Result<ObjectId, DiffTestFailure> {
        if let Some(locator) = step.object_locators.get(ts_id) {
            let resolved = match locator {
                ObjectLocator::ZoneSlot(loc) => self.resolve_zone_slot(state, loc),
                ObjectLocator::OwnerOrdinal(loc) => self.resolve_owner_ordinal(state, loc),
                ObjectLocator::InitialCard(loc) => self.resolve_initial_card(state, loc),
                ObjectLocator::Token(loc) => self.resolve_token(state, loc),
            };
            if let Some(oid) = resolved {
                self.ts_id_to_index.insert(ts_id.to_string(), oid.index);
                return Ok(oid);
            }
            return Err(step_failure(
                step,
                DiffFailureKind::LocatorResolution,
                format!("could not resolve TS instanceId '{ts_id}' via locator {locator:?}"),
                Vec::new(),
            ));
        }

        if let Some(&idx) = self.ts_id_to_index.get(ts_id)
            && let Some(oid) = Self::live_id_for_index(state, idx)
        {
            return Ok(oid);
        }

        Err(step_failure(
            step,
            DiffFailureKind::LocatorResolution,
            format!("missing ObjectLocator for TS instanceId '{ts_id}'"),
            Vec::new(),
        ))
    }

    fn resolve_zone_slot(&self, state: &GameState, loc: &ZoneSlotLocator) -> Option<ObjectId> {
        if let Some(ord) = loc.initial_card_ordinal
            && let Some(&idx) = self.initial_ordinal_to_index.get(ord as usize)
            && let Some(oid) = Self::live_id_for_index(state, idx)
            && let Some(obj) = state.object(oid)
            && obj.zone == loc.zone
            && Self::oracle_matches(state, oid, &loc.oracle_id)
        {
            return Some(oid);
        }

        if let Some(seq) = loc.token_seq
            && let Some(&idx) = self.token_seq_to_index.get(&seq)
            && let Some(oid) = Self::live_id_for_index(state, idx)
            && let Some(obj) = state.object(oid)
            && obj.zone == loc.zone
            && Self::oracle_matches(state, oid, &loc.oracle_id)
        {
            return Some(oid);
        }

        if loc.zone == Zone::Stack {
            if let Some(entry) = state.stack_entries().get(loc.zone_index) {
                let candidate = match entry.kind {
                    StackEntryKind::Spell { object } => Some(object),
                    StackEntryKind::Activated { source, .. }
                    | StackEntryKind::Triggered { source, .. } => Some(source),
                };
                if let Some(oid) = candidate
                    && Self::oracle_matches(state, oid, &loc.oracle_id)
                {
                    return Some(oid);
                }
            }
            for entry in state.stack_entries() {
                if let StackEntryKind::Spell { object } = entry.kind
                    && Self::oracle_matches(state, object, &loc.oracle_id)
                {
                    return Some(object);
                }
            }
            return None;
        }

        let p = state.players().get(usize::from(loc.seat))?;
        let slice = Self::resolve_zone_slice(p, loc.zone);
        if let Some(&oid) = slice.get(loc.zone_index)
            && Self::oracle_matches(state, oid, &loc.oracle_id)
        {
            return Some(oid);
        }
        slice
            .into_iter()
            .find(|&oid| Self::oracle_matches(state, oid, &loc.oracle_id))
    }

    fn resolve_owner_ordinal(
        &self,
        state: &GameState,
        loc: &OwnerOrdinalLocator,
    ) -> Option<ObjectId> {
        if loc.zone == Zone::Stack {
            let mut count = 0usize;
            for entry in state.stack_entries() {
                let oid = match entry.kind {
                    StackEntryKind::Spell { object } => object,
                    StackEntryKind::Activated { source, .. }
                    | StackEntryKind::Triggered { source, .. } => source,
                };
                if state
                    .object(oid)
                    .is_some_and(|o| o.owner == PlayerId(loc.owner_seat))
                    && Self::oracle_matches(state, oid, &loc.oracle_id)
                {
                    if count == loc.ordinal {
                        return Some(oid);
                    }
                    count += 1;
                }
            }
            return None;
        }

        let p = state.players().get(usize::from(loc.owner_seat))?;
        let slice = Self::resolve_zone_slice(p, loc.zone);
        slice
            .into_iter()
            .filter(|&oid| {
                state
                    .object(oid)
                    .is_some_and(|o| o.owner == PlayerId(loc.owner_seat))
                    && Self::oracle_matches(state, oid, &loc.oracle_id)
            })
            .nth(loc.ordinal)
    }

    fn resolve_initial_card(
        &self,
        state: &GameState,
        loc: &InitialCardLocator,
    ) -> Option<ObjectId> {
        let &idx = self
            .initial_ordinal_to_index
            .get(loc.initial_card_ordinal as usize)?;
        let oid = Self::live_id_for_index(state, idx)?;
        if let Some(expected_oid) = loc.oracle_id.as_deref()
            && !Self::oracle_matches(state, oid, expected_oid)
        {
            return None;
        }
        Some(oid)
    }

    fn resolve_token(&self, state: &GameState, loc: &TokenLocator) -> Option<ObjectId> {
        if let Some(&idx) = self.token_seq_to_index.get(&loc.token_seq)
            && let Some(oid) = Self::live_id_for_index(state, idx)
        {
            return Some(oid);
        }
        let seat = loc.seat?;
        let p = state.players().get(usize::from(seat))?;
        p.battlefield
            .iter()
            .copied()
            .find(|&oid| state.object(oid).is_some_and(|o| o.is_token()))
    }
}

fn parse_seat_id(step: &TsReplayStep, raw: &str) -> Result<PlayerId, DiffTestFailure> {
    if let Some(rest) = raw.strip_prefix("seat_")
        && let Ok(seat) = rest.parse::<u8>()
    {
        return Ok(PlayerId(seat));
    }
    if let Ok(seat) = raw.parse::<u8>() {
        return Ok(PlayerId(seat));
    }
    Err(step_failure(
        step,
        DiffFailureKind::ActionLowering,
        format!("cannot parse TS player ID '{raw}' as seat_<N>"),
        Vec::new(),
    ))
}

fn parse_zone_str(raw: &str) -> Option<Zone> {
    match raw {
        "library" => Some(Zone::Library),
        "hand" => Some(Zone::Hand),
        "battlefield" => Some(Zone::Battlefield),
        "graveyard" => Some(Zone::Graveyard),
        "exile" => Some(Zone::Exile),
        "command" => Some(Zone::Command),
        "stack" => Some(Zone::Stack),
        _ => None,
    }
}

fn parse_mana_color(raw: &str) -> Option<ManaColor> {
    match raw {
        "W" | "w" => Some(ManaColor::W),
        "U" | "u" => Some(ManaColor::U),
        "B" | "b" => Some(ManaColor::B),
        "R" | "r" => Some(ManaColor::R),
        "G" | "g" => Some(ManaColor::G),
        "C" | "c" => Some(ManaColor::C),
        _ => None,
    }
}

fn match_cast_option_id(
    state: &GameState,
    actor: PlayerId,
    object: ObjectId,
    option_str: Option<&str>,
    face_index: Option<u8>,
    fused: bool,
) -> u16 {
    let options = enumerate_cast_options(state, actor, object);
    if options.is_empty() {
        return 0;
    }

    if fused
        && let Some(opt) = options
            .iter()
            .find(|o| o.fused || matches!(o.alt_kind, AltCastKind::Fuse))
    {
        return opt.option_id;
    }

    if let Some(raw) = option_str {
        let lower = raw.to_ascii_lowercase();
        let key = lower.strip_prefix("alt:").unwrap_or(&lower);
        let target_alt = match key {
            "normal" | "cast" | "exile:permission" => Some(AltCastKind::Normal),
            "commander" | "command" => Some(AltCastKind::CommandZoneCast),
            "adventure" => Some(AltCastKind::AdventureHalf),
            "omen" => Some(AltCastKind::OmenHalf),
            "split_second" | "split_right" | "split:1" => Some(AltCastKind::SplitSecondHalf),
            "mdfc_back" | "back" | "face:1" => Some(AltCastKind::ModalBackFace),
            "prototype" => Some(AltCastKind::Prototype),
            "morph" => Some(AltCastKind::MorphFaceDown),
            "disguise" => Some(AltCastKind::DisguiseFaceDown),
            "bestow" => Some(AltCastKind::Bestow),
            "flashback" => Some(AltCastKind::Flashback),
            "escape" => Some(AltCastKind::Escape),
            "disturb" => Some(AltCastKind::Disturb),
            "encore" => Some(AltCastKind::Encore),
            "harmonize" => Some(AltCastKind::Harmonize),
            "aftermath" => Some(AltCastKind::Aftermath),
            "jump_start" | "jump-start" | "jumpstart" => Some(AltCastKind::JumpStart),
            "madness" => Some(AltCastKind::Madness),
            "miracle" => Some(AltCastKind::Miracle),
            "foretell" => Some(AltCastKind::ForetellCast),
            "plot" => Some(AltCastKind::PlotCast),
            "warp" => Some(AltCastKind::Warp),
            "blitz" => Some(AltCastKind::Blitz),
            "dash" => Some(AltCastKind::Dash),
            "evoke" => Some(AltCastKind::Evoke),
            "overload" => Some(AltCastKind::Overload),
            "cleave" => Some(AltCastKind::Cleave),
            "fuse" => Some(AltCastKind::Fuse),
            "spree" => Some(AltCastKind::Spree),
            "emerge" => Some(AltCastKind::Emerge),
            "mutate" => Some(AltCastKind::Mutate),
            _ => None,
        };
        if let Some(kind) = target_alt
            && let Some(opt) = options.iter().find(|o| o.alt_kind == kind)
        {
            return opt.option_id;
        }
        if (lower.contains("kicker") || lower.contains("kicked"))
            && let Some(opt) = options.iter().find(|o| o.is_kicked)
        {
            return opt.option_id;
        }
        if let Ok(num) = raw.parse::<u16>()
            && let Some(opt) = options.iter().find(|o| o.option_id == num)
        {
            return opt.option_id;
        }
    }

    if let Some(fi) = face_index
        && let Some(opt) = options.iter().find(|o| o.face_idx == fi)
    {
        return opt.option_id;
    }

    options[0].option_id
}

fn match_activated_ability_index(
    state: &GameState,
    object: ObjectId,
    ability_id: &str,
) -> Option<u16> {
    let abilities = resolve_activated_abilities(state, object);
    if abilities.is_empty() {
        return None;
    }
    if let Some(ab) = abilities.iter().find(|a| a.ability.id == ability_id) {
        return Some(ab.ability_index);
    }
    if let Ok(idx) = ability_id.parse::<u16>()
        && abilities.iter().any(|a| a.ability_index == idx)
    {
        return Some(idx);
    }
    if let Some((_, tail)) = ability_id.rsplit_once(':') {
        if let Some(ab) = abilities.iter().find(|a| a.ability.id == tail) {
            return Some(ab.ability_index);
        }
        if let Ok(idx) = tail.parse::<usize>()
            && let Some(ab) = abilities.get(idx)
        {
            return Some(ab.ability_index);
        }
    }
    if let Some(tail) = ability_id.rsplit('_').next()
        && let Ok(idx) = tail.parse::<usize>()
        && let Some(ab) = abilities.get(idx)
    {
        return Some(ab.ability_index);
    }
    if ability_id.to_ascii_lowercase().contains("loyalty")
        && let Some(ab) = abilities.iter().find(|a| {
            a.ability
                .cost
                .iter()
                .any(|c| matches!(c, CostDef::Loyalty { .. }))
        })
    {
        return Some(ab.ability_index);
    }
    Some(abilities[0].ability_index)
}

fn apply_raw_engine_action(
    state: &mut GameState,
    player: PlayerId,
    action: &Action,
) -> Result<(), EngineError> {
    if current_actor(state) != Some(player) {
        return Err(EngineError::IllegalAction(format!(
            "player {player:?} does not currently have the decision"
        )));
    }
    begin_action(state);
    let res = apply_raw_engine_action_inner(state, player, action);
    end_action(state);
    if res.is_ok() {
        state.prune_stale_lki();
        let _ = state.bump_event_seq();
    }
    res
}

fn apply_raw_engine_action_inner(
    state: &mut GameState,
    player: PlayerId,
    action: &Action,
) -> Result<(), EngineError> {
    if matches!(action, Action::Concede) {
        let _ = apply_concede(state, player)?;
        if !is_prompt_active(state)
            && state.casting().is_none()
            && matches!(state.decision(), DecisionState::Priority)
        {
            let _ = check_sbas(state)?;
        }
        return Ok(());
    }

    if is_prompt_active(state) {
        return match action {
            Action::Answer { prompt_id, answer } => {
                apply_choice_answer(state, player, *prompt_id, answer.clone())
            }
            _ => Err(EngineError::IllegalAction(
                "expected Action::Answer for active prompt".into(),
            )),
        };
    }

    match current_decision_state(state) {
        DecisionState::Mulligan => match action {
            Action::KeepHand | Action::TakeMulligan | Action::BottomCard { .. } => {
                apply_mulligan_action(state, player, action).map(|_| ())
            }
            _ => Err(EngineError::IllegalAction(
                "expected mulligan action in Mulligan decision state".into(),
            )),
        },
        DecisionState::Priority => match action {
            Action::PassPriority => pass_priority(state, player).map(|_| ()),
            Action::PlayLand {
                object,
                from_zone,
                face_index,
            } => apply_play_land(state, player, *object, *from_zone, *face_index),
            Action::TapMana {
                object,
                ability_index,
                preferred_color,
            } => activate_mana_ability(state, player, *object, *ability_index, *preferred_color)
                .map(|_| ()),
            Action::BeginCast {
                object,
                option_id,
                from_zone,
            } => begin_cast_spell(state, player, *object, *option_id, *from_zone),
            Action::BeginActivate {
                object,
                ability_index,
            } => begin_activate_ability(state, player, *object, *ability_index),
            Action::SpecialAction { kind } => apply_special_action(state, player, *kind),
            _ => Err(EngineError::IllegalAction(
                "illegal action in Priority decision state".into(),
            )),
        },
        DecisionState::CastingSequence => apply_casting_action(state, player, action),
        DecisionState::DeclareAttackers
        | DecisionState::DeclareBlockers
        | DecisionState::AssignCombatDamage => {
            apply_combat_action(state, player, action).map(|_| ())
        }
        DecisionState::Prompt { .. } => match action {
            Action::Answer { prompt_id, answer } => {
                apply_choice_answer(state, player, *prompt_id, answer.clone())
            }
            _ => Err(EngineError::IllegalAction(
                "expected Action::Answer for active prompt".into(),
            )),
        },
        DecisionState::GameOver { .. } => {
            Err(EngineError::IllegalAction("game is already over".into()))
        }
    }
}

fn lower_target_refs(
    state: &GameState,
    tracker: &mut TsReplayTracker,
    step: &TsReplayStep,
    raw_targets: Option<&serde_json::Value>,
) -> Result<Vec<TargetRef>, DiffTestFailure> {
    let Some(arr) = raw_targets.and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut targets = Vec::with_capacity(arr.len());
    for t in arr {
        let kind = t.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let id_str = t.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
            step_failure(
                step,
                DiffFailureKind::ActionLowering,
                format!("target entry missing string 'id': {t}"),
                Vec::new(),
            )
        })?;
        match kind {
            "player" => {
                let pid = parse_seat_id(step, id_str)?;
                targets.push(TargetRef::Player { id: pid });
            }
            "stack" => {
                let obj_id = tracker.resolve_instance_id(state, step, id_str)?;
                let stack_id = state
                    .stack_entries()
                    .iter()
                    .find(|e| match e.kind {
                        StackEntryKind::Spell { object } => object == obj_id,
                        StackEntryKind::Activated { source, .. }
                        | StackEntryKind::Triggered { source, .. } => source == obj_id,
                    })
                    .map(|e| e.id)
                    .ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::LocatorResolution,
                            format!("stack target '{id_str}' ({obj_id:?}) not found on stack"),
                            Vec::new(),
                        )
                    })?;
                targets.push(TargetRef::Stack { id: stack_id });
            }
            _ => {
                let oid = tracker.resolve_instance_id(state, step, id_str)?;
                targets.push(TargetRef::Object { id: oid });
            }
        }
    }
    Ok(targets)
}

fn pip_accepts_color(pip: &mtg_ir::ManaPip, col: ManaColor) -> bool {
    match pip {
        mtg_ir::ManaPip::Colored { color }
        | mtg_ir::ManaPip::TwoBrid { color }
        | mtg_ir::ManaPip::Phyrexian { color }
        | mtg_ir::ManaPip::ColorlessHybrid { color } => *color == col,
        mtg_ir::ManaPip::Hybrid { a, b } | mtg_ir::ManaPip::HybridPhyrexian { a, b } => {
            *a == col || *b == col
        }
        mtg_ir::ManaPip::Snow => false,
    }
}

fn remaining_pip_needs_by_color(
    remaining_mana: &mtg_ir::ManaCost,
    available_pool: &[u32; 6],
) -> [u32; 6] {
    let mut needs = [0u32; 6];
    for pip in &remaining_mana.pips {
        if let mtg_ir::ManaPip::Colored { color } = pip {
            let idx = color_to_index(*color);
            needs[idx] = needs[idx].saturating_add(1);
        }
    }
    for (n, &avail) in needs.iter_mut().zip(available_pool.iter()) {
        *n = n.saturating_sub(avail);
    }
    needs
}

#[allow(clippy::too_many_arguments)]
fn pick_ts_mana_tap_action(
    state: &GameState,
    actor: PlayerId,
    oid: ObjectId,
    remaining_mana: &mtg_ir::ManaCost,
    simulated_pool: &[u32; 6],
    exp_mana_pool: &[u32; 6],
    exp_life: i32,
    candidates: &[ManaAbilityCandidate],
) -> Option<(u16, Option<ManaColor>, [u32; 6])> {
    let cur_player = state.player(actor)?;
    let cur_pool = cur_player.mana_pool;
    let cur_life = cur_player.life;
    let matching: Vec<&ManaAbilityCandidate> = candidates
        .iter()
        .filter(|c| {
            c.object == oid
                && c.requires_tap
                && !c.only_as_instant
                && !c.has_sacrifice_cost
                && c.extra_mana_cost.is_none()
                && c.has_energy_cost == 0
                && !c.has_non_mana_downside
                && (c.has_life_cost == 0 || exp_life < cur_life)
                && c.bundle_options.len() <= 1
        })
        .collect();
    if matching.is_empty() {
        return None;
    }

    if matching.len() == 1 {
        let c = matching[0];
        if c.max_any_color_amount == 0 {
            return Some((c.ability_index, None, c.produces_amounts));
        }
        let colors = colors_from_mask(c.produces_colors);
        if colors.is_empty() {
            return None;
        }
        let rem_needs = remaining_pip_needs_by_color(remaining_mana, simulated_pool);
        let preferred = colors
            .iter()
            .copied()
            .find(|&col| {
                let idx = color_to_index(col);
                rem_needs[idx] > 0
            })
            .or_else(|| {
                colors.iter().copied().find(|&col| {
                    let idx = color_to_index(col);
                    exp_mana_pool[idx] > cur_pool[idx]
                })
            })
            .or_else(|| {
                colors.iter().copied().find(|&col| {
                    remaining_mana
                        .pips
                        .iter()
                        .any(|p| pip_accepts_color(p, col))
                })
            })
            .or_else(|| colors.first().copied())?;
        let mut added = [0u32; 6];
        added[color_to_index(preferred)] = c.max_any_color_amount;
        return Some((c.ability_index, Some(preferred), added));
    }

    let rem_needs = remaining_pip_needs_by_color(remaining_mana, simulated_pool);
    let chosen = matching
        .iter()
        .copied()
        .find(|c| (0..6).any(|i| c.produces_amounts[i] > 0 && rem_needs[i] > 0))
        .or_else(|| {
            matching.iter().copied().find(|c| {
                (0..6).any(|i| c.produces_amounts[i] > 0 && exp_mana_pool[i] > cur_pool[i])
            })
        })
        .or_else(|| {
            matching.iter().copied().find(|c| {
                remaining_mana.pips.iter().any(|pip| {
                    colors_from_mask(c.produces_colors)
                        .into_iter()
                        .any(|col| pip_accepts_color(pip, col))
                })
            })
        })
        .unwrap_or(matching[0]);
    if chosen.max_any_color_amount > 0 {
        let colors = colors_from_mask(chosen.produces_colors);
        let preferred = colors
            .iter()
            .copied()
            .find(|&col| rem_needs[color_to_index(col)] > 0)
            .or_else(|| colors.first().copied())?;
        let mut added = [0u32; 6];
        added[color_to_index(preferred)] = chosen.max_any_color_amount;
        Some((chosen.ability_index, Some(preferred), added))
    } else {
        Some((chosen.ability_index, None, chosen.produces_amounts))
    }
}

fn replay_ts_tapped_mana_delta(
    state: &mut GameState,
    step: &TsReplayStep,
    prev_digest: Option<&TsStepDigest>,
    actor: PlayerId,
) -> Result<(), DiffTestFailure> {
    let Some(exp_p) = step.digest_after.players.iter().find(|p| p.seat == actor.0) else {
        return Ok(());
    };
    let Some(cs) = state.casting() else {
        return Ok(());
    };
    let Some(plan) = cs.payment_plan.as_deref() else {
        return Ok(());
    };
    let remaining_mana = plan.remaining_mana.clone();
    let has_non_mana_tap_other = plan
        .remaining_non_mana_costs
        .get(plan.non_mana_cursor..)
        .is_some_and(|rem| {
            rem.iter()
                .any(|c| matches!(c, CostDef::TapOthers { .. } | CostDef::TapAttached))
        });
    if has_non_mana_tap_other
        || (remaining_mana == mtg_ir::ManaCost::default()
            && exp_p.mana_pool == state.player(actor).map_or([0; 6], |p| p.mana_pool))
    {
        return Ok(());
    }
    let skip_self_id = matches!(cs.kind, CastKind::ActivatedAbility { .. }).then_some(cs.object);

    let Some(cur_p) = state.player(actor) else {
        return Ok(());
    };
    let act_bf: Vec<ObjectId> = cur_p
        .battlefield
        .iter()
        .copied()
        .filter(|&id| state.object(id).is_some_and(|o| !o.is_phased_out()))
        .collect();
    if act_bf.len() != exp_p.battlefield.len() {
        return Ok(());
    }

    let prev_bf = prev_digest
        .and_then(|d| d.players.iter().find(|p| p.seat == actor.0))
        .map(|p| p.battlefield.as_slice())
        .filter(|bf| bf.len() == exp_p.battlefield.len());

    let mut ts_newly_tapped: Vec<ObjectId> = Vec::new();
    for (b_idx, (&oid, exp_b)) in act_bf.iter().zip(exp_p.battlefield.iter()).enumerate() {
        if !exp_b.tapped {
            continue;
        }
        if let Some(p_bf) = prev_bf
            && p_bf[b_idx].tapped
        {
            continue;
        }
        let can_tap = |cand: ObjectId| -> bool {
            if Some(cand) == skip_self_id || ts_newly_tapped.contains(&cand) {
                return false;
            }
            let Some(obj) = state.object(cand) else {
                return false;
            };
            if obj.flags.contains(ObjectFlags::TAPPED) {
                return false;
            }
            let act_oracle = object_oracle_id(state, cand).unwrap_or_default();
            oracle_ids_match_allowing_tokens(&exp_b.oracle_id, &act_oracle)
        };
        if can_tap(oid) {
            ts_newly_tapped.push(oid);
        } else if prev_bf.is_some()
            && let Some(&alt_oid) = act_bf.iter().find(|&&cand| can_tap(cand))
        {
            ts_newly_tapped.push(alt_oid);
        }
    }
    if ts_newly_tapped.is_empty() {
        return Ok(());
    }

    let candidates = enumerate_mana_abilities(state, actor);
    // Tap deterministic single-output fixed-mana permanents before choice/multi-ability sources
    // so choice sources infer any remaining colored pip needs accurately.
    let is_deterministic_fixed = |oid: ObjectId| -> bool {
        let mut count = 0usize;
        let mut fixed = false;
        for c in &candidates {
            if c.object == oid
                && c.requires_tap
                && !c.only_as_instant
                && !c.has_sacrifice_cost
                && c.extra_mana_cost.is_none()
                && c.has_energy_cost == 0
                && !c.has_non_mana_downside
                && c.bundle_options.len() <= 1
            {
                count += 1;
                fixed = c.max_any_color_amount == 0;
            }
        }
        count == 1 && fixed
    };
    ts_newly_tapped.sort_by_key(|&oid| u8::from(!is_deterministic_fixed(oid)));

    let mut simulated_pool = state.player(actor).map_or([0; 6], |p| p.mana_pool);
    for oid in ts_newly_tapped {
        if state.pending_prompt().is_some() {
            break;
        }
        let Some((ability_index, preferred_color, added)) = pick_ts_mana_tap_action(
            state,
            actor,
            oid,
            &remaining_mana,
            &simulated_pool,
            &exp_p.mana_pool,
            exp_p.life,
            &candidates,
        ) else {
            continue;
        };
        for (dst, &src) in simulated_pool.iter_mut().zip(added.iter()) {
            *dst = dst.saturating_add(src);
        }
        apply_raw_engine_action(
            state,
            actor,
            &Action::TapMana {
                object: oid,
                ability_index,
                preferred_color,
            },
        )
        .map_err(|e| {
            step_failure(
                step,
                DiffFailureKind::EngineError,
                e.to_string(),
                Vec::new(),
            )
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drive_casting_sequence(
    state: &mut GameState,
    step: &TsReplayStep,
    prev_digest: Option<&TsStepDigest>,
    actor: PlayerId,
    modes: &[u16],
    x_value: Option<u16>,
    targets: &[TargetRef],
    divisions: &[(TargetRef, u32)],
) -> Result<(), DiffTestFailure> {
    let mut mode_cursor = 0usize;
    let mut target_cursor = 0usize;
    let mut division_cursor = 0usize;
    let mut x_applied = false;
    let mut ts_tapped_delta_applied = false;
    let mut guard = 0usize;

    while matches!(
        current_decision_state(state),
        DecisionState::CastingSequence | DecisionState::Prompt { .. }
    ) {
        guard += 1;
        if guard > 64 {
            return Err(step_failure(
                step,
                DiffFailureKind::EngineError,
                "casting sequence exceeded 64 sub-steps",
                Vec::new(),
            ));
        }

        if let Some(prompt) = state.pending_prompt().cloned() {
            if prompt.chooser != actor {
                break;
            }
            match &prompt.owner {
                PromptOwner::Casting(CastingPromptKind::CastModes { min, .. }) => {
                    let answer = if let Some(&m_idx) = modes.get(mode_cursor) {
                        mode_cursor += 1;
                        ChoiceAnswer::PickMode { index: m_idx }
                    } else if mode_cursor >= usize::from(*min) {
                        ChoiceAnswer::DonePicking
                    } else {
                        return Err(step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "cast_spell/activate_ability missing required mode selection",
                            Vec::new(),
                        ));
                    };
                    apply_raw_engine_action(
                        state,
                        actor,
                        &Action::Answer {
                            prompt_id: prompt.prompt_id,
                            answer,
                        },
                    )
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                }
                PromptOwner::Casting(CastingPromptKind::CastXValue { .. }) => {
                    if x_applied {
                        return Err(step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            "duplicate CastXValue prompt",
                            Vec::new(),
                        ));
                    }
                    x_applied = true;
                    let value = i32::from(x_value.unwrap_or(0));
                    apply_raw_engine_action(
                        state,
                        actor,
                        &Action::Answer {
                            prompt_id: prompt.prompt_id,
                            answer: ChoiceAnswer::PickNumber { value },
                        },
                    )
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                }
                _ => break,
            }
        } else if state.pending_replacement().is_some() || state.pending_trigger_order().is_some() {
            break;
        } else if let Some(cs) = state.casting() {
            match cs.step {
                CastStep::ChooseTargets => {
                    let act = if let Some(&t) = targets.get(target_cursor) {
                        target_cursor += 1;
                        Action::SelectTarget { target: t }
                    } else {
                        Action::DoneTargets
                    };
                    apply_raw_engine_action(state, actor, &act).map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                }
                CastStep::ChooseDivisions => {
                    if let Some(&(target, amount)) = divisions.get(division_cursor) {
                        division_cursor += 1;
                        apply_raw_engine_action(
                            state,
                            actor,
                            &Action::AssignDivision { target, amount },
                        )
                        .map_err(|e| {
                            step_failure(
                                step,
                                DiffFailureKind::EngineError,
                                e.to_string(),
                                Vec::new(),
                            )
                        })?;
                    } else {
                        break;
                    }
                }
                CastStep::PayCosts => {
                    if !ts_tapped_delta_applied {
                        ts_tapped_delta_applied = true;
                        replay_ts_tapped_mana_delta(state, step, prev_digest, actor)?;
                        continue;
                    }
                    apply_raw_engine_action(state, actor, &Action::AutoPayMana).map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                }
                _ => {
                    return Err(step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        format!("unexpected unprompted casting step {:?}", cs.step),
                        Vec::new(),
                    ));
                }
            }
        } else {
            break;
        }
    }

    Ok(())
}

fn apply_ts_step(
    state: &mut GameState,
    tracker: &mut TsReplayTracker,
    step: &TsReplayStep,
    prev_digest: Option<&TsStepDigest>,
) -> Result<(), DiffTestFailure> {
    let actor = PlayerId(step.actor_seat);
    let action_obj = step.action.as_object().ok_or_else(|| {
        step_failure(
            step,
            DiffFailureKind::ActionLowering,
            "step.action must be a JSON object",
            Vec::new(),
        )
    })?;
    let action_type = action_obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            step_failure(
                step,
                DiffFailureKind::ActionLowering,
                "step.action missing string 'type'",
                Vec::new(),
            )
        })?;

    match action_type {
        "mulligan_keep" => apply_raw_engine_action(state, actor, &Action::KeepHand).map_err(|e| {
            step_failure(
                step,
                DiffFailureKind::EngineError,
                e.to_string(),
                Vec::new(),
            )
        }),
        "mulligan_take" => {
            apply_raw_engine_action(state, actor, &Action::TakeMulligan).map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "london_bottom" => {
            let ids = action_obj
                .get("instanceIds")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "london_bottom missing 'instanceIds' array",
                        Vec::new(),
                    )
                })?;
            let mut objects = Vec::with_capacity(ids.len());
            for id_val in ids {
                let id_str = id_val.as_str().ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "london_bottom instanceId must be string",
                        Vec::new(),
                    )
                })?;
                objects.push(tracker.resolve_instance_id(state, step, id_str)?);
            }
            if !state.player(actor).is_some_and(|p| p.has_kept_hand) {
                apply_raw_engine_action(state, actor, &Action::KeepHand).map_err(|e| {
                    step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        e.to_string(),
                        Vec::new(),
                    )
                })?;
            }
            for obj in objects {
                apply_raw_engine_action(state, actor, &Action::BottomCard { object: obj })
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
            }
            Ok(())
        }
        "pass_priority" => {
            apply_raw_engine_action(state, actor, &Action::PassPriority).map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "concede" => apply_raw_engine_action(state, actor, &Action::Concede).map_err(|e| {
            step_failure(
                step,
                DiffFailureKind::EngineError,
                e.to_string(),
                Vec::new(),
            )
        }),
        "play_land" => {
            let id_str = action_obj
                .get("instanceId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "play_land missing 'instanceId'",
                        Vec::new(),
                    )
                })?;
            let object = tracker.resolve_instance_id(state, step, id_str)?;
            let from_zone = action_obj
                .get("fromZone")
                .and_then(|v| v.as_str())
                .and_then(parse_zone_str)
                .unwrap_or_else(|| state.object(object).map_or(Zone::Hand, |o| o.zone));
            let face_index = action_obj
                .get("faceIndex")
                .and_then(|v| v.as_u64())
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(0);
            apply_raw_engine_action(
                state,
                actor,
                &Action::PlayLand {
                    object,
                    from_zone,
                    face_index,
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "tap_mana" => {
            let id_str = action_obj
                .get("instanceId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "tap_mana missing 'instanceId'",
                        Vec::new(),
                    )
                })?;
            let object = tracker.resolve_instance_id(state, step, id_str)?;
            let preferred_color = action_obj
                .get("preferredColor")
                .and_then(|v| v.as_str())
                .and_then(parse_mana_color)
                .or_else(|| {
                    action_obj
                        .get("chosenBundle")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                        .and_then(parse_mana_color)
                });
            let candidates = enumerate_mana_abilities(state, actor);
            let ability_index = candidates
                .iter()
                .filter(|c| c.object == object)
                .find(|c| {
                    preferred_color.is_none_or(|col| {
                        c.produces_colors.contains(ColorMask::from_mana_color(col))
                            || c.max_any_color_amount > 0
                    })
                })
                .or_else(|| candidates.iter().find(|c| c.object == object))
                .map_or(0, |c| c.ability_index);
            apply_raw_engine_action(
                state,
                actor,
                &Action::TapMana {
                    object,
                    ability_index,
                    preferred_color,
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "cast_spell" => {
            let id_str = action_obj
                .get("instanceId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "cast_spell missing 'instanceId'",
                        Vec::new(),
                    )
                })?;
            let object = tracker.resolve_instance_id(state, step, id_str)?;
            let from_zone = action_obj
                .get("fromZone")
                .and_then(|v| v.as_str())
                .and_then(parse_zone_str)
                .unwrap_or_else(|| state.object(object).map_or(Zone::Hand, |o| o.zone));
            let option_str = action_obj.get("optionId").and_then(|v| v.as_str());
            let face_index = action_obj
                .get("faceIndex")
                .and_then(|v| v.as_u64())
                .and_then(|n| u8::try_from(n).ok());
            let fused = action_obj
                .get("fused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let option_id =
                match_cast_option_id(state, actor, object, option_str, face_index, fused);
            let modes: Vec<u16> = action_obj
                .get("modes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.as_u64().and_then(|n| u16::try_from(n).ok()))
                        .collect()
                })
                .unwrap_or_default();
            let x_value = action_obj
                .get("xValue")
                .and_then(|v| v.as_u64())
                .and_then(|n| u16::try_from(n).ok());
            let targets = lower_target_refs(state, tracker, step, action_obj.get("targets"))?;
            let mut divisions = Vec::new();
            if let Some(div_obj) = action_obj
                .get("targetDivisions")
                .and_then(|v| v.as_object())
            {
                for (k, v) in div_obj {
                    let amt = v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
                    if let Ok(pid) = parse_seat_id(step, k) {
                        divisions.push((TargetRef::Player { id: pid }, amt));
                    } else if let Ok(oid) = tracker.resolve_instance_id(state, step, k) {
                        divisions.push((TargetRef::Object { id: oid }, amt));
                    }
                }
            }

            apply_raw_engine_action(
                state,
                actor,
                &Action::BeginCast {
                    object,
                    option_id,
                    from_zone,
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })?;

            drive_casting_sequence(
                state,
                step,
                prev_digest,
                actor,
                &modes,
                x_value,
                &targets,
                &divisions,
            )
        }
        "activate_ability" => {
            let id_str = action_obj
                .get("instanceId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "activate_ability missing 'instanceId'",
                        Vec::new(),
                    )
                })?;
            let object = tracker.resolve_instance_id(state, step, id_str)?;
            let ability_str = action_obj
                .get("abilityId")
                .and_then(|v| v.as_str())
                .unwrap_or("0");
            let ability_index = match_activated_ability_index(state, object, ability_str)
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        format!("no matching activated ability '{ability_str}' on {object:?}"),
                        Vec::new(),
                    )
                })?;
            let modes: Vec<u16> = action_obj
                .get("chosenModes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.as_u64().and_then(|n| u16::try_from(n).ok()))
                        .collect()
                })
                .unwrap_or_default();
            let x_value = action_obj
                .get("xValue")
                .and_then(|v| v.as_u64())
                .and_then(|n| u16::try_from(n).ok());
            let targets = lower_target_refs(state, tracker, step, action_obj.get("targets"))?;

            apply_raw_engine_action(
                state,
                actor,
                &Action::BeginActivate {
                    object,
                    ability_index,
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })?;

            drive_casting_sequence(
                state,
                step,
                prev_digest,
                actor,
                &modes,
                x_value,
                &targets,
                &[],
            )
        }
        "declare_attackers" => {
            let assignments = action_obj
                .get("assignments")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "declare_attackers missing 'assignments' array",
                        Vec::new(),
                    )
                })?;
            let mut lowered = Vec::with_capacity(assignments.len());
            for item in assignments {
                let atk_id = item
                    .get("attackerInstanceId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "declare_attackers assignment missing 'attackerInstanceId'",
                            Vec::new(),
                        )
                    })?;
                let attacker = tracker.resolve_instance_id(state, step, atk_id)?;
                let target = if let Some(pw_id) =
                    item.get("defenderPlaneswalkerId").and_then(|v| v.as_str())
                {
                    AttackTarget::Planeswalker {
                        id: tracker.resolve_instance_id(state, step, pw_id)?,
                    }
                } else if let Some(bat_id) = item.get("defenderBattleId").and_then(|v| v.as_str()) {
                    AttackTarget::Battle {
                        id: tracker.resolve_instance_id(state, step, bat_id)?,
                    }
                } else {
                    let def_pid = item
                        .get("defenderPlayerId")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            step_failure(
                                step,
                                DiffFailureKind::ActionLowering,
                                "declare_attackers assignment missing 'defenderPlayerId'",
                                Vec::new(),
                            )
                        })?;
                    AttackTarget::Player {
                        id: parse_seat_id(step, def_pid)?,
                    }
                };
                lowered.push((attacker, target));
            }
            for (attacker, target) in lowered {
                apply_raw_engine_action(state, actor, &Action::AssignAttacker { attacker, target })
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
            }
            apply_raw_engine_action(state, actor, &Action::ConfirmAttackers).map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "declare_blockers" => {
            let assignments = action_obj
                .get("assignments")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "declare_blockers missing 'assignments' array",
                        Vec::new(),
                    )
                })?;
            let mut lowered = Vec::with_capacity(assignments.len());
            for item in assignments {
                let blk_id = item
                    .get("blockerInstanceId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "declare_blockers missing 'blockerInstanceId'",
                            Vec::new(),
                        )
                    })?;
                let atk_id = item
                    .get("attackerInstanceId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "declare_blockers missing 'attackerInstanceId'",
                            Vec::new(),
                        )
                    })?;
                let blocker = tracker.resolve_instance_id(state, step, blk_id)?;
                let attacker = tracker.resolve_instance_id(state, step, atk_id)?;
                lowered.push((blocker, attacker));
            }
            for (blocker, attacker) in lowered {
                apply_raw_engine_action(state, actor, &Action::AssignBlocker { blocker, attacker })
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
            }
            apply_raw_engine_action(state, actor, &Action::ConfirmBlockers).map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "choice" => lower_and_apply_ts_choice(state, tracker, step, actor, action_obj),
        other => Err(step_failure(
            step,
            DiffFailureKind::ActionLowering,
            format!("unsupported TS action type '{other}'"),
            Vec::new(),
        )),
    }
}

fn lower_and_apply_ts_choice(
    state: &mut GameState,
    tracker: &mut TsReplayTracker,
    step: &TsReplayStep,
    actor: PlayerId,
    action_obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DiffTestFailure> {
    let ans = action_obj
        .get("answer")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            step_failure(
                step,
                DiffFailureKind::ActionLowering,
                "choice action missing 'answer' object",
                Vec::new(),
            )
        })?;
    let kind = ans.get("kind").and_then(|v| v.as_str()).ok_or_else(|| {
        step_failure(
            step,
            DiffFailureKind::ActionLowering,
            "choice.answer missing 'kind'",
            Vec::new(),
        )
    })?;

    match kind {
        "yesNo" => {
            let prompt_id = active_prompt_id_for_actor(state, step, actor)?;
            let value = ans.get("yes").and_then(|v| v.as_bool()).ok_or_else(|| {
                step_failure(
                    step,
                    DiffFailureKind::ActionLowering,
                    "yesNo answer missing boolean 'yes'",
                    Vec::new(),
                )
            })?;
            apply_raw_engine_action(
                state,
                actor,
                &Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::YesNo { value },
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "chooseObjects" => {
            let ids = ans
                .get("objectIds")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "chooseObjects answer missing 'objectIds'",
                        Vec::new(),
                    )
                })?;
            let mut resolved = Vec::with_capacity(ids.len());
            for id_val in ids {
                let id_str = id_val.as_str().ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "chooseObjects objectId must be string",
                        Vec::new(),
                    )
                })?;
                resolved.push(tracker.resolve_instance_id(state, step, id_str)?);
            }
            let initial_max: Option<u16> = state.pending_prompt().and_then(|pp| match pp.request {
                ChoiceRequest::ChooseObjects { max, .. } => Some(max),
                _ => None,
            });
            let was_empty = resolved.is_empty();
            let resolved_len = resolved.len();
            for oid in resolved {
                let Some(pp) = state.pending_prompt() else {
                    break;
                };
                if pp.chooser != actor {
                    break;
                }
                let prompt_id = pp.prompt_id;
                apply_raw_engine_action(
                    state,
                    actor,
                    &Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickObject { id: oid },
                    },
                )
                .map_err(|e| {
                    step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        e.to_string(),
                        Vec::new(),
                    )
                })?;
            }
            if let Some(pp) = state.pending_prompt()
                && pp.chooser == actor
                && matches!(pp.request, ChoiceRequest::ChooseObjects { .. })
            {
                let still_same_res_pick = !matches!(pp.owner, PromptOwner::ResolutionFrame)
                    || !crate::effects::active_selection_picked(state).is_empty();
                let need_done = was_empty
                    || (still_same_res_pick
                        && initial_max.is_some_and(|m| resolved_len < usize::from(m)));
                if need_done {
                    let prompt_id = pp.prompt_id;
                    apply_raw_engine_action(
                        state,
                        actor,
                        &Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::DonePicking,
                        },
                    )
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                }
            }
            Ok(())
        }
        "chooseMode" => {
            let indices = ans
                .get("indices")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "chooseMode answer missing 'indices'",
                        Vec::new(),
                    )
                })?;
            for idx_val in indices {
                let index = idx_val
                    .as_u64()
                    .and_then(|n| u16::try_from(n).ok())
                    .ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "chooseMode index must be u16",
                            Vec::new(),
                        )
                    })?;
                let Some(pp) = state.pending_prompt() else {
                    break;
                };
                if pp.chooser != actor {
                    break;
                }
                let prompt_id = pp.prompt_id;
                apply_raw_engine_action(
                    state,
                    actor,
                    &Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::PickMode { index },
                    },
                )
                .map_err(|e| {
                    step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        e.to_string(),
                        Vec::new(),
                    )
                })?;
            }
            if let Some(pp) = state.pending_prompt()
                && pp.chooser == actor
                && matches!(pp.request, ChoiceRequest::ChooseMode { .. })
            {
                let prompt_id = pp.prompt_id;
                apply_raw_engine_action(
                    state,
                    actor,
                    &Action::Answer {
                        prompt_id,
                        answer: ChoiceAnswer::DonePicking,
                    },
                )
                .map_err(|e| {
                    step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        e.to_string(),
                        Vec::new(),
                    )
                })?;
            }
            Ok(())
        }
        "chooseNumber" => {
            let prompt_id = active_prompt_id_for_actor(state, step, actor)?;
            let value = ans
                .get("n")
                .and_then(|v| v.as_i64())
                .and_then(|n| i32::try_from(n).ok())
                .ok_or_else(|| {
                    step_failure(
                        step,
                        DiffFailureKind::ActionLowering,
                        "chooseNumber answer missing i32 'n'",
                        Vec::new(),
                    )
                })?;
            apply_raw_engine_action(
                state,
                actor,
                &Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickNumber { value },
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "chooseReplacement" => {
            let prompt_id = active_prompt_id_for_actor(state, step, actor)?;
            let index = ans
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.rsplit(':').next())
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            apply_raw_engine_action(
                state,
                actor,
                &Action::Answer {
                    prompt_id,
                    answer: ChoiceAnswer::PickReplacement { index },
                },
            )
            .map_err(|e| {
                step_failure(
                    step,
                    DiffFailureKind::EngineError,
                    e.to_string(),
                    Vec::new(),
                )
            })
        }
        "orderObjects" => {
            let mut top_ids = Vec::new();
            if let Some(arr) = ans.get("topIds").and_then(|v| v.as_array()) {
                for id_val in arr {
                    let id_str = id_val.as_str().ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "orderObjects topIds must be strings",
                            Vec::new(),
                        )
                    })?;
                    top_ids.push(tracker.resolve_instance_id(state, step, id_str)?);
                }
            }
            let mut bottom_ids = Vec::new();
            if let Some(arr) = ans.get("bottomIds").and_then(|v| v.as_array()) {
                for id_val in arr {
                    let id_str = id_val.as_str().ok_or_else(|| {
                        step_failure(
                            step,
                            DiffFailureKind::ActionLowering,
                            "orderObjects bottomIds must be strings",
                            Vec::new(),
                        )
                    })?;
                    bottom_ids.push(tracker.resolve_instance_id(state, step, id_str)?);
                }
            }
            let shuffle = ans
                .get("shuffle")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut guard = 0usize;
            while is_prompt_active(state) && current_actor(state) == Some(actor) {
                guard += 1;
                if guard > 64 {
                    return Err(step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        "orderObjects loop exceeded 64 sub-steps",
                        Vec::new(),
                    ));
                }

                if let Some(to) = state.pending_trigger_order() {
                    let next_id = top_ids
                        .iter()
                        .copied()
                        .find(|cand| to.object_ids.iter().any(|o| o.index == cand.index))
                        .unwrap_or(to.object_ids[0]);
                    let prompt_id = to.prompt_id;
                    apply_raw_engine_action(
                        state,
                        actor,
                        &Action::Answer {
                            prompt_id,
                            answer: ChoiceAnswer::OrderNext { id: next_id },
                        },
                    )
                    .map_err(|e| {
                        step_failure(
                            step,
                            DiffFailureKind::EngineError,
                            e.to_string(),
                            Vec::new(),
                        )
                    })?;
                    continue;
                }

                let Some(pp) = state.pending_prompt().cloned() else {
                    break;
                };
                match &pp.request {
                    ChoiceRequest::YesNo { prompt_id, .. } => {
                        let pause = state
                            .resolution_vms()
                            .last()
                            .and_then(|vm| vm.pause_state.as_ref());
                        let value = match pause {
                            Some(StepPauseState::ReorderLibrary {
                                deciding_card: Some(cid),
                                ..
                            }) => bottom_ids.iter().any(|b| b.index == cid.index),
                            Some(StepPauseState::OptionalChoice {
                                purpose: YesNoPausePurpose::MayShuffleAfterMoveOrLook,
                                ..
                            }) => shuffle,
                            _ => break,
                        };
                        apply_raw_engine_action(
                            state,
                            actor,
                            &Action::Answer {
                                prompt_id: *prompt_id,
                                answer: ChoiceAnswer::YesNo { value },
                            },
                        )
                        .map_err(|e| {
                            step_failure(
                                step,
                                DiffFailureKind::EngineError,
                                e.to_string(),
                                Vec::new(),
                            )
                        })?;
                    }
                    ChoiceRequest::OrderObjects { prompt_id, objects } => {
                        let next_id = top_ids
                            .iter()
                            .copied()
                            .find_map(|tid| objects.iter().copied().find(|o| o.index == tid.index))
                            .unwrap_or(objects[0]);
                        apply_raw_engine_action(
                            state,
                            actor,
                            &Action::Answer {
                                prompt_id: *prompt_id,
                                answer: ChoiceAnswer::OrderNext { id: next_id },
                            },
                        )
                        .map_err(|e| {
                            step_failure(
                                step,
                                DiffFailureKind::EngineError,
                                e.to_string(),
                                Vec::new(),
                            )
                        })?;
                    }
                    _ => break,
                }
            }
            Ok(())
        }
        other => Err(step_failure(
            step,
            DiffFailureKind::ActionLowering,
            format!("unsupported TS choice answer kind '{other}'"),
            Vec::new(),
        )),
    }
}

fn active_prompt_id_for_actor(
    state: &GameState,
    step: &TsReplayStep,
    actor: PlayerId,
) -> Result<u32, DiffTestFailure> {
    if let Some(pr) = state.pending_replacement()
        && pr.chooser == actor
    {
        return Ok(pr.prompt_id);
    }
    if let Some(to) = state.pending_trigger_order()
        && to.controller == actor
    {
        return Ok(to.prompt_id);
    }
    if let Some(pp) = state.pending_prompt()
        && pp.chooser == actor
    {
        return Ok(pp.prompt_id);
    }
    Err(step_failure(
        step,
        DiffFailureKind::EngineError,
        format!("no active prompt for seat {}", actor.0),
        Vec::new(),
    ))
}

fn project_pending_prompt_kind(state: &GameState) -> Option<String> {
    if let Some(pr) = state.pending_replacement() {
        return match &pr.prompt_kind {
            PendingReplacementPrompt::CommanderRedirect { .. } => {
                Some("commander_zone_choice".to_string())
            }
            PendingReplacementPrompt::ChooseReplacement => Some("chooseReplacement".to_string()),
            PendingReplacementPrompt::EtbCopy { .. } => Some("chooseObjects".to_string()),
            PendingReplacementPrompt::EtbChoice { .. } => state
                .pending_prompt()
                .map(|pp| choice_request_kind_str(&pp.request).to_string())
                .or_else(|| Some("yesNo".to_string())),
            PendingReplacementPrompt::AbundanceChoice { .. } => Some("yesNo".to_string()),
        };
    }

    if state.pending_trigger_order().is_some() {
        return Some("orderObjects".to_string());
    }

    if let Some(pp) = state.pending_prompt() {
        return Some(match &pp.owner {
            PromptOwner::Sba(SbaPromptKind::LegendRule { .. }) => "legend_rule".to_string(),
            PromptOwner::Sba(SbaPromptKind::CommanderReturn { .. }) => {
                "commander_zone_choice".to_string()
            }
            PromptOwner::Turn(TurnPromptKind::CleanupDiscard { .. }) => {
                "discard_to_hand_size".to_string()
            }
            PromptOwner::Turn(TurnPromptKind::UntapOptional { .. })
            | PromptOwner::Turn(TurnPromptKind::EndTurnMay) => "yesNo".to_string(),
            PromptOwner::Replacement => "chooseReplacement".to_string(),
            PromptOwner::TriggerOrder => "orderObjects".to_string(),
            PromptOwner::ResolutionFrame => {
                match state
                    .resolution_vms()
                    .last()
                    .and_then(|vm| vm.pause_state.as_ref())
                {
                    Some(StepPauseState::SearchPick { .. }) => "search_library".to_string(),
                    Some(StepPauseState::ReorderLibrary { .. }) => "scry_choice".to_string(),
                    Some(StepPauseState::GuidedResolution { .. }) => {
                        "guided_resolution".to_string()
                    }
                    _ => choice_request_kind_str(&pp.request).to_string(),
                }
            }
            PromptOwner::Casting(CastingPromptKind::GuidedFallback { .. }) => {
                "guided_resolution".to_string()
            }
            _ => choice_request_kind_str(&pp.request).to_string(),
        });
    }

    if matches!(state.decision(), DecisionState::Mulligan) {
        let actor = current_actor(state).unwrap_or(state.priority_player());
        return if state.mulligan_bottom_remaining(actor) > 0 {
            Some("london_bottom".to_string())
        } else {
            Some("mulligan".to_string())
        };
    }

    None
}

fn choice_request_kind_str(req: &ChoiceRequest) -> &'static str {
    match req {
        ChoiceRequest::YesNo { .. } => "yesNo",
        ChoiceRequest::ChooseObjects { .. } => "chooseObjects",
        ChoiceRequest::ChooseMode { .. } => "chooseMode",
        ChoiceRequest::ChooseNumber { .. } => "chooseNumber",
        ChoiceRequest::OrderObjects { .. } => "orderObjects",
        ChoiceRequest::ChooseReplacement { .. } => "chooseReplacement",
        ChoiceRequest::GuidedResolution { .. } => "guided_resolution",
    }
}

fn card_type_mask_to_ts_names(mask: CardTypeMask) -> Vec<String> {
    let mapping = [
        (CardTypeMask::LAND, "Land"),
        (CardTypeMask::CREATURE, "Creature"),
        (CardTypeMask::ARTIFACT, "Artifact"),
        (CardTypeMask::ENCHANTMENT, "Enchantment"),
        (CardTypeMask::PLANESWALKER, "Planeswalker"),
        (CardTypeMask::INSTANT, "Instant"),
        (CardTypeMask::SORCERY, "Sorcery"),
        (CardTypeMask::BATTLE, "Battle"),
        (CardTypeMask::KINDRED, "Kindred"),
    ];
    let mut out = Vec::new();
    for (flag, name) in mapping {
        if mask.contains(flag) {
            out.push(name.to_string());
        }
    }
    out
}

/// Computes a [`TsStepDigest`] directly from a Rust [`GameState`] for differential comparison.
pub fn compute_ts_step_digest(state: &GameState) -> TsStepDigest {
    let eval_cache: &LayerCache = state.evaluate_board();
    let db = state.db();

    let mut players = Vec::with_capacity(state.players().len());
    for p in state.players() {
        let mut hand_oracle_ids = Vec::with_capacity(p.hand.len());
        for &id in &p.hand {
            hand_oracle_ids
                .push(object_oracle_id(state, id).unwrap_or_else(|| "<unknown>".to_string()));
        }

        let mut battlefield = Vec::new();
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let eval = eval_cache.get(id);
            let oracle_id = object_oracle_id(state, id).unwrap_or_else(|| "<unknown>".to_string());
            let pt = eval.and_then(|e| e.pt).map(|p| [p.power, p.toughness]);
            let types = eval.map_or_else(Vec::new, |e| card_type_mask_to_ts_names(e.types));
            let mut keywords: Vec<String> = eval
                .map(|e| {
                    e.keywords
                        .iter()
                        .filter_map(|&kid| db.keyword_name(kid).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            keywords.sort();
            keywords.dedup();

            let mut counters: Vec<(String, u32)> = state
                .counters(id)
                .iter()
                .filter(|(_, count)| *count > 0)
                .map(|(cid, count)| {
                    (
                        db.counter_name(*cid).unwrap_or("unknown").to_string(),
                        *count,
                    )
                })
                .collect();
            counters.sort_by(|a, b| a.0.cmp(&b.0));

            let attached_to_oracle_id = obj
                .attached_to
                .and_then(|host| object_oracle_id(state, host));

            battlefield.push(TsBattlefieldEntryDigest {
                oracle_id,
                tapped: obj.flags.contains(ObjectFlags::TAPPED),
                pt,
                types,
                keywords,
                counters,
                damage_marked: obj.damage_marked,
                attached_to_oracle_id,
            });
        }

        players.push(TsPlayerDigest {
            seat: p.id.0,
            life: p.life,
            lost: p.lost,
            poison: p.poison,
            energy: p.energy,
            mana_pool: p.mana_pool,
            zone_counts: TsZoneCounts {
                library: p.library.len() as u32,
                hand: p.hand.len() as u32,
                battlefield: battlefield.len() as u32,
                graveyard: p.graveyard.len() as u32,
                exile: p.exile.len() as u32,
                command: p.command.len() as u32,
            },
            hand_oracle_ids,
            battlefield,
        });
    }

    TsStepDigest {
        turn_number: state.turn_number(),
        active_seat: state.active_player().0,
        priority_seat: state.priority_player().0,
        phase: TsReplayPhase::from(state.phase()),
        stack_depth: u16::try_from(state.stack_entries().len()).unwrap_or(u16::MAX),
        pending_prompt_kind: project_pending_prompt_kind(state),
        players,
    }
}

fn oracle_ids_match_allowing_tokens(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    actual.starts_with("token:") && (expected.starts_with("tok_") || expected.starts_with("token:"))
}

/// Compares an expected TS [`TsStepDigest`] against an actual Rust [`TsStepDigest`],
/// applying the documented TS↔Rust parity normalizations and returning all field mismatches.
pub fn diff_ts_step_digest(
    expected: &TsStepDigest,
    actual: &TsStepDigest,
) -> Vec<DiffFieldMismatch> {
    let mut diffs = Vec::new();

    if expected.turn_number != actual.turn_number {
        diffs.push(DiffFieldMismatch {
            path: "turnNumber".into(),
            expected: expected.turn_number.to_string(),
            actual: actual.turn_number.to_string(),
        });
    }
    if expected.active_seat != actual.active_seat {
        diffs.push(DiffFieldMismatch {
            path: "activeSeat".into(),
            expected: expected.active_seat.to_string(),
            actual: actual.active_seat.to_string(),
        });
    }

    // TS leaves priorityPlayerIndex at the pre-prompt holder while pendingPrompts[0] / mulligan
    // is active, whereas Rust sets priority_player to the chooser. Enforce strict priority_seat
    // equality whenever no prompt is pending.
    let allow_prompt_priority_freeze = actual.pending_prompt_kind.is_some()
        && expected.pending_prompt_kind == actual.pending_prompt_kind;
    if !allow_prompt_priority_freeze && expected.priority_seat != actual.priority_seat {
        diffs.push(DiffFieldMismatch {
            path: "priority_seat (prioritySeat)".into(),
            expected: expected.priority_seat.to_string(),
            actual: actual.priority_seat.to_string(),
        });
    }

    if expected.phase != actual.phase {
        diffs.push(DiffFieldMismatch {
            path: "phase".into(),
            expected: format!("{:?}", expected.phase),
            actual: format!("{:?}", actual.phase),
        });
    }
    if expected.stack_depth != actual.stack_depth {
        diffs.push(DiffFieldMismatch {
            path: "stackDepth".into(),
            expected: expected.stack_depth.to_string(),
            actual: actual.stack_depth.to_string(),
        });
    }
    if expected.pending_prompt_kind != actual.pending_prompt_kind {
        diffs.push(DiffFieldMismatch {
            path: "pending_prompt_kind (pendingPromptKind)".into(),
            expected: format!("{:?}", expected.pending_prompt_kind),
            actual: format!("{:?}", actual.pending_prompt_kind),
        });
    }

    if expected.players.len() != actual.players.len() {
        diffs.push(DiffFieldMismatch {
            path: "players.len".into(),
            expected: expected.players.len().to_string(),
            actual: actual.players.len().to_string(),
        });
        return diffs;
    }

    for (idx, (exp_p, act_p)) in expected
        .players
        .iter()
        .zip(actual.players.iter())
        .enumerate()
    {
        if exp_p.seat != act_p.seat {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].seat"),
                expected: exp_p.seat.to_string(),
                actual: act_p.seat.to_string(),
            });
        }
        if exp_p.life != act_p.life {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].life"),
                expected: exp_p.life.to_string(),
                actual: act_p.life.to_string(),
            });
        }
        if exp_p.lost != act_p.lost {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].lost"),
                expected: exp_p.lost.to_string(),
                actual: act_p.lost.to_string(),
            });
        }
        if exp_p.poison != act_p.poison {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].poison"),
                expected: exp_p.poison.to_string(),
                actual: act_p.poison.to_string(),
            });
        }
        if exp_p.energy != act_p.energy {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].energy"),
                expected: exp_p.energy.to_string(),
                actual: act_p.energy.to_string(),
            });
        }
        if exp_p.mana_pool != act_p.mana_pool {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].manaPool"),
                expected: format!("{:?}", exp_p.mana_pool),
                actual: format!("{:?}", act_p.mana_pool),
            });
        }
        if exp_p.zone_counts != act_p.zone_counts {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].zoneCounts"),
                expected: format!("{:?}", exp_p.zone_counts),
                actual: format!("{:?}", act_p.zone_counts),
            });
        }
        if exp_p.hand_oracle_ids != act_p.hand_oracle_ids {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].handOracleIds"),
                expected: format!("{:?}", exp_p.hand_oracle_ids),
                actual: format!("{:?}", act_p.hand_oracle_ids),
            });
        }

        if exp_p.battlefield.len() != act_p.battlefield.len() {
            diffs.push(DiffFieldMismatch {
                path: format!("players[{idx}].battlefield.len"),
                expected: exp_p.battlefield.len().to_string(),
                actual: act_p.battlefield.len().to_string(),
            });
            continue;
        }

        let mut player_bf_diffs = Vec::new();
        for (b_idx, (exp_b, act_b)) in exp_p
            .battlefield
            .iter()
            .zip(act_p.battlefield.iter())
            .enumerate()
        {
            if !oracle_ids_match_allowing_tokens(&exp_b.oracle_id, &act_b.oracle_id) {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].oracleId"),
                    expected: exp_b.oracle_id.clone(),
                    actual: act_b.oracle_id.clone(),
                });
            }
            if exp_b.tapped != act_b.tapped {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].tapped"),
                    expected: exp_b.tapped.to_string(),
                    actual: act_b.tapped.to_string(),
                });
            }

            // Normalize TS non-creature `[0, 0]` P/T to `None`.
            let exp_is_creature = exp_b.types.iter().any(|t| t == "Creature");
            let norm_exp_pt = if !exp_is_creature && exp_b.pt == Some([0, 0]) {
                None
            } else {
                exp_b.pt
            };
            if norm_exp_pt != act_b.pt {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].pt"),
                    expected: format!("{norm_exp_pt:?}"),
                    actual: format!("{:?}", act_b.pt),
                });
            }

            let exp_types: BTreeSet<&str> = exp_b.types.iter().map(String::as_str).collect();
            let act_types: BTreeSet<&str> = act_b.types.iter().map(String::as_str).collect();
            if exp_types != act_types {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].types"),
                    expected: format!("{exp_types:?}"),
                    actual: format!("{act_types:?}"),
                });
            }

            let normalize_kw = |raw: &str| -> String {
                let lower = raw.trim().to_ascii_lowercase();
                lower
                    .split(" {")
                    .next()
                    .unwrap_or(&lower)
                    .trim()
                    .to_string()
            };
            let mut exp_kw: Vec<String> = exp_b.keywords.iter().map(|k| normalize_kw(k)).collect();
            exp_kw.sort();
            exp_kw.dedup();
            let mut act_kw: Vec<String> = act_b.keywords.iter().map(|k| normalize_kw(k)).collect();
            act_kw.sort();
            act_kw.dedup();
            if exp_kw != act_kw {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].keywords"),
                    expected: format!("{exp_kw:?}"),
                    actual: format!("{act_kw:?}"),
                });
            }

            let mut exp_counters = exp_b.counters.clone();
            exp_counters.sort_by(|a, b| a.0.cmp(&b.0));
            let mut act_counters = act_b.counters.clone();
            act_counters.sort_by(|a, b| a.0.cmp(&b.0));
            if exp_counters != act_counters {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].counters"),
                    expected: format!("{exp_counters:?}"),
                    actual: format!("{act_counters:?}"),
                });
            }

            if exp_b.damage_marked != act_b.damage_marked {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].damageMarked"),
                    expected: exp_b.damage_marked.to_string(),
                    actual: act_b.damage_marked.to_string(),
                });
            }

            let attached_ok = match (
                exp_b.attached_to_oracle_id.as_deref(),
                act_b.attached_to_oracle_id.as_deref(),
            ) {
                (None, None) => true,
                (Some(e), Some(a)) => oracle_ids_match_allowing_tokens(e, a),
                _ => false,
            };
            if !attached_ok {
                player_bf_diffs.push(DiffFieldMismatch {
                    path: format!("players[{idx}].battlefield[{b_idx}].attachedToOracleId"),
                    expected: format!("{:?}", exp_b.attached_to_oracle_id),
                    actual: format!("{:?}", act_b.attached_to_oracle_id),
                });
            }
        }

        if !player_bf_diffs.is_empty() {
            let only_tapped_diff = player_bf_diffs.iter().all(|d| d.path.ends_with(".tapped"));
            let multiset_matches = only_tapped_diff && {
                let canonical_key = |b: &TsBattlefieldEntryDigest| {
                    let is_creature = b.types.iter().any(|t| t == "Creature");
                    let norm_pt = if !is_creature && b.pt == Some([0, 0]) {
                        None
                    } else {
                        b.pt
                    };
                    let mut types = b.types.clone();
                    types.sort();
                    types.dedup();
                    let mut kws: Vec<String> = b
                        .keywords
                        .iter()
                        .map(|k| {
                            let l = k.trim().to_ascii_lowercase();
                            l.split(" {").next().unwrap_or(&l).trim().to_string()
                        })
                        .collect();
                    kws.sort();
                    kws.dedup();
                    let mut ctrs = b.counters.clone();
                    ctrs.sort_by(|a, b| a.0.cmp(&b.0));
                    (
                        b.oracle_id.clone(),
                        b.tapped,
                        norm_pt,
                        types,
                        kws,
                        ctrs,
                        b.damage_marked,
                        b.attached_to_oracle_id.clone(),
                    )
                };
                let mut exp_ms: Vec<_> = exp_p.battlefield.iter().map(canonical_key).collect();
                let mut act_ms: Vec<_> = act_p.battlefield.iter().map(canonical_key).collect();
                exp_ms.sort();
                act_ms.sort();
                exp_ms == act_ms
            };
            if !multiset_matches {
                diffs.extend(player_bf_diffs);
            }
        }
    }

    diffs
}

/// Replays a TS-recorded [`TsReplayTrace`] against `mtg-engine` and verifies every step's [`TsStepDigest`].
pub fn run_ts_diff_test(
    db: Arc<CardDatabase>,
    trace: &TsReplayTrace,
) -> Result<DiffTestReport, DiffTestFailure> {
    let (config, decks) = build_ts_config_and_decks(&db, trace)?;
    let mut state = GameState::new(db, config, &decks, u64::from(trace.seed)).map_err(|e| {
        init_failure(
            DiffFailureKind::EngineError,
            format!("GameState::new failed: {e}"),
            Vec::new(),
        )
    })?;
    begin_opening_mulligans(&mut state).map_err(|e| {
        init_failure(
            DiffFailureKind::EngineError,
            format!("begin_opening_mulligans failed: {e}"),
            Vec::new(),
        )
    })?;
    verify_initial_state(&state, trace)?;

    let mut tracker = TsReplayTracker::from_initial_state(&state);
    let is_ts_4p_commander = trace.seats.len() > 2 && trace.rules.commander;
    let mut ts_round_number: u32 = 1;
    let mut prev_turn_serial: u32 = state.turn_serial();
    let mut prev_normal_seat: u8 = state.active_player().0;

    let mut prev_digest: Option<&TsStepDigest> = None;

    for step in &trace.steps {
        let was_mulligan = matches!(current_decision_state(&state), DecisionState::Mulligan);
        let prev_extra_len = state
            .rare_turn_state()
            .map_or(0, |r| r.extra_turn_queue.len());
        apply_ts_step(&mut state, &mut tracker, step, prev_digest)?;

        // TS 4p Commander (firstTurnDraw: true, CR 103.8c) draws 1 card for seat 0 upon
        // leaving mulligan into Turn 1 Main1, whereas Rust turn.rs starts Turn 1 in Main1
        // without an opening draw (verified by t1_f14_01).
        if was_mulligan
            && !is_prompt_active(&state)
            && matches!(current_decision_state(&state), DecisionState::Priority)
            && is_ts_4p_commander
            && state.turn_number() == 1
            && state.phase() == GamePhase::Main1
        {
            begin_action(&mut state);
            match draw_one_raw(&mut state, PlayerId(0)) {
                Ok(Some(drawn_id)) => {
                    emit_event(
                        &mut state,
                        AtomicEventKind::DrawCard {
                            player: PlayerId(0),
                            object: drawn_id,
                            in_own_draw_step_ordinal: None,
                        },
                    );
                    end_action(&mut state);
                    state.prune_stale_lki();
                }
                Ok(None) => {
                    end_action(&mut state);
                    state.prune_stale_lki();
                }
                Err(err) => {
                    end_action(&mut state);
                    return Err(step_failure(
                        step,
                        DiffFailureKind::EngineError,
                        format!("4p Turn 1 draw parity bridge failed: {err}"),
                        Vec::new(),
                    ));
                }
            }
        }

        if state.turn_serial() > prev_turn_serial {
            let new_active = state.active_player().0;
            let new_extra_len = state
                .rare_turn_state()
                .map_or(0, |r| r.extra_turn_queue.len());
            let is_extra_turn = state.last_normal_turn_player() != Some(state.active_player())
                || new_extra_len < prev_extra_len;
            if is_extra_turn || new_active <= prev_normal_seat {
                ts_round_number = ts_round_number.saturating_add(1);
            }
            prev_turn_serial = state.turn_serial();
            if !is_extra_turn {
                prev_normal_seat = new_active;
            }
        }

        tracker.refresh_after_step(&state);
        let mut actual_digest = compute_ts_step_digest(&state);
        actual_digest.turn_number = ts_round_number;
        let diffs = diff_ts_step_digest(&step.digest_after, &actual_digest);
        if !diffs.is_empty() {
            return Err(step_failure(
                step,
                DiffFailureKind::DigestMismatch,
                format!(
                    "step {} digest mismatch ({} field(s))",
                    step.step_index,
                    diffs.len()
                ),
                diffs,
            ));
        }
        prev_digest = Some(&step.digest_after);
    }

    Ok(DiffTestReport {
        seed: trace.seed,
        seat_count: trace.seats.len(),
        steps_replayed: trace.steps.len(),
        final_turn: state.turn_number(),
        final_phase: TsReplayPhase::from(state.phase()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_player_digest(seat: u8) -> serde_json::Value {
        serde_json::json!({
            "seat": seat,
            "life": 40,
            "lost": false,
            "poison": 0,
            "energy": 2,
            "manaPool": [1, 0, 2, 0, 0, 1],
            "zoneCounts": {
                "library": 91,
                "hand": 7,
                "battlefield": 2,
                "graveyard": 1,
                "exile": 0,
                "command": 1
            },
            "handOracleIds": ["synth_land_0", "synth_spell_1"],
            "battlefield": [
                {
                    "oracleId": "synth_creature_0",
                    "tapped": false,
                    "pt": [3, 4],
                    "types": ["Creature"],
                    "keywords": ["trample", "vigilance"],
                    "counters": [["+1/+1", 1]],
                    "damageMarked": 1,
                    "attachedToOracleId": null
                },
                {
                    "oracleId": "synth_aura_0",
                    "tapped": true,
                    "pt": null,
                    "types": ["Enchantment"],
                    "keywords": [],
                    "counters": [],
                    "damageMarked": 0,
                    "attachedToOracleId": "synth_creature_0"
                }
            ]
        })
    }

    #[test]
    fn ts_replay_trace_round_trip_with_all_locator_variants() {
        let trace_json = serde_json::json!({
            "formatVersion": 1,
            "compilerVersion": 46,
            "seed": 42,
            "postInitRngState": 3_141_592_653u32,
            "rules": {
                "startingLife": 40,
                "commander": true,
                "freeFirstMulligan": true
            },
            "seats": [
                {
                    "seat": 0,
                    "deckId": "synth_deck_0",
                    "commanders": ["synth_cmd_0"],
                    "libraryOrderInitial": ["synth_land_0", "synth_spell_1"],
                    "handInitial": ["synth_land_0"]
                }
            ],
            "steps": [
                {
                    "stepIndex": 0,
                    "actorSeat": 0,
                    "action": { "type": "mulligan_keep" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1,
                        "activeSeat": 0,
                        "prioritySeat": 0,
                        "phase": "mulligan",
                        "stackDepth": 0,
                        "pendingPromptKind": "mulligan",
                        "players": [sample_player_digest(0)]
                    }
                },
                {
                    "stepIndex": 1,
                    "actorSeat": 0,
                    "action": { "type": "cast_spell", "instanceId": "crd_1" },
                    "objectLocators": {
                        "crd_1": {
                            "seat": 0,
                            "zone": "hand",
                            "zoneIndex": 1,
                            "oracleId": "synth_spell_1"
                        },
                        "stk_1": {
                            "seat": 0,
                            "zone": "stack",
                            "zoneIndex": 0,
                            "oracleId": "synth_spell_1",
                            "initialCardOrdinal": 8
                        },
                        "crd_2": {
                            "ownerSeat": 0,
                            "zone": "battlefield",
                            "ordinal": 0,
                            "oracleId": "synth_creature_0"
                        },
                        "crd_3": {
                            "initialCardOrdinal": 12,
                            "seat": 0,
                            "oracleId": "synth_land_0"
                        },
                        "tok_1": {
                            "tokenSeq": 0,
                            "seat": 0,
                            "oracleId": "tok_beast"
                        }
                    },
                    "digestAfter": {
                        "turnNumber": 1,
                        "activeSeat": 0,
                        "prioritySeat": 1,
                        "phase": "main1",
                        "stackDepth": 1,
                        "pendingPromptKind": null,
                        "players": [sample_player_digest(0)]
                    }
                }
            ]
        });

        let parsed: TsReplayTrace =
            serde_json::from_value(trace_json).expect("full TsReplayTrace must deserialize");
        assert_eq!(parsed.steps[0].digest_after.phase, TsReplayPhase::Mulligan);
        assert_eq!(parsed.steps[1].digest_after.phase, TsReplayPhase::Main1);

        let locators = &parsed.steps[1].object_locators;
        assert_eq!(
            locators.get("crd_1"),
            Some(&ObjectLocator::ZoneSlot(ZoneSlotLocator {
                seat: 0,
                zone: Zone::Hand,
                zone_index: 1,
                oracle_id: "synth_spell_1".to_string(),
                initial_card_ordinal: None,
                token_seq: None,
            }))
        );
        assert_eq!(
            locators.get("stk_1"),
            Some(&ObjectLocator::ZoneSlot(ZoneSlotLocator {
                seat: 0,
                zone: Zone::Stack,
                zone_index: 0,
                oracle_id: "synth_spell_1".to_string(),
                initial_card_ordinal: Some(8),
                token_seq: None,
            }))
        );
        assert_eq!(
            locators.get("crd_2"),
            Some(&ObjectLocator::OwnerOrdinal(OwnerOrdinalLocator {
                owner_seat: 0,
                zone: Zone::Battlefield,
                ordinal: 0,
                oracle_id: "synth_creature_0".to_string(),
            }))
        );
        assert_eq!(
            locators.get("crd_3"),
            Some(&ObjectLocator::InitialCard(InitialCardLocator {
                initial_card_ordinal: 12,
                token_seq: None,
                seat: Some(0),
                oracle_id: Some("synth_land_0".to_string()),
            }))
        );
        assert_eq!(
            locators.get("tok_1"),
            Some(&ObjectLocator::Token(TokenLocator {
                token_seq: 0,
                initial_card_ordinal: None,
                seat: Some(0),
                oracle_id: Some("tok_beast".to_string()),
            }))
        );

        let re_json = serde_json::to_string(&parsed).expect("serialize TsReplayTrace");
        let reparsed: TsReplayTrace =
            serde_json::from_str(&re_json).expect("re-deserialize TsReplayTrace");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn ts_replay_structs_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<TsReplayRules>(serde_json::json!({
                "startingLife": 20,
                "commander": false,
                "freeFirstMulligan": false,
                "extra": 1
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplaySeat>(serde_json::json!({
                "seat": 0,
                "deckId": "d",
                "commanders": [],
                "libraryOrderInitial": [],
                "handInitial": [],
                "extra": true
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsZoneCounts>(serde_json::json!({
                "library": 53,
                "hand": 7,
                "battlefield": 0,
                "graveyard": 0,
                "exile": 0,
                "command": 0,
                "stack": 0
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsBattlefieldEntryDigest>(serde_json::json!({
                "oracleId": "c",
                "tapped": false,
                "pt": [1, 1],
                "types": ["Creature"],
                "keywords": [],
                "counters": [],
                "damageMarked": 0,
                "attachedToOracleId": null,
                "extra": "nope"
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsPlayerDigest>(serde_json::json!({
                "seat": 0,
                "life": 20,
                "lost": false,
                "poison": 0,
                "energy": 0,
                "manaPool": [0, 0, 0, 0, 0, 0],
                "zoneCounts": {
                    "library": 53,
                    "hand": 7,
                    "battlefield": 0,
                    "graveyard": 0,
                    "exile": 0,
                    "command": 0
                },
                "handOracleIds": [],
                "battlefield": [],
                "extra": 42
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsStepDigest>(serde_json::json!({
                "turnNumber": 1,
                "activeSeat": 0,
                "prioritySeat": 0,
                "phase": "main1",
                "stackDepth": 0,
                "pendingPromptKind": null,
                "players": [],
                "extra": "bad"
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplayStep>(serde_json::json!({
                "stepIndex": 0,
                "actorSeat": 0,
                "action": { "type": "pass_priority" },
                "objectLocators": {},
                "digestAfter": {
                    "turnNumber": 1,
                    "activeSeat": 0,
                    "prioritySeat": 0,
                    "phase": "main1",
                    "stackDepth": 0,
                    "pendingPromptKind": null,
                    "players": []
                },
                "rngStateBefore": 123
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplayTrace>(serde_json::json!({
                "formatVersion": 1,
                "compilerVersion": 46,
                "seed": 42,
                "postInitRngState": 42,
                "rules": {
                    "startingLife": 20,
                    "commander": false,
                    "freeFirstMulligan": false
                },
                "seats": [],
                "steps": [],
                "unexpected": 99
            }))
            .is_err()
        );

        for bad_locator in [
            serde_json::json!({ "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "c", "extra": 1 }),
            serde_json::json!({ "ownerSeat": 0, "zone": "hand", "ordinal": 0, "oracleId": "c", "extra": 1 }),
            serde_json::json!({ "initialCardOrdinal": 0, "extra": 1 }),
            serde_json::json!({ "tokenSeq": 0, "extra": 1 }),
        ] {
            assert!(
                serde_json::from_value::<ObjectLocator>(bad_locator.clone()).is_err(),
                "locator with unknown field must fail: {bad_locator}"
            );
        }
    }

    #[test]
    fn object_locator_rejects_empty_and_null_identity_objects() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "initialCardOrdinal": null, "tokenSeq": null }),
            serde_json::json!({ "initialCardOrdinal": null }),
            serde_json::json!({ "tokenSeq": null }),
            serde_json::json!({ "seat": 0, "oracleId": "card_only" }),
        ] {
            assert!(
                serde_json::from_value::<ObjectLocator>(bad.clone()).is_err(),
                "ambiguous/empty locator must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn ts_replay_phase_mulligan_and_game_phase_conversion() {
        let mulligan: TsReplayPhase =
            serde_json::from_value(serde_json::json!("mulligan")).unwrap();
        assert_eq!(mulligan, TsReplayPhase::Mulligan);
        assert_eq!(mulligan.as_game_phase(), None);

        let all_game_phases = [
            GamePhase::Untap,
            GamePhase::Upkeep,
            GamePhase::Draw,
            GamePhase::Main1,
            GamePhase::BeginCombat,
            GamePhase::DeclareAttackers,
            GamePhase::DeclareBlockers,
            GamePhase::FirstStrikeDamage,
            GamePhase::CombatDamage,
            GamePhase::EndCombat,
            GamePhase::Main2,
            GamePhase::EndStep,
            GamePhase::Cleanup,
        ];
        for gp in all_game_phases {
            let rp = TsReplayPhase::from(gp);
            assert_eq!(rp.as_game_phase(), Some(gp));
            let serialized = serde_json::to_value(rp).unwrap();
            assert_eq!(serialized, serde_json::to_value(gp).unwrap());
            let decoded: TsReplayPhase = serde_json::from_value(serialized).unwrap();
            assert_eq!(decoded, rp);
        }

        assert!(serde_json::from_value::<TsReplayPhase>(serde_json::json!("combat")).is_err());
    }

    #[test]
    fn ts_battlefield_entry_digest_supports_negative_unclamped_and_null_pt() {
        for pt_val in [
            serde_json::json!([-3, -1]),
            serde_json::json!([4, -2]),
            serde_json::json!([0, 0]),
            serde_json::json!(null),
        ] {
            let entry_json = serde_json::json!({
                "oracleId": "synth_card",
                "tapped": false,
                "pt": pt_val,
                "types": ["Creature"],
                "keywords": [],
                "counters": [],
                "damageMarked": 0,
                "attachedToOracleId": null
            });
            let parsed: TsBattlefieldEntryDigest = serde_json::from_value(entry_json.clone())
                .expect("unclamped or null pt must deserialize");
            let re_val = serde_json::to_value(&parsed).unwrap();
            assert_eq!(re_val["pt"], entry_json["pt"]);
        }
    }

    fn synth_diff_db() -> Arc<CardDatabase> {
        use mtg_ir::{
            CardDef, CardLayout, Characteristics, ColorMask, FaceDef, IR_V2_VERSION, InternTables,
            IrV2Bundle, ManaCost, SupertypeMask,
        };
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards: vec![
                CardDef {
                    oracle_id: "synth_plains".into(),
                    compiler_version: 46,
                    text_hash: "00000001".into(),
                    name: "Synth Plains".into(),
                    layout: CardLayout::Normal,
                    faces: vec![FaceDef {
                        name: "Synth Plains".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::default(),
                            types: CardTypeMask::LAND,
                            subtypes: smallvec::SmallVec::new(),
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
                    oracle_id: "synth_relic".into(),
                    compiler_version: 46,
                    text_hash: "00000002".into(),
                    name: "Synth Relic".into(),
                    layout: CardLayout::Normal,
                    faces: vec![FaceDef {
                        name: "Synth Relic".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::default(),
                            types: CardTypeMask::ARTIFACT,
                            subtypes: smallvec::SmallVec::new(),
                            supertypes: SupertypeMask::empty(),
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
            ],
            decks: vec![],
        };
        Arc::new(CardDatabase::from_bundle(bundle).unwrap())
    }

    #[test]
    fn run_ts_diff_test_1v1_cast_and_noncreature_pt_normalization() {
        let db = synth_diff_db();
        let p0_init = serde_json::json!({
            "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_relic"; 7],
            "battlefield": []
        });
        let p0_cast = serde_json::json!({
            "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 6, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_relic"; 6],
            "battlefield": []
        });
        let p0_resolved = serde_json::json!({
            "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 6, "battlefield": 1, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_relic"; 6],
            "battlefield": [{
                "oracleId": "synth_relic", "tapped": false, "pt": [0, 0],
                "types": ["Artifact"], "keywords": [], "counters": [],
                "damageMarked": 0, "attachedToOracleId": null
            }]
        });
        let p1 = serde_json::json!({
            "seat": 1, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_plains"; 7],
            "battlefield": []
        });
        let trace: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
            "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
            "seats": [
                { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": vec!["synth_relic"; 60], "handInitial": vec!["synth_relic"; 7] },
                { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": vec!["synth_plains"; 60], "handInitial": vec!["synth_plains"; 7] }
            ],
            "steps": [
                {
                    "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {},
                    "digestAfter": { "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0, "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan", "players": [p0_init.clone(), p1.clone()] }
                },
                {
                    "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {},
                    "digestAfter": { "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0, "phase": "main1", "stackDepth": 0, "pendingPromptKind": null, "players": [p0_init, p1.clone()] }
                },
                {
                    "stepIndex": 2, "actorSeat": 0,
                    "action": { "type": "cast_spell", "instanceId": "obj_0", "optionId": "normal" },
                    "objectLocators": { "obj_0": { "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "synth_relic" } },
                    "digestAfter": { "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0, "phase": "main1", "stackDepth": 1, "pendingPromptKind": null, "players": [p0_cast.clone(), p1.clone()] }
                },
                {
                    "stepIndex": 3, "actorSeat": 0, "action": { "type": "pass_priority" }, "objectLocators": {},
                    "digestAfter": { "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1, "phase": "main1", "stackDepth": 1, "pendingPromptKind": null, "players": [p0_cast, p1.clone()] }
                },
                {
                    "stepIndex": 4, "actorSeat": 1, "action": { "type": "pass_priority" }, "objectLocators": {},
                    "digestAfter": { "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0, "phase": "main1", "stackDepth": 0, "pendingPromptKind": null, "players": [p0_resolved, p1] }
                }
            ]
        })).unwrap();

        let report = run_ts_diff_test(db, &trace).expect("1v1 synthetic trace must pass");
        assert_eq!(report.steps_replayed, 5);
        assert_eq!(report.seat_count, 2);
    }

    #[test]
    fn run_ts_diff_test_4p_commander_turn1_draw_bridge() {
        let db = synth_diff_db();
        let mut seats = Vec::new();
        let mut players = Vec::new();
        for s in 0..4u8 {
            seats.push(serde_json::json!({
                "seat": s, "deckId": format!("s{s}"), "commanders": [],
                "libraryOrderInitial": vec!["synth_plains"; 100],
                "handInitial": vec!["synth_plains"; 7]
            }));
            players.push(serde_json::json!({
                "seat": s, "life": 40, "lost": false, "poison": 0, "energy": 0,
                "manaPool": [0, 0, 0, 0, 0, 0],
                "zoneCounts": { "library": 93, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
                "handOracleIds": vec!["synth_plains"; 7],
                "battlefield": []
            }));
        }
        let mut main1_players = players.clone();
        main1_players[0] = serde_json::json!({
            "seat": 0, "life": 40, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 92, "hand": 8, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_plains"; 8],
            "battlefield": []
        });
        let mull_digest = serde_json::json!({
            "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
            "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
            "players": players
        });
        let main1_digest = serde_json::json!({
            "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
            "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
            "players": main1_players
        });
        let trace: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1, "compilerVersion": 46, "seed": 99, "postInitRngState": 99,
            "rules": { "startingLife": 40, "commander": true, "freeFirstMulligan": true },
            "seats": seats,
            "steps": [
                { "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mull_digest.clone() },
                { "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mull_digest.clone() },
                { "stepIndex": 2, "actorSeat": 2, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": mull_digest },
                { "stepIndex": 3, "actorSeat": 3, "action": { "type": "mulligan_keep" }, "objectLocators": {}, "digestAfter": main1_digest }
            ]
        })).unwrap();

        let report = run_ts_diff_test(db, &trace).expect("4p Turn 1 draw bridge must pass");
        assert_eq!(report.steps_replayed, 4);
        assert_eq!(report.seat_count, 4);
    }

    #[test]
    fn run_ts_diff_test_rejects_missing_oracle_unshuffled_library_and_strict_priority() {
        let db = synth_diff_db();

        // 1. Missing oracle ID
        let bad_oracle: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
            "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
            "seats": [
                { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": ["synth_missing"], "handInitial": [] },
                { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": ["synth_plains"], "handInitial": [] }
            ],
            "steps": []
        })).unwrap();
        let err1 = run_ts_diff_test(Arc::clone(&db), &bad_oracle).unwrap_err();
        assert_eq!(err1.kind, DiffFailureKind::UnknownOracleId);

        // 2. Unshuffled libraryOrderInitial mismatch
        let mut unshuffled = vec!["synth_plains"; 30];
        unshuffled.extend(vec!["synth_relic"; 30]);
        let bad_lib: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
            "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
            "seats": [
                { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": unshuffled.clone(), "handInitial": [] },
                { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": unshuffled, "handInitial": [] }
            ],
            "steps": []
        })).unwrap();
        let err2 = run_ts_diff_test(Arc::clone(&db), &bad_lib).unwrap_err();
        assert_eq!(err2.kind, DiffFailureKind::InitialStateMismatch);

        // 3. Strict priority_seat enforcement when pendingPromptKind is null
        let p_ok = serde_json::json!({
            "seat": 0, "life": 20, "lost": false, "poison": 0, "energy": 0,
            "manaPool": [0, 0, 0, 0, 0, 0],
            "zoneCounts": { "library": 53, "hand": 7, "battlefield": 0, "graveyard": 0, "exile": 0, "command": 0 },
            "handOracleIds": vec!["synth_plains"; 7],
            "battlefield": []
        });
        let mut p1_ok = p_ok.clone();
        p1_ok["seat"] = serde_json::json!(1);
        let bad_prio: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1, "compilerVersion": 46, "seed": 42, "postInitRngState": 42,
            "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
            "seats": [
                { "seat": 0, "deckId": "s0", "commanders": [], "libraryOrderInitial": vec!["synth_plains"; 60], "handInitial": [] },
                { "seat": 1, "deckId": "s1", "commanders": [], "libraryOrderInitial": vec!["synth_plains"; 60], "handInitial": [] }
            ],
            "steps": [
                {
                    "stepIndex": 0, "actorSeat": 0, "action": { "type": "mulligan_keep" }, "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
                        "players": [p_ok.clone(), p1_ok.clone()]
                    }
                },
                {
                    "stepIndex": 1, "actorSeat": 1, "action": { "type": "mulligan_keep" }, "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
                        "players": [p_ok, p1_ok]
                    }
                }
            ]
        })).unwrap();
        let err3 = run_ts_diff_test(db, &bad_prio).unwrap_err();
        assert_eq!(err3.kind, DiffFailureKind::DigestMismatch);
        assert!(err3.to_string().contains("priority_seat"));
    }

    #[test]
    fn run_ts_diff_test_mulligan_take_and_london_bottom() {
        let db = synth_diff_db();
        let mut sorted_deck: Vec<&str> = Vec::with_capacity(60);
        sorted_deck.extend(std::iter::repeat_n("synth_plains", 30));
        sorted_deck.extend(std::iter::repeat_n("synth_relic", 30));

        let mut rng = crate::rng::RngState::mulberry32(42);
        let mut p0_lib_init = sorted_deck.clone();
        rng.shuffle(&mut p0_lib_init);
        let p0_hand_init = p0_lib_init[0..7].to_vec();

        let mut p1_lib_init = sorted_deck;
        rng.shuffle(&mut p1_lib_init);
        let p1_hand_init = p1_lib_init[0..7].to_vec();

        // Simulate TS mulliganRedraw x2 on P0: append remaining hand to end of library, then shuffle.
        let mut p0_lib: Vec<&str> = p0_lib_init[7..].to_vec();
        p0_lib.extend_from_slice(&p0_hand_init);
        rng.shuffle(&mut p0_lib);
        let p0_hand_m1: Vec<&str> = p0_lib.drain(0..7).collect();

        p0_lib.extend_from_slice(&p0_hand_m1);
        rng.shuffle(&mut p0_lib);
        let p0_hand_m2: Vec<&str> = p0_lib.drain(0..7).collect();

        let bot_0_oracle = p0_hand_m2[0];
        let bot_1_oracle = p0_hand_m2[1];
        let p0_hand_after_bot: Vec<&str> = p0_hand_m2[2..].to_vec();

        let make_player = |seat: u8, lib_cnt: u32, hand: &[&str]| {
            serde_json::json!({
                "seat": seat, "life": 20, "lost": false, "poison": 0, "energy": 0,
                "manaPool": [0, 0, 0, 0, 0, 0],
                "zoneCounts": {
                    "library": lib_cnt,
                    "hand": hand.len() as u32,
                    "battlefield": 0,
                    "graveyard": 0,
                    "exile": 0,
                    "command": 0
                },
                "handOracleIds": hand,
                "battlefield": []
            })
        };

        let p1_digest = make_player(1, 53, &p1_hand_init);
        let trace: TsReplayTrace = serde_json::from_value(serde_json::json!({
            "formatVersion": 1,
            "compilerVersion": 46,
            "seed": 42,
            "postInitRngState": 42,
            "rules": { "startingLife": 20, "commander": false, "freeFirstMulligan": false },
            "seats": [
                {
                    "seat": 0,
                    "deckId": "s0",
                    "commanders": [],
                    "libraryOrderInitial": p0_lib_init,
                    "handInitial": p0_hand_init
                },
                {
                    "seat": 1,
                    "deckId": "s1",
                    "commanders": [],
                    "libraryOrderInitial": p1_lib_init,
                    "handInitial": p1_hand_init
                }
            ],
            "steps": [
                {
                    "stepIndex": 0,
                    "actorSeat": 0,
                    "action": { "type": "mulligan_take" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
                        "players": [make_player(0, 53, &p0_hand_m1), p1_digest.clone()]
                    }
                },
                {
                    "stepIndex": 1,
                    "actorSeat": 0,
                    "action": { "type": "mulligan_take" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
                        "players": [make_player(0, 53, &p0_hand_m2), p1_digest.clone()]
                    }
                },
                {
                    "stepIndex": 2,
                    "actorSeat": 0,
                    "action": { "type": "mulligan_keep" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 1,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "mulligan",
                        "players": [make_player(0, 53, &p0_hand_m2), p1_digest.clone()]
                    }
                },
                {
                    "stepIndex": 3,
                    "actorSeat": 1,
                    "action": { "type": "mulligan_keep" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": "london_bottom",
                        "players": [make_player(0, 53, &p0_hand_m2), p1_digest.clone()]
                    }
                },
                {
                    "stepIndex": 4,
                    "actorSeat": 0,
                    "action": { "type": "london_bottom", "instanceIds": ["bot_0", "bot_1"] },
                    "objectLocators": {
                        "bot_0": { "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": bot_0_oracle },
                        "bot_1": { "seat": 0, "zone": "hand", "zoneIndex": 1, "oracleId": bot_1_oracle }
                    },
                    "digestAfter": {
                        "turnNumber": 1, "activeSeat": 0, "prioritySeat": 0,
                        "phase": "main1", "stackDepth": 0, "pendingPromptKind": null,
                        "players": [make_player(0, 55, &p0_hand_after_bot), p1_digest]
                    }
                }
            ]
        }))
        .unwrap();

        let report = run_ts_diff_test(Arc::clone(&db), &trace)
            .expect("Mulberry32Compat mulligan_take + london_bottom trace must pass");
        assert_eq!(report.steps_replayed, 5);

        // Also assert exact bottomed library[0]/library[1] order `[id_1, id_0, ...]` directly on GameState.
        let (config, decks) = build_ts_config_and_decks(&db, &trace).unwrap();
        let mut state = GameState::new(db, config, &decks, 42).unwrap();
        begin_opening_mulligans(&mut state).unwrap();
        let mut tracker = TsReplayTracker::from_initial_state(&state);
        let mut prev_digest = None;
        for step in &trace.steps {
            apply_ts_step(&mut state, &mut tracker, step, prev_digest).unwrap();
            tracker.refresh_after_step(&state);
            prev_digest = Some(&step.digest_after);
        }
        let p0_lib = &state.player(PlayerId(0)).unwrap().library;
        assert_eq!(p0_lib.len(), 55);
        assert_eq!(
            object_oracle_id(&state, p0_lib[0]).as_deref(),
            Some(bot_1_oracle),
            "second bottomed card id_1 must sit at absolute bottom library[0]"
        );
        assert_eq!(
            object_oracle_id(&state, p0_lib[1]).as_deref(),
            Some(bot_0_oracle),
            "first bottomed card id_0 must sit directly above id_1 at library[1]"
        );
    }
}
