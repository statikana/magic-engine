//! Opaque-box invariant assertions: redaction, observation seat rotation, determinism,
//! and rename invariance.

use std::sync::Arc;

use mtg_engine::{
    Action, DecisionState, DeckSpec, GameConfig, MatchSession, PlayerId, StepOutcome,
};
use mtg_ir::{CardDatabase, IrV2Bundle, Zone};

/// Verifies `view_model(Some(seat))` redacts opponent hands and `view_model(None)` redacts all
/// players' hands while preserving accurate `hand_count`s.
pub fn assert_view_model_redaction(session: &MatchSession) {
    let spec_vm = session.view_model(None);
    let player_count = spec_vm.players.len();
    for pv in &spec_vm.players {
        assert!(
            pv.hand.is_empty(),
            "spectator view leaked hand cards for {:?}",
            pv.id
        );
    }
    assert!(
        spec_vm.legal_actions.is_empty(),
        "spectator view should have empty legal_actions"
    );

    for seat_idx in 0..player_count {
        let seat = PlayerId(seat_idx as u8);
        let vm = session.view_model(Some(seat));
        assert_eq!(vm.players.len(), player_count);
        for (i, pv) in vm.players.iter().enumerate() {
            assert_eq!(pv.hand_count, spec_vm.players[i].hand_count);
            if pv.id == seat {
                assert_eq!(
                    pv.hand.len(),
                    usize::from(pv.hand_count),
                    "viewer {:?} hand length must match hand_count",
                    seat
                );
            } else {
                assert!(
                    pv.hand.is_empty(),
                    "viewer {:?} saw hidden hand of opponent {:?}",
                    seat,
                    pv.id
                );
            }
        }
    }
}

/// Verifies `observe(viewer)` aligns `relative_seat == 0` with `viewer`, rotates subsequent seats,
/// and matches `view_model(Some(viewer))` and `legal_actions(viewer)`.
pub fn assert_observation_consistency(session: &MatchSession, viewer: PlayerId) {
    let vm = session.view_model(Some(viewer));
    let obs = session.observe(viewer);
    let n = vm.players.len();
    assert_eq!(obs.viewer, viewer);
    assert_eq!(obs.turn_number, vm.turn_number);
    assert_eq!(obs.phase, vm.phase);
    assert_eq!(obs.players.len(), n);
    assert_eq!(obs.legal_actions, session.legal_actions(viewer));

    for offset in 0..n {
        let abs_idx = (usize::from(viewer.0) + offset) % n;
        let pv = &vm.players[abs_idx];
        let po = &obs.players[offset];
        assert_eq!(po.relative_seat, offset as u8);
        assert_eq!(po.life, pv.life);
        assert_eq!(po.lost, pv.lost);
        assert_eq!(po.poison, pv.poison);
        assert_eq!(po.energy, pv.energy);
        assert_eq!(po.hand_count, pv.hand_count);
        assert_eq!(po.library_count, pv.library_count);
        assert_eq!(po.graveyard_count as usize, pv.graveyard.len());
    }

    // Ensure unrevealed opponent hands / library cards never appear in observation entities.
    for ent in &obs.entities {
        if ent.zone == Zone::Hand {
            assert_eq!(
                ent.owner_rel, 0,
                "unrevealed opponent hand entity leaked into Observation::entities"
            );
        }
        assert_ne!(
            ent.zone,
            Zone::Library,
            "hidden library entity leaked into Observation::entities"
        );
    }
}

/// Runs `script` twice from identical inputs and asserts identical outcomes, views, and logs.
pub fn assert_determinism_identical(
    db: &Arc<CardDatabase>,
    config: &GameConfig,
    decks: &[DeckSpec],
    seed: u64,
    script: &[(PlayerId, Action)],
) {
    let (outcomes_a, vm_a, log_a) = run_script(db, config, decks, seed, script);
    let (outcomes_b, vm_b, log_b) = run_script(db, config, decks, seed, script);

    assert_eq!(
        outcomes_a, outcomes_b,
        "StepOutcomes diverged across identical runs"
    );
    assert_eq!(
        vm_a, vm_b,
        "final ClientViewModels diverged across identical runs"
    );
    assert_eq!(log_a, log_b, "ReplayLogs diverged across identical runs");
}

/// Runs `script` on `bundle_a` vs a renamed clone `bundle_b` and asserts identical game state,
/// life totals, zone counts, outcomes, and decision states.
pub fn assert_rename_invariance(
    bundle_a: IrV2Bundle,
    bundle_b: IrV2Bundle,
    config: &GameConfig,
    decks_a: &[DeckSpec],
    decks_b: &[DeckSpec],
    seed: u64,
    script: &[(PlayerId, Action)],
) {
    let db_a = Arc::new(CardDatabase::from_bundle(bundle_a).expect("bundle_a must build"));
    let db_b = Arc::new(CardDatabase::from_bundle(bundle_b).expect("bundle_b must build"));

    let (outcomes_a, vm_a, log_a) = run_script(&db_a, config, decks_a, seed, script);
    let (outcomes_b, vm_b, log_b) = run_script(&db_b, config, decks_b, seed, script);

    assert_eq!(
        outcomes_a, outcomes_b,
        "outcomes diverged under card rename"
    );
    assert_eq!(log_a, log_b, "replay logs diverged under card rename");
    assert_eq!(vm_a, vm_b, "ClientViewModel diverged under card rename");
}

/// Drives identical deterministic actions on `bundle_a` vs a renamed clone `bundle_b`
/// (allowing dynamic `ObjectId` lookups via `SessionDriver`) and asserts identical
/// `ReplayLog`s, `ClientViewModel`s, and `Observation`s.
pub fn assert_rename_invariance_with<F>(
    bundle_a: IrV2Bundle,
    bundle_b: IrV2Bundle,
    config: &GameConfig,
    decks_a: &[DeckSpec],
    decks_b: &[DeckSpec],
    seed: u64,
    mut drive: F,
) where
    F: FnMut(&mut crate::driver::SessionDriver),
{
    let db_a = Arc::new(CardDatabase::from_bundle(bundle_a).expect("bundle_a must build"));
    let db_b = Arc::new(CardDatabase::from_bundle(bundle_b).expect("bundle_b must build"));

    let mut drv_a = crate::driver::SessionDriver::new(db_a, config.clone(), decks_a.to_vec(), seed)
        .expect("session_a init");
    let mut drv_b = crate::driver::SessionDriver::new(db_b, config.clone(), decks_b.to_vec(), seed)
        .expect("session_b init");

    drive(&mut drv_a);
    drive(&mut drv_b);

    assert_eq!(
        drv_a.session.replay_log(),
        drv_b.session.replay_log(),
        "replay logs diverged under card rename"
    );
    assert_eq!(
        drv_a.session.view_model(None),
        drv_b.session.view_model(None),
        "ClientViewModel diverged under card rename"
    );
    let n = drv_a.session.view_model(None).players.len();
    for seat in 0..n {
        let pid = PlayerId(seat as u8);
        assert_eq!(
            drv_a.session.observe(pid),
            drv_b.session.observe(pid),
            "Observation for {pid:?} diverged under card rename"
        );
    }
}

fn run_script(
    db: &Arc<CardDatabase>,
    config: &GameConfig,
    decks: &[DeckSpec],
    seed: u64,
    script: &[(PlayerId, Action)],
) -> (
    Vec<StepOutcome>,
    mtg_engine::ClientViewModel,
    mtg_engine::ReplayLog,
) {
    let mut session = MatchSession::new(Arc::clone(db), config.clone(), decks.to_vec(), seed)
        .expect("session init");
    let mut outcomes = Vec::with_capacity(script.len());
    for (actor, act) in script {
        let out = session
            .apply_action(*actor, act.clone())
            .expect("script action should succeed");
        outcomes.push(out);
    }
    let vm = session.view_model(None);
    let log = session.replay_log().clone();
    (outcomes, vm, log)
}

/// Asserts that `GameState` is not in a terminal state without empty `legal_actions`.
pub fn assert_active_has_legal_actions(session: &MatchSession) {
    if !matches!(
        session.view_model(None).decision,
        DecisionState::GameOver { .. }
    ) {
        let actor = session
            .current_actor()
            .expect("active non-gameover match must have a current_actor");
        assert!(
            !session.legal_actions(actor).is_empty(),
            "active actor {actor:?} had zero legal actions"
        );
    }
}
