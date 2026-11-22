use std::collections::VecDeque;
use std::sync::Arc;

use mtg_ir::{
    AbilityDef, CardTypeMask, ColorMask, CopyModificationSpec, KeywordId, RoundMode, StaticBody,
    SupertypeMask, TokenSpec, Zone,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::id::{ObjectId, PlayerId};
use crate::replace::{ReplacementOutcome, ReplacementResult};
use crate::state::{
    CopiableState, DirtyMask, EngineError, EvaluatedObject, FaceDownSpec, GameObject, GameState,
    LkiSnapshot, ObjectFlags, RareObjectState, ReplaceableEventKind, ReplacementAccumulatedMods,
    ReplacementContinuation, StackChoices, StackEntry, StackEntryKind, TOKEN_CARD_DEF_ID,
    evaluate_token_spec_base,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ZonePosition {
    #[default]
    Default,
    Top,
    Bottom,
    /// `0` = top of library (`library.last()`), `n` = `n` cards from top.
    FromTop(usize),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MoveRawOptions {
    pub to_player: Option<PlayerId>,
    pub controller: Option<PlayerId>,
    pub position: ZonePosition,
    pub bump_generation: Option<bool>,
    pub new_timestamp: Option<bool>,
    pub leave_battlefield_reset: Option<bool>,
    pub detach_attachments: Option<bool>,
    pub cease_immediately: bool,
    pub preserve_cast_state: bool,
    pub enter_tapped: bool,
    pub entered_transformed: bool,
    pub entered_flipped: bool,
    pub enter_face_down: Option<FaceDownSpec>,
    pub capture_lki: Option<bool>,
    pub is_discard: bool,
    pub discard_caused_by: Option<PlayerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRawOutcome {
    pub old_id: ObjectId,
    pub new_id: Option<ObjectId>,
    pub from_zone: Zone,
    pub to_zone: Zone,
    #[serde(with = "crate::state::opt_arc_serde")]
    pub lki: Option<Arc<LkiSnapshot>>,
    pub ceased_to_exist: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTokenOptions {
    pub tapped: bool,
    pub attacking: bool,
    pub created_by_source: Option<ObjectId>,
    pub owner: Option<PlayerId>,
}

/// Low-level `CR 400.7` zone funnel across all 7 zones (including `Zone::Stack`).
///
/// M4 wraps this in [`move_object`] with `CR 614–616` replacements and `CR 603` atomic events.
pub fn move_object_raw(
    state: &mut GameState,
    id: ObjectId,
    dest: Zone,
    opts: MoveRawOptions,
    lki_eval: Option<&EvaluatedObject>,
) -> Result<MoveRawOutcome, EngineError> {
    let (from_zone, owner, controller, card_def, eff_def, old_flags) = {
        let obj = state
            .object(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} does not exist")))?;
        let eff_def = state
            .rare(id)
            .and_then(|r| r.copiable.as_deref())
            .map_or(obj.card_def, |c| c.card_def);
        (
            obj.zone,
            obj.owner,
            obj.controller,
            obj.card_def,
            eff_def,
            obj.flags,
        )
    };

    // CR 304.4 / 307.4: an instant or sorcery without a permanent type cannot enter the battlefield.
    if dest == Zone::Battlefield
        && opts.enter_face_down.is_none()
        && !old_flags.contains(ObjectFlags::FACE_DOWN)
        && !old_flags.contains(ObjectFlags::IS_TOKEN)
    {
        let eff_types = lki_eval
            .filter(|_| from_zone == Zone::Stack && !opts.entered_transformed)
            .map(|e| e.types)
            .unwrap_or_else(|| base_types_for_object(state, id, opts.entered_transformed));
        if eff_types.intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
            && !eff_types.intersects(CardTypeMask::PERMANENT)
        {
            return Ok(MoveRawOutcome {
                old_id: id,
                new_id: Some(id),
                from_zone,
                to_zone: from_zone,
                lki: None,
                ceased_to_exist: false,
            });
        }
    }

    // CR 400.3: Library, Hand, Graveyard, and Command are owned-zone only.
    let dest_player = match dest {
        Zone::Library | Zone::Hand | Zone::Graveyard | Zone::Command => owner,
        Zone::Battlefield => opts.controller.or(opts.to_player).unwrap_or(owner),
        Zone::Exile | Zone::Stack => opts.to_player.unwrap_or(owner),
    };
    if dest != Zone::Stack && usize::from(dest_player.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid destination player {dest_player:?}"
        )));
    }

    // Capture Last Known Information prior to leaving the source zone (skip hidden Library/Hand->Library).
    let should_capture_lki = opts.capture_lki.unwrap_or_else(|| {
        !matches!(from_zone, Zone::Library)
            && !(from_zone == Zone::Hand && dest == Zone::Library)
            && from_zone != dest
    });
    let lki = if should_capture_lki {
        let snap = if let Some((pre_snap, _)) =
            crate::events::pre_batch_lki(state, id).filter(|(s, _)| s.last_zone == from_zone)
        {
            Arc::clone(pre_snap)
        } else {
            Arc::new(capture_lki_snapshot(state, id, lki_eval))
        };
        state.record_lki(id, Arc::clone(&snap));
        Some(snap)
    } else {
        None
    };

    // Remove from source zone.
    remove_from_zone(state, id, from_zone, owner, controller);

    // Detach any objects attached to `id` when leaving the battlefield (or explicitly requested).
    let should_detach = opts
        .detach_attachments
        .unwrap_or(from_zone == Zone::Battlefield && dest != Zone::Battlefield);
    if should_detach {
        detach_all_attachments_from(state, id);
    }

    if from_zone == Zone::Battlefield && dest != Zone::Battlefield {
        remove_from_combat(state, id);
    }

    let is_cross_zone = from_zone != dest;
    let bump_gen = opts
        .bump_generation
        .unwrap_or(is_cross_zone && !(from_zone == Zone::Stack && dest == Zone::Battlefield));
    let new_ts = opts.new_timestamp.unwrap_or(is_cross_zone);
    let ltb_reset = opts
        .leave_battlefield_reset
        .unwrap_or(from_zone == Zone::Battlefield && dest != Zone::Battlefield);

    if opts.cease_immediately {
        state.remove_object(id);
        mark_zone_dirty(state, from_zone, dest);
        return Ok(MoveRawOutcome {
            old_id: id,
            new_id: None,
            from_zone,
            to_zone: dest,
            lki,
            ceased_to_exist: true,
        });
    }

    let keep_counters_non_hidden = ltb_reset
        && matches!(
            dest,
            Zone::Battlefield | Zone::Graveyard | Zone::Exile | Zone::Command
        )
        && has_counters_remain_in_non_hidden_zones(state, card_def);

    let allocated_ts = if new_ts {
        Some(state.allocate_timestamp())
    } else {
        None
    };

    let init_room_door = dest == Zone::Battlefield
        && from_zone == Zone::Stack
        && crate::faces::is_room_card(state.db(), eff_def);

    if let Some(obj) = state.object_mut(id) {
        obj.zone = dest;
        if let Some(ts) = allocated_ts {
            obj.entered_timestamp = ts;
        }
    }

    if is_cross_zone && let Some(rare) = state.rare_mut(id) {
        rare.known_to_players.clear();
    }

    if ltb_reset {
        if !keep_counters_non_hidden {
            state.clear_counters(id);
        }
        if let Some(obj) = state.object_mut(id) {
            obj.controller = obj.owner;
            obj.damage_marked = 0;
            obj.deathtouch_damage_marked = false;
            obj.attached_to = None;
            obj.active_face_index = 0;
            let persistent = obj.flags
                & (ObjectFlags::IS_TOKEN
                    | ObjectFlags::IS_COMMANDER
                    | ObjectFlags::IS_EMBLEM
                    | ObjectFlags::IS_DUNGEON);
            obj.flags = persistent;
        }
        if let Some(rare) = state.rare_mut(id) {
            reset_rare_on_ltb(rare, keep_counters_non_hidden);
        }
    } else if from_zone == Zone::Stack && dest != Zone::Battlefield && !opts.preserve_cast_state {
        if let Some(obj) = state.object_mut(id) {
            obj.controller = obj.owner;
            obj.active_face_index = 0;
            obj.flags.remove(
                ObjectFlags::FACE_DOWN
                    | ObjectFlags::BESTOWED
                    | ObjectFlags::PROTOTYPED
                    | ObjectFlags::TRANSFORMED
                    | ObjectFlags::FLIPPED
                    | ObjectFlags::WAS_CAST
                    | ObjectFlags::CAST_VIA_SUSPEND
                    | ObjectFlags::EVOKED
                    | ObjectFlags::ESCAPED
                    | ObjectFlags::BLITZED
                    | ObjectFlags::DASHED
                    | ObjectFlags::FORETOLD
                    | ObjectFlags::HARNESSED,
            );
        }
        if let Some(rare) = state.rare_mut(id) {
            reset_rare_on_leave_stack(rare);
        }
    }

    if from_zone == Zone::Exile && dest != Zone::Exile {
        let clear_face_down = opts.enter_face_down.is_none() && !opts.preserve_cast_state;
        if let Some(obj) = state.object_mut(id) {
            obj.flags.remove(
                ObjectFlags::IS_SUSPENDED
                    | ObjectFlags::FORETOLD
                    | ObjectFlags::UNEARTH_LEAVE_EXILE,
            );
            if clear_face_down {
                obj.flags.remove(ObjectFlags::FACE_DOWN);
            }
        }
        if let Some(rare) = state.rare_mut(id) {
            rare.exiled_by = None;
            rare.playable_from_exile_by = None;
            rare.commander_sba_declined_gen = None;
            if clear_face_down {
                rare.face_down = None;
            }
        }
    }

    if from_zone == Zone::Graveyard
        && dest != Zone::Graveyard
        && let Some(rare) = state.rare_mut(id)
    {
        rare.commander_sba_declined_gen = None;
    }

    state.remove_rare_if_default(id);

    if dest == Zone::Battlefield {
        let mut need_default_controller = None;
        let mut active_fi_for_room = 0u8;
        if let Some(obj) = state.object_mut(id) {
            obj.controller = opts.controller.unwrap_or(dest_player);
            obj.flags
                .insert(ObjectFlags::SUMMONING_SICK | ObjectFlags::ENTERED_THIS_TURN);
            if opts.enter_tapped {
                obj.flags.insert(ObjectFlags::TAPPED);
            }
            if opts.entered_transformed {
                obj.flags.insert(ObjectFlags::TRANSFORMED);
                obj.active_face_index = 1;
            }
            if opts.entered_flipped {
                obj.flags.insert(ObjectFlags::FLIPPED);
                obj.active_face_index = 1;
            }
            if opts.enter_face_down.is_some() {
                obj.flags.insert(ObjectFlags::FACE_DOWN);
            }
            active_fi_for_room = obj.active_face_index;
            if obj.controller != obj.owner || obj.has_rare() {
                need_default_controller = Some(obj.controller);
            }
        }
        if let Some(fd) = opts.enter_face_down
            && let Some(rare) = state.rare_or_default(id)
        {
            rare.face_down = Some(Arc::new(fd));
        }
        if init_room_door
            && let Some(rare) = state.rare_or_default(id)
            && !rare.unlocked_faces.contains(&active_fi_for_room)
        {
            rare.unlocked_faces.push(active_fi_for_room);
        }
        if let Some(ctrl) = need_default_controller
            && let Some(rare) = state.rare_or_default(id)
        {
            rare.default_controller = Some(ctrl);
        }

        let (active_fi, is_face_down) = state.object(id).map_or((0, false), |o| {
            let fi = state
                .rare(id)
                .and_then(|r| r.copiable.as_deref())
                .and_then(|c| c.face_index)
                .unwrap_or_else(|| o.active_face_index());
            (fi, o.is_face_down())
        });
        if !is_face_down
            && crate::faces::is_class_card(state.db(), eff_def, active_fi)
            && state.rare(id).and_then(|r| r.class_level).is_none()
            && let Some(rare) = state.rare_or_default(id)
        {
            rare.class_level = Some(1);
        }
    } else if dest == Zone::Stack
        && let Some(fd) = opts.enter_face_down
    {
        if let Some(obj) = state.object_mut(id) {
            obj.flags.insert(ObjectFlags::FACE_DOWN);
        }
        if let Some(rare) = state.rare_or_default(id) {
            rare.face_down = Some(Arc::new(fd));
        }
    }

    state.remove_rare_if_default(id);

    let new_id = if bump_gen {
        state.bump_object_generation(id).unwrap_or(id)
    } else {
        id
    };

    insert_into_zone(state, new_id, dest, dest_player, opts.position, controller);
    mark_zone_dirty(state, from_zone, dest);

    Ok(MoveRawOutcome {
        old_id: id,
        new_id: Some(new_id),
        from_zone,
        to_zone: dest,
        lki,
        ceased_to_exist: false,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum MoveObjectOutcome {
    Moved(MoveRawOutcome),
    Skipped {
        object: ObjectId,
        zone: Zone,
    },
    ExecuteInstead {
        object: ObjectId,
        zone: Zone,
        steps: Vec<mtg_ir::EffectStep>,
    },
    Paused {
        prompt_id: u32,
        chooser: PlayerId,
    },
}

/// Public zone-change entry point wrapping [`move_object_raw`] with `CR 614–616` replacement
/// effects and `CR 603` atomic `ZoneChange` events.
pub fn move_object(
    state: &mut GameState,
    id: ObjectId,
    dest: Zone,
    opts: MoveRawOptions,
    lki_eval: Option<&EvaluatedObject>,
) -> Result<MoveObjectOutcome, EngineError> {
    let (from_zone, owner, default_ctrl) = {
        let obj = state
            .object(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} does not exist")))?;
        (obj.zone, obj.owner, obj.controller)
    };
    let batch_eval = crate::events::pre_batch_lki(state, id).map(|(_, ev)| ev.clone());
    let pre_eval = lki_eval
        .cloned()
        .or(batch_eval)
        .or_else(|| state.evaluate_board().get(id).cloned());
    let target_ctrl = opts
        .controller
        .or(opts.to_player)
        .unwrap_or(if dest == Zone::Battlefield {
            default_ctrl
        } else {
            owner
        });

    let event = ReplaceableEventKind::ZoneChange {
        object: id,
        from: from_zone,
        to: dest,
        controller: target_ctrl,
    };
    let mods = ReplacementAccumulatedMods {
        move_opts: Some(opts),
        lki_eval: pre_eval,
        ..Default::default()
    };
    if dest == Zone::Battlefield
        && crate::replace::is_battlefield_entry_blocked_by_static(
            state,
            id,
            from_zone,
            target_ctrl,
            &mods,
        )
        && crate::replace::can_pre_block_battlefield_entry(state, id, from_zone, target_ctrl, &mods)
    {
        return Ok(MoveObjectOutcome::Skipped {
            object: id,
            zone: from_zone,
        });
    }
    match crate::replace::resolve_replacements(
        state,
        event,
        mods,
        ReplacementContinuation::ZoneMove,
    )? {
        ReplacementResult::Paused { prompt_id, chooser } => {
            Ok(MoveObjectOutcome::Paused { prompt_id, chooser })
        }
        ReplacementResult::Resolved(outcome) => finalize_zone_move(state, *outcome),
    }
}

pub(crate) fn finalize_zone_move(
    state: &mut GameState,
    outcome: ReplacementOutcome,
) -> Result<MoveObjectOutcome, EngineError> {
    let ReplaceableEventKind::ZoneChange {
        object,
        from,
        to,
        controller,
    } = outcome.event
    else {
        return Err(EngineError::IllegalAction(
            "expected ZoneChange replacement outcome".into(),
        ));
    };
    let mods = outcome.mods;

    if mods.skipped {
        return Ok(MoveObjectOutcome::Skipped { object, zone: from });
    }
    if mods.regenerate_instead {
        if let Some(obj) = state.object_mut(object) {
            obj.flags.insert(ObjectFlags::TAPPED);
            obj.damage_marked = 0;
            obj.deathtouch_damage_marked = false;
        }
        remove_from_combat(state, object);
        state.mark_dirty(DirtyMask::TAPPED | DirtyMask::COMBAT);
        return Ok(MoveObjectOutcome::Skipped { object, zone: from });
    }
    if let Some(steps) = mods.execute_instead {
        return Ok(MoveObjectOutcome::ExecuteInstead {
            object,
            zone: from,
            steps,
        });
    }
    if to == Zone::Battlefield
        && crate::replace::is_battlefield_entry_blocked_by_static(
            state, object, from, controller, &mods,
        )
    {
        return Ok(MoveObjectOutcome::Skipped { object, zone: from });
    }

    let from_player = state.object(object).map(|o| {
        if from == Zone::Battlefield {
            o.controller
        } else {
            o.owner
        }
    });

    let mut final_opts = mods.move_opts.unwrap_or_default();
    if let Some(tapped) = mods.enter_tapped {
        final_opts.enter_tapped = tapped;
    }
    if to == Zone::Battlefield {
        final_opts.controller = Some(controller);
    }

    let single_leave_granted = if from == Zone::Battlefield
        && to != Zone::Battlefield
        && crate::events::pre_batch_lki(state, object).is_none()
        && !mods.lki_eval.as_ref().is_some_and(|e| e.lost_all_abilities)
        && !state.object(object).is_some_and(GameObject::is_phased_out)
    {
        crate::events::capture_granted_triggers_for_object(state, object)
    } else {
        Vec::new()
    };

    let raw = move_object_raw(state, object, to, final_opts, mods.lki_eval.as_ref())?;
    let to_player = raw
        .new_id
        .and_then(|nid| state.object(nid))
        .map(|o| {
            if raw.to_zone == Zone::Battlefield {
                o.controller
            } else {
                o.owner
            }
        })
        .or(Some(controller));

    if raw.to_zone == Zone::Battlefield
        && !raw.ceased_to_exist
        && let Some(new_id) = raw.new_id
    {
        if mods.enters_prepared
            && let Some(obj) = state.object_mut(new_id)
        {
            obj.flags.insert(ObjectFlags::PREPARED);
            state.mark_dirty(DirtyMask::DESIGNATIONS);
        }
        if mods.becomes_day_if_neither && state.day_night().is_none() {
            state.set_day_night(Some(false));
        }

        let mut etb_counters = compute_intrinsic_etb_counters(state, new_id);
        for (cid, cnt) in mods.extra_counters {
            push_etb_counter(&mut etb_counters, cid, cnt);
        }
        return process_etb_counters_and_finish_move(
            state,
            raw,
            from_player,
            to_player,
            controller,
            etb_counters,
        );
    } else if raw.to_zone == Zone::Exile
        && let Some(ex_cid) = mods.exile_with_counter
        && let Some(new_id) = raw.new_id
    {
        state.add_counter(new_id, ex_cid, 1);
    }

    if mods.shuffle_into_library
        && raw.to_zone == Zone::Library
        && let Some(new_id) = raw.new_id
        && let Some(owner) = state.object(new_id).map(|o| o.owner)
    {
        shuffle_library(state, owner, 0)?;
    }

    process_etb_counters_and_finish_move_with_granted(
        state,
        raw,
        from_player,
        to_player,
        controller,
        SmallVec::new(),
        &single_leave_granted,
    )
}

fn push_etb_counter(
    counters: &mut SmallVec<[(mtg_ir::CounterId, u32); 2]>,
    cid: mtg_ir::CounterId,
    amount: u32,
) {
    if amount == 0 {
        return;
    }
    if let Some((_, existing)) = counters.iter_mut().find(|(c, _)| *c == cid) {
        *existing = existing.saturating_add(amount);
    } else {
        counters.push((cid, amount));
    }
}

fn compute_intrinsic_etb_counters(
    state: &GameState,
    id: ObjectId,
) -> SmallVec<[(mtg_ir::CounterId, u32); 2]> {
    let mut out: SmallVec<[(mtg_ir::CounterId, u32); 2]> = SmallVec::new();
    let Some(view) = crate::faces::active_face_view(state, id) else {
        return out;
    };
    if view.is_face_down {
        return out;
    }
    let eff_def = state
        .rare(id)
        .and_then(|r| r.copiable.as_deref())
        .map_or_else(
            || {
                state
                    .object(id)
                    .map_or(mtg_ir::CardDefId(0), |o| o.card_def)
            },
            |c| c.card_def,
        );
    let fi = state
        .rare(id)
        .and_then(|r| r.copiable.as_deref())
        .and_then(|c| c.face_index)
        .unwrap_or_else(|| state.object(id).map_or(0, |o| o.active_face_index()));
    if crate::faces::is_saga_card(state.db(), eff_def, fi)
        && let Some(lore_cid) = state.db().counter_id("lore")
    {
        let read_ahead = state.rare(id).and_then(|r| r.read_ahead_chapter);
        push_etb_counter(&mut out, lore_cid, u32::from(read_ahead.unwrap_or(1)));
    }
    if view.types.contains(CardTypeMask::PLANESWALKER)
        && let Some(loy) = view.loyalty.and_then(|n| u32::try_from(n).ok())
        && let Some(loy_cid) = state.db().counter_id("loyalty")
    {
        push_etb_counter(&mut out, loy_cid, loy);
    }
    if view.types.contains(CardTypeMask::BATTLE)
        && let Some(def) = view.defense.and_then(|n| u32::try_from(n).ok())
        && let Some(def_cid) = state.db().counter_id("defense")
    {
        push_etb_counter(&mut out, def_cid, def);
    }
    out
}

pub(crate) fn apply_resolved_put_counters_outcome(state: &mut GameState, out: &ReplacementOutcome) {
    if !out.mods.skipped
        && let ReplaceableEventKind::PutCounters {
            object,
            counter: final_cid,
            count: final_cnt,
            ..
        } = out.event
        && final_cnt > 0
    {
        state.add_counter(object, final_cid, final_cnt);
    }
}

pub(crate) fn process_etb_counters_and_finish_move(
    state: &mut GameState,
    raw: MoveRawOutcome,
    from_player: Option<PlayerId>,
    to_player: Option<PlayerId>,
    controller: PlayerId,
    etb_counters: SmallVec<[(mtg_ir::CounterId, u32); 2]>,
) -> Result<MoveObjectOutcome, EngineError> {
    process_etb_counters_and_finish_move_with_granted(
        state,
        raw,
        from_player,
        to_player,
        controller,
        etb_counters,
        &[],
    )
}

pub(crate) fn process_etb_counters_and_finish_move_with_granted(
    state: &mut GameState,
    raw: MoveRawOutcome,
    from_player: Option<PlayerId>,
    to_player: Option<PlayerId>,
    controller: PlayerId,
    etb_counters: SmallVec<[(mtg_ir::CounterId, u32); 2]>,
    single_leave_granted: &[crate::events::GrantedTriggerLki],
) -> Result<MoveObjectOutcome, EngineError> {
    if let Some(new_id) = raw.new_id {
        for idx in 0..etb_counters.len() {
            let (cid, cnt) = etb_counters[idx];
            if cnt == 0 {
                continue;
            }
            let remaining_counters: SmallVec<[(mtg_ir::CounterId, u32); 2]> =
                etb_counters[idx + 1..].iter().copied().collect();
            let cont = ReplacementContinuation::ZoneMoveEtbCounters(Box::new(
                crate::state::PendingEtbCounterMove {
                    raw: raw.clone(),
                    from_player,
                    to_player,
                    controller,
                    remaining_counters,
                },
            ));
            match crate::replace::resolve_put_counters_with_continuation(
                state,
                new_id,
                cid,
                cnt,
                Some(controller),
                Some(new_id),
                cont,
            )? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    return Ok(MoveObjectOutcome::Paused { prompt_id, chooser });
                }
                ReplacementResult::Resolved(out) => {
                    apply_resolved_put_counters_outcome(state, &out);
                }
            }
        }
    }

    crate::events::emit_zone_change_with_granted(
        state,
        raw.old_id,
        raw.new_id,
        raw.from_zone,
        raw.to_zone,
        from_player,
        to_player,
        raw.lki.as_ref(),
        raw.ceased_to_exist,
        single_leave_granted,
    );

    Ok(MoveObjectOutcome::Moved(raw))
}

/// Draws the top card of `player`'s library (`library.last()`) into `player`'s hand.
///
/// Returns `Ok(None)` and flags `attempted_empty_library_draw` if the library is empty (CR 704.5b).
pub fn draw_one_raw(
    state: &mut GameState,
    player: PlayerId,
) -> Result<Option<ObjectId>, EngineError> {
    let top_id = state
        .players()
        .get(usize::from(player.0))
        .ok_or_else(|| EngineError::IllegalAction(format!("invalid player {player:?}")))?
        .library
        .last()
        .copied();

    let Some(oid) = top_id else {
        if let Some(p) = state.player_mut(player) {
            p.attempted_empty_library_draw = true;
        }
        state.mark_dirty(DirtyMask::PLAYER_STATE);
        return Ok(None);
    };

    let outcome = move_object_raw(state, oid, Zone::Hand, MoveRawOptions::default(), None)?;
    Ok(outcome.new_id)
}

/// Shuffles `player`'s library while leaving the top `keep_top` cards undisturbed.
///
/// Clears `known_to_players` on shuffled cards (CR 701.20d) and reverses around `rng.shuffle`
/// so `library.last()` matches TS `shuffled[0]` bit-for-bit.
pub fn shuffle_library(
    state: &mut GameState,
    player: PlayerId,
    keep_top: usize,
) -> Result<(), EngineError> {
    let shuffle_ids: Vec<ObjectId> = {
        let p = state
            .players()
            .get(usize::from(player.0))
            .ok_or_else(|| EngineError::IllegalAction(format!("invalid player {player:?}")))?;
        let shuffle_len = p.library.len().saturating_sub(keep_top);
        p.library[..shuffle_len].to_vec()
    };

    if shuffle_ids.is_empty() {
        return Ok(());
    }

    for &oid in &shuffle_ids {
        if let Some(rare) = state.rare_mut(oid) {
            rare.known_to_players.clear();
        }
        state.remove_rare_if_default(oid);
    }

    let mut shuffled = shuffle_ids;
    shuffled.reverse();
    state.rng_mut().shuffle(&mut shuffled);
    shuffled.reverse();

    if let Some(p) = state.player_mut(player) {
        let shuffle_len = shuffled.len();
        p.library[..shuffle_len].copy_from_slice(&shuffled);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// Places `top` at the top of `player`'s library (`top[0]` at `library.last()`) and `bottom` at
/// the bottom (`bottom.last()` at `library[0]`), preserving `ObjectId` generation (same-zone reorder).
pub fn rearrange_library(
    state: &mut GameState,
    player: PlayerId,
    cards: &[ObjectId],
    top: &[ObjectId],
    bottom: &[ObjectId],
) -> Result<(), EngineError> {
    if usize::from(player.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid player {player:?}"
        )));
    }
    if top.len() + bottom.len() != cards.len() {
        return Err(EngineError::IllegalAction(
            "rearrange_library: top + bottom lengths must match cards".into(),
        ));
    }
    for &id in top.iter().chain(bottom.iter()) {
        if !cards.contains(&id) {
            return Err(EngineError::IllegalAction(format!(
                "rearrange_library: {id:?} not in target card set"
            )));
        }
    }

    if let Some(p) = state.player_mut(player) {
        p.library.retain(|id| !cards.contains(id));
        for &id in bottom {
            p.library.insert(0, id);
        }
        for &id in top.iter().rev() {
            p.library.push(id);
        }
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// Creates a shared `Arc<TokenSpec>` token directly on `controller`'s battlefield (CR 111.1, 111.2).
pub fn create_token_arc_raw(
    state: &mut GameState,
    controller: PlayerId,
    spec: Arc<TokenSpec>,
    opts: CreateTokenOptions,
) -> Result<ObjectId, EngineError> {
    if usize::from(controller.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid controller {controller:?}"
        )));
    }
    let owner = opts.owner.unwrap_or(controller);
    if usize::from(owner.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid owner {owner:?}"
        )));
    }
    let mut flags =
        ObjectFlags::IS_TOKEN | ObjectFlags::SUMMONING_SICK | ObjectFlags::ENTERED_THIS_TURN;
    if spec.tapped || opts.tapped {
        flags.insert(ObjectFlags::TAPPED);
    }
    let ts = state.allocate_timestamp();
    let rare = RareObjectState {
        token_spec: Some(spec),
        created_by_source: opts.created_by_source,
        default_controller: (controller != owner).then_some(controller),
        ..Default::default()
    };
    let mut obj = GameObject::new(TOKEN_CARD_DEF_ID, owner, Zone::Battlefield, ts, flags);
    obj.controller = controller;
    let oid = state.insert_object(obj, Some(Arc::new(rare)));
    if let Some(p) = state.player_mut(controller) {
        p.battlefield.push(oid);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(oid)
}

/// Creates a `TokenSpec` token directly on `controller`'s battlefield (CR 111.1, 111.2).
pub fn create_token_raw(
    state: &mut GameState,
    controller: PlayerId,
    spec: TokenSpec,
    opts: CreateTokenOptions,
) -> Result<ObjectId, EngineError> {
    create_token_arc_raw(state, controller, Arc::new(spec), opts)
}

/// Creates a token copy of `source` on `controller`'s battlefield (CR 707.2 / 111.3).
pub fn create_token_copy_raw(
    state: &mut GameState,
    controller: PlayerId,
    source: ObjectId,
    modifications: Option<CopyModificationSpec>,
    tapped: bool,
) -> Result<ObjectId, EngineError> {
    if usize::from(controller.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid controller {controller:?}"
        )));
    }
    let (card_def, copiable, token_spec, transformed) = {
        let src = state.object(source).ok_or_else(|| {
            EngineError::IllegalAction(format!("source {source:?} does not exist"))
        })?;
        let src_rare = state.rare(source);
        let copiable = if let Some(existing_arc) = src_rare.and_then(|r| r.copiable.as_ref()) {
            if let Some(next_mods) = modifications {
                let existing = existing_arc.as_ref();
                let composed_mods = match existing.modifications.clone() {
                    Some(base) => compose_copy_modifications(base, next_mods),
                    None => next_mods,
                };
                Arc::new(CopiableState {
                    modifications: Some(composed_mods),
                    ..existing.clone()
                })
            } else {
                Arc::clone(existing_arc)
            }
        } else {
            Arc::new(CopiableState {
                card_def: src.card_def,
                face_index: Some(src.active_face_index()),
                fused: src_rare.is_some_and(|r| r.fused),
                is_single_face_copy: src.flags.contains(ObjectFlags::TRANSFORMED)
                    || src_rare.is_some_and(|r| r.is_single_face_copy),
                prototyped: src.flags.contains(ObjectFlags::PROTOTYPED),
                merged_components: src_rare
                    .map(|r| r.merged_components.iter().map(|m| m.card_def).collect())
                    .unwrap_or_default(),
                except: None,
                modifications,
            })
        };
        let token_spec = (copiable.card_def == TOKEN_CARD_DEF_ID)
            .then(|| src_rare.and_then(|r| r.token_spec.as_ref().map(Arc::clone)))
            .flatten();
        (
            src.card_def,
            copiable,
            token_spec,
            src.flags.contains(ObjectFlags::TRANSFORMED),
        )
    };

    let mut flags =
        ObjectFlags::IS_TOKEN | ObjectFlags::SUMMONING_SICK | ObjectFlags::ENTERED_THIS_TURN;
    if tapped {
        flags.insert(ObjectFlags::TAPPED);
    }
    if transformed {
        flags.insert(ObjectFlags::TRANSFORMED);
    }
    let ts = state.allocate_timestamp();
    let active_face_index = copiable.face_index.unwrap_or(0);
    let rare = RareObjectState {
        token_spec,
        copiable: Some(copiable),
        created_by_source: Some(source),
        ..Default::default()
    };
    let mut obj = GameObject::new(card_def, controller, Zone::Battlefield, ts, flags);
    obj.active_face_index = active_face_index;
    let oid = state.insert_object(obj, Some(Arc::new(rare)));
    if let Some(p) = state.player_mut(controller) {
        p.battlefield.push(oid);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    for (cid, cnt) in compute_intrinsic_etb_counters(state, oid) {
        state.add_counter(oid, cid, cnt);
    }
    Ok(oid)
}

/// Creates a token copy on `controller`'s battlefield from an [`LkiSnapshot`] (CR 707.2 / 608.2h).
pub fn create_token_copy_from_lki_raw(
    state: &mut GameState,
    controller: PlayerId,
    lki: &LkiSnapshot,
    modifications: Option<CopyModificationSpec>,
    tapped: bool,
) -> Result<ObjectId, EngineError> {
    if usize::from(controller.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid controller {controller:?}"
        )));
    }
    let (card_def, copiable, token_spec, active_face_index) = if lki.card_def == TOKEN_CARD_DEF_ID {
        let (power, toughness) = lki.pt.map_or((0, 0), |pt| (pt.power, pt.toughness));
        let spec = TokenSpec {
            name: lki.name.clone(),
            power,
            toughness,
            colors: lki.colors,
            types: lki.types,
            supertypes: lki.supertypes,
            subtypes: lki.subtypes.iter().copied().collect(),
            type_line: String::new(),
            keywords: lki.keywords.iter().copied().collect(),
            tapped: false,
            quoted_abilities: Vec::new(),
            legendary: lki.supertypes.contains(SupertypeMask::LEGENDARY),
        };
        let copiable = Arc::new(CopiableState {
            card_def: TOKEN_CARD_DEF_ID,
            face_index: None,
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications,
        });
        (TOKEN_CARD_DEF_ID, copiable, Some(Arc::new(spec)), 0)
    } else {
        let copiable = Arc::new(CopiableState {
            card_def: lki.card_def,
            face_index: Some(lki.active_face_index),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications,
        });
        (lki.card_def, copiable, None, lki.active_face_index)
    };

    let mut flags =
        ObjectFlags::IS_TOKEN | ObjectFlags::SUMMONING_SICK | ObjectFlags::ENTERED_THIS_TURN;
    if tapped {
        flags.insert(ObjectFlags::TAPPED);
    }
    let ts = state.allocate_timestamp();
    let rare = RareObjectState {
        token_spec,
        copiable: Some(copiable),
        created_by_source: Some(lki.object_id),
        ..Default::default()
    };
    let mut obj = GameObject::new(card_def, controller, Zone::Battlefield, ts, flags);
    obj.active_face_index = active_face_index;
    let oid = state.insert_object(obj, Some(Arc::new(rare)));
    if let Some(p) = state.player_mut(controller) {
        p.battlefield.push(oid);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    for (cid, cnt) in compute_intrinsic_etb_counters(state, oid) {
        state.add_counter(oid, cid, cnt);
    }
    Ok(oid)
}

/// Removes all tokens currently in any zone other than `Zone::Battlefield` (CR 704.5d).
///
/// Phased-out tokens on `Zone::Battlefield` are untouched (CR 702.26d).
pub fn cleanup_tokens_outside_battlefield(state: &mut GameState) -> Vec<ObjectId> {
    let mut to_remove = Vec::new();
    let mut dirty = DirtyMask::empty();

    let player_count = state.players().len();
    for seat in 0..player_count {
        let pid = PlayerId(seat as u8);
        let Some(p) = state.players().get(seat) else {
            continue;
        };
        for &id in &p.library {
            if state.object(id).is_some_and(GameObject::is_token) {
                to_remove.push((id, Zone::Library, pid));
            }
        }
        for &id in &p.hand {
            if state.object(id).is_some_and(GameObject::is_token) {
                to_remove.push((id, Zone::Hand, pid));
            }
        }
        for &id in &p.graveyard {
            if state.object(id).is_some_and(GameObject::is_token) {
                to_remove.push((id, Zone::Graveyard, pid));
                dirty |= DirtyMask::GRAVEYARDS;
            }
        }
        for &id in &p.exile {
            if state.object(id).is_some_and(GameObject::is_token) {
                to_remove.push((id, Zone::Exile, pid));
                dirty |= DirtyMask::EXILE;
            }
        }
        for &id in &p.command {
            if state.object(id).is_some_and(GameObject::is_token) {
                to_remove.push((id, Zone::Command, pid));
            }
        }
    }

    for (idx, &id) in state.stack().iter().enumerate() {
        let is_spell_slot = state
            .stack_entries()
            .get(idx)
            .is_none_or(|e| matches!(e.kind, StackEntryKind::Spell { object } if object == id));
        if is_spell_slot
            && state
                .object(id)
                .is_some_and(|o| o.is_token() && o.zone == Zone::Stack)
        {
            to_remove.push((id, Zone::Stack, PlayerId(0)));
        }
    }

    if to_remove.is_empty() {
        return Vec::new();
    }

    let mut removed_ids = Vec::with_capacity(to_remove.len());
    for (id, zone, pid) in to_remove {
        match zone {
            Zone::Library => {
                if let Some(p) = state.player_mut(pid) {
                    p.library.retain(|&x| x != id);
                }
            }
            Zone::Hand => {
                if let Some(p) = state.player_mut(pid) {
                    p.hand.retain(|&x| x != id);
                }
            }
            Zone::Graveyard => {
                if let Some(p) = state.player_mut(pid) {
                    p.graveyard.retain(|&x| x != id);
                }
            }
            Zone::Exile => {
                if let Some(p) = state.player_mut(pid) {
                    p.exile.retain(|&x| x != id);
                }
            }
            Zone::Command => {
                if let Some(p) = state.player_mut(pid) {
                    p.command.retain(|&x| x != id);
                }
            }
            Zone::Stack => {
                remove_from_stack(state, id);
            }
            Zone::Battlefield => {}
        }
        state.remove_object(id);
        removed_ids.push(id);
    }

    state.mark_dirty(dirty | DirtyMask::TOPOLOGY);
    removed_ids
}

/// Phases out `id` directly and all permanents attached to it indirectly (CR 702.26d, g).
///
/// Preserves `ObjectId` generation, zone, counters, and token existence; removes from combat.
pub fn phase_out_permanent(state: &mut GameState, id: ObjectId) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.zone != Zone::Battlefield || obj.is_phased_out() {
        return false;
    }

    if let Some(obj_mut) = state.object_mut(id) {
        obj_mut.flags.insert(ObjectFlags::PHASED_OUT);
        obj_mut.flags.remove(ObjectFlags::PHASED_OUT_INDIRECTLY);
    }
    remove_from_combat(state, id);

    // BFS transitive attachments across all battlefields (indirect phasing, CR 702.26g).
    let mut queue = VecDeque::new();
    queue.push_back(id);
    while let Some(host) = queue.pop_front() {
        let attached: SmallVec<[ObjectId; 4]> = all_battlefield_ids(state)
            .into_iter()
            .filter(|&cand| {
                state
                    .object(cand)
                    .is_some_and(|o| o.attached_to == Some(host) && !o.is_phased_out())
            })
            .collect();
        for att_id in attached {
            if let Some(att) = state.object_mut(att_id) {
                att.flags
                    .insert(ObjectFlags::PHASED_OUT | ObjectFlags::PHASED_OUT_INDIRECTLY);
            }
            remove_from_combat(state, att_id);
            queue.push_back(att_id);
        }
    }

    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::COMBAT);
    true
}

/// Phases in a directly phased-out permanent `id` and any attachments that phased out indirectly
/// with it (CR 702.26g), preserving `ObjectId` generation, counters, and tokens.
pub fn phase_in_permanent(state: &mut GameState, id: ObjectId) -> Vec<ObjectId> {
    let Some(obj) = state.object(id) else {
        return Vec::new();
    };
    if obj.zone != Zone::Battlefield || !obj.is_phased_out() {
        return Vec::new();
    }

    let mut phased_in = Vec::new();
    let mut queue = VecDeque::new();
    if let Some(obj_mut) = state.object_mut(id) {
        obj_mut
            .flags
            .remove(ObjectFlags::PHASED_OUT | ObjectFlags::PHASED_OUT_INDIRECTLY);
    }
    phased_in.push(id);
    queue.push_back(id);

    while let Some(host) = queue.pop_front() {
        let attached: SmallVec<[ObjectId; 4]> = all_battlefield_ids(state)
            .into_iter()
            .filter(|&cand| {
                state
                    .object(cand)
                    .is_some_and(|o| o.attached_to == Some(host) && o.is_phased_out_indirectly())
            })
            .collect();
        for att_id in attached {
            if let Some(att) = state.object_mut(att_id) {
                att.flags
                    .remove(ObjectFlags::PHASED_OUT | ObjectFlags::PHASED_OUT_INDIRECTLY);
            }
            phased_in.push(att_id);
            queue.push_back(att_id);
        }
    }

    state.mark_dirty(DirtyMask::TOPOLOGY);
    phased_in
}

/// Phases in all directly phased-out permanents controlled by `active_player` during their untap
/// step, simultaneously phasing in indirectly phased-out attachments (CR 702.26a, g).
pub fn phase_in_untap_step(state: &mut GameState, active_player: PlayerId) -> Vec<ObjectId> {
    let direct_ids: Vec<ObjectId> = state
        .players()
        .get(usize::from(active_player.0))
        .map(|p| {
            p.battlefield
                .iter()
                .copied()
                .filter(|&id| {
                    state
                        .object(id)
                        .is_some_and(GameObject::is_phased_out_directly)
                })
                .collect()
        })
        .unwrap_or_default();

    let mut all_phased_in = Vec::new();
    for id in direct_ids {
        all_phased_in.extend(phase_in_permanent(state, id));
    }
    all_phased_in
}

pub fn reveal_to_player(state: &mut GameState, id: ObjectId, player: PlayerId) -> bool {
    let Some(rare) = state.rare_or_default(id) else {
        return false;
    };
    if !rare.known_to_players.contains(&player) {
        rare.known_to_players.push(player);
    }
    if let Some(fd) = rare.face_down.as_mut()
        && !fd.known_to.contains(&player)
    {
        Arc::make_mut(fd).known_to.push(player);
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    true
}

pub fn reveal_to_all(state: &mut GameState, id: ObjectId) -> bool {
    let pids: SmallVec<[PlayerId; 4]> = state.players().iter().map(|p| p.id).collect();
    let mut ok = false;
    for pid in pids {
        ok |= reveal_to_player(state, id, pid);
    }
    ok
}

pub fn is_known_to(state: &GameState, id: ObjectId, player: PlayerId) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if let Some(rare) = state.rare(id) {
        if rare.known_to_players.contains(&player) {
            return true;
        }
        if let Some(fd) = rare.face_down.as_deref()
            && fd.known_to.contains(&player)
        {
            return true;
        }
    }
    if obj.is_face_down() {
        // On Battlefield/Stack, controller may look at their face-down permanent/spell (CR 708.5).
        return matches!(obj.zone, Zone::Battlefield | Zone::Stack) && obj.controller == player;
    }
    match obj.zone {
        Zone::Battlefield | Zone::Graveyard | Zone::Exile | Zone::Command | Zone::Stack => true,
        Zone::Hand => obj.owner == player,
        Zone::Library => false,
    }
}

pub fn exile_face_down(
    state: &mut GameState,
    id: ObjectId,
    spec: FaceDownSpec,
    exiled_by: Option<ObjectId>,
) -> Result<MoveRawOutcome, EngineError> {
    let outcome = move_object_raw(state, id, Zone::Exile, MoveRawOptions::default(), None)?;
    if let Some(new_id) = outcome.new_id {
        if let Some(obj) = state.object_mut(new_id) {
            obj.flags.insert(ObjectFlags::FACE_DOWN);
        }
        if let Some(rare) = state.rare_or_default(new_id) {
            rare.face_down = Some(Arc::new(spec));
            rare.exiled_by = exiled_by;
        }
    }
    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE);
    Ok(outcome)
}

/// Turns a face-down permanent face-up in place (CR 708.8), preserving `ObjectId` generation.
pub fn turn_face_up_raw(state: &mut GameState, id: ObjectId) -> Result<(), EngineError> {
    let Some(obj) = state.object_mut(id) else {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} does not exist"
        )));
    };
    if !obj.is_face_down() {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} is not face down"
        )));
    }
    obj.flags.remove(ObjectFlags::FACE_DOWN);
    if let Some(rare) = state.rare_mut(id) {
        rare.face_down = None;
    }
    state.remove_rare_if_default(id);
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// Attaches `attachment` to `target` on the battlefield, bumping `attachment`'s timestamp (CR 613.7e).
pub fn attach_raw(
    state: &mut GameState,
    attachment: ObjectId,
    target: ObjectId,
) -> Result<(), EngineError> {
    if attachment == target {
        return Err(EngineError::IllegalAction(
            "an object cannot attach to itself".into(),
        ));
    }
    let att_ok = state
        .object(attachment)
        .is_some_and(|o| o.zone == Zone::Battlefield);
    let tgt_ok = state
        .object(target)
        .is_some_and(|o| o.zone == Zone::Battlefield);
    if !att_ok || !tgt_ok {
        return Err(EngineError::IllegalAction(
            "both attachment and target must be on the battlefield".into(),
        ));
    }
    let ts = state.allocate_timestamp();
    if let Some(att) = state.object_mut(attachment) {
        att.attached_to = Some(target);
        att.entered_timestamp = ts;
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// Changes control of a permanent on the battlefield without changing zones or bumping `ObjectId`
/// generation (CR 108.4 / 302.6).
pub fn transfer_control_raw(
    state: &mut GameState,
    id: ObjectId,
    new_controller: PlayerId,
) -> Result<(), EngineError> {
    if usize::from(new_controller.0) >= state.players().len() {
        return Err(EngineError::IllegalAction(format!(
            "invalid controller {new_controller:?}"
        )));
    }
    let old_controller = {
        let obj = state
            .object(id)
            .ok_or_else(|| EngineError::IllegalAction(format!("object {id:?} does not exist")))?;
        if obj.zone != Zone::Battlefield {
            return Err(EngineError::IllegalAction(format!(
                "object {id:?} is not on the battlefield"
            )));
        }
        obj.controller
    };
    if old_controller == new_controller {
        return Ok(());
    }

    if let Some(old_p) = state.player_mut(old_controller) {
        old_p.battlefield.retain(|&x| x != id);
    } else {
        for seat in 0..state.players().len() {
            if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                p.battlefield.retain(|&x| x != id);
            }
        }
    }
    if let Some(new_p) = state.player_mut(new_controller) {
        new_p.battlefield.push(id);
    }

    let ts = state.allocate_timestamp();
    if let Some(obj) = state.object_mut(id) {
        obj.controller = new_controller;
        obj.flags.insert(ObjectFlags::SUMMONING_SICK);
    }
    if let Some(rare) = state.rare_or_default(id) {
        rare.came_under_control_at = Some(ts);
        rare.default_controller = Some(new_controller);
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

// ---- Internal helpers -------------------------------------------------------------------------

fn all_battlefield_ids(state: &GameState) -> Vec<ObjectId> {
    let mut ids = Vec::new();
    for p in state.players() {
        ids.extend_from_slice(&p.battlefield);
    }
    ids
}

fn detach_all_attachments_from(state: &mut GameState, target: ObjectId) {
    let bf_ids = all_battlefield_ids(state);
    for cand in bf_ids {
        if let Some(obj) = state.object_mut(cand)
            && obj.attached_to == Some(target)
        {
            obj.attached_to = None;
        }
    }
}

fn remove_from_combat(state: &mut GameState, id: ObjectId) {
    let combat = state.combat_mut();
    combat.attackers.retain(|a| a.attacker != id);
    combat
        .blockers
        .retain(|b| b.blocker != id && b.attacker != id);
    combat.damage_assignment_order.retain(|(atk, _)| *atk != id);
    for (_, blockers) in &mut combat.damage_assignment_order {
        blockers.retain(|&b| b != id);
    }
}

fn remove_from_stack(state: &mut GameState, id: ObjectId) {
    if let Some(pos) = state
        .stack_entries()
        .iter()
        .position(|e| matches!(e.kind, StackEntryKind::Spell { object } if object == id))
        .or_else(|| {
            // Fallback only for raw stack slots without a matching StackEntry;
            // never strip Activated/Triggered ability entries whose `source == id`.
            let entries_len = state.stack_entries().len();
            state
                .stack()
                .iter()
                .enumerate()
                .position(|(idx, &x)| idx >= entries_len && x == id)
        })
    {
        state.stack_mut().remove(pos);
        if pos < state.stack_entries().len() {
            state.stack_entries_mut().remove(pos);
        }
    }
}

fn remove_from_zone(
    state: &mut GameState,
    id: ObjectId,
    from: Zone,
    owner: PlayerId,
    controller: PlayerId,
) {
    let mut removed = false;
    match from {
        Zone::Library => {
            if let Some(p) = state.player_mut(owner) {
                let before = p.library.len();
                p.library.retain(|&x| x != id);
                removed = p.library.len() != before;
            }
            if !removed {
                for seat in 0..state.players().len() {
                    if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                        p.library.retain(|&x| x != id);
                    }
                }
            }
        }
        Zone::Hand => {
            if let Some(p) = state.player_mut(owner) {
                let before = p.hand.len();
                p.hand.retain(|&x| x != id);
                removed = p.hand.len() != before;
            }
            if !removed {
                for seat in 0..state.players().len() {
                    if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                        p.hand.retain(|&x| x != id);
                    }
                }
            }
        }
        Zone::Battlefield => {
            if let Some(p) = state.player_mut(controller) {
                let before = p.battlefield.len();
                p.battlefield.retain(|&x| x != id);
                removed = p.battlefield.len() != before;
            }
            if !removed {
                for seat in 0..state.players().len() {
                    if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                        p.battlefield.retain(|&x| x != id);
                    }
                }
            }
        }
        Zone::Graveyard => {
            if let Some(p) = state.player_mut(owner) {
                let before = p.graveyard.len();
                p.graveyard.retain(|&x| x != id);
                removed = p.graveyard.len() != before;
            }
            if !removed {
                for seat in 0..state.players().len() {
                    if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                        p.graveyard.retain(|&x| x != id);
                    }
                }
            }
        }
        Zone::Exile => {
            for seat in 0..state.players().len() {
                if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                    p.exile.retain(|&x| x != id);
                }
            }
        }
        Zone::Command => {
            if let Some(p) = state.player_mut(owner) {
                let before = p.command.len();
                p.command.retain(|&x| x != id);
                removed = p.command.len() != before;
            }
            if !removed {
                for seat in 0..state.players().len() {
                    if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                        p.command.retain(|&x| x != id);
                    }
                }
            }
        }
        Zone::Stack => {
            remove_from_stack(state, id);
        }
    }
}

fn insert_into_zone(
    state: &mut GameState,
    id: ObjectId,
    dest: Zone,
    dest_player: PlayerId,
    position: ZonePosition,
    controller_hint: PlayerId,
) {
    match dest {
        Zone::Library => {
            if let Some(p) = state.player_mut(dest_player) {
                match position {
                    ZonePosition::Default | ZonePosition::Top => p.library.push(id),
                    ZonePosition::Bottom => p.library.insert(0, id),
                    ZonePosition::FromTop(n) => {
                        let idx = p.library.len().saturating_sub(n);
                        p.library.insert(idx, id);
                    }
                }
            }
        }
        Zone::Hand => {
            if let Some(p) = state.player_mut(dest_player) {
                if matches!(position, ZonePosition::Bottom) {
                    p.hand.insert(0, id);
                } else {
                    p.hand.push(id);
                }
            }
        }
        Zone::Battlefield => {
            if let Some(p) = state.player_mut(dest_player) {
                p.battlefield.push(id);
            }
        }
        Zone::Graveyard => {
            if let Some(p) = state.player_mut(dest_player) {
                if matches!(position, ZonePosition::Bottom) {
                    p.graveyard.insert(0, id);
                } else {
                    p.graveyard.push(id);
                }
            }
        }
        Zone::Exile => {
            if let Some(p) = state.player_mut(dest_player) {
                p.exile.push(id);
            }
        }
        Zone::Command => {
            if let Some(p) = state.player_mut(dest_player) {
                p.command.push(id);
            }
        }
        Zone::Stack => {
            let stack_id = state.allocate_stack_id();
            let obj_controller = state.object(id).map_or(controller_hint, |o| o.controller);
            let rare = state.rare(id);
            let paid_mana_colors = rare.map_or(ColorMask::empty(), |r| r.paid_mana_colors);
            let x_value = rare.and_then(|r| r.entered_with_x).unwrap_or(0);
            state.stack_mut().push(id);
            state.stack_entries_mut().push(StackEntry {
                id: stack_id,
                kind: StackEntryKind::Spell { object: id },
                controller: obj_controller,
                choices: StackChoices::default(),
                targets: Vec::new(),
                x_value,
                paid_mana_colors,
            });
        }
    }
}

fn mark_zone_dirty(state: &mut GameState, from: Zone, dest: Zone) {
    let mut dirty = DirtyMask::TOPOLOGY;
    if from == Zone::Battlefield && dest != Zone::Battlefield {
        dirty |= DirtyMask::COMBAT;
    }
    if from == Zone::Graveyard || dest == Zone::Graveyard {
        dirty |= DirtyMask::GRAVEYARDS;
    }
    if from == Zone::Exile || dest == Zone::Exile {
        dirty |= DirtyMask::EXILE;
    }
    state.mark_dirty(dirty);
}

fn reset_rare_on_ltb(rare: &mut RareObjectState, keep_counters_non_hidden: bool) {
    rare.unlocked_faces.clear();
    rare.class_level = None;
    rare.fused = false;
    rare.is_single_face_copy = false;
    rare.prototyped_face = None;
    rare.read_ahead_chapter = None;
    rare.melded_components = None;
    rare.merged_components.clear();
    rare.copiable = None;
    rare.face_down = None;
    rare.entered_with_x = None;
    rare.cast_from_zone = None;
    rare.paid_mana_colors = ColorMask::empty();
    rare.mana_spent_count = 0;
    rare.kicked_count = 0;
    rare.chosen_color = None;
    rare.chosen_subtype = None;
    rare.chosen_card_name = None;
    rare.chosen_player = None;
    rare.chosen_card_type = None;
    rare.chosen_number = None;
    rare.chosen_object = None;
    rare.until_source_leaves_return = None;
    rare.protector = None;
    rare.default_controller = None;
    rare.temp_power_mod = 0;
    rare.temp_toughness_mod = 0;
    rare.granted_keywords.clear();
    if !keep_counters_non_hidden {
        rare.counter_timestamps.clear();
    }
    rare.came_under_control_at = None;
    rare.goaded_by.clear();
    rare.used_loyalty_this_turn = false;
    rare.doesnt_untap_next_untap = false;
    rare.switched_pt_count = 0;
    rare.created_by_source = None;
    rare.chosen_colors.clear();
    rare.chosen_players.clear();
    rare.chosen_anchor_word = None;
    rare.chosen_odd_parity = None;
    rare.dealt_damage_exile_sources.clear();
    rare.exile_instead_of_dying_this_turn = false;
}

fn reset_rare_on_leave_stack(rare: &mut RareObjectState) {
    rare.fused = false;
    rare.is_single_face_copy = false;
    rare.prototyped_face = None;
    rare.copiable = None;
    rare.face_down = None;
    rare.entered_with_x = None;
    rare.cast_from_zone = None;
    rare.paid_mana_colors = ColorMask::empty();
    rare.mana_spent_count = 0;
    rare.kicked_count = 0;
}

fn halve_copy_pt(v: i32, round: RoundMode) -> i32 {
    match round {
        RoundMode::Up => (v + 1).div_euclid(2),
        RoundMode::Down => v.div_euclid(2),
    }
}

fn compose_copy_modifications(
    mut base: CopyModificationSpec,
    next: CopyModificationSpec,
) -> CopyModificationSpec {
    let (name, override_name) = if let Some(ov) = next.override_name {
        (next.name.or(base.name), Some(ov))
    } else if next.name.is_some() && !next.keep_self_name {
        (next.name, None)
    } else {
        (base.name, base.override_name)
    };

    let (set_types, add_types) = if next.set_types.is_some() {
        (next.set_types, next.add_types)
    } else {
        (base.set_types, base.add_types | next.add_types)
    };

    let add_supertypes = (base.add_supertypes & !next.remove_supertypes) | next.add_supertypes;
    let remove_supertypes =
        (base.remove_supertypes & !next.add_supertypes) | next.remove_supertypes;

    let (set_subtypes, add_subtypes) = if next.set_subtypes.is_some() {
        (next.set_subtypes, next.add_subtypes)
    } else {
        let mut merged = base.add_subtypes;
        for st in next.add_subtypes {
            if !merged.contains(&st) {
                merged.push(st);
            }
        }
        (base.set_subtypes, merged)
    };

    let (set_colors, add_colors) = if next.set_colors.is_some() {
        (next.set_colors, next.add_colors)
    } else {
        (base.set_colors, base.add_colors | next.add_colors)
    };

    let mut add_keywords = base.add_keywords;
    for kw in next.add_keywords {
        if !add_keywords.contains(&kw) {
            add_keywords.push(kw);
        }
    }

    if let Some(base_round) = base.halve_pt
        && (next.power.is_some() || next.toughness.is_some() || next.halve_pt.is_some())
    {
        base.power = base.power.map(|p| halve_copy_pt(p, base_round));
        base.toughness = base.toughness.map(|t| halve_copy_pt(t, base_round));
    }
    let power = next.power.or(base.power);
    let toughness = next.toughness.or(base.toughness);
    let halve_pt = if next.halve_pt.is_some() {
        next.halve_pt
    } else if next.power.is_none() && next.toughness.is_none() {
        base.halve_pt
    } else {
        None
    };

    let mut added_ability_texts = base.added_ability_texts;
    added_ability_texts.extend(next.added_ability_texts);

    CopyModificationSpec {
        name,
        keep_self_name: next.keep_self_name,
        override_name,
        power,
        toughness,
        set_colors,
        add_colors,
        add_types,
        set_types,
        add_subtypes,
        set_subtypes,
        add_supertypes,
        remove_supertypes,
        add_keywords,
        no_mana_cost: base.no_mana_cost || next.no_mana_cost,
        added_ability_texts,
        halve_pt,
        keep_this_ability: base.keep_this_ability || next.keep_this_ability,
        additional_counters: next.additional_counters,
        conditional_counters: next.conditional_counters,
        conditional_type_counters: next.conditional_type_counters,
    }
}

fn has_counters_remain_in_non_hidden_zones(state: &GameState, card_def: mtg_ir::CardDefId) -> bool {
    let Some(card) = state.db().get(card_def) else {
        return false;
    };
    card.faces.iter().any(|face| {
        face.abilities.iter().any(|ab| {
            matches!(
                ab,
                AbilityDef::Static(st)
                    if matches!(st.static_def.body, StaticBody::CountersRemainInNonHiddenZones)
            )
        })
    })
}

fn base_types_for_object(
    state: &GameState,
    id: ObjectId,
    entered_transformed: bool,
) -> CardTypeMask {
    let Some(obj) = state.object(id) else {
        return CardTypeMask::empty();
    };
    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    if !obj.is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        return evaluate_token_spec_base(spec, copiable).1.types;
    }
    let Some(mut opts) = crate::faces::face_opts_for_object(state, id) else {
        return CardTypeMask::empty();
    };
    opts.zone = Zone::Battlefield;
    if entered_transformed {
        opts.face_index = 1;
    }
    crate::faces::resolve_object_faces(state.db(), obj.card_def, &opts)
        .map_or(CardTypeMask::empty(), |v| v.types)
}

fn capture_lki_snapshot(
    state: &GameState,
    id: ObjectId,
    lki_eval: Option<&EvaluatedObject>,
) -> LkiSnapshot {
    let obj = state.object(id).expect("object checked before capture");
    let mut attached_ids: SmallVec<[ObjectId; 2]> = SmallVec::new();
    if obj.zone == Zone::Battlefield {
        for p in state.players() {
            for &cand in &p.battlefield {
                if state
                    .object(cand)
                    .is_some_and(|o| o.attached_to == Some(id))
                {
                    attached_ids.push(cand);
                }
            }
        }
    }

    let rare = state.rare(id);
    let active_face_index = obj.active_face_index();
    let copiable = rare.and_then(|r| r.copiable.as_deref());

    let (name, base_pt, base_types, base_subtypes, base_supertypes, base_colors, base_mv, base_kws) =
        if !obj.is_face_down()
            && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
            && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
        {
            let (eff_name, eval) = evaluate_token_spec_base(spec, copiable);
            (
                eff_name,
                eval.pt,
                eval.types,
                eval.subtypes,
                eval.supertypes,
                eval.colors,
                eval.mana_value,
                eval.keywords,
            )
        } else if let Some(opts) = crate::faces::face_opts_for_object(state, id)
            && let Some(resolved) =
                crate::faces::resolve_object_faces(state.db(), obj.card_def, &opts)
        {
            let mut kws: SmallVec<[KeywordId; 4]> =
                resolved.extra_keywords.iter().copied().collect();
            for ab_ref in crate::faces::active_abilities(state.db(), obj.card_def, &opts) {
                if let AbilityDef::Keyword(k) = ab_ref.ability
                    && !kws.contains(&k.keyword.keyword)
                {
                    kws.push(k.keyword.keyword);
                }
            }
            (
                resolved.name,
                resolved.pt,
                resolved.types,
                resolved.subtypes,
                resolved.supertypes,
                resolved.colors,
                resolved.mana_value,
                kws,
            )
        } else {
            (
                String::new(),
                None,
                CardTypeMask::empty(),
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                0,
                SmallVec::new(),
            )
        };

    LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name,
        owner: obj.owner,
        controller: obj.controller,
        last_zone: obj.zone,
        pt: lki_eval.map_or(base_pt, |e| e.pt),
        types: lki_eval.map_or(base_types, |e| e.types),
        subtypes: lki_eval.map_or(base_subtypes, |e| e.subtypes.clone()),
        supertypes: lki_eval.map_or(base_supertypes, |e| e.supertypes),
        colors: lki_eval.map_or(base_colors, |e| e.colors),
        mana_value: lki_eval.map_or(base_mv, |e| e.mana_value),
        keywords: lki_eval.map_or(base_kws, |e| e.keywords.clone()),
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.damage_marked,
        is_token: obj.is_token(),
        is_commander: obj.is_commander(),
        tapped: obj.flags.contains(ObjectFlags::TAPPED),
        phased_out: obj.is_phased_out(),
        face_down: obj.is_face_down(),
        lost_all_abilities: lki_eval.is_some_and(|e| e.lost_all_abilities),
        has_all_creature_types: lki_eval.is_some_and(|e| e.has_all_creature_types),
        attached_ids,
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::{GameConfig, RngMode};
    use crate::state::FaceDownKind;
    use crate::view::PowerToughness;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, CounterId, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, PrintedPt, StaticAbility, StaticDef,
        SubtypeId,
    };

    fn make_test_db() -> Arc<CardDatabase> {
        let cards = vec![
            // 0: Creature
            CardDef {
                oracle_id: "bear".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Bear".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Bear".into(),
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
            },
            // 1: Sorcery
            CardDef {
                oracle_id: "bolt_sorcery".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Sorcery".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Sorcery".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{R}"),
                        types: CardTypeMask::SORCERY,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::R,
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
            // 2: Skullbriar-style creature (CountersRemainInNonHiddenZones)
            CardDef {
                oracle_id: "skull".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "Skull".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Skull".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{B}{G}"),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::LEGENDARY,
                        colors: ColorMask::B | ColorMask::G,
                        color_indicator: None,
                        pt: Some(PrintedPt::Fixed {
                            power: 1,
                            toughness: 1,
                        }),
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::CountersRemainInNonHiddenZones,
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
            },
            // 3: Class enchantment
            CardDef {
                oracle_id: "class_ench".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Synth Study Class".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Study Class".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}{U}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: smallvec::smallvec![SubtypeId(1)],
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::U,
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
        ];
        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Goblin".into(), "Class".into()],
                    counters: vec!["+1/+1".into()],
                    keywords: vec!["flying".into()],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn boot_state(db: Arc<CardDatabase>, main_card: CardDefId) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![main_card; 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.rng_mode = RngMode::Mulberry32Compat;
        GameState::new(db, cfg, &[deck.clone(), deck], 123).unwrap()
    }

    #[test]
    fn cr_400_7_and_400_7d_generation_bumps_and_ltb_resets() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let hand_id = state.players()[0].hand[0];
        assert_eq!(hand_id.generation, 1);

        // Hand -> Stack bumps generation (CR 400.7).
        let to_stack = move_object_raw(
            &mut state,
            hand_id,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let stack_id = to_stack.new_id.unwrap();
        assert!(stack_id.generation > hand_id.generation);
        assert!(state.object(hand_id).is_none());
        assert_eq!(state.stack(), &[stack_id]);
        assert_eq!(state.stack_entries().len(), 1);

        // Stack -> Battlefield preserves exact ObjectId (CR 400.7d).
        let to_bf = move_object_raw(
            &mut state,
            stack_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let bf_id = to_bf.new_id.unwrap();
        assert_eq!(bf_id, stack_id);
        assert!(state.stack().is_empty());
        assert!(state.stack_entries().is_empty());

        // Set transient battlefield state: tapped, damage, counter, chosen color.
        if let Some(obj) = state.object_mut(bf_id) {
            obj.flags.insert(ObjectFlags::TAPPED);
            obj.damage_marked = 1;
        }
        state.add_counter(bf_id, CounterId(0), 3);
        state.rare_or_default(bf_id).unwrap().chosen_color = Some(ColorMask::R);

        // Attach a second permanent to bf_id.
        let att_hand = state.players()[0].hand[0];
        let att_bf = move_object_raw(
            &mut state,
            att_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        attach_raw(&mut state, att_bf, bf_id).unwrap();
        assert_eq!(state.object(att_bf).unwrap().attached_to, Some(bf_id));

        // Battlefield -> Graveyard bumps generation, captures LKI, detaches attachments, and resets transient state.
        let to_gy = move_object_raw(
            &mut state,
            bf_id,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let gy_id = to_gy.new_id.unwrap();
        assert!(gy_id.generation > bf_id.generation);
        assert!(state.object(att_bf).unwrap().attached_to.is_none());

        let lki = to_gy.lki.unwrap();
        assert_eq!(lki.object_id, bf_id);
        assert_eq!(lki.counters.as_slice(), &[(CounterId(0), 3)]);
        assert!(lki.tapped);
        assert_eq!(lki.attached_ids.as_slice(), &[att_bf]);
        assert_eq!(lki.chosen_color, Some(ColorMask::R));

        let gy_obj = state.object(gy_id).unwrap();
        assert!(!gy_obj.flags.contains(ObjectFlags::TAPPED));
        assert_eq!(gy_obj.damage_marked, 0);
        assert!(state.counters(gy_id).is_empty());
        assert_eq!(state.rare(gy_id).and_then(|r| r.chosen_color), None);
    }

    #[test]
    fn cr_304_4_sorcery_cannot_enter_battlefield_and_counters_remain_in_non_hidden_zones() {
        let db = make_test_db();
        let mut state = boot_state(db.clone(), CardDefId(1));
        let sorc_hand = state.players()[0].hand[0];
        let res = move_object_raw(
            &mut state,
            sorc_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert_eq!(res.new_id, Some(sorc_hand));
        assert_eq!(res.to_zone, Zone::Hand);
        assert_eq!(state.object(sorc_hand).unwrap().zone, Zone::Hand);

        // Skullbriar preserves counters to Graveyard (non-hidden) but loses them to Hand (hidden).
        let mut sk_state = boot_state(db, CardDefId(2));
        let sk_hand = sk_state.players()[0].hand[0];
        let sk_bf = move_object_raw(
            &mut sk_state,
            sk_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        sk_state.add_counter(sk_bf, CounterId(0), 2);

        let sk_gy = move_object_raw(
            &mut sk_state,
            sk_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(sk_state.counters(sk_gy), &[(CounterId(0), 2)]);

        let sk_bf2 = move_object_raw(
            &mut sk_state,
            sk_gy,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let sk_hand2 = move_object_raw(
            &mut sk_state,
            sk_bf2,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(sk_state.counters(sk_hand2).is_empty());
    }

    #[test]
    fn shuffle_library_preserves_keep_top_clears_known_and_draws_from_top() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));
        let top_before = *state.players()[0].library.last().unwrap();
        let second_before = state.players()[0].library[state.players()[0].library.len() - 2];

        reveal_to_player(&mut state, top_before, PlayerId(1));
        reveal_to_player(&mut state, second_before, PlayerId(1));
        assert!(is_known_to(&state, top_before, PlayerId(1)));
        assert!(is_known_to(&state, second_before, PlayerId(1)));

        shuffle_library(&mut state, PlayerId(0), 1).unwrap();
        // Top card stayed on top and remained known; shuffled cards cleared known_to_players.
        assert_eq!(state.players()[0].library.last().copied(), Some(top_before));
        assert!(is_known_to(&state, top_before, PlayerId(1)));
        assert!(!is_known_to(&state, second_before, PlayerId(1)));

        let drawn = draw_one_raw(&mut state, PlayerId(0)).unwrap().unwrap();
        assert!(drawn.generation > top_before.generation);
        assert_eq!(drawn.index, top_before.index);
        assert!(state.players()[0].hand.contains(&drawn));
    }

    #[test]
    fn tokens_cease_to_exist_outside_battlefield_cr_704_5d_and_copy_tokens_work() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let spec = TokenSpec {
            name: "Goblin".into(),
            power: 1,
            toughness: 1,
            colors: ColorMask::R,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: smallvec::smallvec![SubtypeId(0)],
            type_line: "Creature — Goblin".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };
        let tok =
            create_token_raw(&mut state, PlayerId(0), spec, CreateTokenOptions::default()).unwrap();
        assert_eq!(state.object(tok).unwrap().card_def, TOKEN_CARD_DEF_ID);
        assert!(state.object(tok).unwrap().is_token());

        let tok_copy = create_token_copy_raw(&mut state, PlayerId(1), tok, None, true).unwrap();
        assert!(state.object(tok_copy).unwrap().is_token());
        assert!(
            state
                .object(tok_copy)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );

        // Move tok off battlefield -> still exists in graveyard for triggers until SBA cleanup.
        let tok_gy = move_object_raw(
            &mut state,
            tok,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(state.object(tok_gy).is_some());
        assert!(state.players()[0].graveyard.contains(&tok_gy));

        let removed = cleanup_tokens_outside_battlefield(&mut state);
        assert_eq!(removed, vec![tok_gy]);
        assert!(state.object(tok_gy).is_none());
        assert!(!state.players()[0].graveyard.contains(&tok_gy));
        // tok_copy on battlefield remains alive.
        assert!(state.object(tok_copy).is_some());
    }

    #[test]
    fn phasing_preserves_id_counters_tokens_and_handles_indirect_attachments() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let host_hand = state.players()[0].hand[0];
        let host = move_object_raw(
            &mut state,
            host_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_counter(host, CounterId(0), 2);

        let opp_aura_hand = state.players()[1].hand[0];
        let opp_aura = move_object_raw(
            &mut state,
            opp_aura_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        attach_raw(&mut state, opp_aura, host).unwrap();

        assert!(phase_out_permanent(&mut state, host));
        assert!(state.object(host).unwrap().is_phased_out_directly());
        assert!(state.object(opp_aura).unwrap().is_phased_out_indirectly());

        // SBA token cleanup never removes phased-out battlefield tokens.
        assert!(cleanup_tokens_outside_battlefield(&mut state).is_empty());

        // Opponent untap step must NOT phase in opp_aura while its host is directly phased out.
        assert!(phase_in_untap_step(&mut state, PlayerId(1)).is_empty());
        assert!(state.object(opp_aura).unwrap().is_phased_out());

        // Host controller untap step phases both back in simultaneously with unchanged ObjectIds & counters.
        let phased_in = phase_in_untap_step(&mut state, PlayerId(0));
        assert_eq!(phased_in, vec![host, opp_aura]);
        assert!(!state.object(host).unwrap().is_phased_out());
        assert!(!state.object(opp_aura).unwrap().is_phased_out());
        assert_eq!(state.counters(host), &[(CounterId(0), 2)]);
    }

    #[test]
    fn face_down_turn_up_and_transfer_control_preserve_generation() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));
        let hand_id = state.players()[0].hand[0];
        let fd_spec = FaceDownSpec {
            kind: FaceDownKind::Morph,
            opaque_seq: 1,
            ..Default::default()
        };
        let bf_id = move_object_raw(
            &mut state,
            hand_id,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(fd_spec),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        assert!(state.object(bf_id).unwrap().is_face_down());
        assert!(is_known_to(&state, bf_id, PlayerId(0)));
        assert!(!is_known_to(&state, bf_id, PlayerId(1)));

        turn_face_up_raw(&mut state, bf_id).unwrap();
        assert!(!state.object(bf_id).unwrap().is_face_down());
        assert!(is_known_to(&state, bf_id, PlayerId(1)));

        transfer_control_raw(&mut state, bf_id, PlayerId(1)).unwrap();
        assert_eq!(state.object(bf_id).unwrap().controller, PlayerId(1));
        assert!(state.players()[1].battlefield.contains(&bf_id));
        assert!(!state.players()[0].battlefield.contains(&bf_id));
    }

    #[test]
    fn rearrange_library_and_exile_face_down_preserve_top_bottom_and_redaction() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));
        let lib = &state.players()[0].library;
        let len = lib.len();
        let c0 = lib[len - 1];
        let c1 = lib[len - 2];
        let c2 = lib[len - 3];

        // Put c1 on top, c2 below c1 on top, and c0 on bottom.
        rearrange_library(&mut state, PlayerId(0), &[c0, c1, c2], &[c1, c2], &[c0]).unwrap();
        let lib_after = &state.players()[0].library;
        assert_eq!(lib_after[lib_after.len() - 1], c1);
        assert_eq!(lib_after[lib_after.len() - 2], c2);
        assert_eq!(lib_after[0], c0);
        // Same-zone reorder preserves generation.
        assert_eq!(c1.generation, 1);

        let hand_id = state.players()[0].hand[0];
        let ex_out = exile_face_down(
            &mut state,
            hand_id,
            FaceDownSpec {
                kind: FaceDownKind::ExiledFaceDown,
                opaque_seq: 9,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let ex_id = ex_out.new_id.unwrap();
        assert!(state.object(ex_id).unwrap().is_face_down());
        assert!(!is_known_to(&state, ex_id, PlayerId(0)));
        assert!(!is_known_to(&state, ex_id, PlayerId(1)));
        reveal_to_player(&mut state, ex_id, PlayerId(0));
        assert!(is_known_to(&state, ex_id, PlayerId(0)));
        assert!(!is_known_to(&state, ex_id, PlayerId(1)));
    }

    #[test]
    fn cleanup_tokens_outside_battlefield_preserves_battlefield_token_with_stack_ability() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let goblin_spec = || TokenSpec {
            name: "Goblin".into(),
            power: 1,
            toughness: 1,
            colors: ColorMask::R,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: smallvec::smallvec![SubtypeId(0)],
            type_line: "Creature — Goblin".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };

        // 1) Live battlefield token with an Activated ability on the stack.
        let tok_bf = create_token_raw(
            &mut state,
            PlayerId(0),
            goblin_spec(),
            CreateTokenOptions::default(),
        )
        .unwrap();
        let bf_lki = Arc::new(capture_lki_snapshot(&state, tok_bf, None));
        let act_sid = state.allocate_stack_id();
        state.stack_mut().push(tok_bf);
        state.stack_entries_mut().push(StackEntry {
            id: act_sid,
            kind: StackEntryKind::Activated {
                source: tok_bf,
                ability_idx: 0,
                lki: bf_lki,
            },
            controller: PlayerId(0),
            choices: StackChoices::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });

        // 2) Graveyard token that also has a Triggered (dies) ability on the stack.
        let tok_dead_bf = create_token_raw(
            &mut state,
            PlayerId(0),
            goblin_spec(),
            CreateTokenOptions::default(),
        )
        .unwrap();
        let tok_gy = move_object_raw(
            &mut state,
            tok_dead_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let gy_lki = Arc::new(capture_lki_snapshot(&state, tok_gy, None));
        let trig_sid = state.allocate_stack_id();
        state.stack_mut().push(tok_gy);
        state.stack_entries_mut().push(StackEntry {
            id: trig_sid,
            kind: StackEntryKind::Triggered {
                source: tok_gy,
                ability_idx: 0,
                lki: gy_lki,
            },
            controller: PlayerId(0),
            choices: StackChoices::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });

        // 3) Token physically in Zone::Stack as a Spell entry (must be cleaned up).
        let tok_3 = create_token_raw(
            &mut state,
            PlayerId(0),
            goblin_spec(),
            CreateTokenOptions::default(),
        )
        .unwrap();
        let tok_stack = move_object_raw(
            &mut state,
            tok_3,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let removed = cleanup_tokens_outside_battlefield(&mut state);
        assert_eq!(removed, vec![tok_gy, tok_stack]);

        // Live battlefield token + its activated ability remain intact.
        assert!(state.object(tok_bf).is_some());
        assert_eq!(state.object(tok_bf).unwrap().zone, Zone::Battlefield);
        assert!(state.players()[0].battlefield.contains(&tok_bf));

        // Graveyard token & Zone::Stack spell token are removed; triggered ability stays on stack.
        assert!(state.object(tok_gy).is_none());
        assert!(!state.players()[0].graveyard.contains(&tok_gy));
        assert!(state.object(tok_stack).is_none());
        assert_eq!(state.stack(), &[tok_bf, tok_gy]);
        assert_eq!(state.stack_entries().len(), 2);

        // Hardened remove_from_stack does not remove non-Spell ability entries.
        remove_from_stack(&mut state, tok_bf);
        assert_eq!(state.stack(), &[tok_bf, tok_gy]);
        assert_eq!(state.stack_entries().len(), 2);
    }

    #[test]
    fn leaving_exile_clears_face_down_unless_reentering_face_down() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let fd_spec = || FaceDownSpec {
            kind: FaceDownKind::ExiledFaceDown,
            opaque_seq: 1,
            ..Default::default()
        };

        let h0 = state.players()[0].hand[0];
        let h1 = state.players()[0].hand[1];
        let h2 = state.players()[0].hand[2];
        let h3 = state.players()[0].hand[3];

        let ex0 = exile_face_down(&mut state, h0, fd_spec(), None)
            .unwrap()
            .new_id
            .unwrap();
        let gy0 = move_object_raw(
            &mut state,
            ex0,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!state.object(gy0).unwrap().is_face_down());
        assert!(state.rare(gy0).and_then(|r| r.face_down.as_ref()).is_none());

        let ex1 = exile_face_down(&mut state, h1, fd_spec(), None)
            .unwrap()
            .new_id
            .unwrap();
        let hand1 = move_object_raw(&mut state, ex1, Zone::Hand, MoveRawOptions::default(), None)
            .unwrap()
            .new_id
            .unwrap();
        assert!(!state.object(hand1).unwrap().is_face_down());
        assert!(
            state
                .rare(hand1)
                .and_then(|r| r.face_down.as_ref())
                .is_none()
        );

        // Exile (face-down) -> Battlefield (face-up) clears FACE_DOWN and restores Green Bear stats.
        let ex2 = exile_face_down(&mut state, h2, fd_spec(), None)
            .unwrap()
            .new_id
            .unwrap();
        let bf2 = move_object_raw(
            &mut state,
            ex2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!state.object(bf2).unwrap().is_face_down());
        assert!(state.rare(bf2).and_then(|r| r.face_down.as_ref()).is_none());
        assert_eq!(
            state.evaluate_board().get(bf2).unwrap().colors,
            ColorMask::G
        );

        // Exile (face-down) -> Battlefield (explicitly face-down Cloak) preserves face-down status.
        let ex3 = exile_face_down(&mut state, h3, fd_spec(), None)
            .unwrap()
            .new_id
            .unwrap();
        let bf3 = move_object_raw(
            &mut state,
            ex3,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Cloak,
                    opaque_seq: 99,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(state.object(bf3).unwrap().is_face_down());
        assert_eq!(
            state
                .rare(bf3)
                .and_then(|r| r.face_down.as_ref())
                .unwrap()
                .kind,
            FaceDownKind::Cloak
        );
    }

    #[test]
    fn token_spec_and_copiable_precedence_and_composition() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let spec = TokenSpec {
            name: "Legendary Goblin".into(),
            power: 1,
            toughness: 1,
            colors: ColorMask::R,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: smallvec::smallvec![SubtypeId(0)],
            type_line: "Legendary Creature — Goblin".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: true,
        };
        let tok =
            create_token_raw(&mut state, PlayerId(0), spec, CreateTokenOptions::default()).unwrap();

        // 1) Token copy with CopyModificationSpec applies on top of TokenSpec.
        let mod1 = CopyModificationSpec {
            add_types: CardTypeMask::ARTIFACT,
            add_colors: ColorMask::U,
            remove_supertypes: SupertypeMask::LEGENDARY,
            add_keywords: smallvec::smallvec![KeywordId(0)],
            power: Some(4),
            toughness: Some(4),
            ..Default::default()
        };
        let copy1 = create_token_copy_raw(&mut state, PlayerId(0), tok, Some(mod1), false).unwrap();
        let ev1 = state.evaluate_board().get(copy1).unwrap().clone();
        assert_eq!(ev1.types, CardTypeMask::CREATURE | CardTypeMask::ARTIFACT);
        assert_eq!(ev1.colors, ColorMask::R | ColorMask::U);
        assert_eq!(ev1.supertypes, SupertypeMask::empty());
        assert_eq!(ev1.keywords.as_slice(), &[KeywordId(0)]);
        assert_eq!(
            ev1.pt,
            Some(PowerToughness {
                power: 4,
                toughness: 4
            })
        );

        // 2) Copying the modified copy composes both modifications (4/4 halved -> 2/2, keeps ARTIFACT + U).
        let mod2 = CopyModificationSpec {
            add_types: CardTypeMask::ENCHANTMENT,
            halve_pt: Some(RoundMode::Down),
            ..Default::default()
        };
        let copy2 =
            create_token_copy_raw(&mut state, PlayerId(0), copy1, Some(mod2), false).unwrap();
        let ev2 = state.evaluate_board().get(copy2).unwrap().clone();
        assert_eq!(
            ev2.types,
            CardTypeMask::CREATURE | CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT
        );
        assert_eq!(ev2.colors, ColorMask::R | ColorMask::U);
        assert_eq!(ev2.supertypes, SupertypeMask::empty());
        assert_eq!(ev2.keywords.as_slice(), &[KeywordId(0)]);
        assert_eq!(
            ev2.pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );

        // LKI capture also reflects composed TokenSpec + copiable modifications.
        let copy2_gy = move_object_raw(
            &mut state,
            copy2,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let lki = copy2_gy.lki.unwrap();
        assert_eq!(
            lki.types,
            CardTypeMask::CREATURE | CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT
        );
        assert_eq!(lki.colors, ColorMask::R | ColorMask::U);
        assert_eq!(
            lki.pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );

        // 3) Clone-style CopiableState with CopyExceptions over TokenSpec (`card_def == TOKEN_CARD_DEF_ID`).
        {
            let rare = state.rare_or_default(copy1).unwrap();
            let mut updated = rare.copiable.as_ref().unwrap().as_ref().clone();
            updated.modifications = None;
            updated.except = Some(crate::state::CopyExceptions {
                pt: Some(PowerToughness {
                    power: 5,
                    toughness: 5,
                }),
                ..Default::default()
            });
            rare.copiable = Some(Arc::new(updated));
        }
        state.mark_dirty(DirtyMask::TOPOLOGY);
        assert_eq!(
            state.evaluate_board().get(copy1).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );

        // 4) Real CardDef in copiable takes precedence over any residual token_spec.
        state.rare_or_default(tok).unwrap().copiable = Some(Arc::new(CopiableState {
            card_def: CardDefId(0), // 2/2 Green Bear
            face_index: Some(0),
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: None,
            modifications: None,
        }));
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let tok_as_bear = state.evaluate_board().get(tok).unwrap();
        assert_eq!(tok_as_bear.colors, ColorMask::G);
        assert_eq!(
            tok_as_bear.pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
    }

    #[test]
    fn leaving_battlefield_marks_combat_dirty_and_leaving_stack_clears_cast_flags() {
        let db = make_test_db();
        let mut state = boot_state(Arc::clone(&db), CardDefId(0));

        // Move card to Stack, tag all cast-modifier flags, then move Stack -> Graveyard.
        let hand0 = state.players()[0].hand[0];
        let stack0 = move_object_raw(
            &mut state,
            hand0,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let cast_flags = ObjectFlags::EVOKED
            | ObjectFlags::ESCAPED
            | ObjectFlags::BLITZED
            | ObjectFlags::DASHED
            | ObjectFlags::FORETOLD
            | ObjectFlags::HARNESSED;
        state.object_mut(stack0).unwrap().flags.insert(cast_flags);

        let gy0 = move_object_raw(
            &mut state,
            stack0,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!state.object(gy0).unwrap().flags.intersects(cast_flags));

        // Enter Battlefield -> reset dirty_since_eval via snapshot -> leave Battlefield -> COMBAT dirty.
        let hand1 = state.players()[0].hand[0];
        let bf1 = move_object_raw(
            &mut state,
            hand1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let mut snap = state.to_snapshot();
        snap.dirty_since_eval = DirtyMask::empty();
        let mut clean_state = GameState::from_snapshot(db, snap);

        move_object_raw(
            &mut clean_state,
            bf1,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(
            clean_state
                .dirty_since_eval()
                .contains(DirtyMask::TOPOLOGY | DirtyMask::GRAVEYARDS | DirtyMask::COMBAT)
        );
    }

    #[test]
    fn face_down_class_does_not_initialize_class_level() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(3)); // Class enchantment

        // 1) Manifested / face-down onto Battlefield does NOT set class_level = Some(1).
        let h0 = state.players()[0].hand[0];
        let bf_fd = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Manifest,
                    opaque_seq: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(state.object(bf_fd).unwrap().is_face_down());
        assert_eq!(state.rare(bf_fd).and_then(|r| r.class_level), None);

        // 2) Face-down spell resolving Stack -> Battlefield also keeps class_level == None.
        let h1 = state.players()[0].hand[0];
        let st_fd = move_object_raw(
            &mut state,
            h1,
            Zone::Stack,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    opaque_seq: 2,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let bf_from_st = move_object_raw(
            &mut state,
            st_fd,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(state.object(bf_from_st).unwrap().is_face_down());
        assert_eq!(state.rare(bf_from_st).and_then(|r| r.class_level), None);

        // 3) Face-up Battlefield entry DOES initialize class_level = Some(1).
        let h2 = state.players()[0].hand[0];
        let bf_fu = move_object_raw(
            &mut state,
            h2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert!(!state.object(bf_fu).unwrap().is_face_down());
        assert_eq!(state.rare(bf_fu).and_then(|r| r.class_level), Some(1));
    }

    #[test]
    fn hidden_zone_lki_skip_and_bounded_lki_records() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        // 1) draw_one_raw (Library -> Hand) skips LKI capture.
        let lib_top = *state.players()[0].library.last().unwrap();
        let drawn = draw_one_raw(&mut state, PlayerId(0)).unwrap().unwrap();
        assert!(state.lki(lib_top).is_none());

        // 2) Hand -> Library skips LKI capture.
        let tucked = move_object_raw(
            &mut state,
            drawn,
            Zone::Library,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(tucked.lki.is_none());
        assert!(state.lki(drawn).is_none());

        // 3) Battlefield -> Graveyard captures LKI and lki_records stays bounded <= MAX_LKI_RECORDS.
        let mut cur = state.players()[0].hand[0];
        for _ in 0..80 {
            let bf = move_object_raw(
                &mut state,
                cur,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();
            let gy = move_object_raw(
                &mut state,
                bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            assert!(gy.lki.is_some());
            assert!(state.lki(bf).is_some());
            cur = gy.new_id.unwrap();
        }
        let snap = state.to_snapshot();
        assert!(snap.lki_records.len() <= crate::state::MAX_LKI_RECORDS);
    }

    #[test]
    fn token_cleanup_and_slot_reuse_leaves_no_orphaned_side_table_entries() {
        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let spec = TokenSpec {
            name: "Goblin".into(),
            power: 1,
            toughness: 1,
            colors: ColorMask::R,
            types: CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: smallvec::smallvec![SubtypeId(0)],
            type_line: "Creature — Goblin".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        };
        let tok = create_token_raw(
            &mut state,
            PlayerId(0),
            spec.clone(),
            CreateTokenOptions::default(),
        )
        .unwrap();
        assert!(state.add_counter(tok, CounterId(0), 4));
        state.rare_or_default(tok).unwrap().chosen_color = Some(ColorMask::R);

        let tok_gy = move_object_raw(
            &mut state,
            tok,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        cleanup_tokens_outside_battlefield(&mut state);
        assert!(state.object(tok_gy).is_none());

        let snap_after_cleanup = state.to_snapshot();
        assert!(
            !snap_after_cleanup
                .object_counters
                .iter()
                .any(|(idx, _)| *idx == tok.index)
        );
        assert!(
            !snap_after_cleanup
                .rare_states
                .iter()
                .any(|(idx, _)| *idx == tok.index)
        );

        // Reuse the freed slot via create_token_raw and verify clean counters and rare state.
        let reused =
            create_token_raw(&mut state, PlayerId(0), spec, CreateTokenOptions::default()).unwrap();
        assert_eq!(reused.index, tok.index);
        assert!(state.counters(reused).is_empty());
        assert_eq!(state.rare(reused).and_then(|r| r.chosen_color), None);
    }

    fn make_zone_test_db() -> Arc<CardDatabase> {
        use mtg_ir::{
            AbilityDef, EventDetail, EventFilter, EventKind, EventPattern, ObjectFilter,
            ReplacementAbility, ReplacementDef, ReplacementWith, ZoneChangeMod, ZoneMask,
        };
        let simple_face = |name: &str,
                           types: CardTypeMask,
                           subtypes: &[SubtypeId],
                           abilities: Vec<AbilityDef>| FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse("{1}"),
                types,
                subtypes: subtypes.iter().copied().collect(),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                color_indicator: None,
                pt: if types.contains(CardTypeMask::CREATURE) {
                    Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    })
                } else {
                    None
                },
                loyalty: None,
                defense: None,
            },
            abilities,
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        };
        let mk = |name: &str,
                  types: CardTypeMask,
                  subtypes: &[SubtypeId],
                  abilities: Vec<AbilityDef>| CardDef {
            oracle_id: name.to_lowercase(),
            compiler_version: 46,
            text_hash: name.into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![simple_face(name, types, subtypes, abilities)],
            fully_supported: true,
        };

        let cards = vec![
            // 0: Bear
            mk("Bear", CardTypeMask::CREATURE, &[], vec![]),
            // 1: Land
            mk("PlainLand", CardTypeMask::LAND, &[], vec![]),
            // 2: Pay-2-life-or-tapped Land
            mk(
                "ShockTest",
                CardTypeMask::LAND,
                &[],
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: EventPattern {
                            event: EventKind::ZoneChange,
                            filter: EventFilter {
                                or_self: true,
                                detail: Some(EventDetail::ZoneChange {
                                    from: ZoneMask::empty(),
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
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                pay_life_or_enter_tapped: Some(2),
                                ..Default::default()
                            }),
                        },
                        self_replacement: true,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 3: LandsCantEnterBattlefield blocker
            mk(
                "LandBlocker",
                CardTypeMask::ENCHANTMENT,
                &[],
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::LandsCantEnterBattlefield,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 4: Grafdigger-style CantEnterFromZones (creatures from Graveyard)
            mk(
                "GyBlocker",
                CardTypeMask::ARTIFACT,
                &[],
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::CantEnterFromZones {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                            zones: ZoneMask::GRAVEYARD,
                            who: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 5: Saga enchantment (subtype 0 = "Saga")
            mk(
                "TestSaga",
                CardTypeMask::ENCHANTMENT,
                &[SubtypeId(0)],
                vec![],
            ),
        ];

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Saga".into()],
                    counters: vec!["+1/+1".into(), "lore".into()],
                    keywords: vec![],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn move_object_pauses_without_moving_or_emitting_then_resumes_cleanly() {
        let db = make_zone_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let ts = state.allocate_timestamp();
        let shock_obj = crate::state::GameObject::new(
            CardDefId(2),
            PlayerId(0),
            Zone::Library,
            ts,
            ObjectFlags::empty(),
        );
        let lib_id = state.insert_object(shock_obj, None);
        state.player_mut(PlayerId(0)).unwrap().library.push(lib_id);
        let hand_id = move_object_raw(
            &mut state,
            lib_id,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        crate::events::begin_action(&mut state);
        let outcome = move_object(
            &mut state,
            hand_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let MoveObjectOutcome::Paused { prompt_id, chooser } = outcome else {
            panic!("expected pause for shockland choice");
        };
        assert_eq!(chooser, PlayerId(0));
        // Object MUST still be in Hand and NO ZoneChange event emitted while paused.
        assert_eq!(state.object(hand_id).unwrap().zone, Zone::Hand);
        assert!(crate::events::peek_events(&state).is_empty());

        // Choose Yes -> pay 2 life, enter untapped, emit ZoneChange.
        let resumed = crate::replace::resume_and_finalize_replacement(
            &mut state,
            chooser,
            prompt_id,
            crate::action::ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        let crate::replace::ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
            resumed
        else {
            panic!("expected ZoneMove after resume");
        };
        let bf_id = raw.new_id.unwrap();
        assert_eq!(state.object(bf_id).unwrap().zone, Zone::Battlefield);
        assert!(
            !state
                .object(bf_id)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert_eq!(state.players()[0].life, 18);
        assert!(crate::events::peek_events(&state).iter().any(|e| {
            matches!(
                e.kind,
                crate::events::AtomicEventKind::ZoneChange {
                    to: Zone::Battlefield,
                    ..
                }
            )
        }));
        crate::events::end_action(&mut state);
    }

    #[test]
    fn move_object_respects_lands_cant_enter_and_cant_enter_from_zones() {
        let db = make_zone_test_db();
        let mut state = boot_state(db, CardDefId(0));

        // Put LandBlocker (3) and GyBlocker (4) on Battlefield.
        for def_id in [CardDefId(3), CardDefId(4)] {
            let ts = state.allocate_timestamp();
            let obj = crate::state::GameObject::new(
                def_id,
                PlayerId(1),
                Zone::Library,
                ts,
                ObjectFlags::empty(),
            );
            let id = state.insert_object(obj, None);
            state.player_mut(PlayerId(1)).unwrap().library.push(id);
            move_object_raw(
                &mut state,
                id,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }

        // Land in Hand tries to enter Battlefield -> Skipped!
        let ts = state.allocate_timestamp();
        let land_obj = crate::state::GameObject::new(
            CardDefId(1),
            PlayerId(0),
            Zone::Library,
            ts,
            ObjectFlags::empty(),
        );
        let land_lib = state.insert_object(land_obj, None);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .library
            .push(land_lib);
        let land_hand = move_object_raw(
            &mut state,
            land_lib,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let res = move_object(
            &mut state,
            land_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(matches!(res, MoveObjectOutcome::Skipped { .. }));
        assert_eq!(state.object(land_hand).unwrap().zone, Zone::Hand);

        // Creature in Graveyard tries to enter Battlefield -> Skipped!
        let bear_hand = state.players()[0].hand[0];
        let bear_gy = move_object_raw(
            &mut state,
            bear_hand,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let res2 = move_object(
            &mut state,
            bear_gy,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(matches!(res2, MoveObjectOutcome::Skipped { .. }));
        assert_eq!(state.object(bear_gy).unwrap().zone, Zone::Graveyard);
    }

    #[test]
    fn move_object_saga_lore_counter_and_ltb_clears_new_rare_fields() {
        let db = make_zone_test_db();
        let mut state = boot_state(db, CardDefId(0));

        let ts = state.allocate_timestamp();
        let saga_obj = crate::state::GameObject::new(
            CardDefId(5),
            PlayerId(0),
            Zone::Library,
            ts,
            ObjectFlags::empty(),
        );
        let saga_lib = state.insert_object(saga_obj, None);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .library
            .push(saga_lib);
        let saga_hand = move_object_raw(
            &mut state,
            saga_lib,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        let MoveObjectOutcome::Moved(raw) = move_object(
            &mut state,
            saga_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Saga ETB to move immediately");
        };
        let saga_bf = raw.new_id.unwrap();
        // Saga ETB automatically gets 1 lore counter (CounterId(1)).
        assert_eq!(state.counter_count(saga_bf, CounterId(1)), 1);

        // Populate all 6 new RareObjectState fields on battlefield, then LTB -> verify cleared.
        {
            let r = state.rare_or_default(saga_bf).unwrap();
            r.chosen_colors.push(mtg_ir::ManaColor::U);
            r.chosen_players.push(PlayerId(1));
            r.chosen_anchor_word = Some("Khans".into());
            r.chosen_odd_parity = Some(true);
            r.dealt_damage_exile_sources.push(saga_bf);
            r.exile_instead_of_dying_this_turn = true;
        }
        let MoveObjectOutcome::Moved(ltb_raw) = move_object(
            &mut state,
            saga_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected LTB move");
        };
        let saga_gy = ltb_raw.new_id.unwrap();
        if let Some(rare_after) = state.rare(saga_gy) {
            assert!(rare_after.chosen_colors.is_empty());
            assert!(rare_after.chosen_players.is_empty());
            assert!(rare_after.chosen_anchor_word.is_none());
            assert!(rare_after.chosen_odd_parity.is_none());
            assert!(rare_after.dealt_damage_exile_sources.is_empty());
            assert!(!rare_after.exile_instead_of_dying_this_turn);
        }
    }

    #[test]
    fn etb_counters_pause_resume_and_snapshot_roundtrip() {
        use mtg_ir::{
            Amount, CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, CounterId,
            EventDetail, EventFilter, EventKind, EventPattern, IR_V2_VERSION, InternTables,
            IrV2Bundle, ManaCost, MultiplierTarget, ObjectFilter, ReplacementAbility,
            ReplacementDef, ReplacementWith, StaticAbility, StaticBody, StaticDef, SubtypeId,
            ZoneChangeMod, ZoneMask,
        };

        let mk =
            |name: &str, types: CardTypeMask, subs: &[SubtypeId], abs: Vec<AbilityDef>| CardDef {
                oracle_id: name.to_lowercase(),
                compiler_version: 46,
                text_hash: name.to_string(),
                name: name.to_string(),
                layout: CardLayout::Normal,
                faces: vec![mtg_ir::FaceDef {
                    name: name.to_string(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}"),
                        types,
                        subtypes: subs.iter().copied().collect(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: abs,
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            };

        let mult_card = |name: &str, factor: f32, delta: i16| {
            mk(
                name,
                CardTypeMask::ENCHANTMENT,
                &[],
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Multiplier {
                            what: MultiplierTarget::Counters,
                            factor,
                            delta,
                            filter: None,
                            counter: None,
                            cause: None,
                            cause_or: vec![],
                            round: None,
                            applies_to_players: false,
                            who_places: None,
                            who_creates: None,
                            extra_token: None,
                            extra_token_matching_count: false,
                            trigger_source_attached_to: false,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )
        };

        let saga_with_extra = mk(
            "SagaWithCounters",
            CardTypeMask::ENCHANTMENT,
            &[SubtypeId(0)],
            vec![AbilityDef::Replacement(ReplacementAbility {
                replacement: ReplacementDef {
                    replaces: EventPattern {
                        event: EventKind::ZoneChange,
                        filter: EventFilter {
                            or_self: true,
                            detail: Some(EventDetail::ZoneChange {
                                from: ZoneMask::empty(),
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
                    with: ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            counters: Some((CounterId(0), Box::new(Amount::Const { n: 2 }))),
                            ..Default::default()
                        }),
                    },
                    self_replacement: true,
                    who: None,
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec!["Saga".into()],
                    counters: vec!["+1/+1".into(), "lore".into()],
                    keywords: vec![],
                },
                cards: vec![
                    mk("Filler", CardTypeMask::CREATURE, &[], vec![]),
                    mult_card("Doubler", 2.0, 0),
                    mult_card("PlusOne", 1.0, 1),
                    saga_with_extra,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = boot_state(db.clone(), CardDefId(0));

        // Put Doubler (1) and PlusOne (2) onto P0's battlefield.
        for def_id in [CardDefId(1), CardDefId(2)] {
            let ts = state.allocate_timestamp();
            let obj = GameObject::new(def_id, PlayerId(0), Zone::Library, ts, ObjectFlags::empty());
            let id = state.insert_object(obj, None);
            state.player_mut(PlayerId(0)).unwrap().library.push(id);
            move_object_raw(
                &mut state,
                id,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        // Put SagaWithCounters (3) in P0's Hand.
        let ts = state.allocate_timestamp();
        let saga_obj = GameObject::new(
            CardDefId(3),
            PlayerId(0),
            Zone::Library,
            ts,
            ObjectFlags::empty(),
        );
        let saga_lib = state.insert_object(saga_obj, None);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .library
            .push(saga_lib);
        let saga_hand = move_object_raw(
            &mut state,
            saga_lib,
            Zone::Hand,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        crate::events::begin_action(&mut state);
        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            saga_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected pause on Saga lore counter replacement ordering");
        };
        assert_eq!(chooser, PlayerId(0));
        assert!(state.pending_replacement().is_some());
        assert!(crate::events::peek_events(&state).is_empty());

        // Round-trip snapshot losslessly via serde_json and fork two branches.
        let json = serde_json::to_string(&state.to_snapshot()).unwrap();
        let snap: crate::state::GameStateSnapshot = serde_json::from_str(&json).unwrap();
        let mut branch_a = GameState::from_snapshot(db, snap);
        let mut branch_b = branch_a.clone();

        // Branch A: pick index 0 (Doubler first -> 1*2 + 1 = 3 lore), then index 0 for +1/+1 (2*2 + 1 = 5).
        let crate::replace::ReplacementResumeOutcome::Paused {
            prompt_id: pid_a2,
            chooser: c_a2,
        } = crate::replace::resume_and_finalize_replacement(
            &mut branch_a,
            chooser,
            prompt_id,
            crate::action::ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap()
        else {
            panic!("expected second ETB counter pause for +1/+1 counters");
        };
        assert!(crate::events::peek_events(&branch_a).is_empty());
        let crate::replace::ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw_a)) =
            crate::replace::resume_and_finalize_replacement(
                &mut branch_a,
                c_a2,
                pid_a2,
                crate::action::ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected final Moved outcome on branch A");
        };
        let bf_a = raw_a.new_id.unwrap();
        assert!(branch_a.pending_replacement().is_none());
        assert_eq!(branch_a.counter_count(bf_a, CounterId(1)), 3);
        assert_eq!(branch_a.counter_count(bf_a, CounterId(0)), 5);
        assert_eq!(
            crate::events::peek_events(&branch_a)
                .iter()
                .filter(|e| matches!(
                    e.kind,
                    crate::events::AtomicEventKind::ZoneChange {
                        to: Zone::Battlefield,
                        ..
                    }
                ))
                .count(),
            1
        );

        // Branch B: pick index 1 (PlusOne first -> (1+1)*2 = 4 lore), then index 1 for +1/+1 ((2+1)*2 = 6).
        let crate::replace::ReplacementResumeOutcome::Paused {
            prompt_id: pid_b2,
            chooser: c_b2,
        } = crate::replace::resume_and_finalize_replacement(
            &mut branch_b,
            chooser,
            prompt_id,
            crate::action::ChoiceAnswer::PickReplacement { index: 1 },
        )
        .unwrap()
        else {
            panic!("expected second ETB counter pause on branch B");
        };
        let crate::replace::ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw_b)) =
            crate::replace::resume_and_finalize_replacement(
                &mut branch_b,
                c_b2,
                pid_b2,
                crate::action::ChoiceAnswer::PickReplacement { index: 1 },
            )
            .unwrap()
        else {
            panic!("expected final Moved outcome on branch B");
        };
        let bf_b = raw_b.new_id.unwrap();
        assert!(branch_b.pending_replacement().is_none());
        assert_eq!(branch_b.counter_count(bf_b, CounterId(1)), 4);
        assert_eq!(branch_b.counter_count(bf_b, CounterId(0)), 6);
        assert_eq!(
            crate::events::peek_events(&branch_b)
                .iter()
                .filter(|e| matches!(
                    e.kind,
                    crate::events::AtomicEventKind::ZoneChange {
                        to: Zone::Battlefield,
                        ..
                    }
                ))
                .count(),
            1
        );
        let _ = ObjectFilter::default();
        crate::events::end_action(&mut branch_a);
        crate::events::end_action(&mut branch_b);
    }

    #[test]
    fn pre_block_skips_shockland_and_respects_filter_zones_and_clone_deferral() {
        use mtg_ir::{
            CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, EtbCopySpec,
            EventDetail, EventFilter, EventKind, EventPattern, IR_V2_VERSION, InternTables,
            IrV2Bundle, ManaCost, ObjectFilter, PrintedPt, ReplacementAbility, ReplacementDef,
            ReplacementWith, StaticAbility, StaticBody, StaticDef, ZoneChangeMod, ZoneMask,
        };

        let mk = |name: &str, types: CardTypeMask, abs: Vec<AbilityDef>| CardDef {
            oracle_id: name.to_lowercase(),
            compiler_version: 46,
            text_hash: name.to_string(),
            name: name.to_string(),
            layout: CardLayout::Normal,
            faces: vec![mtg_ir::FaceDef {
                name: name.to_string(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: types
                        .contains(CardTypeMask::CREATURE)
                        .then_some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        }),
                    loyalty: None,
                    defense: None,
                },
                abilities: abs,
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let zc_self_bf = EventPattern {
            event: EventKind::ZoneChange,
            filter: EventFilter {
                or_self: true,
                detail: Some(EventDetail::ZoneChange {
                    from: ZoneMask::empty(),
                    to: Some(Zone::Battlefield),
                    from_anywhere: false,
                    not_from_zone: None,
                    not_to_zone: None,
                    enters_untapped: false,
                    dealt_damage_by_self_this_turn: false,
                }),
                ..Default::default()
            },
        };

        let cards = vec![
            // 0: Bear
            mk("Bear", CardTypeMask::CREATURE, vec![]),
            // 1: PlainLand
            mk("PlainLand", CardTypeMask::LAND, vec![]),
            // 2: ShockTest
            mk(
                "ShockTest",
                CardTypeMask::LAND,
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: zc_self_bf.clone(),
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                pay_life_or_enter_tapped: Some(2),
                                ..Default::default()
                            }),
                        },
                        self_replacement: true,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 3: LandBlocker
            mk(
                "LandBlocker",
                CardTypeMask::ENCHANTMENT,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::LandsCantEnterBattlefield,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 4: ZoneFilteredBlocker: filter.zones = GRAVEYARD, zones = GRAVEYARD | HAND
            mk(
                "ZoneFilteredBlocker",
                CardTypeMask::ARTIFACT,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::CantEnterFromZones {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                zones: ZoneMask::GRAVEYARD,
                                ..Default::default()
                            }),
                            zones: ZoneMask::GRAVEYARD | ZoneMask::HAND,
                            who: None,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
            // 5: SynthClone (can copy Creature or Land)
            mk(
                "SynthClone",
                CardTypeMask::CREATURE,
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: zc_self_bf,
                        with: ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                copy: Some(EtbCopySpec {
                                    filter: Box::new(ObjectFilter {
                                        types_any: CardTypeMask::CREATURE | CardTypeMask::LAND,
                                        ..Default::default()
                                    }),
                                    optional: false,
                                    from_graveyards: false,
                                    tapped_if_copied: false,
                                    modifications: None,
                                }),
                                ..Default::default()
                            }),
                        },
                        self_replacement: true,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            ),
        ];

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = boot_state(db, CardDefId(0));

        // Part A: ShockTest pre-blocked by LandBlocker without prompting or paying life.
        let put = |s: &mut GameState, p: PlayerId, d: CardDefId, z: Zone| {
            let ts = s.allocate_timestamp();
            let o = GameObject::new(d, p, Zone::Library, ts, ObjectFlags::empty());
            let id = s.insert_object(o, None);
            s.player_mut(p).unwrap().library.push(id);
            move_object_raw(s, id, z, MoveRawOptions::default(), None)
                .unwrap()
                .new_id
                .unwrap()
        };

        put(&mut state, PlayerId(1), CardDefId(3), Zone::Battlefield);
        let shock_hand = put(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);

        crate::events::begin_action(&mut state);
        let res_shock = move_object(
            &mut state,
            shock_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(matches!(res_shock, MoveObjectOutcome::Skipped { .. }));
        assert!(state.pending_replacement().is_none());
        assert_eq!(state.players()[0].life, 20);
        assert_eq!(state.object(shock_hand).unwrap().zone, Zone::Hand);
        assert!(crate::events::peek_events(&state).is_empty());
        crate::events::end_action(&mut state);

        // Part B: CantEnterFromZones checks filter.zones against `from`.
        let mut state_b = state.clone();
        put(&mut state_b, PlayerId(1), CardDefId(4), Zone::Battlefield);
        let bear_gy = put(&mut state_b, PlayerId(0), CardDefId(0), Zone::Graveyard);
        let bear_hand = put(&mut state_b, PlayerId(0), CardDefId(0), Zone::Hand);
        assert!(matches!(
            move_object(
                &mut state_b,
                bear_gy,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None
            )
            .unwrap(),
            MoveObjectOutcome::Skipped { .. }
        ));
        assert!(matches!(
            move_object(
                &mut state_b,
                bear_hand,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None
            )
            .unwrap(),
            MoveObjectOutcome::Moved(_)
        ));

        // Part C: Clone deferral under LandBlocker.
        let plain_land_bf = put(&mut state, PlayerId(0), CardDefId(1), Zone::Battlefield);
        let bear_bf = put(&mut state, PlayerId(0), CardDefId(0), Zone::Battlefield);
        let clone_hand = put(&mut state, PlayerId(0), CardDefId(5), Zone::Hand);

        let MoveObjectOutcome::Paused { prompt_id, chooser } = move_object(
            &mut state,
            clone_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Clone ETB pause despite LandBlocker");
        };
        let mut branch_land = state.clone();
        let crate::replace::ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Skipped {
            ..
        }) = crate::replace::resume_and_finalize_replacement(
            &mut branch_land,
            chooser,
            prompt_id,
            crate::action::ChoiceAnswer::PickObject { id: plain_land_bf },
        )
        .unwrap()
        else {
            panic!("expected Clone copying PlainLand to be Skipped by LandBlocker");
        };
        assert_eq!(branch_land.object(clone_hand).unwrap().zone, Zone::Hand);

        let crate::replace::ReplacementResumeOutcome::ZoneMove(MoveObjectOutcome::Moved(raw)) =
            crate::replace::resume_and_finalize_replacement(
                &mut state,
                chooser,
                prompt_id,
                crate::action::ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap()
        else {
            panic!("expected Clone copying Bear to succeed");
        };
        assert_eq!(
            state.object(raw.new_id.unwrap()).unwrap().zone,
            Zone::Battlefield
        );
    }

    #[test]
    fn synth_planeswalker_battle_etb_multiplier_manifest_and_token_copy() {
        let mut tables = InternTables::default();
        tables.counters.push("loyalty".into());
        tables.counters.push("defense".into());
        tables.counters.push("lore".into());
        let loyalty_cid = CounterId(0);
        let defense_cid = CounterId(1);

        let plains = CardDef {
            oracle_id: "synth_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
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
        };
        let walker = CardDef {
            oracle_id: "synth_walker".into(),
            compiler_version: 46,
            text_hash: "1".into(),
            name: "Synth Walker".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Walker".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{W}"),
                    types: CardTypeMask::PLANESWALKER,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::LEGENDARY,
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: Some(3),
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
        let battle = CardDef {
            oracle_id: "synth_siege".into(),
            compiler_version: 46,
            text_hash: "2".into(),
            name: "Synth Siege".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Siege".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{R}"),
                    types: CardTypeMask::BATTLE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: Some(5),
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let doubler = CardDef {
            oracle_id: "synth_doubler".into(),
            compiler_version: 46,
            text_hash: "3".into(),
            name: "Synth Doubler".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Doubler".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{4}{G}"),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Multiplier {
                            what: mtg_ir::MultiplierTarget::Counters,
                            factor: 2.0,
                            delta: 0,
                            filter: None,
                            counter: None,
                            cause: None,
                            cause_or: vec![],
                            round: None,
                            applies_to_players: false,
                            who_places: Some(mtg_ir::RelativeControl::You),
                            who_creates: None,
                            extra_token: None,
                            extra_token_matching_count: false,
                            trigger_source_attached_to: false,
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

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains, walker, battle, doubler],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = boot_state(db, CardDefId(0));

        let spawn_hand = |s: &mut GameState, d: CardDefId| {
            let ts = s.allocate_timestamp();
            let o = GameObject::new(d, PlayerId(0), Zone::Hand, ts, ObjectFlags::empty());
            let id = s.insert_object(o, None);
            s.player_mut(PlayerId(0)).unwrap().hand.push(id);
            s.mark_dirty(DirtyMask::TOPOLOGY);
            id
        };

        // 1. Planeswalker enters with 3 loyalty (CR 306.5b) and survives SBAs.
        let pw_hand = spawn_hand(&mut state, CardDefId(1));
        let MoveObjectOutcome::Moved(pw_raw) = move_object(
            &mut state,
            pw_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Planeswalker move");
        };
        let pw_bf = pw_raw.new_id.unwrap();
        assert_eq!(state.counter_count(pw_bf, loyalty_cid), 3);
        let _ = state.evaluate_board();
        assert_eq!(
            crate::sba::check_sbas(&mut state).unwrap(),
            crate::sba::SbaOutcome::Done
        );
        assert_eq!(state.object(pw_bf).unwrap().zone, Zone::Battlefield);

        // 2. Battle enters with 5 defense (CR 310.4b) and survives SBAs.
        let bat_hand = spawn_hand(&mut state, CardDefId(2));
        let MoveObjectOutcome::Moved(bat_raw) = move_object(
            &mut state,
            bat_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Battle move");
        };
        let bat_bf = bat_raw.new_id.unwrap();
        assert_eq!(state.counter_count(bat_bf, defense_cid), 5);
        let _ = state.evaluate_board();
        assert_eq!(
            crate::sba::check_sbas(&mut state).unwrap(),
            crate::sba::SbaOutcome::Done
        );
        assert_eq!(state.object(bat_bf).unwrap().zone, Zone::Battlefield);

        // 3. Manifest (face-down) Planeswalker enters with 0 loyalty counters.
        let pw_manifest_hand = spawn_hand(&mut state, CardDefId(1));
        let MoveObjectOutcome::Moved(man_raw) = move_object(
            &mut state,
            pw_manifest_hand,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Manifest,
                    opaque_seq: 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap() else {
            panic!("expected Manifest move");
        };
        let man_bf = man_raw.new_id.unwrap();
        assert_eq!(state.counter_count(man_bf, loyalty_cid), 0);
        let _ = state.evaluate_board();

        // 4. Token copy of Planeswalker gets 3 loyalty and survives SBAs (remove original first to avoid legend rule).
        let lki_pw = capture_lki_snapshot(&state, pw_bf, None);
        let _ = move_object_raw(
            &mut state,
            pw_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let tok_pw =
            create_token_copy_from_lki_raw(&mut state, PlayerId(0), &lki_pw, None, false).unwrap();
        assert_eq!(state.counter_count(tok_pw, loyalty_cid), 3);
        let _ = state.evaluate_board();
        assert_eq!(
            crate::sba::check_sbas(&mut state).unwrap(),
            crate::sba::SbaOutcome::Done
        );
        assert_eq!(state.object(tok_pw).unwrap().zone, Zone::Battlefield);

        let tok_pw2 = create_token_copy_raw(&mut state, PlayerId(0), tok_pw, None, false).unwrap();
        assert_eq!(state.counter_count(tok_pw2, loyalty_cid), 3);

        // 5. Synth Doubler-style PutCounters multiplier doubles ETB loyalty (6) and defense (10).
        let doubler_hand = spawn_hand(&mut state, CardDefId(3));
        let MoveObjectOutcome::Moved(_) = move_object(
            &mut state,
            doubler_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected Doubler move");
        };
        let pw2_hand = spawn_hand(&mut state, CardDefId(1));
        let MoveObjectOutcome::Moved(pw2_raw) = move_object(
            &mut state,
            pw2_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected doubled Planeswalker move");
        };
        assert_eq!(state.counter_count(pw2_raw.new_id.unwrap(), loyalty_cid), 6);

        let bat2_hand = spawn_hand(&mut state, CardDefId(2));
        let MoveObjectOutcome::Moved(bat2_raw) = move_object(
            &mut state,
            bat2_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap() else {
            panic!("expected doubled Battle move");
        };
        assert_eq!(
            state.counter_count(bat2_raw.new_id.unwrap(), defense_cid),
            10
        );
        let _ = state.evaluate_board();
    }

    #[test]
    fn synth_transfer_control_raw_updates_layer_cache_controller() {
        use crate::layers::predicates::{ContinuousTarget, TimedContinuousEffect};
        use mtg_ir::{Duration, Layer, LayerMod, PlayerRef};

        let db = make_test_db();
        let mut state = boot_state(db, CardDefId(0));

        // Simulate casting a creature from hand so RareObjectState exists at ETB
        // and `zones.rs` records `rare.default_controller = Some(PlayerId(0))`.
        let hand_id = state.players()[0].hand[0];
        let stack_id = move_object_raw(
            &mut state,
            hand_id,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.rare_or_default(stack_id).unwrap().cast_from_zone = Some(Zone::Hand);
        let bf_id = move_object_raw(
            &mut state,
            stack_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        assert_eq!(
            state.rare(bf_id).and_then(|r| r.default_controller),
            Some(PlayerId(0))
        );
        assert_eq!(
            state.evaluate_board().get(bf_id).unwrap().controller,
            PlayerId(0)
        );

        // Permanent GainControl (`Duration::None`) calls `transfer_control_raw`:
        // both `obj.controller`, `rare.default_controller`, and `LayerCache` must update.
        transfer_control_raw(&mut state, bf_id, PlayerId(1)).unwrap();
        assert_eq!(state.object(bf_id).unwrap().controller, PlayerId(1));
        assert_eq!(
            state.rare(bf_id).and_then(|r| r.default_controller),
            Some(PlayerId(1))
        );
        assert!(state.players()[1].battlefield.contains(&bf_id));
        assert!(!state.players()[0].battlefield.contains(&bf_id));
        assert_eq!(
            state.evaluate_board().get(bf_id).unwrap().controller,
            PlayerId(1)
        );

        // A temporary Layer 2 `SetController` still overrides `default_controller`,
        // and removing it reverts `LayerCache` to `PlayerId(1)` (the permanent controller).
        let eff_id = state.allocate_continuous_effect_id();
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: eff_id,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer2,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bf_id]),
            modifier: LayerMod::SetController {
                controller: PlayerRef::You,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert_eq!(
            state.evaluate_board().get(bf_id).unwrap().controller,
            PlayerId(0)
        );

        state.retain_continuous_effects(|e| e.id != eff_id);
        assert_eq!(
            state.evaluate_board().get(bf_id).unwrap().controller,
            PlayerId(1)
        );

        // Transferring control back to PlayerId(0) updates LayerCache again.
        transfer_control_raw(&mut state, bf_id, PlayerId(0)).unwrap();
        assert_eq!(
            state.evaluate_board().get(bf_id).unwrap().controller,
            PlayerId(0)
        );
    }
}
