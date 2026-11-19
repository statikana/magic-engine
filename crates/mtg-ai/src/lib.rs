//! `mtg-ai`: Heuristic evaluation, parallel ISMCTS (`rayon`), and pluggable neural/policy
//! traits for `mtg-engine`.

use mtg_engine::{Action, GameState, MatchSession, PlayerId, RngState};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchBudget {
    pub determinizations: u32,
    pub iterations_per_determinization: u32,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            determinizations: 8,
            iterations_per_determinization: 64,
        }
    }
}

/// Evaluates a live or determinized [`GameState`] for multi-player MaxN / ISMCTS search.
///
/// Swap `HeuristicEvaluator` for an ONNX/Candle neural model later with zero changes to search.
pub trait StateEvaluator: Send + Sync {
    /// Returns per-player win probabilities summing to `1.0` (supports 2..N players uniformly).
    fn evaluate(&self, state: &GameState) -> SmallVec<[f32; 4]>;

    /// Returns prior move probabilities aligned 1:1 with `legal_actions` (PUCT guidance).
    fn policy_priors(&self, _state: &GameState, legal_actions: &[Action]) -> Vec<f32> {
        if legal_actions.is_empty() {
            return Vec::new();
        }
        let p = 1.0 / (legal_actions.len() as f32);
        vec![p; legal_actions.len()]
    }
}

pub trait Agent: Send + Sync {
    fn choose_action(
        &self,
        session: &MatchSession,
        viewer: PlayerId,
        budget: SearchBudget,
        rng: &mut RngState,
    ) -> Option<Action>;
}

/// Baseline heuristic evaluator (Phase 3 will expand with board/tempo/threat/combat scoring).
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicEvaluator;

impl StateEvaluator for HeuristicEvaluator {
    fn evaluate(&self, state: &GameState) -> SmallVec<[f32; 4]> {
        let alive_count = state.players().iter().filter(|p| !p.lost).count();
        let mut scores = SmallVec::with_capacity(state.players().len());
        if alive_count == 0 {
            scores.resize(state.players().len(), 0.0);
            return scores;
        }
        let mut total = 0.0f32;
        for p in state.players() {
            let s = if p.lost {
                0.0
            } else {
                (p.life.max(1) as f32) + (p.battlefield.len() as f32) * 3.0 + (p.hand.len() as f32)
            };
            scores.push(s);
            total += s;
        }
        if total > 0.0 {
            for s in &mut scores {
                *s /= total;
            }
        }
        scores
    }
}

/// Deterministic random-policy baseline agent for fuzzing and smoke tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomAgent;

impl Agent for RandomAgent {
    fn choose_action(
        &self,
        session: &MatchSession,
        viewer: PlayerId,
        _budget: SearchBudget,
        rng: &mut RngState,
    ) -> Option<Action> {
        let legal = session.legal_actions(viewer);
        if legal.is_empty() {
            return None;
        }
        let idx = rng.next_index(legal.len());
        Some(legal[idx].clone())
    }
}
