use mtg_ir::{
    CardTypeMask, DamagePreventionSpec, EachCreatureDealsDamage, EffectStep, ManaColor,
    PlayerFilter, PlayerRef, PreventReflectTarget, Selector,
};

use crate::action::TargetRef;
use crate::eval::evaluate_amount;
use crate::events::{AtomicEventKind, emit_event};
use crate::filter::{find_objects, matches_player_filter};
use crate::id::{ObjectId, PlayerId};
use crate::keywords::{has_deathtouch, has_infect, has_lifelink, has_trample, has_wither};
use crate::replace::{self, ReplacementOutcome, ReplacementResult};
use crate::state::{
    DirtyMask, EngineError, GameState, ObjectFlags, PendingStepContinuation, ShieldRemaining,
};
use crate::zones;

use super::vm::{
    OP_TAG_DAMAGE, OP_TAG_GAIN_LIFE, StepExecOutcome, rare_turn_state_mut,
    resolve_selector_objects, resolve_selector_targets, resolve_step_amount,
    resolve_target_players, set_waiting_on_replacement, vm_controller, vm_source, vm_stack_item,
    with_vm_eval,
};

pub(crate) fn apply_resolved_life_gain(
    state: &mut GameState,
    player: PlayerId,
    outcome: &ReplacementOutcome,
) {
    if outcome.mods.skipped {
        return;
    }
    let crate::state::ReplaceableEventKind::LifeGain { amount, .. } = outcome.event else {
        return;
    };
    if amount == 0 {
        return;
    }
    let (cant_gain, cant_change) = {
        let cache = state.evaluate_board();
        cache.player(player).map_or((false, false), |ep| {
            (ep.cant_gain_life, ep.life_cant_change)
        })
    };
    if cant_gain || cant_change {
        return;
    }
    if let Some(p) = state.player_mut(player) {
        p.life = p.life.saturating_add(amount as i32);
    }
    state.mark_dirty(DirtyMask::LIFE);
    emit_event(
        state,
        AtomicEventKind::LifeChange {
            player,
            delta: amount as i32,
        },
    );
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_effect_amount = amount as i32;
    }
}

pub(crate) fn apply_resolved_damage(
    state: &mut GameState,
    outcome: &ReplacementOutcome,
) -> Result<(), EngineError> {
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_excess_damage = 0;
    }
    let crate::state::ReplaceableEventKind::Damage {
        source,
        target,
        amount,
        is_combat,
        ..
    } = outcome.event
    else {
        return Ok(());
    };
    let mods = &outcome.mods;

    // Handle prevented-damage riders first (before skipping remaining damage).
    for (shield, prevented_amt) in &mods.prevented_riders {
        if *prevented_amt == 0 {
            continue;
        }
        if let Some(rider) = &shield.on_prevented {
            let recipient = if shield.on_prevented_to_damage_source_controller {
                source
                    .and_then(|s| state.object(s).map(|o| o.controller))
                    .map(|id| TargetRef::Player { id })
            } else {
                Some(TargetRef::Player {
                    id: shield.controller,
                })
            };
            match rider.kind {
                mtg_ir::OnPreventedKind::GainLifeEqual => {
                    if let Some(TargetRef::Player { id }) = recipient
                        && let ReplacementResult::Resolved(lg) =
                            replace::resolve_life_gain(state, id, *prevented_amt, source)?
                    {
                        apply_resolved_life_gain(state, id, &lg);
                    }
                }
                mtg_ir::OnPreventedKind::DealDamageEqual => {
                    if let Some(t) = recipient
                        && let ReplacementResult::Resolved(dmg) =
                            replace::resolve_damage(state, source, t, *prevented_amt, false, false)?
                    {
                        apply_resolved_damage(state, &dmg)?;
                    }
                }
                mtg_ir::OnPreventedKind::PutCountersEqual => {
                    if let Some(cid) = rider.counter
                        && let TargetRef::Object { id } = target
                    {
                        state.add_counter(id, cid, *prevented_amt);
                    }
                }
                mtg_ir::OnPreventedKind::DrawEqual => {
                    for _ in 0..*prevented_amt {
                        let _ = zones::draw_one_raw(state, shield.controller)?;
                    }
                }
                mtg_ir::OnPreventedKind::DrawOne => {
                    let _ = zones::draw_one_raw(state, shield.controller)?;
                }
            }
        }
        if let Some(reflect) = shield.on_prevented_creature
            && source.is_some_and(|s| {
                state
                    .evaluate_board()
                    .get(s)
                    .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            })
        {
            let tgt = match reflect {
                PreventReflectTarget::SourceCreature => source.map(|id| TargetRef::Object { id }),
                PreventReflectTarget::SourceController => source
                    .and_then(|s| state.object(s).map(|o| o.controller))
                    .map(|id| TargetRef::Player { id }),
            };
            if let Some(t) = tgt
                && let ReplacementResult::Resolved(dmg) =
                    replace::resolve_damage(state, source, t, *prevented_amt, false, false)?
            {
                apply_resolved_damage(state, &dmg)?;
            }
        }
        if let Some(reflect) = shield.on_prevented_noncreature
            && source.is_some_and(|s| {
                state
                    .evaluate_board()
                    .get(s)
                    .is_some_and(|e| !e.types.contains(CardTypeMask::CREATURE))
            })
        {
            let tgt = match reflect {
                PreventReflectTarget::SourceCreature => source.map(|id| TargetRef::Object { id }),
                PreventReflectTarget::SourceController => source
                    .and_then(|s| state.object(s).map(|o| o.controller))
                    .map(|id| TargetRef::Player { id }),
            };
            if let Some(t) = tgt
                && let ReplacementResult::Resolved(dmg) =
                    replace::resolve_damage(state, source, t, *prevented_amt, false, false)?
            {
                apply_resolved_damage(state, &dmg)?;
            }
        }
    }

    // Apply redirected damage packets recursively.
    for &(redir_target, redir_amt) in &mods.redirected_damage {
        if redir_amt > 0 {
            apply_single_damage_packet(state, source, redir_target, redir_amt, is_combat, mods)?;
        }
    }

    if mods.skipped || amount == 0 {
        return Ok(());
    }

    apply_single_damage_packet(state, source, target, amount, is_combat, mods)
}

fn apply_single_damage_packet(
    state: &mut GameState,
    source: Option<ObjectId>,
    target: TargetRef,
    amount: u32,
    is_combat: bool,
    mods: &crate::state::ReplacementAccumulatedMods,
) -> Result<(), EngineError> {
    if amount == 0 {
        return Ok(());
    }
    let (src_ctrl, src_lifelink, src_deathtouch, src_infect, src_wither, src_commander) = {
        let cache = state.evaluate_board();
        let ctrl = source
            .and_then(|s| {
                cache
                    .get(s)
                    .map(|e| e.controller)
                    .or_else(|| state.object(s).map(|o| o.controller))
            })
            .unwrap_or_else(|| vm_controller(state));
        let ll = source.is_some_and(|s| has_lifelink(state, s));
        let dt = source.is_some_and(|s| has_deathtouch(state, s));
        let inf = source.is_some_and(|s| has_infect(state, s));
        let wit = source.is_some_and(|s| has_wither(state, s));
        let cmd = source.is_some_and(|s| state.object(s).is_some_and(|o| o.is_commander()));
        (ctrl, ll, dt, inf, wit, cmd)
    };

    let mut total_dealt: u32 = 0;

    match target {
        TargetRef::Player { id: pid } => {
            let (as_infect, life_cant_change) = {
                let cache = state.evaluate_board();
                cache.player(pid).map_or((false, false), |ep| {
                    (ep.damage_to_you_as_infect, ep.life_cant_change)
                })
            };
            if src_infect || as_infect {
                if let Some(p) = state.player_mut(pid) {
                    p.poison = p.poison.saturating_add(amount as u16);
                }
                state.mark_dirty(DirtyMask::PLAYER_STATE);
            } else if !life_cant_change {
                let mut actual_loss = amount as i32;
                if let Some(p) = state.player_mut(pid) {
                    let next = p.life.saturating_sub(actual_loss);
                    actual_loss = p.life.saturating_sub(next);
                    p.life = next;
                }
                if actual_loss > 0 {
                    state.mark_dirty(DirtyMask::LIFE);
                    emit_event(
                        state,
                        AtomicEventKind::LifeChange {
                            player: pid,
                            delta: -actual_loss,
                        },
                    );
                }
            }
            if is_combat
                && src_commander
                && let Some(src_id) = source
            {
                state.add_commander_damage(pid, src_id, amount as i32);
            }
            total_dealt = amount;
        }
        TargetRef::Object { id: oid } => {
            let (types, toughness, all_wither, cur_marked) = {
                let cache = state.evaluate_board();
                let ev = cache.get(oid);
                let types = ev.map_or(CardTypeMask::empty(), |e| e.types);
                let t = ev.and_then(|e| e.pt.map(|p| p.toughness)).unwrap_or(0);
                let aw = ev.is_some_and(|e| {
                    cache
                        .player(e.controller)
                        .is_some_and(|p| p.all_damage_has_wither)
                });
                let marked = state.object(oid).map_or(0, |o| o.damage_marked);
                (types, t, aw, marked)
            };

            if mods.damage_as_minus_counters || src_wither || src_infect || all_wither {
                if let Some(minus_cid) = state.db().counter_id("-1/-1") {
                    let prev = state.counter_count(oid, minus_cid);
                    state.add_counter(oid, minus_cid, amount);
                    let next = state.counter_count(oid, minus_cid);
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: oid,
                            counter: minus_cid,
                            previous_count: prev,
                            new_count: next,
                            actor: Some(src_ctrl),
                        },
                    );
                }
            } else if let Some(cid) = mods.damage_to_self_as_counter {
                let prev = state.counter_count(oid, cid);
                state.add_counter(oid, cid, amount);
                let next = state.counter_count(oid, cid);
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object: oid,
                        counter: cid,
                        previous_count: prev,
                        new_count: next,
                        actor: Some(src_ctrl),
                    },
                );
            } else {
                if types.contains(CardTypeMask::CREATURE) {
                    let lethal = if src_deathtouch {
                        1
                    } else {
                        (toughness - cur_marked).max(0) as u32
                    };
                    let excess = amount.saturating_sub(lethal) as i32;
                    if let Some(vm) = state.resolution_vms_mut().last_mut() {
                        vm.last_excess_damage = excess;
                    }
                    if let Some(obj) = state.object_mut(oid) {
                        obj.damage_marked = obj.damage_marked.saturating_add(amount as i32);
                        if src_deathtouch {
                            obj.deathtouch_damage_marked = true;
                        }
                        obj.flags.insert(ObjectFlags::DEALT_DAMAGE_THIS_TURN);
                    }
                    state.mark_dirty(DirtyMask::COMBAT);
                }
                if types.contains(CardTypeMask::PLANESWALKER)
                    && let Some(loy_cid) = state.db().counter_id("loyalty")
                {
                    let prev = state.counter_count(oid, loy_cid);
                    state.remove_counter(oid, loy_cid, amount);
                    let next = state.counter_count(oid, loy_cid);
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: oid,
                            counter: loy_cid,
                            previous_count: prev,
                            new_count: next,
                            actor: Some(src_ctrl),
                        },
                    );
                }
                if types.contains(CardTypeMask::BATTLE)
                    && let Some(def_cid) = state.db().counter_id("defense")
                {
                    let prev = state.counter_count(oid, def_cid);
                    state.remove_counter(oid, def_cid, amount);
                    let next = state.counter_count(oid, def_cid);
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: oid,
                            counter: def_cid,
                            previous_count: prev,
                            new_count: next,
                            actor: Some(src_ctrl),
                        },
                    );
                }
            }
            if let Some(src_id) = source
                && let Some(rare) = state.rare_or_default(oid)
                && !rare.dealt_damage_exile_sources.contains(&src_id)
            {
                rare.dealt_damage_exile_sources.push(src_id);
            }
            total_dealt = amount;
        }
        TargetRef::Stack { .. } => {}
    }

    if total_dealt > 0 {
        if let Some(vm) = state.resolution_vms_mut().last_mut() {
            vm.last_effect_amount = total_dealt as i32;
        }
        emit_event(
            state,
            AtomicEventKind::DamageDealt {
                source,
                source_controller: src_ctrl,
                target,
                amount: total_dealt,
                is_combat,
            },
        );
        if src_lifelink
            && let ReplacementResult::Resolved(lg) =
                replace::resolve_life_gain(state, src_ctrl, total_dealt, source)?
        {
            apply_resolved_life_gain(state, src_ctrl, &lg);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_damage(
    state: &mut GameState,
    step: &EffectStep,
    source_sel: Option<&Selector>,
    also_players: Option<&PlayerFilter>,
    divided: bool,
    excess_to_controller: bool,
    excess_to_controller_if_trample: bool,
    or_planeswalker_controlled_by: Option<&PlayerRef>,
    damage_attacked_target_by_self: bool,
    each_creature_deals: Option<&EachCreatureDealsDamage>,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    let dmg_source = match source_sel {
        Some(sel) => resolve_selector_objects(state, sel).into_iter().next(),
        None => vm_source(state),
    };

    if let Some(ecd) = each_creature_deals {
        let creatures: Vec<(ObjectId, PlayerId, i32)> = with_vm_eval(state, |ctx, cache| {
            find_objects(state, &ecd.filter, ctx)
                .into_iter()
                .map(|id| {
                    let c = cache.get(id).map_or(ctrl, |e| e.controller);
                    let amt = match ecd.amount.as_deref() {
                        Some(a) => evaluate_amount(state, a, ctx).max(0),
                        None => cache
                            .get(id)
                            .and_then(|e| e.pt.map(|p| p.power))
                            .unwrap_or(0)
                            .max(0),
                    };
                    (id, c, amt)
                })
                .collect()
        });
        let packets: Vec<(Option<ObjectId>, TargetRef, u32)> = creatures
            .into_iter()
            .filter(|&(_, _, amt)| amt > 0)
            .map(|(cid, c_ctrl, amt)| {
                let tgt = if ecd.to_controller {
                    TargetRef::Player { id: c_ctrl }
                } else {
                    TargetRef::Player { id: ctrl }
                };
                (Some(cid), tgt, amt as u32)
            })
            .collect();
        return run_damage_packets(state, packets, false, None, None);
    }

    let base_amt = resolve_step_amount(state, step, 0).max(0) as u32;
    let mut packets: Vec<(Option<ObjectId>, TargetRef, u32)> = Vec::new();

    if divided {
        let (divs, vm_targets) = state
            .resolution_vms()
            .last()
            .map(|v| (v.divisions.clone(), v.targets.clone()))
            .unwrap_or_default();
        for (t, a) in divs {
            if a > 0 && vm_targets.contains(&t) {
                packets.push((dmg_source, t, a));
            }
        }
    } else {
        let mut targets = resolve_selector_targets(state, &step.subject);
        if targets.is_empty()
            && damage_attacked_target_by_self
            && let Some(src) = dmg_source
            && let Some(att) = state.combat().attackers.iter().find(|a| a.attacker == src)
        {
            targets.push(match att.target {
                crate::action::AttackTarget::Player { id } => TargetRef::Player { id },
                crate::action::AttackTarget::Planeswalker { id }
                | crate::action::AttackTarget::Battle { id } => TargetRef::Object { id },
            });
        }
        if let Some(pf) = also_players {
            let extra_players: Vec<TargetRef> = with_vm_eval(state, |ctx, _| {
                state
                    .players()
                    .iter()
                    .filter(|p| !p.lost && matches_player_filter(state, p.id, pf, ctx))
                    .map(|p| TargetRef::Player { id: p.id })
                    .collect()
            });
            for pt in extra_players {
                if !targets.contains(&pt) {
                    targets.push(pt);
                }
            }
        }
        let _ = or_planeswalker_controlled_by;
        for t in targets {
            if base_amt > 0 {
                packets.push((dmg_source, t, base_amt));
            }
        }
    }

    let src_has_trample = dmg_source.is_some_and(|s| has_trample(state, s));
    let do_excess = excess_to_controller || (excess_to_controller_if_trample && src_has_trample);
    run_damage_packets(state, packets, do_excess, None, None)
}

pub(crate) fn apply_fight_excess_mana(
    state: &mut GameState,
    excess_mana: Option<ManaColor>,
) -> Result<(), EngineError> {
    if let Some(mc) = excess_mana
        && let Some(ex) = state
            .resolution_vms()
            .last()
            .map(|v| v.last_excess_damage)
            .filter(|&e| e > 0)
    {
        let ctrl = vm_controller(state);
        let syms: smallvec::SmallVec<[ManaColor; 4]> = (0..ex).map(|_| mc).collect();
        let _ = crate::mana::add_mana_from_production(
            state,
            ctrl,
            vm_source(state),
            &mtg_ir::ManaProduction::Fixed { symbols: syms },
            None,
            None,
            false,
            None,
            None,
            1,
        )?;
    }
    Ok(())
}

pub(crate) fn run_damage_packets(
    state: &mut GameState,
    packets: Vec<(Option<ObjectId>, TargetRef, u32)>,
    do_excess: bool,
    fight_return: Option<(ObjectId, ObjectId, u32)>,
    mut fight_excess_mana: Option<ManaColor>,
) -> Result<StepExecOutcome, EngineError> {
    for (idx, &(src, t, amt)) in packets.iter().enumerate() {
        match replace::resolve_damage(state, src, t, amt, false, false)? {
            ReplacementResult::Paused { prompt_id, chooser } => {
                if let Some(vm) = state.resolution_vms_mut().last_mut() {
                    vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                        remaining_damage: packets[idx + 1..].to_vec(),
                        damage_do_excess: do_excess,
                        fight_return_damage: fight_return,
                        fight_excess_mana,
                        ..Default::default()
                    }));
                }
                set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_DAMAGE);
                return Ok(StepExecOutcome::Paused { prompt_id, chooser });
            }
            ReplacementResult::Resolved(out) => {
                apply_resolved_damage(state, &out)?;
                apply_fight_excess_mana(state, fight_excess_mana.take())?;
                if do_excess
                    && let TargetRef::Object { id: oid } = t
                    && let Some(excess) = state
                        .resolution_vms()
                        .last()
                        .map(|v| v.last_excess_damage)
                        .filter(|&e| e > 0)
                    && let Some(def_ctrl) = state.object(oid).map(|o| o.controller)
                {
                    match replace::resolve_damage(
                        state,
                        src,
                        TargetRef::Player { id: def_ctrl },
                        excess as u32,
                        false,
                        false,
                    )? {
                        ReplacementResult::Paused { prompt_id, chooser } => {
                            if let Some(vm) = state.resolution_vms_mut().last_mut() {
                                vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                                    remaining_damage: packets[idx + 1..].to_vec(),
                                    damage_do_excess: do_excess,
                                    fight_return_damage: fight_return,
                                    fight_excess_mana: None,
                                    ..Default::default()
                                }));
                            }
                            set_waiting_on_replacement(
                                state,
                                prompt_id,
                                Vec::new(),
                                0,
                                OP_TAG_DAMAGE,
                            );
                            return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                        }
                        ReplacementResult::Resolved(ex_out) => {
                            apply_resolved_damage(state, &ex_out)?;
                        }
                    }
                }
            }
        }
    }
    if let Some((b_id, a_id, b_pow)) = fight_return
        && b_pow > 0
    {
        return run_damage_packets(
            state,
            vec![(Some(b_id), TargetRef::Object { id: a_id }, b_pow)],
            false,
            None,
            None,
        );
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_fight(
    state: &mut GameState,
    step: &EffectStep,
    with_sel: &Selector,
    excess_damage_to_mana: Option<ManaColor>,
) -> Result<StepExecOutcome, EngineError> {
    let a_opt = resolve_selector_objects(state, &step.subject)
        .into_iter()
        .next();
    let b_opt = resolve_selector_objects(state, with_sel)
        .into_iter()
        .next()
        .or_else(|| {
            resolve_selector_objects(state, &step.subject)
                .get(1)
                .copied()
        });
    let (Some(a), Some(b)) = (a_opt, b_opt) else {
        return Ok(StepExecOutcome::Done);
    };
    let (a_ok, b_ok, a_pow, b_pow) = {
        let cache = state.evaluate_board();
        let ae = cache.get(a);
        let be = cache.get(b);
        let a_ok = ae.is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            && state
                .object(a)
                .is_some_and(|o| o.zone == mtg_ir::Zone::Battlefield);
        let b_ok = be.is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
            && state
                .object(b)
                .is_some_and(|o| o.zone == mtg_ir::Zone::Battlefield);
        let ap = ae.and_then(|e| e.pt.map(|p| p.power)).unwrap_or(0).max(0) as u32;
        let bp = be.and_then(|e| e.pt.map(|p| p.power)).unwrap_or(0).max(0) as u32;
        (a_ok, b_ok, ap, bp)
    };
    if !a_ok || !b_ok {
        return Ok(StepExecOutcome::Done);
    }
    let fight_ret = (b_pow > 0).then_some((b, a, b_pow));
    if a_pow > 0 {
        run_damage_packets(
            state,
            vec![(Some(a), TargetRef::Object { id: b }, a_pow)],
            false,
            fight_ret,
            excess_damage_to_mana,
        )
    } else if let Some((b_id, a_id, bp)) = fight_ret {
        run_damage_packets(
            state,
            vec![(Some(b_id), TargetRef::Object { id: a_id }, bp)],
            false,
            None,
            None,
        )
    } else {
        Ok(StepExecOutcome::Done)
    }
}

pub(crate) fn run_gain_life_players(
    state: &mut GameState,
    players: &[PlayerId],
    amt: i32,
    set_life_total: bool,
) -> Result<StepExecOutcome, EngineError> {
    let src = vm_source(state);
    for (idx, &pid) in players.iter().enumerate() {
        if set_life_total {
            let cur = state.player(pid).map_or(0, |p| p.life);
            let delta = amt.saturating_sub(cur);
            if delta > 0 {
                match replace::resolve_life_gain(state, pid, delta as u32, src)? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        if let Some(vm) = state.resolution_vms_mut().last_mut() {
                            vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                                remaining_players: players[idx + 1..].iter().copied().collect(),
                                ..Default::default()
                            }));
                        }
                        set_waiting_on_replacement(
                            state,
                            prompt_id,
                            Vec::new(),
                            0,
                            OP_TAG_GAIN_LIFE,
                        );
                        return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => apply_resolved_life_gain(state, pid, &out),
                }
            } else if delta < 0 {
                let loss = (-delta) as u32;
                let cant_change = state
                    .evaluate_board()
                    .player(pid)
                    .is_some_and(|ep| ep.life_cant_change);
                if !cant_change {
                    if let Some(p) = state.player_mut(pid) {
                        p.life = p.life.saturating_sub(loss as i32);
                    }
                    state.mark_dirty(DirtyMask::LIFE);
                    emit_event(
                        state,
                        AtomicEventKind::LifeChange {
                            player: pid,
                            delta: -(loss as i32),
                        },
                    );
                }
            }
        } else if amt > 0 {
            match replace::resolve_life_gain(state, pid, amt as u32, src)? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    if let Some(vm) = state.resolution_vms_mut().last_mut() {
                        vm.pending_continuation = Some(Box::new(PendingStepContinuation {
                            remaining_players: players[idx + 1..].iter().copied().collect(),
                            ..Default::default()
                        }));
                    }
                    set_waiting_on_replacement(state, prompt_id, Vec::new(), 0, OP_TAG_GAIN_LIFE);
                    return Ok(StepExecOutcome::Paused { prompt_id, chooser });
                }
                ReplacementResult::Resolved(out) => apply_resolved_life_gain(state, pid, &out),
            }
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_gain_life(
    state: &mut GameState,
    step: &EffectStep,
    set_life_total: bool,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 0);
    run_gain_life_players(state, &players, amt, set_life_total)
}

pub(crate) fn exec_lose_life(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 0).max(0) as u32;
    if amt == 0 {
        return Ok(StepExecOutcome::Done);
    }
    for pid in players {
        let cant_change = state
            .evaluate_board()
            .player(pid)
            .is_some_and(|ep| ep.life_cant_change);
        if cant_change {
            continue;
        }
        if let Some(p) = state.player_mut(pid) {
            p.life = p.life.saturating_sub(amt as i32);
        }
        state.mark_dirty(DirtyMask::LIFE);
        emit_event(
            state,
            AtomicEventKind::LifeChange {
                player: pid,
                delta: -(amt as i32),
            },
        );
    }
    if let Some(vm) = state.resolution_vms_mut().last_mut() {
        vm.last_effect_amount = amt as i32;
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_prevent_damage(
    state: &mut GameState,
    step: &EffectStep,
    spec: &DamagePreventionSpec,
) -> Result<StepExecOutcome, EngineError> {
    let ctrl = vm_controller(state);
    let amt = resolve_step_amount(state, step, 0).max(0) as u32;
    let targets = resolve_selector_targets(state, &step.subject);
    let sid = vm_stack_item(state);
    let _ = replace::create_prevention_shield(state, ctrl, spec, &targets, amt, sid);
    if spec.scope == mtg_ir::PreventScope::Any
        && spec.mode == mtg_ir::PreventMode::All
        && !spec.combat_only
        && spec.from_source.is_none()
    {
        // Global Fog-like shields still need explicit expiry at EOT (handled by `clear_eot_prevention_state`).
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_regenerate(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let objs = resolve_selector_objects(state, &step.subject);
    for id in objs {
        let rts = rare_turn_state_mut(state);
        if rts.cant_be_regenerated_this_turn.contains(&id) {
            continue;
        }
        if let Some((_, count)) = rts
            .regeneration_shields
            .iter_mut()
            .find(|(oid, _)| *oid == id)
        {
            *count = count.saturating_add(1);
        } else {
            rts.regeneration_shields.push((id, 1));
        }
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_cant_regenerate(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let objs = resolve_selector_objects(state, &step.subject);
    let rts = rare_turn_state_mut(state);
    for id in objs {
        if !rts.cant_be_regenerated_this_turn.contains(&id) {
            rts.cant_be_regenerated_this_turn.push(id);
        }
        rts.regeneration_shields.retain(|(oid, _)| *oid != id);
    }
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_exile_instead_of_dying(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let objs = resolve_selector_objects(state, &step.subject);
    for id in objs {
        if let Some(r) = state.rare_or_default(id) {
            r.exile_instead_of_dying_this_turn = true;
        }
    }
    state.mark_dirty(DirtyMask::DESIGNATIONS);
    Ok(StepExecOutcome::Done)
}

pub(crate) fn exec_gain_energy(
    state: &mut GameState,
    step: &EffectStep,
) -> Result<StepExecOutcome, EngineError> {
    let players = resolve_target_players(state, step);
    let amt = resolve_step_amount(state, step, 1).max(0) as u16;
    if amt == 0 {
        return Ok(StepExecOutcome::Done);
    }
    for pid in players {
        if let Some(p) = state.player_mut(pid) {
            p.energy = p.energy.saturating_add(amt);
        }
        state.mark_dirty(DirtyMask::PLAYER_STATE);
        emit_event(
            state,
            AtomicEventKind::EnergyGained {
                player: pid,
                amount: amt,
            },
        );
    }
    Ok(StepExecOutcome::Done)
}

#[allow(dead_code)]
fn _keep_imports(_: ShieldRemaining, _: PlayerRef) {}
