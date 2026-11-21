use std::sync::Arc;

use mtg_ir::{CardTypeMask, GamePhase, RuleMod, SupertypeMask, Zone};
use smallvec::SmallVec;

use crate::action::{AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::choices::ReplacementResumeOutcome;
use crate::commander::{
    lethal_commander_damage_players, mark_commander_sba_declined, pending_commander_sba_returns,
    return_commander_to_command_zone,
};
use crate::eval::{BoardView, EvalContext};
use crate::events::{
    AtomicEventKind, BatchLkiEntry, build_batch_lki_snapshot, capture_granted_triggers_from_cache,
    emit_event, end_simultaneous_batch,
};
use crate::faces::{
    battle_starting_defense, face_opts_for_object, is_saga_card, resolve_object_faces,
    saga_final_chapter,
};
use crate::filter::matches_object_filter;
use crate::id::{ObjectId, PlayerId};
use crate::keywords::{
    enchant_legality_for_aura, has_indestructible, has_protection_from, has_umbra_armor,
};
use crate::state::{
    DirtyMask, EngineError, GameState, ObjectFlags, PendingPromptContext, PendingSbaBatchState,
    PromptOwner, ReplacementResumeHook, SbaPromptKind, StackEntryKind,
};
use crate::triggers::{check_state_triggers, flush_triggers};
use crate::zones::{
    MoveObjectOutcome, MoveRawOptions, cleanup_tokens_outside_battlefield, move_object,
};

const MAX_SBA_PASSES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbaOutcome {
    Done,
    Paused { prompt_id: u32, chooser: PlayerId },
    GameOver { winner: Option<PlayerId> },
}

/// CR 704.3: Iterative fixpoint SBA loop (max 32 passes).
pub fn check_sbas(state: &mut GameState) -> Result<SbaOutcome, EngineError> {
    let in_cleanup = state.phase() == GamePhase::Cleanup;
    check_sbas_with_cleanup(state, in_cleanup)
}

pub(crate) fn check_sbas_with_cleanup(
    state: &mut GameState,
    in_cleanup: bool,
) -> Result<SbaOutcome, EngineError> {
    if let DecisionState::GameOver { winner } = state.decision() {
        return Ok(SbaOutcome::GameOver { winner: *winner });
    }
    if state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
        || state.pending_prompt().is_some()
        || state.casting().is_some()
    {
        return Ok(SbaOutcome::Done);
    }

    for _pass in 0..MAX_SBA_PASSES {
        if let DecisionState::GameOver { winner } = state.decision() {
            return Ok(SbaOutcome::GameOver { winner: *winner });
        }

        crate::turn::expire_source_bound_copiable_backups(state);

        // Step 0: Pre-pass CR 704.8 LKI snapshot for all phased-in battlefield permanents
        // BEFORE counter annihilation or destruction mutates the board.
        let pre_pass_lki = capture_pre_pass_battlefield_lki(state);
        let mut any_taken = false;

        // Step 1: Combat validity (CR 506.4).
        if prune_invalid_combat_entries(state) {
            any_taken = true;
        }

        // Step 2: Counter annihilation (+1/+1 and -1/-1 cancel, CR 704.5q).
        if annihilate_plus_minus_counters(state) {
            any_taken = true;
        }

        // Step 3: Check attachments (Aura / Equipment / Fortification, CR 704.5m-n, 702.103f)
        // and permanent destruction SBAs (CR 704.5f-i, 704.5s, 704.5v-y).
        let mut sba_graveyard_batch: Vec<ObjectId> = Vec::new();
        if check_attachment_sbas(state, &mut sba_graveyard_batch) {
            any_taken = true;
        }
        if check_permanent_destruction_sbas(state, &mut sba_graveyard_batch) {
            any_taken = true;
        }

        // Step 4: Legend rule (CR 704.5j) — only if no pending LegendRule already.
        if let Some((owner, name, candidates)) = find_legend_rule_violation(state) {
            // Execute any already-queued destruction batch first if present, or pause on Legend Rule.
            if !sba_graveyard_batch.is_empty() {
                if let Some(paused) = execute_sba_graveyard_batch(
                    state,
                    &sba_graveyard_batch,
                    &pre_pass_lki,
                    in_cleanup,
                )? {
                    return Ok(paused);
                }
                continue;
            }
            let prompt_id = state.allocate_prompt_id();
            let req = ChoiceRequest::ChooseObjects {
                prompt_id,
                candidates: candidates.clone(),
                min: 1,
                max: 1,
            };
            state.set_pending_sba_in_cleanup(in_cleanup);
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: owner,
                request: req.clone(),
                owner: PromptOwner::Sba(SbaPromptKind::LegendRule {
                    owner,
                    name,
                    candidates,
                }),
            }));
            state.set_decision(DecisionState::Prompt { request: req });
            return Ok(SbaOutcome::Paused {
                prompt_id,
                chooser: owner,
            });
        }

        // Step 5: Simultaneous graveyard batch move (CR 704.8 / CR 603.10a).
        if !sba_graveyard_batch.is_empty() {
            if let Some(paused) =
                execute_sba_graveyard_batch(state, &sba_graveyard_batch, &pre_pass_lki, in_cleanup)?
            {
                return Ok(paused);
            }
            any_taken = true;
        }

        // Step 6: Token cleanup outside battlefield (CR 704.5d).
        if !cleanup_tokens_outside_battlefield(state).is_empty() {
            any_taken = true;
        }

        // Step 7: Commander return SBA (CR 903.9a / CR 704.6d).
        let pending_cmd = pending_commander_sba_returns(state);
        if let Some(&(owner, commander)) = pending_cmd.first() {
            let from_zone = state.object(commander).map_or(Zone::Graveyard, |o| o.zone);
            let prompt_id = state.allocate_prompt_id();
            let req = ChoiceRequest::YesNo {
                prompt_id,
                label: "Return your commander to the command zone?".into(),
            };
            state.set_pending_sba_in_cleanup(in_cleanup);
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: owner,
                request: req.clone(),
                owner: PromptOwner::Sba(SbaPromptKind::CommanderReturn {
                    owner,
                    commander,
                    from_zone,
                }),
            }));
            state.set_decision(DecisionState::Prompt { request: req });
            return Ok(SbaOutcome::Paused {
                prompt_id,
                chooser: owner,
            });
        }

        // Step 8: Simultaneous player loss SBAs (CR 704.5a-c, CR 903.10a, CR 104.3c, CR 104.4a).
        if let Some(game_over) = check_player_loss_sbas(state, &mut any_taken) {
            return Ok(game_over);
        }

        if !any_taken {
            break;
        }
    }

    // After fixpoint stabilizes, check state triggers (CR 603.8) and flush triggers.
    check_state_triggers(state);
    flush_triggers(state);
    let _ = crate::casting::prepare_next_untargeted_stack_trigger(state)?;
    Ok(SbaOutcome::Done)
}

/// Resumes `SbaPromptKind::{LegendRule, CommanderReturn}` from `Action::Answer`.
pub fn resume_sba_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<SbaOutcome, EngineError> {
    let pp = state
        .pending_prompt()
        .cloned()
        .ok_or_else(|| EngineError::IllegalAction("no pending SBA prompt to resume".into()))?;
    if pp.chooser != actor || pp.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "SBA prompt actor or prompt_id mismatch".into(),
        ));
    }
    let PromptOwner::Sba(kind) = pp.owner else {
        return Err(EngineError::IllegalAction(
            "pending prompt is not an SBA prompt".into(),
        ));
    };

    let in_cleanup = state.pending_sba_in_cleanup();

    match kind {
        SbaPromptKind::LegendRule {
            owner: _,
            name: _,
            candidates,
        } => {
            let keep_id = match answer {
                ChoiceAnswer::PickObject { id } if candidates.contains(&id) => id,
                _ => {
                    return Err(EngineError::IllegalAction(
                        "Legend Rule prompt requires picking one of the candidate permanents"
                            .into(),
                    ));
                }
            };
            state.set_pending_prompt(None);
            state.set_decision(DecisionState::Priority);
            let to_gy: Vec<ObjectId> = candidates.into_iter().filter(|&id| id != keep_id).collect();
            if !to_gy.is_empty() {
                let pre_lki = capture_pre_pass_battlefield_lki(state);
                if let Some(paused) =
                    execute_sba_graveyard_batch(state, &to_gy, &pre_lki, in_cleanup)?
                {
                    return Ok(paused);
                }
            }
        }
        SbaPromptKind::CommanderReturn {
            owner: _,
            commander,
            from_zone: _,
        } => {
            let accept = match answer {
                ChoiceAnswer::YesNo { value } => value,
                _ => {
                    return Err(EngineError::IllegalAction(
                        "Commander return prompt requires YesNo answer".into(),
                    ));
                }
            };
            state.set_pending_prompt(None);
            state.set_decision(DecisionState::Priority);
            if accept {
                return_commander_to_command_zone(state, commander)?;
            } else {
                mark_commander_sba_declined(state, commander);
            }
        }
    }

    state.set_pending_sba_in_cleanup(false);
    check_sbas_with_cleanup(state, in_cleanup)
}

/// Resumes a paused `Replacement` triggered during an SBA simultaneous graveyard move.
pub fn resume_sba_after_replacement(
    state: &mut GameState,
    _outcome: ReplacementResumeOutcome,
    in_cleanup: bool,
) -> Result<SbaOutcome, EngineError> {
    if let Some(batch) = state.take_pending_sba_batch() {
        let mut idx = 0usize;
        while idx < batch.remaining_graveyard_ids.len() {
            let id = batch.remaining_graveyard_ids[idx];
            idx += 1;
            if state
                .object(id)
                .is_none_or(|o| o.zone != Zone::Battlefield || o.is_phased_out())
            {
                continue;
            }
            let lki_eval = crate::events::pre_batch_lki(state, id).map(|(_, ev)| ev.clone());
            match move_object(
                state,
                id,
                Zone::Graveyard,
                MoveRawOptions::default(),
                lki_eval.as_ref(),
            )? {
                MoveObjectOutcome::Paused { prompt_id, chooser } => {
                    let rest = batch.remaining_graveyard_ids[idx..].to_vec();
                    state.set_pending_sba_batch(Some(PendingSbaBatchState {
                        remaining_graveyard_ids: rest,
                        in_cleanup,
                    }));
                    state.set_pending_replacement_resume(Some(ReplacementResumeHook::SbaLoop {
                        in_cleanup,
                    }));
                    return Ok(SbaOutcome::Paused { prompt_id, chooser });
                }
                MoveObjectOutcome::ExecuteInstead { .. }
                | MoveObjectOutcome::Moved(_)
                | MoveObjectOutcome::Skipped { .. } => {}
            }
        }
        end_simultaneous_batch(state);
    }

    state.set_pending_sba_in_cleanup(false);
    check_sbas_with_cleanup(state, in_cleanup)
}

/// CR 800.4: Cleans up an eliminated player's objects, stack items, combat entries,
/// continuous control effects, Monarch (CR 726.4), and Initiative (CR 725.4).
pub fn cleanup_eliminated_player(state: &mut GameState, eliminated: PlayerId) {
    // 1. Remove all stack entries controlled by `eliminated` (CR 800.4a).
    let mut stack_positions_to_remove: Vec<usize> = state
        .stack_entries()
        .iter()
        .enumerate()
        .filter_map(|(i, e)| (e.controller == eliminated).then_some(i))
        .collect();
    stack_positions_to_remove.sort_unstable();
    for pos in stack_positions_to_remove.into_iter().rev() {
        if pos < state.stack().len() {
            let stack_oid = state.stack()[pos];
            let entry = state.stack_entries()[pos].clone();
            state.stack_mut().remove(pos);
            state.stack_entries_mut().remove(pos);
            state.remove_stack_trigger_meta(entry.id);
            state
                .resolution_vms_mut()
                .retain(|vm| vm.stack_item != entry.id);
            state
                .resolution_frames_mut()
                .retain(|f| f.stack_item != entry.id);
            if matches!(entry.kind, StackEntryKind::Spell { .. }) {
                let _ = state.remove_object(stack_oid);
            }
        }
    }
    if let Some(cs) = state.casting()
        && cs.caster == eliminated
        && cs.kind == crate::state::CastKind::Spell
    {
        let cast_obj = cs.object;
        state.stack_mut().retain(|&oid| oid != cast_obj);
        if let Some(owner) = state
            .object(cast_obj)
            .filter(|o| o.zone == Zone::Stack)
            .map(|o| o.owner)
        {
            if owner == eliminated {
                let _ = state.remove_object(cast_obj);
            } else {
                let _ = crate::zones::move_object_raw(
                    state,
                    cast_obj,
                    Zone::Exile,
                    MoveRawOptions::default(),
                    None,
                );
            }
        }
    }

    // 2. Remove from combat (any attackers/blockers controlled by eliminated or attacking eliminated).
    {
        let keep_attackers: SmallVec<[ObjectId; 8]> = state
            .combat()
            .attackers
            .iter()
            .filter(|a| {
                let owned_or_ctrl = state
                    .object(a.attacker)
                    .is_some_and(|o| o.controller == eliminated || o.owner == eliminated);
                let target_elim = match a.target {
                    AttackTarget::Player { id } => id == eliminated,
                    AttackTarget::Planeswalker { id } | AttackTarget::Battle { id } => state
                        .object(id)
                        .is_some_and(|o| o.controller == eliminated || o.owner == eliminated),
                };
                !owned_or_ctrl && !target_elim
            })
            .map(|a| a.attacker)
            .collect();
        let keep_blockers: SmallVec<[ObjectId; 8]> = state
            .combat()
            .blockers
            .iter()
            .filter(|b| {
                let b_elim = state
                    .object(b.blocker)
                    .is_some_and(|o| o.controller == eliminated || o.owner == eliminated);
                let a_still = keep_attackers.contains(&b.attacker);
                !b_elim && a_still
            })
            .map(|b| b.blocker)
            .collect();
        let combat = state.combat_mut();
        combat
            .attackers
            .retain(|a| keep_attackers.contains(&a.attacker));
        combat
            .blockers
            .retain(|b| keep_blockers.contains(&b.blocker) && keep_attackers.contains(&b.attacker));
        combat
            .damage_assignment_order
            .retain(|(atk, _)| keep_attackers.contains(atk));
    }

    // 3. End control-changing effects giving control to `eliminated` (CR 800.4c).
    state.retain_continuous_effects(|eff| {
        !(eff.controller == eliminated
            && matches!(eff.modifier, mtg_ir::LayerMod::SetController { .. }))
    });

    // 4. Collect all owned objects in all zones and exile/remove objects controlled by `eliminated`.
    let mut owned_bf: Vec<ObjectId> = Vec::new();
    let mut owned_other: Vec<(ObjectId, Zone, PlayerId)> = Vec::new();
    let mut stolen_controlled: Vec<ObjectId> = Vec::new();
    let player_count = state.players().len();
    for seat in 0..player_count {
        let pid = PlayerId(seat as u8);
        let Some(p) = state.players().get(seat) else {
            continue;
        };
        for &id in &p.battlefield {
            if let Some(obj) = state.object(id) {
                if obj.owner == eliminated {
                    owned_bf.push(id);
                } else if obj.controller == eliminated {
                    stolen_controlled.push(id);
                }
            }
        }
        for (z, slice) in [
            (Zone::Hand, p.hand.as_slice()),
            (Zone::Library, p.library.as_slice()),
            (Zone::Graveyard, p.graveyard.as_slice()),
            (Zone::Exile, p.exile.as_slice()),
            (Zone::Command, p.command.as_slice()),
        ] {
            for &id in slice {
                if state.object(id).is_some_and(|o| o.owner == eliminated) {
                    owned_other.push((id, z, pid));
                }
            }
        }
    }

    // Exile stolen permanents controlled by `eliminated` (CR 800.4a).
    for id in stolen_controlled {
        let _ =
            crate::zones::move_object_raw(state, id, Zone::Exile, MoveRawOptions::default(), None);
    }

    // Remove all objects owned by `eliminated` from the game.
    for id in owned_bf {
        if let Some(p) = state.player_mut(eliminated) {
            p.battlefield.retain(|&x| x != id);
        }
        for seat in 0..player_count {
            if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
                p.battlefield.retain(|&x| x != id);
            }
        }
        let _ = state.remove_object(id);
    }
    for (id, z, pid) in owned_other {
        if let Some(p) = state.player_mut(pid) {
            match z {
                Zone::Hand => p.hand.retain(|&x| x != id),
                Zone::Library => p.library.retain(|&x| x != id),
                Zone::Graveyard => p.graveyard.retain(|&x| x != id),
                Zone::Exile => p.exile.retain(|&x| x != id),
                Zone::Command => p.command.retain(|&x| x != id),
                Zone::Battlefield | Zone::Stack => {}
            }
        }
        let _ = state.remove_object(id);
    }

    // Clear pending triggers and delayed triggers owned by `eliminated`.
    state
        .pending_triggers_mut()
        .retain(|t| t.controller != eliminated);
    state
        .delayed_triggers_mut()
        .retain(|t| t.controller != eliminated);
    state
        .pending_reflexive_triggers_mut()
        .retain(|t| t.controller != eliminated);

    // 5. Monarch (CR 726.4) & Initiative (CR 725.4) succession.
    let next_alive = next_alive_player_from(state, eliminated);
    if state.monarch() == Some(eliminated) {
        let new_monarch = if state.active_player() != eliminated
            && state.player(state.active_player()).is_some_and(|p| !p.lost)
        {
            Some(state.active_player())
        } else {
            next_alive
        };
        state.set_monarch(new_monarch);
        if let Some(m) = new_monarch {
            emit_event(
                state,
                AtomicEventKind::BecameMonarch {
                    player: m,
                    previous: Some(eliminated),
                },
            );
        }
    }
    if state.initiative() == Some(eliminated) {
        let new_init = if state.active_player() != eliminated
            && state.player(state.active_player()).is_some_and(|p| !p.lost)
        {
            Some(state.active_player())
        } else {
            next_alive
        };
        state.set_initiative(new_init);
        if let Some(i) = new_init {
            emit_event(state, AtomicEventKind::TookInitiative { player: i });
        }
    }

    state.mark_dirty(DirtyMask::ALL);
}

fn next_alive_player_from(state: &GameState, from: PlayerId) -> Option<PlayerId> {
    let n = state.players().len();
    for offset in 1..=n {
        let seat = (usize::from(from.0) + offset) % n;
        let pid = PlayerId(seat as u8);
        if state.player(pid).is_some_and(|p| !p.lost) && pid != from {
            return Some(pid);
        }
    }
    None
}

fn capture_pre_pass_battlefield_lki(state: &GameState) -> Vec<BatchLkiEntry> {
    let mut entries = Vec::new();
    let cache = state.evaluate_board();
    for p in state.players() {
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.zone != Zone::Battlefield || obj.is_phased_out() {
                continue;
            }
            let Some(eval_obj) = cache.get(id).cloned() else {
                continue;
            };
            let lki = build_batch_lki_snapshot(state, id, &eval_obj);
            let granted_triggers = capture_granted_triggers_from_cache(
                state.db(),
                cache,
                id,
                eval_obj.lost_all_abilities,
            );
            entries.push(BatchLkiEntry {
                id,
                lki: Arc::new(lki),
                eval: eval_obj,
                granted_triggers,
            });
        }
    }
    entries
}

fn execute_sba_graveyard_batch(
    state: &mut GameState,
    ids: &[ObjectId],
    pre_pass_lki: &[BatchLkiEntry],
    in_cleanup: bool,
) -> Result<Option<SbaOutcome>, EngineError> {
    let scope: Vec<BatchLkiEntry> = pre_pass_lki
        .iter()
        .filter(|e| ids.contains(&e.id))
        .cloned()
        .collect();
    state.batch_lki_scopes_mut().push(scope);
    let defer = state.defer_flush();
    state.set_defer_flush(defer.saturating_add(1));

    for (idx, &id) in ids.iter().enumerate() {
        if state
            .object(id)
            .is_none_or(|o| o.zone != Zone::Battlefield || o.is_phased_out())
        {
            continue;
        }
        let lki_eval = crate::events::pre_batch_lki(state, id).map(|(_, ev)| ev.clone());
        match move_object(
            state,
            id,
            Zone::Graveyard,
            MoveRawOptions::default(),
            lki_eval.as_ref(),
        )? {
            MoveObjectOutcome::Paused { prompt_id, chooser } => {
                let remaining = ids[idx + 1..].to_vec();
                state.set_pending_sba_batch(Some(PendingSbaBatchState {
                    remaining_graveyard_ids: remaining,
                    in_cleanup,
                }));
                state.set_pending_replacement_resume(Some(ReplacementResumeHook::SbaLoop {
                    in_cleanup,
                }));
                return Ok(Some(SbaOutcome::Paused { prompt_id, chooser }));
            }
            MoveObjectOutcome::ExecuteInstead { .. }
            | MoveObjectOutcome::Moved(_)
            | MoveObjectOutcome::Skipped { .. } => {}
        }
    }

    end_simultaneous_batch(state);
    Ok(None)
}

fn prune_invalid_combat_entries(state: &mut GameState) -> bool {
    let mut invalid_ids: SmallVec<[ObjectId; 8]> = SmallVec::new();
    let mut invalid_targets: SmallVec<[ObjectId; 4]> = SmallVec::new();
    {
        let cache = state.evaluate_board();
        for a in &state.combat().attackers {
            let valid_atk = state.object(a.attacker).is_some_and(|o| {
                o.zone == Zone::Battlefield
                    && !o.is_phased_out()
                    && cache
                        .get(a.attacker)
                        .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            });
            let valid_tgt = match a.target {
                AttackTarget::Player { id } => state.player(id).is_some_and(|p| !p.lost),
                AttackTarget::Planeswalker { id } => state.object(id).is_some_and(|o| {
                    o.zone == Zone::Battlefield
                        && !o.is_phased_out()
                        && cache
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::PLANESWALKER))
                }),
                AttackTarget::Battle { id } => state.object(id).is_some_and(|o| {
                    o.zone == Zone::Battlefield
                        && !o.is_phased_out()
                        && cache
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::BATTLE))
                }),
            };
            if !valid_atk {
                invalid_ids.push(a.attacker);
            } else if !valid_tgt {
                invalid_targets.push(a.attacker);
            }
        }
        for b in &state.combat().blockers {
            let valid_blk = state.object(b.blocker).is_some_and(|o| {
                o.zone == Zone::Battlefield
                    && !o.is_phased_out()
                    && cache
                        .get(b.blocker)
                        .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            });
            if !valid_blk {
                invalid_ids.push(b.blocker);
            }
        }
    }

    if invalid_ids.is_empty() && invalid_targets.is_empty() {
        return false;
    }
    let combat = state.combat_mut();
    combat
        .attackers
        .retain(|a| !invalid_ids.contains(&a.attacker) && !invalid_targets.contains(&a.attacker));
    combat
        .blockers
        .retain(|b| !invalid_ids.contains(&b.blocker) && !invalid_ids.contains(&b.attacker));
    combat
        .damage_assignment_order
        .retain(|(atk, _)| combat.attackers.iter().any(|a| a.attacker == *atk));
    for (_, blks) in &mut combat.damage_assignment_order {
        blks.retain(|b| !invalid_ids.contains(b));
    }
    state.mark_dirty(DirtyMask::COMBAT);
    true
}

fn annihilate_plus_minus_counters(state: &mut GameState) -> bool {
    let Some(plus_cid) = state.db().counter_id("+1/+1") else {
        return false;
    };
    let Some(minus_cid) = state.db().counter_id("-1/-1") else {
        return false;
    };

    let mut pairs: SmallVec<[(ObjectId, u32, u32, u32); 4]> = SmallVec::new();
    for p in state.players() {
        for &id in &p.battlefield {
            if state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            {
                let plus = state.counter_count(id, plus_cid);
                let minus = state.counter_count(id, minus_cid);
                let cancel = plus.min(minus);
                if cancel > 0 {
                    pairs.push((id, plus - cancel, minus - cancel, cancel));
                }
            }
        }
    }

    if pairs.is_empty() {
        return false;
    }

    for (id, new_plus, new_minus, _) in pairs {
        state.set_counter(id, plus_cid, new_plus);
        state.set_counter(id, minus_cid, new_minus);
    }
    true
}

fn check_attachment_sbas(state: &mut GameState, graveyard_batch: &mut Vec<ObjectId>) -> bool {
    let aura_sid = state.db().subtype_id("Aura");
    let equip_sid = state.db().subtype_id("Equipment");
    let fort_sid = state.db().subtype_id("Fortification");

    let mut unattach_list: SmallVec<[(ObjectId, ObjectId, bool); 4]> = SmallVec::new();
    let mut clear_bestow_only: SmallVec<[ObjectId; 2]> = SmallVec::new();
    let mut aura_die_list: SmallVec<[ObjectId; 4]> = SmallVec::new();

    {
        let cache = state.evaluate_board();
        for p in state.players() {
            for &id in &p.battlefield {
                let Some(obj) = state.object(id) else {
                    continue;
                };
                if obj.zone != Zone::Battlefield || obj.is_phased_out() {
                    continue;
                }
                let Some(eval) = cache.get(id) else {
                    continue;
                };
                let is_aura = eval.types.contains(CardTypeMask::ENCHANTMENT)
                    && aura_sid.is_some_and(|sid| eval.subtypes.contains(&sid));
                let is_equip = eval.types.contains(CardTypeMask::ARTIFACT)
                    && equip_sid.is_some_and(|sid| eval.subtypes.contains(&sid));
                let is_fort = eval.types.contains(CardTypeMask::ARTIFACT)
                    && fort_sid.is_some_and(|sid| eval.subtypes.contains(&sid));

                if is_aura {
                    let invalid = match obj.attached_to {
                        None => true,
                        Some(host) if host == id => true,
                        Some(_) if eval.types.contains(CardTypeMask::CREATURE) => true,
                        Some(host) => {
                            let host_ok = state.object(host).is_some_and(|ho| {
                                ho.zone == Zone::Battlefield && !ho.is_phased_out()
                            });
                            !host_ok
                                || !enchant_legality_for_aura(
                                    state,
                                    id,
                                    TargetRef::Object { id: host },
                                )
                                || has_protection_from(state, host, id, true)
                        }
                    };
                    if invalid {
                        if obj.flags.contains(ObjectFlags::BESTOWED) {
                            if let Some(host) = obj.attached_to {
                                unattach_list.push((id, host, true));
                            } else {
                                clear_bestow_only.push(id);
                            }
                        } else {
                            aura_die_list.push(id);
                        }
                    }
                } else if (is_equip || is_fort)
                    && let Some(host) = obj.attached_to
                {
                    let host_ok = state
                        .object(host)
                        .is_some_and(|ho| ho.zone == Zone::Battlefield && !ho.is_phased_out())
                        && cache.get(host).is_some_and(|he| {
                            if is_equip {
                                he.types.contains(CardTypeMask::CREATURE)
                            } else {
                                he.types.contains(CardTypeMask::LAND)
                            }
                        });
                    let invalid = host == id
                        || eval.types.contains(CardTypeMask::CREATURE)
                        || !host_ok
                        || has_protection_from(state, host, id, true);
                    if invalid {
                        unattach_list.push((id, host, false));
                    }
                }
            }
        }
    }

    let mut changed = false;
    for id in clear_bestow_only {
        if let Some(obj) = state.object_mut(id) {
            obj.flags.remove(ObjectFlags::BESTOWED);
        }
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        changed = true;
    }
    for (id, host, clear_bestow) in unattach_list {
        if let Some(obj) = state.object_mut(id) {
            obj.attached_to = None;
            if clear_bestow {
                obj.flags.remove(ObjectFlags::BESTOWED);
            }
        }
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
        emit_event(
            state,
            AtomicEventKind::AttachmentChanged {
                attachment: id,
                host,
                attached: false,
            },
        );
        changed = true;
    }

    for id in aura_die_list {
        if !graveyard_batch.contains(&id) {
            graveyard_batch.push(id);
        }
    }

    changed
}

fn is_cant_be_regenerated(state: &GameState, id: ObjectId) -> bool {
    state
        .rare_turn_state()
        .is_some_and(|r| r.cant_be_regenerated_this_turn.contains(&id))
}

fn try_consume_regeneration_shield(state: &mut GameState, id: ObjectId) -> bool {
    if is_cant_be_regenerated(state, id) {
        return false;
    }
    let has_shield = state.rare_turn_state().is_some_and(|r| {
        r.regeneration_shields
            .iter()
            .any(|&(oid, c)| oid == id && c > 0)
    });
    if !has_shield {
        return false;
    }

    // Consume 1 regeneration shield.
    {
        let rts = state.rare_turn_state_mut();
        if let Some(pos) = rts
            .regeneration_shields
            .iter()
            .position(|&(oid, _)| oid == id)
        {
            if rts.regeneration_shields[pos].1 <= 1 {
                rts.regeneration_shields.remove(pos);
            } else {
                rts.regeneration_shields[pos].1 -= 1;
            }
        }
    }
    state.prune_rare_turn_state_if_empty();

    // Tap, remove from combat, clear all marked damage (CR 701.15a).
    if let Some(obj) = state.object_mut(id) {
        obj.flags.insert(ObjectFlags::TAPPED);
        obj.damage_marked = 0;
        obj.deathtouch_damage_marked = false;
    }
    crate::combat::remove_from_combat(state, id);
    state.mark_dirty(DirtyMask::TAPPED | DirtyMask::COMBAT);
    true
}

fn try_umbra_armor(
    state: &mut GameState,
    creature_id: ObjectId,
    graveyard_batch: &mut Vec<ObjectId>,
) -> bool {
    let mut umbra_aura: Option<ObjectId> = None;
    for p in state.players() {
        for &cand in &p.battlefield {
            if state
                .object(cand)
                .is_some_and(|o| !o.is_phased_out() && o.attached_to == Some(creature_id))
                && has_umbra_armor(state, cand)
            {
                umbra_aura = Some(cand);
                break;
            }
        }
        if umbra_aura.is_some() {
            break;
        }
    }
    let Some(aura) = umbra_aura else {
        return false;
    };
    if !graveyard_batch.contains(&aura) {
        graveyard_batch.push(aura);
    }
    if let Some(obj) = state.object_mut(creature_id) {
        obj.damage_marked = 0;
        obj.deathtouch_damage_marked = false;
    }
    state.mark_dirty(DirtyMask::COMBAT);
    true
}

fn check_permanent_destruction_sbas(
    state: &mut GameState,
    graveyard_batch: &mut Vec<ObjectId>,
) -> bool {
    let loyalty_cid = state.db().counter_id("loyalty");
    let defense_cid = state.db().counter_id("defense");
    let lore_cid = state.db().counter_id("lore");

    let mut bf_ids: Vec<ObjectId> = Vec::new();
    for p in state.players() {
        for &id in &p.battlefield {
            if state
                .object(id)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            {
                bf_ids.push(id);
            }
        }
    }

    let mut changed = false;
    for id in bf_ids {
        let Some(obj) = state.object(id) else {
            continue;
        };
        let eff_def = state
            .rare(id)
            .and_then(|r| r.copiable.as_deref())
            .map_or(obj.card_def, |c| c.card_def);
        let fi = obj.active_face_index();
        let face_down = obj.is_face_down();
        let dmg = obj.damage_marked;
        let dt_dmg = obj.deathtouch_damage_marked;

        let eval = {
            let cache = state.evaluate_board();
            cache.get(id).cloned()
        };
        let Some(eval) = eval else {
            continue;
        };

        // 1. Creature toughness <= 0 (CR 704.5f) or lethal/deathtouch damage (CR 704.5g-h).
        if eval.types.contains(CardTypeMask::CREATURE) {
            let toughness = eval.pt.map_or(0, |pt| pt.toughness);
            if toughness <= 0 {
                if !graveyard_batch.contains(&id) {
                    graveyard_batch.push(id);
                }
                continue;
            }
            let lethal = (dmg > 0 && dmg >= toughness) || dt_dmg;
            if lethal {
                if has_indestructible(state, id) {
                    if dt_dmg && let Some(om) = state.object_mut(id) {
                        om.deathtouch_damage_marked = false;
                    }
                    continue;
                }
                if try_consume_regeneration_shield(state, id) {
                    changed = true;
                    continue;
                }
                if try_umbra_armor(state, id, graveyard_batch) {
                    changed = true;
                    continue;
                }
                if !graveyard_batch.contains(&id) {
                    graveyard_batch.push(id);
                }
                continue;
            }
        }

        // 2. Planeswalker 0 loyalty (CR 704.5i).
        if eval.types.contains(CardTypeMask::PLANESWALKER) {
            let loy = loyalty_cid.map_or(0, |cid| state.counter_count(id, cid));
            if loy == 0 {
                if !graveyard_batch.contains(&id) {
                    graveyard_batch.push(id);
                }
                continue;
            }
        }

        // 3. Battle 0 defense or missing protector (CR 704.5v-y).
        if eval.types.contains(CardTypeMask::BATTLE) {
            let def = defense_cid.map_or(0, |cid| state.counter_count(id, cid));
            let has_pending_trigger = state.stack_entries().iter().any(
                |e| matches!(e.kind, StackEntryKind::Triggered { source, .. } if source == id),
            ) || state.pending_triggers().iter().any(|t| t.source == id);
            if def == 0 && !has_pending_trigger {
                if !graveyard_batch.contains(&id) {
                    graveyard_batch.push(id);
                }
                continue;
            }
            let _ = battle_starting_defense;
        }

        // 4. Saga final chapter sacrifice (CR 704.5s).
        if !face_down && is_saga_card(state.db(), eff_def, fi) {
            let final_ch = saga_final_chapter(state.db(), eff_def, fi);
            if final_ch > 0 {
                let lore = lore_cid.map_or(0, |cid| state.counter_count(id, cid));
                if lore >= u32::from(final_ch) {
                    let chapter_on_stack = state.stack_entries().iter().any(|e| {
                        matches!(e.kind, StackEntryKind::Triggered { source, .. } if source == id)
                    }) || state.pending_triggers().iter().any(|t| t.source == id);
                    if !chapter_on_stack && !graveyard_batch.contains(&id) {
                        graveyard_batch.push(id);
                    }
                }
            }
        }
    }

    changed
}

fn find_legend_rule_violation(state: &GameState) -> Option<(PlayerId, String, Vec<ObjectId>)> {
    let cache = state.evaluate_board();
    for p in state.players() {
        if p.lost {
            continue;
        }
        let ep = cache.player(p.id);
        if ep.is_some_and(|e| e.legend_rule_exempt_all) {
            continue;
        }
        let mut groups: Vec<(String, Vec<ObjectId>)> = Vec::new();
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.zone != Zone::Battlefield || obj.is_phased_out() || obj.is_face_down() {
                continue;
            }
            let Some(eval) = cache.get(id) else {
                continue;
            };
            if !eval.supertypes.contains(SupertypeMask::LEGENDARY) {
                continue;
            }
            // Check per-filter LegendRuleExempt.
            let is_exempt = ep.is_some_and(|player_eval| {
                player_eval.active_rule_mods.iter().any(|entry| {
                    if let RuleMod::LegendRuleExempt { filter } = &entry.rule {
                        match filter.as_deref() {
                            None => true,
                            Some(f) => {
                                let ctx = EvalContext::for_source(
                                    entry.controller,
                                    entry.source.unwrap_or(id),
                                )
                                .with_board(BoardView::Cached(cache));
                                matches_object_filter(state, id, f, &ctx)
                            }
                        }
                    } else {
                        false
                    }
                })
            });
            if is_exempt {
                continue;
            }

            let eff_name = if let Some(ov) = &eval.name_override {
                ov.clone()
            } else if let Some(opts) = face_opts_for_object(state, id)
                && let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts)
            {
                resolved.name
            } else {
                continue;
            };
            if eff_name.is_empty() {
                continue;
            }
            if let Some((_, list)) = groups.iter_mut().find(|(n, _)| *n == eff_name) {
                list.push(id);
            } else {
                groups.push((eff_name, vec![id]));
            }
        }
        if let Some((name, candidates)) = groups.into_iter().find(|(_, list)| list.len() > 1) {
            return Some((p.id, name, candidates));
        }
    }
    None
}

fn check_player_loss_sbas(state: &mut GameState, any_taken: &mut bool) -> Option<SbaOutcome> {
    let lethal_cmd = lethal_commander_damage_players(state);
    let mut newly_lost: SmallVec<[PlayerId; 4]> = SmallVec::new();
    {
        let cache = state.evaluate_board();
        for p in state.players() {
            if p.lost {
                continue;
            }
            let ep = cache.player(p.id);
            if ep.is_some_and(|e| e.cant_lose) {
                continue;
            }
            let dont_lose_zero = ep.is_some_and(|e| e.dont_lose_at_zero_life);
            let lost_life = p.life <= 0 && !dont_lose_zero;
            let lost_poison = p.poison >= 10;
            let lost_empty_lib = p.attempted_empty_library_draw;
            let lost_cmd = lethal_cmd.iter().any(|&(pid, _, _)| pid == p.id);
            if lost_life || lost_poison || lost_empty_lib || lost_cmd {
                newly_lost.push(p.id);
            }
        }
    }

    if newly_lost.is_empty() {
        return None;
    }
    *any_taken = true;

    for &pid in &newly_lost {
        if let Some(p) = state.player_mut(pid) {
            p.lost = true;
        }
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
        emit_event(state, AtomicEventKind::PlayerLostGame { player: pid });
    }
    for &pid in &newly_lost {
        cleanup_eliminated_player(state, pid);
    }

    let alive: SmallVec<[PlayerId; 4]> = state
        .players()
        .iter()
        .filter(|p| !p.lost)
        .map(|p| p.id)
        .collect();

    if alive.is_empty() {
        state.set_decision(DecisionState::GameOver { winner: None });
        return Some(SbaOutcome::GameOver { winner: None });
    }
    if alive.len() == 1 && state.players().len() >= 2 {
        let survivor = alive[0];
        let opp_cant_win = state
            .evaluate_board()
            .players
            .iter()
            .enumerate()
            .any(|(seat, ep)| {
                let pid = PlayerId(seat as u8);
                pid != survivor
                    && ep.opponents_cant_win
                    && !state.player(pid).is_some_and(|p| p.lost)
            });
        if !opp_cant_win {
            state.set_decision(DecisionState::GameOver {
                winner: Some(survivor),
            });
            return Some(SbaOutcome::GameOver {
                winner: Some(survivor),
            });
        }
    }

    if state
        .player(state.priority_player())
        .is_some_and(|p| p.lost)
    {
        let n = state.players().len();
        let cur = usize::from(state.priority_player().0);
        for offset in 1..=n {
            let cand = PlayerId(((cur + offset) % n) as u8);
            if state.player(cand).is_some_and(|p| !p.lost) {
                state.set_priority_player(cand);
                break;
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::zones::CreateTokenOptions;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, SupertypeMask, TokenSpec,
    };
    use std::sync::Arc;

    fn synth_sba_state() -> (GameState, mtg_ir::KeywordId, mtg_ir::CounterId) {
        let mut tables = InternTables::default();
        let kw_ind = mtg_ir::KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("indestructible".into());
        let c_m1 = mtg_ir::CounterId(tables.counters.len() as u16);
        tables.counters.push("-1/-1".into());
        let land = CardDef {
            oracle_id: "synth_sba_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Sba Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Sba Plains".into(),
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
        let legend = CardDef {
            oracle_id: "synth_legend_hero".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Legend Hero".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Legend Hero".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::LEGENDARY,
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
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![land, legend],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 3;
        let state = GameState::new(db, cfg, &[deck.clone(), deck.clone(), deck], 19).unwrap();
        (state, kw_ind, c_m1)
    }

    #[test]
    fn synth_token_dies_captures_pre_annihilation_lki_before_704_5q() {
        let (mut state, _kw_ind, _c_m1) = synth_sba_state();
        let tok = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            TokenSpec {
                name: "Synth Wisp".into(),
                power: 1,
                toughness: 1,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();
        state.object_mut(tok).unwrap().damage_marked = 1;
        assert_eq!(check_sbas(&mut state).unwrap(), SbaOutcome::Done);

        // Token ceased to exist (`CR 704.5q`) AND pre-annihilation LKI was emitted (`CR 704.8`).
        assert!(state.object(tok).is_none());
        assert!(state.player(PlayerId(0)).unwrap().graveyard.is_empty());
        let has_token_lki = state.event_log().iter().any(|ev| {
            matches!(
                &ev.kind,
                AtomicEventKind::ZoneChange {
                    from: Some(Zone::Battlefield),
                    to: Zone::Graveyard,
                    lki: Some(lki),
                    ..
                } if lki.name == "Synth Wisp" && lki.is_token
            )
        });
        assert!(has_token_lki, "expected CR 704.8 LKI on token death");
    }

    #[test]
    fn synth_toughness_zero_kills_indestructible_and_legend_rule_prompts() {
        let (mut state, kw_ind, c_m1) = synth_sba_state();
        let mut kws = SmallVec::new();
        kws.push(kw_ind);
        let ind = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            TokenSpec {
                name: "Synth Golem".into(),
                power: 2,
                toughness: 2,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: kws,
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();
        // Lethal damage does not destroy Indestructible (`CR 704.5g`).
        state.object_mut(ind).unwrap().damage_marked = 5;
        assert_eq!(check_sbas(&mut state).unwrap(), SbaOutcome::Done);
        assert!(state.object(ind).is_some());

        // Toughness <= 0 DOES put Indestructible into GY (`CR 704.5f`).
        state.set_counter(ind, c_m1, 2);
        assert_eq!(check_sbas(&mut state).unwrap(), SbaOutcome::Done);
        assert!(state.object(ind).is_none());

        // Duplicate Legendaries trigger LegendRule prompt and leave kept one only.
        let lib0 = state.player(PlayerId(0)).unwrap().library[0];
        let lib1 = state.player(PlayerId(0)).unwrap().library[1];
        state.object_mut(lib0).unwrap().card_def = CardDefId(1);
        state.object_mut(lib1).unwrap().card_def = CardDefId(1);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let l0 = match move_object(
            &mut state,
            lib0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected move"),
        };
        let l1 = match move_object(
            &mut state,
            lib1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => panic!("expected move"),
        };

        let SbaOutcome::Paused { prompt_id, chooser } = check_sbas(&mut state).unwrap() else {
            panic!("expected legend rule prompt");
        };
        assert_eq!(chooser, PlayerId(0));
        assert_eq!(
            resume_sba_prompt(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickObject { id: l0 },
            )
            .unwrap(),
            SbaOutcome::Done
        );
        assert!(state.player(PlayerId(0)).unwrap().battlefield.contains(&l0));
        assert!(!state.player(PlayerId(0)).unwrap().battlefield.contains(&l1));
    }

    #[test]
    fn synth_cleanup_sba_legend_rule_and_replacement_resume_advances_turn() {
        let (mut state, _kw_ind, _c_m1) = synth_sba_state();
        state.set_phase(GamePhase::Cleanup);
        state.set_turn_number(1);
        state.set_active_player(PlayerId(0));
        state.set_last_normal_turn_player(Some(PlayerId(0)));
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        // Put two identical legendaries under P1's control so Cleanup SBAs prompt P1 (`LegendRule`).
        for idx in 0..2 {
            let lib = state.player(PlayerId(1)).unwrap().library[idx];
            state.object_mut(lib).unwrap().card_def = CardDefId(1);
            state.mark_dirty(DirtyMask::TOPOLOGY);
            move_object(
                &mut state,
                lib,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }

        let out = crate::turn::advance_phase_or_step(&mut state).unwrap();
        assert!(matches!(
            out,
            crate::turn::TurnAdvanceOutcome::Paused {
                chooser: PlayerId(1),
                ..
            }
        ));
        let pid = state.pending_prompt().unwrap().prompt_id;
        let keep = state.player(PlayerId(1)).unwrap().battlefield[0];
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(1),
            pid,
            ChoiceAnswer::PickObject { id: keep },
        )
        .unwrap();
        assert_eq!(state.turn_number(), 2);
        assert_eq!(state.active_player(), PlayerId(1));
        assert_eq!(state.player(PlayerId(1)).unwrap().battlefield.len(), 1);
    }

    #[test]
    fn synth_sba_704_8_pre_annihilation_lki_counters() {
        let mut tables = InternTables::default();
        let c_p1 = mtg_ir::CounterId(tables.counters.len() as u16);
        tables.counters.push("+1/+1".into());
        let c_m1 = mtg_ir::CounterId(tables.counters.len() as u16);
        tables.counters.push("-1/-1".into());
        let land = CardDef {
            oracle_id: "synth_704_8_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Lki Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lki Land".into(),
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
                tables,
                cards: vec![land],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 51).unwrap();
        let tok = crate::zones::create_token_raw(
            &mut state,
            PlayerId(0),
            TokenSpec {
                name: "Synth Wisp".into(),
                power: 1,
                toughness: 1,
                colors: ColorMask::empty(),
                types: CardTypeMask::CREATURE,
                supertypes: SupertypeMask::empty(),
                subtypes: SmallVec::new(),
                type_line: String::new(),
                keywords: SmallVec::new(),
                tapped: false,
                quoted_abilities: Vec::new(),
                legendary: false,
            },
            CreateTokenOptions::default(),
        )
        .unwrap();
        state.set_counter(tok, c_p1, 1);
        state.set_counter(tok, c_m1, 2);
        assert_eq!(check_sbas(&mut state).unwrap(), SbaOutcome::Done);

        let lki = state.lki(tok).expect("expected recorded LKI");
        assert!(lki.counters.contains(&(c_p1, 1)));
        assert!(lki.counters.contains(&(c_m1, 2)));
        let zc_has_both = state.event_log().iter().any(|ev| {
            matches!(
                &ev.kind,
                AtomicEventKind::ZoneChange {
                    from: Some(Zone::Battlefield),
                    to: Zone::Graveyard,
                    lki: Some(z_lki),
                    ..
                } if z_lki.counters.contains(&(c_p1, 1)) && z_lki.counters.contains(&(c_m1, 2))
            )
        });
        assert!(
            zc_has_both,
            "expected ZoneChange LKI to retain pre-annihilation +1/+1 and -1/-1 counters"
        );
    }

    #[test]
    fn synth_4p_player_loss_sba_under_verify_layer_cache() {
        let tables = InternTables::default();
        let land = CardDef {
            oracle_id: "synth_4p_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth FourP Land".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth FourP Land".into(),
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
        let anthem = CardDef {
            oracle_id: "synth_4p_anthem".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth FourP Anthem".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth FourP Anthem".into(),
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
                abilities: vec![mtg_ir::AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        condition: None,
                        body: mtg_ir::StaticBody::Continuous {
                            layer: mtg_ir::Layer::Layer7c,
                            affected: mtg_ir::Affected::Filter {
                                filter: Box::new(mtg_ir::ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(mtg_ir::RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: mtg_ir::LayerMod::Pt {
                                power: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                toughness: Box::new(mtg_ir::Amount::Const { n: 1 }),
                            },
                            active_in_zone: None,
                        },
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
                cards: vec![land, anthem],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 4;
        let mut state = GameState::new(
            db,
            cfg,
            &[deck.clone(), deck.clone(), deck.clone(), deck],
            53,
        )
        .unwrap();

        let ant_h = state.player(PlayerId(1)).unwrap().hand[0];
        state.object_mut(ant_h).unwrap().card_def = CardDefId(1);
        move_object(
            &mut state,
            ant_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let _ = state.evaluate_board();

        // P1 loses via empty-library draw SBA -> LayerCache invalidated before PlayerLostGame triggers run.
        state
            .player_mut(PlayerId(1))
            .unwrap()
            .attempted_empty_library_draw = true;
        assert_eq!(check_sbas(&mut state).unwrap(), SbaOutcome::Done);
        assert!(state.player(PlayerId(1)).unwrap().lost);
        assert!(state.player(PlayerId(1)).unwrap().battlefield.is_empty());
        let _ = state.evaluate_board();
    }
}
