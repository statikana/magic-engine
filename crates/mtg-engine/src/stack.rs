use std::sync::Arc;

use mtg_ir::{
    AbilityDef, CardTypeMask, CounterReplacementZone, EffectStep, ModalDef, TargetSpec, Zone,
};
use smallvec::SmallVec;

use crate::action::{DecisionState, TargetRef};
use crate::casting::prepare_next_untargeted_stack_trigger;
use crate::effects::{FrameOutcome, begin_guided_resolution_frame, begin_resolution_frame};
use crate::events::{AtomicEventKind, emit_event, snapshot_for_live_object};
use crate::faces::{
    active_face_view, active_stack_spell_ability, face_opts_for_object, resolve_object_faces,
};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::legality::resolve_activated_abilities;
use crate::sba::check_sbas;
use crate::state::{
    AltCastKind, DirtyMask, EngineError, GameObject, GameState, LkiSnapshot, MergedComponent,
    ObjectFlags, ReplacementResumeHook, ResolutionBinding, StackChoices, StackEntry,
    StackEntryKind, TOKEN_CARD_DEF_ID,
};
use crate::targets::{
    TargetRevalidation, TargetSourceInfo, build_stack_entry_target_source_info,
    is_target_legal_for_spec, revalidate_targets,
};
use crate::triggers::{flush_triggers, trigger_intervening_if_holds_on_resolution};
use crate::zones::{
    MoveObjectOutcome, MoveRawOptions, ZonePosition, create_token_copy_raw, move_object,
};

/// Resolves the top item of the stack (`CR 608.2`), keeping it on `state.stack` until resolution
/// finishes (`CR 608.2n`). Returns `Ok(true)` if the item finished resolving or fizzled, or `Ok(false)`
/// if the stack was empty or resolution paused on a prompt/replacement/game-over.
pub fn resolve_top_of_stack(state: &mut GameState) -> Result<bool, EngineError> {
    let Some(entry) = state.stack_entries().last().cloned() else {
        return Ok(false);
    };
    let stack_id = entry.id;

    // 1. CR 603.4: Intervening-if check on triggered ability resolution.
    if matches!(entry.kind, StackEntryKind::Triggered { .. })
        && !entry.choices.resolution_started
        && !trigger_intervening_if_holds_on_resolution(state, stack_id)
    {
        remove_stack_entry_by_id(state, stack_id);
        state.remove_stack_trigger_meta(stack_id);
        let _ = check_sbas(state)?;
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
        return Ok(true);
    }

    // 2. CR 608.2b: Target revalidation upon resolution (skip if already started).
    if !entry.targets.is_empty() && !entry.choices.resolution_started {
        let specs = target_specs_for_stack_entry(state, &entry);
        let chosen_by_spec: Vec<Vec<TargetRef>> = if !entry.choices.targets_by_spec.is_empty() {
            entry.choices.targets_by_spec.clone()
        } else {
            vec![entry.targets.clone()]
        };
        let reval = {
            let info = build_stack_entry_target_source_info(state, &entry, &chosen_by_spec);
            revalidate_targets(state, &specs, &info, &chosen_by_spec)
        };
        match reval {
            TargetRevalidation::AllLegal => {}
            TargetRevalidation::AllIllegal => {
                // CR 702.103e: Bestow aura spell whose target became illegal resolves as a creature.
                if entry.choices.bestowed {
                    if let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) {
                        state.stack_entries_mut()[pos].choices.bestowed = false;
                        state.stack_entries_mut()[pos].targets.clear();
                        state.stack_entries_mut()[pos]
                            .choices
                            .targets_by_spec
                            .clear();
                    }
                    if let StackEntryKind::Spell { object } = entry.kind
                        && let Some(obj) = state.object_mut(object)
                    {
                        obj.flags.remove(ObjectFlags::BESTOWED);
                    }
                    state.mark_dirty(DirtyMask::DESIGNATIONS);
                } else if entry.choices.mutating {
                    // CR 702.140b: Mutate spell whose target became illegal resolves as a regular creature.
                    if let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) {
                        state.stack_entries_mut()[pos].choices.mutating = false;
                        state.stack_entries_mut()[pos].choices.mutate_target = None;
                        state.stack_entries_mut()[pos].targets.clear();
                        state.stack_entries_mut()[pos]
                            .choices
                            .targets_by_spec
                            .clear();
                    }
                    state.mark_dirty(DirtyMask::DESIGNATIONS);
                } else {
                    fizzle_stack_entry(state, &entry)?;
                    let _ = check_sbas(state)?;
                    flush_triggers(state);
                    let _ = prepare_next_untargeted_stack_trigger(state)?;
                    return Ok(true);
                }
            }
            TargetRevalidation::PartiallyIllegal { .. } => {
                filter_illegal_targets_on_entry(state, stack_id, &specs, &chosen_by_spec);
            }
        }
    }

    // Reload potentially updated entry and mark resolution_started = true.
    if let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) {
        state.stack_entries_mut()[pos].choices.resolution_started = true;
    }
    let Some(entry) = state
        .stack_entries()
        .iter()
        .find(|e| e.id == stack_id)
        .cloned()
    else {
        return Ok(false);
    };

    // 3. Gather EffectSteps, initial bindings, and optional guided fallback.
    let (steps, initial_bindings, guided_text, source_opt, source_lki) =
        extract_resolution_payload(state, &entry)?;

    if let Some((source_obj, text)) = guided_text {
        if state.config().allow_guided_fallback {
            match begin_guided_resolution_frame(
                state,
                stack_id,
                entry.controller,
                source_obj,
                text,
            )? {
                FrameOutcome::Completed => {}
                FrameOutcome::Paused { .. } | FrameOutcome::GameOver { .. } => return Ok(false),
            }
        } else {
            return Err(EngineError::NotImplemented(
                "unsupported ability without allow_guided_fallback",
            ));
        }
    } else if !steps.is_empty() {
        let targets_by_spec = if !entry.choices.targets_by_spec.is_empty() {
            entry.choices.targets_by_spec.clone()
        } else if !entry.targets.is_empty() {
            vec![entry.targets.clone()]
        } else {
            Vec::new()
        };
        match begin_resolution_frame(
            state,
            stack_id,
            entry.controller,
            source_opt,
            source_lki,
            entry.x_value,
            entry.targets.clone(),
            targets_by_spec,
            entry.choices.divisions.clone(),
            steps,
            initial_bindings,
        )? {
            FrameOutcome::Completed => {}
            FrameOutcome::Paused { .. } | FrameOutcome::GameOver { .. } => return Ok(false),
        }
    }

    // 4. Resolution completed synchronously; move/pop stack item and run SBAs + triggers.
    finalize_resolved_stack_entry(state, stack_id)
}

/// Completes a stack item after its [`ResolutionVmState`] finishes via [`resume_resolution_frame`]
/// or [`resume_after_replacement`].
pub fn finish_or_resume_stack_resolution(state: &mut GameState) -> Result<(), EngineError> {
    let top_resolving = state
        .stack_entries()
        .iter()
        .rev()
        .find(|e| e.choices.resolution_started)
        .map(|e| e.id);

    if let Some(stack_id) = top_resolving {
        let completed = finalize_resolved_stack_entry(state, stack_id)?;
        if !completed {
            return Ok(());
        }
    } else {
        let _ = check_sbas(state)?;
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
    }

    if !matches!(state.decision(), DecisionState::GameOver { .. })
        && state.pending_prompt().is_none()
        && state.pending_replacement().is_none()
        && state.pending_trigger_order().is_none()
        && state.casting().is_none()
    {
        let active = active_or_next_alive_player(state);
        state.clear_passed_priority();
        state.set_priority_player(active);
        state.set_decision(DecisionState::Priority);
    }
    Ok(())
}

/// CR 701.5: Counters a spell or ability on the stack.
pub fn counter_stack_item(
    state: &mut GameState,
    target: StackItemId,
    replacement_zone: Option<CounterReplacementZone>,
    by_controller: PlayerId,
    by_source: Option<ObjectId>,
) -> Result<(), EngineError> {
    let Some(pos) = state.stack_entries().iter().position(|e| e.id == target) else {
        return Ok(());
    };
    let entry = state.stack_entries()[pos].clone();

    if entry.choices.cant_be_countered || is_stack_item_uncounterable(state, &entry) {
        return Ok(());
    }

    let countered_lki = build_stack_entry_lki(state, &entry);
    state.stack_mut().remove(pos);
    state.stack_entries_mut().remove(pos);
    state.remove_stack_trigger_meta(target);
    state
        .resolution_vms_mut()
        .retain(|vm| vm.stack_item != target);
    state
        .resolution_frames_mut()
        .retain(|f| f.stack_item != target);

    if let StackEntryKind::Spell { object } = entry.kind {
        if entry.choices.is_copy {
            let _ = state.remove_object(object);
        } else {
            let (dest, pos_opt) = match replacement_zone {
                Some(CounterReplacementZone::Exile) => (Zone::Exile, ZonePosition::Default),
                Some(CounterReplacementZone::LibraryTop) => (Zone::Library, ZonePosition::Top),
                Some(CounterReplacementZone::LibraryBottom) => {
                    (Zone::Library, ZonePosition::Bottom)
                }
                None => {
                    if entry.choices.exile_on_leave_stack {
                        (Zone::Exile, ZonePosition::Default)
                    } else {
                        (Zone::Graveyard, ZonePosition::Default)
                    }
                }
            };
            let opts = MoveRawOptions {
                position: pos_opt,
                ..Default::default()
            };
            let _ = move_object(state, object, dest, opts, None)?;
        }
    }

    emit_event(
        state,
        AtomicEventKind::SpellCountered {
            stack_item: target,
            countered_lki: Arc::new(countered_lki),
            by_controller,
            by_source,
            by_spell_or_ability: true,
        },
    );
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

/// CR 707.10: Copies a spell or ability on the stack.
pub fn copy_stack_item(
    state: &mut GameState,
    source_stack_id: StackItemId,
    new_controller: PlayerId,
    new_targets: Option<Vec<TargetRef>>,
) -> Result<StackItemId, EngineError> {
    let Some(orig) = state
        .stack_entries()
        .iter()
        .find(|e| e.id == source_stack_id)
        .cloned()
    else {
        return Err(EngineError::IllegalAction(format!(
            "stack item {source_stack_id:?} not found to copy"
        )));
    };

    let new_sid = state.allocate_stack_id();
    let targets = new_targets.unwrap_or_else(|| orig.targets.clone());
    let mut choices = orig.choices.clone();
    choices.is_copy = true;
    choices.resolution_started = false;
    if targets != orig.targets {
        choices.targets_by_spec = if targets.is_empty() {
            Vec::new()
        } else {
            vec![targets.clone()]
        };
    }

    match &orig.kind {
        StackEntryKind::Spell { object } => {
            let orig_obj = state
                .object(*object)
                .ok_or_else(|| EngineError::IllegalAction("stack spell object missing".into()))?;
            let card_def = orig_obj.card_def;
            let active_face = orig_obj.active_face_index();
            let orig_rare = state.rare(*object).cloned();
            let ts = state.allocate_timestamp();
            let mut copy_obj = GameObject::new(
                card_def,
                new_controller,
                Zone::Stack,
                ts,
                ObjectFlags::IS_TOKEN,
            );
            copy_obj.active_face_index = active_face;
            let copy_oid = state.insert_object(copy_obj, orig_rare.map(Arc::new));

            state.stack_mut().push(copy_oid);
            state.stack_entries_mut().push(StackEntry {
                id: new_sid,
                kind: StackEntryKind::Spell { object: copy_oid },
                controller: new_controller,
                choices,
                targets,
                x_value: orig.x_value,
                paid_mana_colors: orig.paid_mana_colors,
            });
            state.mark_dirty(DirtyMask::TOPOLOGY);
            emit_event(
                state,
                AtomicEventKind::SpellCopied {
                    player: new_controller,
                    object: copy_oid,
                    stack_item: new_sid,
                },
            );
        }
        StackEntryKind::Activated { source, .. } => {
            state.stack_mut().push(*source);
            state.stack_entries_mut().push(StackEntry {
                id: new_sid,
                kind: orig.kind.clone(),
                controller: new_controller,
                choices,
                targets,
                x_value: orig.x_value,
                paid_mana_colors: orig.paid_mana_colors,
            });
            state.mark_dirty(DirtyMask::TOPOLOGY);
        }
        StackEntryKind::Triggered { source, .. } => {
            state.stack_mut().push(*source);
            state.stack_entries_mut().push(StackEntry {
                id: new_sid,
                kind: orig.kind.clone(),
                controller: new_controller,
                choices,
                targets,
                x_value: orig.x_value,
                paid_mana_colors: orig.paid_mana_colors,
            });
            if let Some(meta) = state.stack_trigger_meta(source_stack_id).cloned() {
                state.push_stack_trigger_meta(new_sid, meta);
            }
            state.mark_dirty(DirtyMask::TOPOLOGY);
        }
    }

    Ok(new_sid)
}

fn finalize_resolved_stack_entry(
    state: &mut GameState,
    stack_id: StackItemId,
) -> Result<bool, EngineError> {
    // Verify stack_id is still on stack (e.g., EndTurn or CounterSpell might have removed it!).
    let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) else {
        let _ = check_sbas(state)?;
        flush_triggers(state);
        let _ = prepare_next_untargeted_stack_trigger(state)?;
        return Ok(true);
    };
    let entry = state.stack_entries()[pos].clone();

    state.stack_mut().remove(pos);
    state.stack_entries_mut().remove(pos);
    state.remove_stack_trigger_meta(stack_id);

    match entry.kind {
        StackEntryKind::Spell { object } => {
            let is_permanent = spell_resolves_as_permanent(state, object, &entry.choices);
            if entry.choices.is_copy {
                if is_permanent {
                    let token_id =
                        create_token_copy_raw(state, entry.controller, object, None, false)?;
                    emit_event(
                        state,
                        AtomicEventKind::TokenCreated {
                            player: entry.controller,
                            object: token_id,
                        },
                    );
                }
                let _ = state.remove_object(object);
            } else if is_permanent {
                if entry.choices.mutating
                    && let Some(target_perm) = entry.choices.mutate_target
                    && state
                        .object(target_perm)
                        .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
                {
                    apply_mutate_merge(
                        state,
                        object,
                        target_perm,
                        entry.controller,
                        entry.choices.mutate_under,
                    )?;
                } else {
                    let attach_target = if entry.choices.bestowed || is_aura_spell(state, object) {
                        entry.targets.first().copied()
                    } else {
                        None
                    };
                    let attach_obj = attach_target.and_then(|t| match t {
                        TargetRef::Object { id } => Some(id),
                        _ => None,
                    });
                    let opts = MoveRawOptions {
                        controller: Some(entry.controller),
                        preserve_cast_state: false,
                        ..Default::default()
                    };
                    let mv = move_object(state, object, Zone::Battlefield, opts, None)?;
                    if matches!(mv, MoveObjectOutcome::Paused { .. }) {
                        state.set_pending_replacement_resume(Some(
                            ReplacementResumeHook::StackResolutionFinish {
                                attach_target: attach_obj,
                                is_adventure: false,
                            },
                        ));
                        return Ok(false);
                    }
                    complete_stack_resolution_after_replacement(state, mv, attach_obj, false)?;
                    return Ok(true);
                }
            } else {
                // Instant / Sorcery / AdventureHalf / OmenHalf
                let is_adventure = entry.choices.alt_cast_kind == AltCastKind::AdventureHalf;
                let dest = if is_adventure || entry.choices.exile_on_leave_stack {
                    Zone::Exile
                } else {
                    Zone::Graveyard
                };
                let mv = move_object(state, object, dest, MoveRawOptions::default(), None)?;
                if matches!(mv, MoveObjectOutcome::Paused { .. }) {
                    state.set_pending_replacement_resume(Some(
                        ReplacementResumeHook::StackResolutionFinish {
                            attach_target: None,
                            is_adventure,
                        },
                    ));
                    return Ok(false);
                }
                complete_stack_resolution_after_replacement(state, mv, None, is_adventure)?;
                return Ok(true);
            }
        }
        StackEntryKind::Activated { .. } | StackEntryKind::Triggered { .. } => {}
    }

    state.mark_dirty(DirtyMask::TOPOLOGY);
    let _ = check_sbas(state)?;
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    Ok(true)
}

pub(crate) fn complete_stack_resolution_after_replacement(
    state: &mut GameState,
    mv: MoveObjectOutcome,
    attach_target: Option<ObjectId>,
    is_adventure: bool,
) -> Result<(), EngineError> {
    if let MoveObjectOutcome::Moved(raw) = mv {
        if raw.to_zone == Zone::Battlefield
            && let Some(new_id) = raw.new_id
            && let Some(host) = attach_target
        {
            if state
                .object(host)
                .is_some_and(|h| h.zone == Zone::Battlefield)
            {
                if let Some(obj) = state.object_mut(new_id) {
                    obj.attached_to = Some(host);
                }
                state.mark_dirty(DirtyMask::TOPOLOGY);
                emit_event(
                    state,
                    AtomicEventKind::AttachmentChanged {
                        attachment: new_id,
                        host,
                        attached: true,
                    },
                );
            } else if state
                .object(new_id)
                .is_some_and(|o| o.flags.contains(ObjectFlags::BESTOWED))
            {
                if let Some(obj) = state.object_mut(new_id) {
                    obj.flags.remove(ObjectFlags::BESTOWED);
                }
                state.mark_dirty(DirtyMask::DESIGNATIONS);
            }
        }
        if is_adventure
            && raw.to_zone == Zone::Exile
            && let Some(nid) = raw.new_id
            && let Some(rare) = state.rare_or_default(nid)
        {
            rare.on_an_adventure = true;
            state.mark_dirty(DirtyMask::DESIGNATIONS);
        }
    }

    state.mark_dirty(DirtyMask::TOPOLOGY);
    let _ = check_sbas(state)?;
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    Ok(())
}

fn fizzle_stack_entry(state: &mut GameState, entry: &StackEntry) -> Result<(), EngineError> {
    remove_stack_entry_by_id(state, entry.id);
    state.remove_stack_trigger_meta(entry.id);
    if let StackEntryKind::Spell { object } = entry.kind {
        if entry.choices.is_copy {
            let _ = state.remove_object(object);
        } else {
            let dest = if entry.choices.exile_on_leave_stack {
                Zone::Exile
            } else {
                Zone::Graveyard
            };
            let _ = move_object(state, object, dest, MoveRawOptions::default(), None)?;
        }
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
    Ok(())
}

fn remove_stack_entry_by_id(state: &mut GameState, stack_id: StackItemId) {
    if let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) {
        state.stack_mut().remove(pos);
        state.stack_entries_mut().remove(pos);
    }
}

fn target_specs_for_stack_entry(state: &GameState, entry: &StackEntry) -> Vec<TargetSpec> {
    match &entry.kind {
        StackEntryKind::Spell { object } => {
            let mut out = Vec::new();
            if entry.choices.bestowed {
                out.push(crate::legality::bestow_target_spec());
            }
            if let Some(obj) = state.object(*object)
                && let Some(opts) = face_opts_for_object(state, *object)
            {
                let eff_def = opts.copiable_state.map_or(obj.card_def, |c| c.card_def);
                if let Some(spell) = active_stack_spell_ability(state.db(), eff_def, &opts) {
                    out.extend(spell.targets.iter().cloned());
                    if let Some(modal) = &spell.modal {
                        for &m_idx in &entry.choices.modes {
                            if let Some(mode) = modal.modes.get(usize::from(m_idx)) {
                                out.extend(mode.targets.iter().cloned());
                            }
                        }
                    }
                }
            }
            out
        }
        StackEntryKind::Activated {
            source,
            ability_idx,
            lki,
        } => {
            if state.object(*source).is_some()
                && let Some(res) = resolve_activated_abilities(state, *source)
                    .into_iter()
                    .find(|a| usize::from(a.ability_index) == *ability_idx)
            {
                let mut out = res.ability.targets.clone();
                if let Some(modal) = &res.ability.modal {
                    for &m_idx in &entry.choices.modes {
                        if let Some(mode) = modal.modes.get(usize::from(m_idx)) {
                            out.extend(mode.targets.iter().cloned());
                        }
                    }
                }
                return out;
            }
            if let Some(ab) = fallback_activated_ability_from_lki(
                state,
                *source,
                lki,
                entry.choices.face_index,
                *ability_idx,
            ) {
                let mut out = ab.targets.clone();
                if let Some(modal) = &ab.modal {
                    for &m_idx in &entry.choices.modes {
                        if let Some(mode) = modal.modes.get(usize::from(m_idx)) {
                            out.extend(mode.targets.iter().cloned());
                        }
                    }
                }
                return out;
            }
            Vec::new()
        }
        StackEntryKind::Triggered { .. } => {
            if let Some(meta) = state.stack_trigger_meta(entry.id) {
                let mut out = meta.targets.clone();
                if let Some(modal) = &meta.modal {
                    for &m_idx in &entry.choices.modes {
                        if let Some(mode) = modal.modes.get(usize::from(m_idx)) {
                            out.extend(mode.targets.iter().cloned());
                        }
                    }
                }
                return out;
            }
            Vec::new()
        }
    }
}

fn filter_illegal_targets_on_entry(
    state: &mut GameState,
    stack_id: StackItemId,
    specs: &[TargetSpec],
    chosen_by_spec: &[Vec<TargetRef>],
) {
    let filtered_by_spec: Vec<Vec<TargetRef>> = {
        let Some(entry) = state.stack_entries().iter().find(|e| e.id == stack_id) else {
            return;
        };
        let info = build_stack_entry_target_source_info(state, entry, chosen_by_spec);
        let mut out = Vec::with_capacity(chosen_by_spec.len());
        for (si, chosen) in chosen_by_spec.iter().enumerate() {
            let Some(spec) = specs.get(si) else {
                out.push(Vec::new());
                continue;
            };
            let local_info = TargetSourceInfo {
                targets_by_spec: &chosen_by_spec[..si],
                ..info.clone()
            };
            let mut accepted: Vec<TargetRef> = Vec::new();
            for &t in chosen {
                if is_target_legal_for_spec(state, spec, &local_info, &accepted, t) {
                    accepted.push(t);
                }
            }
            out.push(accepted);
        }
        out
    };
    let flat: Vec<TargetRef> = filtered_by_spec.iter().flatten().copied().collect();
    if let Some(pos) = state.stack_entries().iter().position(|e| e.id == stack_id) {
        state.stack_entries_mut()[pos].targets = flat.clone();
        state.stack_entries_mut()[pos].choices.targets_by_spec = filtered_by_spec;
        state.stack_entries_mut()[pos]
            .choices
            .divisions
            .retain(|(t, _)| flat.contains(t));
    }
}

type ExtractedResolutionPayload = (
    Vec<EffectStep>,
    Vec<ResolutionBinding>,
    Option<(ObjectId, String)>,
    Option<ObjectId>,
    Option<Arc<LkiSnapshot>>,
);

fn extract_resolution_payload(
    state: &GameState,
    entry: &StackEntry,
) -> Result<ExtractedResolutionPayload, EngineError> {
    match &entry.kind {
        StackEntryKind::Spell { object } => {
            let obj = state.object(*object).ok_or_else(|| {
                EngineError::IllegalAction("resolving spell object missing".into())
            })?;
            let opts = face_opts_for_object(state, *object);
            let eff_def = opts
                .as_ref()
                .and_then(|o| o.copiable_state.map(|c| c.card_def))
                .unwrap_or(obj.card_def);
            if let Some(opts) = &opts
                && let Some(spell) = active_stack_spell_ability(state.db(), eff_def, opts)
            {
                let steps =
                    assemble_modal_steps(spell.modal.as_ref(), &entry.choices.modes, &spell.effect);
                return Ok((steps, Vec::new(), None, Some(*object), None));
            }
            if !obj.is_face_down()
                && let Some(card) = state.db().get(eff_def)
                && let Some(face) = card.faces.get(usize::from(entry.choices.face_index))
                && let Some(u) = face.abilities.iter().find_map(|ab| match ab {
                    AbilityDef::Unsupported(u) => Some(u),
                    _ => None,
                })
                && !spell_resolves_as_permanent(state, *object, &entry.choices)
            {
                return Ok((
                    Vec::new(),
                    Vec::new(),
                    Some((*object, u.text.clone())),
                    Some(*object),
                    None,
                ));
            }
            Ok((Vec::new(), Vec::new(), None, Some(*object), None))
        }
        StackEntryKind::Activated {
            source,
            ability_idx,
            lki,
        } => {
            if state.object(*source).is_some()
                && let Some(res) = resolve_activated_abilities(state, *source)
                    .into_iter()
                    .find(|a| usize::from(a.ability_index) == *ability_idx)
            {
                let steps = assemble_modal_steps(
                    res.ability.modal.as_ref(),
                    &entry.choices.modes,
                    &res.ability.effect,
                );
                return Ok((
                    steps,
                    Vec::new(),
                    None,
                    Some(*source),
                    Some(Arc::clone(lki)),
                ));
            }
            if let Some(ab) = fallback_activated_ability_from_lki(
                state,
                *source,
                lki,
                entry.choices.face_index,
                *ability_idx,
            ) {
                let steps =
                    assemble_modal_steps(ab.modal.as_ref(), &entry.choices.modes, &ab.effect);
                return Ok((
                    steps,
                    Vec::new(),
                    None,
                    Some(*source),
                    Some(Arc::clone(lki)),
                ));
            }
            Ok((
                Vec::new(),
                Vec::new(),
                None,
                Some(*source),
                Some(Arc::clone(lki)),
            ))
        }
        StackEntryKind::Triggered { source, lki, .. } => {
            if let Some(meta) = state.stack_trigger_meta(entry.id) {
                let steps =
                    assemble_modal_steps(meta.modal.as_ref(), &entry.choices.modes, &meta.effect);
                return Ok((
                    steps,
                    meta.bindings.clone(),
                    None,
                    Some(*source),
                    Some(Arc::clone(lki)),
                ));
            }
            Ok((
                Vec::new(),
                Vec::new(),
                None,
                Some(*source),
                Some(Arc::clone(lki)),
            ))
        }
    }
}

fn assemble_modal_steps(
    modal: Option<&ModalDef>,
    chosen_modes: &[u16],
    base_effect: &[EffectStep],
) -> Vec<EffectStep> {
    let Some(modal) = modal else {
        return base_effect.to_vec();
    };
    let mut out = Vec::new();
    out.extend(modal.prefix_effect.iter().cloned());
    for &idx in chosen_modes {
        if let Some(mode) = modal.modes.get(usize::from(idx)) {
            out.extend(mode.effect.iter().cloned());
        }
    }
    out.extend(modal.suffix_effect.iter().cloned());
    if !base_effect.is_empty() && out.is_empty() {
        out.extend(base_effect.iter().cloned());
    }
    out
}

fn fallback_activated_ability_from_lki<'a>(
    state: &'a GameState,
    source: ObjectId,
    lki: &LkiSnapshot,
    face_index: u8,
    ability_idx: usize,
) -> Option<&'a mtg_ir::ActivatedAbility> {
    let card_def = if lki.card_def != TOKEN_CARD_DEF_ID {
        Some(lki.card_def)
    } else {
        state.lki(source).map(|s| s.card_def)
    }?;
    let card = state.db().get(card_def)?;
    let face = card
        .faces
        .get(usize::from(face_index))
        .or_else(|| card.faces.first())?;
    match face.abilities.get(ability_idx) {
        Some(AbilityDef::Activated(a)) => Some(a),
        _ => face.abilities.iter().find_map(|ab| match ab {
            AbilityDef::Activated(a) => Some(a),
            _ => None,
        }),
    }
}

fn spell_resolves_as_permanent(
    state: &GameState,
    object: ObjectId,
    choices: &StackChoices,
) -> bool {
    if choices.alt_cast_kind == AltCastKind::AdventureHalf
        || choices.alt_cast_kind == AltCastKind::OmenHalf
    {
        return false;
    }
    if choices.bestowed || choices.mutating {
        return true;
    }
    if let Some(obj) = state.object(object) {
        if obj.is_face_down() {
            return true;
        }
        if let Some(view) = active_face_view(state, object) {
            return view.types.intersects(CardTypeMask::PERMANENT);
        }
        if let Some(opts) = face_opts_for_object(state, object)
            && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
        {
            return view.types.intersects(CardTypeMask::PERMANENT);
        }
    }
    false
}

fn is_aura_spell(state: &GameState, object: ObjectId) -> bool {
    let Some(aura_sid) = state.db().subtype_id("Aura") else {
        return false;
    };
    active_face_view(state, object).is_some_and(|v| {
        v.types.contains(CardTypeMask::ENCHANTMENT) && v.subtypes.contains(&aura_sid)
    })
}

fn apply_mutate_merge(
    state: &mut GameState,
    spell_obj: ObjectId,
    target_perm: ObjectId,
    controller: PlayerId,
    mutate_under: bool,
) -> Result<(), EngineError> {
    let Some(spell_card_def) = state.object(spell_obj).map(|o| o.card_def) else {
        return Ok(());
    };
    let spell_is_token = state.object(spell_obj).is_some_and(GameObject::is_token);
    let spell_fi = state.object(spell_obj).map_or(0, |o| o.active_face_index());

    let mutate_count = {
        let existing_card_def = state
            .object(target_perm)
            .map_or(TOKEN_CARD_DEF_ID, |o| o.card_def);
        let existing_is_token = state.object(target_perm).is_some_and(GameObject::is_token);
        let rare = state
            .rare_or_default(target_perm)
            .ok_or_else(|| EngineError::IllegalAction("mutate target missing".into()))?;
        if rare.merged_components.is_empty() && existing_card_def != TOKEN_CARD_DEF_ID {
            rare.merged_components.push(MergedComponent {
                card_def: existing_card_def,
                is_token: existing_is_token,
            });
        }
        let new_comp = MergedComponent {
            card_def: spell_card_def,
            is_token: spell_is_token,
        };
        if mutate_under {
            rare.merged_components.push(new_comp);
        } else {
            rare.merged_components.insert(0, new_comp);
        }
        let len = rare.merged_components.len() as u16;
        if !mutate_under && let Some(perm) = state.object_mut(target_perm) {
            perm.card_def = spell_card_def;
            perm.active_face_index = spell_fi;
        }
        len
    };

    let _ = state.remove_object(spell_obj);
    state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
    emit_event(
        state,
        AtomicEventKind::Mutates {
            player: controller,
            object: target_perm,
            mutate_count,
        },
    );
    Ok(())
}

fn is_stack_item_uncounterable(state: &GameState, entry: &StackEntry) -> bool {
    let StackEntryKind::Spell { object } = entry.kind else {
        return false;
    };
    let cache = state.evaluate_board();
    if let Some(extras) = cache.extra(object)
        && extras
            .granted_abilities
            .iter()
            .any(|g| matches!(g, crate::layers::predicates::GrantedAbilityEntry::Inline { ability, .. } if matches!(ability.as_ref(), AbilityDef::Static(st) if matches!(st.static_def.body, mtg_ir::StaticBody::Restriction { restriction: mtg_ir::RestrictionKind::CantBeCountered, .. }))))
    {
        return true;
    }
    if let Some(obj) = state.object(object)
        && let Some(card) = state.db().get(obj.card_def)
        && let Some(face) = card.faces.get(usize::from(entry.choices.face_index))
    {
        return face.abilities.iter().any(|ab| {
            matches!(
                ab,
                AbilityDef::Static(st)
                    if matches!(
                        st.static_def.body,
                        mtg_ir::StaticBody::Restriction {
                            restriction: mtg_ir::RestrictionKind::CantBeCountered,
                            ..
                        }
                    )
            )
        });
    }
    false
}

fn build_stack_entry_lki(state: &GameState, entry: &StackEntry) -> LkiSnapshot {
    match &entry.kind {
        StackEntryKind::Spell { object } => snapshot_for_live_object(state, *object)
            .unwrap_or_else(|| LkiSnapshot {
                object_id: *object,
                card_def: TOKEN_CARD_DEF_ID,
                name: String::new(),
                owner: entry.controller,
                controller: entry.controller,
                last_zone: Zone::Stack,
                pt: None,
                types: CardTypeMask::empty(),
                subtypes: SmallVec::new(),
                supertypes: mtg_ir::SupertypeMask::empty(),
                colors: mtg_ir::ColorMask::empty(),
                mana_value: 0,
                keywords: SmallVec::new(),
                counters: SmallVec::new(),
                damage_marked: 0,
                is_token: false,
                is_commander: false,
                tapped: false,
                phased_out: false,
                face_down: false,
                lost_all_abilities: false,
                has_all_creature_types: false,
                attached_ids: SmallVec::new(),
                chosen_color: None,
                chosen_subtype: None,
                chosen_card_name: None,
                active_face_index: entry.choices.face_index,
            }),
        StackEntryKind::Activated { lki, .. } | StackEntryKind::Triggered { lki, .. } => {
            (**lki).clone()
        }
    }
}

fn active_or_next_alive_player(state: &GameState) -> PlayerId {
    let active = state.active_player();
    if state.player(active).is_some_and(|p| !p.lost) {
        return active;
    }
    let n = state.players().len();
    for offset in 1..=n {
        let seat = (usize::from(active.0) + offset) % n;
        let pid = PlayerId(seat as u8);
        if state.player(pid).is_some_and(|p| !p.lost) {
            return pid;
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use mtg_ir::{
        AbilityDef, Affected, CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask,
        Characteristics, ColorMask, EffectOp, EffectStep, FaceDef, IR_V2_VERSION, InternTables,
        IrV2Bundle, ManaCost, ObjectFilter, RestrictionKind, SpellAbility, StaticAbility,
        StaticBody, StaticDef, SupertypeMask, TargetKind, TargetSpec,
    };
    use std::sync::Arc;

    fn synth_stack_state() -> GameState {
        let spell = CardDef {
            oracle_id: "synth_bolt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bolt".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            condition: None,
                            body: StaticBody::Restriction {
                                affected: Affected::SelfObj,
                                restriction: RestrictionKind::CantBeCountered,
                            },
                        },
                        span: Default::default(),
                    }),
                    AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![TargetSpec {
                            kind: TargetKind::Object,
                            filter: ObjectFilter::default(),
                            player: None,
                            min: 1,
                            max: 1,
                            same_owner: false,
                            condition: None,
                            other_target: false,
                            divide_total: None,
                            with_single_target: false,
                            dynamic_count: None,
                            dynamic_up_to: false,
                            max_total_mana_value: None,
                            also_spells: false,
                            per_opponent: false,
                            different_controllers: false,
                            chosen_by_event_player: false,
                            shares_card_type_with_target: None,
                            zones: mtg_ir::ZoneMask::BATTLEFIELD,
                        }],
                        effect: vec![EffectStep {
                            op: EffectOp::GainLife {
                                set_life_total: false,
                            },
                            subject: mtg_ir::Selector::PlayerRef {
                                player: mtg_ir::PlayerRef::You,
                            },
                            actor: None,
                            amount: Some(Box::new(mtg_ir::Amount::Const { n: 2 })),
                            optional: None,
                            gate: None,
                            bind_as: None,
                            do_this_once_per_turn: false,
                            reflexive_trigger: None,
                            span: Default::default(),
                        }],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Default::default(),
                    }),
                ],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let land = CardDef {
            oracle_id: "synth_stack_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Stack Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Stack Land".into(),
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![land, spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 37).unwrap()
    }

    #[test]
    fn synth_spell_fizzle_cant_be_countered_and_copy() {
        let mut state = synth_stack_state();
        let card_oid = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(card_oid).unwrap().card_def = CardDefId(1);
        let mv = move_object(
            &mut state,
            card_oid,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let stack_oid = match mv {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected stack move"),
        };

        // Pick an invalid / non-battlefield target -> spell fizzles on resolution (`CR 608.2b`).
        let dead_target = state.player(PlayerId(1)).unwrap().library[0];
        let sid = state.stack_entries().last().unwrap().id;
        assert_eq!(
            state.stack_entries().last().unwrap().kind,
            StackEntryKind::Spell { object: stack_oid }
        );
        state.stack_entries_mut().last_mut().unwrap().targets =
            vec![TargetRef::Object { id: dead_target }];

        // CantBeCountered keeps spell on the stack.
        counter_stack_item(&mut state, sid, None, PlayerId(1), None).unwrap();
        assert_eq!(state.stack().len(), 1);

        // Copy stack item creates a second copy above it.
        let copy_id = copy_stack_item(&mut state, sid, PlayerId(1), None).unwrap();
        assert_eq!(state.stack().len(), 2);
        assert_eq!(state.stack_entries().last().unwrap().id, copy_id);

        // Resolve top (copy) -> fizzles because dead_target is not on battlefield; life stays 20!
        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.player(PlayerId(1)).unwrap().life, 20);
        assert_eq!(state.stack().len(), 1);

        // Resolve original -> fizzles to graveyard!
        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert!(state.stack().is_empty());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 20);
    }

    fn empty_kw_def(keyword: mtg_ir::KeywordId, cost: Option<ManaCost>) -> mtg_ir::KeywordDef {
        mtg_ir::KeywordDef {
            keyword,
            cost,
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
        }
    }

    fn simple_target_spec(kind: TargetKind, zones: mtg_ir::ZoneMask) -> TargetSpec {
        TargetSpec {
            kind,
            filter: ObjectFilter::default(),
            player: None,
            min: 1,
            max: 1,
            same_owner: false,
            condition: None,
            other_target: false,
            divide_total: None,
            with_single_target: false,
            dynamic_count: None,
            dynamic_up_to: false,
            max_total_mana_value: None,
            also_spells: false,
            per_opponent: false,
            different_controllers: false,
            chosen_by_event_player: false,
            shares_card_type_with_target: None,
            zones,
        }
    }

    #[test]
    fn synth_bestow_target_revalidation_and_etb_replacement_pause() {
        let mut tables = InternTables::default();
        let kw_bestow = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("bestow".into());

        let land = CardDef {
            oracle_id: "synth_bestow_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bestow Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bestow Land".into(),
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
        let host_bear = CardDef {
            oracle_id: "synth_bestow_host".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bestow Host".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bestow Host".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
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
        let bestow_card = CardDef {
            oracle_id: "synth_bestow_aura".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bestow Aura".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bestow Aura".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{W}"),
                    types: CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![
                    AbilityDef::Keyword(mtg_ir::KeywordAbility {
                        keyword: empty_kw_def(kw_bestow, Some(ManaCost::parse("{1}{W}"))),
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Replacement(mtg_ir::ReplacementAbility {
                        replacement: mtg_ir::ReplacementDef {
                            replaces: mtg_ir::EventPattern {
                                event: mtg_ir::EventKind::ZoneChange,
                                filter: mtg_ir::EventFilter {
                                    or_self: true,
                                    detail: Some(mtg_ir::EventDetail::ZoneChange {
                                        from: mtg_ir::ZoneMask::empty(),
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
                                mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                    pay_life_or_enter_tapped: Some(2),
                                    ..Default::default()
                                }),
                            },
                            self_replacement: true,
                            who: None,
                            condition: None,
                        },
                        span: Default::default(),
                    }),
                ],
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
                cards: vec![land, host_bear, bestow_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 41).unwrap();
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_decision(DecisionState::Priority);

        let host_hand = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(host_hand).unwrap().card_def = CardDefId(1);
        let host = match move_object(
            &mut state,
            host_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected host on BF"),
        };

        // Case A: Bestow with legal host pauses on ETB replacement, then attaches after resume.
        let bestow_hand = state.player(PlayerId(0)).unwrap().hand[1];
        state.object_mut(bestow_hand).unwrap().card_def = CardDefId(2);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let stack_oid = match move_object(
            &mut state,
            bestow_hand,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected stack move"),
        };
        state
            .object_mut(stack_oid)
            .unwrap()
            .flags
            .insert(ObjectFlags::BESTOWED);
        state.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            se.choices.bestowed = true;
            se.choices.alt_cast_kind = AltCastKind::Bestow;
            se.targets = vec![TargetRef::Object { id: host }];
            se.choices.targets_by_spec = vec![vec![TargetRef::Object { id: host }]];
        }

        assert!(!resolve_top_of_stack(&mut state).unwrap());
        let pr = state
            .pending_replacement()
            .expect("expected ETB pay-life replacement prompt")
            .clone();
        assert_eq!(
            state.pending_replacement_resume(),
            Some(ReplacementResumeHook::StackResolutionFinish {
                attach_target: Some(host),
                is_adventure: false,
            })
        );
        crate::choices::apply_choice_answer(
            &mut state,
            pr.chooser,
            pr.prompt_id,
            crate::action::ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();

        let aura_bf = *state
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .find(|&&id| id != host)
            .expect("bestowed aura should be on battlefield");
        assert_eq!(state.object(aura_bf).unwrap().attached_to, Some(host));
        assert!(state.event_log().iter().any(|ev| matches!(
            ev.kind,
            AtomicEventKind::AttachmentChanged {
                attachment,
                host: h,
                attached: true
            } if attachment == aura_bf && h == host
        )));

        // Case B: Bestow whose target leaves BF before resolution strips bestowed and resolves as creature.
        let bestow_hand2 = state.player(PlayerId(0)).unwrap().hand[2];
        state.object_mut(bestow_hand2).unwrap().card_def = CardDefId(2);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let stack_oid2 = match move_object(
            &mut state,
            bestow_hand2,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected stack move"),
        };
        state
            .object_mut(stack_oid2)
            .unwrap()
            .flags
            .insert(ObjectFlags::BESTOWED);
        state.mark_dirty(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
        let dead_id = state.player(PlayerId(1)).unwrap().library[0];
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            se.choices.bestowed = true;
            se.choices.alt_cast_kind = AltCastKind::Bestow;
            se.targets = vec![TargetRef::Object { id: dead_id }];
            se.choices.targets_by_spec = vec![vec![TargetRef::Object { id: dead_id }]];
        }
        assert!(!resolve_top_of_stack(&mut state).unwrap());
        let pr2 = state.pending_replacement().unwrap().clone();
        assert_eq!(
            state.pending_replacement_resume(),
            Some(ReplacementResumeHook::StackResolutionFinish {
                attach_target: None,
                is_adventure: false,
            })
        );
        crate::choices::apply_choice_answer(
            &mut state,
            pr2.chooser,
            pr2.prompt_id,
            crate::action::ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();
        let unbestowed = state
            .player(PlayerId(0))
            .unwrap()
            .battlefield
            .iter()
            .copied()
            .find(|&id| id != host && id != aura_bf)
            .expect("should resolve as creature");
        assert_eq!(state.object(unbestowed).unwrap().attached_to, None);
        assert!(
            !state
                .object(unbestowed)
                .unwrap()
                .flags
                .contains(ObjectFlags::BESTOWED)
        );
    }

    #[test]
    fn synth_modal_plus_base_target_order_and_activated_lki_modal() {
        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let land = CardDef {
            oracle_id: "synth_modal_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Modal Land".into(),
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
        let modal_def = ModalDef {
            min: 1,
            max: 1,
            allow_repeat: false,
            once_per_turn_unique: false,
            up_to: false,
            conditional_choose_all: None,
            modes: vec![mtg_ir::ModalMode {
                targets: vec![simple_target_spec(
                    TargetKind::Player,
                    mtg_ir::ZoneMask::empty(),
                )],
                effect: vec![EffectStep {
                    op: EffectOp::GainLife {
                        set_life_total: false,
                    },
                    subject: mtg_ir::Selector::Target { index: 1 },
                    actor: None,
                    amount: Some(Box::new(mtg_ir::Amount::Const { n: 3 })),
                    optional: None,
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span: Default::default(),
                }],
                span: Default::default(),
            }],
            per_mode_costs: vec![],
            unchosen_permanent: false,
            opponent_chooses: None,
            pawprint_budget: None,
            pawprint_costs: vec![],
            dynamic_max: None,
            random_mode: false,
            prefix_effect: vec![],
            suffix_effect: vec![],
            conditional_choose_range: None,
            conditional_not_random: None,
            different_player_targets_per_mode: false,
        };
        let modal_spell = CardDef {
            oracle_id: "synth_modal_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Modal Spell".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![simple_target_spec(
                        TargetKind::Object,
                        mtg_ir::ZoneMask::BATTLEFIELD,
                    )],
                    effect: vec![],
                    modal: Some(modal_def.clone()),
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let modal_creature = CardDef {
            oracle_id: "synth_modal_creature".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal Creature".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Modal Creature".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "act0".into(),
                    cost: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        modes: vec![mtg_ir::ModalMode {
                            targets: vec![simple_target_spec(
                                TargetKind::Player,
                                mtg_ir::ZoneMask::empty(),
                            )],
                            effect: vec![EffectStep {
                                op: EffectOp::GainLife {
                                    set_life_total: false,
                                },
                                subject: mtg_ir::Selector::Target { index: 0 },
                                actor: None,
                                amount: Some(Box::new(mtg_ir::Amount::Const { n: 4 })),
                                optional: None,
                                gate: None,
                                bind_as: None,
                                do_this_once_per_turn: false,
                                reflexive_trigger: None,
                                span: Default::default(),
                            }],
                            span: Default::default(),
                        }],
                        ..modal_def
                    }),
                    timing: mtg_ir::Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: mtg_ir::ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
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
                cards: vec![land, modal_spell, modal_creature],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 42).unwrap();

        // (a) Base target illegal (library card) + modal player target legal -> PartiallyIllegal keeps player!
        let card_oid = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(card_oid).unwrap().card_def = CardDefId(1);
        move_object(
            &mut state,
            card_oid,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let dead_bf = state.player(PlayerId(1)).unwrap().library[0];
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            se.choices.modes.push(0);
            se.targets = vec![
                TargetRef::Object { id: dead_bf },
                TargetRef::Player { id: PlayerId(0) },
            ];
            se.choices.targets_by_spec = vec![
                vec![TargetRef::Object { id: dead_bf }],
                vec![TargetRef::Player { id: PlayerId(0) }],
            ];
        }
        let specs = target_specs_for_stack_entry(&state, state.stack_entries().last().unwrap());
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].kind, TargetKind::Object);
        assert_eq!(specs[1].kind, TargetKind::Player);
        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 23);

        // (b) Activated modal ability whose source left BF (LKI fallback) still revalidates modal target.
        let src_hand = state.player(PlayerId(0)).unwrap().hand[1];
        state.object_mut(src_hand).unwrap().card_def = CardDefId(2);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let src_bf = match move_object(
            &mut state,
            src_hand,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected BF move"),
        };
        let lki = Arc::new(snapshot_for_live_object(&state, src_bf).unwrap());
        move_object(
            &mut state,
            src_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let mut act_choices = StackChoices::default();
        act_choices.modes.push(0);
        act_choices.targets_by_spec = vec![vec![TargetRef::Player { id: PlayerId(0) }]];
        let act_sid = state.allocate_stack_id();
        state.stack_mut().push(src_bf);
        state.stack_entries_mut().push(StackEntry {
            id: act_sid,
            kind: StackEntryKind::Activated {
                source: src_bf,
                ability_idx: 0,
                lki,
            },
            controller: PlayerId(0),
            choices: act_choices,
            targets: vec![TargetRef::Player { id: PlayerId(0) }],
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        });
        state.mark_dirty(DirtyMask::TOPOLOGY);
        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.player(PlayerId(0)).unwrap().life, 27);
    }

    #[test]
    fn synth_adventure_replacement_pause_sets_on_an_adventure() {
        let tables = InternTables::default();
        let land = CardDef {
            oracle_id: "synth_adv_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Adv Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Adv Land".into(),
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
        let adv = CardDef {
            oracle_id: "synth_adv_card".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Adv Knight".into(),
            layout: CardLayout::Adventure,
            faces: vec![
                FaceDef {
                    name: "Synth Adv Knight".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}{W}"),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::W,
                        color_indicator: None,
                        pt: Some(mtg_ir::PrintedPt::Fixed {
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
                },
                FaceDef {
                    name: "Synth Adv Quest".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{W}"),
                        types: CardTypeMask::SORCERY,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::W,
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![],
                        effect: vec![EffectStep {
                            op: EffectOp::GainLife {
                                set_life_total: false,
                            },
                            subject: mtg_ir::Selector::PlayerRef {
                                player: mtg_ir::PlayerRef::You,
                            },
                            actor: None,
                            amount: Some(Box::new(mtg_ir::Amount::Const { n: 2 })),
                            optional: None,
                            gate: None,
                            bind_as: None,
                            do_this_once_per_turn: false,
                            reflexive_trigger: None,
                            span: Default::default(),
                        }],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Default::default(),
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
            ],
            fully_supported: true,
        };
        let make_exile_rep = |name: &str, tapped: bool| CardDef {
            oracle_id: name.to_ascii_lowercase(),
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
                                    from: mtg_ir::ZoneMask::empty(),
                                    to: Some(Zone::Exile),
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
                            mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                to: Some(Zone::Exile),
                                tapped: Some(tapped),
                                ..Default::default()
                            }),
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

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    land,
                    adv,
                    make_exile_rep("Synth Exile Tapper", true),
                    make_exile_rep("Synth Exile Untapper", false),
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 43).unwrap();
        for (idx, def_id) in [CardDefId(2), CardDefId(3)].into_iter().enumerate() {
            let h = state.player(PlayerId(0)).unwrap().hand[idx];
            state.object_mut(h).unwrap().card_def = def_id;
            move_object(
                &mut state,
                h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let adv_h = state.player(PlayerId(0)).unwrap().hand[2];
        state.object_mut(adv_h).unwrap().card_def = CardDefId(1);
        move_object(
            &mut state,
            adv_h,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            se.choices.alt_cast_kind = AltCastKind::AdventureHalf;
            se.choices.face_index = 1;
        }
        assert!(!resolve_top_of_stack(&mut state).unwrap());
        let pr = state
            .pending_replacement()
            .expect("expected exile replacement prompt")
            .clone();
        assert_eq!(
            state.pending_replacement_resume(),
            Some(ReplacementResumeHook::StackResolutionFinish {
                attach_target: None,
                is_adventure: true,
            })
        );
        crate::choices::apply_choice_answer(
            &mut state,
            pr.chooser,
            pr.prompt_id,
            crate::action::ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let ex_id = state.player(PlayerId(0)).unwrap().exile[0];
        assert!(state.rare(ex_id).is_some_and(|r| r.on_an_adventure));
    }

    #[test]
    fn synth_divided_partial_illegality_cr608_2b() {
        let mut tables = InternTables::default();
        tables.keywords.push("hexproof".into());
        tables.counters.push("+1/+1".into());
        let kw_hexproof = mtg_ir::KeywordId(0);
        let plus_counter = mtg_ir::CounterId(0);

        let land = CardDef {
            oracle_id: "synth_div_cr608_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Div Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Div Land".into(),
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
        let bear = CardDef {
            oracle_id: "synth_div_cr608_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Div Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Div Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 3,
                        toughness: 3,
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
        let mut div_spec = simple_target_spec(TargetKind::Object, mtg_ir::ZoneMask::BATTLEFIELD);
        div_spec.filter.types_any = CardTypeMask::CREATURE;
        div_spec.min = 1;
        div_spec.max = 4;
        div_spec.divide_total = Some(Box::new(mtg_ir::Amount::Const { n: 4 }));

        let div_dmg_spell = CardDef {
            oracle_id: "synth_div_cr608_dmg".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Divided Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Divided Bolt".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{R}"),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![div_spec.clone()],
                    effect: vec![EffectStep {
                        op: EffectOp::Damage {
                            source: None,
                            also_players: None,
                            divided: true,
                            excess_to_controller: false,
                            excess_to_controller_if_trample: false,
                            or_planeswalker_controlled_by: None,
                            damage_attacked_target_by_self: false,
                            soulfire_exile_damage_per_target: false,
                            each_creature_deals: None,
                        },
                        subject: mtg_ir::Selector::Target { index: 0 },
                        actor: None,
                        amount: Some(Box::new(mtg_ir::Amount::Const { n: 4 })),
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Default::default(),
                    }],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let div_ctr_spell = CardDef {
            oracle_id: "synth_div_cr608_ctr".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Divided Counters".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Divided Counters".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{G}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![div_spec],
                    effect: vec![EffectStep {
                        op: EffectOp::PutCounters {
                            counter: Some(plus_counter),
                            multi_counters: vec![],
                            select: mtg_ir::SelectionMod::default(),
                            divided: true,
                            allow_exile_counters: false,
                            remove_instead_of_add: false,
                            copy_counters_from: None,
                            choose_counter_on_subject_and_add_one: false,
                            conditional_counter_on_type: None,
                            cast_bolas_bonus_loyalty: false,
                        },
                        subject: mtg_ir::Selector::Target { index: 0 },
                        actor: None,
                        amount: Some(Box::new(mtg_ir::Amount::Const { n: 4 })),
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Default::default(),
                    }],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
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
                cards: vec![land, bear, div_dmg_spell, div_ctr_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 707).unwrap();

        let spawn_p1_bear = |state: &mut GameState, idx: usize| -> ObjectId {
            let h = state.player(PlayerId(1)).unwrap().hand[idx];
            state.object_mut(h).unwrap().card_def = CardDefId(1);
            state.mark_dirty(DirtyMask::TOPOLOGY);
            match move_object(state, h, Zone::Battlefield, MoveRawOptions::default(), None).unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => panic!("expected BF move"),
            }
        };
        let bear_a = spawn_p1_bear(&mut state, 0);
        let bear_b = spawn_p1_bear(&mut state, 1);

        // Put Synth Divided Bolt on stack targeting [bear_a, bear_b] with divisions [(bear_a, 2), (bear_b, 2)].
        let bolt_h = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(bolt_h).unwrap().card_def = CardDefId(2);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let bolt_stack = match move_object(
            &mut state,
            bolt_h,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected Stack move"),
        };
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            assert_eq!(se.kind, StackEntryKind::Spell { object: bolt_stack });
            se.targets = vec![
                TargetRef::Object { id: bear_a },
                TargetRef::Object { id: bear_b },
            ];
            se.choices.targets_by_spec = vec![se.targets.clone()];
            se.choices.divisions = smallvec::smallvec![
                (TargetRef::Object { id: bear_a }, 2),
                (TargetRef::Object { id: bear_b }, 2),
            ];
        }

        // Grant bear_a hexproof before resolution -> bear_a becomes illegal, bear_b stays legal.
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(1),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                bear_a
            ]),
            modifier: mtg_ir::LayerMod::AddKeywords {
                keywords: smallvec::smallvec![kw_hexproof],
                protection_attachment_exemption: None,
            },
            duration: mtg_ir::Duration::EndOfTurn,
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

        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.object(bear_a).unwrap().damage_marked, 0);
        assert_eq!(
            state.object(bear_b).unwrap().damage_marked,
            2,
            "CR 608.2b: legal target keeps locked-in 2 damage, never redistributed to 4"
        );

        // Also test divided PutCounters under partial illegality (bear_a still has hexproof).
        let ctr_h = state.player(PlayerId(0)).unwrap().hand[1];
        state.object_mut(ctr_h).unwrap().card_def = CardDefId(3);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let _ctr_stack = match move_object(
            &mut state,
            ctr_h,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected Stack move"),
        };
        {
            let se = state.stack_entries_mut().last_mut().unwrap();
            se.targets = vec![
                TargetRef::Object { id: bear_a },
                TargetRef::Object { id: bear_b },
            ];
            se.choices.targets_by_spec = vec![se.targets.clone()];
            se.choices.divisions = smallvec::smallvec![
                (TargetRef::Object { id: bear_a }, 2),
                (TargetRef::Object { id: bear_b }, 2),
            ];
        }
        assert!(resolve_top_of_stack(&mut state).unwrap());
        assert_eq!(state.counter_count(bear_a, plus_counter), 0);
        assert_eq!(state.counter_count(bear_b, plus_counter), 2);
    }
}
