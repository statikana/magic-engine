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

/// Samples a zero-leak determinized [`GameState`] from `session` for `viewer`.
pub fn sample_determinization(
    session: &MatchSession,
    viewer: PlayerId,
    rng: &mut RngState,
) -> GameState {
    session.determinize(viewer, rng)
}

/// Builds a viewer-relative [`mtg_engine::Observation`] aligned with `session.legal_actions(viewer)`.
pub fn observe_session(session: &MatchSession, viewer: PlayerId) -> mtg_engine::Observation {
    session.observe(viewer)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mtg_engine::{DeckSpec, GameConfig};
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, PrintedPt, SupertypeMask, Zone,
    };

    use super::*;

    fn make_synth_session(seed: u64) -> MatchSession {
        let make_card =
            |id: u32, name: &str, types: CardTypeMask, pt: Option<(i32, i32)>| CardDef {
                oracle_id: format!("synth_ai_{id}"),
                compiler_version: 46,
                text_hash: format!("ai_{id}"),
                name: name.into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: name.into(),
                    characteristics: Characteristics {
                        mana_cost: if types.contains(CardTypeMask::LAND) {
                            ManaCost::default()
                        } else {
                            ManaCost::parse("{1}{G}")
                        },
                        types,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: if types.contains(CardTypeMask::LAND) {
                            ColorMask::empty()
                        } else {
                            ColorMask::G
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

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    make_card(0, "Synth Forest", CardTypeMask::LAND, None),
                    make_card(1, "Synth Bear", CardTypeMask::CREATURE, Some((2, 2))),
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut mainboard = vec![CardDefId(0); 24];
        mainboard.extend(vec![CardDefId(1); 36]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        MatchSession::new(
            db,
            GameConfig::standard_1v1(),
            vec![deck.clone(), deck],
            seed,
        )
        .unwrap()
    }

    #[test]
    fn heuristic_evaluator_sums_to_one_and_random_agent_picks_legal_action() {
        let session = make_synth_session(42);
        let eval = HeuristicEvaluator;
        let scores = eval.evaluate(session.state());
        assert_eq!(scores.len(), 2);
        let sum: f32 = scores.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-5,
            "scores should sum to 1.0: {scores:?}"
        );

        let mut rng = RngState::splitmix64(77);
        let chosen =
            RandomAgent.choose_action(&session, PlayerId(0), SearchBudget::default(), &mut rng);
        let legal = session.legal_actions(PlayerId(0));
        assert!(chosen.is_some());
        assert!(legal.contains(&chosen.unwrap()));
        assert_eq!(
            RandomAgent.choose_action(&session, PlayerId(1), SearchBudget::default(), &mut rng),
            None
        );
    }

    #[test]
    fn sample_determinization_and_observe_session_prevent_hidden_leakage() {
        let mut session = make_synth_session(101);
        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        let obs0 = observe_session(&session, PlayerId(0));
        assert_eq!(obs0.viewer, PlayerId(0));
        assert_eq!(obs0.players.len(), 2);
        assert_eq!(obs0.legal_actions, session.legal_actions(PlayerId(0)));
        for ent in &obs0.entities {
            assert_ne!(ent.zone, Zone::Library);
            if ent.zone == Zone::Hand {
                assert_eq!(ent.owner_rel, 0);
            }
        }

        let p0_hand_orig: Vec<CardDefId> = session.state().players()[0]
            .hand
            .iter()
            .map(|&id| session.state().object(id).unwrap().card_def)
            .collect();
        let mut saw_diff = false;
        let eval = HeuristicEvaluator;
        for seed in 1..=8u64 {
            let mut rng = RngState::splitmix64(seed);
            let det = sample_determinization(&session, PlayerId(0), &mut rng);
            let p0_hand_det: Vec<CardDefId> = det.players()[0]
                .hand
                .iter()
                .map(|&id| det.object(id).unwrap().card_def)
                .collect();
            assert_eq!(p0_hand_det, p0_hand_orig);
            if det.players()[1].hand != session.state().players()[1].hand
                || det.players()[1]
                    .library
                    .iter()
                    .map(|&id| det.object(id).unwrap().card_def)
                    .collect::<Vec<_>>()
                    != session.state().players()[1]
                        .library
                        .iter()
                        .map(|&id| session.state().object(id).unwrap().card_def)
                        .collect::<Vec<_>>()
            {
                saw_diff = true;
            }
            let scores = eval.evaluate(&det);
            assert!((scores.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        }
        assert!(
            saw_diff,
            "determinizations should vary opponent hidden cards"
        );
    }
}
