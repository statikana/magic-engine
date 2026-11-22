use mtg_ir::{
    CardTypeMask, ColorMask, DelayedTriggerPayload, DigKeepTo, DigRestTo, DigSpec, EffectStep,
    ExileUntilHitAction, ExileUntilRest, ManaProduction, ManaRestrictionTag,
    ManaSpendingRestriction, PileSplitSpec, PlayFromExileDuration, RuleMod, SearchDestination,
    SearchSpec, SelectionMod, SubtypeId, TokenSpec, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{ChoiceAnswer, ChoiceRequest, DecisionState};
use crate::eval::{BoardView, EvalContext, evaluate_amount, resolve_player_ref};
use crate::events::{AtomicEventKind, emit_event};
use crate::filter::{find_objects, matches_object_filter};
use crate::id::{ObjectId, PlayerId};
use crate::mana;
use crate::replace::{self, ReplacementOutcome, ReplacementResult};
use crate::state::{
    ControlFlowFrame, DigMoveRole, DirtyMask, EngineError, FaceDownKind, FaceDownSpec, GameState,
    ModePausePurpose, NumberPausePurpose, PendingPromptContext, PendingStepContinuation,
    PlayableFromExileState, PromptOwner, ReorderMode, StepPauseState, YesNoPausePurpose,
};
use crate::zones::{self, CreateTokenOptions, MoveObjectOutcome, MoveRawOptions, ZonePosition};

use super::counters_mods::apply_resolved_put_counters;
use super::vm::{
    OP_TAG_DIG, OP_TAG_DRAW, OP_TAG_SEARCH, SelectionResult, StepExecOutcome, append_step_binding,
    move_opts_for_zone, record_step_bindings, resolve_selection_mod, resolve_selector_objects,
    resolve_step_actor, resolve_step_amount, resolve_target_players, set_waiting_on_replacement,
    vm_controller, vm_source, with_vm_eval,
};
use super::zones_tokens::apply_resolved_create_token;

pub(crate) fn exec_add_mana(
    state: &mut GameState,
    step: &EffectStep,
    production: &ManaProduction,
    restriction: Option<ManaRestrictionTag>,
    restriction_spec: Option<&ManaSpendingRestriction>,
    retain_until_eot: bool,
    delayed_rider: Option<&DelayedTriggerPayload>,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let src = vm_source(state);
    let mult = if mana::add_mana_ignores_step_amount(production) {
        1
    } else if step.amount.is_some() {
        resolve_step_amount(state, step, 1).max(0) as u32
    } else {
        1
    };
    let _ = mana::add_mana_from_production(
        state,
        actor,
        src,
        production,
        restriction,
        restriction_spec,
        retain_until_eot,
        delayed_rider,
        None,
        mult,
    )?;
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_retain_mana(
    state: &mut GameState,
    step: &EffectStep,
    colors: ColorMask,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    if let Some(p) = state.player_mut(actor) {
        let masks = [
            (0usize, ColorMask::W),
            (1, ColorMask::U),
            (2, ColorMask::B),
            (3, ColorMask::R),
            (4, ColorMask::G),
            (5, ColorMask::C),
        ];
        let pool = p.mana_pool;
        let ext = p.mana_ext_mut();
        ext.unemptied_mana_colors_this_turn |= colors;
        for (i, m) in masks {
            if colors.contains(m) {
                ext.retained_mana_counts[i] = ext.retained_mana_counts[i].max(pool[i]);
            }
        }
    }
    state.mark_dirty(DirtyMask::PLAYER_STATE);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn apply_resolved_draw(
    state: &mut GameState,
    step: &EffectStep,
    outcome: &ReplacementOutcome,
    reveal_drawn: bool,
) -> Result<(), EngineError> {
    if outcome.mods.skipped {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.not_drawn_cards_this_way = true;
        }
        return Ok(());
    }
    let crate::state::ReplaceableEventKind::Draw {
        player,
        count,
        source,
    } = outcome.event
    else {
        return Ok(());
    };
    let effective_player = outcome.mods.draw_stealer.unwrap_or(player);
    let (cant_draw, cant_draw_extra) = {
        let cache = state.evaluate_board();
        cache.player(effective_player).map_or((false, false), |ep| {
            (ep.cant_draw_cards, ep.cant_draw_extra_cards)
        })
    };
    let cards_drawn_so_far = state
        .player(effective_player)
        .map_or(0, |p| p.cards_drawn_this_turn);
    let max_allowed = if cant_draw {
        0
    } else if cant_draw_extra {
        if cards_drawn_so_far == 0 && state.phase() == mtg_ir::GamePhase::Draw {
            1
        } else {
            0
        }
    } else {
        count
    };

    let mut drawn = 0u32;
    let bind_as = step.bind_as.as_deref();
    let _ = source;

    // Handle Abundance replacement (land vs nonland)
    if let Some(want_land) = outcome.mods.abundance_land {
        for _ in 0..max_allowed {
            let found_idx = {
                let cache = state.evaluate_board();
                let lib = state
                    .player(effective_player)
                    .map(|p| p.library.clone())
                    .unwrap_or_default();
                lib.iter().rposition(|&id| {
                    state.object(id).is_some_and(|o| {
                        let ctx =
                            EvalContext::new(o.controller).with_board(BoardView::Cached(cache));
                        let off = crate::eval::off_battlefield_characteristics(
                            state,
                            id,
                            o,
                            Zone::Library,
                            &ctx,
                        );
                        off.types.contains(CardTypeMask::LAND) == want_land
                    })
                })
            };
            if let Some(idx) = found_idx
                && let Some(cid) = state
                    .player(effective_player)
                    .and_then(|p| p.library.get(idx).copied())
            {
                let mv = zones::move_object(state, cid, Zone::Hand, Default::default(), None)?;
                if let MoveObjectOutcome::Moved(raw) = mv {
                    let final_id = raw.new_id.unwrap_or(raw.old_id);
                    if reveal_drawn {
                        zones::reveal_to_all(state, final_id);
                    }
                    append_step_binding(state, bind_as, final_id, raw.lki);
                    drawn += 1;
                }
            }
        }
    } else {
        for _ in 0..max_allowed {
            match zones::draw_one_raw(state, effective_player)? {
                Some(new_id) => {
                    if reveal_drawn {
                        zones::reveal_to_all(state, new_id);
                    }
                    emit_event(
                        state,
                        AtomicEventKind::DrawCard {
                            player: effective_player,
                            object: new_id,
                            in_own_draw_step_ordinal: None,
                        },
                    );
                    append_step_binding(state, bind_as, new_id, None);
                    drawn += 1;
                }
                None => break,
            }
        }
    }

    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_effect_amount = drawn as i32;
        if drawn == 0 {
            vm.step_way_stats.not_drawn_cards_this_way = true;
        }
    }
    Ok(())
}

pub(crate) fn run_draw_players(
    state: &mut GameState,
    step: &EffectStep,
    players: &[PlayerId],
    count: u32,
    reveal_drawn: bool,
) -> Result<StepExecOutcome, EngineError> {
    if count == 0 {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.not_drawn_cards_this_way = true;
        }
        return Ok(StepExecOutcome::Done);
    }
    let src = vm_source(state);
    for (idx, &pid) in players.iter().enumerate() {
        match replace::resolve_draw(state, pid, count, src)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                        remaining_players: players[idx + 1..].iter().copied().collect(),
                        ..Default::default()
                    }));
                }
                set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_DRAW);
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_draw(state, step, &out, reveal_drawn)?;
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_draw(
    state: &mut GameState,
    step: &EffectStep,
    up_to: bool,
    reveal_drawn: bool,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let count = resolve_step_amount(state, step, 1).max(0) as u32;
    if count == 0 {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.not_drawn_cards_this_way = true;
        }
        return Ok(StepExecOutcome::Done);
    }

    if up_to && let Some(&chooser) = players.first() {
        let prompt_id = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                remaining_players: players[1..].iter().copied().collect(),
                ..Default::default()
            }));
            vm.pause_state = Some(StepPauseState::ChooseNumber {
                chooser,
                purpose: NumberPausePurpose::UpToDraw,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::ChooseNumber {
            prompt_id,
            min: 0,
            max: count as i32,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(chooser);
        return Ok(StepExecOutcome::Paused { prompt_id, chooser });
    }

    run_draw_players(state, step, &players, count, reveal_drawn)
}

pub(crate) fn resume_up_to_draw(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    count: i32,
    reveal_drawn: bool,
) -> Result<StepExecOutcome, EngineError> {
    let rem = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pending_continuation.take())
        .map(|c| c.remaining_players)
        .unwrap_or_default();
    let n = count.max(0) as u32;
    if n == 0 {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.step_way_stats.not_drawn_cards_this_way = true;
        }
        if !rem.is_empty() {
            let max_c = resolve_step_amount(state, step, 1).max(0) as u32;
            return run_draw_players(state, step, &rem, max_c, reveal_drawn);
        }
        return Ok(StepExecOutcome::Done);
    }
    let src = vm_source(state);
    match replace::resolve_draw(state, chooser, n, src)? {
        ReplacementResult::Paused {
            prompt_id,
            chooser: rep_chooser,
        } => {
            if !rem.is_empty()
                && let Some(vm) = state.resolution_vms_mut().last_mut()
            {
                vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                    remaining_players: rem,
                    ..Default::default()
                }));
            }
            set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_DRAW);
            Ok(StepExecOutcome::Paused {
                prompt_id,
                chooser: rep_chooser,
            })
        }
        ReplacementResult::Resolved(out) => {
            apply_resolved_draw(state, step, &out, reveal_drawn)?;
            if !rem.is_empty() {
                let max_c = resolve_step_amount(state, step, 1).max(0) as u32;
                return run_draw_players(state, step, &rem, max_c, reveal_drawn);
            }
            Ok(StepExecOutcome::Done)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_reorder_yes_no(
    state: &mut GameState,
    player: PlayerId,
    mode: ReorderMode,
    remaining: Vec<ObjectId>,
    top_order: Vec<ObjectId>,
    bottom_or_gy: Vec<ObjectId>,
    card: ObjectId,
    may_shuffle: bool,
) -> (u32, PlayerId) {
    let prompt_id = state.allocate_prompt_id();
    let label = match mode {
        ReorderMode::Scry => "Put card on the bottom of your library?".to_string(),
        ReorderMode::Surveil => "Put card into your graveyard?".to_string(),
        ReorderMode::LookAndReorder => "Keep card on top?".to_string(),
    };
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::ReorderLibrary {
            player,
            mode,
            remaining,
            top_order,
            bottom_or_gy,
            deciding_card: Some(card),
            may_shuffle,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::YesNo { prompt_id, label };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: player,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(player);
    (prompt_id, player)
}

fn push_reorder_order_objects(
    state: &mut GameState,
    player: PlayerId,
    mode: ReorderMode,
    remaining: Vec<ObjectId>,
    top_order: Vec<ObjectId>,
    bottom_or_gy: Vec<ObjectId>,
    may_shuffle: bool,
) -> (u32, PlayerId) {
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::ReorderLibrary {
            player,
            mode,
            remaining: remaining.clone(),
            top_order,
            bottom_or_gy,
            deciding_card: None,
            may_shuffle,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::OrderObjects {
        prompt_id,
        objects: remaining,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: player,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(player);
    (prompt_id, player)
}

pub(crate) fn exec_scry(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let player = resolve_step_actor(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as usize;
    let cards: Vec<ObjectId> = state
        .player(player)
        .map(|p| {
            let n = amt.min(p.library.len());
            p.library[p.library.len() - n..]
                .iter()
                .copied()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    if cards.is_empty() {
        emit_event(state, AtomicEventKind::Scry { player, amount: 0 });
        return Ok(StepExecOutcome::Done);
    }
    let first = cards[0];
    let rest = cards[1..].to_vec();
    let (prompt_id, chooser) = push_reorder_yes_no(
        state,
        player,
        ReorderMode::Scry,
        rest,
        Vec::new(),
        Vec::new(),
        first,
        false,
    );
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn exec_surveil(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let player = resolve_step_actor(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as usize;
    let cards: Vec<ObjectId> = state
        .player(player)
        .map(|p| {
            let n = amt.min(p.library.len());
            p.library[p.library.len() - n..]
                .iter()
                .copied()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    if cards.is_empty() {
        emit_event(
            state,
            AtomicEventKind::Surveil {
                player,
                count: 0,
                graveyard_count: 0,
            },
        );
        return Ok(StepExecOutcome::Done);
    }
    let first = cards[0];
    let rest = cards[1..].to_vec();
    let (prompt_id, chooser) = push_reorder_yes_no(
        state,
        player,
        ReorderMode::Surveil,
        rest,
        Vec::new(),
        Vec::new(),
        first,
        false,
    );
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

pub(crate) fn exec_look_and_reorder(
    state: &mut GameState,
    step: &EffectStep,
    may_shuffle: bool,
) -> Result<StepExecOutcome, EngineError> {
    let player = resolve_step_actor(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as usize;
    let cards: Vec<ObjectId> = state
        .player(player)
        .map(|p| {
            let n = amt.min(p.library.len());
            p.library[p.library.len() - n..]
                .iter()
                .copied()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    for &id in &cards {
        zones::reveal_to_player(state, id, player);
    }
    if cards.len() >= 2 {
        let (prompt_id, chooser) = push_reorder_order_objects(
            state,
            player,
            ReorderMode::LookAndReorder,
            cards,
            Vec::new(),
            Vec::new(),
            may_shuffle,
        );
        return Ok(StepExecOutcome::Paused { prompt_id, chooser });
    }
    if may_shuffle {
        let prompt_id = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::OptionalChoice {
                chooser: player,
                purpose: YesNoPausePurpose::MayShuffleAfterMoveOrLook,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::YesNo {
            prompt_id,
            label: "Shuffle your library?".into(),
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: player,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(player);
        return Ok(StepExecOutcome::Paused {
            prompt_id,
            chooser: player,
        });
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_reorder_library(
    state: &mut GameState,
    _step: &EffectStep,
    player: PlayerId,
    mode: ReorderMode,
    mut remaining: Vec<ObjectId>,
    mut top_order: Vec<ObjectId>,
    mut bottom_or_gy: Vec<ObjectId>,
    deciding_card: Option<ObjectId>,
    may_shuffle: bool,
    answer: ChoiceAnswer,
) -> Result<StepExecOutcome, EngineError> {
    if let Some(card) = deciding_card {
        let ChoiceAnswer::YesNo { value } = answer else {
            return Err(EngineError::IllegalAction(
                "expected YesNo for Scry/Surveil card decision".into(),
            ));
        };
        if value {
            bottom_or_gy.push(card);
        } else {
            top_order.push(card);
        }
        if let Some(next_card) = (!remaining.is_empty()).then(|| remaining.remove(0)) {
            let (prompt_id, chooser) = push_reorder_yes_no(
                state,
                player,
                mode,
                remaining,
                top_order,
                bottom_or_gy,
                next_card,
                may_shuffle,
            );
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        if top_order.len() >= 2 {
            let (prompt_id, chooser) = push_reorder_order_objects(
                state,
                player,
                mode,
                top_order,
                Vec::new(),
                bottom_or_gy,
                may_shuffle,
            );
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
    } else {
        let ChoiceAnswer::OrderNext { id } = answer else {
            return Err(EngineError::IllegalAction(
                "expected OrderNext for library reorder".into(),
            ));
        };
        let Some(pos) = remaining.iter().position(|&x| x == id) else {
            return Err(EngineError::IllegalAction(
                "ordered card not in remaining set".into(),
            ));
        };
        remaining.remove(pos);
        top_order.push(id);
        if remaining.len() >= 2 {
            let (prompt_id, chooser) = push_reorder_order_objects(
                state,
                player,
                mode,
                remaining,
                top_order,
                bottom_or_gy,
                may_shuffle,
            );
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        if let Some(last_id) = remaining.pop() {
            top_order.push(last_id);
        }
    }

    match mode {
        ReorderMode::Scry => {
            let all_cards: Vec<ObjectId> = top_order
                .iter()
                .chain(bottom_or_gy.iter())
                .copied()
                .collect();
            let total = all_cards.len() as u16;
            if !all_cards.is_empty() {
                zones::rearrange_library(state, player, &all_cards, &top_order, &bottom_or_gy)?;
            }
            emit_event(
                state,
                AtomicEventKind::Scry {
                    player,
                    amount: total,
                },
            );
        }
        ReorderMode::Surveil => {
            let gy_count = bottom_or_gy.len() as u16;
            let total = (top_order.len() + bottom_or_gy.len()) as u16;
            for gid in bottom_or_gy {
                let _ = zones::move_object(state, gid, Zone::Graveyard, Default::default(), None)?;
            }
            if top_order.len() >= 2 {
                zones::rearrange_library(state, player, &top_order, &top_order, &[])?;
            }
            emit_event(
                state,
                AtomicEventKind::Surveil {
                    player,
                    count: total,
                    graveyard_count: gy_count,
                },
            );
        }
        ReorderMode::LookAndReorder => {
            if top_order.len() >= 2 {
                zones::rearrange_library(state, player, &top_order, &top_order, &[])?;
            }
            if may_shuffle {
                let prompt_id = state.allocate_prompt_id();
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pause_state = Some(StepPauseState::OptionalChoice {
                        chooser: player,
                        purpose: YesNoPausePurpose::MayShuffleAfterMoveOrLook,
                    });
                }
                if let Some(fr) = state.resolution_frames_mut().last_mut() {
                    fr.pending_prompt_id = Some(prompt_id);
                }
                let req = ChoiceRequest::YesNo {
                    prompt_id,
                    label: "Shuffle your library?".into(),
                };
                state.set_pending_prompt(Some(PendingPromptContext {
                    prompt_id,
                    chooser: player,
                    request: req.clone(),
                    owner: PromptOwner::ResolutionFrame,
                }));
                state.set_decision(DecisionState::Prompt { request: req });
                state.set_priority_player(player);
                return Ok(StepExecOutcome::Paused {
                    prompt_id,
                    chooser: player,
                });
            }
        }
    }

    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_search(
    state: &mut GameState,
    step: &EffectStep,
    spec: &SearchSpec,
    tapped: bool,
) -> Result<StepExecOutcome, EngineError> {
    let chooser = resolve_step_actor(state, step);
    let searched_player = match spec.searched_player.as_ref() {
        Some(pr) => with_vm_eval(state, |ctx, _| {
            resolve_player_ref(state, pr, ctx).unwrap_or(chooser)
        }),
        None => chooser,
    };

    // Check RuleMod::CantSearchLibraries
    let (cant_search, top_n_limit) = {
        let cache = state.evaluate_board();
        let mut blocked = false;
        let mut top_n: Option<u16> = None;
        if let Some(ep) = cache.player(chooser) {
            for rm in &ep.active_rule_mods {
                if let RuleMod::CantSearchLibraries { top_n: cap } = rm.rule {
                    if let Some(n) = cap {
                        top_n = Some(top_n.map_or(n, |cur| cur.min(n)));
                    } else {
                        blocked = true;
                    }
                }
            }
        }
        (blocked, top_n)
    };
    if cant_search {
        return Ok(StepExecOutcome::Done);
    }

    let zones_mask = if spec.zones.is_empty() {
        ZoneMask::LIBRARY
    } else {
        spec.zones
    };
    let shuffle_library = !spec.shuffle_only_if_library || zones_mask.contains(ZoneMask::LIBRARY);

    let mut candidates: Vec<ObjectId> = with_vm_eval(state, |ctx, _| {
        let mut cands = Vec::new();
        let Some(sp) = state.player(searched_player) else {
            return cands;
        };
        if zones_mask.contains(ZoneMask::LIBRARY) {
            let lib_slice: &[ObjectId] = match top_n_limit {
                Some(n) => {
                    let take_n = usize::from(n).min(sp.library.len());
                    &sp.library[sp.library.len() - take_n..]
                }
                None => &sp.library,
            };
            for &id in lib_slice.iter().rev() {
                if matches_object_filter(state, id, &spec.filter, ctx) {
                    cands.push(id);
                }
            }
        }
        if zones_mask.contains(ZoneMask::HAND) {
            for &id in &sp.hand {
                if matches_object_filter(state, id, &spec.filter, ctx) {
                    cands.push(id);
                }
            }
        }
        if zones_mask.contains(ZoneMask::GRAVEYARD) {
            for &id in &sp.graveyard {
                if matches_object_filter(state, id, &spec.filter, ctx) {
                    cands.push(id);
                }
            }
        }
        if zones_mask.contains(ZoneMask::EXILE) {
            for &id in &sp.exile {
                if matches_object_filter(state, id, &spec.filter, ctx) {
                    cands.push(id);
                }
            }
        }
        cands
    });

    if spec.different_names {
        let mut seen_names: Vec<String> = Vec::new();
        candidates.retain(|&id| {
            let name = state
                .object(id)
                .and_then(|o| state.db().get(o.card_def))
                .map(|c| c.name.clone())
                .unwrap_or_default();
            if seen_names.contains(&name) {
                false
            } else {
                seen_names.push(name);
                true
            }
        });
    }

    let is_multi_dest = spec.destinations.len() > 1;
    let eff_tapped = if is_multi_dest {
        false
    } else {
        tapped || spec.destinations.first().is_some_and(|d| d.tapped)
    };
    let max = if is_multi_dest {
        if candidates.is_empty() { 0 } else { 1 }
    } else {
        spec.count.min(candidates.len() as u16)
    };
    if max == 0 || candidates.is_empty() {
        return finalize_search_picks(
            state,
            step,
            chooser,
            searched_player,
            Vec::new(),
            &spec.destinations,
            shuffle_library,
            spec.reveal,
            eff_tapped,
        );
    }

    let min = if spec.up_to || spec.stated_quality {
        0
    } else {
        max
    };
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::SearchPick {
            chooser,
            searched_player,
            candidates: candidates.clone(),
            selected: Vec::new(),
            min,
            max,
            destinations: spec.destinations.clone(),
            dest_idx: 0,
            moved_ids: Vec::new(),
            shuffle_library,
            reveal: spec.reveal,
            tapped: eff_tapped,
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseObjects {
        prompt_id,
        candidates,
        min,
        max,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(chooser);
    Ok(StepExecOutcome::Paused { prompt_id, chooser })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_multi_dest_search(
    state: &mut GameState,
    step: &EffectStep,
    searched_player: PlayerId,
    moved_ids: Vec<ObjectId>,
    shuffle_library: bool,
) -> Result<StepExecOutcome, EngineError> {
    if shuffle_library {
        zones::shuffle_library(state, searched_player, 0)?;
        emit_event(
            state,
            AtomicEventKind::LibraryShuffled {
                player: searched_player,
            },
        );
    }
    emit_event(
        state,
        AtomicEventKind::LibrarySearched {
            player: searched_player,
        },
    );
    if moved_ids.is_empty() {
        record_step_bindings(state, step.bind_as.as_deref(), &[]);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pending_continuation = None;
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn continue_or_finish_multi_dest_search(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    searched_player: PlayerId,
    mut rem_candidates: Vec<ObjectId>,
    destinations: Vec<SearchDestination>,
    next_dest_idx: usize,
    moved_ids: Vec<ObjectId>,
    shuffle_library: bool,
    reveal: bool,
) -> Result<StepExecOutcome, EngineError> {
    rem_candidates.retain(|&id| state.object(id).is_some());
    if next_dest_idx < destinations.len() && !rem_candidates.is_empty() {
        let next_min = if let mtg_ir::EffectOp::Search { spec, .. } = &step.op {
            if spec.up_to || spec.stated_quality {
                0
            } else {
                1
            }
        } else {
            0
        };
        let next_pid = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::SearchPick {
                chooser,
                searched_player,
                candidates: rem_candidates.clone(),
                selected: Vec::new(),
                min: next_min,
                max: 1,
                destinations,
                dest_idx: next_dest_idx,
                moved_ids,
                shuffle_library,
                reveal,
                tapped: false,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(next_pid);
        }
        let req = ChoiceRequest::ChooseObjects {
            prompt_id: next_pid,
            candidates: rem_candidates,
            min: next_min,
            max: 1,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id: next_pid,
            chooser,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(chooser);
        return Ok(StepExecOutcome::Paused {
            prompt_id: next_pid,
            chooser,
        });
    }
    finish_multi_dest_search(state, step, searched_player, moved_ids, shuffle_library)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn advance_multi_dest_search(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    searched_player: PlayerId,
    candidates: Vec<ObjectId>,
    destinations: Vec<SearchDestination>,
    dest_idx: usize,
    mut moved_ids: Vec<ObjectId>,
    shuffle_library: bool,
    reveal: bool,
    picked_id: ObjectId,
) -> Result<StepExecOutcome, EngineError> {
    let dest_spec = destinations
        .get(dest_idx)
        .or_else(|| destinations.first())
        .cloned()
        .unwrap_or(SearchDestination {
            zone: Zone::Hand,
            tapped: false,
            on_top: false,
        });
    let different_names = matches!(
        &step.op,
        mtg_ir::EffectOp::Search { spec, .. } if spec.different_names
    );
    let picked_name = if different_names {
        state
            .object(picked_id)
            .and_then(|o| state.db().get(o.card_def))
            .map(|c| c.name.clone())
    } else {
        None
    };
    let mut rem_candidates: Vec<ObjectId> = candidates
        .into_iter()
        .filter(|&cid| {
            if cid == picked_id {
                return false;
            }
            if let Some(ref pname) = picked_name {
                let cname = state
                    .object(cid)
                    .and_then(|o| state.db().get(o.card_def))
                    .map(|c| c.name.as_str());
                if cname == Some(pname.as_str()) {
                    return false;
                }
            }
            true
        })
        .collect();

    if reveal {
        zones::reveal_to_all(state, picked_id);
    }
    let pos = if dest_spec.on_top {
        ZonePosition::Top
    } else {
        ZonePosition::Default
    };
    let opts = move_opts_for_zone(dest_spec.zone, dest_spec.tapped, Some(chooser), pos);
    match zones::move_object(state, picked_id, dest_spec.zone, opts, None)? {
        MoveObjectOutcome::Moved(raw) => {
            let final_id = raw.new_id.unwrap_or(raw.old_id);
            moved_ids.push(final_id);
            append_step_binding(state, step.bind_as.as_deref(), final_id, raw.lki);
        }
        MoveObjectOutcome::Paused {
            prompt_id,
            chooser: rep_chooser,
        } => {
            rem_candidates.retain(|&id| state.object(id).is_some());
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                    search_moved_ids: moved_ids,
                    search_destinations: destinations,
                    search_dest_idx: dest_idx,
                    search_different_names: different_names,
                    search_candidates: rem_candidates,
                    search_searched_player: Some(searched_player),
                    search_chooser: Some(chooser),
                    search_shuffle: shuffle_library,
                    search_reveal: reveal,
                    search_tapped: false,
                    ..Default::default()
                }));
            }
            set_waiting_on_replacement(state, prompt_id, vec![picked_id], 0, OP_TAG_SEARCH);
            return Ok(StepExecOutcome::Paused {
                prompt_id,
                chooser: rep_chooser,
            });
        }
        MoveObjectOutcome::ExecuteInstead { object, steps, .. } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.subframes.push(ControlFlowFrame::SubSteps {
                    steps,
                    step_pc: 0,
                    event_object: Some(object),
                    event_player: None,
                    event_amount: None,
                });
            }
            return Ok(StepExecOutcome::EnterSubframe);
        }
        MoveObjectOutcome::Skipped { .. } => {}
    }

    continue_or_finish_multi_dest_search(
        state,
        step,
        chooser,
        searched_player,
        rem_candidates,
        destinations,
        dest_idx + 1,
        moved_ids,
        shuffle_library,
        reveal,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_search_picks_from(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    searched_player: PlayerId,
    selected: &[ObjectId],
    start_idx: usize,
    destinations: &[SearchDestination],
    shuffle_library: bool,
    reveal: bool,
    tapped: bool,
    mut moved_ids: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    for (off, &id) in selected[start_idx..].iter().enumerate() {
        let idx = start_idx + off;
        let dest_spec = destinations
            .get(idx)
            .or_else(|| destinations.first())
            .cloned()
            .unwrap_or(SearchDestination {
                zone: Zone::Hand,
                tapped: false,
                on_top: false,
            });
        if reveal {
            zones::reveal_to_all(state, id);
        }
        let pos = if dest_spec.on_top {
            ZonePosition::Top
        } else {
            ZonePosition::Default
        };
        let opts = move_opts_for_zone(
            dest_spec.zone,
            tapped || dest_spec.tapped,
            Some(chooser),
            pos,
        );
        match zones::move_object(state, id, dest_spec.zone, opts, None)? {
            MoveObjectOutcome::Moved(raw) => {
                let final_id = raw.new_id.unwrap_or(raw.old_id);
                moved_ids.push(final_id);
                append_step_binding(state, step.bind_as.as_deref(), final_id, raw.lki);
            }
            MoveObjectOutcome::Paused {
                prompt_id,
                chooser: rep_chooser,
            } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                        search_moved_ids: moved_ids,
                        search_destinations: destinations.to_vec(),
                        search_searched_player: Some(searched_player),
                        search_chooser: Some(chooser),
                        search_shuffle: shuffle_library,
                        search_reveal: reveal,
                        search_tapped: tapped,
                        ..Default::default()
                    }));
                }
                set_waiting_on_replacement(state, prompt_id, selected.to_vec(), idx, OP_TAG_SEARCH);
                return Ok(StepExecOutcome::Paused {
                    prompt_id,
                    chooser: rep_chooser,
                });
            }
            MoveObjectOutcome::ExecuteInstead { object, steps, .. } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.subframes.push(ControlFlowFrame::SubSteps {
                        steps,
                        step_pc: 0,
                        event_object: Some(object),
                        event_player: None,
                        event_amount: None,
                    });
                }
                return Ok(StepExecOutcome::EnterSubframe);
            }
            MoveObjectOutcome::Skipped { .. } => {}
        }
    }
    if shuffle_library {
        zones::shuffle_library(state, searched_player, 0)?;
        emit_event(
            state,
            AtomicEventKind::LibraryShuffled {
                player: searched_player,
            },
        );
    }
    emit_event(
        state,
        AtomicEventKind::LibrarySearched {
            player: searched_player,
        },
    );
    if moved_ids.is_empty() {
        record_step_bindings(state, step.bind_as.as_deref(), &[]);
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pending_continuation = None;
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_search_picks(
    state: &mut GameState,
    step: &EffectStep,
    chooser: PlayerId,
    searched_player: PlayerId,
    selected: Vec<ObjectId>,
    destinations: &[SearchDestination],
    shuffle_library: bool,
    reveal: bool,
    tapped: bool,
) -> Result<StepExecOutcome, EngineError> {
    finalize_search_picks_from(
        state,
        step,
        chooser,
        searched_player,
        &selected,
        0,
        destinations,
        shuffle_library,
        reveal,
        tapped,
        Vec::new(),
    )
}

pub(crate) fn exec_dig(
    state: &mut GameState,
    step: &EffectStep,
    spec: &DigSpec,
    look_only: bool,
) -> Result<StepExecOutcome, EngineError> {
    let player = resolve_step_actor(state, step);
    let n = resolve_step_amount(state, step, 1).max(0) as usize;
    let revealed: Vec<ObjectId> = state
        .player(player)
        .map(|p| {
            let take_n = n.min(p.library.len());
            p.library[p.library.len() - take_n..]
                .iter()
                .copied()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    if revealed.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    for &id in &revealed {
        if spec.reveal_all && !look_only {
            zones::reveal_to_all(state, id);
        } else {
            zones::reveal_to_player(state, id, player);
        }
    }

    if !spec.partitions.is_empty() {
        if revealed.len() == 1 {
            let only_id = revealed[0];
            return finalize_dig_partitions(state, step, player, spec, revealed, vec![only_id]);
        }
        let prompt_id = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::DigPick {
                player,
                revealed: revealed.clone(),
                candidates: revealed.clone(),
                selected: Vec::new(),
                min: 1,
                max: 1,
                partition_picks: Vec::new(),
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(prompt_id);
        }
        let req = ChoiceRequest::ChooseObjects {
            prompt_id,
            candidates: revealed,
            min: 1,
            max: 1,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id,
            chooser: player,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(player);
        return Ok(StepExecOutcome::Paused {
            prompt_id,
            chooser: player,
        });
    }

    let candidates: Vec<ObjectId> = if let Some(kf) = spec.keep_filter.as_deref() {
        with_vm_eval(state, |ctx, _| {
            revealed
                .iter()
                .copied()
                .filter(|&id| matches_object_filter(state, id, kf, ctx))
                .collect()
        })
    } else {
        revealed.clone()
    };

    if spec.keep_all_matching || candidates.is_empty() {
        let kept = if spec.keep_all_matching {
            candidates
        } else {
            Vec::new()
        };
        return finalize_dig_picks(state, step, player, revealed, kept);
    }

    let max = spec.keep.min(candidates.len() as u16);
    let min = if spec.up_to { 0 } else { max };
    let prompt_id = state.allocate_prompt_id();
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pause_state = Some(StepPauseState::DigPick {
            player,
            revealed,
            candidates: candidates.clone(),
            selected: Vec::new(),
            min,
            max,
            partition_picks: Vec::new(),
        });
    }
    if let Some(fr) = state.resolution_frames_mut().last_mut() {
        fr.pending_prompt_id = Some(prompt_id);
    }
    let req = ChoiceRequest::ChooseObjects {
        prompt_id,
        candidates,
        min,
        max,
    };
    state.set_pending_prompt(Some(PendingPromptContext {
        prompt_id,
        chooser: player,
        request: req.clone(),
        owner: PromptOwner::ResolutionFrame,
    }));
    state.set_decision(DecisionState::Prompt { request: req });
    state.set_priority_player(player);
    Ok(StepExecOutcome::Paused {
        prompt_id,
        chooser: player,
    })
}

pub(crate) fn advance_dig_partitions(
    state: &mut GameState,
    step: &EffectStep,
    player: PlayerId,
    spec: &DigSpec,
    revealed: Vec<ObjectId>,
    mut partition_picks: Vec<ObjectId>,
    picked_id: ObjectId,
) -> Result<StepExecOutcome, EngineError> {
    partition_picks.push(picked_id);
    let rem: Vec<ObjectId> = revealed
        .iter()
        .copied()
        .filter(|id| !partition_picks.contains(id))
        .collect();
    let has_second_non_bottom = spec
        .partitions
        .iter()
        .any(|p| !p.to_hand && p.to != DigRestTo::LibraryBottom);
    if partition_picks.len() == 1 && rem.len() >= 2 && has_second_non_bottom {
        let next_pid = state.allocate_prompt_id();
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.pause_state = Some(StepPauseState::DigPick {
                player,
                revealed,
                candidates: rem.clone(),
                selected: Vec::new(),
                min: 1,
                max: 1,
                partition_picks,
            });
        }
        if let Some(fr) = state.resolution_frames_mut().last_mut() {
            fr.pending_prompt_id = Some(next_pid);
        }
        let req = ChoiceRequest::ChooseObjects {
            prompt_id: next_pid,
            candidates: rem,
            min: 1,
            max: 1,
        };
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id: next_pid,
            chooser: player,
            request: req.clone(),
            owner: PromptOwner::ResolutionFrame,
        }));
        state.set_decision(DecisionState::Prompt { request: req });
        state.set_priority_player(player);
        return Ok(StepExecOutcome::Paused {
            prompt_id: next_pid,
            chooser: player,
        });
    }
    finalize_dig_partitions(state, step, player, spec, revealed, partition_picks)
}

pub(crate) fn finalize_dig_partitions(
    state: &mut GameState,
    step: &EffectStep,
    player: PlayerId,
    spec: &DigSpec,
    revealed: Vec<ObjectId>,
    partition_picks: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let hand_part = spec.partitions.iter().find(|p| p.to_hand);
    let second_part = spec
        .partitions
        .iter()
        .find(|p| !p.to_hand && p.to != DigRestTo::LibraryBottom);

    let hand_id = if hand_part.is_some() {
        partition_picks.first().copied()
    } else {
        None
    };
    let second_id = if second_part.is_some() {
        if hand_part.is_some() {
            partition_picks
                .get(1)
                .copied()
                .or_else(|| revealed.iter().copied().find(|id| Some(*id) != hand_id))
        } else {
            partition_picks.first().copied()
        }
    } else {
        None
    };
    let bottom_ids: Vec<ObjectId> = revealed
        .iter()
        .copied()
        .filter(|&id| Some(id) != hand_id && Some(id) != second_id)
        .collect();

    let mut batch = Vec::new();
    let mut roles = SmallVec::<[DigMoveRole; 8]>::new();
    if let Some(hid) = hand_id {
        batch.push(hid);
        roles.push(DigMoveRole::PartitionHand);
    }
    if let (Some(sid), Some(_)) = (second_id, second_part) {
        batch.push(sid);
        roles.push(DigMoveRole::PartitionSecond);
    }
    for bid in bottom_ids {
        batch.push(bid);
        roles.push(DigMoveRole::PartitionBottom);
    }

    record_step_bindings(state, step.bind_as.as_deref(), &[]);
    run_dig_move_batch_from(state, step, player, spec, &batch, &roles, 0)
}

pub(crate) fn finalize_dig_picks(
    state: &mut GameState,
    step: &EffectStep,
    player: PlayerId,
    revealed: Vec<ObjectId>,
    selected: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let EffectOpRef::Dig(spec) = EffectOpRef::from_step(step) else {
        return Ok(StepExecOutcome::Done);
    };
    let mut batch = Vec::new();
    let mut roles = SmallVec::<[DigMoveRole; 8]>::new();
    for &id in &selected {
        batch.push(id);
        roles.push(DigMoveRole::PickKept);
    }
    for id in revealed.into_iter().filter(|id| !selected.contains(id)) {
        batch.push(id);
        roles.push(DigMoveRole::PickRest);
    }

    record_step_bindings(state, step.bind_as.as_deref(), &[]);
    run_dig_move_batch_from(state, step, player, spec, &batch, &roles, 0)
}

fn dig_move_params(spec: &DigSpec, role: DigMoveRole) -> (Zone, ZonePosition, bool, bool) {
    match role {
        DigMoveRole::PartitionHand => (Zone::Hand, ZonePosition::Default, false, spec.reveal_kept),
        DigMoveRole::PartitionSecond => {
            let second_part = spec
                .partitions
                .iter()
                .find(|p| !p.to_hand && p.to != DigRestTo::LibraryBottom);
            let (dest, pos) = match second_part.map(|p| p.to) {
                Some(DigRestTo::Exile) => (Zone::Exile, ZonePosition::Default),
                Some(DigRestTo::Graveyard) => (Zone::Graveyard, ZonePosition::Default),
                Some(DigRestTo::LibraryTop) => (Zone::Library, ZonePosition::Top),
                Some(DigRestTo::LibraryBottom) | None => (Zone::Library, ZonePosition::Bottom),
            };
            (dest, pos, false, false)
        }
        DigMoveRole::PartitionBottom => (Zone::Library, ZonePosition::Bottom, false, false),
        DigMoveRole::PickKept => {
            let (dest, pos) = match spec.keep_to {
                DigKeepTo::LibraryTop => (Zone::Library, ZonePosition::Top),
                DigKeepTo::Hand => (Zone::Hand, ZonePosition::Default),
                DigKeepTo::Battlefield => (Zone::Battlefield, ZonePosition::Default),
                DigKeepTo::Graveyard => (Zone::Graveyard, ZonePosition::Default),
                DigKeepTo::Exile | DigKeepTo::CastFree => (Zone::Exile, ZonePosition::Default),
            };
            (dest, pos, spec.keep_tapped, spec.reveal_kept)
        }
        DigMoveRole::PickRest => {
            let (dest, pos) = if spec.rest_to_hand {
                (Zone::Hand, ZonePosition::Default)
            } else {
                match spec.rest {
                    DigRestTo::LibraryBottom => (Zone::Library, ZonePosition::Bottom),
                    DigRestTo::LibraryTop => (Zone::Library, ZonePosition::Top),
                    DigRestTo::Graveyard => (Zone::Graveyard, ZonePosition::Default),
                    DigRestTo::Exile => (Zone::Exile, ZonePosition::Default),
                }
            };
            (dest, pos, false, false)
        }
    }
}

fn run_dig_move_batch_from(
    state: &mut GameState,
    step: &EffectStep,
    player: PlayerId,
    spec: &DigSpec,
    batch: &[ObjectId],
    roles: &[DigMoveRole],
    start_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    for (off, &id) in batch.get(start_idx..).unwrap_or(&[]).iter().enumerate() {
        let idx = start_idx + off;
        let Some(&role) = roles.get(idx) else {
            break;
        };
        let (dest, pos, tapped, reveal_before) = dig_move_params(spec, role);
        if reveal_before {
            zones::reveal_to_all(state, id);
        }
        let opts = move_opts_for_zone(dest, tapped, Some(player), pos);
        let mv = zones::move_object(state, id, dest, opts, None)?;
        let out = apply_dig_move_outcome(state, step, player, spec, role, mv, batch, roles, idx)?;
        if !matches!(out, StepExecOutcome::Done) {
            return Ok(out);
        }
    }
    if spec.partitions.is_empty() && spec.shuffle_rest {
        zones::shuffle_library(state, player, 0)?;
        emit_event(state, AtomicEventKind::LibraryShuffled { player });
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.pending_continuation = None;
    }
    Ok(StepExecOutcome::Done)
}

#[allow(clippy::too_many_arguments)]
fn apply_dig_move_outcome(
    state: &mut GameState,
    step: &EffectStep,
    player: PlayerId,
    spec: &DigSpec,
    role: DigMoveRole,
    mv: MoveObjectOutcome,
    batch: &[ObjectId],
    roles: &[DigMoveRole],
    idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    match mv {
        MoveObjectOutcome::Moved(raw) => {
            let final_id = raw.new_id.unwrap_or(raw.old_id);
            match role {
                DigMoveRole::PartitionSecond => {
                    let second_part = spec
                        .partitions
                        .iter()
                        .find(|p| !p.to_hand && p.to != DigRestTo::LibraryBottom);
                    if second_part.is_some_and(|sp| sp.playable_this_turn)
                        && !raw.ceased_to_exist
                        && raw.to_zone == Zone::Exile
                    {
                        let cur_turn = state.turn_number();
                        if let Some(r) = state.rare_or_default(final_id) {
                            r.playable_from_exile_by = Some(PlayableFromExileState {
                                player,
                                expires_turn: Some(cur_turn),
                                cast_with_mana_cost: None,
                            });
                            state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
                        }
                    }
                    append_step_binding(state, step.bind_as.as_deref(), final_id, raw.lki);
                }
                DigMoveRole::PickKept => {
                    if let Some(pfe) = &spec.play_from_exile
                        && !raw.ceased_to_exist
                        && raw.to_zone == Zone::Exile
                    {
                        let cur_turn = state.turn_number();
                        if let Some(r) = state.rare_or_default(final_id) {
                            let exp = match pfe.duration {
                                PlayFromExileDuration::ThisTurn => Some(cur_turn),
                                PlayFromExileDuration::UntilEndOfYourNextTurn
                                | PlayFromExileDuration::UntilYourNextEndStep => Some(cur_turn + 1),
                                PlayFromExileDuration::AsLongAsExiled => None,
                            };
                            r.playable_from_exile_by = Some(PlayableFromExileState {
                                player,
                                expires_turn: exp,
                                cast_with_mana_cost: pfe.alternative_mana_cost.clone(),
                            });
                        }
                        state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
                    }
                    append_step_binding(state, step.bind_as.as_deref(), final_id, raw.lki);
                }
                DigMoveRole::PartitionHand => {
                    append_step_binding(state, step.bind_as.as_deref(), final_id, raw.lki);
                }
                DigMoveRole::PartitionBottom | DigMoveRole::PickRest => {}
            }
            Ok(StepExecOutcome::Done)
        }
        MoveObjectOutcome::Skipped { .. } => Ok(StepExecOutcome::Done),
        MoveObjectOutcome::Paused { prompt_id, chooser } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                    dig_player: Some(player),
                    dig_roles: roles.iter().copied().collect(),
                    ..Default::default()
                }));
            }
            set_waiting_on_replacement(state, prompt_id, batch.to_vec(), idx, OP_TAG_DIG);
            Ok(StepExecOutcome::Paused { prompt_id, chooser })
        }
        MoveObjectOutcome::ExecuteInstead { object, steps, .. } => {
            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                vm.subframes.push(ControlFlowFrame::SubSteps {
                    steps,
                    step_pc: 0,
                    event_object: Some(object),
                    event_player: None,
                    event_amount: None,
                });
            }
            Ok(StepExecOutcome::EnterSubframe)
        }
    }
}

pub(crate) fn resume_dig_after_replacement(
    state: &mut GameState,
    step: &EffectStep,
    mv: MoveObjectOutcome,
    batch_remaining: &[ObjectId],
    batch_idx: usize,
) -> Result<StepExecOutcome, EngineError> {
    let cont = state
        .resolution_vms_mut()
        .last_mut()
        .and_then(|vm| vm.pending_continuation.take())
        .unwrap_or_default();
    let player = cont
        .dig_player
        .unwrap_or_else(|| resolve_step_actor(state, step));
    let roles = cont.dig_roles;
    let EffectOpRef::Dig(spec) = EffectOpRef::from_step(step) else {
        return Ok(StepExecOutcome::Done);
    };
    let Some(&role) = roles.get(batch_idx) else {
        return Ok(StepExecOutcome::Done);
    };
    let res = apply_dig_move_outcome(
        state,
        step,
        player,
        spec,
        role,
        mv,
        batch_remaining,
        &roles,
        batch_idx,
    )?;
    if !matches!(res, StepExecOutcome::Done) {
        return Ok(res);
    }
    run_dig_move_batch_from(
        state,
        step,
        player,
        spec,
        batch_remaining,
        &roles,
        batch_idx + 1,
    )
}

enum EffectOpRef<'a> {
    Dig(&'a DigSpec),
    Other,
}

impl<'a> EffectOpRef<'a> {
    fn from_step(step: &'a EffectStep) -> Self {
        match &step.op {
            mtg_ir::EffectOp::Dig { spec, .. } => Self::Dig(spec),
            _ => Self::Other,
        }
    }
}

pub(crate) fn exec_exile_until(
    state: &mut GameState,
    step: &EffectStep,
    spec: &mtg_ir::ExileUntilSpec,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let lib_owner = match spec.library_owner.as_deref() {
        Some(sel) => {
            let mut tmp = step.clone();
            tmp.subject = sel.clone();
            resolve_target_players(state, &tmp)
                .first()
                .copied()
                .or_else(|| {
                    resolve_selector_objects(state, sel)
                        .first()
                        .and_then(|&id| state.object(id).map(|o| o.controller))
                })
                .unwrap_or(actor)
        }
        None => actor,
    };

    let mut exiled_misses = Vec::new();
    let mut hit: Option<ObjectId> = None;

    loop {
        let top = state
            .player(lib_owner)
            .and_then(|p| p.library.last().copied());
        let Some(cid) = top else {
            break;
        };
        let mv = zones::move_object(state, cid, Zone::Exile, Default::default(), None)?;
        let new_id = match mv {
            MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            _ => break,
        };
        let vm_src = vm_source(state);
        if let Some(r) = state.rare_or_default(new_id) {
            r.exiled_by = vm_src;
        }
        state.mark_dirty(DirtyMask::EXILE);
        let is_match = with_vm_eval(state, |ctx, _| {
            matches_object_filter(state, new_id, &spec.filter, ctx)
        });
        if is_match {
            hit = Some(new_id);
            break;
        }
        exiled_misses.push(new_id);
    }

    if let Some(hid) = hit {
        let cur_turn = state.turn_number();
        match spec.hit_action {
            ExileUntilHitAction::Hand => {
                let mv = zones::move_object(state, hid, Zone::Hand, Default::default(), None)?;
                if let MoveObjectOutcome::Moved(raw) = mv {
                    append_step_binding(
                        state,
                        step.bind_as.as_deref(),
                        raw.new_id.unwrap_or(raw.old_id),
                        raw.lki,
                    );
                }
            }
            ExileUntilHitAction::Battlefield | ExileUntilHitAction::BattlefieldOrHand => {
                let opts = move_opts_for_zone(
                    Zone::Battlefield,
                    false,
                    Some(actor),
                    ZonePosition::Default,
                );
                let mv = zones::move_object(state, hid, Zone::Battlefield, opts, None)?;
                if let MoveObjectOutcome::Moved(raw) = mv {
                    append_step_binding(
                        state,
                        step.bind_as.as_deref(),
                        raw.new_id.unwrap_or(raw.old_id),
                        raw.lki,
                    );
                }
            }
            ExileUntilHitAction::PlayThisTurn => {
                if let Some(r) = state.rare_or_default(hid) {
                    r.playable_from_exile_by = Some(PlayableFromExileState {
                        player: actor,
                        expires_turn: Some(cur_turn),
                        cast_with_mana_cost: None,
                    });
                }
                state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
                append_step_binding(state, step.bind_as.as_deref(), hid, None);
            }
            ExileUntilHitAction::PlayUntilEndOfNextTurn => {
                if let Some(r) = state.rare_or_default(hid) {
                    r.playable_from_exile_by = Some(PlayableFromExileState {
                        player: actor,
                        expires_turn: Some(cur_turn + 1),
                        cast_with_mana_cost: None,
                    });
                }
                state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
                append_step_binding(state, step.bind_as.as_deref(), hid, None);
            }
            ExileUntilHitAction::CastFree
            | ExileUntilHitAction::StayExiled
            | ExileUntilHitAction::None => {
                if matches!(spec.hit_action, ExileUntilHitAction::CastFree)
                    && let Some(r) = state.rare_or_default(hid)
                {
                    r.playable_from_exile_by = Some(PlayableFromExileState {
                        player: actor,
                        expires_turn: Some(cur_turn),
                        cast_with_mana_cost: Some(Default::default()),
                    });
                    state.mark_dirty(DirtyMask::EXILE | DirtyMask::PLAYER_STATE);
                }
                append_step_binding(state, step.bind_as.as_deref(), hid, None);
            }
        }
    }

    match spec.rest_destination {
        ExileUntilRest::LibraryBottomRandom => {
            for mid in exiled_misses {
                let opts = move_opts_for_zone(Zone::Library, false, None, ZonePosition::Bottom);
                let _ = zones::move_object(state, mid, Zone::Library, opts, None)?;
            }
        }
        ExileUntilRest::Graveyard => {
            for mid in exiled_misses {
                let _ = zones::move_object(state, mid, Zone::Graveyard, Default::default(), None)?;
            }
        }
        ExileUntilRest::StayExiled => {}
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_pile_split(
    state: &mut GameState,
    step: &EffectStep,
    spec: &PileSplitSpec,
) -> Result<StepExecOutcome, EngineError> {
    let objs = resolve_selector_objects(state, &step.subject);
    if objs.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    if spec.reveal {
        for &id in &objs {
            zones::reveal_to_all(state, id);
        }
    }
    let mid = objs.len() / 2;
    let pile_a = &objs[..mid];
    let pile_b = &objs[mid..];
    for &id in pile_b {
        let _ = zones::move_object(state, id, spec.chosen_to, Default::default(), None)?;
    }
    let other_dest = match spec.other_to {
        DigRestTo::LibraryBottom => (Zone::Library, ZonePosition::Bottom),
        DigRestTo::LibraryTop => (Zone::Library, ZonePosition::Top),
        DigRestTo::Graveyard => (Zone::Graveyard, ZonePosition::Default),
        DigRestTo::Exile => (Zone::Exile, ZonePosition::Default),
    };
    for &id in pile_a {
        let opts = move_opts_for_zone(other_dest.0, false, None, other_dest.1);
        let _ = zones::move_object(state, id, other_dest.0, opts, None)?;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_reveal_targets(
    state: &mut GameState,
    step: &EffectStep,
    targets: Vec<ObjectId>,
    look_only: bool,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    for &id in &targets {
        if look_only {
            zones::reveal_to_player(state, id, actor);
        } else {
            zones::reveal_to_all(state, id);
        }
    }
    if !targets.is_empty()
        && let Some(vm) = state.resolution_vms_mut().last_mut()
    {
        vm.step_way_stats.did_reveal_this_way = true;
    }
    let acted: Vec<_> = targets.into_iter().map(|id| (id, None)).collect();
    record_step_bindings(state, step.bind_as.as_deref(), &acted);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_reveal(
    state: &mut GameState,
    step: &EffectStep,
    select: &SelectionMod,
    look_only: bool,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let raw_cands = resolve_selector_objects(state, &step.subject);
    let targets = match resolve_selection_mod(state, raw_cands, select, actor, ZoneMask::HAND)? {
        SelectionResult::Paused { prompt_id, chooser } => {
            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
        }
        SelectionResult::Ready(v) => v,
    };
    run_reveal_targets(state, step, targets, look_only)
}

pub(crate) fn exec_pay_cost(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let actor = resolve_step_actor(state, step);
    let amt = resolve_step_amount(state, step, 0).max(0) as u32;
    let cost = mtg_ir::ManaCost {
        generic: amt,
        ..Default::default()
    };
    let spend_ctx = mana::ManaSpendContext::for_generic_cost(actor);
    if mana::can_pay_mana_from_pool(state, actor, &cost, &spend_ctx) {
        mana::deduct_mana_for_context(state, actor, &cost, &spend_ctx, &[])?;
        return Ok(StepExecOutcome::Done);
    }
    Ok(StepExecOutcome::Declined)
}

fn create_simple_token(
    state: &mut GameState,
    ctrl: PlayerId,
    spec: TokenSpec,
    count: u32,
    bind_as: Option<&str>,
) -> Result<(), EngineError> {
    let src = vm_source(state);
    let opts = CreateTokenOptions {
        owner: Some(ctrl),
        created_by_source: src,
        ..Default::default()
    };
    if let ReplacementResult::Resolved(out) =
        replace::resolve_create_token(state, ctrl, Some(spec), count, src, opts)?
    {
        let dummy_step = EffectStep {
            op: mtg_ir::EffectOp::Investigate,
            subject: mtg_ir::Selector::None,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: bind_as.map(str::to_string),
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        apply_resolved_create_token(state, &dummy_step, &out)?;
    }
    Ok(())
}

pub(crate) fn exec_investigate(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let count = resolve_step_amount(state, step, 1).max(0) as u32;
    if count == 0 {
        return Ok(StepExecOutcome::Done);
    }
    let mut subtypes = SmallVec::new();
    if let Some(sid) = state.db().subtype_id("Clue") {
        subtypes.push(sid);
    }
    let spec = TokenSpec {
        name: "Clue".into(),
        power: 0,
        toughness: 0,
        colors: ColorMask::empty(),
        types: CardTypeMask::ARTIFACT,
        supertypes: Default::default(),
        subtypes,
        type_line: "Artifact — Clue".into(),
        keywords: SmallVec::new(),
        tapped: false,
        quoted_abilities: vec!["{2}, Sacrifice this artifact: Draw a card.".into()],
        legendary: false,
    };
    create_simple_token(state, ctrl, spec, count, step.bind_as.as_deref())?;
    emit_event(
        state,
        AtomicEventKind::Investigate {
            player: ctrl,
            count: count as u16,
        },
    );
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_explore(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let subjects = resolve_selector_objects(state, &step.subject);
    for id in subjects {
        let top = state.player(ctrl).and_then(|p| p.library.last().copied());
        let Some(top_id) = top else {
            emit_event(
                state,
                AtomicEventKind::Explore {
                    player: ctrl,
                    object: id,
                    revealed_land: false,
                },
            );
            continue;
        };
        zones::reveal_to_all(state, top_id);
        let is_land = {
            let cache = state.evaluate_board();
            state.object(top_id).is_some_and(|o| {
                let ctx = EvalContext::new(o.controller).with_board(BoardView::Cached(cache));
                crate::eval::off_battlefield_characteristics(state, top_id, o, Zone::Library, &ctx)
                    .types
                    .contains(CardTypeMask::LAND)
            })
        };
        if is_land {
            let _ = zones::move_object(state, top_id, Zone::Hand, Default::default(), None)?;
        } else if let Some(cid) = state.db().counter_id("+1/+1")
            && let ReplacementResult::Resolved(out) =
                replace::resolve_put_counters(state, id, cid, 1, Some(ctrl), src)?
        {
            apply_resolved_put_counters(state, &out, ctrl);
        }
        emit_event(
            state,
            AtomicEventKind::Explore {
                player: ctrl,
                object: id,
                revealed_land: is_land,
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_connive(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let n = resolve_step_amount(state, step, 1).max(0) as u32;
    let subjects = resolve_selector_objects(state, &step.subject);
    for id in subjects {
        for _ in 0..n {
            let _ = zones::draw_one_raw(state, ctrl)?;
        }
        let mut nonland = 0u16;
        for _ in 0..n {
            let disc = state.player(ctrl).and_then(|p| p.hand.last().copied());
            let Some(did) = disc else { break };
            let is_land = {
                let cache = state.evaluate_board();
                state.object(did).is_some_and(|o| {
                    let ctx = EvalContext::new(o.controller).with_board(BoardView::Cached(cache));
                    crate::eval::off_battlefield_characteristics(state, did, o, Zone::Hand, &ctx)
                        .types
                        .contains(CardTypeMask::LAND)
                })
            };
            let _ = zones::move_object(
                state,
                did,
                Zone::Graveyard,
                MoveRawOptions {
                    is_discard: true,
                    discard_caused_by: Some(ctrl),
                    ..Default::default()
                },
                None,
            )?;
            if !is_land {
                nonland += 1;
            }
        }
        if nonland > 0
            && let Some(cid) = state.db().counter_id("+1/+1")
            && let ReplacementResult::Resolved(out) =
                replace::resolve_put_counters(state, id, cid, u32::from(nonland), Some(ctrl), src)?
        {
            apply_resolved_put_counters(state, &out, ctrl);
        }
        emit_event(
            state,
            AtomicEventKind::Connive {
                player: ctrl,
                object: id,
                nonland_discarded: nonland,
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_amass(
    state: &mut GameState,
    step: &EffectStep,
    subtype: Option<SubtypeId>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let n = resolve_step_amount(state, step, 1).max(0) as u32;
    let army_sid = state.db().subtype_id("Army");
    let existing_army = army_sid.and_then(|asid| {
        let cache = state.evaluate_board();
        state
            .player(ctrl)?
            .battlefield
            .iter()
            .copied()
            .find(|&id| cache.get(id).is_some_and(|e| e.subtypes.contains(&asid)))
    });
    let target_army = if let Some(aid) = existing_army {
        aid
    } else {
        let mut subtypes = SmallVec::new();
        if let Some(st) = subtype.or_else(|| state.db().subtype_id("Zombie")) {
            subtypes.push(st);
        }
        if let Some(asid) = army_sid {
            subtypes.push(asid);
        }
        let spec = TokenSpec {
            name: "Army".into(),
            power: 0,
            toughness: 0,
            colors: ColorMask::B,
            types: CardTypeMask::CREATURE,
            supertypes: Default::default(),
            subtypes,
            type_line: "Creature — Zombie Army".into(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: Vec::new(),
            legendary: false,
        };
        let tid = zones::create_token_raw(state, ctrl, spec, Default::default())?;
        emit_event(
            state,
            AtomicEventKind::TokenCreated {
                player: ctrl,
                object: tid,
            },
        );
        tid
    };
    if n > 0
        && let Some(cid) = state.db().counter_id("+1/+1")
        && let ReplacementResult::Resolved(out) =
            replace::resolve_put_counters(state, target_army, cid, n, Some(ctrl), src)?
    {
        apply_resolved_put_counters(state, &out, ctrl);
    }
    let event_subtype = subtype
        .or_else(|| state.db().subtype_id("Zombie"))
        .unwrap_or(SubtypeId(0));
    emit_event(
        state,
        AtomicEventKind::Amass {
            player: ctrl,
            object: Some(target_army),
            subtype: event_subtype,
            amount: n as u16,
        },
    );
    append_step_binding(state, step.bind_as.as_deref(), target_army, None);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_incubate(
    state: &mut GameState,
    step: &EffectStep,
    count: Option<&mtg_ir::Amount>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let src = vm_source(state);
    let n = count
        .map(|a| with_vm_eval(state, |ctx, _| evaluate_amount(state, a, ctx).max(0) as u32))
        .unwrap_or_else(|| resolve_step_amount(state, step, 1).max(0) as u32);
    let mut subtypes = SmallVec::new();
    if let Some(sid) = state.db().subtype_id("Incubator") {
        subtypes.push(sid);
    }
    let spec = TokenSpec {
        name: "Incubator".into(),
        power: 0,
        toughness: 0,
        colors: ColorMask::empty(),
        types: CardTypeMask::ARTIFACT,
        supertypes: Default::default(),
        subtypes,
        type_line: "Artifact — Incubator".into(),
        keywords: SmallVec::new(),
        tapped: false,
        quoted_abilities: Vec::new(),
        legendary: false,
    };
    let tid = zones::create_token_raw(state, ctrl, spec, Default::default())?;
    emit_event(
        state,
        AtomicEventKind::TokenCreated {
            player: ctrl,
            object: tid,
        },
    );
    if n > 0
        && let Some(cid) = state.db().counter_id("+1/+1")
        && let ReplacementResult::Resolved(out) =
            replace::resolve_put_counters(state, tid, cid, n, Some(ctrl), src)?
    {
        apply_resolved_put_counters(state, &out, ctrl);
    }
    append_step_binding(state, step.bind_as.as_deref(), tid, None);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn run_populate_targets(
    state: &mut GameState,
    step: &EffectStep,
    picked: Vec<ObjectId>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    for id in picked {
        let tid = zones::create_token_copy_raw(state, ctrl, id, None, false)?;
        emit_event(
            state,
            AtomicEventKind::TokenCreated {
                player: ctrl,
                object: tid,
            },
        );
        append_step_binding(state, step.bind_as.as_deref(), tid, None);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_populate(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let token_cands: Vec<ObjectId> = {
        let cache = state.evaluate_board();
        state
            .player(ctrl)
            .map(|p| {
                p.battlefield
                    .iter()
                    .copied()
                    .filter(|&id| {
                        state.object(id).is_some_and(|o| o.is_token())
                            && cache
                                .get(id)
                                .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    if token_cands.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let sel_mod = SelectionMod {
        choice_count: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
        ..Default::default()
    };
    let picked =
        match resolve_selection_mod(state, token_cands, &sel_mod, ctrl, ZoneMask::BATTLEFIELD)? {
            SelectionResult::Paused { prompt_id, chooser } => {
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            SelectionResult::Ready(v) => v,
        };
    run_populate_targets(state, step, picked)
}

pub(crate) fn exec_manifest_or_cloak(
    state: &mut GameState,
    step: &EffectStep,
    kind: FaceDownKind,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let n = resolve_step_amount(state, step, 1).max(0) as usize;
    let cards: Vec<ObjectId> = if matches!(step.subject, mtg_ir::Selector::None) {
        state
            .player(ctrl)
            .map(|p| {
                let take_n = n.min(p.library.len());
                p.library[p.library.len() - take_n..]
                    .iter()
                    .copied()
                    .rev()
                    .collect()
            })
            .unwrap_or_default()
    } else {
        resolve_selector_objects(state, &step.subject)
    };
    let ward_cost = if kind == FaceDownKind::Cloak {
        Some(mtg_ir::ManaCost {
            generic: 2,
            ..Default::default()
        })
    } else {
        None
    };
    for id in cards {
        let fd = FaceDownSpec {
            kind,
            ward_cost: ward_cost.clone(),
            known_to: smallvec::smallvec![ctrl],
            ..Default::default()
        };
        let opts = MoveRawOptions {
            controller: Some(ctrl),
            enter_face_down: Some(fd),
            ..Default::default()
        };
        let mv = zones::move_object(state, id, Zone::Battlefield, opts, None)?;
        if let MoveObjectOutcome::Moved(raw) = mv {
            append_step_binding(
                state,
                step.bind_as.as_deref(),
                raw.new_id.unwrap_or(raw.old_id),
                raw.lki,
            );
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_manifest_dread(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let cards: Vec<ObjectId> = state
        .player(ctrl)
        .map(|p| {
            let take_n = 2.min(p.library.len());
            p.library[p.library.len() - take_n..]
                .iter()
                .copied()
                .rev()
                .collect()
        })
        .unwrap_or_default();
    if cards.is_empty() {
        return Ok(StepExecOutcome::Done);
    }
    let manifest_id = cards[0];
    let fd = FaceDownSpec {
        kind: FaceDownKind::Manifest,
        known_to: smallvec::smallvec![ctrl],
        ..Default::default()
    };
    let opts = MoveRawOptions {
        controller: Some(ctrl),
        enter_face_down: Some(fd),
        ..Default::default()
    };
    let mv = zones::move_object(state, manifest_id, Zone::Battlefield, opts, None)?;
    let final_manifested = match mv {
        MoveObjectOutcome::Moved(raw) => {
            let fid = raw.new_id.unwrap_or(raw.old_id);
            append_step_binding(state, step.bind_as.as_deref(), fid, raw.lki);
            Some(fid)
        }
        _ => None,
    };
    let mut graveyard_ids = Vec::with_capacity(cards.len().saturating_sub(1));
    for &gid in &cards[1..] {
        let gmv = zones::move_object(state, gid, Zone::Graveyard, Default::default(), None)?;
        if let MoveObjectOutcome::Moved(raw) = gmv {
            graveyard_ids.push(raw.new_id.unwrap_or(raw.old_id));
        }
    }
    if final_manifested.is_some() || !graveyard_ids.is_empty() {
        emit_event(
            state,
            AtomicEventKind::ManifestDread {
                player: ctrl,
                manifested: final_manifested,
                graveyard_ids,
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_forage(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let food_sid = state.db().subtype_id("Food");
    let food_id = food_sid.and_then(|fsid| {
        let cache = state.evaluate_board();
        state
            .player(ctrl)?
            .battlefield
            .iter()
            .copied()
            .find(|&id| cache.get(id).is_some_and(|e| e.subtypes.contains(&fsid)))
    });
    if let Some(fid) = food_id {
        let _ = zones::move_object(state, fid, Zone::Graveyard, Default::default(), None)?;
        emit_event(state, AtomicEventKind::Forage { player: ctrl });
        return Ok(StepExecOutcome::Done);
    }
    let gy = state
        .player(ctrl)
        .map(|p| p.graveyard.clone())
        .unwrap_or_default();
    if gy.len() >= 3 {
        for &id in gy.iter().take(3) {
            let _ = zones::move_object(state, id, Zone::Exile, Default::default(), None)?;
        }
        emit_event(state, AtomicEventKind::Forage { player: ctrl });
        return Ok(StepExecOutcome::Done);
    }
    Ok(StepExecOutcome::Declined)
}

pub(crate) fn exec_collect_evidence(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = resolve_step_actor(state, step);
    let need = resolve_step_amount(state, step, 0).max(0) as u32;
    let gy = state
        .player(ctrl)
        .map(|p| p.graveyard.clone())
        .unwrap_or_default();
    let mut picked = Vec::new();
    let mut acc = 0u32;
    for id in gy {
        if acc >= need {
            break;
        }
        let mv = state
            .object(id)
            .and_then(|o| state.db().get(o.card_def))
            .and_then(|c| c.faces.first())
            .map_or(0, |f| f.characteristics.mana_cost.mana_value());
        picked.push(id);
        acc = acc.saturating_add(mv);
    }
    if acc < need {
        return Ok(StepExecOutcome::Declined);
    }
    for id in picked {
        let _ = zones::move_object(state, id, Zone::Exile, Default::default(), None)?;
    }
    emit_event(
        state,
        AtomicEventKind::CollectEvidence {
            player: ctrl,
            total_mv: acc,
        },
    );
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_become_monarch(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let pid = players
        .first()
        .copied()
        .unwrap_or_else(|| vm_controller(state));
    let previous = state.monarch();
    state.set_monarch(Some(pid));
    emit_event(
        state,
        AtomicEventKind::BecameMonarch {
            player: pid,
            previous,
        },
    );
    Ok(StepExecOutcome::Done)
}

#[allow(dead_code)]
fn _keep_used(
    _: ModePausePurpose,
    _: fn(&GameState, &mtg_ir::ObjectFilter, &EvalContext<'_>) -> Vec<ObjectId>,
) {
    let _ = find_objects;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use mtg_ir::{
        Amount, CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, EffectOp, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaColor, ManaCost, SupertypeMask,
    };
    use smallvec::smallvec;
    use std::sync::Arc;

    #[test]
    fn synth_exec_add_mana_ignores_redundant_step_amount() {
        let dummy = CardDef {
            oracle_id: "synth_dummy".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dummy".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Dummy".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
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
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![dummy],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 1).unwrap();
        state.set_priority_player(PlayerId(0));

        // 1. Multi-symbol Fixed {B}{B}{B} with redundant amount=3 adds 3 {B}, NOT 9!
        let ritual_prod = ManaProduction::Fixed {
            symbols: smallvec![ManaColor::B, ManaColor::B, ManaColor::B],
        };
        let ritual_step = EffectStep {
            op: EffectOp::AddMana {
                production: ritual_prod.clone(),
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
            subject: mtg_ir::Selector::None,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 3 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        exec_add_mana(
            &mut state,
            &ritual_step,
            &ritual_prod,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 3, 0, 0, 0]
        );

        // 2. Single-symbol Fixed {G} with amount=2 STILL adds 2 {G}.
        let two_g_prod = ManaProduction::Fixed {
            symbols: smallvec![ManaColor::G],
        };
        let two_g_step = EffectStep {
            op: EffectOp::AddMana {
                production: two_g_prod.clone(),
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
            subject: mtg_ir::Selector::None,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 2 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        exec_add_mana(
            &mut state,
            &two_g_step,
            &two_g_prod,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 3, 0, 2, 0]
        );
    }
}
