use mtg_ir::{
    AbilityDef, CardTypeMask, ColorMask, GamePhase, RelativeControl, StaticBody, StaticDef, Zone,
};
use smallvec::SmallVec;

use crate::action::{Action, ChoiceRequest};
use crate::eval::{BoardView, EvalContext, evaluate_condition};
use crate::faces::active_abilities_with_card_for_object;
use crate::id::{ObjectId, PlayerId};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::state::{
    CastingPromptKind, CostPromptKind, GameState, LayerCache, ObjectFlags, PromptOwner,
    StackEntryKind, StaticPresenceFlags, StepPauseState, TOKEN_CARD_DEF_ID,
    static_presence_flag_for_body,
};
use crate::view::{EntityObs, Observation, PlayerObs};
use crate::zones::is_known_to;

/// Dense numeric encoding of an [`Observation`] for `mtg-py` `VecEnvCore` and policy/value models.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatObservation {
    pub global_features: [f32; 8],
    pub player_features: Vec<f32>,
    pub entity_features: Vec<f32>,
}

#[inline]
fn rel_seat(abs: PlayerId, viewer: PlayerId, n: usize) -> u8 {
    if n == 0 {
        return 0;
    }
    ((usize::from(abs.0) + n - (usize::from(viewer.0) % n)) % n) as u8
}

#[inline]
fn perm_has_innate_static_flag(
    state: &GameState,
    perm_id: ObjectId,
    flag_filter: StaticPresenceFlags,
) -> bool {
    let Some(obj) = state.object(perm_id) else {
        return false;
    };
    if obj.is_face_down() {
        return false;
    }
    let db = state.db();
    let rare = state.rare(perm_id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let base_id = copiable.map_or(obj.card_def, |c| c.card_def);
    if db.get(base_id).is_some_and(|card| {
        card.faces.iter().any(|face| {
            face.abilities.iter().any(|ab| {
                matches!(
                    ab,
                    AbilityDef::Static(st)
                        if !(static_presence_flag_for_body(&st.static_def.body) & flag_filter)
                            .is_empty()
                )
            })
        })
    }) {
        return true;
    }
    if let Some(r) = rare {
        if !r.merged_components.is_empty() {
            return r.merged_components.iter().skip(1).any(|comp| {
                !comp.is_token
                    && db.get(comp.card_def).is_some_and(|mc| {
                        mc.faces.first().is_some_and(|face| {
                            face.abilities.iter().any(|ab| {
                                matches!(
                                    ab,
                                    AbilityDef::Static(st)
                                        if !(static_presence_flag_for_body(&st.static_def.body)
                                            & flag_filter)
                                            .is_empty()
                                )
                            })
                        })
                    })
            });
        }
        if let Some(cop) = copiable {
            return cop.merged_components.iter().skip(1).any(|&cid| {
                db.get(cid).is_some_and(|mc| {
                    mc.faces.first().is_some_and(|face| {
                        face.abilities.iter().any(|ab| {
                            matches!(
                                ab,
                                AbilityDef::Static(st)
                                    if !(static_presence_flag_for_body(&st.static_def.body)
                                        & flag_filter)
                                        .is_empty()
                            )
                        })
                    })
                })
            });
        }
    }
    false
}

#[inline]
fn for_each_perm_static<'a>(
    state: &'a GameState,
    perm_id: ObjectId,
    cache: &'a LayerCache,
    ctx: &EvalContext<'_>,
    flag_filter: StaticPresenceFlags,
    mut f: impl FnMut(&'a StaticDef),
) {
    if perm_has_innate_static_flag(state, perm_id, flag_filter) {
        for ab in active_abilities_with_card_for_object(state, perm_id, ctx) {
            if let AbilityDef::Static(st) = ab.ability
                && !(static_presence_flag_for_body(&st.static_def.body) & flag_filter).is_empty()
            {
                f(&st.static_def);
            }
        }
    }
    if let Some(extra) = cache.extra(perm_id) {
        for granted in &extra.granted_abilities {
            match granted {
                GrantedAbilityEntry::Inline { ability, .. } => {
                    if let AbilityDef::Static(st) = ability.as_ref()
                        && !(static_presence_flag_for_body(&st.static_def.body) & flag_filter)
                            .is_empty()
                    {
                        f(&st.static_def);
                    }
                }
                GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index,
                    ..
                } => {
                    if let Some(AbilityDef::Static(st)) = state
                        .db()
                        .get(*card_def)
                        .and_then(|c| c.faces.get(usize::from(*face_index)))
                        .and_then(|face| face.abilities.get(*ability_index))
                        && !(static_presence_flag_for_body(&st.static_def.body) & flag_filter)
                            .is_empty()
                    {
                        f(&st.static_def);
                    }
                }
                GrantedAbilityEntry::IntrinsicBasicMana { .. }
                | GrantedAbilityEntry::DynamicWard { .. } => {}
            }
        }
    }
}

/// Returns `true` if `owner`'s hand is currently revealed to `viewer` via
/// `StaticBody::HandVisibility` (CR 400.2, 402; e.g., Telepathy / Revelation)
/// or because `viewer == Some(owner)`.
pub fn is_hand_revealed_to(state: &GameState, owner: PlayerId, viewer: Option<PlayerId>) -> bool {
    let cache = state.evaluate_board();
    is_hand_revealed_to_cached(state, owner, viewer, cache)
}

pub(crate) fn is_hand_revealed_to_cached(
    state: &GameState,
    owner: PlayerId,
    viewer: Option<PlayerId>,
    cache: &LayerCache,
) -> bool {
    if viewer == Some(owner) {
        return true;
    }
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::HAND_VISIBILITY)
        && !state
            .players()
            .iter()
            .any(|p| p.lost && !p.battlefield.is_empty())
    {
        return false;
    }
    for p in state.players() {
        for &perm_id in &p.battlefield {
            let Some(perm_obj) = state.object(perm_id) else {
                continue;
            };
            if perm_obj.is_phased_out() {
                continue;
            }
            let eval = cache.get(perm_id);
            if eval.is_some_and(|e| e.lost_all_abilities) {
                continue;
            }
            let perm_ctrl = eval.map_or(perm_obj.controller, |e| e.controller);
            let perm_ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            let mut matched = false;
            for_each_perm_static(
                state,
                perm_id,
                cache,
                &perm_ctx,
                StaticPresenceFlags::HAND_VISIBILITY,
                |st| {
                    if matched {
                        return;
                    }
                    if let StaticBody::HandVisibility { players } = &st.body {
                        if let Some(cond) = st.condition.as_deref()
                            && !evaluate_condition(state, cond, &perm_ctx)
                        {
                            return;
                        }
                        let applies_to_owner = match players {
                            RelativeControl::All | RelativeControl::Any => true,
                            RelativeControl::Opponents | RelativeControl::Opponent => {
                                owner != perm_ctrl
                            }
                            RelativeControl::You => owner == perm_ctrl,
                        };
                        if applies_to_owner {
                            matched = true;
                        }
                    }
                },
            );
            if matched {
                return true;
            }
        }
    }
    false
}

/// Returns `true` if `owner`'s top library card is visible to `viewer` via
/// `StaticBody::TopOfLibraryVisibility` (CR 401.5; suppressed for controller-only look
/// while `state.casting().is_some()`) or `zones::is_known_to`.
pub fn is_top_of_library_visible_to(
    state: &GameState,
    owner: PlayerId,
    viewer: Option<PlayerId>,
) -> bool {
    let cache = state.evaluate_board();
    is_top_of_library_visible_to_cached(state, owner, viewer, cache)
}

pub(crate) fn is_top_of_library_visible_to_cached(
    state: &GameState,
    owner: PlayerId,
    viewer: Option<PlayerId>,
    cache: &LayerCache,
) -> bool {
    if let Some(v) = viewer
        && let Some(top_id) = state.player(owner).and_then(|p| p.library.last().copied())
        && is_known_to(state, top_id, v)
    {
        return true;
    }
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY)
        && !state
            .players()
            .iter()
            .any(|p| p.lost && !p.battlefield.is_empty())
    {
        return false;
    }

    for p in state.players() {
        for &perm_id in &p.battlefield {
            let Some(perm_obj) = state.object(perm_id) else {
                continue;
            };
            if perm_obj.is_phased_out() {
                continue;
            }
            let eval = cache.get(perm_id);
            if eval.is_some_and(|e| e.lost_all_abilities) {
                continue;
            }
            let perm_ctrl = eval.map_or(perm_obj.controller, |e| e.controller);
            let perm_ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            let mut matched = false;
            for_each_perm_static(
                state,
                perm_id,
                cache,
                &perm_ctx,
                StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY,
                |st| {
                    if matched {
                        return;
                    }
                    if let StaticBody::TopOfLibraryVisibility {
                        reveal_to_all,
                        all_players,
                    } = &st.body
                    {
                        if let Some(cond) = st.condition.as_deref()
                            && !evaluate_condition(state, cond, &perm_ctx)
                        {
                            return;
                        }
                        let applies_to_owner = *all_players || owner == perm_ctrl;
                        if !applies_to_owner {
                            return;
                        }
                        if *reveal_to_all {
                            matched = true;
                        } else if viewer == Some(owner)
                            && state.casting().is_none()
                            && state.inline_mana_depth() == 0
                        {
                            // CR 401.5: controller may not look at top card while casting/activating.
                            matched = true;
                        }
                    }
                },
            );
            if matched {
                return true;
            }
        }
    }
    false
}

pub(crate) fn is_in_active_viewer_prompt(
    state: &GameState,
    id: ObjectId,
    viewer: PlayerId,
) -> bool {
    if let Some(pp) = state.pending_prompt()
        && pp.chooser == viewer
    {
        match &pp.request {
            ChoiceRequest::ChooseObjects { candidates, .. } if candidates.contains(&id) => {
                return true;
            }
            ChoiceRequest::OrderObjects { objects, .. } if objects.contains(&id) => {
                return true;
            }
            _ => {}
        }
        match &pp.owner {
            PromptOwner::CostPayment(cp) => {
                if let CostPromptKind::ObjectSelection {
                    candidates, picked, ..
                } = &cp.kind
                    && (candidates.contains(&id) || picked.contains(&id))
                {
                    return true;
                }
            }
            PromptOwner::Casting(CastingPromptKind::CostObjectSelection {
                candidates,
                picked,
                ..
            }) if candidates.contains(&id) || picked.contains(&id) => {
                return true;
            }
            _ => {}
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
            } if *chooser == viewer && (candidates.contains(&id) || selected.contains(&id)) => {
                return true;
            }
            StepPauseState::SearchPick {
                chooser,
                candidates,
                selected,
                ..
            } if *chooser == viewer && (candidates.contains(&id) || selected.contains(&id)) => {
                return true;
            }
            StepPauseState::ReorderLibrary {
                player,
                remaining,
                top_order,
                bottom_or_gy,
                deciding_card,
                ..
            } if *player == viewer
                && (remaining.contains(&id)
                    || top_order.contains(&id)
                    || bottom_or_gy.contains(&id)
                    || *deciding_card == Some(id)) =>
            {
                return true;
            }
            StepPauseState::DigPick {
                player,
                revealed,
                candidates,
                selected,
                ..
            } if *player == viewer
                && (revealed.contains(&id)
                    || candidates.contains(&id)
                    || selected.contains(&id)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Unified object visibility predicate combining `zones::is_known_to`,
/// `is_hand_revealed_to`, `is_top_of_library_visible_to` (for `owner.library.last()`),
/// `StaticBody::MayLookAtExiledWithSelf`, and active `viewer` prompt/pause candidates.
pub fn is_object_visible_to(state: &GameState, id: ObjectId, viewer: Option<PlayerId>) -> bool {
    let cache = state.evaluate_board();
    is_object_visible_to_cached(state, id, viewer, cache)
}

pub(crate) fn is_object_visible_to_cached(
    state: &GameState,
    id: ObjectId,
    viewer: Option<PlayerId>,
    cache: &LayerCache,
) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone == Zone::Battlefield && obj.is_phased_out() {
        return false;
    }
    if let Some(v) = viewer
        && (is_known_to(state, id, v) || is_in_active_viewer_prompt(state, id, v))
    {
        return true;
    }

    match obj.zone {
        Zone::Battlefield | Zone::Stack => true,
        Zone::Graveyard | Zone::Command => !obj.is_face_down(),
        Zone::Exile => {
            if !obj.is_face_down() {
                return true;
            }
            if let Some(v) = viewer
                && (cache
                    .static_presence
                    .contains(StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF)
                    || state
                        .players()
                        .iter()
                        .any(|p| p.lost && !p.battlefield.is_empty()))
                && let Some(host_id) = state.rare(id).and_then(|r| r.exiled_by)
                && let Some(host_obj) = state.object(host_id)
                && host_obj.zone == Zone::Battlefield
                && !host_obj.is_phased_out()
            {
                let host_eval = cache.get(host_id);
                if !host_eval.is_some_and(|e| e.lost_all_abilities) {
                    let host_ctrl = host_eval.map_or(host_obj.controller, |e| e.controller);
                    if host_ctrl == v {
                        let ctx = EvalContext::for_source(host_ctrl, host_id)
                            .with_board(BoardView::Cached(cache));
                        let mut may_look = false;
                        for_each_perm_static(
                            state,
                            host_id,
                            cache,
                            &ctx,
                            StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF,
                            |st| {
                                if matches!(st.body, StaticBody::MayLookAtExiledWithSelf)
                                    && st
                                        .condition
                                        .as_deref()
                                        .is_none_or(|c| evaluate_condition(state, c, &ctx))
                                {
                                    may_look = true;
                                }
                            },
                        );
                        if may_look {
                            return true;
                        }
                    }
                }
            }
            false
        }
        Zone::Hand => {
            !obj.is_face_down() && is_hand_revealed_to_cached(state, obj.owner, viewer, cache)
        }
        Zone::Library => {
            let is_top = state
                .player(obj.owner)
                .and_then(|p| p.library.last().copied())
                == Some(id);
            is_top && is_top_of_library_visible_to_cached(state, obj.owner, viewer, cache)
        }
    }
}

/// Builds a viewer-relative ML [`Observation`] aligned 1:1 with `legal_actions`.
///
/// Guarantees:
/// - `players` is rotated so `relative_seat == 0` corresponds to `viewer`.
/// - `entities` NEVER contains `Zone::Library` or `Zone::Hand` with `owner_rel != 0`.
/// - Face-down objects not known to `viewer` have `card_def` redacted to `TOKEN_CARD_DEF_ID`.
pub fn observe_state(
    state: &GameState,
    viewer: PlayerId,
    legal_actions: Vec<Action>,
) -> Observation {
    let n = state.players().len();
    let mut players = SmallVec::with_capacity(n);
    if n > 0 {
        let start = usize::from(viewer.0) % n;
        for offset in 0..n {
            let idx = (start + offset) % n;
            let p = &state.players()[idx];
            players.push(PlayerObs {
                relative_seat: offset as u8,
                life: p.life,
                lost: p.lost,
                poison: p.poison,
                energy: p.energy,
                hand_count: p.hand.len() as u16,
                library_count: p.library.len() as u16,
                graveyard_count: p.graveyard.len() as u16,
            });
        }
    }

    let eval_cache = state.evaluate_board();
    let mut entities = Vec::new();

    let push_object_entity = |entities: &mut Vec<EntityObs>, id: ObjectId, zone_override: Zone| {
        let Some(obj) = state.object(id) else {
            return;
        };
        let eval = eval_cache.get(id);
        let known = if zone_override == Zone::Exile {
            is_object_visible_to_cached(state, id, Some(viewer), eval_cache)
        } else {
            is_known_to(state, id, viewer)
        };
        let redact_fd = obj.is_face_down() && !known;
        let (card_def, pt, types, colors) = if redact_fd {
            if matches!(zone_override, Zone::Battlefield | Zone::Stack) {
                (
                    TOKEN_CARD_DEF_ID,
                    eval.and_then(|e| e.pt),
                    eval.map_or(CardTypeMask::CREATURE, |e| e.types),
                    eval.map_or(ColorMask::empty(), |e| e.colors),
                )
            } else {
                (
                    TOKEN_CARD_DEF_ID,
                    None,
                    CardTypeMask::empty(),
                    ColorMask::empty(),
                )
            }
        } else {
            (
                obj.card_def,
                eval.and_then(|e| e.pt),
                eval.map_or(CardTypeMask::empty(), |e| e.types),
                eval.map_or(ColorMask::empty(), |e| e.colors),
            )
        };
        let eff_ctrl = eval.map_or(obj.controller, |e| e.controller);
        entities.push(EntityObs {
            id,
            card_def,
            zone: zone_override,
            owner_rel: rel_seat(obj.owner, viewer, n),
            controller_rel: rel_seat(eff_ctrl, viewer, n),
            pt,
            types,
            colors,
            tapped: obj.flags.contains(ObjectFlags::TAPPED),
            summoning_sick: obj.flags.contains(ObjectFlags::SUMMONING_SICK),
        });
    };

    // 1. Battlefield (excluding phased-out permanents).
    for p in state.players() {
        for &id in &p.battlefield {
            if state.object(id).is_some_and(|o| !o.is_phased_out()) {
                push_object_entity(&mut entities, id, Zone::Battlefield);
            }
        }
    }

    // 2. Stack (both Spell objects and Activated/Triggered LKI items, plus any raw slots).
    for entry in state.stack_entries() {
        match &entry.kind {
            StackEntryKind::Spell { object } => {
                push_object_entity(&mut entities, *object, Zone::Stack);
            }
            StackEntryKind::Activated { source, lki, .. }
            | StackEntryKind::Triggered { source, lki, .. } => {
                let redact_fd = lki.face_down && !is_known_to(state, *source, viewer);
                entities.push(EntityObs {
                    id: *source,
                    card_def: if redact_fd {
                        TOKEN_CARD_DEF_ID
                    } else {
                        lki.card_def
                    },
                    zone: Zone::Stack,
                    owner_rel: rel_seat(lki.owner, viewer, n),
                    controller_rel: rel_seat(entry.controller, viewer, n),
                    pt: lki.pt,
                    types: lki.types,
                    colors: lki.colors,
                    tapped: false,
                    summoning_sick: false,
                });
            }
        }
    }
    if state.stack().len() > state.stack_entries().len() {
        for &id in state.stack().iter().skip(state.stack_entries().len()) {
            push_object_entity(&mut entities, id, Zone::Stack);
        }
    }

    // 3. Viewer's Hand ONLY (never emit opponent Zone::Hand entities).
    if let Some(vp) = state.player(viewer) {
        for &id in &vp.hand {
            push_object_entity(&mut entities, id, Zone::Hand);
        }
    }

    // 4. Graveyard (all players).
    for p in state.players() {
        for &id in &p.graveyard {
            push_object_entity(&mut entities, id, Zone::Graveyard);
        }
    }

    // 5. Exile (all players; face-down unknown cards are redacted).
    for p in state.players() {
        for &id in &p.exile {
            push_object_entity(&mut entities, id, Zone::Exile);
        }
    }

    // 6. Command (all players).
    for p in state.players() {
        for &id in &p.command {
            push_object_entity(&mut entities, id, Zone::Command);
        }
    }

    Observation {
        viewer,
        turn_number: state.turn_number(),
        phase: state.phase(),
        players,
        entities,
        legal_actions,
    }
}

#[inline]
fn phase_code(phase: GamePhase) -> f32 {
    match phase {
        GamePhase::Untap => 0.0,
        GamePhase::Upkeep => 1.0,
        GamePhase::Draw => 2.0,
        GamePhase::Main1 => 3.0,
        GamePhase::BeginCombat => 4.0,
        GamePhase::DeclareAttackers => 5.0,
        GamePhase::DeclareBlockers => 6.0,
        GamePhase::FirstStrikeDamage => 7.0,
        GamePhase::CombatDamage => 8.0,
        GamePhase::EndCombat => 9.0,
        GamePhase::Main2 => 10.0,
        GamePhase::EndStep => 11.0,
        GamePhase::Cleanup => 12.0,
    }
}

#[inline]
fn zone_code(zone: Zone) -> f32 {
    match zone {
        Zone::Library => 0.0,
        Zone::Hand => 1.0,
        Zone::Battlefield => 2.0,
        Zone::Graveyard => 3.0,
        Zone::Exile => 4.0,
        Zone::Command => 5.0,
        Zone::Stack => 6.0,
    }
}

/// Encodes an [`Observation`] into fixed-stride finite `f32` vectors (`8` global, `8 * N` player,
/// `12 * E` entity features).
pub fn encode_flat_observation(obs: &Observation) -> FlatObservation {
    let alive_players = obs.players.iter().filter(|p| !p.lost).count() as f32;
    let viewer_life = obs.players.first().map_or(0.0, |p| p.life as f32);
    let global_features = [
        obs.turn_number as f32,
        phase_code(obs.phase),
        f32::from(obs.viewer.0),
        obs.players.len() as f32,
        obs.entities.len() as f32,
        obs.legal_actions.len() as f32,
        viewer_life,
        alive_players,
    ];

    let mut player_features = Vec::with_capacity(obs.players.len() * 8);
    for p in &obs.players {
        player_features.extend_from_slice(&[
            f32::from(p.relative_seat),
            p.life as f32,
            if p.lost { 1.0 } else { 0.0 },
            f32::from(p.poison),
            f32::from(p.energy),
            f32::from(p.hand_count),
            f32::from(p.library_count),
            f32::from(p.graveyard_count),
        ]);
    }

    let mut entity_features = Vec::with_capacity(obs.entities.len() * 12);
    for ent in &obs.entities {
        let def_code = if ent.card_def == TOKEN_CARD_DEF_ID {
            -1.0
        } else {
            ent.card_def.0 as f32
        };
        let (power, toughness, has_pt) = match ent.pt {
            Some(pt) => (pt.power as f32, pt.toughness as f32, 1.0),
            None => (0.0, 0.0, 0.0),
        };
        entity_features.extend_from_slice(&[
            zone_code(ent.zone),
            f32::from(ent.owner_rel),
            f32::from(ent.controller_rel),
            def_code,
            power,
            toughness,
            has_pt,
            f32::from(ent.types.bits()),
            f32::from(ent.colors.bits()),
            if ent.tapped { 1.0 } else { 0.0 },
            if ent.summoning_sick { 1.0 } else { 0.0 },
            ent.id.index as f32,
        ]);
    }

    FlatObservation {
        global_features,
        player_features,
        entity_features,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mtg_ir::{
        AbilityDef, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics,
        ColorMask, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, PrintedPt,
        RelativeControl, StaticAbility, StaticBody, StaticDef, SupertypeMask, Zone,
    };
    use smallvec::SmallVec;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{
        AltCastKind, CastKind, CastStep, CastingState, FaceDownKind, FaceDownSpec, GameState,
    };
    use crate::zones::{
        MoveRawOptions, exile_face_down, move_object_raw, phase_out_permanent, reveal_to_player,
    };

    fn make_synth_db() -> Arc<CardDatabase> {
        let bear = CardDef {
            oracle_id: "synth_bear".into(),
            compiler_version: 46,
            text_hash: "0001".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{G}"),
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let telepathy_aura = CardDef {
            oracle_id: "synth_telepathy".into(),
            compiler_version: 46,
            text_hash: "0002".into(),
            name: "Synth Eye".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Eye".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{U}"),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::HandVisibility {
                            players: RelativeControl::Opponents,
                        },
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
        let oracle_lens = CardDef {
            oracle_id: "synth_lens".into(),
            compiler_version: 46,
            text_hash: "0003".into(),
            name: "Synth Lens".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lens".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::TopOfLibraryVisibility {
                            reveal_to_all: false,
                            all_players: false,
                        },
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
        let public_lantern = CardDef {
            oracle_id: "synth_lantern".into(),
            compiler_version: 46,
            text_hash: "0004".into(),
            name: "Synth Lantern".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lantern".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::TopOfLibraryVisibility {
                            reveal_to_all: true,
                            all_players: true,
                        },
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
        let vault_glass = CardDef {
            oracle_id: "synth_vault_glass".into(),
            compiler_version: 46,
            text_hash: "0005".into(),
            name: "Synth Vault Glass".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Vault Glass".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}"),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::MayLookAtExiledWithSelf,
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
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    bear,
                    telepathy_aura,
                    oracle_lens,
                    public_lantern,
                    vault_glass,
                ],
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn observe_state_rotates_4p_seats_and_never_emits_library_or_opp_hand() {
        let db = make_synth_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::commander(4);
        cfg.starting_hand_size = 5;
        let mut state = GameState::new(
            db,
            cfg,
            &[deck.clone(), deck.clone(), deck.clone(), deck],
            7,
        )
        .unwrap();

        // Move one P0 card onto Battlefield and phase out another P0 card.
        let p0_hand_0 = state.players()[0].hand[0];
        let p0_bf = move_object_raw(
            &mut state,
            p0_hand_0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let p0_hand_1 = state.players()[0].hand[0];
        let p0_phased = move_object_raw(
            &mut state,
            p0_hand_1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(phase_out_permanent(&mut state, p0_phased));

        // Move a P1 card face-down into Exile and another into Battlefield face-down.
        let p1_hand_0 = state.players()[1].hand[0];
        let p1_exile_fd = exile_face_down(
            &mut state,
            p1_hand_0,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let p1_hand_1 = state.players()[1].hand[0];
        let p1_bf_fd = move_object_raw(
            &mut state,
            p1_hand_1,
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

        // Observe from seat 2.
        let obs = observe_state(&state, PlayerId(2), vec![Action::PassPriority]);
        assert_eq!(obs.viewer, PlayerId(2));
        assert_eq!(obs.players.len(), 4);
        assert_eq!(obs.players[0].relative_seat, 0);
        assert_eq!(
            obs.players[0].hand_count,
            state.players()[2].hand.len() as u16
        );
        assert_eq!(obs.players[2].relative_seat, 2);
        assert_eq!(
            obs.players[2].hand_count,
            state.players()[0].hand.len() as u16
        );
        assert_eq!(obs.legal_actions, vec![Action::PassPriority]);

        for ent in &obs.entities {
            assert_ne!(ent.zone, Zone::Library);
            if ent.zone == Zone::Hand {
                assert_eq!(ent.owner_rel, 0);
            }
            assert_ne!(ent.id, p0_phased);
        }

        let bf_ent = obs.entities.iter().find(|e| e.id == p0_bf).unwrap();
        assert_eq!(bf_ent.owner_rel, 2);
        assert_eq!(bf_ent.card_def, CardDefId(0));

        let fd_bf_ent = obs.entities.iter().find(|e| e.id == p1_bf_fd).unwrap();
        assert_eq!(fd_bf_ent.card_def, TOKEN_CARD_DEF_ID);
        assert_eq!(fd_bf_ent.types, CardTypeMask::CREATURE);
        assert_eq!(fd_bf_ent.owner_rel, 3);

        let fd_ex_ent = obs.entities.iter().find(|e| e.id == p1_exile_fd).unwrap();
        assert_eq!(fd_ex_ent.card_def, TOKEN_CARD_DEF_ID);
        assert_eq!(fd_ex_ent.pt, None);
        assert!(fd_ex_ent.types.is_empty());

        let flat = encode_flat_observation(&obs);
        assert_eq!(flat.player_features.len(), 4 * 8);
        assert_eq!(flat.entity_features.len(), obs.entities.len() * 12);
        assert!(flat.global_features.iter().all(|v| v.is_finite()));
        assert!(flat.player_features.iter().all(|v| v.is_finite()));
        assert!(flat.entity_features.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn hand_and_top_of_library_visibility_respect_statics_and_cr_401_5() {
        let db = make_synth_db();
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(3); 10],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 2;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 11).unwrap();

        let p0_hand_card = state.players()[0].hand[0];
        let p1_hand_card = state.players()[1].hand[0];
        let p0_top = *state.players()[0].library.last().unwrap();
        let p1_top = *state.players()[1].library.last().unwrap();

        assert!(!is_hand_revealed_to(&state, PlayerId(1), Some(PlayerId(0))));
        assert!(is_hand_revealed_to(&state, PlayerId(0), Some(PlayerId(0))));
        assert!(!is_object_visible_to(
            &state,
            p1_hand_card,
            Some(PlayerId(0))
        ));
        assert!(!is_object_visible_to(&state, p0_top, Some(PlayerId(0))));

        // Find Synth Eye (CardDefId(1)) and Synth Lens (CardDefId(2)) in P0's hand or library.
        let eye_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let lens_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();

        move_object_raw(
            &mut state,
            eye_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        move_object_raw(
            &mut state,
            lens_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        // HandVisibility { Opponents } reveals P1's hand to P0, not P0's hand to P1.
        assert!(is_hand_revealed_to(&state, PlayerId(1), Some(PlayerId(0))));
        assert!(!is_hand_revealed_to(&state, PlayerId(0), Some(PlayerId(1))));
        assert!(is_object_visible_to(
            &state,
            p1_hand_card,
            Some(PlayerId(0))
        ));
        assert!(!is_object_visible_to(
            &state,
            p0_hand_card,
            Some(PlayerId(1))
        ));

        // TopOfLibraryVisibility { reveal_to_all: false, all_players: false }:
        // P0 can see P0's top card when not casting; P1 and spectator cannot.
        let p0_top_now = *state.players()[0].library.last().unwrap();
        assert!(is_top_of_library_visible_to(
            &state,
            PlayerId(0),
            Some(PlayerId(0))
        ));
        assert!(!is_top_of_library_visible_to(
            &state,
            PlayerId(0),
            Some(PlayerId(1))
        ));
        assert!(!is_top_of_library_visible_to(&state, PlayerId(0), None));
        assert!(is_object_visible_to(&state, p0_top_now, Some(PlayerId(0))));

        // CR 401.5: while casting, controller-only look is suppressed.
        state.set_casting(Some(CastingState {
            caster: PlayerId(0),
            object: p0_hand_card,
            face_idx: 0,
            from_zone: Zone::Hand,
            step: CastStep::PayCosts,
            chosen_modes: SmallVec::new(),
            x_value: None,
            optional_costs: SmallVec::new(),
            optional_cost_cursor: 0,
            targets: Vec::new(),
            divisions: SmallVec::new(),
            locked_cost: ManaCost::default(),
            mana_paid: SmallVec::new(),
            paid_mana_colors: ColorMask::empty(),
            kind: CastKind::Spell,
            option_id: 0,
            alt_kind: AltCastKind::Normal,
            is_kicked: false,
            kicked_indices: SmallVec::new(),
            kicked_tags: SmallVec::new(),
            fused: false,
            cleaved: false,
            gift_promised: false,
            additional_cost_paid: false,
            bargained: false,
            target_spec_cursor: 0,
            targets_by_spec: Vec::new(),
            division_cursor: 0,
            payment_plan: None,
            paid_mana_sources: SmallVec::new(),
            cant_be_countered_from_mana: false,
            mana_commander_bonus_counters: 0,
            mana_spent_total: 0,
            cast_option: None,
            pre_cast_exile: None,
        }));
        assert!(!is_top_of_library_visible_to(
            &state,
            PlayerId(0),
            Some(PlayerId(0))
        ));
        state.take_casting();

        // Explicit reveal_to_player makes P1's top card visible to P0.
        assert!(reveal_to_player(&mut state, p1_top, PlayerId(0)));
        assert!(is_top_of_library_visible_to(
            &state,
            PlayerId(1),
            Some(PlayerId(0))
        ));
    }

    #[test]
    fn observe_state_reveals_face_down_exile_with_may_look_at_exiled_with_self_for_controller_only()
    {
        let db = make_synth_db();
        let mut p0_cards = vec![CardDefId(4)];
        p0_cards.extend(vec![CardDefId(0); 9]);
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: p0_cards,
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 2;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 29).unwrap();

        let glass_src = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(4))
            .unwrap();
        let glass_bf = move_object_raw(
            &mut state,
            glass_src,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let p1_card = state.players()[1].library[0];
        let exiled_fd = exile_face_down(
            &mut state,
            p1_card,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.rare_or_default(exiled_fd).unwrap().exiled_by = Some(glass_bf);
        state.mark_dirty(crate::state::DirtyMask::EXILE);

        let obs_p0 = observe_state(&state, PlayerId(0), vec![]);
        let ent_p0 = obs_p0.entities.iter().find(|e| e.id == exiled_fd).unwrap();
        assert_eq!(ent_p0.zone, Zone::Exile);
        assert_eq!(ent_p0.card_def, CardDefId(0));

        let obs_p1 = observe_state(&state, PlayerId(1), vec![]);
        let ent_p1 = obs_p1.entities.iter().find(|e| e.id == exiled_fd).unwrap();
        assert_eq!(ent_p1.zone, Zone::Exile);
        assert_eq!(ent_p1.card_def, TOKEN_CARD_DEF_ID);
    }

    #[test]
    fn synth_visibility_fast_path_with_granted_statics_and_flags() {
        let db = make_synth_db();
        let deck0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 15],
            sideboard: vec![],
        };
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 15],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.starting_hand_size = 2;
        let mut state = GameState::new(db, cfg, &[deck0, deck1], 43).unwrap();

        let p0_bear_hand = state.players()[0].hand[0];
        let p0_bear_bf = move_object_raw(
            &mut state,
            p0_bear_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // Initially no visibility flags are present.
        assert!(state.evaluate_board().static_presence.is_empty());
        assert!(!is_hand_revealed_to(&state, PlayerId(1), Some(PlayerId(0))));
        assert!(!is_top_of_library_visible_to(
            &state,
            PlayerId(0),
            Some(PlayerId(0))
        ));

        // Grant HandVisibility and TopOfLibraryVisibility { reveal_to_all: false } via LayerMod::GrantAbility
        // onto a vanilla Synth Bear (perm_has_innate_static_flag is false, granted_abilities supplies it).
        let ts1 = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                p0_bear_bf
            ]),
            modifier: mtg_ir::LayerMod::GrantAbility {
                ability: Box::new(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::HandVisibility {
                            players: RelativeControl::Opponents,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })),
            },
            duration: mtg_ir::Duration::EndOfTurn,
            timestamp: ts1,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                p0_bear_bf
            ]),
            modifier: mtg_ir::LayerMod::GrantAbility {
                ability: Box::new(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::TopOfLibraryVisibility {
                            reveal_to_all: false,
                            all_players: false,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })),
            },
            duration: mtg_ir::Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        let flags = state.evaluate_board().static_presence;
        assert!(flags.contains(StaticPresenceFlags::HAND_VISIBILITY));
        assert!(flags.contains(StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY));
        assert!(!flags.contains(StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF));

        assert!(is_hand_revealed_to(&state, PlayerId(1), Some(PlayerId(0))));
        assert!(is_hand_revealed_to(&state, PlayerId(1), None));
        assert!(!is_hand_revealed_to(&state, PlayerId(0), None));
        assert!(!is_hand_revealed_to(&state, PlayerId(0), Some(PlayerId(1))));
        assert!(is_top_of_library_visible_to(
            &state,
            PlayerId(0),
            Some(PlayerId(0))
        ));
        assert!(!is_top_of_library_visible_to(&state, PlayerId(0), None));
    }
}
