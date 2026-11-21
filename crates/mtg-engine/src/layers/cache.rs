use crate::layers::compute_layer_cache_inner;
use crate::state::{GameState, LayerCache};

/// Computes a fresh [`LayerCache`] across all live battlefield and off-battlefield objects
/// without touching `state.layer_cache`.
pub fn compute_layer_cache(state: &GameState) -> LayerCache {
    compute_layer_cache_inner(state, false).0
}

/// Cross-checks a cached [`LayerCache`] against a fresh uncached recomputation (`--features verify-layer-cache`).
#[cfg(feature = "verify-layer-cache")]
pub fn verify_layer_cache(state: &GameState, cached: &LayerCache) {
    let fresh = compute_layer_cache(state);
    assert_eq!(
        cached.rev,
        fresh.rev,
        "verify-layer-cache: rev mismatch (cached={}, fresh={}, dirty_since_eval={:?})",
        cached.rev,
        fresh.rev,
        state.dirty_since_eval()
    );
    assert_eq!(
        cached.reads_mask, fresh.reads_mask,
        "verify-layer-cache: reads_mask mismatch (cached={:?}, fresh={:?})",
        cached.reads_mask, fresh.reads_mask
    );
    assert_eq!(
        cached.all_off_battlefield_colorless, fresh.all_off_battlefield_colorless,
        "verify-layer-cache: all_off_battlefield_colorless mismatch"
    );
    assert_eq!(
        cached.may_have_off_bf_char_grants, fresh.may_have_off_bf_char_grants,
        "verify-layer-cache: may_have_off_bf_char_grants mismatch"
    );
    assert_eq!(
        cached.off_bf_grantor_ids, fresh.off_bf_grantor_ids,
        "verify-layer-cache: off_bf_grantor_ids mismatch"
    );
    assert_eq!(
        cached.players,
        fresh.players,
        "verify-layer-cache: players mismatch (dirty_since_eval={:?})",
        state.dirty_since_eval()
    );
    assert_eq!(
        cached.objects.len(),
        fresh.objects.len(),
        "verify-layer-cache: object slot len mismatch"
    );
    for (idx, (c_slot, f_slot)) in cached.objects.iter().zip(fresh.objects.iter()).enumerate() {
        assert_eq!(
            c_slot,
            f_slot,
            "verify-layer-cache: EvaluatedObject mismatch at slot {idx} (dirty_since_eval={:?}, reads_mask={:?})",
            state.dirty_since_eval(),
            cached.reads_mask
        );
    }
    assert_eq!(
        cached.extras,
        fresh.extras,
        "verify-layer-cache: extras mismatch (dirty_since_eval={:?})",
        state.dirty_since_eval()
    );
    assert_eq!(
        cached.static_presence,
        fresh.static_presence,
        "verify-layer-cache: static_presence mismatch (cached={:?}, fresh={:?}, dirty_since_eval={:?})",
        cached.static_presence,
        fresh.static_presence,
        state.dirty_since_eval()
    );
}
