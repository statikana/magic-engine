use std::sync::Arc;

use mtg_ir::{AbilityDef, CardDatabase, CardDefId, CardTypeMask, KeywordDef, Zone};
use smallvec::SmallVec;

use crate::action::ChoiceRequest;
use crate::config::RngMode;
use crate::events::AtomicEventKind;
use crate::id::{ObjectId, PlayerId};
use crate::observe::{
    is_hand_revealed_to_cached, is_object_visible_to_cached, is_top_of_library_visible_to_cached,
};
use crate::rng::RngState;
use crate::state::{
    CastingPromptKind, CostPromptKind, DirtyMask, FaceDownKind, FaceDownSpec, GameObject,
    GameState, LayerCache, LkiSnapshot, PromptOwner, StackEntryKind, StepPauseState,
    TOKEN_CARD_DEF_ID,
};
use crate::zones::is_known_to;

#[derive(Debug, Clone, Copy)]
struct PoolSlot {
    id: ObjectId,
    zone: Zone,
}

fn find_card_keyword<'a>(
    db: &'a CardDatabase,
    card_def: CardDefId,
    kw_name: &str,
) -> Option<&'a KeywordDef> {
    let card = db.get(card_def)?;
    let face = card.faces.first()?;
    face.abilities.iter().find_map(|ab| match ab {
        AbilityDef::Keyword(k)
            if db
                .keyword_name(k.keyword.keyword)
                .is_some_and(|n| n.eq_ignore_ascii_case(kw_name)) =>
        {
            Some(&k.keyword)
        }
        _ => None,
    })
}

fn recompute_face_down_turn_up_costs(
    db: &CardDatabase,
    card_def: CardDefId,
    spec: &mut FaceDownSpec,
) {
    match spec.kind {
        FaceDownKind::Manifest | FaceDownKind::Cloak => {
            spec.turn_up_cost_defs.clear();
            spec.turn_up_mana_cost = db
                .get(card_def)
                .and_then(|c| c.faces.first())
                .filter(|f| f.characteristics.types.intersects(CardTypeMask::PERMANENT))
                .map(|f| f.characteristics.mana_cost.clone());
        }
        FaceDownKind::Morph => {
            let kw = find_card_keyword(db, card_def, "morph");
            spec.turn_up_mana_cost = kw.and_then(|k| k.cost.clone());
            spec.turn_up_cost_defs = kw.map(|k| k.cost_defs.clone()).unwrap_or_default();
        }
        FaceDownKind::Megamorph => {
            let kw = find_card_keyword(db, card_def, "megamorph");
            spec.turn_up_mana_cost = kw.and_then(|k| k.cost.clone());
            spec.turn_up_cost_defs = kw.map(|k| k.cost_defs.clone()).unwrap_or_default();
        }
        FaceDownKind::Disguise => {
            let kw = find_card_keyword(db, card_def, "disguise");
            spec.turn_up_mana_cost = kw.and_then(|k| k.cost.clone());
            spec.turn_up_cost_defs = kw.map(|k| k.cost_defs.clone()).unwrap_or_default();
        }
        FaceDownKind::Foretell => {
            let kw = find_card_keyword(db, card_def, "foretell");
            spec.turn_up_mana_cost = kw.and_then(|k| k.cost.clone());
            spec.turn_up_cost_defs = kw.map(|k| k.cost_defs.clone()).unwrap_or_default();
        }
        FaceDownKind::ExiledFaceDown | FaceDownKind::Cyberman | FaceDownKind::Other => {
            spec.turn_up_mana_cost = None;
            spec.turn_up_cost_defs.clear();
        }
    }
}

fn collect_viewer_prompt_pins(state: &GameState, viewer: PlayerId) -> Vec<ObjectId> {
    let mut pins = Vec::new();

    if let Some(pp) = state.pending_prompt()
        && pp.chooser == viewer
    {
        match &pp.request {
            ChoiceRequest::ChooseObjects { candidates, .. } => {
                pins.extend_from_slice(candidates);
            }
            ChoiceRequest::OrderObjects { objects, .. } => {
                pins.extend_from_slice(objects);
            }
            _ => {}
        }
        match &pp.owner {
            PromptOwner::CostPayment(cp) => {
                if let CostPromptKind::ObjectSelection {
                    candidates, picked, ..
                } = &cp.kind
                {
                    pins.extend_from_slice(candidates);
                    pins.extend_from_slice(picked);
                }
            }
            PromptOwner::Casting(CastingPromptKind::CostObjectSelection {
                candidates,
                picked,
                ..
            }) => {
                pins.extend_from_slice(candidates);
                pins.extend_from_slice(picked);
            }
            _ => {}
        }
    }

    if let Some(cs) = state.casting()
        && cs.caster == viewer
    {
        pins.push(cs.object);
        if let Some(plan) = &cs.payment_plan {
            pins.extend_from_slice(&plan.revealed_cards);
            pins.extend_from_slice(&plan.discarded_cards);
            pins.extend_from_slice(&plan.delve_exiled);
            pins.extend_from_slice(&plan.convoke_tapped);
            pins.extend_from_slice(&plan.improvise_tapped);
            pins.extend_from_slice(&plan.waterbend_tapped);
            if let Some(em) = plan.emerge_sacrificed {
                pins.push(em);
            }
        }
    }

    for vm in state.resolution_vms() {
        let Some(pause) = &vm.pause_state else {
            continue;
        };
        match pause {
            StepPauseState::SelectionPick {
                chooser,
                candidates,
                selected,
                ..
            } if *chooser == viewer => {
                pins.extend_from_slice(candidates);
                pins.extend_from_slice(selected);
            }
            StepPauseState::SearchPick {
                chooser,
                candidates,
                selected,
                moved_ids,
                ..
            } if *chooser == viewer => {
                pins.extend_from_slice(candidates);
                pins.extend_from_slice(selected);
                pins.extend_from_slice(moved_ids);
            }
            StepPauseState::ReorderLibrary {
                player,
                remaining,
                top_order,
                bottom_or_gy,
                deciding_card,
                ..
            } if *player == viewer => {
                pins.extend_from_slice(remaining);
                pins.extend_from_slice(top_order);
                pins.extend_from_slice(bottom_or_gy);
                if let Some(dc) = deciding_card {
                    pins.push(*dc);
                }
            }
            StepPauseState::DigPick {
                player,
                revealed,
                candidates,
                selected,
                partition_picks,
                ..
            } if *player == viewer => {
                pins.extend_from_slice(revealed);
                pins.extend_from_slice(candidates);
                pins.extend_from_slice(selected);
                pins.extend_from_slice(partition_picks);
            }
            StepPauseState::MoveCountersSelectTarget {
                chooser,
                from_id,
                candidates,
                transfers,
                ..
            } if *chooser == viewer => {
                pins.push(*from_id);
                pins.extend_from_slice(candidates);
                for &(tid, _) in transfers {
                    pins.push(tid);
                }
            }
            StepPauseState::MoveCountersChooseAmount {
                chooser,
                from_id,
                candidates,
                target_id,
                transfers,
                ..
            } if *chooser == viewer => {
                pins.push(*from_id);
                pins.push(*target_id);
                pins.extend_from_slice(candidates);
                for &(tid, _) in transfers {
                    pins.push(tid);
                }
            }
            StepPauseState::WaitingOnReplacement {
                batch_remaining,
                batch_idx,
                op_tag,
            } if *op_tag == crate::effects::vm::OP_TAG_SEARCH => {
                if let Some(cont) = vm.pending_continuation.as_deref()
                    && cont.search_chooser == Some(viewer)
                {
                    if let Some(rem) = batch_remaining.get(*batch_idx..) {
                        pins.extend_from_slice(rem);
                    }
                    pins.extend_from_slice(&cont.search_candidates);
                    pins.extend_from_slice(&cont.search_moved_ids);
                }
            }
            StepPauseState::WaitingOnReplacement {
                batch_remaining,
                batch_idx,
                op_tag,
            } if *op_tag == crate::effects::vm::OP_TAG_DIG => {
                if let Some(cont) = vm.pending_continuation.as_deref()
                    && cont.dig_player == Some(viewer)
                    && let Some(rem) = batch_remaining.get(*batch_idx..)
                {
                    pins.extend_from_slice(rem);
                }
            }
            _ => {}
        }
    }

    pins
}

#[inline]
fn is_swappable_card(
    state: &GameState,
    id: ObjectId,
    viewer: PlayerId,
    viewer_pins: &[ObjectId],
) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.is_token() || obj.card_def == TOKEN_CARD_DEF_ID {
        return false;
    }
    if state.rare(id).is_some_and(|r| {
        r.copiable.is_some() || r.melded_components.is_some() || !r.merged_components.is_empty()
    }) {
        return false;
    }
    if is_known_to(state, id, viewer) || viewer_pins.contains(&id) {
        return false;
    }
    true
}

fn shuffle_slot_pool(
    cloned: &mut GameState,
    slots: &[PoolSlot],
    rng: &mut RngState,
    touched_bf_or_stack: &mut bool,
) {
    if slots.len() <= 1 {
        return;
    }
    let mut defs: Vec<CardDefId> = slots
        .iter()
        .filter_map(|s| cloned.object(s.id).map(|o| o.card_def))
        .collect();
    if defs.len() != slots.len() || defs.len() <= 1 {
        return;
    }
    rng.shuffle(&mut defs);

    for (idx, slot) in slots.iter().enumerate() {
        let new_def = defs[idx];
        let is_face_down = if let Some(obj) = cloned.object_mut(slot.id) {
            obj.card_def = new_def;
            if matches!(slot.zone, Zone::Hand | Zone::Library | Zone::Exile) || obj.is_face_down() {
                obj.active_face_index = 0;
            }
            if matches!(slot.zone, Zone::Battlefield | Zone::Stack) {
                *touched_bf_or_stack = true;
            }
            obj.is_face_down()
        } else {
            false
        };
        if is_face_down {
            let db = cloned.db().clone();
            if let Some(rare) = cloned.rare_mut(slot.id)
                && let Some(fd_arc) = rare.face_down.as_mut()
            {
                recompute_face_down_turn_up_costs(&db, new_def, Arc::make_mut(fd_arc));
            }
        }
    }
}

#[inline]
fn redact_lki(snap: &mut LkiSnapshot) {
    snap.card_def = TOKEN_CARD_DEF_ID;
    snap.name.clear();
    snap.chosen_card_name = None;
    snap.active_face_index = 0;
}

#[inline]
fn should_redact_lki(
    orig: &GameState,
    snap: &LkiSnapshot,
    viewer: PlayerId,
    cache: &LayerCache,
) -> bool {
    if snap.face_down {
        return !is_known_to(orig, snap.object_id, viewer);
    }
    match snap.last_zone {
        Zone::Hand => {
            snap.owner != viewer
                && !is_known_to(orig, snap.object_id, viewer)
                && !is_object_visible_to_cached(orig, snap.object_id, Some(viewer), cache)
        }
        Zone::Library => {
            !is_known_to(orig, snap.object_id, viewer)
                && !is_object_visible_to_cached(orig, snap.object_id, Some(viewer), cache)
        }
        _ => false,
    }
}

fn scrub_bindings(
    orig: &GameState,
    bindings: &mut [crate::state::ResolutionBinding],
    viewer: PlayerId,
    cache: &LayerCache,
) {
    for b in bindings {
        for lki_arc in &mut b.lki {
            if should_redact_lki(orig, lki_arc, viewer, cache) {
                redact_lki(Arc::make_mut(lki_arc));
            }
        }
    }
}

/// Returns a cloned [`GameState`] with all information hidden from `viewer`
/// resampled consistently and all hidden-info side tables / RNG scrubbed.
pub fn determinize_state(state: &GameState, viewer: PlayerId, rng: &mut RngState) -> GameState {
    let cache = state.evaluate_board();
    let n = state.players().len();
    let mut hand_revealed: SmallVec<[bool; 4]> = SmallVec::with_capacity(n);
    let mut top_visible: SmallVec<[bool; 4]> = SmallVec::with_capacity(n);
    for p in state.players() {
        hand_revealed.push(is_hand_revealed_to_cached(state, p.id, Some(viewer), cache));
        top_visible.push(is_top_of_library_visible_to_cached(
            state,
            p.id,
            Some(viewer),
            cache,
        ));
    }
    let viewer_pins = collect_viewer_prompt_pins(state, viewer);

    let mut cloned = state.clone();
    let mut touched_bf_or_stack = false;

    for (seat, p) in state.players().iter().enumerate() {
        let pid = p.id;
        let mut all_slots: Vec<PoolSlot> = Vec::new();

        // 1. Opponent unrevealed hand.
        if pid != viewer && !hand_revealed[seat] {
            for &id in &p.hand {
                if is_swappable_card(state, id, viewer, &viewer_pins) {
                    all_slots.push(PoolSlot {
                        id,
                        zone: Zone::Hand,
                    });
                }
            }
        }

        // 2. Player's unknown library (including viewer's own unknown library; excluding visible top).
        let visible_top = if top_visible[seat] {
            p.library.last().copied()
        } else {
            None
        };
        for &id in &p.library {
            if Some(id) != visible_top && is_swappable_card(state, id, viewer, &viewer_pins) {
                all_slots.push(PoolSlot {
                    id,
                    zone: Zone::Library,
                });
            }
        }

        // 3. Player's face-down unknown exile.
        for &id in &p.exile {
            if state.object(id).is_some_and(GameObject::is_face_down)
                && !is_object_visible_to_cached(state, id, Some(viewer), cache)
                && is_swappable_card(state, id, viewer, &viewer_pins)
            {
                all_slots.push(PoolSlot {
                    id,
                    zone: Zone::Exile,
                });
            }
        }

        // 4. Face-down unknown permanents on any player's battlefield owned by `pid`.
        for bp in state.players() {
            for &id in &bp.battlefield {
                if state
                    .object(id)
                    .is_some_and(|o| o.owner == pid && o.is_face_down())
                    && is_swappable_card(state, id, viewer, &viewer_pins)
                    && !all_slots.iter().any(|s| s.id == id)
                {
                    all_slots.push(PoolSlot {
                        id,
                        zone: Zone::Battlefield,
                    });
                }
            }
        }

        // 5. Face-down unknown spell entries on the stack owned by `pid`.
        for entry in state.stack_entries() {
            if let StackEntryKind::Spell { object } = entry.kind
                && state
                    .object(object)
                    .is_some_and(|o| o.owner == pid && o.is_face_down())
                && is_swappable_card(state, object, viewer, &viewer_pins)
                && !all_slots.iter().any(|s| s.id == object)
            {
                all_slots.push(PoolSlot {
                    id: object,
                    zone: Zone::Stack,
                });
            }
        }
        if state.stack().len() > state.stack_entries().len() {
            for &id in state.stack().iter().skip(state.stack_entries().len()) {
                if state
                    .object(id)
                    .is_some_and(|o| o.owner == pid && o.is_face_down())
                    && is_swappable_card(state, id, viewer, &viewer_pins)
                    && !all_slots.iter().any(|s| s.id == id)
                {
                    all_slots.push(PoolSlot {
                        id,
                        zone: Zone::Stack,
                    });
                }
            }
        }

        if all_slots.len() <= 1 {
            continue;
        }

        // Partition matching keyword sub-pools (Morph / Megamorph / Disguise / Foretell)
        // when >= 2 unknown cards in the pool carry that keyword.
        let mut assigned = vec![false; all_slots.len()];
        const KEYWORD_POOLS: [(FaceDownKind, &str); 4] = [
            (FaceDownKind::Morph, "morph"),
            (FaceDownKind::Megamorph, "megamorph"),
            (FaceDownKind::Disguise, "disguise"),
            (FaceDownKind::Foretell, "foretell"),
        ];

        for (fd_kind, kw_name) in KEYWORD_POOLS {
            let has_matching_fd = all_slots.iter().enumerate().any(|(i, s)| {
                !assigned[i]
                    && state.object(s.id).is_some_and(|o| {
                        o.is_face_down()
                            && state
                                .rare(s.id)
                                .and_then(|r| r.face_down.as_deref())
                                .is_some_and(|fd| fd.kind == fd_kind)
                            && find_card_keyword(state.db(), o.card_def, kw_name).is_some()
                    })
            });
            if !has_matching_fd {
                continue;
            }
            let matching_indices: Vec<usize> = all_slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| {
                    if assigned[i] {
                        return None;
                    }
                    let obj = state.object(s.id)?;
                    find_card_keyword(state.db(), obj.card_def, kw_name).map(|_| i)
                })
                .collect();
            if matching_indices.len() >= 2 {
                let sub_slots: Vec<PoolSlot> =
                    matching_indices.iter().map(|&i| all_slots[i]).collect();
                for &i in &matching_indices {
                    assigned[i] = true;
                }
                shuffle_slot_pool(&mut cloned, &sub_slots, rng, &mut touched_bf_or_stack);
            }
        }

        let general_slots: Vec<PoolSlot> = all_slots
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| (!assigned[i]).then_some(s))
            .collect();
        shuffle_slot_pool(&mut cloned, &general_slots, rng, &mut touched_bf_or_stack);
    }

    // Reseed cloned RNG so future draws/shuffles never leak real-game RNG state.
    let next_seed = rng.next_u64();
    *cloned.rng_mut() = match cloned.config().rng_mode {
        RngMode::Mulberry32Compat => RngState::mulberry32(next_seed as u32),
        RngMode::Fast => RngState::splitmix64(next_seed),
    };

    // Scrub hidden LKI records.
    for (_, snap_arc) in cloned.lki_records_mut() {
        if should_redact_lki(state, snap_arc, viewer, cache) {
            redact_lki(Arc::make_mut(snap_arc));
        }
    }

    // Scrub batch LKI scopes.
    for scope in cloned.batch_lki_scopes_mut() {
        for entry in scope {
            if should_redact_lki(state, &entry.lki, viewer, cache) {
                redact_lki(Arc::make_mut(&mut entry.lki));
            }
        }
    }

    // Scrub hidden ZoneChange / Sacrificed / SpellCountered / Exploited / HauntedCreatureDies LKI in event_log.
    for ev in cloned.event_log_mut() {
        match &mut ev.kind {
            AtomicEventKind::ZoneChange {
                object,
                new_id,
                from,
                to,
                from_player,
                to_player,
                lki: Some(snap_arc),
                ..
            } => {
                let dest_visible = (*to == Zone::Hand && *to_player == Some(viewer))
                    || new_id.is_some_and(|nid| {
                        is_known_to(state, nid, viewer)
                            || (!state.object(nid).is_some_and(GameObject::is_face_down)
                                && is_object_visible_to_cached(state, nid, Some(viewer), cache))
                    });
                let hidden_source = should_redact_lki(state, snap_arc, viewer, cache)
                    || (matches!(from, Some(Zone::Library))
                        && !is_known_to(state, *object, viewer))
                    || (matches!(from, Some(Zone::Hand))
                        && *from_player != Some(viewer)
                        && !is_known_to(state, *object, viewer));
                if hidden_source && !dest_visible {
                    redact_lki(Arc::make_mut(snap_arc));
                }
            }
            AtomicEventKind::Sacrificed { lki, .. } => {
                if should_redact_lki(state, lki, viewer, cache) {
                    redact_lki(Arc::make_mut(lki));
                }
            }
            AtomicEventKind::SpellCountered { countered_lki, .. } => {
                if should_redact_lki(state, countered_lki, viewer, cache) {
                    redact_lki(Arc::make_mut(countered_lki));
                }
            }
            AtomicEventKind::Exploited { exploited_lki, .. } => {
                if should_redact_lki(state, exploited_lki, viewer, cache) {
                    redact_lki(Arc::make_mut(exploited_lki));
                }
            }
            AtomicEventKind::HauntedCreatureDies { haunted_lki, .. }
                if should_redact_lki(state, haunted_lki, viewer, cache) =>
            {
                redact_lki(Arc::make_mut(haunted_lki));
            }
            _ => {}
        }
    }

    // Scrub hidden LkiSnapshots on stack_entries.
    for entry in cloned.stack_entries_mut() {
        match &mut entry.kind {
            StackEntryKind::Activated { lki, .. } | StackEntryKind::Triggered { lki, .. } => {
                if should_redact_lki(state, lki, viewer, cache) {
                    redact_lki(Arc::make_mut(lki));
                }
            }
            StackEntryKind::Spell { .. } => {}
        }
    }

    // Scrub hidden LkiSnapshots on stack_trigger_meta.
    for (_, meta) in cloned.stack_trigger_meta_mut() {
        if let Some(lki) = meta.event_lki.as_mut()
            && should_redact_lki(state, lki, viewer, cache)
        {
            redact_lki(Arc::make_mut(lki));
        }
        scrub_bindings(state, &mut meta.bindings, viewer, cache);
    }

    // Scrub hidden LkiSnapshots on pending_triggers, delayed_triggers, pending_reflexive_triggers.
    for trig in cloned.pending_triggers_mut() {
        if should_redact_lki(state, &trig.source_lki, viewer, cache) {
            redact_lki(Arc::make_mut(&mut trig.source_lki));
        }
        if let Some(lki) = trig.event_lki.as_mut()
            && should_redact_lki(state, lki, viewer, cache)
        {
            redact_lki(Arc::make_mut(lki));
        }
    }
    for dt in cloned.delayed_triggers_mut() {
        let hide_src = dt.source_object.is_some_and(|oid| {
            state.object(oid).is_some_and(GameObject::is_face_down)
                && !is_known_to(state, oid, viewer)
        }) || dt
            .source_lki
            .as_ref()
            .is_some_and(|lki| should_redact_lki(state, lki, viewer, cache));
        if hide_src {
            dt.source_card = TOKEN_CARD_DEF_ID;
            if let Some(lki) = dt.source_lki.as_mut() {
                redact_lki(Arc::make_mut(lki));
            }
        }
    }
    for rt in cloned.pending_reflexive_triggers_mut() {
        if should_redact_lki(state, &rt.source_lki, viewer, cache) {
            redact_lki(Arc::make_mut(&mut rt.source_lki));
        }
        if let Some(lki) = rt.event_lki.as_mut()
            && should_redact_lki(state, lki, viewer, cache)
        {
            redact_lki(Arc::make_mut(lki));
        }
        scrub_bindings(state, &mut rt.bindings, viewer, cache);
    }

    // Scrub resolution_frames and resolution_vms.
    for frame in cloned.resolution_frames_mut() {
        scrub_bindings(state, &mut frame.bindings, viewer, cache);
    }
    for vm in cloned.resolution_vms_mut() {
        if let Some(lki) = vm.source_lki.as_mut()
            && should_redact_lki(state, lki, viewer, cache)
        {
            redact_lki(Arc::make_mut(lki));
        }
        if let Some(lki) = vm.event_lki.as_mut()
            && should_redact_lki(state, lki, viewer, cache)
        {
            redact_lki(Arc::make_mut(lki));
        }
    }

    // Scrub opponent pre_cast_exile secret FaceDownSpec costs and any hidden sacrificed_lki.
    if let Some(cs) = cloned.casting_mut() {
        if cs.caster != viewer
            && let Some(ex) = cs.pre_cast_exile.as_mut()
            && ex.was_face_down
            && !ex.known_to_players.contains(&viewer)
            && let Some(fd_arc) = ex.face_down.as_mut()
        {
            let fd = Arc::make_mut(fd_arc);
            fd.turn_up_mana_cost = None;
            fd.turn_up_cost_defs.clear();
        }
        if let Some(plan) = cs.payment_plan.as_mut() {
            for lki in &mut plan.sacrificed_lki {
                if should_redact_lki(state, lki, viewer, cache) {
                    redact_lki(Arc::make_mut(lki));
                }
            }
        }
    }

    if touched_bf_or_stack {
        cloned.mark_dirty(DirtyMask::TOPOLOGY);
    }

    cloned
}

impl GameState {
    /// Returns a cloned [`GameState`] with all information hidden from `viewer`
    /// resampled consistently and all hidden-info side tables / RNG scrubbed.
    pub fn determinize(&self, viewer: PlayerId, rng: &mut RngState) -> GameState {
        determinize_state(self, viewer, rng)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mtg_ir::{
        AbilityDef, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics,
        ColorMask, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility, KeywordDef,
        KeywordId, ManaCost, PrintedPt, SupertypeMask, Zone,
    };
    use smallvec::SmallVec;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::events::AtomicEvent;
    use crate::state::{LkiSnapshot, PreCastExileState};
    use crate::view::PowerToughness;
    use crate::zones::{MoveRawOptions, exile_face_down, move_object_raw, reveal_to_player};

    fn make_morph_test_db() -> Arc<CardDatabase> {
        let mut tables = InternTables::default();
        tables.keywords.push("morph".into());
        let morph_kw_id = KeywordId(0);

        let make_card = |idx: u32, name: &str, cost: &str, morph_cost: Option<&str>| {
            let mut abilities = Vec::new();
            if let Some(mc) = morph_cost {
                abilities.push(AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: morph_kw_id,
                        cost: Some(ManaCost::parse(mc)),
                        param: None,
                        n: None,
                        cost_defs: vec![],
                        kicker_costs: vec![],
                        equip_filter: None,
                        enchant_filter: None,
                        enchant_player: None,
                        affinity_filter: None,
                        cycling_filter: None,
                        cycling_desc: None,
                        prototype_spec: None,
                        awaken_n: None,
                        emerge_filter: None,
                        champion_filter: None,
                        devour_filter: None,
                        devour_x: false,
                        equip_allow_planeswalker: false,
                        backup_ability_start_index: None,
                        gift_kind: None,
                        craft_materials: vec![],
                        craft_any_number: false,
                        craft_share_card_type: false,
                        splice_filter: None,
                        impending_n: None,
                        landwalk_filter: None,
                        suspend_x: false,
                        equip_cost_reduction: None,
                        dynamic_amount: None,
                    },
                    id: None,
                    span: Default::default(),
                }));
            }
            CardDef {
                oracle_id: format!("synth_{idx}"),
                compiler_version: 46,
                text_hash: format!("hash_{idx}"),
                name: name.into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: name.into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse(cost),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::G,
                        color_indicator: None,
                        pt: Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        }),
                        loyalty: None,
                        defense: None,
                    },
                    abilities,
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            }
        };

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    make_card(0, "Synth Bear", "{1}{G}", None),
                    make_card(1, "Synth Giant", "{4}{G}", None),
                    make_card(2, "Synth Morph Alpha", "{2}{G}", Some("{1}{G}")),
                    make_card(3, "Synth Morph Beta", "{3}{G}", Some("{2}{G}{G}")),
                ],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn determinize_shuffles_viewer_library_and_opponent_hidden_zones_preserving_multiset() {
        let db = make_morph_test_db();
        let mut p0_cards = vec![CardDefId(0); 8];
        p0_cards.extend(vec![CardDefId(1); 8]);
        let mut p1_cards = vec![CardDefId(0); 10];
        p1_cards.extend(vec![CardDefId(1); 10]);

        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: p0_cards,
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: p1_cards,
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 5;
        let state = GameState::new(db, cfg, &[deck0, deck1], 101).unwrap();

        let p0_hand_orig: Vec<CardDefId> = state.players()[0]
            .hand
            .iter()
            .map(|&id| state.object(id).unwrap().card_def)
            .collect();
        let p0_lib_orig: Vec<CardDefId> = state.players()[0]
            .library
            .iter()
            .map(|&id| state.object(id).unwrap().card_def)
            .collect();
        let p1_lib_orig: Vec<CardDefId> = state.players()[1]
            .library
            .iter()
            .map(|&id| state.object(id).unwrap().card_def)
            .collect();

        let multiset = |s: &GameState, pid: PlayerId| -> (usize, usize) {
            let p = s.player(pid).unwrap();
            let mut c0 = 0usize;
            let mut c1 = 0usize;
            for &id in p.hand.iter().chain(p.library.iter()) {
                match s.object(id).unwrap().card_def {
                    CardDefId(0) => c0 += 1,
                    CardDefId(1) => c1 += 1,
                    _ => {}
                }
            }
            (c0, c1)
        };

        let mut p0_lib_changed = false;
        let mut p1_changed = false;
        for seed in 1..=12u64 {
            let mut rng = RngState::splitmix64(seed);
            let det = state.determinize(PlayerId(0), &mut rng);

            // Viewer's hand is never altered.
            let p0_hand_det: Vec<CardDefId> = det.players()[0]
                .hand
                .iter()
                .map(|&id| det.object(id).unwrap().card_def)
                .collect();
            assert_eq!(p0_hand_det, p0_hand_orig);

            // Exact multiset conservation for both players.
            assert_eq!(multiset(&det, PlayerId(0)), (8, 8));
            assert_eq!(multiset(&det, PlayerId(1)), (10, 10));

            // RNG was scrubbed / reseeded.
            assert_ne!(det.to_snapshot().rng, state.to_snapshot().rng);

            let p0_lib_det: Vec<CardDefId> = det.players()[0]
                .library
                .iter()
                .map(|&id| det.object(id).unwrap().card_def)
                .collect();
            if p0_lib_det != p0_lib_orig {
                p0_lib_changed = true;
            }
            let p1_lib_det: Vec<CardDefId> = det.players()[1]
                .library
                .iter()
                .map(|&id| det.object(id).unwrap().card_def)
                .collect();
            if p1_lib_det != p1_lib_orig {
                p1_changed = true;
            }
        }

        assert!(p0_lib_changed, "viewer unknown library must be resampled");
        assert!(p1_changed, "opponent hidden zones must be resampled");

        // Same seed produces identical determinized state.
        let mut rng_a = RngState::splitmix64(999);
        let mut rng_b = RngState::splitmix64(999);
        let det_a = determinize_state(&state, PlayerId(0), &mut rng_a);
        let det_b = determinize_state(&state, PlayerId(0), &mut rng_b);
        assert_eq!(
            postcard::to_allocvec(&det_a.to_snapshot()).unwrap(),
            postcard::to_allocvec(&det_b.to_snapshot()).unwrap()
        );
    }

    #[test]
    fn determinize_respects_morph_pool_recomputes_face_down_costs_and_scrubs_lki() {
        let db = make_morph_test_db();
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(2),
                CardDefId(3),
                CardDefId(0),
                CardDefId(0),
                CardDefId(1),
                CardDefId(1),
                CardDefId(0),
                CardDefId(1),
                CardDefId(0),
                CardDefId(1),
            ],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 4;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 42).unwrap();

        // Put Morph Alpha (CardDefId(2)) onto P1's battlefield face-down with its real turn_up_mana_cost {1}{G}.
        let morph_alpha_id = state.players()[1]
            .hand
            .iter()
            .chain(state.players()[1].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let bf_morph = move_object_raw(
            &mut state,
            morph_alpha_id,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    turn_up_mana_cost: Some(ManaCost::parse("{1}{G}")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // Put one P1 card manifested face-down on BF with CardDefId(1) ({4}{G}), and reveal P1's hand[0] to P0.
        let p1_revealed = state.players()[1].hand[0];
        assert!(reveal_to_player(&mut state, p1_revealed, PlayerId(0)));
        let p1_revealed_def = state.object(p1_revealed).unwrap().card_def;

        // Exile another P1 card face-down (hidden from P0) and record an LKI + event + stack entry.
        let p1_exile_src = state.players()[1].library[0];
        let p1_exiled = exile_face_down(
            &mut state,
            p1_exile_src,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                turn_up_mana_cost: Some(ManaCost::parse("{9}")),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let secret_lki = Arc::new(LkiSnapshot {
            object_id: bf_morph,
            card_def: CardDefId(2),
            name: "Synth Morph Alpha".into(),
            owner: PlayerId(1),
            controller: PlayerId(1),
            last_zone: Zone::Battlefield,
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 3,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down: true,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        });
        state
            .lki_records_mut()
            .push((bf_morph, Arc::clone(&secret_lki)));
        state.event_log_mut().push(AtomicEvent {
            seq: 1,
            kind: AtomicEventKind::ZoneChange {
                object: p1_exile_src,
                new_id: Some(p1_exiled),
                from: Some(Zone::Library),
                to: Zone::Exile,
                from_player: Some(PlayerId(1)),
                to_player: Some(PlayerId(1)),
                ceased_to_exist: false,
                lki: Some(Arc::clone(&secret_lki)),
            },
        });
        state.set_casting(Some(crate::state::CastingState {
            caster: PlayerId(1),
            object: p1_exiled,
            face_idx: 0,
            from_zone: Zone::Exile,
            step: crate::state::CastStep::PayCosts,
            chosen_modes: SmallVec::new(),
            x_value: None,
            optional_costs: SmallVec::new(),
            optional_cost_cursor: 0,
            targets: vec![],
            divisions: SmallVec::new(),
            locked_cost: ManaCost::default(),
            mana_paid: SmallVec::new(),
            paid_mana_colors: ColorMask::empty(),
            kind: crate::state::CastKind::Spell,
            option_id: 0,
            alt_kind: crate::state::AltCastKind::Normal,
            is_kicked: false,
            kicked_indices: SmallVec::new(),
            kicked_tags: SmallVec::new(),
            fused: false,
            cleaved: false,
            gift_promised: false,
            additional_cost_paid: false,
            bargained: false,
            target_spec_cursor: 0,
            targets_by_spec: vec![],
            division_cursor: 0,
            payment_plan: None,
            paid_mana_sources: SmallVec::new(),
            cant_be_countered_from_mana: false,
            mana_commander_bonus_counters: 0,
            mana_spent_total: 0,
            cast_option: None,
            pre_cast_exile: Some(PreCastExileState {
                foretold: true,
                was_face_down: true,
                is_suspended: false,
                unearth_leave_exile: false,
                face_down: Some(Arc::new(FaceDownSpec {
                    kind: FaceDownKind::Foretell,
                    turn_up_mana_cost: Some(ManaCost::parse("{1}{U}")),
                    ..Default::default()
                })),
                known_to_players: SmallVec::from_slice(&[PlayerId(1)]),
                exiled_by: None,
                playable_from_exile_by: None,
                plotted_by: None,
                plotted_turn: None,
            }),
        }));

        let mut saw_morph_swap = false;
        for seed in 1..=32u64 {
            let mut rng = RngState::splitmix64(seed);
            let det = determinize_state(&state, PlayerId(0), &mut rng);

            // Revealed P1 hand card must remain untouched.
            assert_eq!(det.object(p1_revealed).unwrap().card_def, p1_revealed_def);

            // Morph on BF only swaps within {Morph Alpha (2), Morph Beta (3)}.
            let assigned = det.object(bf_morph).unwrap().card_def;
            assert!(assigned == CardDefId(2) || assigned == CardDefId(3));

            let fd_cost = det
                .rare(bf_morph)
                .and_then(|r| r.face_down.as_deref())
                .and_then(|fd| fd.turn_up_mana_cost.as_ref());
            if assigned == CardDefId(3) {
                saw_morph_swap = true;
                assert_eq!(fd_cost, Some(&ManaCost::parse("{2}{G}{G}")));
            } else {
                assert_eq!(fd_cost, Some(&ManaCost::parse("{1}{G}")));
            }

            // Hidden LKI records, event_log, and opponent pre_cast_exile must be scrubbed.
            let det_lki = det.lki(bf_morph).unwrap();
            assert_eq!(det_lki.card_def, TOKEN_CARD_DEF_ID);
            assert!(det_lki.name.is_empty());

            if let AtomicEventKind::ZoneChange { lki: Some(l), .. } = &det.event_log()[0].kind {
                assert_eq!(l.card_def, TOKEN_CARD_DEF_ID);
                assert!(l.name.is_empty());
            } else {
                panic!("expected ZoneChange event");
            }

            let pre_ex_cost = det
                .casting()
                .and_then(|c| c.pre_cast_exile.as_ref())
                .and_then(|ex| ex.face_down.as_deref())
                .and_then(|fd| fd.turn_up_mana_cost.as_ref());
            assert_eq!(pre_ex_cost, None);
        }
        assert!(
            saw_morph_swap,
            "morph card on battlefield should swap with the other morph card in pool"
        );
    }

    fn make_synth_lki(
        object_id: ObjectId,
        card_def: CardDefId,
        name: &str,
        owner: PlayerId,
        last_zone: Zone,
        face_down: bool,
    ) -> Arc<LkiSnapshot> {
        Arc::new(LkiSnapshot {
            object_id,
            card_def,
            name: name.into(),
            owner,
            controller: owner,
            last_zone,
            pt: Some(PowerToughness {
                power: 2,
                toughness: 2,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 5,
            keywords: SmallVec::new(),
            counters: SmallVec::new(),
            damage_marked: 0,
            is_token: false,
            is_commander: false,
            tapped: false,
            phased_out: false,
            face_down,
            lost_all_abilities: false,
            has_all_creature_types: false,
            attached_ids: SmallVec::new(),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        })
    }

    #[test]
    fn determinize_scrubs_viewer_unknown_library_lki_and_preserves_visible_destinations() {
        let db = make_morph_test_db();
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 12],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 3;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 77).unwrap();

        let p0_lib_0 = state.players()[0].library[0];
        let p0_exiled_fd = exile_face_down(
            &mut state,
            p0_lib_0,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let p0_lib_1 = state.players()[0].library[0];
        let opp_manifest_bf = move_object_raw(
            &mut state,
            p0_lib_1,
            Zone::Battlefield,
            MoveRawOptions {
                controller: Some(PlayerId(1)),
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Manifest,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let p0_lib_2 = state.players()[0].library[0];
        let p0_drawn = move_object_raw(
            &mut state,
            p0_lib_2,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let p0_top = *state.players()[0].library.last().unwrap();
        assert!(reveal_to_player(&mut state, p0_top, PlayerId(0)));

        let lib_lki_exile = make_synth_lki(
            p0_lib_0,
            CardDefId(1),
            "Synth Giant",
            PlayerId(0),
            Zone::Library,
            false,
        );
        let lib_lki_manifest = make_synth_lki(
            p0_lib_1,
            CardDefId(1),
            "Synth Giant",
            PlayerId(0),
            Zone::Library,
            false,
        );
        let lib_lki_draw = make_synth_lki(
            p0_lib_2,
            CardDefId(1),
            "Synth Giant",
            PlayerId(0),
            Zone::Library,
            false,
        );
        let lib_lki_top = make_synth_lki(
            p0_top,
            CardDefId(1),
            "Synth Giant",
            PlayerId(0),
            Zone::Library,
            false,
        );

        state
            .lki_records_mut()
            .push((p0_lib_0, Arc::clone(&lib_lki_exile)));
        state
            .lki_records_mut()
            .push((p0_top, Arc::clone(&lib_lki_top)));

        let stack_id = crate::id::StackItemId(1);
        state.push_stack_trigger_meta(
            stack_id,
            crate::triggers::StackTriggerMeta {
                source: p0_lib_0,
                face_index: 0,
                ability_idx: 0,
                kind: crate::triggers::TriggerSourceKind::Innate {
                    component_index: 0,
                    face_index: 0,
                    ability_idx: 0,
                },
                condition: None,
                targets: vec![],
                effect: vec![],
                modal: None,
                event_object: Some(p0_lib_0),
                event_lki: Some(Arc::clone(&lib_lki_exile)),
                event_player: Some(PlayerId(0)),
                event_amount: None,
                defending_player: None,
                last_tapped_mana_colors: SmallVec::new(),
                is_look_back: false,
                x_value: 0,
                bindings: vec![crate::state::ResolutionBinding {
                    name: "hidden".into(),
                    objects: vec![p0_lib_0],
                    lki: vec![Arc::clone(&lib_lki_exile)],
                }],
            },
        );

        state.event_log_mut().push(AtomicEvent {
            seq: 1,
            kind: AtomicEventKind::ZoneChange {
                object: p0_lib_0,
                new_id: Some(p0_exiled_fd),
                from: Some(Zone::Library),
                to: Zone::Exile,
                from_player: Some(PlayerId(0)),
                to_player: Some(PlayerId(0)),
                ceased_to_exist: false,
                lki: Some(Arc::clone(&lib_lki_exile)),
            },
        });
        state.event_log_mut().push(AtomicEvent {
            seq: 2,
            kind: AtomicEventKind::ZoneChange {
                object: p0_lib_1,
                new_id: Some(opp_manifest_bf),
                from: Some(Zone::Library),
                to: Zone::Battlefield,
                from_player: Some(PlayerId(0)),
                to_player: Some(PlayerId(1)),
                ceased_to_exist: false,
                lki: Some(Arc::clone(&lib_lki_manifest)),
            },
        });
        state.event_log_mut().push(AtomicEvent {
            seq: 3,
            kind: AtomicEventKind::ZoneChange {
                object: p0_lib_2,
                new_id: Some(p0_drawn),
                from: Some(Zone::Library),
                to: Zone::Hand,
                from_player: Some(PlayerId(0)),
                to_player: Some(PlayerId(0)),
                ceased_to_exist: false,
                lki: Some(Arc::clone(&lib_lki_draw)),
            },
        });

        let mut rng = RngState::splitmix64(55);
        let det = determinize_state(&state, PlayerId(0), &mut rng);

        // Unknown viewer Library LKI in lki_records must be redacted.
        let det_hidden = det.lki(p0_lib_0).unwrap();
        assert_eq!(det_hidden.card_def, TOKEN_CARD_DEF_ID);
        assert!(det_hidden.name.is_empty());

        // Revealed top-of-library LKI in lki_records must be preserved.
        let det_top = det.lki(p0_top).unwrap();
        assert_eq!(det_top.card_def, CardDefId(1));
        assert_eq!(det_top.name, "Synth Giant");

        // stack_trigger_meta event_lki and bindings must be redacted.
        let meta = det.stack_trigger_meta(stack_id).unwrap();
        let meta_lki = meta.event_lki.as_ref().unwrap();
        assert_eq!(meta_lki.card_def, TOKEN_CARD_DEF_ID);
        assert!(meta_lki.name.is_empty());
        assert_eq!(meta.bindings[0].lki[0].card_def, TOKEN_CARD_DEF_ID);
        assert!(meta.bindings[0].lki[0].name.is_empty());

        // event_log[0]: Library -> Exile face-down with to_player: Some(viewer) must be redacted.
        if let AtomicEventKind::ZoneChange { lki: Some(l), .. } = &det.event_log()[0].kind {
            assert_eq!(l.card_def, TOKEN_CARD_DEF_ID);
            assert!(l.name.is_empty());
        } else {
            panic!("expected ZoneChange event 0");
        }

        // event_log[1]: Library -> Battlefield face-down controlled by opponent must be redacted.
        if let AtomicEventKind::ZoneChange { lki: Some(l), .. } = &det.event_log()[1].kind {
            assert_eq!(l.card_def, TOKEN_CARD_DEF_ID);
            assert!(l.name.is_empty());
        } else {
            panic!("expected ZoneChange event 1");
        }

        // event_log[2]: Library -> viewer's Hand must be preserved.
        if let AtomicEventKind::ZoneChange { lki: Some(l), .. } = &det.event_log()[2].kind {
            assert_eq!(l.card_def, CardDefId(1));
            assert_eq!(l.name, "Synth Giant");
        } else {
            panic!("expected ZoneChange event 2");
        }
    }

    #[test]
    fn determinize_scrubs_exploited_face_down_lki_even_when_exploiter_is_face_up() {
        let db = make_morph_test_db();
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(2); 10],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 3;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 88).unwrap();

        let exploiter_src = state.players()[1].hand[0];
        let exploiter_bf = move_object_raw(
            &mut state,
            exploiter_src,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let fodder_src = state.players()[1].hand[0];
        let fodder_fd = move_object_raw(
            &mut state,
            fodder_src,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let exploited_lki = make_synth_lki(
            fodder_fd,
            CardDefId(2),
            "Synth Morph Alpha",
            PlayerId(1),
            Zone::Battlefield,
            true,
        );
        state.event_log_mut().push(AtomicEvent {
            seq: 1,
            kind: AtomicEventKind::Exploited {
                player: PlayerId(1),
                exploiter: exploiter_bf,
                exploited_lki,
            },
        });

        let mut rng = RngState::splitmix64(19);
        let det = determinize_state(&state, PlayerId(0), &mut rng);

        if let AtomicEventKind::Exploited { exploited_lki, .. } = &det.event_log()[0].kind {
            assert_eq!(exploited_lki.card_def, TOKEN_CARD_DEF_ID);
            assert!(exploited_lki.name.is_empty());
        } else {
            panic!("expected Exploited event");
        }
    }

    fn make_search_rep_test_db() -> Arc<CardDatabase> {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let make_plain =
            |oracle_id: &str, name: &str, types: CardTypeMask, pt: Option<PrintedPt>| CardDef {
                oracle_id: oracle_id.into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: name.into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: name.into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::G,
                        color_indicator: None,
                        pt,
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
        let make_bf_rep = |oracle_id: &str, name: &str, mod_spec: mtg_ir::ZoneChangeMod| CardDef {
            oracle_id: oracle_id.into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Replacement(mtg_ir::ReplacementAbility {
                    replacement: mtg_ir::ReplacementDef {
                        replaces: mtg_ir::EventPattern {
                            event: mtg_ir::EventKind::ZoneChange,
                            filter: mtg_ir::EventFilter {
                                detail: Some(mtg_ir::EventDetail::ZoneChange {
                                    from: mtg_ir::ZoneMask::LIBRARY,
                                    to: Some(Zone::Battlefield),
                                    from_anywhere: false,
                                    not_from_zone: None,
                                    not_to_zone: None,
                                    enters_untapped: false,
                                    dealt_damage_by_self_this_turn: false,
                                }),
                                ..Default::default()
                            },
                        },
                        with: mtg_ir::ReplacementWith::ZoneChange {
                            mod_spec: Box::new(mod_spec),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let land = make_plain(
            "synth_search_land",
            "Synth Search Land",
            CardTypeMask::LAND,
            None,
        );
        let bear = make_plain(
            "synth_search_bear",
            "Synth Search Bear",
            CardTypeMask::CREATURE,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
        );
        let rep_tap = make_bf_rep(
            "synth_rep_bf_tap",
            "Synth Rep Bf Tap",
            mtg_ir::ZoneChangeMod {
                tapped: Some(true),
                ..Default::default()
            },
        );
        let rep_ctr = make_bf_rep(
            "synth_rep_bf_ctr",
            "Synth Rep Bf Ctr",
            mtg_ir::ZoneChangeMod {
                counters: Some((
                    mtg_ir::CounterId(0),
                    Box::new(mtg_ir::Amount::Const { n: 1 }),
                )),
                ..Default::default()
            },
        );
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land, bear, rep_tap, rep_ctr],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn make_search_step(
        destinations: Vec<mtg_ir::SearchDestination>,
        count: u16,
    ) -> mtg_ir::EffectStep {
        mtg_ir::EffectStep {
            op: mtg_ir::EffectOp::Search {
                spec: Box::new(mtg_ir::SearchSpec {
                    filter: Box::new(mtg_ir::ObjectFilter {
                        types_any: CardTypeMask::LAND,
                        ..Default::default()
                    }),
                    count,
                    up_to: true,
                    stated_quality: true,
                    destinations,
                    reveal: false,
                    description: None,
                    zones: mtg_ir::ZoneMask::LIBRARY,
                    different_names: false,
                    shuffle_only_if_library: false,
                    searched_player: None,
                    cast_free: false,
                    target_controller_as_actor: false,
                }),
                tapped: false,
            },
            subject: mtg_ir::Selector::None,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        }
    }

    #[test]
    fn synth_determinize_pins_multi_dest_search_during_waiting_on_replacement_and_redacts_opponent()
    {
        let db = make_search_rep_test_db();
        let mut mainboard = vec![CardDefId(1); 10];
        mainboard.extend([CardDefId(0), CardDefId(0), CardDefId(0)]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 0;
        let mut state = GameState::new(db, cfg, &[deck.clone(), deck], 17).unwrap();

        for cd in [CardDefId(2), CardDefId(3)] {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    cd,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let step = make_search_step(
            vec![
                mtg_ir::SearchDestination {
                    zone: Zone::Battlefield,
                    tapped: false,
                    on_top: false,
                },
                mtg_ir::SearchDestination {
                    zone: Zone::Hand,
                    tapped: false,
                    on_top: false,
                },
            ],
            2,
        );

        let out1 = crate::effects::begin_resolution_frame(
            &mut state,
            crate::id::StackItemId(701),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![step],
            vec![],
        )
        .unwrap();
        let crate::effects::FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected SearchPick prompt, got {out1:?}");
        };
        let picked_land = match &state.pending_prompt().unwrap().request {
            ChoiceRequest::ChooseObjects { candidates, .. } => {
                assert_eq!(candidates.len(), 3);
                candidates[0]
            }
            other => panic!("expected ChooseObjects, got {other:?}"),
        };

        let out2 = crate::effects::resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            crate::action::ChoiceAnswer::PickObject { id: picked_land },
        )
        .unwrap();
        let crate::effects::FrameOutcome::Paused {
            prompt_id: rep_pid, ..
        } = out2
        else {
            panic!("expected WaitingOnReplacement pause, got {out2:?}");
        };

        let rem_candidates = state
            .resolution_vms()
            .last()
            .and_then(|v| v.pending_continuation.as_ref())
            .expect("pending_continuation must exist")
            .search_candidates
            .clone();
        assert_eq!(rem_candidates.len(), 2);
        let rem_land = rem_candidates[0];
        assert!(!is_known_to(&state, rem_land, PlayerId(0)));

        // Chooser visibility + opponent redaction during WaitingOnReplacement:
        assert!(crate::observe::is_object_visible_to(
            &state,
            rem_land,
            Some(PlayerId(0))
        ));
        assert!(crate::observe::is_object_visible_to(
            &state,
            picked_land,
            Some(PlayerId(0))
        ));
        assert!(!crate::observe::is_object_visible_to(
            &state,
            rem_land,
            Some(PlayerId(1))
        ));
        assert!(!crate::observe::is_object_visible_to(
            &state,
            picked_land,
            Some(PlayerId(1))
        ));
        let obs_opp = crate::observe::observe_state(&state, PlayerId(1), vec![]);
        assert!(
            !obs_opp
                .entities
                .iter()
                .any(|e| e.zone == Zone::Library || e.zone == Zone::Hand)
        );

        // Chooser determinization pins picked_land and all rem_candidates across 16 seeds:
        for s in 1..=16u64 {
            let mut rng = RngState::splitmix64(s);
            let mut det = determinize_state(&state, PlayerId(0), &mut rng);
            assert_eq!(det.object(picked_land).unwrap().card_def, CardDefId(0));
            for &cid in &rem_candidates {
                assert_eq!(
                    det.object(cid).unwrap().card_def,
                    CardDefId(0),
                    "rem_candidate must stay Land in chooser determinization"
                );
            }
            if s == 1 {
                let rep_out = crate::replace::resume_and_finalize_replacement(
                    &mut det,
                    PlayerId(0),
                    rep_pid,
                    crate::action::ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap();
                let res2 = crate::effects::resume_after_replacement(&mut det, rep_out).unwrap();
                let crate::effects::FrameOutcome::Paused { prompt_id: p2, .. } = res2 else {
                    panic!("expected second SearchPick prompt, got {res2:?}");
                };
                let cands2 = match &det.pending_prompt().unwrap().request {
                    ChoiceRequest::ChooseObjects { candidates, .. } => candidates.clone(),
                    other => panic!("expected ChooseObjects, got {other:?}"),
                };
                assert_eq!(cands2.len(), 2);
                for &c in &cands2 {
                    assert_eq!(det.object(c).unwrap().card_def, CardDefId(0));
                }
                let res3 = crate::effects::resume_resolution_frame(
                    &mut det,
                    PlayerId(0),
                    p2,
                    crate::action::ChoiceAnswer::PickObject { id: cands2[0] },
                )
                .unwrap();
                assert_eq!(res3, crate::effects::FrameOutcome::Completed);
                assert_eq!(det.player(PlayerId(0)).unwrap().hand.len(), 1);
                let hand_card = det.player(PlayerId(0)).unwrap().hand[0];
                assert_eq!(det.object(hand_card).unwrap().card_def, CardDefId(0));
            }
        }

        // Opponent determinization does NOT pin P0's library:
        let mut saw_resampled = false;
        for s in 1..=16u64 {
            let mut rng = RngState::splitmix64(s);
            let det_opp = determinize_state(&state, PlayerId(1), &mut rng);
            let p0_lib = &det_opp.player(PlayerId(0)).unwrap().library;
            let land_cnt = p0_lib
                .iter()
                .filter(|&&id| det_opp.object(id).unwrap().card_def == CardDefId(0))
                .count();
            assert_eq!(land_cnt, 3);
            if det_opp.object(rem_land).unwrap().card_def != CardDefId(0) {
                saw_resampled = true;
            }
        }
        assert!(
            saw_resampled,
            "opponent determinization must be able to resample P0 library slots"
        );
    }

    #[test]
    fn synth_determinize_pins_single_dest_multi_pick_search_during_waiting_on_replacement() {
        let db = make_search_rep_test_db();
        let mut mainboard = vec![CardDefId(1); 10];
        mainboard.extend([CardDefId(0), CardDefId(0), CardDefId(0)]);
        let deck = DeckSpec {
            commanders: vec![],
            mainboard,
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 0;
        let mut state = GameState::new(db, cfg, &[deck.clone(), deck], 23).unwrap();

        for cd in [CardDefId(2), CardDefId(3)] {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    cd,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    crate::state::ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let step = make_search_step(
            vec![mtg_ir::SearchDestination {
                zone: Zone::Battlefield,
                tapped: false,
                on_top: false,
            }],
            2,
        );

        let out1 = crate::effects::begin_resolution_frame(
            &mut state,
            crate::id::StackItemId(702),
            PlayerId(0),
            None,
            None,
            0,
            vec![],
            vec![],
            SmallVec::new(),
            vec![step],
            vec![],
        )
        .unwrap();
        let crate::effects::FrameOutcome::Paused { prompt_id: p1, .. } = out1 else {
            panic!("expected SearchPick prompt 1, got {out1:?}");
        };
        let lands: Vec<ObjectId> = match &state.pending_prompt().unwrap().request {
            ChoiceRequest::ChooseObjects { candidates, .. } => candidates.clone(),
            other => panic!("expected ChooseObjects, got {other:?}"),
        };
        let land_0 = lands[0];
        let land_1 = lands[1];

        let out2 = crate::effects::resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p1,
            crate::action::ChoiceAnswer::PickObject { id: land_0 },
        )
        .unwrap();
        let crate::effects::FrameOutcome::Paused { prompt_id: p2, .. } = out2 else {
            panic!("expected SearchPick prompt 2, got {out2:?}");
        };
        let out3 = crate::effects::resume_resolution_frame(
            &mut state,
            PlayerId(0),
            p2,
            crate::action::ChoiceAnswer::PickObject { id: land_1 },
        )
        .unwrap();
        let crate::effects::FrameOutcome::Paused {
            prompt_id: rep_pid, ..
        } = out3
        else {
            panic!("expected WaitingOnReplacement pause, got {out3:?}");
        };

        assert!(crate::observe::is_object_visible_to(
            &state,
            land_0,
            Some(PlayerId(0))
        ));
        assert!(crate::observe::is_object_visible_to(
            &state,
            land_1,
            Some(PlayerId(0))
        ));
        assert!(!crate::observe::is_object_visible_to(
            &state,
            land_0,
            Some(PlayerId(1))
        ));
        assert!(!crate::observe::is_object_visible_to(
            &state,
            land_1,
            Some(PlayerId(1))
        ));

        for s in 1..=16u64 {
            let mut rng = RngState::splitmix64(s);
            let mut det = determinize_state(&state, PlayerId(0), &mut rng);
            assert_eq!(det.object(land_0).unwrap().card_def, CardDefId(0));
            assert_eq!(det.object(land_1).unwrap().card_def, CardDefId(0));
            if s == 1 {
                let rep_out = crate::replace::resume_and_finalize_replacement(
                    &mut det,
                    PlayerId(0),
                    rep_pid,
                    crate::action::ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap();
                let res2 = crate::effects::resume_after_replacement(&mut det, rep_out).unwrap();
                let crate::effects::FrameOutcome::Paused {
                    prompt_id: rep_pid2,
                    ..
                } = res2
                else {
                    panic!("expected second WaitingOnReplacement pause for land_1, got {res2:?}");
                };
                let rep_out2 = crate::replace::resume_and_finalize_replacement(
                    &mut det,
                    PlayerId(0),
                    rep_pid2,
                    crate::action::ChoiceAnswer::PickReplacement { index: 0 },
                )
                .unwrap();
                let res3 = crate::effects::resume_after_replacement(&mut det, rep_out2).unwrap();
                assert_eq!(res3, crate::effects::FrameOutcome::Completed);
                let bf_lands = det
                    .player(PlayerId(0))
                    .unwrap()
                    .battlefield
                    .iter()
                    .filter(|&&id| det.object(id).unwrap().card_def == CardDefId(0))
                    .count();
                assert_eq!(bf_lands, 2);
            }
        }
    }
}
