use std::sync::Arc;

use mtg_ir::{
    AbilityDef, CardLayout, CardTypeMask, ColorMask, CostDef, FaceDef, LoyaltySign, ManaColor,
    ManaCost, ManaPip, ModalDef, SpellAbility, TargetSpec, Zone,
};
use smallvec::SmallVec;

use crate::action::{
    Action, ChoiceAnswer, ChoiceRequest, CostAidKind, DecisionState, MacroAction,
    SpecialActionKind, TargetRef,
};
use crate::auto_tap::{
    AutoTapConstraints, AutoTapPolicy, execute_auto_tap_plan, plan_auto_tap_cached,
    plan_auto_tap_with_constraints_cached,
};
use crate::costs::{
    CostAidCaps, CostListSelfContext, CostStepOutcome, amount_uses_x,
    apply_cost_aid_with_constraints, auto_tap_constraints_for_costs_cached,
    can_pay_costs_and_mana_with_self_ctx_cached, can_pay_non_mana_costs_cached,
    cant_sacrifice_object_cached, compute_effective_ability_cost_cached,
    compute_effective_spell_cost_with_modes_cached, convoke_can_reduce_mana, cost_uses_x,
    delve_reserved_gy_cards_cached, execute_non_mana_cost_step,
    max_x_for_cost_list_with_ctx_cached, non_mana_cost_candidates_in_list_with_ctx_cached,
    resume_cost_prompt, resume_paused_non_mana_cost_effect_step,
    resume_paused_non_mana_cost_zone_step,
};
use crate::eval::{BoardView, EvalContext, evaluate_amount, evaluate_condition};
use crate::events::{AtomicEventKind, emit_event};
use crate::faces::{
    active_face_view, can_turn_face_up, is_room_card, turn_face_up, unlock_room_door,
};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::legality::{
    bestow_target_spec, build_mana_spend_context_cached, can_activate_ability,
    can_cast_spell_option, can_play_land, enumerate_cast_options_cached, find_keyword,
    hand_foretell_cost, resolve_activated_abilities_cached, spell_can_be_paid_with_x_cached,
};
use crate::mana::{
    ManaActivationOutcome, ManaSpendContext, ManaSpendContextKind, activate_mana_ability,
    can_activate_mana_candidate_now_cached, can_pay_mana_from_pool_cached,
    can_pay_mana_from_pool_with_life_reserve_cached, colors_from_mask, deduct_mana_for_context,
    enumerate_mana_abilities_cached,
};
use crate::replace::ReplacementOutcome;
use crate::state::{
    AltCastKind, CastKind, CastOption, CastStep, CastingPromptKind, CastingState,
    CostPaymentPromptState, DirtyMask, EngineError, FaceDownKind, FaceDownSpec, GameState,
    LayerCache, LkiSnapshot, ObjectFlags, PendingCostPaymentPlan, PendingNonManaCostStep,
    PendingPromptContext, PreCastExileState, PromptOwner, StackChoices, StackEntry, StackEntryKind,
};
use crate::targets::{
    EffectiveModalBounds, TargetSourceInfo, build_ability_target_source_info_cached,
    build_modal_target_specs, build_spell_target_source_info_cached,
    can_finish_spec_flagbearer_ok_cached, effective_modal_bounds_cached,
    effective_target_bounds_cached, filter_casting_target_choices_with_modal_cached,
    has_legal_target_assignment_with_modal_cached, is_target_legal_for_spec_cached,
    legal_next_modes_and_done_cached, legal_targets_for_spec_cached,
    partial_targets_can_complete_with_modal_cached,
};
use crate::triggers::flush_triggers;
use crate::zones::{MoveObjectOutcome, MoveRawOptions, move_object, move_object_raw};

fn find_spell_ability(
    state: &GameState,
    card_def: mtg_ir::CardDefId,
    face_idx: u8,
    cleaved: bool,
    gift_promised: bool,
) -> Option<&SpellAbility> {
    let card = state.db().get(card_def)?;
    let face = card.faces.get(usize::from(face_idx))?;
    if cleaved && let Some(c) = face.cleaved_spell_spec.as_deref() {
        return Some(c);
    }
    if gift_promised && let Some(g) = face.gift_promised_spell_spec.as_deref() {
        return Some(g);
    }
    face.abilities.iter().find_map(|ab| match ab {
        AbilityDef::Spell(s) => Some(s),
        _ => None,
    })
}

fn find_modal_def_cached<'a>(
    state: &'a GameState,
    cs: &CastingState,
    cache: &'a LayerCache,
) -> Option<&'a ModalDef> {
    match cs.kind {
        CastKind::Spell => {
            let obj = state.object(cs.object)?;
            let eff_def = state
                .rare(cs.object)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            find_spell_ability(state, eff_def, cs.face_idx, cs.cleaved, cs.gift_promised)
                .and_then(|sa| sa.modal.as_ref())
        }
        CastKind::ActivatedAbility { ability_index, .. } => {
            resolve_activated_abilities_cached(state, cs.object, cache)
                .into_iter()
                .find(|r| r.ability_index == ability_index)
                .and_then(|r| r.ability.modal.as_ref())
        }
        CastKind::TriggeredAbility { stack_item } => state
            .stack_trigger_meta(stack_item)
            .and_then(|m| m.modal.as_ref()),
    }
}

fn base_target_specs_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Vec<TargetSpec> {
    let mut specs = Vec::new();
    match cs.kind {
        CastKind::Spell => {
            if cs.alt_kind == AltCastKind::Bestow {
                specs.push(bestow_target_spec());
            }
            if let Some(obj) = state.object(cs.object) {
                let eff_def = state
                    .rare(cs.object)
                    .and_then(|r| r.copiable.as_deref())
                    .map_or(obj.card_def, |c| c.card_def);
                if let Some(sa) =
                    find_spell_ability(state, eff_def, cs.face_idx, cs.cleaved, cs.gift_promised)
                {
                    specs.extend(sa.targets.iter().cloned());
                }
            }
        }
        CastKind::ActivatedAbility { ability_index, .. } => {
            if let Some(res) = resolve_activated_abilities_cached(state, cs.object, cache)
                .into_iter()
                .find(|r| r.ability_index == ability_index)
            {
                specs.extend(res.ability.targets.iter().cloned());
            }
        }
        CastKind::TriggeredAbility { stack_item } => {
            if let Some(meta) = state.stack_trigger_meta(stack_item) {
                specs.extend(meta.targets.iter().cloned());
            }
        }
    }
    specs
}

fn active_target_specs_with_modal_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> (Vec<TargetSpec>, Option<SmallVec<[u16; 8]>>) {
    let base = base_target_specs_cached(state, cs, cache);
    if let Some(modal) = find_modal_def_cached(state, cs, cache) {
        build_modal_target_specs(&base, modal, &cs.chosen_modes)
    } else {
        (base, None)
    }
}

fn active_target_specs_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Vec<TargetSpec> {
    active_target_specs_with_modal_cached(state, cs, cache).0
}

fn build_casting_target_info_cached<'a>(
    state: &'a GameState,
    cs: &'a CastingState,
    spec_slice_end: usize,
    cache: &LayerCache,
) -> TargetSourceInfo<'a> {
    let slice = &cs.targets_by_spec[..spec_slice_end.min(cs.targets_by_spec.len())];
    match cs.kind {
        CastKind::Spell => build_spell_target_source_info_cached(
            state, cs.caster, cs.object, cs.x_value, slice, cache,
        ),
        CastKind::ActivatedAbility { .. } => build_ability_target_source_info_cached(
            state,
            cs.caster,
            cs.object,
            None,
            cs.x_value,
            slice,
            &[],
            cache,
        ),
        CastKind::TriggeredAbility { stack_item } => {
            let lki = state.stack_entries().iter().find_map(|e| {
                if e.id == stack_item
                    && let StackEntryKind::Triggered { lki, .. } = &e.kind
                {
                    Some(lki.as_ref())
                } else {
                    None
                }
            });
            let meta = state.stack_trigger_meta(stack_item);
            let bindings = meta.map_or(&[][..], |m| m.bindings.as_slice());
            let mut info = build_ability_target_source_info_cached(
                state, cs.caster, cs.object, lki, cs.x_value, slice, bindings, cache,
            );
            if let Some(meta) = meta {
                info.event_object = meta.event_object;
                info.event_lki = meta.event_lki.as_deref();
                info.event_player = meta.event_player;
                info.defending_player = meta.defending_player;
            }
            info
        }
    }
}

fn build_casting_spend_ctx_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> ManaSpendContext {
    match cs.kind {
        CastKind::Spell => {
            let opt = reconstruct_cast_option_cached(state, cs, cache);
            build_mana_spend_context_cached(state, cs.caster, cs.object, &opt, cache)
        }
        CastKind::ActivatedAbility { ability_index, .. } => {
            let resolved = resolve_activated_abilities_cached(state, cs.object, cache)
                .into_iter()
                .find(|r| r.ability_index == ability_index);
            let act = resolved.as_ref().map(|r| r.ability);
            ManaSpendContext {
                player: cs.caster,
                kind: ManaSpendContextKind::ActivatedAbility,
                source_object: Some(cs.object),
                face_idx: cs.face_idx,
                from_zone: cs.from_zone,
                is_commander: crate::commander::is_commander_object(state, cs.object),
                has_x_in_cost: act.is_some_and(|a| {
                    a.cost
                        .iter()
                        .any(|c| matches!(c, CostDef::Mana { cost, .. } if cost.x_count > 0))
                }),
                is_fused: false,
                is_prototyped: false,
                is_bestowed: false,
                is_face_down: false,
                alt_cast_spend_any_color: false,
                mana_as_any_color_ability: resolved.is_some_and(|r| r.mana_as_any_color),
                x_spend_colors: act.map_or(ColorMask::empty(), |a| a.x_spend_colors),
                x_colored_only: act.is_some_and(|a| a.x_colored_only),
                x_max_one_per_color: act.is_some_and(|a| a.x_max_one_per_color),
                source_restriction: None,
            }
        }
        CastKind::TriggeredAbility { .. } => ManaSpendContext {
            player: cs.caster,
            kind: ManaSpendContextKind::TriggeredAbility,
            source_object: Some(cs.object),
            face_idx: cs.face_idx,
            from_zone: cs.from_zone,
            is_commander: false,
            has_x_in_cost: false,
            is_fused: false,
            is_prototyped: false,
            is_bestowed: false,
            is_face_down: false,
            alt_cast_spend_any_color: false,
            mana_as_any_color_ability: false,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
            source_restriction: None,
        },
    }
}

fn reconstruct_cast_option_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> CastOption {
    if let Some(mut saved) = cs.cast_option.as_deref().cloned() {
        saved.optional_cost_indices = cs.optional_costs.clone();
        return saved;
    }
    let opts = enumerate_cast_options_cached(state, cs.caster, cs.object, cache);
    if let Some(found) = opts.into_iter().find(|o| o.option_id == cs.option_id) {
        return found;
    }
    CastOption {
        option_id: cs.option_id,
        face_idx: cs.face_idx,
        from_zone: cs.from_zone,
        alt_kind: cs.alt_kind,
        kicker_indices: cs.kicked_indices.clone(),
        is_kicked: cs.is_kicked,
        gift_promised: cs.gift_promised,
        fused: cs.fused,
        cleaved: cs.cleaved,
        spree_required: false,
        optional_cost_indices: cs.optional_costs.clone(),
        mana_cost_override: None,
        extra_costs: Vec::new(),
        permission_source: None,
    }
}

fn casting_breakdown_cached(
    state: &GameState,
    cs: &CastingState,
    x_value: Option<u32>,
    cache: &LayerCache,
) -> Option<crate::costs::EffectiveCostBreakdown> {
    match cs.kind {
        CastKind::Spell => {
            let opt = reconstruct_cast_option_cached(state, cs, cache);
            compute_effective_spell_cost_with_modes_cached(
                state,
                cs.caster,
                cs.object,
                &opt,
                &cs.chosen_modes,
                x_value,
                &cs.optional_costs,
                &cs.targets,
                cache,
            )
            .ok()
        }
        CastKind::ActivatedAbility { ability_index, .. } => compute_effective_ability_cost_cached(
            state,
            cs.caster,
            cs.object,
            ability_index,
            x_value,
            &cs.targets,
            cache,
        )
        .ok(),
        CastKind::TriggeredAbility { .. } => None,
    }
}

fn sync_spend_ctx_with_breakdown(
    mut spend_ctx: ManaSpendContext,
    breakdown: &crate::costs::EffectiveCostBreakdown,
) -> ManaSpendContext {
    spend_ctx.x_spend_colors |= breakdown.x_spend_colors;
    spend_ctx.x_colored_only |= breakdown.x_colored_only;
    spend_ctx.x_max_one_per_color |= breakdown.x_max_one_per_color;
    if breakdown.source_restriction.is_some() {
        spend_ctx.source_restriction = breakdown.source_restriction;
    }
    if breakdown.base_mana.x_count > 0 || breakdown.locked_mana.x_count > 0 {
        spend_ctx.has_x_in_cost = true;
    }
    spend_ctx
}

fn casting_cost_aid_caps(
    state: &GameState,
    cs: &CastingState,
    breakdown: Option<&crate::costs::EffectiveCostBreakdown>,
    cache: &LayerCache,
) -> CostAidCaps {
    let emerge_unused = cs
        .payment_plan
        .as_ref()
        .is_none_or(|p| p.emerge_sacrificed.is_none());
    let is_spell = cs.kind == CastKind::Spell;
    CostAidCaps {
        allows_convoke: breakdown.is_some_and(|b| b.allows_convoke)
            || (is_spell && casting_object_has_keyword_cached(state, cs, "convoke", cache)),
        allows_improvise: breakdown.is_some_and(|b| b.allows_improvise)
            || (is_spell && casting_object_has_keyword_cached(state, cs, "improvise", cache)),
        allows_delve: breakdown.is_some_and(|b| b.allows_delve)
            || (is_spell && casting_object_has_keyword_cached(state, cs, "delve", cache)),
        allows_emerge: (breakdown.is_some_and(|b| b.allows_emerge)
            || (is_spell && cs.alt_kind == AltCastKind::Emerge))
            && emerge_unused,
    }
}

fn casting_x_cant_be_zero(state: &GameState, cs: &CastingState, cache: &LayerCache) -> bool {
    match cs.kind {
        CastKind::Spell => {
            let Some(obj) = state.object(cs.object) else {
                return false;
            };
            let eff_def = state
                .rare(cs.object)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            let Some(card) = state.db().get(eff_def) else {
                return false;
            };
            let Some(face) = card.faces.get(usize::from(cs.face_idx)) else {
                return false;
            };
            find_spell_ability(state, eff_def, cs.face_idx, cs.cleaved, cs.gift_promised)
                .is_some_and(|sa| sa.x_cant_be_zero)
                || face.abilities.iter().any(|ab| {
                    matches!(
                        ab,
                        AbilityDef::Static(st)
                            if matches!(
                                st.static_def.body,
                                mtg_ir::StaticBody::SelfCastRestriction {
                                    x_cant_be_zero: true,
                                    ..
                                }
                            )
                    )
                })
        }
        CastKind::ActivatedAbility { ability_index, .. } => {
            resolve_activated_abilities_cached(state, cs.object, cache)
                .into_iter()
                .find(|r| r.ability_index == ability_index)
                .is_some_and(|r| r.ability.x_cant_be_zero)
        }
        CastKind::TriggeredAbility { .. } => false,
    }
}

fn invert_target_count_x_bound(amt: &mtg_ir::Amount, target_cap: i32) -> i32 {
    match amt {
        mtg_ir::Amount::X | mtg_ir::Amount::AnyNumber => target_cap,
        mtg_ir::Amount::Mul { factor, of } if *factor > 0 => {
            invert_target_count_x_bound(of, target_cap / *factor)
        }
        mtg_ir::Amount::Half { of, .. } => {
            invert_target_count_x_bound(of, target_cap.saturating_mul(2).saturating_add(1))
        }
        _ => i32::MAX,
    }
}

fn estimate_max_affordable_x_with_aids_cached(
    state: &GameState,
    cs: &CastingState,
    base_bd: &crate::costs::EffectiveCostBreakdown,
    cache: &LayerCache,
) -> i32 {
    let x_div = i32::from(base_bd.locked_mana.x_count.max(1));
    let Some(p) = state.player(cs.caster) else {
        return 0;
    };
    let pool_sum: u32 = p.mana_pool.iter().sum::<u32>()
        + p.restricted_mana()
            .iter()
            .map(|b| b.colors.iter().sum::<u32>())
            .sum::<u32>();
    let avail_cands = enumerate_mana_abilities_cached(state, cs.caster, cache);
    let mut seen_tap_objs: SmallVec<[(ObjectId, u32); 16]> = SmallVec::new();
    let mut tap_mana = 0u32;
    for c in avail_cands {
        let amt = c
            .produces_amounts
            .iter()
            .copied()
            .sum::<u32>()
            .max(c.max_any_color_amount)
            .max(1);
        if c.requires_tap {
            if let Some((_, prev)) = seen_tap_objs.iter_mut().find(|(id, _)| *id == c.object) {
                if amt > *prev {
                    tap_mana = tap_mana.saturating_sub(*prev).saturating_add(amt);
                    *prev = amt;
                }
                continue;
            }
            seen_tap_objs.push((c.object, amt));
        }
        tap_mana = tap_mana.saturating_add(amt);
    }

    let caps = casting_cost_aid_caps(state, cs, Some(base_bd), cache);
    let mut aid_extra = 0u32;
    if caps.allows_convoke || caps.allows_improvise {
        for &id in &p.battlefield {
            if seen_tap_objs.iter().any(|(sid, _)| *sid == id) {
                continue;
            }
            let Some(o) = state.object(id) else {
                continue;
            };
            if o.is_phased_out() || o.flags.contains(ObjectFlags::TAPPED) {
                continue;
            }
            let Some(ev) = cache.get(id) else {
                continue;
            };
            if ev.controller != cs.caster {
                continue;
            }
            let convoke_ok = caps.allows_convoke && ev.types.contains(CardTypeMask::CREATURE);
            let improvise_ok = caps.allows_improvise && ev.types.contains(CardTypeMask::ARTIFACT);
            if convoke_ok || improvise_ok {
                aid_extra = aid_extra.saturating_add(1);
            }
        }
    }
    if caps.allows_delve {
        let reserved =
            delve_reserved_gy_cards_cached(state, cs.caster, &base_bd.non_mana_costs, cache);
        let delve_cnt = p.graveyard.len().saturating_sub(reserved) as u32;
        aid_extra = aid_extra.saturating_add(delve_cnt);
    }
    if caps.allows_emerge {
        let mut emerge_max = 0u32;
        for &id in &p.battlefield {
            let Some(o) = state.object(id) else {
                continue;
            };
            if o.is_phased_out() {
                continue;
            }
            let Some(ev) = cache.get(id) else {
                continue;
            };
            if ev.controller == cs.caster
                && ev.types.contains(CardTypeMask::CREATURE)
                && !cant_sacrifice_object_cached(state, cs.caster, id, false, cache)
            {
                emerge_max = emerge_max.max(ev.mana_value);
            }
        }
        aid_extra = aid_extra.saturating_add(emerge_max);
    }

    let total_avail = pool_sum.saturating_add(tap_mana).saturating_add(aid_extra);
    let base_at_zero = casting_breakdown_cached(state, cs, Some(0), cache);
    let mut base_req = base_at_zero.as_ref().map_or(0, |b| {
        let non_phyrexian_pips = b
            .locked_mana
            .pips
            .iter()
            .filter(|p| {
                !matches!(
                    p,
                    ManaPip::Phyrexian { .. } | ManaPip::HybridPhyrexian { .. }
                )
            })
            .count() as u32;
        b.locked_mana.generic.saturating_add(non_phyrexian_pips)
    });
    if base_at_zero
        .as_ref()
        .is_some_and(|b| b.defiler_phyrexian_available.is_some())
    {
        base_req = base_req.saturating_sub(1);
    }

    let rem = total_avail.saturating_sub(base_req) as i32;
    let mut mana_cap = (rem / x_div).max(0);
    let spend_ctx =
        sync_spend_ctx_with_breakdown(build_casting_spend_ctx_cached(state, cs, cache), base_bd);
    if spend_ctx.x_max_one_per_color {
        let max_distinct_types = if !spend_ctx.x_spend_colors.is_empty() {
            colors_from_mask(spend_ctx.x_spend_colors).len() as i32
        } else if spend_ctx.x_colored_only {
            5
        } else {
            6
        };
        mana_cap = mana_cap.min(max_distinct_types / x_div);
    }
    mana_cap
}

#[inline]
fn casting_self_ctx(cs: &CastingState) -> CostListSelfContext {
    if cs.kind == CastKind::Spell {
        CostListSelfContext::for_spell_cast(cs.from_zone)
    } else {
        CostListSelfContext::default()
    }
}

fn is_casting_x_value_valid_cached(
    state: &GameState,
    cs: &CastingState,
    x: u32,
    cache: &LayerCache,
) -> bool {
    if x == 0 && casting_x_cant_be_zero(state, cs, cache) {
        return false;
    }
    let Some(breakdown) = casting_breakdown_cached(state, cs, Some(x), cache) else {
        return false;
    };
    let spend_ctx =
        sync_spend_ctx_with_breakdown(build_casting_spend_ctx_cached(state, cs, cache), &breakdown);
    let aid_caps = casting_cost_aid_caps(state, cs, Some(&breakdown), cache);
    let self_ctx = casting_self_ctx(cs);
    if !can_pay_costs_and_mana_with_self_ctx_cached(
        state,
        cs.caster,
        cs.object,
        &breakdown.locked_mana,
        &breakdown.non_mana_costs,
        &spend_ctx,
        aid_caps,
        Some(x),
        self_ctx,
        cache,
    ) {
        return false;
    }
    for c in &breakdown.non_mana_costs {
        if let Some((_, candidates, min_c, _max_c)) =
            non_mana_cost_candidates_in_list_with_ctx_cached(
                state,
                cs.caster,
                cs.object,
                c,
                &breakdown.non_mana_costs,
                Some(x),
                self_ctx,
                cache,
            )
            && min_c > 0
            && candidates.len() < usize::from(min_c)
        {
            return false;
        }
    }
    let (specs, spec_mode_positions) = active_target_specs_with_modal_cached(state, cs, cache);
    if !specs.is_empty() {
        let mut x_cs = cs.clone();
        x_cs.x_value = Some(x);
        let info = build_casting_target_info_cached(state, &x_cs, 0, cache);
        if !has_legal_target_assignment_with_modal_cached(
            state,
            &specs,
            &info,
            spec_mode_positions.as_deref(),
            cache,
        ) {
            return false;
        }
    }
    true
}

pub(crate) fn is_casting_x_value_valid(state: &GameState, value: i32) -> bool {
    if value < 0 {
        return false;
    }
    let Some(cs) = state.casting() else {
        return true;
    };
    is_casting_x_value_valid_cached(state, cs, value as u32, state.evaluate_board())
}

fn casting_x_info_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Option<(bool, i32, i32)> {
    let base_bd = casting_breakdown_cached(state, cs, None, cache)?;
    let has_x = base_bd.locked_mana.x_count > 0 || base_bd.non_mana_costs.iter().any(cost_uses_x);
    if !has_x {
        return None;
    }
    let x_cant_be_zero = casting_x_cant_be_zero(state, cs, cache);
    let min_x = if x_cant_be_zero { 1 } else { 0 };

    let mana_cap = if base_bd.locked_mana.x_count > 0 {
        estimate_max_affordable_x_with_aids_cached(state, cs, &base_bd, cache)
    } else {
        i32::MAX
    };
    let non_mana_cap = max_x_for_cost_list_with_ctx_cached(
        state,
        cs.caster,
        cs.object,
        &base_bd.non_mana_costs,
        casting_self_ctx(cs),
        cache,
    );
    let mut target_cap = i32::MAX;
    let specs = active_target_specs_cached(state, cs, cache);
    if !specs.is_empty() {
        let info = build_casting_target_info_cached(state, cs, 0, cache);
        let max_possible_targets =
            (state.objects().len() + state.players().len() + state.stack().len()) as i32;
        for spec in &specs {
            if let Some(dyn_amt) = spec.dynamic_count.as_deref()
                && amount_uses_x(dyn_amt)
                && !spec.dynamic_up_to
            {
                let mut cap = max_possible_targets;
                if let Some(div_amt) = spec.divide_total.as_deref()
                    && !amount_uses_x(div_amt)
                {
                    let div_val = info
                        .with_eval_context_cached(cache, |ctx| evaluate_amount(state, div_amt, ctx))
                        .max(0);
                    cap = cap.min(div_val);
                }
                target_cap = target_cap.min(invert_target_count_x_bound(dyn_amt, cap));
            }
        }
    }

    let mut raw_max_x = mana_cap.min(non_mana_cap).min(target_cap);
    if raw_max_x == i32::MAX {
        raw_max_x = 100;
    }
    let mut max_bound = raw_max_x;
    while max_bound >= min_x && !is_casting_x_value_valid_cached(state, cs, max_bound as u32, cache)
    {
        max_bound -= 1;
    }
    if max_bound < min_x {
        return Some((x_cant_be_zero, min_x, max_bound));
    }
    let mut min_bound = min_x;
    while min_bound <= max_bound
        && !is_casting_x_value_valid_cached(state, cs, min_bound as u32, cache)
    {
        min_bound += 1;
    }
    Some((x_cant_be_zero, min_bound, max_bound))
}

fn legal_casting_cost_branch_indices_cached(
    state: &GameState,
    cs: &CastingState,
    cost_index: u16,
    branches: &[Vec<CostDef>],
    cache: &LayerCache,
) -> SmallVec<[u16; 8]> {
    let mut out = SmallVec::new();
    let Some(plan) = cs.payment_plan.as_deref() else {
        return out;
    };
    let idx_usize = usize::from(cost_index);
    let rem_tail = plan
        .remaining_non_mana_costs
        .get(idx_usize + 1..)
        .unwrap_or(&[]);
    let breakdown = casting_breakdown_cached(state, cs, cs.x_value, cache);
    let mut spend_ctx = build_casting_spend_ctx_cached(state, cs, cache);
    if let Some(bd) = breakdown.as_ref() {
        spend_ctx = sync_spend_ctx_with_breakdown(spend_ctx, bd);
    }
    let aid_caps = casting_cost_aid_caps(state, cs, breakdown.as_ref(), cache);
    let self_ctx = casting_self_ctx(cs);
    for (i, branch) in branches.iter().enumerate() {
        let mut combined_costs = Vec::with_capacity(branch.len() + rem_tail.len());
        combined_costs.extend(branch.iter().cloned());
        combined_costs.extend(rem_tail.iter().cloned());
        if can_pay_costs_and_mana_with_self_ctx_cached(
            state,
            cs.caster,
            cs.object,
            &plan.remaining_mana,
            &combined_costs,
            &spend_ctx,
            aid_caps,
            cs.x_value,
            self_ctx,
            cache,
        ) {
            out.push(i as u16);
        }
    }
    out
}

pub(crate) fn legal_casting_cost_branch_indices(
    state: &GameState,
    cost_index: u16,
    branches: &[Vec<CostDef>],
) -> SmallVec<[u16; 8]> {
    let Some(cs) = state.casting() else {
        return SmallVec::new();
    };
    legal_casting_cost_branch_indices_cached(
        state,
        cs,
        cost_index,
        branches,
        state.evaluate_board(),
    )
}

fn collect_optional_cost_indices(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    if cs.kind != CastKind::Spell {
        return out;
    }
    let Some(obj) = state.object(cs.object) else {
        return out;
    };
    let eff_def = state
        .rare(cs.object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let Some(card) = state.db().get(eff_def) else {
        return out;
    };
    let Some(face) = card.faces.get(usize::from(cs.face_idx)) else {
        return out;
    };
    let caster_ctx =
        EvalContext::for_source(cs.caster, cs.object).with_board(BoardView::Cached(cache));
    let mut opt_idx_counter: u16 = 0;
    for ab in &face.abilities {
        match ab {
            AbilityDef::Spell(sp) => {
                if (!cs.cleaved || face.cleaved_spell_spec.is_none())
                    && (!cs.gift_promised || face.gift_promised_spell_spec.is_none())
                {
                    for c in &sp.additional_costs {
                        if matches!(c, CostDef::Optional { .. }) {
                            let idx = opt_idx_counter;
                            out.push((idx, format!("Pay optional cost #{idx}")));
                            opt_idx_counter = opt_idx_counter.saturating_add(1);
                        }
                    }
                }
            }
            AbilityDef::Keyword(kw) => {
                for _ in &kw.keyword.kicker_costs {
                    opt_idx_counter = opt_idx_counter.saturating_add(1);
                }
            }
            AbilityDef::Static(st) => {
                if let mtg_ir::StaticBody::AdditionalCastCost { costs } = &st.static_def.body
                    && st
                        .static_def
                        .condition
                        .as_deref()
                        .is_none_or(|cond| evaluate_condition(state, cond, &caster_ctx))
                {
                    for c in costs {
                        if matches!(c, CostDef::Optional { .. }) {
                            let idx = opt_idx_counter;
                            out.push((idx, format!("Pay optional cost #{idx}")));
                            opt_idx_counter = opt_idx_counter.saturating_add(1);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn can_pay_optional_cost_cached(
    state: &GameState,
    cs: &CastingState,
    cost_index: u16,
    cache: &LayerCache,
) -> bool {
    if cs.kind != CastKind::Spell {
        return false;
    }
    let mut test_optional = cs.optional_costs.clone();
    if !test_optional.contains(&cost_index) {
        test_optional.push(cost_index);
    }
    let opt = reconstruct_cast_option_cached(state, cs, cache);
    let Ok(breakdown) = compute_effective_spell_cost_with_modes_cached(
        state,
        cs.caster,
        cs.object,
        &opt,
        &cs.chosen_modes,
        cs.x_value,
        &test_optional,
        &cs.targets,
        cache,
    ) else {
        return false;
    };
    if !breakdown
        .optional_costs
        .iter()
        .any(|(idx, _)| *idx == cost_index)
    {
        return false;
    }
    let spend_ctx =
        sync_spend_ctx_with_breakdown(build_casting_spend_ctx_cached(state, cs, cache), &breakdown);
    spell_can_be_paid_with_x_cached(state, cs.caster, &breakdown, &spend_ctx, cs.x_value, cache)
}

pub(crate) fn can_pay_casting_optional_cost(state: &GameState, cost_index: u16) -> bool {
    let Some(cs) = state.casting() else {
        return false;
    };
    can_pay_optional_cost_cached(state, cs, cost_index, state.evaluate_board())
}

fn remove_triggered_stack_item(state: &mut GameState, stack_item: StackItemId) {
    if let Some(pos) = state
        .stack_entries()
        .iter()
        .position(|e| e.id == stack_item)
    {
        state.stack_mut().remove(pos);
        state.stack_entries_mut().remove(pos);
    }
    state.remove_stack_trigger_meta(stack_item);
    state.mark_dirty(DirtyMask::TOPOLOGY);
}

fn casting_modal_needs_cost_check(state: &GameState, cs: &CastingState, modal: &ModalDef) -> bool {
    if cs.kind != CastKind::Spell {
        return false;
    }
    if !modal.per_mode_costs.is_empty() {
        return true;
    }
    let Some(obj) = state.object(cs.object) else {
        return false;
    };
    let eff_def = state
        .rare(cs.object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    state
        .db()
        .get(eff_def)
        .and_then(|c| c.faces.get(usize::from(cs.face_idx)))
        .is_some_and(|face| find_keyword(face, state.db(), "escalate").is_some())
}

fn effective_x_for_modal_cost_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Option<u32> {
    if let Some(x) = cs.x_value {
        return Some(x);
    }
    if cs.kind != CastKind::Spell {
        return None;
    }
    let base_bd = casting_breakdown_cached(state, cs, None, cache)?;
    let has_x = base_bd.locked_mana.x_count > 0 || base_bd.non_mana_costs.iter().any(cost_uses_x);
    if !has_x {
        return None;
    }
    Some(if casting_x_cant_be_zero(state, cs, cache) {
        1
    } else {
        0
    })
}

fn casting_mode_cost_ok_cached(
    state: &GameState,
    cs: &CastingState,
    modal: &ModalDef,
    modes: &[u16],
    cache: &LayerCache,
) -> bool {
    if !casting_modal_needs_cost_check(state, cs, modal) {
        return true;
    }
    let opt = reconstruct_cast_option_cached(state, cs, cache);
    let x_val = effective_x_for_modal_cost_cached(state, cs, cache);
    let Ok(breakdown) = compute_effective_spell_cost_with_modes_cached(
        state,
        cs.caster,
        cs.object,
        &opt,
        modes,
        x_val,
        &cs.optional_costs,
        &[],
        cache,
    ) else {
        return false;
    };
    let spend_ctx =
        sync_spend_ctx_with_breakdown(build_casting_spend_ctx_cached(state, cs, cache), &breakdown);
    spell_can_be_paid_with_x_cached(state, cs.caster, &breakdown, &spend_ctx, x_val, cache)
}

pub(crate) fn legal_casting_mode_choices(
    state: &GameState,
    min: u16,
    max: u16,
    _allow_duplicates: bool,
    option_count: u16,
    picked: &[u16],
) -> (SmallVec<[u16; 8]>, bool) {
    let Some(cs) = state.casting() else {
        return (SmallVec::new(), false);
    };
    let cache = state.evaluate_board();
    let Some(modal) = find_modal_def_cached(state, cs, cache) else {
        return (SmallVec::new(), false);
    };
    let base_targets = base_target_specs_cached(state, cs, cache);
    let info = build_casting_target_info_cached(state, cs, 0, cache);
    let bounds = EffectiveModalBounds {
        choose_all: false,
        min,
        max,
        option_count,
    };
    legal_next_modes_and_done_cached(
        state,
        modal,
        &base_targets,
        &info,
        bounds,
        picked,
        |modes| casting_mode_cost_ok_cached(state, cs, modal, modes, cache),
        cache,
    )
}

/// Begins sequential `CR 601.2` casting of `object` from `from_zone` using `option_id`.
pub fn begin_cast_spell(
    state: &mut GameState,
    caster: PlayerId,
    object: ObjectId,
    option_id: u16,
    from_zone: Zone,
) -> Result<(), EngineError> {
    if state.casting().is_some()
        || state.pending_prompt().is_some()
        || state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
    {
        return Err(EngineError::IllegalAction(
            "cannot begin casting while another sequence or prompt is active".into(),
        ));
    }
    let Some(obj) = state.object(object) else {
        return Err(EngineError::IllegalAction(format!(
            "object {object:?} does not exist"
        )));
    };
    if obj.zone != from_zone {
        return Err(EngineError::IllegalAction(format!(
            "object {object:?} is in {:?}, expected {from_zone:?}",
            obj.zone
        )));
    }
    let Some(opt) = can_cast_spell_option(state, caster, object, option_id) else {
        return Err(EngineError::IllegalAction(format!(
            "cannot cast {object:?} with option_id {option_id} from {from_zone:?}"
        )));
    };

    let kicked_tags = {
        let mut tags = SmallVec::new();
        if let Some(card) = state.db().get(obj.card_def)
            && let Some(face) = card.faces.get(usize::from(opt.face_idx))
        {
            for ab in &face.abilities {
                if let AbilityDef::Keyword(k) = ab {
                    for &ki in &opt.kicker_indices {
                        if let Some(kc) = k.keyword.kicker_costs.get(usize::from(ki)) {
                            tags.extend(kc.tags.iter().cloned());
                        }
                    }
                }
            }
        }
        tags
    };

    let pre_cast_exile = if from_zone == Zone::Exile {
        let flags = state
            .object(object)
            .map_or(ObjectFlags::empty(), |o| o.flags);
        let rare = state.rare(object);
        Some(PreCastExileState {
            foretold: flags.contains(ObjectFlags::FORETOLD),
            was_face_down: flags.contains(ObjectFlags::FACE_DOWN),
            is_suspended: flags.contains(ObjectFlags::IS_SUSPENDED),
            unearth_leave_exile: flags.contains(ObjectFlags::UNEARTH_LEAVE_EXILE),
            face_down: rare.and_then(|r| r.face_down.clone()),
            known_to_players: rare.map(|r| r.known_to_players.clone()).unwrap_or_default(),
            exiled_by: rare.and_then(|r| r.exiled_by),
            playable_from_exile_by: rare.and_then(|r| r.playable_from_exile_by.clone()),
            plotted_by: rare.and_then(|r| r.plotted_by),
            plotted_turn: rare.and_then(|r| r.plotted_turn),
        })
    } else {
        None
    };

    // Move card onto Stack (CR 601.2a)
    let mut mv_opts = MoveRawOptions {
        to_player: Some(caster),
        controller: Some(caster),
        preserve_cast_state: false,
        ..Default::default()
    };
    if matches!(
        opt.alt_kind,
        AltCastKind::MorphFaceDown | AltCastKind::DisguiseFaceDown
    ) {
        mv_opts.enter_face_down = Some(FaceDownSpec {
            kind: if opt.alt_kind == AltCastKind::DisguiseFaceDown {
                FaceDownKind::Disguise
            } else {
                FaceDownKind::Morph
            },
            ..Default::default()
        });
    }

    let saved_opt = Box::new(opt.clone());
    let mv = move_object(state, object, Zone::Stack, mv_opts, None)?;
    let stack_obj = match mv {
        MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(object),
        MoveObjectOutcome::Paused { .. } => {
            let cs = CastingState {
                caster,
                object,
                face_idx: opt.face_idx,
                from_zone,
                step: CastStep::ChooseModes,
                chosen_modes: SmallVec::new(),
                x_value: None,
                optional_costs: opt.optional_cost_indices.clone(),
                optional_cost_cursor: 0,
                targets: Vec::new(),
                divisions: SmallVec::new(),
                locked_cost: ManaCost::default(),
                mana_paid: SmallVec::new(),
                paid_mana_colors: ColorMask::empty(),
                kind: CastKind::Spell,
                option_id: opt.option_id,
                alt_kind: opt.alt_kind,
                is_kicked: opt.is_kicked,
                kicked_indices: opt.kicker_indices.clone(),
                kicked_tags,
                fused: opt.fused,
                cleaved: opt.cleaved,
                gift_promised: opt.gift_promised,
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
                cast_option: Some(saved_opt),
                pre_cast_exile,
            };
            state.set_casting(Some(cs));
            return Ok(());
        }
        _ => {
            return Err(EngineError::IllegalAction(
                "spell zone move to stack was unexpectedly skipped".into(),
            ));
        }
    };

    apply_stack_cast_flags(state, stack_obj, &opt);

    let cs = CastingState {
        caster,
        object: stack_obj,
        face_idx: opt.face_idx,
        from_zone,
        step: CastStep::ChooseModes,
        chosen_modes: SmallVec::new(),
        x_value: None,
        optional_costs: opt.optional_cost_indices.clone(),
        optional_cost_cursor: 0,
        targets: Vec::new(),
        divisions: SmallVec::new(),
        locked_cost: ManaCost::default(),
        mana_paid: SmallVec::new(),
        paid_mana_colors: ColorMask::empty(),
        kind: CastKind::Spell,
        option_id: opt.option_id,
        alt_kind: opt.alt_kind,
        is_kicked: opt.is_kicked,
        kicked_indices: opt.kicker_indices,
        kicked_tags,
        fused: opt.fused,
        cleaved: opt.cleaved,
        gift_promised: opt.gift_promised,
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
        cast_option: Some(saved_opt),
        pre_cast_exile,
    };
    state.set_casting(Some(cs));
    state.set_decision(DecisionState::CastingSequence);
    state.set_priority_player(caster);
    advance_casting_state_machine(state)
}

fn apply_stack_cast_flags(state: &mut GameState, stack_obj: ObjectId, opt: &CastOption) {
    let is_face_down_cast = matches!(
        opt.alt_kind,
        AltCastKind::MorphFaceDown | AltCastKind::DisguiseFaceDown
    );
    if let Some(o) = state.object_mut(stack_obj) {
        o.active_face_index = opt.face_idx;
        o.flags.insert(ObjectFlags::WAS_CAST);
        if !is_face_down_cast {
            o.flags.remove(ObjectFlags::FACE_DOWN);
        }
        match opt.alt_kind {
            AltCastKind::Bestow => o.flags.insert(ObjectFlags::BESTOWED),
            AltCastKind::Prototype => o.flags.insert(ObjectFlags::PROTOTYPED),
            AltCastKind::Evoke => o.flags.insert(ObjectFlags::EVOKED),
            AltCastKind::Escape => o.flags.insert(ObjectFlags::ESCAPED),
            AltCastKind::Blitz => o.flags.insert(ObjectFlags::BLITZED),
            AltCastKind::Dash => o.flags.insert(ObjectFlags::DASHED),
            AltCastKind::ForetellCast => o.flags.insert(ObjectFlags::FORETOLD),
            _ => {}
        }
    }
    if let Some(rare) = state.rare_or_default(stack_obj) {
        rare.cast_from_zone = Some(opt.from_zone);
        rare.alt_cast_kind = opt.alt_kind;
        rare.fused = opt.fused;
        if !is_face_down_cast {
            rare.face_down = None;
        }
        rare.plotted_by = None;
        rare.plotted_turn = None;
        if opt.alt_kind == AltCastKind::Prototype {
            rare.prototyped_face = Some(opt.face_idx);
        }
    }
    if let Some(idx) = state.stack().iter().position(|&id| id == stack_obj)
        && let Some(se) = state.stack_entries_mut().get_mut(idx)
    {
        se.choices.alt_cast_kind = opt.alt_kind;
        se.choices.face_index = opt.face_idx;
        se.choices.from_zone = opt.from_zone;
        se.choices.fused = opt.fused;
        se.choices.cleaved = opt.cleaved;
        se.choices.gift_promised = opt.gift_promised;
        se.choices.bestowed = opt.alt_kind == AltCastKind::Bestow;
    }
    state.mark_dirty(DirtyMask::TOPOLOGY);
}

/// Begins sequential `CR 602.2` activation of non-mana activated ability `ability_index` on `object`.
pub fn begin_activate_ability(
    state: &mut GameState,
    activator: PlayerId,
    object: ObjectId,
    ability_index: u16,
) -> Result<(), EngineError> {
    if state.casting().is_some()
        || state.pending_prompt().is_some()
        || state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
    {
        return Err(EngineError::IllegalAction(
            "cannot begin ability activation while another sequence or prompt is active".into(),
        ));
    }
    let Some(res) = can_activate_ability(state, activator, object, ability_index) else {
        return Err(EngineError::IllegalAction(format!(
            "cannot activate ability {ability_index} on {object:?}"
        )));
    };
    let is_loyalty = res
        .ability
        .cost
        .iter()
        .any(|c| matches!(c, CostDef::Loyalty { .. }));
    let face_idx = res.face_index;
    let from_zone = state.object(object).map_or(Zone::Battlefield, |o| o.zone);

    let cs = CastingState {
        caster: activator,
        object,
        face_idx,
        from_zone,
        step: CastStep::ChooseModes,
        chosen_modes: SmallVec::new(),
        x_value: None,
        optional_costs: SmallVec::new(),
        optional_cost_cursor: 0,
        targets: Vec::new(),
        divisions: SmallVec::new(),
        locked_cost: ManaCost::default(),
        mana_paid: SmallVec::new(),
        paid_mana_colors: ColorMask::empty(),
        kind: CastKind::ActivatedAbility {
            ability_index,
            is_loyalty,
        },
        option_id: ability_index,
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
    };
    state.set_casting(Some(cs));
    state.set_decision(DecisionState::CastingSequence);
    state.set_priority_player(activator);
    advance_casting_state_machine(state)
}

/// Begins interactive mode/target selection for a triggered ability already on the stack (`CR 603.3c`).
pub fn begin_triggered_ability_targets(
    state: &mut GameState,
    stack_item: StackItemId,
) -> Result<(), EngineError> {
    if state.casting().is_some() || state.pending_prompt().is_some() {
        return Ok(());
    }
    let Some(entry) = state
        .stack_entries()
        .iter()
        .find(|e| e.id == stack_item)
        .cloned()
    else {
        return Err(EngineError::IllegalAction(format!(
            "stack item {stack_item:?} does not exist"
        )));
    };
    let Some(meta) = state.stack_trigger_meta(stack_item).cloned() else {
        return Ok(());
    };
    let needs_modes =
        meta.modal.is_some() && !entry.choices.modes_chosen && entry.choices.modes.is_empty();
    let needs_targets =
        !meta.targets.is_empty() && !entry.choices.targets_chosen && entry.targets.is_empty();
    if !needs_modes && !needs_targets {
        return Ok(());
    }

    let cs = CastingState {
        caster: entry.controller,
        object: meta.source,
        face_idx: meta.face_index,
        from_zone: Zone::Stack,
        step: if needs_modes {
            CastStep::ChooseModes
        } else {
            CastStep::ChooseTargets
        },
        chosen_modes: entry.choices.modes.clone(),
        x_value: Some(meta.x_value),
        optional_costs: SmallVec::new(),
        optional_cost_cursor: 0,
        targets: Vec::new(),
        divisions: SmallVec::new(),
        locked_cost: ManaCost::default(),
        mana_paid: SmallVec::new(),
        paid_mana_colors: ColorMask::empty(),
        kind: CastKind::TriggeredAbility { stack_item },
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
    };
    state.set_casting(Some(cs));
    state.set_decision(DecisionState::CastingSequence);
    state.set_priority_player(entry.controller);
    advance_casting_state_machine(state)
}

/// Scans stack entries (top-first) for the next triggered ability requiring mode/target selection.
pub fn prepare_next_untargeted_stack_trigger(state: &mut GameState) -> Result<bool, EngineError> {
    if state.casting().is_some()
        || state.pending_prompt().is_some()
        || state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
    {
        return Ok(false);
    }
    let next_sid = state.stack_entries().iter().rev().find_map(|entry| {
        if let StackEntryKind::Triggered { .. } = &entry.kind
            && !entry.choices.resolution_started
            && let Some(meta) = state.stack_trigger_meta(entry.id)
        {
            let needs_modes = meta.modal.is_some()
                && !entry.choices.modes_chosen
                && entry.choices.modes.is_empty();
            let needs_targets = !meta.targets.is_empty()
                && !entry.choices.targets_chosen
                && entry.targets.is_empty();
            if needs_modes || needs_targets {
                return Some(entry.id);
            }
        }
        None
    });
    if let Some(sid) = next_sid {
        begin_triggered_ability_targets(state, sid)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn on_replacement_resumed_during_casting(
    state: &mut GameState,
    mv: MoveObjectOutcome,
) -> Result<(), EngineError> {
    let Some(mut cs) = state.take_casting() else {
        return Ok(());
    };
    if cs.step == CastStep::ChooseModes {
        match mv {
            MoveObjectOutcome::Moved(raw) if raw.to_zone == Zone::Stack => {
                cs.object = raw.new_id.unwrap_or(raw.old_id);
                let opt = reconstruct_cast_option_cached(state, &cs, state.evaluate_board());
                let obj_id = cs.object;
                state.set_casting(Some(cs));
                apply_stack_cast_flags(state, obj_id, &opt);
                state.set_decision(DecisionState::CastingSequence);
                return advance_casting_state_machine(state);
            }
            _ => {
                let caster = cs.caster;
                state.clear_pending_mana_delayed_riders();
                state.set_decision(DecisionState::Priority);
                state.set_priority_player(caster);
                return Ok(());
            }
        }
    }
    if cs.step == CastStep::PayCosts
        && cs.payment_plan.as_ref().is_some_and(|p| {
            matches!(
                p.pending_step,
                Some(PendingNonManaCostStep::ZoneMove { .. })
            )
        })
    {
        let caster = cs.caster;
        let obj_id = cs.object;
        let mut plan = cs.payment_plan.take().unwrap();
        match resume_paused_non_mana_cost_zone_step(state, caster, obj_id, &mut plan, mv)? {
            CostStepOutcome::Paid => {
                plan.pending_step = None;
                plan.non_mana_cursor += 1;
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                state.set_decision(DecisionState::CastingSequence);
                state.set_priority_player(caster);
                return advance_casting_state_machine(state);
            }
            CostStepOutcome::Paused { .. } => {
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                return Ok(());
            }
        }
    }
    state.set_casting(Some(cs));
    advance_casting_state_machine(state)
}

pub(crate) fn on_effect_replacement_resumed_during_casting(
    state: &mut GameState,
    outcome: ReplacementOutcome,
) -> Result<(), EngineError> {
    let Some(mut cs) = state.take_casting() else {
        return Ok(());
    };
    if cs.step == CastStep::PayCosts
        && cs.payment_plan.as_ref().is_some_and(|p| {
            matches!(
                p.pending_step,
                Some(PendingNonManaCostStep::EffectReplace { .. })
            )
        })
    {
        let caster = cs.caster;
        let obj_id = cs.object;
        let mut plan = cs.payment_plan.take().unwrap();
        match resume_paused_non_mana_cost_effect_step(state, caster, obj_id, &mut plan, outcome)? {
            CostStepOutcome::Paid => {
                plan.pending_step = None;
                plan.non_mana_cursor += 1;
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                state.set_decision(DecisionState::CastingSequence);
                state.set_priority_player(caster);
                return advance_casting_state_machine(state);
            }
            CostStepOutcome::Paused { .. } => {
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                return Ok(());
            }
        }
    }
    state.set_casting(Some(cs));
    advance_casting_state_machine(state)
}

/// Advances the sequential `CR 601.2` state machine until it needs player input or commits.
pub fn advance_casting_state_machine(state: &mut GameState) -> Result<(), EngineError> {
    loop {
        let Some(mut cs) = state.take_casting() else {
            return Ok(());
        };

        match cs.step {
            CastStep::ChooseModes => {
                let cache = state.evaluate_board();
                if let Some(modal) = find_modal_def_cached(state, &cs, cache).cloned()
                    && cs.chosen_modes.is_empty()
                {
                    let base_targets = base_target_specs_cached(state, &cs, cache);
                    let info = build_casting_target_info_cached(state, &cs, 0, cache);
                    let bounds = info.with_eval_context_cached(cache, |eval_ctx| {
                        effective_modal_bounds_cached(state, &modal, eval_ctx)
                    });
                    if bounds.choose_all {
                        let all_modes: SmallVec<[u16; 8]> = (0..bounds.option_count).collect();
                        let (specs, mode_pos) =
                            build_modal_target_specs(&base_targets, &modal, &all_modes);
                        let ok =
                            has_legal_target_assignment_with_modal_cached(
                                state,
                                &specs,
                                &info,
                                mode_pos.as_deref(),
                                cache,
                            ) && casting_mode_cost_ok_cached(state, &cs, &modal, &all_modes, cache);
                        if !ok {
                            let caster = cs.caster;
                            if let CastKind::TriggeredAbility { stack_item } = cs.kind {
                                remove_triggered_stack_item(state, stack_item);
                                state.take_pending_prompt();
                                state.set_decision(DecisionState::Priority);
                                state.set_priority_player(caster);
                                let _ = prepare_next_untargeted_stack_trigger(state)?;
                                return Ok(());
                            }
                            state.set_casting(Some(cs));
                            let _ = cancel_active_cast(state, caster);
                            return Err(EngineError::IllegalAction(
                                "no legal modal choices".into(),
                            ));
                        }
                        for m in all_modes {
                            cs.chosen_modes.push(m);
                        }
                        cs.step = CastStep::ChooseX;
                        state.set_casting(Some(cs));
                        continue;
                    }

                    if bounds.max == 0 || bounds.option_count == 0 {
                        cs.step = CastStep::ChooseX;
                        state.set_casting(Some(cs));
                        continue;
                    }

                    let (legal_indices, can_done) = legal_next_modes_and_done_cached(
                        state,
                        &modal,
                        &base_targets,
                        &info,
                        bounds,
                        &[],
                        |modes| casting_mode_cost_ok_cached(state, &cs, &modal, modes, cache),
                        cache,
                    );
                    if bounds.min == 0 && legal_indices.is_empty() && can_done {
                        cs.step = CastStep::ChooseX;
                        state.set_casting(Some(cs));
                        continue;
                    }
                    if legal_indices.is_empty() && !can_done {
                        let caster = cs.caster;
                        if let CastKind::TriggeredAbility { stack_item } = cs.kind {
                            remove_triggered_stack_item(state, stack_item);
                            state.take_pending_prompt();
                            state.set_decision(DecisionState::Priority);
                            state.set_priority_player(caster);
                            let _ = prepare_next_untargeted_stack_trigger(state)?;
                            return Ok(());
                        }
                        state.set_casting(Some(cs));
                        let _ = cancel_active_cast(state, caster);
                        return Err(EngineError::IllegalAction("no legal modal choices".into()));
                    }

                    let is_random = modal.random_mode
                        && modal.conditional_not_random.as_deref().is_none_or(|c| {
                            !info.with_eval_context_cached(cache, |eval_ctx| {
                                evaluate_condition(state, c, eval_ctx)
                            })
                        });
                    if is_random && bounds.option_count > 0 {
                        let pick_n = bounds.min.max(1);
                        for _ in 0..pick_n {
                            let cur_legal = {
                                let cache = state.evaluate_board();
                                let info = build_casting_target_info_cached(state, &cs, 0, cache);
                                let (cur_legal, cur_can_done) = legal_next_modes_and_done_cached(
                                    state,
                                    &modal,
                                    &base_targets,
                                    &info,
                                    bounds,
                                    &cs.chosen_modes,
                                    |modes| {
                                        casting_mode_cost_ok_cached(
                                            state, &cs, &modal, modes, cache,
                                        )
                                    },
                                    cache,
                                );
                                if cur_legal.is_empty() && cur_can_done {
                                    break;
                                }
                                cur_legal
                            };
                            if cur_legal.is_empty() {
                                break;
                            }
                            let idx = (state.rng_mut().next_u32() as usize) % cur_legal.len();
                            cs.chosen_modes.push(cur_legal[idx]);
                        }
                        cs.chosen_modes.sort_unstable();
                        cs.step = CastStep::ChooseX;
                        state.set_casting(Some(cs));
                        continue;
                    }

                    let chooser = match modal.opponent_chooses {
                        Some(_) => state
                            .players()
                            .iter()
                            .find(|p| !p.lost && p.id != cs.caster)
                            .map_or(cs.caster, |p| p.id),
                        None => cs.caster,
                    };
                    let prompt_id = state.allocate_prompt_id();
                    let req = ChoiceRequest::ChooseMode {
                        prompt_id,
                        option_count: bounds.option_count,
                        min: bounds.min,
                        max: bounds.max,
                    };
                    state.set_pending_prompt(Some(PendingPromptContext {
                        prompt_id,
                        chooser,
                        request: req.clone(),
                        owner: PromptOwner::Casting(CastingPromptKind::CastModes {
                            min: bounds.min,
                            max: bounds.max,
                            allow_duplicates: modal.allow_repeat,
                            option_count: bounds.option_count,
                            picked: SmallVec::new(),
                        }),
                    }));
                    state.set_casting(Some(cs));
                    state.set_priority_player(chooser);
                    state.set_decision(DecisionState::Prompt { request: req });
                    return Ok(());
                }
                cs.step = CastStep::ChooseX;
                state.set_casting(Some(cs));
            }
            CastStep::ChooseX => {
                if cs.x_value.is_none()
                    && let Some((_x_cant_be_zero, min_bound, max_bound)) =
                        casting_x_info_cached(state, &cs, state.evaluate_board())
                {
                    if min_bound > max_bound {
                        let caster = cs.caster;
                        if let CastKind::TriggeredAbility { stack_item } = cs.kind {
                            remove_triggered_stack_item(state, stack_item);
                            state.take_pending_prompt();
                            state.set_decision(DecisionState::Priority);
                            state.set_priority_player(caster);
                            let _ = prepare_next_untargeted_stack_trigger(state)?;
                            return Ok(());
                        }
                        state.set_casting(Some(cs));
                        let _ = cancel_active_cast(state, caster);
                        return Err(EngineError::IllegalAction("no valid X value".into()));
                    }
                    let prompt_id = state.allocate_prompt_id();
                    let req = ChoiceRequest::ChooseNumber {
                        prompt_id,
                        min: min_bound,
                        max: max_bound,
                    };
                    let chooser = cs.caster;
                    state.set_pending_prompt(Some(PendingPromptContext {
                        prompt_id,
                        chooser,
                        request: req.clone(),
                        owner: PromptOwner::Casting(CastingPromptKind::CastXValue {
                            min: min_bound,
                            max: max_bound,
                        }),
                    }));
                    state.set_casting(Some(cs));
                    state.set_priority_player(chooser);
                    state.set_decision(DecisionState::Prompt { request: req });
                    return Ok(());
                }
                cs.step = CastStep::ChooseOptionalCosts;
                state.set_casting(Some(cs));
            }
            CastStep::ChooseOptionalCosts => {
                let cache = state.evaluate_board();
                let opt_list = collect_optional_cost_indices(state, &cs, cache);
                if let Some((idx, label)) = opt_list.into_iter().find(|(idx, _)| {
                    usize::from(*idx) >= cs.optional_cost_cursor
                        && !cs.optional_costs.contains(idx)
                        && can_pay_optional_cost_cached(state, &cs, *idx, cache)
                }) {
                    let prompt_id = state.allocate_prompt_id();
                    let req = ChoiceRequest::YesNo {
                        prompt_id,
                        label: label.clone(),
                    };
                    let chooser = cs.caster;
                    state.set_pending_prompt(Some(PendingPromptContext {
                        prompt_id,
                        chooser,
                        request: req.clone(),
                        owner: PromptOwner::Casting(CastingPromptKind::CastOptionalCost {
                            cost_index: idx,
                            label,
                        }),
                    }));
                    state.set_casting(Some(cs));
                    state.set_priority_player(chooser);
                    state.set_decision(DecisionState::Prompt { request: req });
                    return Ok(());
                }
                cs.step = CastStep::ChooseTargets;
                state.set_casting(Some(cs));
            }
            CastStep::ChooseTargets => {
                let cache = state.evaluate_board();
                let (specs, spec_mode_positions) =
                    active_target_specs_with_modal_cached(state, &cs, cache);
                if cs.targets_by_spec.len() < specs.len() {
                    cs.targets_by_spec.resize(specs.len(), Vec::new());
                }
                while cs.target_spec_cursor < specs.len() {
                    let cur = cs.target_spec_cursor;
                    let spec = &specs[cur];
                    let info = build_casting_target_info_cached(state, &cs, cur, cache);
                    let (min_req, max_req) =
                        effective_target_bounds_cached(state, spec, &info, cache);
                    if min_req <= max_req
                        && (max_req == 0 || cs.targets_by_spec[cur].len() >= usize::from(max_req))
                    {
                        cs.target_spec_cursor += 1;
                        continue;
                    }
                    let raw_cands = legal_targets_for_spec_cached(
                        state,
                        spec,
                        &info,
                        &cs.targets_by_spec[cur],
                        cache,
                    );
                    let can_done_base = min_req <= max_req
                        && cs.targets_by_spec[cur].len() >= usize::from(min_req)
                        && can_finish_spec_flagbearer_ok_cached(
                            state,
                            spec,
                            &info,
                            &cs.targets_by_spec[cur],
                            cache,
                        );
                    let (cands, can_done) = filter_casting_target_choices_with_modal_cached(
                        state,
                        &specs,
                        &info,
                        cur,
                        &cs.targets_by_spec[cur],
                        raw_cands,
                        can_done_base,
                        spec_mode_positions.as_deref(),
                        cache,
                    );
                    if cands.is_empty() && can_done {
                        cs.target_spec_cursor += 1;
                        continue;
                    }
                    if cands.is_empty()
                        && !can_done
                        && let CastKind::TriggeredAbility { stack_item } = cs.kind
                    {
                        let caster = cs.caster;
                        remove_triggered_stack_item(state, stack_item);
                        state.take_pending_prompt();
                        state.set_decision(DecisionState::Priority);
                        state.set_priority_player(caster);
                        let _ = prepare_next_untargeted_stack_trigger(state)?;
                        return Ok(());
                    }
                    let caster = cs.caster;
                    state.set_casting(Some(cs));
                    state.set_priority_player(caster);
                    state.set_decision(DecisionState::CastingSequence);
                    return Ok(());
                }
                cs.targets = cs.targets_by_spec.iter().flatten().copied().collect();
                cs.step = CastStep::ChooseDivisions;
                state.set_casting(Some(cs));
            }
            CastStep::ChooseDivisions => {
                let cache = state.evaluate_board();
                let specs = active_target_specs_cached(state, &cs, cache);
                let mut pending_div: Option<(TargetRef, u32)> = None;
                for (si, spec) in specs.iter().enumerate() {
                    let Some(amt) = spec.divide_total.as_deref() else {
                        continue;
                    };
                    let chosen = cs.targets_by_spec.get(si).map_or(&[][..], |v| v.as_slice());
                    if chosen.is_empty() {
                        continue;
                    }
                    let info = build_casting_target_info_cached(state, &cs, si, cache);
                    let total = info
                        .with_eval_context_cached(cache, |ctx| evaluate_amount(state, amt, ctx))
                        .max(0) as u32;
                    let assigned: u32 = chosen
                        .iter()
                        .filter_map(|t| {
                            cs.divisions.iter().find(|(dt, _)| dt == t).map(|(_, a)| *a)
                        })
                        .sum();
                    if chosen.len() == 1 {
                        if !cs.divisions.iter().any(|(dt, _)| *dt == chosen[0]) {
                            cs.divisions.push((chosen[0], total));
                        }
                        continue;
                    }
                    let unassigned: Vec<TargetRef> = chosen
                        .iter()
                        .copied()
                        .filter(|t| !cs.divisions.iter().any(|(dt, _)| dt == t))
                        .collect();
                    let rem = total.saturating_sub(assigned);
                    if total == 0 || assigned >= total {
                        for t in unassigned {
                            cs.divisions.push((t, 0));
                        }
                    } else if unassigned.len() == 1 {
                        cs.divisions.push((unassigned[0], rem));
                    } else if rem == unassigned.len() as u32 {
                        for t in unassigned {
                            cs.divisions.push((t, 1));
                        }
                    } else if !unassigned.is_empty() {
                        pending_div = Some((unassigned[0], rem));
                        break;
                    }
                }
                if pending_div.is_some() {
                    let caster = cs.caster;
                    state.set_casting(Some(cs));
                    state.set_priority_player(caster);
                    state.set_decision(DecisionState::CastingSequence);
                    return Ok(());
                }
                cs.step = CastStep::PayCosts;
                state.set_casting(Some(cs));
            }
            CastStep::PayCosts => {
                if matches!(cs.kind, CastKind::TriggeredAbility { .. }) {
                    cs.step = CastStep::Complete;
                    state.set_casting(Some(cs));
                    continue;
                }

                // CR 601.2f: Lock total cost BEFORE paying any non-mana costs (t1_f11_03).
                if cs.payment_plan.is_none() {
                    let breakdown = {
                        let cache = state.evaluate_board();
                        match cs.kind {
                            CastKind::Spell => {
                                let opt = reconstruct_cast_option_cached(state, &cs, cache);
                                compute_effective_spell_cost_with_modes_cached(
                                    state,
                                    cs.caster,
                                    cs.object,
                                    &opt,
                                    &cs.chosen_modes,
                                    cs.x_value,
                                    &cs.optional_costs,
                                    &cs.targets,
                                    cache,
                                )?
                            }
                            CastKind::ActivatedAbility { ability_index, .. } => {
                                compute_effective_ability_cost_cached(
                                    state,
                                    cs.caster,
                                    cs.object,
                                    ability_index,
                                    cs.x_value,
                                    &cs.targets,
                                    cache,
                                )?
                            }
                            CastKind::TriggeredAbility { .. } => unreachable!(),
                        }
                    };
                    cs.locked_cost = breakdown.locked_mana.clone();
                    cs.additional_cost_paid = !breakdown.non_mana_costs.is_empty()
                        || !cs.optional_costs.is_empty()
                        || cs.is_kicked;
                    cs.bargained = cs
                        .kicked_tags
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case("bargain"));
                    let plan = PendingCostPaymentPlan {
                        locked_mana_cost: breakdown.locked_mana.clone(),
                        remaining_mana: breakdown.locked_mana,
                        remaining_non_mana_costs: breakdown.non_mana_costs,
                        non_mana_cursor: 0,
                        pending_step: None,
                        chosen_optional_costs: cs.optional_costs.clone(),
                        convoke_tapped: SmallVec::new(),
                        improvise_tapped: SmallVec::new(),
                        delve_exiled: SmallVec::new(),
                        emerge_sacrificed: None,
                        waterbend_tapped: SmallVec::new(),
                        sacrificed_lki: Vec::new(),
                        revealed_cards: Vec::new(),
                        revealed_card_defs: Vec::new(),
                        discarded_cards: Vec::new(),
                        counters_removed: 0,
                        phyrexian_pay_life: SmallVec::new(),
                        cant_be_countered_from_mana: false,
                        mana_commander_bonus_counters: 0,
                    };
                    cs.payment_plan = Some(Box::new(plan));
                }

                // Step 1: Mana payment (wait for PayFromPool / TapMana / AutoPayMana / SelectCostAid)
                let needs_mana = cs.payment_plan.as_ref().is_some_and(|p| {
                    p.remaining_mana.generic > 0 || !p.remaining_mana.pips.is_empty()
                });
                if needs_mana {
                    let caster = cs.caster;
                    state.set_casting(Some(cs));
                    state.set_priority_player(caster);
                    state.set_decision(DecisionState::CastingSequence);
                    return Ok(());
                }

                // Step 2: Non-mana costs
                let cursor_cost = cs
                    .payment_plan
                    .as_ref()
                    .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor).cloned());
                if let Some(cost_def) = cursor_cost {
                    let cost_idx = cs.payment_plan.as_ref().map_or(0, |p| p.non_mana_cursor);

                    if let CostDef::Mana { cost: mc, .. } = &cost_def {
                        if let Some(plan) = cs.payment_plan.as_deref_mut() {
                            let mut eff = mc.clone();
                            if eff.x_count > 0
                                && let Some(xv) = cs.x_value
                            {
                                eff.generic = eff
                                    .generic
                                    .saturating_add(xv.saturating_mul(u32::from(eff.x_count)));
                                eff.x_count = 0;
                            }
                            plan.remaining_mana.generic =
                                plan.remaining_mana.generic.saturating_add(eff.generic);
                            plan.remaining_mana.x_count =
                                plan.remaining_mana.x_count.saturating_add(eff.x_count);
                            plan.remaining_mana.pips.extend(eff.pips);
                            plan.non_mana_cursor += 1;
                        }
                        state.set_casting(Some(cs));
                        continue;
                    }

                    let remaining_costs: &[CostDef] = cs
                        .payment_plan
                        .as_deref()
                        .and_then(|p| p.remaining_non_mana_costs.get(cost_idx..))
                        .unwrap_or(&[]);
                    if let Some((kind, candidates, min_c, max_c)) =
                        non_mana_cost_candidates_in_list_with_ctx_cached(
                            state,
                            cs.caster,
                            cs.object,
                            &cost_def,
                            remaining_costs,
                            cs.x_value,
                            casting_self_ctx(&cs),
                            state.evaluate_board(),
                        )
                        && !candidates.is_empty()
                        && max_c > 0
                    {
                        if candidates.len() < usize::from(min_c) {
                            state.set_casting(Some(cs));
                            return Err(EngineError::IllegalAction(
                                "insufficient candidates to satisfy non-mana cost".into(),
                            ));
                        }
                        let prompt_id = state.allocate_prompt_id();
                        let req = ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates: candidates.clone(),
                            min: min_c,
                            max: max_c,
                        };
                        let chooser = cs.caster;
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id,
                            chooser,
                            request: req.clone(),
                            owner: PromptOwner::Casting(CastingPromptKind::CostObjectSelection {
                                cost_index: cost_idx as u16,
                                kind,
                                candidates,
                                min: min_c,
                                max: max_c,
                                picked: Vec::new(),
                            }),
                        }));
                        state.set_casting(Some(cs));
                        state.set_priority_player(chooser);
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(());
                    }

                    if let CostDef::Choice { options, forage } = &cost_def
                        && !*forage
                    {
                        let legal_indices = legal_casting_cost_branch_indices_cached(
                            state,
                            &cs,
                            cost_idx as u16,
                            options,
                            state.evaluate_board(),
                        );
                        if legal_indices.is_empty() {
                            state.set_casting(Some(cs));
                            return Err(EngineError::IllegalAction(
                                "no payable option in Choice cost".into(),
                            ));
                        }
                        let prompt_id = state.allocate_prompt_id();
                        let option_count = options.len() as u16;
                        let req = ChoiceRequest::ChooseMode {
                            prompt_id,
                            option_count,
                            min: 1,
                            max: 1,
                        };
                        let chooser = cs.caster;
                        state.set_pending_prompt(Some(PendingPromptContext {
                            prompt_id,
                            chooser,
                            request: req.clone(),
                            owner: PromptOwner::Casting(CastingPromptKind::CostBranchChoice {
                                cost_index: cost_idx as u16,
                                branches: options.clone(),
                            }),
                        }));
                        state.set_casting(Some(cs));
                        state.set_priority_player(chooser);
                        state.set_decision(DecisionState::Prompt { request: req });
                        return Ok(());
                    }

                    if let CostDef::Optional { cost: inner } = &cost_def {
                        let payable = {
                            let cache = state.evaluate_board();
                            let plan_ref = cs.payment_plan.as_deref();
                            let rem_mana = plan_ref
                                .map_or_else(ManaCost::default, |p| p.remaining_mana.clone());
                            let rem_tail = plan_ref
                                .and_then(|p| p.remaining_non_mana_costs.get(cost_idx + 1..))
                                .unwrap_or(&[]);
                            let mut combined = Vec::with_capacity(inner.len() + rem_tail.len());
                            combined.extend(inner.iter().cloned());
                            combined.extend(rem_tail.iter().cloned());
                            let breakdown = casting_breakdown_cached(state, &cs, cs.x_value, cache);
                            let mut spend_ctx = build_casting_spend_ctx_cached(state, &cs, cache);
                            if let Some(bd) = breakdown.as_ref() {
                                spend_ctx = sync_spend_ctx_with_breakdown(spend_ctx, bd);
                            }
                            let aid_caps =
                                casting_cost_aid_caps(state, &cs, breakdown.as_ref(), cache);
                            can_pay_costs_and_mana_with_self_ctx_cached(
                                state,
                                cs.caster,
                                cs.object,
                                &rem_mana,
                                &combined,
                                &spend_ctx,
                                aid_caps,
                                cs.x_value,
                                casting_self_ctx(&cs),
                                cache,
                            )
                        };
                        if let Some(plan) = cs.payment_plan.as_deref_mut() {
                            let replacement = if payable { inner.clone() } else { Vec::new() };
                            plan.remaining_non_mana_costs
                                .splice(cost_idx..=cost_idx, replacement);
                        }
                        state.set_casting(Some(cs));
                        continue;
                    }

                    let caster = cs.caster;
                    let obj_id = cs.object;
                    let x_val = cs.x_value;
                    let mut plan = cs.payment_plan.take().unwrap();
                    let outcome = execute_non_mana_cost_step(
                        state,
                        caster,
                        obj_id,
                        &cost_def,
                        &[],
                        x_val,
                        Some(&mut plan),
                    )?;
                    match outcome {
                        CostStepOutcome::Paid => {
                            plan.pending_step = None;
                            plan.non_mana_cursor += 1;
                            cs.payment_plan = Some(plan);
                            state.set_casting(Some(cs));
                            continue;
                        }
                        CostStepOutcome::Paused { .. } => {
                            if plan.pending_step.is_none() {
                                plan.pending_step = Some(PendingNonManaCostStep::WaitingCostPrompt);
                            }
                            cs.payment_plan = Some(plan);
                            state.set_casting(Some(cs));
                            return Ok(());
                        }
                    }
                }

                cs.step = CastStep::Complete;
                state.set_casting(Some(cs));
            }
            CastStep::Complete => {
                finalize_cast_commit(state, cs)?;
                return Ok(());
            }
        }
    }
}

fn finalize_cast_commit(state: &mut GameState, cs: CastingState) -> Result<(), EngineError> {
    let cant_be_countered_from_mana = cs.cant_be_countered_from_mana
        || cs
            .payment_plan
            .as_ref()
            .is_some_and(|p| p.cant_be_countered_from_mana);
    let commander_bonus_counters = cs.mana_commander_bonus_counters.max(
        cs.payment_plan
            .as_ref()
            .map_or(0, |p| p.mana_commander_bonus_counters),
    );
    let kicked_count = if cs.is_kicked {
        cs.kicked_indices.len().max(1) as u16
    } else {
        0
    };
    let exile_on_leave_stack = matches!(
        cs.alt_kind,
        AltCastKind::Flashback
            | AltCastKind::JumpStart
            | AltCastKind::Harmonize
            | AltCastKind::Encore
    );
    let x_val = cs.x_value.unwrap_or(0);

    match cs.kind {
        CastKind::Spell => {
            crate::commander::record_command_zone_cast(state, cs.object, cs.from_zone);
            if let Some(rare) = state.rare_or_default(cs.object) {
                rare.entered_with_x = cs.x_value;
                rare.cast_from_zone = Some(cs.from_zone);
                rare.paid_mana_colors = cs.paid_mana_colors;
                rare.mana_spent_count = cs.mana_spent_total;
                rare.kicked_count = kicked_count;
                rare.bargained = cs.bargained;
                rare.additional_cost_paid = cs.additional_cost_paid;
                rare.alt_cast_kind = cs.alt_kind;
                rare.kicked_tags = cs.kicked_tags.clone();
                rare.on_an_adventure = false;
            }
            let is_creature = state
                .evaluate_board()
                .get(cs.object)
                .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE));
            if !is_creature && let Some(p) = state.player_mut(cs.caster) {
                p.noncreature_spells_cast_this_turn =
                    p.noncreature_spells_cast_this_turn.saturating_add(1);
            }

            let mut stack_item_id = StackItemId(0);
            if let Some(idx) = state.stack().iter().position(|&id| id == cs.object)
                && let Some(se) = state.stack_entries_mut().get_mut(idx)
            {
                stack_item_id = se.id;
                se.targets = cs.targets.clone();
                se.x_value = x_val;
                se.paid_mana_colors = cs.paid_mana_colors;
                se.choices.modes = cs.chosen_modes.clone();
                se.choices.optional_costs = cs.optional_costs.clone();
                se.choices.divisions = cs.divisions.clone();
                se.choices.fused = cs.fused;
                se.choices.cleaved = cs.cleaved;
                se.choices.gift_promised = cs.gift_promised;
                se.choices.kicked_count = kicked_count;
                se.choices.alt_cast_kind = cs.alt_kind;
                se.choices.additional_cost_paid = cs.additional_cost_paid;
                se.choices.bargained = cs.bargained;
                se.choices.kicked_indices = cs.kicked_indices.clone();
                se.choices.kicked_tags = cs.kicked_tags.clone();
                se.choices.cant_be_countered = cant_be_countered_from_mana;
                se.choices.commander_bonus_counters = commander_bonus_counters;
                se.choices.face_index = cs.face_idx;
                se.choices.from_zone = cs.from_zone;
                se.choices.exile_on_leave_stack = exile_on_leave_stack;
                se.choices.targets_by_spec = cs.targets_by_spec.clone();
                se.choices.modes_chosen = true;
                se.choices.targets_chosen = true;
            }

            let riders = state.take_pending_mana_delayed_riders();
            if !riders.is_empty() {
                state.push_stack_mana_riders(stack_item_id, riders);
            }

            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);

            let colored_spent = cs.paid_mana_colors.color_count();
            emit_event(
                state,
                AtomicEventKind::Cast {
                    player: cs.caster,
                    object: cs.object,
                    stack_item: stack_item_id,
                    is_copy: false,
                    targets: cs.targets.clone(),
                    mana_spent: cs.mana_spent_total,
                    colored_mana_spent: colored_spent,
                    kicked: cs.is_kicked,
                    from_zone: cs.from_zone,
                },
            );
            emit_target_and_crime_events(state, cs.caster, true, Some(stack_item_id), &cs.targets);
        }
        CastKind::ActivatedAbility {
            ability_index,
            is_loyalty,
        } => {
            if is_loyalty && let Some(rare) = state.rare_or_default(cs.object) {
                rare.used_loyalty_this_turn = true;
            }
            let (is_once_turn, is_once_game) = {
                let res =
                    resolve_activated_abilities_cached(state, cs.object, state.evaluate_board())
                        .into_iter()
                        .find(|r| r.ability_index == ability_index);
                (
                    res.as_ref().is_some_and(|r| {
                        r.grant_once_each_turn
                            || r.ability
                                .activation_restrictions
                                .contains(&mtg_ir::ActivationRestriction::OnceEachTurn)
                    }),
                    res.as_ref().is_some_and(|r| {
                        r.ability
                            .activation_restrictions
                            .contains(&mtg_ir::ActivationRestriction::OncePerGame)
                    }),
                )
            };
            if (is_once_turn || is_once_game)
                && let Some(p) = state.player_mut(cs.caster)
            {
                if is_once_turn
                    && !p
                        .once_per_turn_abilities_used()
                        .contains(&(cs.object, ability_index))
                {
                    p.mana_ext_mut()
                        .once_per_turn_abilities_used
                        .push((cs.object, ability_index));
                }
                if is_once_game
                    && !p
                        .once_per_game_abilities_used()
                        .contains(&(cs.object, ability_index))
                {
                    p.mana_ext_mut()
                        .once_per_game_abilities_used
                        .push((cs.object, ability_index));
                }
            }
            let lki = build_source_lki_snapshot(state, cs.object);
            let sid = state.allocate_stack_id();
            state.stack_mut().push(cs.object);
            state.stack_entries_mut().push(StackEntry {
                id: sid,
                kind: StackEntryKind::Activated {
                    source: cs.object,
                    ability_idx: usize::from(ability_index),
                    lki,
                },
                controller: cs.caster,
                choices: StackChoices {
                    modes: cs.chosen_modes.clone(),
                    optional_costs: cs.optional_costs.clone(),
                    divisions: cs.divisions.clone(),
                    cant_be_countered: cant_be_countered_from_mana,
                    face_index: cs.face_idx,
                    from_zone: cs.from_zone,
                    targets_by_spec: cs.targets_by_spec.clone(),
                    modes_chosen: true,
                    targets_chosen: true,
                    ..Default::default()
                },
                targets: cs.targets.clone(),
                x_value: x_val,
                paid_mana_colors: cs.paid_mana_colors,
            });
            let riders = state.take_pending_mana_delayed_riders();
            if !riders.is_empty() {
                state.push_stack_mana_riders(sid, riders);
            }
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
            emit_event(
                state,
                AtomicEventKind::AbilityActivated {
                    player: cs.caster,
                    source: cs.object,
                    stack_item: Some(sid),
                    is_mana: false,
                    is_loyalty,
                    life_paid: 0,
                },
            );
            emit_target_and_crime_events(state, cs.caster, false, Some(sid), &cs.targets);
        }
        CastKind::TriggeredAbility { stack_item } => {
            if let Some(se) = state
                .stack_entries_mut()
                .iter_mut()
                .find(|e| e.id == stack_item)
            {
                se.targets = cs.targets.clone();
                se.choices.modes = cs.chosen_modes.clone();
                se.choices.divisions = cs.divisions.clone();
                se.choices.targets_by_spec = cs.targets_by_spec.clone();
                se.choices.modes_chosen = true;
                se.choices.targets_chosen = true;
            }
            emit_target_and_crime_events(state, cs.caster, false, Some(stack_item), &cs.targets);
        }
    }

    state.set_decision(DecisionState::Priority);
    state.set_priority_player(cs.caster);
    flush_triggers(state);
    let _ = prepare_next_untargeted_stack_trigger(state)?;
    Ok(())
}

fn build_source_lki_snapshot(state: &GameState, id: ObjectId) -> Arc<LkiSnapshot> {
    let obj = state.object(id);
    let cache = state.evaluate_board();
    let ev = cache.get(id);
    let view = active_face_view(state, id);
    let rare = state.rare(id);
    let name = ev
        .and_then(|e| e.name_override.clone())
        .or_else(|| view.as_ref().map(|v| v.name.clone()))
        .unwrap_or_default();
    Arc::new(LkiSnapshot {
        object_id: id,
        card_def: obj.map_or(crate::state::TOKEN_CARD_DEF_ID, |o| o.card_def),
        name,
        owner: obj.map_or(PlayerId(0), |o| o.owner),
        controller: ev
            .map(|e| e.controller)
            .or_else(|| obj.map(|o| o.controller))
            .unwrap_or(PlayerId(0)),
        last_zone: obj.map_or(Zone::Battlefield, |o| o.zone),
        pt: ev
            .and_then(|e| e.pt)
            .or_else(|| view.as_ref().and_then(|v| v.pt)),
        types: ev
            .map(|e| e.types)
            .or_else(|| view.as_ref().map(|v| v.types))
            .unwrap_or_default(),
        subtypes: ev
            .map(|e| e.subtypes.clone())
            .or_else(|| view.as_ref().map(|v| v.subtypes.clone()))
            .unwrap_or_default(),
        supertypes: ev
            .map(|e| e.supertypes)
            .or_else(|| view.as_ref().map(|v| v.supertypes))
            .unwrap_or_default(),
        colors: ev
            .map(|e| e.colors)
            .or_else(|| view.as_ref().map(|v| v.colors))
            .unwrap_or_default(),
        mana_value: ev
            .map(|e| e.mana_value)
            .or_else(|| view.as_ref().map(|v| v.mana_value))
            .unwrap_or(0),
        keywords: ev.map(|e| e.keywords.clone()).unwrap_or_default(),
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.map_or(0, |o| o.damage_marked),
        is_token: obj.is_some_and(|o| o.is_token()),
        is_commander: obj.is_some_and(|o| o.is_commander()),
        tapped: obj.is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED)),
        phased_out: obj.is_some_and(|o| o.is_phased_out()),
        face_down: obj.is_some_and(|o| o.is_face_down()),
        lost_all_abilities: ev.is_some_and(|e| e.lost_all_abilities),
        has_all_creature_types: ev.is_some_and(|e| e.has_all_creature_types),
        attached_ids: SmallVec::new(),
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.map_or(0, |o| o.active_face_index()),
    })
}

fn emit_target_and_crime_events(
    state: &mut GameState,
    caster: PlayerId,
    by_spell: bool,
    stack_item: Option<StackItemId>,
    targets: &[TargetRef],
) {
    let mut committed_crime = false;
    for &t in targets {
        let is_opponent_target = match t {
            TargetRef::Player { id } => id != caster,
            TargetRef::Object { id } => state.object(id).is_some_and(|o| {
                matches!(o.zone, Zone::Battlefield | Zone::Graveyard)
                    && o.controller != caster
                    && (o.zone != Zone::Graveyard || o.owner != caster)
            }),
            TargetRef::Stack { id } => state
                .stack_entries()
                .iter()
                .find(|e| e.id == id)
                .is_some_and(|e| e.controller != caster),
        };
        if is_opponent_target {
            committed_crime = true;
        }
        emit_event(
            state,
            AtomicEventKind::BecomesTarget {
                target: t,
                by_spell,
                controller: caster,
                stack_item,
            },
        );
    }
    if committed_crime {
        emit_event(
            state,
            AtomicEventKind::CommittedCrime {
                player: caster,
                stack_item,
            },
        );
    }
}

/// Resumes a casting-owned [`PendingPromptContext`] (modes, X, optional cost, cost object selection, branch choice).
pub fn resume_casting_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<(), EngineError> {
    let Some(prompt) = state.pending_prompt().cloned() else {
        return Err(EngineError::IllegalAction(
            "no active casting prompt".into(),
        ));
    };
    if prompt.chooser != actor || prompt.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "prompt actor or prompt_id mismatch".into(),
        ));
    }
    let PromptOwner::Casting(mut kind) = prompt.owner else {
        return Err(EngineError::IllegalAction(
            "active prompt is not a casting prompt".into(),
        ));
    };

    match &mut kind {
        CastingPromptKind::CastModes {
            min,
            max,
            allow_duplicates,
            option_count,
            picked,
        } => {
            let (legal_indices, can_done) = legal_casting_mode_choices(
                state,
                *min,
                *max,
                *allow_duplicates,
                *option_count,
                picked,
            );
            match answer {
                ChoiceAnswer::PickMode { index } => {
                    if index >= *option_count {
                        return Err(EngineError::IllegalAction(format!(
                            "mode index {index} out of range (0..{option_count})"
                        )));
                    }
                    if !*allow_duplicates && picked.contains(&index) {
                        return Err(EngineError::IllegalAction(format!(
                            "duplicate mode {index} not allowed"
                        )));
                    }
                    if !legal_indices.contains(&index) {
                        return Err(EngineError::IllegalAction(format!(
                            "mode index {index} has no legal targets or costs cannot be paid"
                        )));
                    }
                    picked.push(index);
                    if picked.len() >= usize::from(*max) {
                        let mut modes = picked.clone();
                        modes.sort_unstable();
                        state.take_pending_prompt();
                        if let Some(cs) = state.casting_mut() {
                            cs.chosen_modes = modes.into_iter().collect();
                            cs.step = CastStep::ChooseX;
                        }
                        state.set_decision(DecisionState::CastingSequence);
                        return advance_casting_state_machine(state);
                    }
                    let next_id = state.allocate_prompt_id();
                    let req = ChoiceRequest::ChooseMode {
                        prompt_id: next_id,
                        option_count: *option_count,
                        min: *min,
                        max: *max,
                    };
                    state.set_pending_prompt(Some(PendingPromptContext {
                        prompt_id: next_id,
                        chooser: actor,
                        request: req.clone(),
                        owner: PromptOwner::Casting(kind),
                    }));
                    state.set_decision(DecisionState::Prompt { request: req });
                    Ok(())
                }
                ChoiceAnswer::DonePicking if picked.len() >= usize::from(*min) && can_done => {
                    let mut modes = picked.clone();
                    modes.sort_unstable();
                    state.take_pending_prompt();
                    if let Some(cs) = state.casting_mut() {
                        cs.chosen_modes = modes.into_iter().collect();
                        cs.step = CastStep::ChooseX;
                    }
                    state.set_decision(DecisionState::CastingSequence);
                    advance_casting_state_machine(state)
                }
                _ => Err(EngineError::IllegalAction(
                    "invalid answer for CastModes prompt".into(),
                )),
            }
        }
        CastingPromptKind::CastXValue { min, max } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickNumber answer for X value".into(),
                ));
            };
            if value < *min || value > *max {
                return Err(EngineError::IllegalAction(format!(
                    "X value {value} out of allowed range [{min}, {max}]"
                )));
            }
            if let Some(cs) = state.casting()
                && !is_casting_x_value_valid_cached(state, cs, value as u32, state.evaluate_board())
            {
                return Err(EngineError::IllegalAction(format!(
                    "X value {value} cannot be paid or has no legal targets"
                )));
            }
            state.take_pending_prompt();
            if let Some(cs) = state.casting_mut() {
                cs.x_value = Some(value as u32);
                cs.step = CastStep::ChooseOptionalCosts;
            }
            state.set_decision(DecisionState::CastingSequence);
            advance_casting_state_machine(state)
        }
        CastingPromptKind::CastOptionalCost { cost_index, .. } => {
            let ChoiceAnswer::YesNo { value } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected YesNo answer for optional cost".into(),
                ));
            };
            if value {
                let can_pay = state.casting().is_some_and(|cs| {
                    can_pay_optional_cost_cached(state, cs, *cost_index, state.evaluate_board())
                });
                if !can_pay {
                    return Err(EngineError::IllegalAction(format!(
                        "cannot pay optional cost #{cost_index}"
                    )));
                }
            }
            state.take_pending_prompt();
            if let Some(cs) = state.casting_mut() {
                if value && !cs.optional_costs.contains(cost_index) {
                    cs.optional_costs.push(*cost_index);
                }
                cs.optional_cost_cursor = usize::from(*cost_index) + 1;
                cs.step = CastStep::ChooseOptionalCosts;
            }
            state.set_decision(DecisionState::CastingSequence);
            advance_casting_state_machine(state)
        }
        CastingPromptKind::CostObjectSelection {
            cost_index,
            kind: sel_kind,
            candidates,
            min,
            max,
            picked,
        } => {
            let done_now = match answer {
                ChoiceAnswer::PickObject { id } => {
                    if !candidates.contains(&id) || picked.contains(&id) {
                        return Err(EngineError::IllegalAction(format!(
                            "object {id:?} is not an unpicked legal cost candidate"
                        )));
                    }
                    picked.push(id);
                    picked.len() >= usize::from(*max)
                        || (picked.len() >= candidates.len() && picked.len() >= usize::from(*min))
                }
                ChoiceAnswer::DonePicking => {
                    if picked.len() < usize::from(*min) {
                        return Err(EngineError::IllegalAction(
                            "cannot finish cost object selection before minimum".into(),
                        ));
                    }
                    true
                }
                _ => {
                    return Err(EngineError::IllegalAction(
                        "expected PickObject or DonePicking for cost object selection".into(),
                    ));
                }
            };

            if !done_now {
                let next_id = state.allocate_prompt_id();
                let remaining_cands: Vec<ObjectId> = candidates
                    .iter()
                    .copied()
                    .filter(|id| !picked.contains(id))
                    .collect();
                let req = ChoiceRequest::ChooseObjects {
                    prompt_id: next_id,
                    candidates: remaining_cands,
                    min: *min,
                    max: *max,
                };
                state.set_pending_prompt(Some(PendingPromptContext {
                    prompt_id: next_id,
                    chooser: actor,
                    request: req.clone(),
                    owner: PromptOwner::Casting(CastingPromptKind::CostObjectSelection {
                        cost_index: *cost_index,
                        kind: *sel_kind,
                        candidates: candidates.clone(),
                        min: *min,
                        max: *max,
                        picked: picked.clone(),
                    }),
                }));
                state.set_decision(DecisionState::Prompt { request: req });
                return Ok(());
            }

            let final_picked = picked.clone();
            let c_idx = usize::from(*cost_index);
            state.take_pending_prompt();
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction(
                    "missing CastingState on cost selection resume".into(),
                ));
            };
            let Some(mut plan) = cs.payment_plan.take() else {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "missing payment plan on cost selection resume".into(),
                ));
            };
            let Some(cost_def) = plan.remaining_non_mana_costs.get(c_idx).cloned() else {
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction("invalid cost index".into()));
            };
            state.set_decision(DecisionState::CastingSequence);
            state.set_priority_player(cs.caster);
            let outcome = execute_non_mana_cost_step(
                state,
                cs.caster,
                cs.object,
                &cost_def,
                &final_picked,
                cs.x_value,
                Some(&mut plan),
            )?;
            match outcome {
                CostStepOutcome::Paid => {
                    plan.pending_step = None;
                    plan.non_mana_cursor = c_idx + 1;
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    state.set_decision(DecisionState::CastingSequence);
                    advance_casting_state_machine(state)
                }
                CostStepOutcome::Paused { .. } => {
                    if plan.pending_step.is_none() {
                        plan.pending_step = Some(PendingNonManaCostStep::WaitingCostPrompt);
                    }
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    Ok(())
                }
            }
        }
        CastingPromptKind::CostBranchChoice {
            cost_index,
            branches,
        } => {
            let ChoiceAnswer::PickMode { index } = answer else {
                return Err(EngineError::IllegalAction(
                    "expected PickMode for cost branch choice".into(),
                ));
            };
            let Some(chosen_branch) = branches.get(usize::from(index)).cloned() else {
                return Err(EngineError::IllegalAction(format!(
                    "branch index {index} out of range"
                )));
            };
            let Some(cs) = state.casting() else {
                return Err(EngineError::IllegalAction(
                    "missing CastingState on cost branch resume".into(),
                ));
            };
            let legal = legal_casting_cost_branch_indices_cached(
                state,
                cs,
                *cost_index,
                branches,
                state.evaluate_board(),
            );
            if !legal.contains(&index) {
                return Err(EngineError::IllegalAction(format!(
                    "cost branch {index} is not payable"
                )));
            }
            let c_idx = usize::from(*cost_index);
            state.take_pending_prompt();
            if let Some(cs) = state.casting_mut()
                && let Some(plan) = cs.payment_plan.as_deref_mut()
            {
                plan.remaining_non_mana_costs
                    .splice(c_idx..=c_idx, chosen_branch);
            }
            state.set_decision(DecisionState::CastingSequence);
            advance_casting_state_machine(state)
        }
        CastingPromptKind::CostPayLifeOrNot { .. } | CastingPromptKind::GuidedFallback { .. } => {
            state.take_pending_prompt();
            state.set_decision(DecisionState::CastingSequence);
            advance_casting_state_machine(state)
        }
    }
}

/// Resumes a standalone [`PromptOwner::CostPayment`] prompt.
pub fn resume_cost_payment_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<(), EngineError> {
    let Some(prompt) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction(
            "no active cost payment prompt".into(),
        ));
    };
    if prompt.chooser != actor || prompt.prompt_id != prompt_id {
        return Err(EngineError::IllegalAction(
            "prompt actor or prompt_id mismatch".into(),
        ));
    }
    let PromptOwner::CostPayment(CostPaymentPromptState { .. }) = &prompt.owner else {
        return Err(EngineError::IllegalAction(
            "active prompt is not CostPayment".into(),
        ));
    };
    match resume_cost_prompt(state, actor, prompt_id, answer)? {
        CostStepOutcome::Paid => {
            if let Some(cs) = state.casting_mut()
                && let Some(plan) = cs.payment_plan.as_deref_mut()
                && matches!(
                    plan.pending_step,
                    Some(PendingNonManaCostStep::WaitingCostPrompt)
                )
            {
                plan.pending_step = None;
                plan.non_mana_cursor += 1;
            }
            if state.casting().is_some() {
                return advance_casting_state_machine(state);
            }
            Ok(())
        }
        CostStepOutcome::Paused { .. } => Ok(()),
    }
}

fn current_divided_spec_cached(
    state: &GameState,
    cs: &CastingState,
    cache: &LayerCache,
) -> Option<(Vec<TargetRef>, u32, u32, usize)> {
    let specs = active_target_specs_cached(state, cs, cache);
    for (si, spec) in specs.iter().enumerate() {
        let Some(amt) = spec.divide_total.as_deref() else {
            continue;
        };
        let chosen = cs.targets_by_spec.get(si).map_or(&[][..], |v| v.as_slice());
        if chosen.is_empty() {
            continue;
        }
        let info = build_casting_target_info_cached(state, cs, si, cache);
        let total = info
            .with_eval_context_cached(cache, |ctx| evaluate_amount(state, amt, ctx))
            .max(0) as u32;
        let assigned: u32 = chosen
            .iter()
            .filter_map(|t| cs.divisions.iter().find(|(dt, _)| dt == t).map(|(_, a)| *a))
            .sum();
        let unassigned_count = chosen
            .iter()
            .filter(|t| !cs.divisions.iter().any(|(dt, _)| dt == *t))
            .count();
        if unassigned_count > 0 {
            return Some((chosen.to_vec(), total, assigned, unassigned_count));
        }
    }
    None
}

/// Enumerates legal actions when `DecisionState::CastingSequence` is active.
pub fn legal_casting_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(cs) = state.casting() else {
        return actions;
    };
    if cs.caster != player {
        return actions;
    }

    let cache = state.evaluate_board();
    match cs.step {
        CastStep::ChooseTargets => {
            let (specs, spec_mode_positions) =
                active_target_specs_with_modal_cached(state, cs, cache);
            if let Some(spec) = specs.get(cs.target_spec_cursor) {
                let info =
                    build_casting_target_info_cached(state, cs, cs.target_spec_cursor, cache);
                let picked = cs
                    .targets_by_spec
                    .get(cs.target_spec_cursor)
                    .map_or(&[][..], |v| v.as_slice());
                let raw_cands = legal_targets_for_spec_cached(state, spec, &info, picked, cache);
                let (min_req, max_req) = effective_target_bounds_cached(state, spec, &info, cache);
                let can_done_base = min_req <= max_req
                    && picked.len() >= usize::from(min_req)
                    && can_finish_spec_flagbearer_ok_cached(state, spec, &info, picked, cache);
                let (cands, can_done) = filter_casting_target_choices_with_modal_cached(
                    state,
                    &specs,
                    &info,
                    cs.target_spec_cursor,
                    picked,
                    raw_cands,
                    can_done_base,
                    spec_mode_positions.as_deref(),
                    cache,
                );
                for t in cands {
                    actions.push(Action::SelectTarget { target: t });
                }
                if can_done {
                    actions.push(Action::DoneTargets);
                }
            }
        }
        CastStep::ChooseDivisions => {
            if let Some((chosen, total, assigned, unassigned_count)) =
                current_divided_spec_cached(state, cs, cache)
                && let Some(&next_t) = chosen
                    .iter()
                    .find(|t| !cs.divisions.iter().any(|(dt, _)| dt == *t))
            {
                let rem = total.saturating_sub(assigned);
                let max_allowed = rem.saturating_sub((unassigned_count as u32).saturating_sub(1));
                for a in 1..=max_allowed {
                    actions.push(Action::AssignDivision {
                        target: next_t,
                        amount: a,
                    });
                }
            }
        }
        CastStep::PayCosts => {
            if let Some(plan) = cs.payment_plan.as_deref() {
                let spend_ctx = build_casting_spend_ctx_cached(state, cs, cache);
                let rem_non_mana = plan
                    .remaining_non_mana_costs
                    .get(plan.non_mana_cursor..)
                    .unwrap_or(&[]);
                let constraints = auto_tap_constraints_for_costs_cached(
                    state,
                    player,
                    cs.object,
                    rem_non_mana,
                    cs.x_value,
                    cache,
                );
                if constraints.is_satisfiable_cached(state, player, cache) {
                    let can_pay_pool = can_pay_mana_from_pool_with_life_reserve_cached(
                        state,
                        player,
                        &plan.remaining_mana,
                        &spend_ctx,
                        constraints.reserved_life,
                        cache,
                    );
                    if can_pay_pool {
                        actions.push(Action::PayFromPool);
                    } else {
                        if plan_auto_tap_with_constraints_cached(
                            state,
                            player,
                            &plan.remaining_mana,
                            &spend_ctx,
                            AutoTapPolicy::default(),
                            &constraints,
                            cache,
                        )
                        .can_pay
                        {
                            actions.push(Action::AutoPayMana);
                        }
                        for cand in enumerate_mana_abilities_cached(state, player, cache) {
                            if cand.only_as_instant
                                || !can_activate_mana_candidate_now_cached(
                                    state, player, &cand, cache,
                                )
                                || !constraints.allows_mana_candidate(state, &cand, player, cache)
                            {
                                continue;
                            }
                            let act = Action::TapMana {
                                object: cand.object,
                                ability_index: cand.ability_index,
                                preferred_color: None,
                            };
                            if !actions.contains(&act) {
                                actions.push(act);
                            }
                        }
                        // Cost aids (Convoke / Improvise / Delve / Emerge / Waterbend)
                        emit_cost_aid_actions_cached(
                            state,
                            player,
                            cs,
                            plan,
                            &constraints,
                            &mut actions,
                            cache,
                        );
                    }
                }
            }
        }
        _ => {}
    }

    if !matches!(cs.kind, CastKind::TriggeredAbility { .. }) {
        actions.push(Action::CancelCast);
    }
    actions.push(Action::Concede);
    actions
}

fn casting_object_has_keyword_cached(
    state: &GameState,
    cs: &CastingState,
    name: &str,
    cache: &LayerCache,
) -> bool {
    let Some(obj) = state.object(cs.object) else {
        return false;
    };
    let eff_def = state
        .rare(cs.object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let in_face = state
        .db()
        .get(eff_def)
        .and_then(|c| c.faces.get(usize::from(cs.face_idx)))
        .is_some_and(|face| find_keyword(face, state.db(), name).is_some());
    if in_face {
        return true;
    }
    cache.get(cs.object).is_some_and(|ev| {
        ev.keywords.iter().any(|&kw| {
            state
                .db()
                .keyword_name(kw)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    })
}

fn cost_aid_enabled_cached(
    state: &GameState,
    cs: &CastingState,
    plan: &PendingCostPaymentPlan,
    aid_kind: CostAidKind,
    cache: &LayerCache,
) -> bool {
    match cs.kind {
        CastKind::Spell => match aid_kind {
            CostAidKind::Convoke => casting_object_has_keyword_cached(state, cs, "convoke", cache),
            CostAidKind::Improvise => {
                casting_object_has_keyword_cached(state, cs, "improvise", cache)
                    && plan.remaining_mana.generic > 0
            }
            CostAidKind::Delve => {
                casting_object_has_keyword_cached(state, cs, "delve", cache)
                    && plan.remaining_mana.generic > 0
            }
            CostAidKind::Emerge => {
                cs.alt_kind == AltCastKind::Emerge && plan.emerge_sacrificed.is_none()
            }
            CostAidKind::Waterbend => {
                (casting_object_has_keyword_cached(state, cs, "waterbend", cache)
                    || plan
                        .remaining_non_mana_costs
                        .iter()
                        .any(|c| matches!(c, CostDef::Waterbend { .. })))
                    && plan.remaining_mana.generic > 0
            }
        },
        CastKind::ActivatedAbility { .. } => {
            aid_kind == CostAidKind::Waterbend
                && (casting_object_has_keyword_cached(state, cs, "waterbend", cache)
                    || plan
                        .remaining_non_mana_costs
                        .iter()
                        .any(|c| matches!(c, CostDef::Waterbend { .. })))
                && plan.remaining_mana.generic > 0
        }
        CastKind::TriggeredAbility { .. } => false,
    }
}

fn emit_cost_aid_actions_cached(
    state: &GameState,
    player: PlayerId,
    cs: &CastingState,
    plan: &PendingCostPaymentPlan,
    constraints: &AutoTapConstraints,
    actions: &mut Vec<Action>,
    cache: &LayerCache,
) {
    let Some(p) = state.player(player) else {
        return;
    };

    if cost_aid_enabled_cached(state, cs, plan, CostAidKind::Convoke, cache) {
        for &id in &p.battlefield {
            if !plan.convoke_tapped.contains(&id)
                && constraints.allows_tap(id)
                && state
                    .object(id)
                    .is_some_and(|o| !o.is_phased_out() && !o.flags.contains(ObjectFlags::TAPPED))
                && cache.get(id).is_some_and(|e| {
                    e.controller == player
                        && e.types.contains(CardTypeMask::CREATURE)
                        && convoke_can_reduce_mana(&plan.remaining_mana, e.colors)
                })
            {
                actions.push(Action::SelectCostAid {
                    aid_kind: CostAidKind::Convoke,
                    object: id,
                });
            }
        }
    }
    if cost_aid_enabled_cached(state, cs, plan, CostAidKind::Improvise, cache) {
        for &id in &p.battlefield {
            if !plan.improvise_tapped.contains(&id)
                && constraints.allows_tap(id)
                && state
                    .object(id)
                    .is_some_and(|o| !o.is_phased_out() && !o.flags.contains(ObjectFlags::TAPPED))
                && cache.get(id).is_some_and(|e| {
                    e.controller == player && e.types.contains(CardTypeMask::ARTIFACT)
                })
            {
                actions.push(Action::SelectCostAid {
                    aid_kind: CostAidKind::Improvise,
                    object: id,
                });
            }
        }
    }
    if cost_aid_enabled_cached(state, cs, plan, CostAidKind::Delve, cache) {
        let rem_non_mana = plan
            .remaining_non_mana_costs
            .get(plan.non_mana_cursor..)
            .unwrap_or(&[]);
        let reserved_gy = delve_reserved_gy_cards_cached(state, player, rem_non_mana, cache);
        if p.graveyard.len().saturating_sub(1) >= reserved_gy {
            for &id in &p.graveyard {
                if !plan.delve_exiled.contains(&id)
                    && state
                        .object(id)
                        .is_some_and(|o| o.zone == Zone::Graveyard && o.owner == player)
                {
                    actions.push(Action::SelectCostAid {
                        aid_kind: CostAidKind::Delve,
                        object: id,
                    });
                }
            }
        }
    }
    if cost_aid_enabled_cached(state, cs, plan, CostAidKind::Emerge, cache) {
        for &id in &p.battlefield {
            if constraints.allows_leave_battlefield(id)
                && state.object(id).is_some_and(|o| !o.is_phased_out())
                && cache.get(id).is_some_and(|e| {
                    e.controller == player && e.types.contains(CardTypeMask::CREATURE)
                })
                && !cant_sacrifice_object_cached(state, player, id, false, cache)
            {
                actions.push(Action::SelectCostAid {
                    aid_kind: CostAidKind::Emerge,
                    object: id,
                });
            }
        }
    }
    if cost_aid_enabled_cached(state, cs, plan, CostAidKind::Waterbend, cache) {
        for &id in &p.battlefield {
            if !plan.waterbend_tapped.contains(&id)
                && constraints.allows_tap(id)
                && state
                    .object(id)
                    .is_some_and(|o| !o.is_phased_out() && !o.flags.contains(ObjectFlags::TAPPED))
                && cache.get(id).is_some_and(|e| {
                    e.controller == player
                        && e.types
                            .intersects(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
                })
            {
                actions.push(Action::SelectCostAid {
                    aid_kind: CostAidKind::Waterbend,
                    object: id,
                });
            }
        }
    }
}

/// Applies an [`Action`] during `DecisionState::CastingSequence`.
pub fn apply_casting_action(
    state: &mut GameState,
    player: PlayerId,
    action: &Action,
) -> Result<(), EngineError> {
    let Some(cs_ref) = state.casting() else {
        return Err(EngineError::IllegalAction("no active CastingState".into()));
    };
    if cs_ref.caster != player {
        return Err(EngineError::IllegalAction(
            "casting sequence controlled by another player".into(),
        ));
    }

    match action {
        Action::SelectTarget { target } => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::ChooseTargets {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "not currently in ChooseTargets step".into(),
                ));
            }
            let specs_cur = cs.target_spec_cursor;
            let max_req = {
                let cache = state.evaluate_board();
                let (specs, spec_mode_positions) =
                    active_target_specs_with_modal_cached(state, &cs, cache);
                let Some(spec) = specs.get(specs_cur) else {
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "target spec cursor out of bounds".into(),
                    ));
                };
                if cs.targets_by_spec.len() < specs.len() {
                    cs.targets_by_spec.resize(specs.len(), Vec::new());
                }
                let info = build_casting_target_info_cached(state, &cs, specs_cur, cache);
                if !is_target_legal_for_spec_cached(
                    state,
                    spec,
                    &info,
                    &cs.targets_by_spec[specs_cur],
                    *target,
                    cache,
                ) {
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(format!(
                        "illegal target {target:?} for spec {specs_cur}"
                    )));
                }
                if let Some(positions) = spec_mode_positions.as_deref()
                    && let TargetRef::Player { id } = *target
                {
                    let cur_pos = positions.get(specs_cur).copied().unwrap_or(u16::MAX);
                    if cur_pos != u16::MAX {
                        let dup_player = cs.targets_by_spec.iter().take(specs_cur).enumerate().any(
                            |(pi, pts)| {
                                let p_pos = positions.get(pi).copied().unwrap_or(u16::MAX);
                                p_pos != u16::MAX
                                    && p_pos != cur_pos
                                    && pts.contains(&TargetRef::Player { id })
                            },
                        );
                        if dup_player {
                            state.set_casting(Some(cs));
                            return Err(EngineError::IllegalAction(format!(
                                "illegal target {target:?} for spec {specs_cur}: player already targeted by another mode"
                            )));
                        }
                    }
                }
                let mut next_picked = Vec::with_capacity(cs.targets_by_spec[specs_cur].len() + 1);
                next_picked.extend_from_slice(&cs.targets_by_spec[specs_cur]);
                next_picked.push(*target);
                if !partial_targets_can_complete_with_modal_cached(
                    state,
                    &specs,
                    &info,
                    specs_cur,
                    &next_picked,
                    false,
                    spec_mode_positions.as_deref(),
                    cache,
                ) {
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(format!(
                        "target {target:?} for spec {specs_cur} leaves later target specs unsatisfiable"
                    )));
                }
                effective_target_bounds_cached(state, spec, &info, cache).1
            };
            let cur = specs_cur;
            cs.targets_by_spec[cur].push(*target);
            if cs.targets_by_spec[cur].len() >= usize::from(max_req) {
                cs.target_spec_cursor += 1;
            }
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::DoneTargets => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::ChooseTargets {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "not currently in ChooseTargets step".into(),
                ));
            }
            let cur = cs.target_spec_cursor;
            let (flagbearer_ok, can_complete) = {
                let cache = state.evaluate_board();
                let (specs, spec_mode_positions) =
                    active_target_specs_with_modal_cached(state, &cs, cache);
                let Some(spec) = specs.get(cur) else {
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "target spec cursor out of bounds".into(),
                    ));
                };
                let info = build_casting_target_info_cached(state, &cs, cur, cache);
                let (min_req, _) = effective_target_bounds_cached(state, spec, &info, cache);
                let picked = cs
                    .targets_by_spec
                    .get(cur)
                    .map_or(&[][..], |v| v.as_slice());
                if picked.len() < usize::from(min_req) {
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "minimum target count not yet selected".into(),
                    ));
                }
                let fb_ok = can_finish_spec_flagbearer_ok_cached(state, spec, &info, picked, cache);
                let comp_ok = fb_ok
                    && partial_targets_can_complete_with_modal_cached(
                        state,
                        &specs,
                        &info,
                        cur,
                        picked,
                        true,
                        spec_mode_positions.as_deref(),
                        cache,
                    );
                (fb_ok, comp_ok)
            };
            if !flagbearer_ok {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "must choose a legal Flagbearer target".into(),
                ));
            }
            if !can_complete {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "cannot finish targets here because later target specs would become unsatisfiable"
                        .into(),
                ));
            }
            cs.target_spec_cursor += 1;
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::AssignDivision { target, amount } => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::ChooseDivisions {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "not currently in ChooseDivisions step".into(),
                ));
            }
            let Some((chosen, total, assigned, unassigned_count)) =
                current_divided_spec_cached(state, &cs, state.evaluate_board())
            else {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "no pending divided target spec".into(),
                ));
            };
            if !chosen.contains(target) {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "division target is not in current divided target spec".into(),
                ));
            }
            if cs.divisions.iter().any(|(dt, _)| dt == target) {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "division already assigned for target".into(),
                ));
            }
            let rem = total.saturating_sub(assigned);
            let max_allowed = rem.saturating_sub((unassigned_count as u32).saturating_sub(1));
            if *amount < 1 || *amount > max_allowed {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(format!(
                    "division amount {amount} must be in 1..={max_allowed}"
                )));
            }
            cs.divisions.push((*target, *amount));
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::ToggleOptionalCost { index } => {
            if cs_ref.step != CastStep::ChooseOptionalCosts {
                return Err(EngineError::IllegalAction(
                    "ToggleOptionalCost only valid in ChooseOptionalCosts step".into(),
                ));
            }
            {
                let cache = state.evaluate_board();
                let valid = collect_optional_cost_indices(state, cs_ref, cache);
                if !valid.iter().any(|(i, _)| i == index) {
                    return Err(EngineError::IllegalAction(format!(
                        "invalid optional cost index {index}"
                    )));
                }
                if !cs_ref.optional_costs.contains(index)
                    && !can_pay_optional_cost_cached(state, cs_ref, *index, cache)
                {
                    return Err(EngineError::IllegalAction(format!(
                        "cannot pay optional cost #{index}"
                    )));
                }
            }
            if let Some(cs) = state.casting_mut() {
                if let Some(pos) = cs.optional_costs.iter().position(|x| x == index) {
                    cs.optional_costs.remove(pos);
                } else {
                    cs.optional_costs.push(*index);
                }
            }
            Ok(())
        }
        Action::DoneOptionalCosts => {
            if cs_ref.step != CastStep::ChooseOptionalCosts {
                return Err(EngineError::IllegalAction(
                    "DoneOptionalCosts only valid in ChooseOptionalCosts step".into(),
                ));
            }
            if let Some(cs) = state.casting_mut() {
                cs.step = CastStep::ChooseTargets;
            }
            advance_casting_state_machine(state)
        }
        Action::TapMana {
            object,
            ability_index,
            preferred_color,
        } => {
            if cs_ref.step != CastStep::PayCosts {
                return Err(EngineError::IllegalAction(
                    "mana abilities during casting can only be tapped in PayCosts".into(),
                ));
            }
            {
                let cache = state.evaluate_board();
                let rem_non_mana = cs_ref
                    .payment_plan
                    .as_deref()
                    .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                    .unwrap_or(&[]);
                let constraints = auto_tap_constraints_for_costs_cached(
                    state,
                    player,
                    cs_ref.object,
                    rem_non_mana,
                    cs_ref.x_value,
                    cache,
                );
                let cands = enumerate_mana_abilities_cached(state, player, cache);
                if let Some(cand) = cands
                    .iter()
                    .find(|c| c.object == *object && c.ability_index == *ability_index)
                {
                    if cand.only_as_instant {
                        return Err(EngineError::IllegalAction(
                            "cannot activate OnlyAsInstant mana ability during casting".into(),
                        ));
                    }
                    if !constraints.allows_mana_candidate(state, cand, player, cache) {
                        return Err(EngineError::IllegalAction(
                            "mana ability conflicts with pending non-mana costs".into(),
                        ));
                    }
                }
            }
            match activate_mana_ability(state, player, *object, *ability_index, *preferred_color)? {
                ManaActivationOutcome::Produced => {
                    if let Some(cs) = state.casting_mut()
                        && !cs.paid_mana_sources.contains(object)
                    {
                        cs.paid_mana_sources.push(*object);
                    }
                    advance_casting_state_machine(state)
                }
                ManaActivationOutcome::Paused { .. } => Ok(()),
            }
        }
        Action::PayFromPool => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::PayCosts {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "PayFromPool only valid in PayCosts step".into(),
                ));
            }
            let Some(mut plan) = cs.payment_plan.take() else {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction("missing payment plan".into()));
            };
            let spend_ctx = {
                let cache = state.evaluate_board();
                let spend_ctx = build_casting_spend_ctx_cached(state, &cs, cache);
                let rem_non_mana = plan
                    .remaining_non_mana_costs
                    .get(plan.non_mana_cursor..)
                    .unwrap_or(&[]);
                let constraints = auto_tap_constraints_for_costs_cached(
                    state,
                    player,
                    cs.object,
                    rem_non_mana,
                    cs.x_value,
                    cache,
                );
                if !constraints.is_satisfiable_cached(state, player, cache) {
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "non-mana cost constraints are unsatisfiable".into(),
                    ));
                }
                if constraints.reserved_life > 0
                    && !can_pay_mana_from_pool_with_life_reserve_cached(
                        state,
                        player,
                        &plan.remaining_mana,
                        &spend_ctx,
                        constraints.reserved_life,
                        cache,
                    )
                {
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "cannot pay mana without spending life reserved for non-mana costs".into(),
                    ));
                }
                spend_ctx
            };
            let ded = deduct_mana_for_context(
                state,
                player,
                &plan.remaining_mana,
                &spend_ctx,
                &plan.phyrexian_pay_life,
            )?;
            cs.paid_mana_colors |= ded.spent_colors;
            cs.mana_spent_total = cs.mana_spent_total.saturating_add(ded.total_spent);
            if ded.cant_be_countered_from_mana {
                cs.cant_be_countered_from_mana = true;
                plan.cant_be_countered_from_mana = true;
            }
            if ded.commander_bonus_counters > 0 {
                cs.mana_commander_bonus_counters = cs
                    .mana_commander_bonus_counters
                    .saturating_add(ded.commander_bonus_counters);
                plan.mana_commander_bonus_counters = plan
                    .mana_commander_bonus_counters
                    .saturating_add(ded.commander_bonus_counters);
            }
            plan.remaining_mana = ManaCost::default();
            cs.payment_plan = Some(plan);
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::AutoPayMana => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::PayCosts {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "AutoPayMana only valid in PayCosts step".into(),
                ));
            }
            let Some(mut plan) = cs.payment_plan.take() else {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction("missing payment plan".into()));
            };
            let auto_pay_res = {
                let cache = state.evaluate_board();
                let spend_ctx = build_casting_spend_ctx_cached(state, &cs, cache);
                let rem_non_mana = plan
                    .remaining_non_mana_costs
                    .get(plan.non_mana_cursor..)
                    .unwrap_or(&[]);
                let constraints = auto_tap_constraints_for_costs_cached(
                    state,
                    player,
                    cs.object,
                    rem_non_mana,
                    cs.x_value,
                    cache,
                );
                if !constraints.is_satisfiable_cached(state, player, cache) {
                    Err("non-mana cost constraints are unsatisfiable")
                } else if can_pay_mana_from_pool_with_life_reserve_cached(
                    state,
                    player,
                    &plan.remaining_mana,
                    &spend_ctx,
                    constraints.reserved_life,
                    cache,
                ) {
                    Ok((spend_ctx, None))
                } else {
                    let tap_plan = plan_auto_tap_with_constraints_cached(
                        state,
                        player,
                        &plan.remaining_mana,
                        &spend_ctx,
                        AutoTapPolicy::default(),
                        &constraints,
                        cache,
                    );
                    Ok((spend_ctx, Some(tap_plan)))
                }
            };
            let (spend_ctx, tap_plan_opt) = match auto_pay_res {
                Ok(pair) => pair,
                Err(msg) => {
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(msg.into()));
                }
            };
            if let Some(tap_plan) = tap_plan_opt {
                if !tap_plan.can_pay {
                    cs.payment_plan = Some(plan);
                    state.set_casting(Some(cs));
                    return Err(EngineError::IllegalAction(
                        "insufficient mana to cast spell or activate ability".into(),
                    ));
                }
                for &src in &tap_plan.sources_to_tap {
                    if !cs.paid_mana_sources.contains(&src) {
                        cs.paid_mana_sources.push(src);
                    }
                }
                plan.phyrexian_pay_life = tap_plan.phyrexian_life_flags.clone();
                execute_auto_tap_plan(state, player, &tap_plan)?;
            }
            let ded = deduct_mana_for_context(
                state,
                player,
                &plan.remaining_mana,
                &spend_ctx,
                &plan.phyrexian_pay_life,
            )?;
            cs.paid_mana_colors |= ded.spent_colors;
            cs.mana_spent_total = cs.mana_spent_total.saturating_add(ded.total_spent);
            if ded.cant_be_countered_from_mana {
                cs.cant_be_countered_from_mana = true;
                plan.cant_be_countered_from_mana = true;
            }
            if ded.commander_bonus_counters > 0 {
                cs.mana_commander_bonus_counters = cs
                    .mana_commander_bonus_counters
                    .saturating_add(ded.commander_bonus_counters);
                plan.mana_commander_bonus_counters = plan
                    .mana_commander_bonus_counters
                    .saturating_add(ded.commander_bonus_counters);
            }
            plan.remaining_mana = ManaCost::default();
            cs.payment_plan = Some(plan);
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::SelectCostAid { aid_kind, object } => {
            let Some(mut cs) = state.take_casting() else {
                return Err(EngineError::IllegalAction("no casting state".into()));
            };
            if cs.step != CastStep::PayCosts {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(
                    "SelectCostAid only valid in PayCosts step".into(),
                ));
            }
            let Some(mut plan) = cs.payment_plan.take() else {
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction("missing payment plan".into()));
            };
            if !cost_aid_enabled_cached(state, &cs, &plan, *aid_kind, state.evaluate_board()) {
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                return Err(EngineError::IllegalAction(format!(
                    "cost aid {aid_kind:?} is not enabled for this spell or ability"
                )));
            }
            let constraints = {
                let cache = state.evaluate_board();
                let rem_non_mana = plan
                    .remaining_non_mana_costs
                    .get(plan.non_mana_cursor..)
                    .unwrap_or(&[]);
                auto_tap_constraints_for_costs_cached(
                    state,
                    player,
                    cs.object,
                    rem_non_mana,
                    cs.x_value,
                    cache,
                )
            };
            if let Err(err) = apply_cost_aid_with_constraints(
                state,
                player,
                &mut plan,
                *aid_kind,
                *object,
                &constraints,
            ) {
                cs.payment_plan = Some(plan);
                state.set_casting(Some(cs));
                return Err(err);
            }
            cs.payment_plan = Some(plan);
            state.set_casting(Some(cs));
            advance_casting_state_machine(state)
        }
        Action::CancelCast => cancel_active_cast(state, player),
        _ => Err(EngineError::IllegalAction(format!(
            "action {action:?} is not valid during CastingSequence"
        ))),
    }
}

pub fn cancel_active_cast(state: &mut GameState, player: PlayerId) -> Result<(), EngineError> {
    let Some(cs) = state.take_casting() else {
        return Err(EngineError::IllegalAction(
            "no active cast to cancel".into(),
        ));
    };
    if cs.caster != player {
        state.set_casting(Some(cs));
        return Err(EngineError::IllegalAction(
            "only caster can cancel cast".into(),
        ));
    }
    if matches!(cs.kind, CastKind::TriggeredAbility { .. }) {
        state.set_casting(Some(cs));
        return Err(EngineError::IllegalAction(
            "cannot cancel triggered ability targeting".into(),
        ));
    }
    state.take_pending_prompt();
    if cs.kind == CastKind::Spell {
        let owner = state.object(cs.object).map_or(player, |o| o.owner);
        let _ = move_object_raw(
            state,
            cs.object,
            cs.from_zone,
            MoveRawOptions {
                to_player: Some(owner),
                bump_generation: Some(false),
                capture_lki: Some(false),
                ..Default::default()
            },
            None,
        );
        if let Some(rare) = state.rare_mut(cs.object) {
            rare.alt_cast_kind = AltCastKind::Normal;
            rare.bargained = false;
            rare.additional_cost_paid = false;
            rare.kicked_tags.clear();
        }
        if cs.from_zone == Zone::Exile
            && let Some(ex) = cs.pre_cast_exile
        {
            if let Some(o) = state.object_mut(cs.object) {
                if ex.foretold {
                    o.flags.insert(ObjectFlags::FORETOLD);
                }
                if ex.was_face_down {
                    o.flags.insert(ObjectFlags::FACE_DOWN);
                }
                if ex.is_suspended {
                    o.flags.insert(ObjectFlags::IS_SUSPENDED);
                }
                if ex.unearth_leave_exile {
                    o.flags.insert(ObjectFlags::UNEARTH_LEAVE_EXILE);
                }
            }
            if (ex.face_down.is_some()
                || !ex.known_to_players.is_empty()
                || ex.exiled_by.is_some()
                || ex.playable_from_exile_by.is_some()
                || ex.plotted_by.is_some()
                || ex.plotted_turn.is_some())
                && let Some(rare) = state.rare_or_default(cs.object)
            {
                rare.face_down = ex.face_down;
                rare.known_to_players = ex.known_to_players;
                rare.exiled_by = ex.exiled_by;
                rare.playable_from_exile_by = ex.playable_from_exile_by;
                rare.plotted_by = ex.plotted_by;
                rare.plotted_turn = ex.plotted_turn;
            }
        }
        state.remove_rare_if_default(cs.object);
    }
    if let Some(plan) = cs.payment_plan {
        for id in plan
            .convoke_tapped
            .into_iter()
            .chain(plan.improvise_tapped)
            .chain(plan.waterbend_tapped)
        {
            if let Some(o) = state.object_mut(id) {
                o.flags.remove(ObjectFlags::TAPPED);
            }
        }
    }
    state.clear_pending_mana_delayed_riders();
    let mut dirty = DirtyMask::TOPOLOGY | DirtyMask::TAPPED;
    if cs.from_zone == Zone::Exile {
        dirty |= DirtyMask::EXILE;
    }
    state.mark_dirty(dirty);
    state.set_decision(DecisionState::Priority);
    state.set_priority_player(player);
    Ok(())
}

/// Atomically executes `MacroAction::CastSpell` using clone-and-rollback on any error (`t2_f12_03`, `t1_f12_05`),
/// while preserving `DecisionState::Prompt` if a non-mana cost (e.g. `Sacrifice` in `t1_f11_03`) requires object choice.
pub fn apply_macro_cast_spell(
    state: &mut GameState,
    player: PlayerId,
    macro_act: &MacroAction,
) -> Result<(), EngineError> {
    let MacroAction::CastSpell {
        object,
        option_id,
        from_zone,
        modes,
        x_value,
        targets,
    } = macro_act
    else {
        return Err(EngineError::IllegalAction(
            "expected MacroAction::CastSpell".into(),
        ));
    };

    let saved = state.clone();
    let res = (|| -> Result<(), EngineError> {
        begin_cast_spell(state, player, *object, *option_id, *from_zone)?;

        let mut mode_cursor = 0usize;
        let mut target_cursor = 0usize;
        let mut x_applied = false;
        let mut guard = 0usize;

        while matches!(
            state.decision(),
            DecisionState::CastingSequence | DecisionState::Prompt { .. }
        ) {
            guard += 1;
            if guard > 64 {
                return Err(EngineError::IllegalAction(
                    "MacroAction::CastSpell exceeded step limit".into(),
                ));
            }
            if let Some(prompt) = state.pending_prompt().cloned() {
                match prompt.owner {
                    PromptOwner::Casting(CastingPromptKind::CastModes { min, .. }) => {
                        if let Some(&m_idx) = modes.get(mode_cursor) {
                            mode_cursor += 1;
                            resume_casting_prompt(
                                state,
                                prompt.chooser,
                                prompt.prompt_id,
                                ChoiceAnswer::PickMode { index: m_idx },
                            )?;
                        } else if mode_cursor >= usize::from(min) {
                            resume_casting_prompt(
                                state,
                                prompt.chooser,
                                prompt.prompt_id,
                                ChoiceAnswer::DonePicking,
                            )?;
                        } else {
                            return Err(EngineError::IllegalAction(
                                "MacroAction::CastSpell missing required mode selection".into(),
                            ));
                        }
                    }
                    PromptOwner::Casting(CastingPromptKind::CastXValue { .. }) => {
                        if x_applied {
                            return Err(EngineError::IllegalAction(
                                "unexpected duplicate X value prompt".into(),
                            ));
                        }
                        let xv = i32::from(x_value.unwrap_or(0));
                        x_applied = true;
                        resume_casting_prompt(
                            state,
                            prompt.chooser,
                            prompt.prompt_id,
                            ChoiceAnswer::PickNumber { value: xv },
                        )?;
                    }
                    PromptOwner::Casting(CastingPromptKind::CostObjectSelection { .. }) => {
                        // Return paused in Prompt so interactive caller/driver answers (t1_f11_03).
                        break;
                    }
                    _ => break,
                }
            } else if let Some(cs) = state.casting() {
                match cs.step {
                    CastStep::ChooseTargets => {
                        if let Some(&t) = targets.get(target_cursor) {
                            target_cursor += 1;
                            apply_casting_action(
                                state,
                                player,
                                &Action::SelectTarget { target: t },
                            )?;
                        } else {
                            apply_casting_action(state, player, &Action::DoneTargets)?;
                        }
                    }
                    CastStep::ChooseDivisions => break,
                    CastStep::PayCosts => {
                        apply_casting_action(state, player, &Action::AutoPayMana)?;
                    }
                    _ => {
                        advance_casting_state_machine(state)?;
                    }
                }
            } else {
                break;
            }
        }
        let (past_modes, past_targets) = match state.casting() {
            None => (true, true),
            Some(cs) => (
                !matches!(cs.step, CastStep::ChooseModes),
                matches!(
                    cs.step,
                    CastStep::ChooseDivisions | CastStep::PayCosts | CastStep::Complete
                ),
            ),
        };
        if (past_modes && mode_cursor < modes.len())
            || (past_targets && target_cursor < targets.len())
        {
            return Err(EngineError::IllegalAction(
                "extraneous modes or targets in MacroAction::CastSpell".into(),
            ));
        }
        Ok(())
    })();

    if let Err(err) = res {
        *state = saved;
        return Err(err);
    }
    Ok(())
}

/// Executes `Action::PlayLand` (`CR 305.1`), recording `pending_land_play` if ETB replacements pause (Shocklands).
pub fn apply_play_land(
    state: &mut GameState,
    player: PlayerId,
    object: ObjectId,
    from_zone: Zone,
    face_index: u8,
) -> Result<(), EngineError> {
    if !can_play_land(state, player, object, from_zone, face_index) {
        return Err(EngineError::IllegalAction(format!(
            "cannot play land {object:?} (face {face_index}) from {from_zone:?}"
        )));
    }
    if let Some(p) = state.player_mut(player) {
        p.lands_played_this_turn = p.lands_played_this_turn.saturating_add(1);
    }
    let is_transform = state
        .object(object)
        .and_then(|o| state.db().get(o.card_def))
        .is_some_and(|c| c.layout == CardLayout::Transform);
    if let Some(o) = state.object_mut(object) {
        o.active_face_index = face_index;
    }
    state.mark_dirty(DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY);

    let mv = move_object(
        state,
        object,
        Zone::Battlefield,
        MoveRawOptions {
            controller: Some(player),
            entered_transformed: is_transform && face_index == 1,
            ..Default::default()
        },
        None,
    )?;

    match mv {
        MoveObjectOutcome::Moved(raw) => {
            let land_id = raw.new_id.unwrap_or(object);
            if let Some(o) = state.object_mut(land_id) {
                o.active_face_index = face_index;
            }
            state.mark_dirty(DirtyMask::TOPOLOGY);
            emit_event(
                state,
                AtomicEventKind::LandPlayed {
                    player,
                    object: land_id,
                    from_zone,
                },
            );
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
        }
        MoveObjectOutcome::Paused { .. } => {
            state.set_pending_land_play(Some((player, from_zone)));
        }
        MoveObjectOutcome::Skipped { .. } | MoveObjectOutcome::ExecuteInstead { .. } => {}
    }
    Ok(())
}

/// Executes `Action::SpecialAction { kind }` (`UnlockRoomDoor`, `TurnFaceUp`, `Foretell`, `Plot`, etc.).
pub fn apply_special_action(
    state: &mut GameState,
    player: PlayerId,
    kind: SpecialActionKind,
) -> Result<(), EngineError> {
    match kind {
        SpecialActionKind::UnlockRoomDoor { object, face_index } => {
            if state.active_player() != player
                || !matches!(
                    state.phase(),
                    mtg_ir::GamePhase::Main1 | mtg_ir::GamePhase::Main2
                )
                || !state.stack().is_empty()
            {
                return Err(EngineError::IllegalAction(
                    "UnlockRoomDoor requires sorcery speed".into(),
                ));
            }
            let Some(obj) = state.object(object) else {
                return Err(EngineError::IllegalAction("object does not exist".into()));
            };
            let ctrl = state
                .evaluate_board()
                .get(object)
                .map_or(obj.controller, |e| e.controller);
            if ctrl != player
                || obj.zone != Zone::Battlefield
                || obj.is_phased_out()
                || obj.is_face_down()
            {
                return Err(EngineError::IllegalAction(
                    "must control phased-in face-up Room permanent on battlefield".into(),
                ));
            }
            let eff_def = state
                .rare(object)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            if !is_room_card(state.db(), eff_def) {
                return Err(EngineError::IllegalAction(
                    "object is not a Room card".into(),
                ));
            }
            if state
                .rare(object)
                .is_some_and(|r| r.unlocked_faces.contains(&face_index))
            {
                return Err(EngineError::IllegalAction(format!(
                    "Room door {face_index} is already unlocked"
                )));
            }
            let door_cost = state
                .db()
                .get(eff_def)
                .and_then(|c| c.faces.get(usize::from(face_index)))
                .map(|f| f.characteristics.mana_cost.clone())
                .ok_or_else(|| {
                    EngineError::IllegalAction(format!("invalid Room door {face_index}"))
                })?;

            pay_special_action_mana(
                state,
                player,
                object,
                face_index,
                Zone::Battlefield,
                ManaSpendContextKind::SpecialAction,
                &door_cost,
            )?;
            let all_unlocked = unlock_room_door(state, object, face_index)?;
            emit_event(
                state,
                AtomicEventKind::DoorUnlocked {
                    player,
                    object,
                    face_index,
                },
            );
            if all_unlocked {
                emit_event(state, AtomicEventKind::RoomFullyUnlocked { player, object });
            }
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            Ok(())
        }
        SpecialActionKind::TurnFaceUp { object } => {
            let Some(obj) = state.object(object) else {
                return Err(EngineError::IllegalAction("object does not exist".into()));
            };
            let cache = state.evaluate_board();
            let ctrl = cache.get(object).map_or(obj.controller, |e| e.controller);
            if ctrl != player || obj.zone != Zone::Battlefield || obj.is_phased_out() {
                return Err(EngineError::IllegalAction(
                    "must control phased-in face-down permanent".into(),
                ));
            }
            if !can_turn_face_up(state, object) {
                return Err(EngineError::IllegalAction(
                    "permanent cannot be turned face up".into(),
                ));
            }
            let active_fi = obj.active_face_index();
            let fd = state
                .rare(object)
                .and_then(|r| r.face_down.as_deref())
                .cloned()
                .unwrap_or_default();
            if !can_pay_non_mana_costs_cached(
                state,
                player,
                object,
                &fd.turn_up_cost_defs,
                None,
                cache,
            ) {
                return Err(EngineError::IllegalAction(
                    "cannot pay turn-up non-mana costs".into(),
                ));
            }
            if let Some(cost) = &fd.turn_up_mana_cost {
                pay_special_action_mana(
                    state,
                    player,
                    object,
                    active_fi,
                    Zone::Battlefield,
                    ManaSpendContextKind::SpecialAction,
                    cost,
                )?;
            }
            for c in &fd.turn_up_cost_defs {
                let _ = execute_non_mana_cost_step(state, player, object, c, &[], None, None)?;
            }
            turn_face_up(state, object)?;
            emit_event(
                state,
                AtomicEventKind::TurnedFaceUp {
                    object,
                    player,
                    x_value: None,
                },
            );
            flush_triggers(state);
            let _ = prepare_next_untargeted_stack_trigger(state)?;
            Ok(())
        }
        SpecialActionKind::Foretell { object } => {
            if state.active_player() != player {
                return Err(EngineError::IllegalAction(
                    "Foretell only allowed during your turn".into(),
                ));
            }
            let Some(obj) = state.object(object) else {
                return Err(EngineError::IllegalAction("object does not exist".into()));
            };
            if obj.owner != player || obj.zone != Zone::Hand {
                return Err(EngineError::IllegalAction(
                    "Foretell target must be in your hand".into(),
                ));
            }
            let Some(granted_cast_cost) = hand_foretell_cost(state, player, object) else {
                return Err(EngineError::IllegalAction(
                    "card in hand does not have Foretell".into(),
                ));
            };
            let eff_def = state
                .rare(object)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            let has_innate_foretell = state
                .db()
                .get(eff_def)
                .and_then(|c| c.faces.first())
                .is_some_and(|f0| find_keyword(f0, state.db(), "foretell").is_some());
            let cost = ManaCost::parse("{2}");
            pay_special_action_mana(
                state,
                player,
                object,
                0,
                Zone::Hand,
                ManaSpendContextKind::ForetellAction,
                &cost,
            )?;
            let fd_spec = FaceDownSpec {
                kind: FaceDownKind::Foretell,
                known_to: SmallVec::from_slice(&[player]),
                turn_up_mana_cost: if has_innate_foretell {
                    None
                } else {
                    Some(granted_cast_cost)
                },
                ..Default::default()
            };
            let mv = move_object(
                state,
                object,
                Zone::Exile,
                MoveRawOptions {
                    enter_face_down: Some(fd_spec.clone()),
                    ..Default::default()
                },
                None,
            )?;
            if let MoveObjectOutcome::Moved(raw) = mv {
                let new_id = raw.new_id.unwrap_or(raw.old_id);
                if let Some(o) = state.object_mut(new_id) {
                    o.flags.insert(
                        ObjectFlags::FACE_DOWN
                            | ObjectFlags::FORETOLD
                            | ObjectFlags::ENTERED_THIS_TURN,
                    );
                }
                if let Some(rare) = state.rare_or_default(new_id) {
                    rare.face_down = Some(Arc::new(fd_spec));
                }
                state.mark_dirty(DirtyMask::TOPOLOGY);
                emit_event(
                    state,
                    AtomicEventKind::Foretell {
                        player,
                        object: new_id,
                    },
                );
                flush_triggers(state);
                let _ = prepare_next_untargeted_stack_trigger(state)?;
            }
            Ok(())
        }
        SpecialActionKind::Plot { object } => {
            if state.active_player() != player
                || !matches!(
                    state.phase(),
                    mtg_ir::GamePhase::Main1 | mtg_ir::GamePhase::Main2
                )
                || !state.stack().is_empty()
            {
                return Err(EngineError::IllegalAction(
                    "Plot requires sorcery speed".into(),
                ));
            }
            let Some(obj) = state.object(object) else {
                return Err(EngineError::IllegalAction("object does not exist".into()));
            };
            if obj.owner != player || obj.zone != Zone::Hand {
                return Err(EngineError::IllegalAction(
                    "Plot target must be in your hand".into(),
                ));
            }
            let eff_def = state
                .rare(object)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            let pk = state
                .db()
                .get(eff_def)
                .and_then(|c| c.faces.first())
                .and_then(|f| {
                    f.abilities.iter().find_map(|ab| match ab {
                        AbilityDef::Keyword(k)
                            if state
                                .db()
                                .keyword_name(k.keyword.keyword)
                                .is_some_and(|n| n.eq_ignore_ascii_case("plot")) =>
                        {
                            Some(k.keyword.clone())
                        }
                        _ => None,
                    })
                })
                .ok_or_else(|| EngineError::IllegalAction("card does not have Plot".into()))?;
            if !can_pay_non_mana_costs_cached(
                state,
                player,
                object,
                &pk.cost_defs,
                None,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction(
                    "cannot pay Plot non-mana costs".into(),
                ));
            }
            if let Some(cost) = &pk.cost {
                pay_special_action_mana(
                    state,
                    player,
                    object,
                    0,
                    Zone::Hand,
                    ManaSpendContextKind::SpecialAction,
                    cost,
                )?;
            }
            for c in &pk.cost_defs {
                let _ = execute_non_mana_cost_step(state, player, object, c, &[], None, None)?;
            }
            let mv = move_object(state, object, Zone::Exile, MoveRawOptions::default(), None)?;
            if let MoveObjectOutcome::Moved(raw) = mv {
                let new_id = raw.new_id.unwrap_or(raw.old_id);
                let cur_turn = state.turn_number();
                if let Some(o) = state.object_mut(new_id) {
                    o.flags.insert(ObjectFlags::ENTERED_THIS_TURN);
                }
                if let Some(rare) = state.rare_or_default(new_id) {
                    rare.plotted_by = Some(player);
                    rare.plotted_turn = Some(cur_turn);
                }
                state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE);
                emit_event(
                    state,
                    AtomicEventKind::Plot {
                        player,
                        object: new_id,
                    },
                );
                flush_triggers(state);
                let _ = prepare_next_untargeted_stack_trigger(state)?;
            }
            Ok(())
        }
        SpecialActionKind::Suspend { .. }
        | SpecialActionKind::CompanionToHand
        | SpecialActionKind::HandSpecialAction { .. } => Err(EngineError::NotImplemented(
            "this special action is deferred to Phase 2",
        )),
    }
}

fn pay_special_action_mana(
    state: &mut GameState,
    player: PlayerId,
    object: ObjectId,
    face_idx: u8,
    from_zone: Zone,
    kind: ManaSpendContextKind,
    cost: &ManaCost,
) -> Result<(), EngineError> {
    if cost.is_empty() {
        return Ok(());
    }
    let is_face_down = state.object(object).is_some_and(|o| o.is_face_down());
    let spend_ctx = ManaSpendContext {
        player,
        kind,
        source_object: Some(object),
        face_idx,
        from_zone,
        is_commander: crate::commander::is_commander_object(state, object),
        has_x_in_cost: cost.x_count > 0,
        is_fused: false,
        is_prototyped: false,
        is_bestowed: false,
        is_face_down,
        alt_cast_spend_any_color: false,
        mana_as_any_color_ability: false,
        x_spend_colors: ColorMask::empty(),
        x_colored_only: false,
        x_max_one_per_color: false,
        source_restriction: None,
    };
    let tap_plan_opt = {
        let cache = state.evaluate_board();
        if can_pay_mana_from_pool_cached(state, player, cost, &spend_ctx, cache) {
            None
        } else {
            Some(plan_auto_tap_cached(
                state,
                player,
                cost,
                &spend_ctx,
                AutoTapPolicy::default(),
                cache,
            ))
        }
    };
    if let Some(plan) = tap_plan_opt {
        if !plan.can_pay {
            return Err(EngineError::IllegalAction(
                "insufficient mana for special action".into(),
            ));
        }
        execute_auto_tap_plan(state, player, &plan)?;
        let _ =
            deduct_mana_for_context(state, player, cost, &spend_ctx, &plan.phyrexian_life_flags)?;
    } else {
        let _ = deduct_mana_for_context(state, player, cost, &spend_ctx, &[])?;
    }
    Ok(())
}

pub fn _unused_face_marker(_f: &FaceDef, _ls: LoyaltySign, _mc: ManaColor) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use mtg_ir::{
        AbilityDef, CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, ColorMask,
        FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, ModalDef, ModalMode,
        ObjectFilter, PlayerFilter, PlayerRelation, SpellAbility, SubtypeId, SupertypeMask,
        TargetKind, TargetSpec, ZoneMask,
    };
    use std::sync::Arc;

    #[test]
    fn sequential_cast_modal_x_target_tap_mana_pay_from_pool() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Mountain".into());
        let sub_mtn = SubtypeId(0);
        let mtn = CardDef {
            oracle_id: "c_mtn".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Mountain".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Mountain".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_mtn]),
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
        let modal_x = CardDef {
            oracle_id: "c_mx".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal Fire".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Modal Fire".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{X}{R}"),
                    types: CardTypeMask::SORCERY,
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
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 1,
                        max: 1,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![ModalMode {
                            targets: vec![TargetSpec {
                                kind: TargetKind::Player,
                                filter: ObjectFilter::default(),
                                player: Some(PlayerFilter {
                                    relation: PlayerRelation::Opponent,
                                }),
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
                                zones: ZoneMask::empty(),
                            }],
                            effect: vec![],
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
                    }),
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
                cards: vec![mtn, modal_x],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(0),
                CardDefId(1),
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
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            9,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        // Put 2 Mountains on battlefield
        for _ in 0..2 {
            let m_id = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            let _ = move_object(
                &mut state,
                m_id,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let spell_id = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();

        begin_cast_spell(&mut state, PlayerId(0), spell_id, 0, Zone::Hand).unwrap();
        assert!(matches!(
            state.decision(),
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode { .. }
            }
        ));
        let pid = state.pending_prompt().unwrap().prompt_id;
        resume_casting_prompt(
            &mut state,
            PlayerId(0),
            pid,
            ChoiceAnswer::PickMode { index: 0 },
        )
        .unwrap();

        assert!(matches!(
            state.decision(),
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseNumber { .. }
            }
        ));
        let pid2 = state.pending_prompt().unwrap().prompt_id;
        resume_casting_prompt(
            &mut state,
            PlayerId(0),
            pid2,
            ChoiceAnswer::PickNumber { value: 1 },
        )
        .unwrap();

        assert_eq!(state.decision(), &DecisionState::CastingSequence);
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(1) },
            },
        )
        .unwrap();

        // Now in PayCosts: AutoPayMana pays {1}{R} from the 2 Mountains and commits to Stack.
        apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(state.stack_entries()[0].x_value, 1);
        assert_eq!(
            state.stack_entries()[0].targets,
            vec![TargetRef::Player { id: PlayerId(1) }]
        );
    }

    #[test]
    fn macro_cast_rollback_and_room_unlock_and_snapshot_round_trip() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Island".into());
        let sub_island = SubtypeId(0);
        tables.subtypes.push("Room".into());
        let sub_room = SubtypeId(1);
        let island = CardDef {
            oracle_id: "c_isl".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Island".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Island".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_island]),
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
        let room = CardDef {
            oracle_id: "c_room".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Study // Synth Vault".into(),
            layout: CardLayout::Split,
            faces: vec![
                FaceDef {
                    name: "Synth Study".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{U}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: SmallVec::from_slice(&[sub_room]),
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
                },
                FaceDef {
                    name: "Synth Vault".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}{U}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: SmallVec::from_slice(&[sub_room]),
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
                },
            ],
            fully_supported: true,
        };
        let bolt = CardDef {
            oracle_id: "c_bolt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shock".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Shock".into(),
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
                    targets: vec![TargetSpec {
                        kind: TargetKind::Player,
                        filter: ObjectFilter::default(),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Any,
                        }),
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
                        zones: ZoneMask::empty(),
                    }],
                    effect: vec![],
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
                cards: vec![island, room, bolt],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            19,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        // MacroAction::CastSpell with no Red mana must fail & roll back rev/seq cleanly
        let bolt_id = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let rev_before = state.layer_rev();
        let seq_before = state.event_seq();
        let err = apply_macro_cast_spell(
            &mut state,
            PlayerId(0),
            &MacroAction::CastSpell {
                object: bolt_id,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![TargetRef::Player { id: PlayerId(1) }],
            },
        );
        assert!(matches!(err, Err(EngineError::IllegalAction(_))));
        assert_eq!(state.layer_rev(), rev_before);
        assert_eq!(state.event_seq(), seq_before);

        // Room unlock pays {1}{U} and emits DoorUnlocked + RoomFullyUnlocked
        for _ in 0..2 {
            let isl = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            let _ = move_object(
                &mut state,
                isl,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let room_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let room_bf = match move_object(
            &mut state,
            room_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap(),
            _ => unreachable!(),
        };
        state
            .rare_or_default(room_bf)
            .unwrap()
            .unlocked_faces
            .push(0);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::UnlockRoomDoor {
                object: room_bf,
                face_index: 1,
            },
        )
        .unwrap();
        assert!(
            state
                .event_log()
                .iter()
                .any(|e| matches!(e.kind, AtomicEventKind::DoorUnlocked { face_index: 1, .. }))
        );
        assert!(
            state
                .event_log()
                .iter()
                .any(|e| matches!(e.kind, AtomicEventKind::RoomFullyUnlocked { .. }))
        );

        // JSON round-trip test with populated CastingState + PendingPromptContext + RestrictedManaBucket
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .mana_ext_mut()
            .restricted_mana
            .push(crate::state::RestrictedManaBucket {
                colors: [0, 1, 0, 0, 0, 0],
                restriction: Arc::new(mtg_ir::ManaSpendingRestriction::default()),
                source_id: Some(room_bf),
                source_tag: None,
                source_kind_flags: 0,
                is_snow: false,
                retain_until_eot: true,
                delayed_rider: None,
            });
        let pid = state.allocate_prompt_id();
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id: pid,
            chooser: PlayerId(0),
            request: ChoiceRequest::ChooseNumber {
                prompt_id: pid,
                min: 0,
                max: 3,
            },
            owner: PromptOwner::Casting(CastingPromptKind::CastXValue { min: 0, max: 3 }),
        }));
        let snap = state.to_snapshot();
        let json_bytes = serde_json::to_vec(&snap).unwrap();
        let decoded: crate::state::GameStateSnapshot = serde_json::from_slice(&json_bytes).unwrap();
        let restored = GameState::from_snapshot(db, decoded);
        assert_eq!(restored.event_log().len(), state.event_log().len());
        assert_eq!(restored.players()[0].restricted_mana().len(), 1);
        assert_eq!(restored.pending_prompt().map(|p| p.prompt_id), Some(pid));
    }

    fn make_basic_face(
        name: &str,
        mana_cost: ManaCost,
        types: CardTypeMask,
        colors: ColorMask,
        abilities: Vec<AbilityDef>,
    ) -> FaceDef {
        FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost,
                types,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors,
                color_indicator: None,
                pt: if types.contains(CardTypeMask::CREATURE) {
                    Some(mtg_ir::PrintedPt::Fixed {
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
        }
    }

    fn make_keyword_def(
        keyword: mtg_ir::KeywordId,
        cost: Option<ManaCost>,
        cost_defs: Vec<CostDef>,
    ) -> mtg_ir::KeywordDef {
        mtg_ir::KeywordDef {
            keyword,
            cost,
            param: None,
            n: None,
            cost_defs,
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

    #[test]
    fn choose_number_between_and_multi_sacrifice_replacement_pause_resume_cleanly() {
        use mtg_ir::{
            ActivationRestriction, Amount, CostObjectTarget, EventDetail, EventFilter, EventKind,
            EventPattern, ReplacementAbility, ReplacementDef, ReplacementWith, ZoneChangeMod,
        };

        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());
        let _kw0 = mtg_ir::KeywordId(0);

        // Card 0: Synth Bear (2/2 creature)
        let bear = CardDef {
            oracle_id: "c_bear_pause".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };

        // Card 1: Spell with ChooseNumberBetween(1, 5) additional cost
        let num_spell = CardDef {
            oracle_id: "c_num_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Guess".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Guess",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::ChooseNumberBetween { min: 1, max: 5 }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: Spell with Sacrifice { count: 2, filter: Creature } additional cost
        let sac2_spell = CardDef {
            oracle_id: "c_sac2_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Double Rite".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Double Rite",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::B,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Sacrifice {
                        count: Box::new(Amount::Const { n: 2 }),
                        target: CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        sacrifice_all: false,
                        half_rounded_up: false,
                        attached_creature: false,
                        one_or_more: false,
                        up_to: None,
                        different_names: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 3 & 4: Two competing Battlefield -> Graveyard replacement enchantments -> triggers ChooseReplacement
        let repl_exile = CardDef {
            oracle_id: "c_repl_exile".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Exile Seal".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Exile Seal",
                ManaCost::default(),
                CardTypeMask::ENCHANTMENT,
                ColorMask::W,
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: EventPattern {
                            event: EventKind::ZoneChange,
                            filter: EventFilter {
                                detail: Some(EventDetail::ZoneChange {
                                    from: ZoneMask::BATTLEFIELD,
                                    to: Some(Zone::Graveyard),
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
                                to: Some(Zone::Exile),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let repl_hand = CardDef {
            oracle_id: "c_repl_hand".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Hand Seal".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Hand Seal",
                ManaCost::default(),
                CardTypeMask::ENCHANTMENT,
                ColorMask::U,
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: EventPattern {
                            event: EventKind::ZoneChange,
                            filter: EventFilter {
                                detail: Some(EventDetail::ZoneChange {
                                    from: ZoneMask::BATTLEFIELD,
                                    to: Some(Zone::Graveyard),
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
                                to: Some(Zone::Hand),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let _unused_ar = ActivationRestriction::OnceEachTurn;

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bear, num_spell, sac2_spell, repl_exile, repl_hand],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(2),
                CardDefId(0),
                CardDefId(0),
                CardDefId(3),
                CardDefId(4),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            31,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        // 1. ChooseNumberBetween pauses with PromptOwner::CostPayment and resumes without looping
        let num_id = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        begin_cast_spell(&mut state, PlayerId(0), num_id, 0, Zone::Hand).unwrap();
        let pid = match state.decision() {
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseNumber {
                        prompt_id,
                        min,
                        max,
                    },
            } => {
                assert_eq!((*min, *max), (1, 5));
                *prompt_id
            }
            other => panic!("expected ChooseNumber prompt, got {other:?}"),
        };
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid,
            ChoiceAnswer::PickNumber { value: 3 },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert!(state.casting().is_none());
        assert_eq!(state.stack_entries().len(), 1);
        let stack_num_obj = state.stack()[0];
        assert_eq!(
            state.rare(stack_num_obj).and_then(|r| r.chosen_number),
            Some(3)
        );

        // 2. Multi-object Sacrifice { count: 2 } with replacement pause on item 0 and item 1
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let mut bf_bears = Vec::new();
        for _ in 0..2 {
            let b = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            if let MoveObjectOutcome::Moved(raw) = move_object(
                &mut state,
                b,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                bf_bears.push(raw.new_id.unwrap());
            }
        }
        for seal_def in [CardDefId(3), CardDefId(4)] {
            let s = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == seal_def)
                .unwrap();
            let _ = move_object(
                &mut state,
                s,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let sac_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        if state.object(sac_id).unwrap().zone != Zone::Hand {
            let _ = move_object(
                &mut state,
                sac_id,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let sac_hand = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();

        let sac_events_before = state
            .event_log()
            .iter()
            .filter(|e| matches!(e.kind, AtomicEventKind::Sacrificed { .. }))
            .count();

        begin_cast_spell(&mut state, PlayerId(0), sac_hand, 0, Zone::Hand).unwrap();
        // CostObjectSelection prompt for Sacrifice { count: 2 }
        let p_sel1 = state.pending_prompt().unwrap().prompt_id;
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            p_sel1,
            ChoiceAnswer::PickObject { id: bf_bears[0] },
        )
        .unwrap();
        let p_sel2 = state.pending_prompt().unwrap().prompt_id;
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            p_sel2,
            ChoiceAnswer::PickObject { id: bf_bears[1] },
        )
        .unwrap();

        // Item 0 should pause with ChooseReplacement (2 competing ZoneChange replacements)
        let r_pid1 = match state.decision() {
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseReplacement {
                        prompt_id,
                        option_count,
                    },
            } => {
                assert_eq!(*option_count, 2);
                *prompt_id
            }
            other => panic!("expected ChooseReplacement pause on item 0, got {other:?}"),
        };
        assert!(state.casting().is_some());
        assert_eq!(
            state
                .casting()
                .unwrap()
                .payment_plan
                .as_ref()
                .unwrap()
                .non_mana_cursor,
            0
        );

        // Resolve item 0 replacement -> item 0 sacrificed_lki recorded, item 1 starts and pauses
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            r_pid1,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        let r_pid2 = match state.decision() {
            DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseReplacement {
                        prompt_id,
                        option_count,
                    },
            } => {
                assert_eq!(*option_count, 2);
                *prompt_id
            }
            other => panic!("expected ChooseReplacement pause on item 1, got {other:?}"),
        };
        assert_eq!(
            state
                .casting()
                .unwrap()
                .payment_plan
                .as_ref()
                .unwrap()
                .sacrificed_lki
                .len(),
            1
        );

        // Resolve item 1 replacement -> finishes Sacrifice cost, advances cursor once, commits spell
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            r_pid2,
            ChoiceAnswer::PickReplacement { index: 0 },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert!(state.casting().is_none());
        assert_eq!(state.stack_entries().len(), 1);
        let sac_events_after = state
            .event_log()
            .iter()
            .filter(|e| matches!(e.kind, AtomicEventKind::Sacrificed { .. }))
            .count();
        assert_eq!(sac_events_after - sac_events_before, 2);
    }

    #[test]
    fn special_action_preconditions_foretell_plot_room_and_turn_face_up() {
        use mtg_ir::{Amount, KeywordAbility, StaticAbility, StaticBody, StaticDef};

        let mut tables = InternTables::default();
        tables.subtypes.push("Island".into());
        let sub_isl = SubtypeId(0);
        tables.subtypes.push("Room".into());
        let sub_room = SubtypeId(1);
        tables.keywords.push("plot".into());
        let kw_plot = mtg_ir::KeywordId(0);

        let island = CardDef {
            oracle_id: "sp_isl".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Island".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Island".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_isl]),
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

        // Card 1: Plain sorcery {2}{U} (no innate foretell)
        let plain_sorcery = CardDef {
            oracle_id: "sp_plain".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Gaze".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Gaze",
                ManaCost::parse("{2}{U}"),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: GrantHandForetell { reduce_generic: 2 } permanent
        let foretell_granter = CardDef {
            oracle_id: "sp_ft_grant".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dreamer".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Dreamer",
                ManaCost::parse("{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::GrantHandForetell {
                            filter: Box::new(ObjectFilter::default()),
                            reduce_generic: 2,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 3: Plot card with mana cost {U} + non-mana PayLife(5) cost
        let plot_card = CardDef {
            oracle_id: "sp_plot".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plotter".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Plotter",
                ManaCost::parse("{2}{U}"),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: make_keyword_def(
                        kw_plot,
                        Some(ManaCost::parse("{U}")),
                        vec![CostDef::PayLife {
                            amount: Box::new(Amount::Const { n: 5 }),
                            half_life_rounded_up: false,
                        }],
                    ),
                    id: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 4: Room card
        let room = CardDef {
            oracle_id: "sp_room".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Door A // Synth Door B".into(),
            layout: CardLayout::Split,
            faces: vec![
                FaceDef {
                    name: "Synth Door A".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{U}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: SmallVec::from_slice(&[sub_room]),
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
                },
                FaceDef {
                    name: "Synth Door B".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}{U}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: SmallVec::from_slice(&[sub_room]),
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
                },
            ],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![island, plain_sorcery, foretell_granter, plot_card, room],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(4),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            41,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        // Move 3 Islands to battlefield
        let mut islands = Vec::new();
        for _ in 0..3 {
            let isl = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            if let MoveObjectOutcome::Moved(raw) = move_object(
                &mut state,
                isl,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                islands.push(raw.new_id.unwrap());
            }
        }
        let all_untapped = |s: &GameState| {
            islands
                .iter()
                .all(|&id| !s.object(id).unwrap().flags.contains(ObjectFlags::TAPPED))
        };

        // Ensure plain_sorcery and plot_card are in hand
        let plain_h = {
            let id = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
                .unwrap();
            if state.object(id).unwrap().zone != Zone::Hand {
                match move_object(&mut state, id, Zone::Hand, MoveRawOptions::default(), None)
                    .unwrap()
                {
                    MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                    _ => unreachable!(),
                }
            } else {
                id
            }
        };
        let plot_h = {
            let id = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            if state.object(id).unwrap().zone != Zone::Hand {
                match move_object(&mut state, id, Zone::Hand, MoveRawOptions::default(), None)
                    .unwrap()
                {
                    MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                    _ => unreachable!(),
                }
            } else {
                id
            }
        };

        // 1. Foretell on plain card without granter MUST error & NOT tap lands
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::Foretell { object: plain_h }
            )
            .is_err()
        );
        assert!(all_untapped(&state));

        // 2. Plot with insufficient life (3 < 5) MUST error BEFORE paying {U} & NOT tap lands
        state.player_mut(PlayerId(0)).unwrap().life = 3;
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::Plot { object: plot_h }
            )
            .is_err()
        );
        assert!(all_untapped(&state));
        state.player_mut(PlayerId(0)).unwrap().life = 20;

        // 3. UnlockRoomDoor on non-Room MUST error without tapping lands
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::UnlockRoomDoor {
                    object: islands[0],
                    face_index: 0,
                }
            )
            .is_err()
        );
        assert!(all_untapped(&state));

        // 4. Put Room on BF with door 0 already unlocked -> UnlockRoomDoor(0) errors without tapping lands
        let room_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(4))
            .unwrap();
        let room_bf = match move_object(
            &mut state,
            room_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };
        state
            .rare_or_default(room_bf)
            .unwrap()
            .unlocked_faces
            .push(0);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::UnlockRoomDoor {
                    object: room_bf,
                    face_index: 0,
                }
            )
            .is_err()
        );
        assert!(all_untapped(&state));

        // 5. TurnFaceUp on face-up permanent errors without tapping lands
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::TurnFaceUp { object: room_bf }
            )
            .is_err()
        );
        assert!(all_untapped(&state));

        // 6. Put face-down permanent with unpayable turn_up_cost_defs (PayLife 100) -> TurnFaceUp errors BEFORE paying mana
        let fd_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let fd_bf = match move_object(
            &mut state,
            fd_id,
            Zone::Battlefield,
            MoveRawOptions {
                enter_face_down: Some(FaceDownSpec {
                    kind: FaceDownKind::Morph,
                    turn_up_mana_cost: Some(ManaCost::parse("{U}")),
                    turn_up_cost_defs: vec![CostDef::PayLife {
                        amount: Box::new(Amount::Const { n: 100 }),
                        half_life_rounded_up: false,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };
        assert!(
            apply_special_action(
                &mut state,
                PlayerId(0),
                SpecialActionKind::TurnFaceUp { object: fd_bf }
            )
            .is_err()
        );
        assert!(all_untapped(&state));

        // 7. Put GrantHandForetell creature on BF -> Foretell on plain_sorcery succeeds & emits Foretell event
        let granter_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let _granter_bf = move_object(
            &mut state,
            granter_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::Foretell { object: plain_h },
        )
        .unwrap();
        assert!(state.event_log().iter().any(|e| matches!(
            e.kind,
            AtomicEventKind::Foretell {
                player: PlayerId(0),
                ..
            }
        )));
        // Verify granted ForetellCast cost {U} is stamped in Exile FaceDownSpec
        let exiled_ft = state.players()[0].exile[0];
        let ft_stamped = state
            .rare(exiled_ft)
            .and_then(|r| r.face_down.as_deref())
            .and_then(|fd| fd.turn_up_mana_cost.as_ref())
            .unwrap();
        assert_eq!(ft_stamped.generic, 0);
        assert_eq!(ft_stamped.pips, ManaCost::parse("{U}").pips);

        // 8. Now life == 20 and 1 Island is untapped -> Plot succeeds & emits Plot event
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::Plot { object: plot_h },
        )
        .unwrap();
        assert!(state.event_log().iter().any(|e| matches!(
            e.kind,
            AtomicEventKind::Plot {
                player: PlayerId(0),
                ..
            }
        )));
    }

    #[test]
    fn flagbearer_min_lt_max_done_targets_and_assign_division_validation() {
        use mtg_ir::{Amount, RuleMod, StaticAbility, StaticBody, StaticDef};

        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());

        // Card 0: Plain Bear
        let bear = CardDef {
            oracle_id: "fb_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        // Card 1: Flagbearer
        let flag = CardDef {
            oracle_id: "fb_flag".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Standard".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Standard",
                ManaCost::parse("{1}{W}"),
                CardTypeMask::CREATURE,
                ColorMask::W,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::RuleModifier {
                            rule: RuleMod::Flagbearer,
                            who: mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            },
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Card 2: Sorcery targeting 1..=2 creatures
        let spell_1_2 = CardDef {
            oracle_id: "fb_s12".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dual Touch".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Dual Touch",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 1,
                        max: 2,
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
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Card 3: Divided damage spell (divide 3 across 2 creatures)
        let div3 = CardDef {
            oracle_id: "fb_div3".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Split".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Split",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 2,
                        max: 2,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::Const { n: 3 })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Card 4: Divided spell with total == 0 across 2 targets
        let div0 = CardDef {
            oracle_id: "fb_div0".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Zero Split".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Zero Split",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 2,
                        max: 2,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::Const { n: 0 })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 5: Optional divided spell with total == 0 (min: 0, max: 2)
        let div0_opt = CardDef {
            oracle_id: "fb_div0_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Zero Opt Split".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Zero Opt Split",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 0,
                        max: 2,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::Const { n: 0 })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bear, flag, spell_1_2, div3, div0, div0_opt],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(2),
                CardDefId(3),
                CardDefId(4),
                CardDefId(5),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[p0_deck, p1_deck],
            53,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        let ensure_p0_hand = |state: &mut GameState, def: CardDefId| -> ObjectId {
            if let Some(id) = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == def)
            {
                return id;
            }
            let lib_id = state.players()[0]
                .library
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == def)
                .unwrap();
            match move_object(
                state,
                lib_id,
                Zone::Hand,
                MoveRawOptions {
                    to_player: Some(PlayerId(0)),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap_or(lib_id),
                _ => unreachable!(),
            }
        };

        let p1_bear = {
            let id = state.players()[1]
                .hand
                .iter()
                .chain(state.players()[1].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            match move_object(
                &mut state,
                id,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(PlayerId(1)),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        };
        let p1_flag = {
            let id = state.players()[1]
                .hand
                .iter()
                .chain(state.players()[1].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
                .unwrap();
            match move_object(
                &mut state,
                id,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(PlayerId(1)),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        };

        // 1. Pick Bear on min=1,max=2 -> DoneTargets MUST be illegal until Flagbearer is picked
        let s12_id = ensure_p0_hand(&mut state, CardDefId(2));
        begin_cast_spell(&mut state, PlayerId(0), s12_id, 0, Zone::Hand).unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: p1_bear },
            },
        )
        .unwrap();
        assert!(!legal_casting_actions(&state, PlayerId(0)).contains(&Action::DoneTargets));
        assert!(apply_casting_action(&mut state, PlayerId(0), &Action::DoneTargets).is_err());
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: p1_flag },
            },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);

        // 2. AssignDivision validation: wrong target, 0, and > max_allowed rejected; valid 2 leaves 1
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let d3_id = ensure_p0_hand(&mut state, CardDefId(3));
        begin_cast_spell(&mut state, PlayerId(0), d3_id, 0, Zone::Hand).unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: p1_bear },
            },
        )
        .unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: p1_flag },
            },
        )
        .unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseDivisions);
        assert!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::AssignDivision {
                    target: TargetRef::Player { id: PlayerId(1) },
                    amount: 1,
                }
            )
            .is_err()
        );
        assert!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::AssignDivision {
                    target: TargetRef::Object { id: p1_bear },
                    amount: 0,
                }
            )
            .is_err()
        );
        assert!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::AssignDivision {
                    target: TargetRef::Object { id: p1_bear },
                    amount: 3,
                }
            )
            .is_err()
        );
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::AssignDivision {
                target: TargetRef::Object { id: p1_bear },
                amount: 2,
            },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(
            state
                .stack_entries()
                .last()
                .unwrap()
                .choices
                .divisions
                .as_slice(),
            &[
                (TargetRef::Object { id: p1_bear }, 2),
                (TargetRef::Object { id: p1_flag }, 1),
            ]
        );

        // 3. CR 601.2d: total == 0 with min == 2 is uncastable (each target must receive >= 1),
        //    whereas total == 0 with min == 0 completes cleanly with 0 targets and 0 divisions.
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let d0_id = ensure_p0_hand(&mut state, CardDefId(4));
        assert!(can_cast_spell_option(&state, PlayerId(0), d0_id, 0).is_none());
        assert!(begin_cast_spell(&mut state, PlayerId(0), d0_id, 0, Zone::Hand).is_err());

        let d0_opt_id = ensure_p0_hand(&mut state, CardDefId(5));
        assert!(can_cast_spell_option(&state, PlayerId(0), d0_opt_id, 0).is_some());
        begin_cast_spell(&mut state, PlayerId(0), d0_opt_id, 0, Zone::Hand).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        let entry = state.stack_entries().last().unwrap();
        assert!(entry.targets.is_empty());
        assert!(entry.choices.divisions.is_empty());
    }

    #[test]
    fn multi_optional_costs_bestow_cost_aids_and_once_per_turn_tracking() {
        use mtg_ir::{ActivatedAbility, ActivationRestriction, Amount, KeywordAbility, Timing};

        let mut tables = InternTables::default();
        tables.keywords.push("bestow".into());
        let kw_bestow = mtg_ir::KeywordId(0);
        tables.keywords.push("waterbend".into());
        let kw_waterbend = mtg_ir::KeywordId(1);
        tables.subtypes.push("Island".into());
        let sub_isl = SubtypeId(0);

        // Card 0: Spell with two CostDef::Optional entries ({1} and {2})
        let multi_opt = CardDef {
            oracle_id: "m_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Twin Options".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Twin Options",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![
                        CostDef::Optional {
                            cost: vec![CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        },
                        CostDef::Optional {
                            cost: vec![CostDef::PayLife {
                                amount: Box::new(Amount::Const { n: 2 }),
                                half_life_rounded_up: false,
                            }],
                        },
                    ],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 1: Bestow creature {2}{W} with bestow {3}{W}
        let bestow_card = CardDef {
            oracle_id: "m_bestow".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Aura Lynx".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Aura Lynx",
                ManaCost::parse("{2}{W}"),
                CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                ColorMask::W,
                vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: make_keyword_def(kw_bestow, Some(ManaCost::parse("{3}{W}")), vec![]),
                    id: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: Creature with [0] normal activated ability (no OnceEachTurn) and [1] OnceEachTurn + Waterbend {2}
        let ab_creature = CardDef {
            oracle_id: "m_ab".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Tide Shaper".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Tide Shaper",
                ManaCost::parse("{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![
                    AbilityDef::Activated(ActivatedAbility {
                        id: "ab_plain".into(),
                        cost: vec![],
                        targets: vec![],
                        effect: vec![],
                        modal: None,
                        timing: Timing::Instant,
                        is_mana: false,
                        activation_restrictions: vec![],
                        activation_condition: None,
                        activator_who: None,
                        power_up_reduction: false,
                        x_cant_be_zero: false,
                        activation_cost_reduction: None,
                        activation_zones: ZoneMask::empty(),
                        cost_keyword: None,
                        intrinsic: false,
                        text: None,
                        grant_condition: None,
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        span: Default::default(),
                    }),
                    AbilityDef::Activated(ActivatedAbility {
                        id: "ab_wb_once".into(),
                        cost: vec![CostDef::Mana {
                            cost: ManaCost::parse("{2}"),
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                        }],
                        targets: vec![],
                        effect: vec![],
                        modal: None,
                        timing: Timing::Instant,
                        is_mana: false,
                        activation_restrictions: vec![ActivationRestriction::OnceEachTurn],
                        activation_condition: None,
                        activator_who: None,
                        power_up_reduction: false,
                        x_cant_be_zero: false,
                        activation_cost_reduction: None,
                        activation_zones: ZoneMask::empty(),
                        cost_keyword: Some(kw_waterbend),
                        intrinsic: false,
                        text: None,
                        grant_condition: None,
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        span: Default::default(),
                    }),
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_keyword_def(kw_waterbend, None, vec![]),
                        id: None,
                        span: Default::default(),
                    }),
                ],
            )],
            fully_supported: true,
        };

        let island = CardDef {
            oracle_id: "m_isl".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Island".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Island".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_isl]),
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
                cards: vec![multi_opt, bestow_card, ab_creature, island],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            67,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);

        // 1. Multi-Optional: decline #0, must STILL prompt for #1, accept #1 -> loses 2 life, pays 0 mana
        let opt_h = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [1, 0, 0, 0, 0, 0];
        begin_cast_spell(&mut state, PlayerId(0), opt_h, 0, Zone::Hand).unwrap();
        let pid_opt0 = state.pending_prompt().unwrap().prompt_id;
        assert!(matches!(
            state.pending_prompt().unwrap().owner,
            PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index: 0, .. })
        ));
        resume_casting_prompt(
            &mut state,
            PlayerId(0),
            pid_opt0,
            ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();
        let pid_opt1 = state.pending_prompt().unwrap().prompt_id;
        assert!(matches!(
            state.pending_prompt().unwrap().owner,
            PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index: 1, .. })
        ));
        let life_before = state.players()[0].life;
        resume_casting_prompt(
            &mut state,
            PlayerId(0),
            pid_opt1,
            ChoiceAnswer::YesNo { value: true },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.players()[0].life, life_before - 2);
        assert_eq!(state.players()[0].mana_pool, [1, 0, 0, 0, 0, 0]);

        // 2. Put creature on battlefield and test normal vs OnceEachTurn + Waterbend cost aid
        let cr_h = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let cr_bf = match move_object(
            &mut state,
            cr_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };

        // Normal ability (index 0): must NOT populate mana_ext / once_per_turn_abilities_used
        begin_activate_ability(&mut state, PlayerId(0), cr_bf, 0).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert!(state.players()[0].mana_ext().is_none());

        // Bestow spell targets cr_bf via injected Bestow target spec
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let bestow_h = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let bestow_opt = crate::legality::enumerate_cast_options(&state, PlayerId(0), bestow_h)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::Bestow)
            .unwrap();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [5, 0, 0, 0, 0, 0];
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            bestow_h,
            bestow_opt.option_id,
            Zone::Hand,
        )
        .unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: cr_bf },
            },
        )
        .unwrap();
        // Disabled CostAidKind::Emerge must be rejected
        assert!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::SelectCostAid {
                    aid_kind: CostAidKind::Emerge,
                    object: cr_bf,
                }
            )
            .is_err()
        );
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert!(state.stack_entries().last().unwrap().choices.bestowed);

        // OnceEachTurn + Waterbend ability (index 1): put an Island on BF (so {2} is payable via {1} pool + Island),
        // verify Waterbend aid is emitted, tap cr_bf for {1}, and pay remaining {1} from pool without tapping Island.
        let isl_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
            .unwrap();
        let isl_bf = match move_object(
            &mut state,
            isl_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };
        begin_activate_ability(&mut state, PlayerId(0), cr_bf, 1).unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
        let acts = legal_casting_actions(&state, PlayerId(0));
        assert!(acts.contains(&Action::SelectCostAid {
            aid_kind: CostAidKind::Waterbend,
            object: cr_bf,
        }));
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectCostAid {
                aid_kind: CostAidKind::Waterbend,
                object: cr_bf,
            },
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert!(
            !state
                .object(isl_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert!(
            state
                .object(cr_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert_eq!(
            state.players()[0].once_per_turn_abilities_used(),
            &[(cr_bf, 1)]
        );
        assert!(state.players()[0].once_per_game_abilities_used().is_empty());
    }

    #[test]
    fn up_to_one_and_zero_mode_triggered_abilities_never_loop_or_overflow() {
        use mtg_ir::{
            Amount, EventFilter, EventKind, ModalDef, ModalMode, ObjectFilter, TriggerDef,
            TriggeredAbility,
        };

        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());

        // Card 0: Bear (no targets, plain 2/2)
        let bear = CardDef {
            oracle_id: "t_bear_up_to".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };

        // Card 1: Triggered permanent ("Whenever you cast a spell, return up to one target artifact...")
        let up_to_target_perm = CardDef {
            oracle_id: "t_tyrant_like".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Tyrant".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Tyrant",
                ManaCost::parse("{1}{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::Cast,
                        filter: EventFilter {
                            player: Some(mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::ARTIFACT,
                            ..Default::default()
                        },
                        player: None,
                        min: 0,
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
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: Artifact (optional legal target)
        let artifact = CardDef {
            oracle_id: "t_art_up_to".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Relic",
                ManaCost::parse("{0}"),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };

        // Card 3: Modal up_to=true trigger (min: 0, max: 1)
        let modal_up_to_perm = CardDef {
            oracle_id: "t_modal_up_to".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal UpTo".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Modal UpTo",
                ManaCost::parse("{1}{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::Cast,
                        filter: EventFilter {
                            player: Some(mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 1,
                        max: 1,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: true,
                        conditional_choose_all: None,
                        modes: vec![ModalMode {
                            targets: vec![],
                            effect: vec![],
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
                    }),
                    text: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 4: Modal dynamic_max == 0 trigger
        let modal_dyn_zero_perm = CardDef {
            oracle_id: "t_modal_dyn_zero".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Modal DynZero".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Modal DynZero",
                ManaCost::parse("{1}{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::Cast,
                        filter: EventFilter {
                            player: Some(mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            }),
                            ..Default::default()
                        },
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 1,
                        max: 1,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![ModalMode {
                            targets: vec![],
                            effect: vec![],
                            span: Default::default(),
                        }],
                        per_mode_costs: vec![],
                        unchosen_permanent: false,
                        opponent_chooses: None,
                        pawprint_budget: None,
                        pawprint_costs: vec![],
                        dynamic_max: Some(Box::new(Amount::Const { n: 0 })),
                        random_mode: false,
                        prefix_effect: vec![],
                        suffix_effect: vec![],
                        conditional_choose_range: None,
                        conditional_not_random: None,
                        different_player_targets_per_mode: false,
                    }),
                    text: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    bear,
                    up_to_target_perm,
                    artifact,
                    modal_up_to_perm,
                    modal_dyn_zero_perm,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // Subtest 1: up to one target with 0 legal targets -> commits cleanly without SIGSEGV
        let deck1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(0),
                CardDefId(0),
                CardDefId(2),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck1.clone(), deck1],
            31,
        )
        .unwrap();
        let tyrant_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let _tyrant_bf = move_object(
            &mut state,
            tyrant_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let bear_h = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 2, 0];
        begin_cast_spell(&mut state, PlayerId(0), bear_h, 0, Zone::Hand).unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        let trig_entry = state
            .stack_entries()
            .iter()
            .find(|e| matches!(e.kind, StackEntryKind::Triggered { .. }))
            .unwrap();
        assert!(trig_entry.targets.is_empty());
        assert!(trig_entry.choices.targets_chosen);
        assert!(!prepare_next_untargeted_stack_trigger(&mut state).unwrap());

        // Subtest 2: same trigger when a legal artifact exists -> DoneTargets with 0 targets commits cleanly
        let art_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let _art_bf = move_object(
            &mut state,
            art_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let bear_h2 = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 2, 0];
        begin_cast_spell(&mut state, PlayerId(0), bear_h2, 0, Zone::Hand).unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.decision(), &DecisionState::CastingSequence);
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);
        apply_casting_action(&mut state, PlayerId(0), &Action::DoneTargets).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        let trig_entry2 = state.stack_entries().last().unwrap();
        assert!(trig_entry2.targets.is_empty());
        assert!(trig_entry2.choices.targets_chosen);
        assert!(!prepare_next_untargeted_stack_trigger(&mut state).unwrap());

        // Subtest 3: modal.up_to == true (DonePicking 0 modes) & modal.dynamic_max == 0
        let deck3 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(3),
                CardDefId(4),
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
        let mut state3 =
            GameState::new(db, GameConfig::standard_1v1(), &[deck3.clone(), deck3], 37).unwrap();
        let m_upto_h = state3.players()[0]
            .hand
            .iter()
            .chain(state3.players()[0].library.iter())
            .copied()
            .find(|&id| state3.object(id).unwrap().card_def == CardDefId(3))
            .unwrap();
        let m_dyn_h = state3.players()[0]
            .hand
            .iter()
            .chain(state3.players()[0].library.iter())
            .copied()
            .find(|&id| state3.object(id).unwrap().card_def == CardDefId(4))
            .unwrap();
        let _ = move_object(
            &mut state3,
            m_upto_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let _ = move_object(
            &mut state3,
            m_dyn_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let bear3 = state3.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state3.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        state3.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 2, 0];
        begin_cast_spell(&mut state3, PlayerId(0), bear3, 0, Zone::Hand).unwrap();
        apply_casting_action(&mut state3, PlayerId(0), &Action::PayFromPool).unwrap();
        // Simultaneous triggers emit OrderObjects, then up_to trigger prompts ChooseMode;
        // dynamic_max==0 skips automatically without overflowing or looping.
        for _ in 0..4 {
            let DecisionState::Prompt { request } = state3.decision().clone() else {
                break;
            };
            match request {
                ChoiceRequest::OrderObjects { prompt_id, objects } => {
                    crate::choices::apply_choice_answer(
                        &mut state3,
                        PlayerId(0),
                        prompt_id,
                        ChoiceAnswer::OrderNext { id: objects[0] },
                    )
                    .unwrap();
                }
                ChoiceRequest::ChooseMode { prompt_id, .. } => {
                    crate::choices::apply_choice_answer(
                        &mut state3,
                        PlayerId(0),
                        prompt_id,
                        ChoiceAnswer::DonePicking,
                    )
                    .unwrap();
                }
                other => panic!("unexpected prompt {other:?}"),
            }
        }
        assert_eq!(state3.decision(), &DecisionState::Priority);
        for se in state3.stack_entries() {
            if matches!(se.kind, StackEntryKind::Triggered { .. }) {
                assert!(se.choices.modes_chosen);
                assert!(se.choices.targets_chosen);
            }
        }
        assert!(!prepare_next_untargeted_stack_trigger(&mut state3).unwrap());
    }

    #[test]
    fn foretell_to_foretell_cast_and_cancel_cast_lifecycle() {
        use mtg_ir::{KeywordAbility, ObjectFilter, StaticAbility, StaticBody, StaticDef};

        let mut tables = InternTables::default();
        tables.keywords.push("foretell".into());
        let kw_foretell = mtg_ir::KeywordId(0);

        // Card 0: Noncreature Sorcery {2}{U} (no innate foretell; relies on GrantHandForetell -> {U})
        let granted_sorcery = CardDef {
            oracle_id: "ft_sorc".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Insight".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Insight",
                ManaCost::parse("{2}{U}"),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![],
            )],
            fully_supported: true,
        };

        // Card 1: Innate Foretell Instant ({1}{U}, Foretell {U})
        let foretell_instant = CardDef {
            oracle_id: "ft_inst".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Behold".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Behold",
                ManaCost::parse("{1}{U}"),
                CardTypeMask::INSTANT,
                ColorMask::U,
                vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: make_keyword_def(kw_foretell, Some(ManaCost::parse("{U}")), vec![]),
                    id: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: Creature with GrantHandForetell (reduce_generic: 2)
        let granter = CardDef {
            oracle_id: "ft_granter".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dreamer".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Dreamer",
                ManaCost::parse("{1}{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::GrantHandForetell {
                            filter: Box::new(ObjectFilter::default()),
                            reduce_generic: 2,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![granted_sorcery, foretell_instant, granter],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
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
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 41).unwrap();

        let granter_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let _ = move_object(
            &mut state,
            granter_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let mut sorc_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        if state.object(sorc_h).unwrap().zone != Zone::Hand {
            sorc_h = match move_object(
                &mut state,
                sorc_h,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                crate::zones::MoveObjectOutcome::Moved(out) => out.new_id.unwrap(),
                other => panic!("expected Moved, got {other:?}"),
            };
        }
        let mut inst_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        if state.object(inst_h).unwrap().zone != Zone::Hand {
            inst_h = match move_object(
                &mut state,
                inst_h,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                crate::zones::MoveObjectOutcome::Moved(out) => out.new_id.unwrap(),
                other => panic!("expected Moved, got {other:?}"),
            };
        }

        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 0, 4];
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::Foretell { object: sorc_h },
        )
        .unwrap();
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::Foretell { object: inst_h },
        )
        .unwrap();

        let exiled_sorc = state.players()[0]
            .exile
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let exiled_inst = state.players()[0]
            .exile
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();

        // Advance to Turn 2 and clear ENTERED_THIS_TURN on foretold exile cards
        state.set_turn_number(2);
        state
            .object_mut(exiled_sorc)
            .unwrap()
            .flags
            .remove(ObjectFlags::ENTERED_THIS_TURN);
        state
            .object_mut(exiled_inst)
            .unwrap()
            .flags
            .remove(ObjectFlags::ENTERED_THIS_TURN);
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::EXILE);

        // Foretold Instant is castable on opponent's turn!
        state.set_active_player(PlayerId(1));
        state.set_priority_player(PlayerId(0));
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 0];
        let inst_ft_opt = crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_inst)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::ForetellCast)
            .unwrap();
        assert!(
            can_cast_spell_option(&state, PlayerId(0), exiled_inst, inst_ft_opt.option_id)
                .is_some()
        );
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            exiled_inst,
            inst_ft_opt.option_id,
            Zone::Exile,
        )
        .unwrap();
        let stack_inst = state.casting().unwrap().object;
        assert!(!state.object(stack_inst).unwrap().is_face_down());
        assert!(state.rare(stack_inst).is_none_or(|r| r.face_down.is_none()));
        assert!(
            state
                .object(stack_inst)
                .unwrap()
                .flags
                .contains(ObjectFlags::FORETOLD)
        );
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.players()[0].noncreature_spells_cast_this_turn, 1);

        // Now test CancelCast mid-ForetellCast on GrantHandForetell sorcery (costs {U}, not {2}{U})
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 0];
        let sorc_ft_opt = crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_sorc)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::ForetellCast)
            .unwrap();
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            exiled_sorc,
            sorc_ft_opt.option_id,
            Zone::Exile,
        )
        .unwrap();
        let stack_sorc = state.casting().unwrap().object;
        assert!(!state.object(stack_sorc).unwrap().is_face_down());
        assert!(state.rare(stack_sorc).is_none_or(|r| r.face_down.is_none()));
        assert!(
            state
                .object(stack_sorc)
                .unwrap()
                .flags
                .contains(ObjectFlags::FORETOLD)
        );
        // CancelCast restores Exile, FORETOLD | FACE_DOWN, and turn_up_mana_cost = Some({U})
        apply_casting_action(&mut state, PlayerId(0), &Action::CancelCast).unwrap();
        let exiled_sorc2 = state.players()[0]
            .exile
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let sorc_obj = state.object(exiled_sorc2).unwrap();
        assert_eq!(sorc_obj.zone, Zone::Exile);
        assert!(sorc_obj.flags.contains(ObjectFlags::FORETOLD));
        assert!(sorc_obj.flags.contains(ObjectFlags::FACE_DOWN));
        let sorc_rare = state.rare(exiled_sorc2).unwrap();
        assert_eq!(sorc_rare.alt_cast_kind, AltCastKind::Normal);
        let turn_up_mc = sorc_rare
            .face_down
            .as_deref()
            .and_then(|fd| fd.turn_up_mana_cost.as_ref())
            .expect("restored turn_up_mana_cost");
        assert_eq!(turn_up_mc.generic, 0);
        assert_eq!(turn_up_mc.x_count, 0);
        assert_eq!(turn_up_mc.pips, ManaCost::parse("{U}").pips);

        // Immediately castable again for {U} (NOT {2}{U}!)
        let sorc_ft_opt2 =
            crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_sorc2)
                .into_iter()
                .find(|o| o.alt_kind == AltCastKind::ForetellCast)
                .unwrap();
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            exiled_sorc2,
            sorc_ft_opt2.option_id,
            Zone::Exile,
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.players()[0].mana_pool, [0; 6]);
        assert_eq!(state.players()[0].noncreature_spells_cast_this_turn, 2);
    }

    #[test]
    fn plot_to_plot_cast_sorcery_speed_and_cancel_cast() {
        use mtg_ir::KeywordAbility;

        let mut tables = InternTables::default();
        tables.keywords.push("plot".into());
        let kw_plot = mtg_ir::KeywordId(0);
        tables.keywords.push("flash".into());
        let kw_flash = mtg_ir::KeywordId(1);

        // Card 0: Instant with Plot {1}{U} (CR 702.170d: still sorcery-only when PlotCast!)
        let plotted_instant = CardDef {
            oracle_id: "plot_inst".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plot Spark".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Plot Spark",
                ManaCost::parse("{2}{U}"),
                CardTypeMask::INSTANT,
                ColorMask::U,
                vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_keyword_def(kw_plot, Some(ManaCost::parse("{1}{U}")), vec![]),
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_keyword_def(kw_flash, None, vec![]),
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Spell(mtg_ir::SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![TargetSpec {
                            kind: TargetKind::Player,
                            filter: mtg_ir::ObjectFilter::default(),
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
                            zones: ZoneMask::empty(),
                        }],
                        effect: vec![],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Default::default(),
                    }),
                ],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plotted_instant],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 43).unwrap();

        let card_h = state.players()[0].hand[0];
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 1];
        apply_special_action(
            &mut state,
            PlayerId(0),
            SpecialActionKind::Plot { object: card_h },
        )
        .unwrap();
        let exiled_id = state.players()[0].exile[0];
        assert_eq!(state.rare(exiled_id).unwrap().plotted_by, Some(PlayerId(0)));
        assert_eq!(state.rare(exiled_id).unwrap().plotted_turn, Some(1));

        // Same turn: PlotCast cannot be cast
        assert!(crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_id).is_empty());

        // Turn 2: PlotCast is enumerated, but sorcery_only rejects opponent turn, combat, or non-empty stack
        state.set_turn_number(2);
        let plot_opt = crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_id)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::PlotCast)
            .unwrap();

        state.set_active_player(PlayerId(1));
        assert!(
            can_cast_spell_option(&state, PlayerId(0), exiled_id, plot_opt.option_id).is_none()
        );

        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::BeginCombat);
        assert!(
            can_cast_spell_option(&state, PlayerId(0), exiled_id, plot_opt.option_id).is_none()
        );

        state.set_phase(mtg_ir::GamePhase::Main1);
        let dummy_stack = state.players()[1].hand[0];
        state.stack_mut().push(dummy_stack);
        assert!(
            can_cast_spell_option(&state, PlayerId(0), exiled_id, plot_opt.option_id).is_none()
        );
        state.stack_mut().clear();

        // Main1, empty stack, own turn -> PlotCast for {0}
        assert!(
            can_cast_spell_option(&state, PlayerId(0), exiled_id, plot_opt.option_id).is_some()
        );
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            exiled_id,
            plot_opt.option_id,
            Zone::Exile,
        )
        .unwrap();
        let stack_id = state.casting().unwrap().object;
        assert_eq!(state.object(stack_id).unwrap().zone, Zone::Stack);
        // While on Stack before commit/cancel, plotted metadata is cleared
        assert_eq!(state.rare(stack_id).and_then(|r| r.plotted_by), None);
        assert_eq!(state.rare(stack_id).and_then(|r| r.plotted_turn), None);

        // CancelCast restores plotted_by / plotted_turn in Exile
        apply_casting_action(&mut state, PlayerId(0), &Action::CancelCast).unwrap();
        let exiled_id2 = state.players()[0].exile[0];
        assert_eq!(state.object(exiled_id2).unwrap().zone, Zone::Exile);
        assert_eq!(
            state.rare(exiled_id2).unwrap().plotted_by,
            Some(PlayerId(0))
        );
        assert_eq!(state.rare(exiled_id2).unwrap().plotted_turn, Some(1));

        // Re-cast cleanly with {0} and select target
        let plot_opt2 = crate::legality::enumerate_cast_options(&state, PlayerId(0), exiled_id2)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::PlotCast)
            .unwrap();
        begin_cast_spell(
            &mut state,
            PlayerId(0),
            exiled_id2,
            plot_opt2.option_id,
            Zone::Exile,
        )
        .unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.stack().len(), 1);
    }

    #[test]
    fn toggle_and_done_optional_cost_require_choose_optional_costs_step() {
        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());

        // Card 0: Creature with 1 target and an optional cost
        let spell_with_target = CardDef {
            oracle_id: "opt_guard_card".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bolt",
                ManaCost::parse("{R}"),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(mtg_ir::SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Optional {
                        cost: vec![CostDef::PayLife {
                            amount: Box::new(mtg_ir::Amount::Const { n: 2 }),
                            half_life_rounded_up: false,
                        }],
                    }],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Player,
                        filter: mtg_ir::ObjectFilter::default(),
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
                        zones: ZoneMask::empty(),
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![spell_with_target],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 47).unwrap();
        let bolt_h = state.players()[0].hand[0];
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 1, 0, 0];
        begin_cast_spell(&mut state, PlayerId(0), bolt_h, 0, Zone::Hand).unwrap();

        // Answer ChooseOptionalCosts YesNo prompt to enter ChooseTargets
        let pid = match state.decision().clone() {
            DecisionState::Prompt {
                request: ChoiceRequest::YesNo { prompt_id, .. },
            } => prompt_id,
            other => panic!("expected YesNo prompt, got {other:?}"),
        };
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid,
            ChoiceAnswer::YesNo { value: false },
        )
        .unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);

        // During ChooseTargets: both ToggleOptionalCost and DoneOptionalCosts must return IllegalAction
        assert!(matches!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::ToggleOptionalCost { index: 0 }
            ),
            Err(EngineError::IllegalAction(_))
        ));
        assert!(matches!(
            apply_casting_action(&mut state, PlayerId(0), &Action::DoneOptionalCosts),
            Err(EngineError::IllegalAction(_))
        ));

        // Advance to PayCosts and verify both still return IllegalAction
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
        assert!(matches!(
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::ToggleOptionalCost { index: 0 }
            ),
            Err(EngineError::IllegalAction(_))
        ));
        assert!(matches!(
            apply_casting_action(&mut state, PlayerId(0), &Action::DoneOptionalCosts),
            Err(EngineError::IllegalAction(_))
        ));
    }

    #[test]
    fn choose_modes_skips_when_allow_repeat_and_empty_modes() {
        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());

        let empty_modal_spell = CardDef {
            oracle_id: "empty_modal_repeat".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Empty Modal".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Empty Modal",
                ManaCost::parse("{R}"),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(mtg_ir::SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 0,
                        max: 2,
                        allow_repeat: true,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![],
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
                    }),
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![empty_modal_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 48).unwrap();
        let card_id = state.players()[0].hand[0];
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 1, 0, 0];

        begin_cast_spell(&mut state, PlayerId(0), card_id, 0, Zone::Hand).unwrap();

        assert!(
            !matches!(
                state.decision(),
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseMode { .. }
                }
            ),
            "must not emit ChooseMode prompt when modes is empty; got {:?}",
            state.decision()
        );
        assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
    }

    #[test]
    fn emerge_cost_aid_respects_cant_be_sacrificed_cached() {
        use mtg_ir::{Affected, RestrictionKind, Span, StaticAbility, StaticBody, StaticDef};

        let mut tables = InternTables::default();
        tables.keywords.push("emerge".into());
        let kw_emerge = mtg_ir::KeywordId(0);

        let sac_bear = CardDef {
            oracle_id: "t_emerge_sac".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Sac Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Sac Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let unsac_creature = CardDef {
            oracle_id: "t_emerge_unsac".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Unsacrificable".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Unsacrificable",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Restriction {
                            affected: Affected::SelfObj,
                            restriction: RestrictionKind::CantBeSacrificed,
                        },
                        condition: None,
                    },
                    span: Span::default(),
                })],
            )],
            fully_supported: true,
        };
        let emerge_hulk = CardDef {
            oracle_id: "t_emerge_hulk".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Emerge Hulk".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Emerge Hulk",
                ManaCost::parse("{5}{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: make_keyword_def(kw_emerge, Some(ManaCost::parse("{4}{U}")), vec![]),
                    id: None,
                    span: Span::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![sac_bear, unsac_creature, emerge_hulk],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
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
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 51).unwrap();

        let move_to_bf = |st: &mut GameState, def: CardDefId| -> ObjectId {
            let id = st.players()[0]
                .hand
                .iter()
                .chain(st.players()[0].library.iter())
                .copied()
                .find(|&oid| st.object(oid).unwrap().card_def == def)
                .unwrap();
            match move_object(st, id, Zone::Battlefield, MoveRawOptions::default(), None).unwrap() {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        };

        let sac_id = move_to_bf(&mut state, CardDefId(0));
        let unsac_id = move_to_bf(&mut state, CardDefId(1));

        let hulk_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let hulk_id = if state.object(hulk_id).unwrap().zone != Zone::Hand {
            match move_object(
                &mut state,
                hulk_id,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        } else {
            hulk_id
        };

        // Pool has {2}{U}: not enough for {4}{U} without sacrificing a MV-2 creature via Emerge.
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [2, 1, 0, 0, 0, 0];
        let emerge_opt = crate::legality::enumerate_cast_options(&state, PlayerId(0), hulk_id)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::Emerge)
            .expect("Emerge cast option must exist");

        begin_cast_spell(
            &mut state,
            PlayerId(0),
            hulk_id,
            emerge_opt.option_id,
            Zone::Hand,
        )
        .unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);

        let acts = legal_casting_actions(&state, PlayerId(0));
        assert!(
            acts.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Emerge,
                object: sac_id,
            }),
            "sacrificeable creature must appear as Emerge cost aid; acts={acts:?}"
        );
        assert!(
            !acts.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Emerge,
                object: unsac_id,
            }),
            "CantBeSacrificed creature must NOT appear as Emerge cost aid; acts={acts:?}"
        );
    }

    #[test]
    fn choose_targets_lookahead_prevents_stranding_dependent_spec() {
        let tables = InternTables::default();
        let plain_card = CardDef {
            oracle_id: "t_plain_gy".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Relic Card".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Relic Card",
                ManaCost::parse("{1}"),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };
        let base_spec = TargetSpec {
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
            zones: ZoneMask::empty(),
        };
        let dependent_spell = CardDef {
            oracle_id: "t_dep_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Recall Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Recall Spell",
                ManaCost::parse("{G}"),
                CardTypeMask::SORCERY,
                ColorMask::G,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![
                        TargetSpec {
                            kind: TargetKind::Player,
                            player: Some(PlayerFilter {
                                relation: PlayerRelation::Any,
                            }),
                            min: 1,
                            max: 1,
                            ..base_spec.clone()
                        },
                        TargetSpec {
                            kind: TargetKind::Object,
                            filter: ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                extra: vec![mtg_ir::ObjectPredicate::ControllerRef {
                                    player: mtg_ir::PlayerRef::Target { index: 0 },
                                }],
                                ..Default::default()
                            },
                            min: 1,
                            max: 3,
                            same_owner: true,
                            zones: ZoneMask::GRAVEYARD,
                            ..base_spec
                        },
                    ],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plain_card, dependent_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let d0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(0),
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
        let d1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state = GameState::new(db, GameConfig::standard_1v1(), &[d0, d1], 52).unwrap();

        // P0 graveyard empty; P1 graveyard has 2 cards (so min=1, max=3 requires DoneTargets after 1 pick).
        let p1_card_0 = state.players()[1].hand[0];
        let p1_card_1 = state.players()[1].hand[1];
        let p1_gy_card = match move_object(
            &mut state,
            p1_card_0,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };
        let _p1_gy_card_2 = move_object(
            &mut state,
            p1_card_1,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let spell_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let spell_id = if state.object(spell_id).unwrap().zone != Zone::Hand {
            match move_object(
                &mut state,
                spell_id,
                Zone::Hand,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        } else {
            spell_id
        };
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 1, 0];

        begin_cast_spell(&mut state, PlayerId(0), spell_id, 0, Zone::Hand).unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);

        let acts = legal_casting_actions(&state, PlayerId(0));
        assert!(
            !acts.contains(&Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(0) }
            }),
            "must NOT offer PlayerId(0) when P0 graveyard is empty; acts={acts:?}"
        );
        assert!(acts.contains(&Action::SelectTarget {
            target: TargetRef::Player { id: PlayerId(1) }
        }));

        let cs_before = state.casting().cloned();
        let err = apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(0) },
            },
        );
        assert!(matches!(err, Err(EngineError::IllegalAction(_))));
        assert_eq!(state.casting(), cs_before.as_ref());

        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Player { id: PlayerId(1) },
            },
        )
        .unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: p1_gy_card },
            },
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::DoneTargets).unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.stack_entries().len(), 1);
    }

    #[test]
    fn choose_targets_lookahead_blocks_done_targets_and_starving_optional_picks() {
        let tables = InternTables::default();
        let creature_card = CardDef {
            oracle_id: "t_lk_cr".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Elk".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Elk",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let base_spec = TargetSpec {
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
            zones: ZoneMask::empty(),
        };
        // Subcase A: spec0 (min=0, max=1 GY card) + spec1 (min=1, max=1, shares_card_type_with_target=Some(0))
        let share_spell = CardDef {
            oracle_id: "t_lk_share".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Shared Recall".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Shared Recall",
                ManaCost::parse("{U}"),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![
                        TargetSpec {
                            filter: ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                ..Default::default()
                            },
                            min: 0,
                            max: 1,
                            zones: ZoneMask::GRAVEYARD,
                            ..base_spec.clone()
                        },
                        TargetSpec {
                            filter: ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                ..Default::default()
                            },
                            min: 1,
                            max: 1,
                            shares_card_type_with_target: Some(0),
                            zones: ZoneMask::GRAVEYARD,
                            ..base_spec.clone()
                        },
                    ],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Subcase B: spec0 (min=0, max=1 BF creature) + spec1 (min=1, max=1, other_target=true BF creature)
        let other_spell = CardDef {
            oracle_id: "t_lk_other".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Twin Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Twin Bolt",
                ManaCost::parse("{R}"),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![
                        TargetSpec {
                            filter: ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            },
                            min: 0,
                            max: 1,
                            zones: ZoneMask::BATTLEFIELD,
                            ..base_spec.clone()
                        },
                        TargetSpec {
                            filter: ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            },
                            min: 1,
                            max: 1,
                            other_target: true,
                            zones: ZoneMask::BATTLEFIELD,
                            ..base_spec
                        },
                    ],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![creature_card, share_spell, other_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let d0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(2),
                CardDefId(2),
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
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[d0.clone(), d0], 53).unwrap();

        let move_def_to = |st: &mut GameState, def: CardDefId, dest: Zone| -> ObjectId {
            let id = st.players()[0]
                .hand
                .iter()
                .chain(st.players()[0].library.iter())
                .copied()
                .find(|&oid| st.object(oid).unwrap().card_def == def)
                .unwrap();
            match move_object(st, id, dest, MoveRawOptions::default(), None).unwrap() {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            }
        };

        // Subcase A setup: 2 creatures in GY
        let gy_c1 = move_def_to(&mut state, CardDefId(0), Zone::Graveyard);
        let gy_c2 = move_def_to(&mut state, CardDefId(0), Zone::Graveyard);
        let share_id = move_def_to(&mut state, CardDefId(1), Zone::Hand);

        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 2, 0, 2, 0, 0];
        begin_cast_spell(&mut state, PlayerId(0), share_id, 0, Zone::Hand).unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);
        assert_eq!(state.casting().unwrap().target_spec_cursor, 0);

        let acts_a = legal_casting_actions(&state, PlayerId(0));
        assert!(
            !acts_a.contains(&Action::DoneTargets),
            "DoneTargets at spec0 must be blocked when spec1 shares_card_type_with_target(0); acts={acts_a:?}"
        );
        assert!(acts_a.contains(&Action::SelectTarget {
            target: TargetRef::Object { id: gy_c1 }
        }));
        assert!(acts_a.contains(&Action::SelectTarget {
            target: TargetRef::Object { id: gy_c2 }
        }));
        assert!(matches!(
            apply_casting_action(&mut state, PlayerId(0), &Action::DoneTargets),
            Err(EngineError::IllegalAction(_))
        ));
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: gy_c1 },
            },
        )
        .unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: gy_c2 },
            },
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();

        // Subcase B setup: 1 creature bf_c on BF
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        let bf_c = move_def_to(&mut state, CardDefId(0), Zone::Battlefield);
        let ensure_in_hand = |st: &mut GameState, def: CardDefId| -> ObjectId {
            let id = st.players()[0]
                .hand
                .iter()
                .chain(st.players()[0].library.iter())
                .copied()
                .find(|&oid| st.object(oid).unwrap().card_def == def)
                .unwrap();
            if st.object(id).unwrap().zone == Zone::Hand {
                id
            } else {
                match move_object(st, id, Zone::Hand, MoveRawOptions::default(), None).unwrap() {
                    MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                    _ => unreachable!(),
                }
            }
        };
        let other_id_1 = ensure_in_hand(&mut state, CardDefId(2));

        assert!(can_cast_spell_option(&state, PlayerId(0), other_id_1, 0).is_some());
        begin_cast_spell(&mut state, PlayerId(0), other_id_1, 0, Zone::Hand).unwrap();
        // Auto-advanced past spec0 (since picking bf_c for spec0 would starve spec1) straight to spec1!
        assert_eq!(state.casting().unwrap().step, CastStep::ChooseTargets);
        assert_eq!(state.casting().unwrap().target_spec_cursor, 1);
        let acts_b = legal_casting_actions(&state, PlayerId(0));
        assert!(acts_b.contains(&Action::SelectTarget {
            target: TargetRef::Object { id: bf_c }
        }));
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: bf_c },
            },
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();

        // MacroAction::CastSpell with [bf_c] also completes cleanly using the second copy
        let other_id_2 = ensure_in_hand(&mut state, CardDefId(2));
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        apply_macro_cast_spell(
            &mut state,
            PlayerId(0),
            &MacroAction::CastSpell {
                object: other_id_2,
                option_id: 0,
                from_zone: Zone::Hand,
                modes: vec![],
                x_value: None,
                targets: vec![TargetRef::Object { id: bf_c }],
            },
        )
        .unwrap();
        assert_eq!(state.stack_entries().len(), 1);
    }

    #[test]
    fn choose_modes_evaluates_dynamic_max_when_static_max_zero() {
        let tables = InternTables::default();
        let dyn_modal_spell = CardDef {
            oracle_id: "t_dyn_modal".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dynamic Charm".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Dynamic Charm",
                ManaCost::parse("{U}"),
                CardTypeMask::INSTANT,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 1,
                        max: 0,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![
                            ModalMode {
                                targets: vec![],
                                effect: vec![],
                                span: Default::default(),
                            },
                            ModalMode {
                                targets: vec![],
                                effect: vec![],
                                span: Default::default(),
                            },
                        ],
                        per_mode_costs: vec![],
                        unchosen_permanent: false,
                        opponent_chooses: None,
                        pawprint_budget: None,
                        pawprint_costs: vec![],
                        dynamic_max: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
                        random_mode: false,
                        prefix_effect: vec![],
                        suffix_effect: vec![],
                        conditional_choose_range: None,
                        conditional_not_random: None,
                        different_player_targets_per_mode: false,
                    }),
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![dyn_modal_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 54).unwrap();
        let card_id = state.players()[0].hand[0];
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 0];

        begin_cast_spell(&mut state, PlayerId(0), card_id, 0, Zone::Hand).unwrap();

        assert!(
            matches!(
                state.decision(),
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseMode {
                        option_count: 2,
                        min: 1,
                        max: 1,
                        ..
                    }
                }
            ),
            "expected ChooseMode prompt with min=1, max=1 from dynamic_max; got {:?}",
            state.decision()
        );
    }

    fn make_synth_activated_ability(
        id: &str,
        cost: Vec<CostDef>,
        effect: Vec<mtg_ir::EffectStep>,
        timing: mtg_ir::Timing,
        is_mana: bool,
    ) -> AbilityDef {
        AbilityDef::Activated(mtg_ir::ActivatedAbility {
            id: id.into(),
            cost,
            targets: vec![],
            effect,
            modal: None,
            timing,
            is_mana,
            activation_restrictions: vec![],
            activation_condition: None,
            activator_who: None,
            power_up_reduction: false,
            x_cant_be_zero: false,
            activation_cost_reduction: None,
            activation_zones: ZoneMask::empty(),
            cost_keyword: None,
            intrinsic: false,
            text: None,
            grant_condition: None,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
            span: Default::default(),
        })
    }

    fn make_synth_step(subject: mtg_ir::Selector, op: mtg_ir::EffectOp) -> mtg_ir::EffectStep {
        mtg_ir::EffectStep {
            subject,
            op,
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

    fn add_synth_bf(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            crate::state::GameObject::new(def, player, Zone::Battlefield, ts, ObjectFlags::empty()),
            None,
        );
        state.player_mut(player).unwrap().battlefield.push(oid);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        oid
    }

    #[test]
    fn synth_auto_pay_mana_excludes_self_tap_and_uses_other_land() {
        let mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let draw_step = make_synth_step(
            mtg_ir::Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::You,
                },
            },
            mtg_ir::EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
        );
        let synth_rock = CardDef {
            oracle_id: "c_synth_rock".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Rock".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Rock",
                ManaCost::parse("{2}"),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![
                    make_synth_activated_ability(
                        "mana",
                        vec![CostDef::TapSelf],
                        vec![mana_step],
                        mtg_ir::Timing::Mana,
                        true,
                    ),
                    make_synth_activated_ability(
                        "draw",
                        vec![
                            CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            },
                            CostDef::TapSelf,
                            CostDef::Sacrifice {
                                count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                target: mtg_ir::CostObjectTarget::SelfObj,
                                sacrifice_all: false,
                                half_rounded_up: false,
                                attached_creature: false,
                                one_or_more: false,
                                up_to: None,
                                different_names: false,
                            },
                        ],
                        vec![draw_step],
                        mtg_ir::Timing::Instant,
                        false,
                    ),
                ],
            )],
            fully_supported: true,
        };
        let synth_swamp = CardDef {
            oracle_id: "c_synth_swamp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Swamp",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "mana_b",
                    vec![CostDef::TapSelf],
                    vec![make_synth_step(
                        mtg_ir::Selector::SelfObj,
                        mtg_ir::EffectOp::AddMana {
                            production: mtg_ir::ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[ManaColor::B]),
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                    )],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![synth_rock, synth_swamp],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 20],
            sideboard: vec![],
        };

        // Negative check: with ONLY Synth Rock on battlefield, BeginActivate for Ability 1 is NOT legal.
        let mut neg_state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck.clone()],
            101,
        )
        .unwrap();
        neg_state.set_active_player(PlayerId(0));
        neg_state.set_phase(mtg_ir::GamePhase::Main1);
        neg_state.set_priority_player(PlayerId(0));
        neg_state.set_decision(DecisionState::Priority);
        let neg_rock = add_synth_bf(&mut neg_state, PlayerId(0), CardDefId(0));
        let neg_actions = crate::legality::collect_priority_actions(&neg_state, PlayerId(0));
        assert!(
            !neg_actions.contains(&Action::BeginActivate {
                object: neg_rock,
                ability_index: 1,
            }),
            "BeginActivate must NOT be legal when Synth Rock is the only mana source"
        );

        // Positive check: Synth Rock + Synth Swamp -> TapMana(rock) excluded & rejected; AutoPayMana taps Swamp and resolves to stack.
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 102).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);
        let rock = add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
        let swamp = add_synth_bf(&mut state, PlayerId(0), CardDefId(1));

        let prio = crate::legality::collect_priority_actions(&state, PlayerId(0));
        assert!(prio.contains(&Action::BeginActivate {
            object: rock,
            ability_index: 1,
        }));

        begin_activate_ability(&mut state, PlayerId(0), rock, 1).unwrap();
        let cast_actions = legal_casting_actions(&state, PlayerId(0));
        assert!(cast_actions.contains(&Action::AutoPayMana));
        assert!(
            !cast_actions.iter().any(|a| matches!(
                a,
                Action::TapMana {
                    object,
                    ability_index: 0,
                    ..
                } if *object == rock
            )),
            "TapMana for Synth Rock itself must be excluded while TapSelf is pending"
        );

        let tap_err = apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::TapMana {
                object: rock,
                ability_index: 0,
                preferred_color: None,
            },
        );
        assert!(
            matches!(tap_err, Err(EngineError::IllegalAction(_))),
            "manually tapping Synth Rock for mana must fail with IllegalAction"
        );

        apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
        assert!(
            state
                .object(swamp)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED),
            "AutoPayMana must tap Synth Swamp"
        );
        assert_eq!(state.stack().len(), 1);
        assert_eq!(state.players()[0].graveyard.len(), 1);
    }

    #[test]
    fn synth_auto_pay_mana_allows_self_tap_when_only_sac_self_no_tap_cost() {
        let draw_step = make_synth_step(
            mtg_ir::Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::You,
                },
            },
            mtg_ir::EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
        );
        let synth_sac_relic = CardDef {
            oracle_id: "c_synth_sac_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth SacRelic".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth SacRelic",
                ManaCost::parse("{1}"),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![
                    make_synth_activated_ability(
                        "mana",
                        vec![CostDef::TapSelf],
                        vec![make_synth_step(
                            mtg_ir::Selector::SelfObj,
                            mtg_ir::EffectOp::AddMana {
                                production: mtg_ir::ManaProduction::Fixed {
                                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                                },
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                                delayed_rider: None,
                            },
                        )],
                        mtg_ir::Timing::Mana,
                        true,
                    ),
                    make_synth_activated_ability(
                        "draw_no_tap",
                        vec![
                            CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            },
                            CostDef::Sacrifice {
                                count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                target: mtg_ir::CostObjectTarget::SelfObj,
                                sacrifice_all: false,
                                half_rounded_up: false,
                                attached_creature: false,
                                one_or_more: false,
                                up_to: None,
                                different_names: false,
                            },
                        ],
                        vec![draw_step],
                        mtg_ir::Timing::Instant,
                        false,
                    ),
                ],
            )],
            fully_supported: true,
        };
        let filler_land = CardDef {
            oracle_id: "c_filler".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plainland".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Plainland",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![synth_sac_relic, filler_land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 103).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        let relic = add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
        let prio = crate::legality::collect_priority_actions(&state, PlayerId(0));
        assert!(
            prio.contains(&Action::BeginActivate {
                object: relic,
                ability_index: 1,
            }),
            "Sacrifice(SelfObj) without TapSelf must allow tapping self for mana first"
        );

        begin_activate_ability(&mut state, PlayerId(0), relic, 1).unwrap();
        let cast_actions = legal_casting_actions(&state, PlayerId(0));
        assert!(cast_actions.contains(&Action::AutoPayMana));
        apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();

        assert_eq!(state.stack().len(), 1);
        assert_eq!(state.players()[0].graveyard.len(), 1);
        assert!(state.players()[0].battlefield.is_empty());
    }

    #[test]
    fn synth_auto_pay_mana_excludes_self_sac_mana_ability_when_sac_self_cost() {
        let sac_self_cost = CostDef::Sacrifice {
            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
            target: mtg_ir::CostObjectTarget::SelfObj,
            sacrifice_all: false,
            half_rounded_up: false,
            attached_creature: false,
            one_or_more: false,
            up_to: None,
            different_names: false,
        };
        let synth_dual_sac = CardDef {
            oracle_id: "c_synth_dual_sac".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth DualSac".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth DualSac",
                ManaCost::parse("{1}"),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![
                    make_synth_activated_ability(
                        "mana_sac",
                        vec![sac_self_cost.clone()],
                        vec![make_synth_step(
                            mtg_ir::Selector::SelfObj,
                            mtg_ir::EffectOp::AddMana {
                                production: mtg_ir::ManaProduction::Fixed {
                                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                                },
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                                delayed_rider: None,
                            },
                        )],
                        mtg_ir::Timing::Mana,
                        true,
                    ),
                    make_synth_activated_ability(
                        "draw_sac",
                        vec![
                            CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            },
                            sac_self_cost,
                        ],
                        vec![make_synth_step(
                            mtg_ir::Selector::Player {
                                filter: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::You,
                                },
                            },
                            mtg_ir::EffectOp::Draw {
                                up_to: false,
                                reveal_drawn: false,
                            },
                        )],
                        mtg_ir::Timing::Instant,
                        false,
                    ),
                ],
            )],
            fully_supported: true,
        };
        let synth_swamp = CardDef {
            oracle_id: "c_synth_swamp_ds".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Swamp",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "mana_b",
                    vec![CostDef::TapSelf],
                    vec![make_synth_step(
                        mtg_ir::Selector::SelfObj,
                        mtg_ir::EffectOp::AddMana {
                            production: mtg_ir::ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[ManaColor::B]),
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                    )],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![synth_dual_sac, synth_swamp],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 20],
            sideboard: vec![],
        };

        // Negative: only Synth DualSac on battlefield -> cannot use sac-self mana ability to pay {1} + sac self.
        let mut neg_state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck.clone()],
            104,
        )
        .unwrap();
        neg_state.set_active_player(PlayerId(0));
        neg_state.set_phase(mtg_ir::GamePhase::Main1);
        neg_state.set_priority_player(PlayerId(0));
        neg_state.set_decision(DecisionState::Priority);
        let neg_ds = add_synth_bf(&mut neg_state, PlayerId(0), CardDefId(0));
        assert!(
            !crate::legality::collect_priority_actions(&neg_state, PlayerId(0)).contains(
                &Action::BeginActivate {
                    object: neg_ds,
                    ability_index: 1,
                }
            )
        );

        // Positive: DualSac + Swamp -> excludes DualSac's Ability 0, taps Swamp, sacrifices DualSac to Ability 1.
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 105).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);
        let dual_sac = add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
        let swamp = add_synth_bf(&mut state, PlayerId(0), CardDefId(1));

        begin_activate_ability(&mut state, PlayerId(0), dual_sac, 1).unwrap();
        let cast_actions = legal_casting_actions(&state, PlayerId(0));
        assert!(
            !cast_actions.iter().any(|a| matches!(
                a,
                Action::TapMana {
                    object,
                    ability_index: 0,
                    ..
                } if *object == dual_sac
            )),
            "self-sacrificing mana ability must be excluded while Sacrifice(SelfObj) is pending"
        );
        apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
        assert!(
            state
                .object(swamp)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        assert_eq!(state.stack().len(), 1);
        assert_eq!(state.players()[0].graveyard.len(), 1);
    }

    #[test]
    fn synth_cost_object_selection_min_2_and_min_10_via_legal_actions() {
        let draw_step = make_synth_step(
            mtg_ir::Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::You,
                },
            },
            mtg_ir::EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
        );
        let synth_sac2 = CardDef {
            oracle_id: "c_synth_sac2".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Sac2".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Sac2",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "sac2",
                    vec![CostDef::Sacrifice {
                        count: Box::new(mtg_ir::Amount::Const { n: 2 }),
                        target: mtg_ir::CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        sacrifice_all: false,
                        half_rounded_up: false,
                        attached_creature: false,
                        one_or_more: false,
                        up_to: None,
                        different_names: false,
                    }],
                    vec![draw_step.clone()],
                    mtg_ir::Timing::Instant,
                    false,
                )],
            )],
            fully_supported: true,
        };
        let synth_citadel = CardDef {
            oracle_id: "c_synth_citadel".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Citadel".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Citadel",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "sac10",
                    vec![
                        CostDef::TapSelf,
                        CostDef::Sacrifice {
                            count: Box::new(mtg_ir::Amount::Const { n: 10 }),
                            target: mtg_ir::CostObjectTarget::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_none: CardTypeMask::LAND,
                                    flags: mtg_ir::FilterFlags::OTHER,
                                    ..Default::default()
                                }),
                            },
                            sacrifice_all: false,
                            half_rounded_up: false,
                            attached_creature: false,
                            one_or_more: false,
                            up_to: None,
                            different_names: false,
                        },
                    ],
                    vec![draw_step.clone()],
                    mtg_ir::Timing::Instant,
                    false,
                )],
            )],
            fully_supported: true,
        };
        let synth_up_to_5 = CardDef {
            oracle_id: "c_synth_upto5".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth UpTo5".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth UpTo5",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "upto5",
                    vec![CostDef::Sacrifice {
                        count: Box::new(mtg_ir::Amount::Const { n: 0 }),
                        target: mtg_ir::CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        sacrifice_all: false,
                        half_rounded_up: false,
                        attached_creature: false,
                        one_or_more: false,
                        up_to: Some(5),
                        different_names: false,
                    }],
                    vec![draw_step],
                    mtg_ir::Timing::Instant,
                    false,
                )],
            )],
            fully_supported: true,
        };
        let synth_bear = CardDef {
            oracle_id: "c_synth_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let synth_relic = CardDef {
            oracle_id: "c_synth_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Relic",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    synth_sac2,
                    synth_citadel,
                    synth_up_to_5,
                    synth_bear,
                    synth_relic,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(3); 20],
            sideboard: vec![],
        };

        // Subtest A: min = 2, max = 2 with 3 Synth Bears, driven purely through legal_choice_actions.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                106,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let sac2 = add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let b0 = add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            let b1 = add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            let b2 = add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            let _ = (b0, b1, b2);

            begin_activate_ability(&mut state, PlayerId(0), sac2, 0).unwrap();

            // Pick 1
            let acts0 = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(
                !acts0.iter().any(|a| matches!(
                    a,
                    Action::Answer {
                        answer: ChoiceAnswer::DonePicking,
                        ..
                    }
                )),
                "DonePicking must not be offered at 0/2"
            );
            let Action::Answer {
                prompt_id: pid0,
                answer: ans0,
            } = acts0
                .into_iter()
                .find(|a| {
                    matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::PickObject { .. },
                            ..
                        }
                    )
                })
                .unwrap()
            else {
                unreachable!()
            };
            crate::choices::apply_choice_answer(&mut state, PlayerId(0), pid0, ans0).unwrap();

            // After pick 1 (1/2): must offer remaining 2 bears and NO DonePicking
            let acts1 = crate::choices::legal_choice_actions(&state, PlayerId(0));
            let pick_count_1 = acts1
                .iter()
                .filter(|a| {
                    matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::PickObject { .. },
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(pick_count_1, 2, "must offer remaining 2 bears at 1/2");
            assert!(
                !acts1.iter().any(|a| matches!(
                    a,
                    Action::Answer {
                        answer: ChoiceAnswer::DonePicking,
                        ..
                    }
                )),
                "DonePicking must not be offered at 1/2"
            );

            let Action::Answer {
                prompt_id: pid1,
                answer: ans1,
            } = acts1
                .into_iter()
                .find(|a| {
                    matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::PickObject { .. },
                            ..
                        }
                    )
                })
                .unwrap()
            else {
                unreachable!()
            };
            crate::choices::apply_choice_answer(&mut state, PlayerId(0), pid1, ans1).unwrap();
            assert_eq!(state.stack().len(), 1);
            assert!(state.pending_prompt().is_none());
        }

        // Subtest B: min = 10, max = 10 (Citadel parity) driven all 10 picks via legal_choice_actions.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                107,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let citadel = add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            for _ in 0..10 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(4));
            }
            begin_activate_ability(&mut state, PlayerId(0), citadel, 0).unwrap();

            for pick_idx in 0..10 {
                let acts = crate::choices::legal_choice_actions(&state, PlayerId(0));
                assert!(
                    !acts.iter().any(|a| matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::DonePicking,
                            ..
                        }
                    )),
                    "DonePicking must not appear at pick {pick_idx}/10"
                );
                let pick_act = acts.into_iter().find(|a| {
                    matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::PickObject { .. },
                            ..
                        }
                    )
                });
                let Some(Action::Answer { prompt_id, answer }) = pick_act else {
                    panic!("expected PickObject at pick {pick_idx}/10");
                };
                crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, answer)
                    .unwrap();
            }
            assert_eq!(state.stack().len(), 1);
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.players()[0].graveyard.len(), 10);
        }

        // Subtest C: up_to: Some(5) with 2 candidates (min = 0, max = 5).
        {
            let mut state =
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 108).unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let up5 = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(3));

            begin_activate_ability(&mut state, PlayerId(0), up5, 0).unwrap();
            for pick_idx in 0..2 {
                let acts = crate::choices::legal_choice_actions(&state, PlayerId(0));
                assert!(
                    acts.iter().any(|a| matches!(
                        a,
                        Action::Answer {
                            answer: ChoiceAnswer::DonePicking,
                            ..
                        }
                    )),
                    "DonePicking must be offered for up_to at pick {pick_idx}"
                );
                let Action::Answer { prompt_id, answer } = acts
                    .into_iter()
                    .find(|a| {
                        matches!(
                            a,
                            Action::Answer {
                                answer: ChoiceAnswer::PickObject { .. },
                                ..
                            }
                        )
                    })
                    .unwrap()
                else {
                    unreachable!()
                };
                crate::choices::apply_choice_answer(&mut state, PlayerId(0), prompt_id, answer)
                    .unwrap();
            }
            assert_eq!(state.stack().len(), 1);
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.players()[0].graveyard.len(), 2);
        }
    }

    #[test]
    fn synth_optional_one_or_more_sacrifice_unpayable_skips_prompt_and_guards_actions() {
        let plumb_card = CardDef {
            oracle_id: "c_synth_plumb".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plumb".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Plumb",
                ManaCost::parse("{1}{B}"),
                CardTypeMask::INSTANT,
                ColorMask::B,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Optional {
                        cost: vec![CostDef::Sacrifice {
                            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                            target: mtg_ir::CostObjectTarget::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(mtg_ir::RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            sacrifice_all: false,
                            half_rounded_up: false,
                            attached_creature: false,
                            one_or_more: true,
                            up_to: None,
                            different_names: false,
                        }],
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let swamp_mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::B]),
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let synth_swamp = CardDef {
            oracle_id: "c_synth_swamp_plumb".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Swamp",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "tap_b",
                    vec![CostDef::TapSelf],
                    vec![swamp_mana_step],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };
        let synth_bear = CardDef {
            oracle_id: "c_synth_bear_plumb".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![plumb_card, synth_swamp, synth_bear],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };

        // 1. 0 creatures + 2 Synth Swamps: skips CastOptionalCost, goes straight to PayCosts, AutoPayMana commits.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                121,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            let plumb_h = state.players()[0].hand[0];

            begin_cast_spell(&mut state, PlayerId(0), plumb_h, 0, Zone::Hand).unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);

            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            let se = state.stack_entries().last().unwrap();
            assert!(!se.choices.additional_cost_paid);
            assert!(se.choices.optional_costs.is_empty());
        }

        // 2. Direct guard test with 0 creatures: YesNo(true) omitted & rejected, ToggleOptionalCost(0) rejected, YesNo(false) advances.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                122,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            let plumb_h = state.players()[0].hand[0];

            let mv = move_object(
                &mut state,
                plumb_h,
                Zone::Stack,
                MoveRawOptions {
                    to_player: Some(PlayerId(0)),
                    controller: Some(PlayerId(0)),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            let stack_obj = match mv {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap_or(plumb_h),
                _ => unreachable!(),
            };
            let opt = enumerate_cast_options_cached(
                &state,
                PlayerId(0),
                stack_obj,
                state.evaluate_board(),
            )
            .pop()
            .unwrap_or(CastOption {
                option_id: 0,
                face_idx: 0,
                from_zone: Zone::Hand,
                alt_kind: AltCastKind::Normal,
                kicker_indices: SmallVec::new(),
                is_kicked: false,
                gift_promised: false,
                fused: false,
                cleaved: false,
                spree_required: false,
                optional_cost_indices: SmallVec::new(),
                mana_cost_override: None,
                extra_costs: Vec::new(),
                permission_source: None,
            });
            state.set_casting(Some(CastingState {
                caster: PlayerId(0),
                object: stack_obj,
                face_idx: 0,
                from_zone: Zone::Hand,
                step: CastStep::ChooseOptionalCosts,
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
                cast_option: Some(Box::new(opt)),
                pre_cast_exile: None,
            }));
            assert!(matches!(
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::ToggleOptionalCost { index: 0 }
                ),
                Err(EngineError::IllegalAction(_))
            ));
            let pid = state.allocate_prompt_id();
            let req = ChoiceRequest::YesNo {
                prompt_id: pid,
                label: "Pay optional cost #0".into(),
            };
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id: pid,
                chooser: PlayerId(0),
                request: req.clone(),
                owner: PromptOwner::Casting(CastingPromptKind::CastOptionalCost {
                    cost_index: 0,
                    label: "Pay optional cost #0".into(),
                }),
            }));
            state.set_decision(DecisionState::Prompt { request: req });
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(!legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::YesNo { value: true },
            }));
            assert!(legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::YesNo { value: false },
            }));
            assert!(matches!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::YesNo { value: true }
                ),
                Err(EngineError::IllegalAction(_))
            ));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::YesNo { value: false },
            )
            .unwrap();
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
        }

        // 3. 1 Synth Bear + 2 Synth Swamps: prompts CastOptionalCost(0), accepts true, AutoPayMana, sacrifices Bear.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck],
                123,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            let bear_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            let plumb_h = state.players()[0].hand[0];

            begin_cast_spell(&mut state, PlayerId(0), plumb_h, 0, Zone::Hand).unwrap();
            let pid = state.pending_prompt().unwrap().prompt_id;
            assert!(matches!(
                state.pending_prompt().unwrap().owner,
                PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index: 0, .. })
            ));
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::YesNo { value: true },
            }));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            let obj_pid = state.pending_prompt().unwrap().prompt_id;
            assert!(matches!(
                state.pending_prompt().unwrap().owner,
                PromptOwner::Casting(CastingPromptKind::CostObjectSelection { min: 1, max: 1, .. })
            ));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                obj_pid,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.players()[0].graveyard.len(), 1);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .optional_costs
                    .as_slice(),
                &[0]
            );
        }
    }

    #[test]
    fn synth_optional_mana_life_discard_forage_payability() {
        let opt_combo = CardDef {
            oracle_id: "c_synth_opt_combo".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth OptCombo".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth OptCombo",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::G,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![
                        CostDef::Optional {
                            cost: vec![CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        },
                        CostDef::Optional {
                            cost: vec![CostDef::PayLife {
                                amount: Box::new(mtg_ir::Amount::Const { n: 3 }),
                                half_life_rounded_up: false,
                            }],
                        },
                        CostDef::Optional {
                            cost: vec![CostDef::Discard {
                                count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                target: None,
                                random: false,
                                same_name_pair: false,
                                same_name_as_controlled: None,
                                different_names: false,
                            }],
                        },
                        CostDef::Optional {
                            cost: vec![CostDef::Choice {
                                options: vec![],
                                forage: true,
                            }],
                        },
                    ],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let conflict_spell = CardDef {
            oracle_id: "c_synth_conflict_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ConflictSpell".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth ConflictSpell",
                ManaCost::parse("{1}"),
                CardTypeMask::SORCERY,
                ColorMask::B,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Optional {
                        cost: vec![CostDef::Sacrifice {
                            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                            target: mtg_ir::CostObjectTarget::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::ARTIFACT,
                                    controller: Some(mtg_ir::RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            sacrifice_all: false,
                            half_rounded_up: false,
                            attached_creature: false,
                            one_or_more: false,
                            up_to: None,
                            different_names: false,
                        }],
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let treasure_mana = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::B]),
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let synth_treasure = CardDef {
            oracle_id: "c_synth_treasure".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Treasure".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Treasure",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "sac_b",
                    vec![
                        CostDef::TapSelf,
                        CostDef::Sacrifice {
                            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                            target: mtg_ir::CostObjectTarget::SelfObj,
                            sacrifice_all: false,
                            half_rounded_up: false,
                            attached_creature: false,
                            one_or_more: false,
                            up_to: None,
                            different_names: false,
                        },
                    ],
                    vec![treasure_mana.clone()],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };
        let synth_swamp = CardDef {
            oracle_id: "c_synth_swamp_opt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Swamp",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "tap_b",
                    vec![CostDef::TapSelf],
                    vec![treasure_mana],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![opt_combo, conflict_spell, synth_treasure, synth_swamp],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };

        // Case A: 0 mana, 2 life, 0 other cards in hand, 0 GY, 0 Foods -> all 4 skipped.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                131,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let cast_id = state.players()[0].hand[0];
            while state.players()[0].hand.len() > 1 {
                let extra = state.player_mut(PlayerId(0)).unwrap().hand.pop().unwrap();
                state.player_mut(PlayerId(0)).unwrap().library.push(extra);
            }
            state.player_mut(PlayerId(0)).unwrap().life = 2;
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);

            begin_cast_spell(&mut state, PlayerId(0), cast_id, 0, Zone::Hand).unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert!(state.pending_prompt().is_none());
            assert!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .optional_costs
                    .is_empty()
            );
        }

        // Case B: {1} in pool, 5 life, 1 spare card in hand, 3 cards in GY -> all 4 prompted and accepted.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                132,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let cast_id = state.players()[0].hand[0];
            while state.players()[0].hand.len() > 2 {
                let extra = state.player_mut(PlayerId(0)).unwrap().hand.pop().unwrap();
                state.player_mut(PlayerId(0)).unwrap().library.push(extra);
            }
            for _ in 0..3 {
                let lib_card = state
                    .player_mut(PlayerId(0))
                    .unwrap()
                    .library
                    .pop()
                    .unwrap();
                move_object(
                    &mut state,
                    lib_card,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap();
            }
            state.player_mut(PlayerId(0)).unwrap().life = 5;
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [1, 0, 0, 0, 0, 0];
            state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);

            begin_cast_spell(&mut state, PlayerId(0), cast_id, 0, Zone::Hand).unwrap();
            for expected_idx in 0..4u16 {
                let pid = state.pending_prompt().unwrap().prompt_id;
                assert!(matches!(
                    state.pending_prompt().unwrap().owner,
                    PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index, .. })
                        if cost_index == expected_idx
                ));
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::YesNo { value: true },
                )
                .unwrap();
            }
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
            apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
            if let Some(pp) = state.pending_prompt() {
                let pid = pp.prompt_id;
                let spare = state.players()[0].hand[0];
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::PickObject { id: spare },
                )
                .unwrap();
            }
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.players()[0].life, 2);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .optional_costs
                    .as_slice(),
                &[0, 1, 2, 3]
            );
        }

        // Case C: Combined conflict ({1} spell + Optional Sacrifice(Artifact)):
        // only Synth Treasure -> skipped; Synth Treasure + Synth Swamp -> prompted and accepted.
        let conflict_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 10],
            sideboard: vec![],
        };
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[conflict_deck.clone(), conflict_deck.clone()],
                133,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            let spell_h = state.players()[0].hand[0];
            begin_cast_spell(&mut state, PlayerId(0), spell_h, 0, Zone::Hand).unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .optional_costs
                    .is_empty()
            );
        }
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[conflict_deck.clone(), conflict_deck],
                134,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let treas = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            let spell_h = state.players()[0].hand[0];
            begin_cast_spell(&mut state, PlayerId(0), spell_h, 0, Zone::Hand).unwrap();
            let pid = state.pending_prompt().unwrap().prompt_id;
            assert!(matches!(
                state.pending_prompt().unwrap().owner,
                PromptOwner::Casting(CastingPromptKind::CastOptionalCost { cost_index: 0, .. })
            ));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::YesNo { value: true },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            let sac_pid = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                sac_pid,
                ChoiceAnswer::PickObject { id: treas },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert!(!state.players()[0].battlefield.contains(&treas));
            assert_eq!(state.players()[0].graveyard.len(), 1);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .optional_costs
                    .as_slice(),
                &[0]
            );
        }
    }

    #[test]
    fn synth_filter_land_tap_mana_pay_costs_and_priority() {
        let filter_mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::W, ManaColor::U]),
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let filter_land = CardDef {
            oracle_id: "c_synth_filter_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth FilterLand".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth FilterLand",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "filter_wu",
                    vec![
                        CostDef::Mana {
                            cost: ManaCost::parse("{1}"),
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                        },
                        CostDef::TapSelf,
                    ],
                    vec![filter_mana_step],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };
        let island_mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::U]),
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let synth_island = CardDef {
            oracle_id: "c_synth_island_fl".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Island".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Island",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "tap_u",
                    vec![CostDef::TapSelf],
                    vec![island_mana_step],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };
        let synth_spell = CardDef {
            oracle_id: "c_synth_wu_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Spell",
                ManaCost::parse("{W}{U}"),
                CardTypeMask::INSTANT,
                ColorMask::W | ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let creature_only_mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                restriction: None,
                restriction_spec: Some(Box::new(mtg_ir::ManaSpendingRestriction {
                    allow_spells: Some(mtg_ir::AllowSpellsRestriction {
                        filter: Some(mtg_ir::PlainManaFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        let creature_only_land = CardDef {
            oracle_id: "c_synth_creature_land".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth CreatureOnlyLand".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth CreatureOnlyLand",
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "tap_g_creature",
                    vec![CostDef::TapSelf],
                    vec![creature_only_mana_step],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![filter_land, synth_island, synth_spell, creature_only_land],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(2); 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 141).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        let fl_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
        let isl_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
        let cl_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
        let spell_h = state.players()[0].hand[0];

        // 1. Priority with empty pool: Island & CreatureOnlyLand emitted, FilterLand omitted.
        let prio0 = crate::legality::collect_priority_actions(&state, PlayerId(0));
        assert!(prio0.contains(&Action::TapMana {
            object: isl_bf,
            ability_index: 0,
            preferred_color: None,
        }));
        assert!(!prio0.contains(&Action::TapMana {
            object: fl_bf,
            ability_index: 0,
            preferred_color: None,
        }));

        // 2. Tap CreatureOnlyLand -> pool has only Creature-restricted {G} -> FilterLand STILL omitted in priority & PayCosts.
        activate_mana_ability(&mut state, PlayerId(0), cl_bf, 0, None).unwrap();
        let prio1 = crate::legality::collect_priority_actions(&state, PlayerId(0));
        assert!(!prio1.contains(&Action::TapMana {
            object: fl_bf,
            ability_index: 0,
            preferred_color: None,
        }));

        begin_cast_spell(&mut state, PlayerId(0), spell_h, 0, Zone::Hand).unwrap();
        assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
        let cast_acts0 = legal_casting_actions(&state, PlayerId(0));
        assert!(cast_acts0.contains(&Action::AutoPayMana));
        assert!(cast_acts0.contains(&Action::TapMana {
            object: isl_bf,
            ability_index: 0,
            preferred_color: None,
        }));
        assert!(!cast_acts0.contains(&Action::TapMana {
            object: fl_bf,
            ability_index: 0,
            preferred_color: None,
        }));

        // 3. Tap Island inside PayCosts -> pool has unrestricted {U} -> FilterLand TapMana IS emitted, activates to {W}{U}, PayFromPool succeeds.
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::TapMana {
                object: isl_bf,
                ability_index: 0,
                preferred_color: None,
            },
        )
        .unwrap();
        let cast_acts1 = legal_casting_actions(&state, PlayerId(0));
        assert!(cast_acts1.contains(&Action::TapMana {
            object: fl_bf,
            ability_index: 0,
            preferred_color: None,
        }));
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::TapMana {
                object: fl_bf,
                ability_index: 0,
                preferred_color: None,
            },
        )
        .unwrap();
        assert!(
            state
                .object(fl_bf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);
        assert_eq!(state.stack().len(), 1);
    }

    #[test]
    fn synth_titan_modal_trigger_filters_untargetable_mode_cr_700_2a() {
        let make_titan_etb = |modes: Vec<ModalMode>| mtg_ir::TriggeredAbility {
            trigger: mtg_ir::TriggerDef {
                event: mtg_ir::EventKind::ZoneChange,
                filter: mtg_ir::EventFilter {
                    or_self: true,
                    detail: Some(mtg_ir::EventDetail::ZoneChange {
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
                scope: None,
                from_zones: None,
                only_once_per_turn: false,
                do_this_once_per_turn: false,
                is_state_trigger: false,
                or: vec![],
                chapter_numbers: SmallVec::new(),
                is_chapter_ability: false,
                spell_delayed: None,
            },
            condition: None,
            grant_condition: None,
            targets: vec![],
            effect: vec![],
            modal: Some(ModalDef {
                min: 2,
                max: 2,
                allow_repeat: false,
                once_per_turn_unique: false,
                up_to: false,
                conditional_choose_all: None,
                modes,
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
            }),
            text: None,
            span: Default::default(),
        };
        let target_art_ench_mode = ModalMode {
            targets: vec![TargetSpec {
                kind: TargetKind::Object,
                filter: ObjectFilter {
                    types_any: CardTypeMask::ARTIFACT | CardTypeMask::ENCHANTMENT,
                    ..Default::default()
                },
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
                zones: ZoneMask::empty(),
            }],
            effect: vec![],
            span: Default::default(),
        };
        let plain_mode = ModalMode {
            targets: vec![],
            effect: vec![],
            span: Default::default(),
        };
        let titan_4 = CardDef {
            oracle_id: "c_synth_titan_4".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Titan".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Titan",
                ManaCost::parse("{4}{G}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![AbilityDef::Triggered(make_titan_etb(vec![
                    target_art_ench_mode.clone(),
                    plain_mode.clone(),
                    plain_mode.clone(),
                    plain_mode.clone(),
                ]))],
            )],
            fully_supported: true,
        };
        let titan_2 = CardDef {
            oracle_id: "c_synth_titan_2".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Titan TwoModes".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Titan TwoModes",
                ManaCost::parse("{4}{G}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![AbilityDef::Triggered(make_titan_etb(vec![
                    target_art_ench_mode,
                    plain_mode,
                ]))],
            )],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![titan_4, titan_2],
                decks: vec![],
            })
            .unwrap(),
        );

        // 1. 4 modes (Mode 0 untargetable, Modes 1..=3 legal) -> Mode 0 excluded and rejected; picking 1 + 2 commits.
        {
            let deck = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(0); 10],
                sideboard: vec![],
            };
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck],
                151,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let titan_h = state.players()[0].hand[0];
            move_object(
                &mut state,
                titan_h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert!(prepare_next_untargeted_stack_trigger(&mut state).unwrap());

            let pid1 = state.pending_prompt().unwrap().prompt_id;
            let legal1 = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(!legal1.contains(&Action::Answer {
                prompt_id: pid1,
                answer: ChoiceAnswer::PickMode { index: 0 },
            }));
            for idx in 1..=3u16 {
                assert!(legal1.contains(&Action::Answer {
                    prompt_id: pid1,
                    answer: ChoiceAnswer::PickMode { index: idx },
                }));
            }
            assert!(matches!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid1,
                    ChoiceAnswer::PickMode { index: 0 }
                ),
                Err(EngineError::IllegalAction(_))
            ));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid1,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            let pid2 = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid2,
                ChoiceAnswer::PickMode { index: 2 },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .modes
                    .as_slice(),
                &[1, 2]
            );
        }

        // 2. min=2, max=2 with only 1 legal mode -> trigger NOT put on stack (CR 603.3c/d).
        {
            let deck2 = DeckSpec {
                commanders: vec![],
                mainboard: vec![CardDefId(1); 10],
                sideboard: vec![],
            };
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck2.clone(), deck2],
                152,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let titan_h = state.players()[0].hand[0];
            move_object(
                &mut state,
                titan_h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert!(!prepare_next_untargeted_stack_trigger(&mut state).unwrap());
            assert!(state.stack().is_empty());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }
    }

    #[test]
    fn synth_modal_spell_and_ability_joint_legality_cr_700_2a() {
        let mut tables = InternTables::default();
        tables.keywords.push("escalate".into());
        let cr_spec = |other_target: bool| TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            },
            player: None,
            min: 1,
            max: 1,
            same_owner: false,
            condition: None,
            other_target,
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
            zones: ZoneMask::empty(),
        };
        let opp_spec = TargetSpec {
            kind: TargetKind::Player,
            filter: ObjectFilter::default(),
            player: Some(mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::Opponent,
            }),
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
            zones: ZoneMask::empty(),
        };
        let make_modal_spell = |name: &str,
                                mana: &str,
                                min: u16,
                                max: u16,
                                modes: Vec<ModalMode>,
                                per_mode_costs: Vec<Vec<CostDef>>,
                                diff_players: bool| CardDef {
            oracle_id: name.into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                name,
                ManaCost::parse(mana),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min,
                        max,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes,
                        per_mode_costs,
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
                        different_player_targets_per_mode: diff_players,
                    }),
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let mode_cr0 = ModalMode {
            targets: vec![cr_spec(false)],
            effect: vec![],
            span: Default::default(),
        };
        let mode_cr_other = ModalMode {
            targets: vec![cr_spec(true)],
            effect: vec![],
            span: Default::default(),
        };
        let mode_opp = ModalMode {
            targets: vec![opp_spec],
            effect: vec![],
            span: Default::default(),
        };
        let mode_plain = ModalMode {
            targets: vec![],
            effect: vec![],
            span: Default::default(),
        };

        // 0: 2-mode (cr0 + cr_other), 1: 3-mode (cr0 + cr_other + plain),
        // 2: 2-mode diff_players (opp + opp), 3: 3-mode diff_players (opp + opp + plain),
        // 4: Spree {1}{R} min:1 [{R}, {2}{R}], 5: Bear, 6: Modal activated ability creature (min:2, cr0 + cr_other)
        let card0 = make_modal_spell(
            "Synth ModalTwoCr",
            "{U}",
            2,
            2,
            vec![mode_cr0.clone(), mode_cr_other.clone()],
            vec![],
            false,
        );
        let card1 = make_modal_spell(
            "Synth ModalThreeCr",
            "{U}",
            2,
            2,
            vec![mode_cr0.clone(), mode_cr_other.clone(), mode_plain.clone()],
            vec![],
            false,
        );
        let card2 = make_modal_spell(
            "Synth DiffTwo",
            "{U}",
            2,
            2,
            vec![mode_opp.clone(), mode_opp.clone()],
            vec![],
            true,
        );
        let card3 = make_modal_spell(
            "Synth DiffThree",
            "{U}",
            2,
            2,
            vec![mode_opp.clone(), mode_opp, mode_plain.clone()],
            vec![],
            true,
        );
        let card4 = make_modal_spell(
            "Synth Spree",
            "{1}{R}",
            1,
            2,
            vec![mode_plain.clone(), mode_plain],
            vec![
                vec![CostDef::Mana {
                    cost: ManaCost::parse("{R}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
                vec![CostDef::Mana {
                    cost: ManaCost::parse("{2}{R}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
            ],
            false,
        );
        let card5 = CardDef {
            oracle_id: "c_synth_bear_m".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let card6 = CardDef {
            oracle_id: "c_synth_modal_ab".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ModalAdept".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth ModalAdept",
                ManaCost::parse("{U}"),
                CardTypeMask::CREATURE,
                ColorMask::U,
                vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "modal_ab".into(),
                    cost: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(ModalDef {
                        min: 2,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![mode_cr0, mode_cr_other],
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
                    }),
                    timing: mtg_ir::Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::empty(),
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![card0, card1, card2, card3, card4, card5, card6],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(4),
                CardDefId(5),
                CardDefId(6),
                CardDefId(0),
                CardDefId(0),
                CardDefId(0),
            ],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 161).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);

        let ensure_hand = |st: &mut GameState, def: CardDefId| -> ObjectId {
            let id = st.players()[0]
                .hand
                .iter()
                .chain(st.players()[0].library.iter())
                .copied()
                .find(|&id| st.object(id).unwrap().card_def == def)
                .unwrap();
            if st.object(id).unwrap().zone != Zone::Hand {
                match move_object(st, id, Zone::Hand, MoveRawOptions::default(), None).unwrap() {
                    MoveObjectOutcome::Moved(r) => r.new_id.unwrap_or(id),
                    _ => unreachable!(),
                }
            } else {
                id
            }
        };
        let c0 = ensure_hand(&mut state, CardDefId(0));
        let c1 = ensure_hand(&mut state, CardDefId(1));
        let c2 = ensure_hand(&mut state, CardDefId(2));
        let c3 = ensure_hand(&mut state, CardDefId(3));
        let c4 = ensure_hand(&mut state, CardDefId(4));
        let bear_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(5))
            .unwrap();
        let adept_h = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(6))
            .unwrap();
        let bear_bf = match move_object(
            &mut state,
            bear_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };

        // 1. 1 creature: Card0 (cr0 + cr_other) uncastable, Card1 (cr0 + cr_other + plain) castable; picking Mode 0 excludes Mode 1.
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 0];
        assert!(can_cast_spell_option(&state, PlayerId(0), c0, 0).is_none());
        assert!(can_cast_spell_option(&state, PlayerId(0), c1, 0).is_some());
        begin_cast_spell(&mut state, PlayerId(0), c1, 0, Zone::Hand).unwrap();
        let pid_c1_a = state.pending_prompt().unwrap().prompt_id;
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid_c1_a,
            ChoiceAnswer::PickMode { index: 0 },
        )
        .unwrap();
        let pid_c1_b = state.pending_prompt().unwrap().prompt_id;
        let acts_c1_b = crate::choices::legal_choice_actions(&state, PlayerId(0));
        assert!(!acts_c1_b.contains(&Action::Answer {
            prompt_id: pid_c1_b,
            answer: ChoiceAnswer::PickMode { index: 1 },
        }));
        assert!(acts_c1_b.contains(&Action::Answer {
            prompt_id: pid_c1_b,
            answer: ChoiceAnswer::PickMode { index: 2 },
        }));
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid_c1_b,
            ChoiceAnswer::PickMode { index: 2 },
        )
        .unwrap();
        apply_casting_action(
            &mut state,
            PlayerId(0),
            &Action::SelectTarget {
                target: TargetRef::Object { id: bear_bf },
            },
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        state.stack_mut().clear();
        state.stack_entries_mut().clear();
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // 2. different_player_targets_per_mode in 1v1: Card2 uncastable, Card3 castable; Mode 0 excludes Mode 1.
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 0];
        assert!(can_cast_spell_option(&state, PlayerId(0), c2, 0).is_none());
        assert!(can_cast_spell_option(&state, PlayerId(0), c3, 0).is_some());
        begin_cast_spell(&mut state, PlayerId(0), c3, 0, Zone::Hand).unwrap();
        let pid_c3_a = state.pending_prompt().unwrap().prompt_id;
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid_c3_a,
            ChoiceAnswer::PickMode { index: 0 },
        )
        .unwrap();
        let pid_c3_b = state.pending_prompt().unwrap().prompt_id;
        let acts_c3_b = crate::choices::legal_choice_actions(&state, PlayerId(0));
        assert!(!acts_c3_b.contains(&Action::Answer {
            prompt_id: pid_c3_b,
            answer: ChoiceAnswer::PickMode { index: 1 },
        }));
        assert!(matches!(
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid_c3_b,
                ChoiceAnswer::PickMode { index: 1 }
            ),
            Err(EngineError::IllegalAction(_))
        ));
        cancel_active_cast(&mut state, PlayerId(0)).unwrap();

        // 3. Spree {1}{R}: {1}{R} -> None; {1}{R}{R} -> Some, ChooseMode offers ONLY Mode 0.
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 1, 0, 1];
        assert!(can_cast_spell_option(&state, PlayerId(0), c4, 0).is_none());
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 2, 0, 1];
        assert!(can_cast_spell_option(&state, PlayerId(0), c4, 0).is_some());
        begin_cast_spell(&mut state, PlayerId(0), c4, 0, Zone::Hand).unwrap();
        let pid_sp1 = state.pending_prompt().unwrap().prompt_id;
        let acts_sp1 = crate::choices::legal_choice_actions(&state, PlayerId(0));
        assert!(acts_sp1.contains(&Action::Answer {
            prompt_id: pid_sp1,
            answer: ChoiceAnswer::PickMode { index: 0 },
        }));
        assert!(!acts_sp1.contains(&Action::Answer {
            prompt_id: pid_sp1,
            answer: ChoiceAnswer::PickMode { index: 1 },
        }));
        assert!(matches!(
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid_sp1,
                ChoiceAnswer::PickMode { index: 1 }
            ),
            Err(EngineError::IllegalAction(_))
        ));
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid_sp1,
            ChoiceAnswer::PickMode { index: 0 },
        )
        .unwrap();
        let pid_sp2 = state.pending_prompt().unwrap().prompt_id;
        crate::choices::apply_choice_answer(
            &mut state,
            PlayerId(0),
            pid_sp2,
            ChoiceAnswer::DonePicking,
        )
        .unwrap();
        apply_casting_action(&mut state, PlayerId(0), &Action::PayFromPool).unwrap();
        assert_eq!(state.decision(), &DecisionState::Priority);

        // 4. Modal activated ability with 1 creature -> None; with 2 creatures (Adept + Bear) -> Some.
        assert!(can_activate_ability(&state, PlayerId(0), bear_bf, 0).is_none());
        move_object(
            &mut state,
            bear_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let adept_bf = match move_object(
            &mut state,
            adept_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
            _ => unreachable!(),
        };
        assert!(can_activate_ability(&state, PlayerId(0), adept_bf, 0).is_none());
        add_synth_bf(&mut state, PlayerId(0), CardDefId(5));
        assert!(can_activate_ability(&state, PlayerId(0), adept_bf, 0).is_some());
    }

    #[test]
    fn synth_triggered_ability_disappeared_target_cr_603_3d() {
        let cr_target = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            },
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
            zones: ZoneMask::empty(),
        };
        let art_target = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::ARTIFACT,
                ..Default::default()
            },
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
            zones: ZoneMask::empty(),
        };
        let cast_trig_def =
            |targets: Vec<TargetSpec>, modal: Option<ModalDef>| mtg_ir::TriggeredAbility {
                trigger: mtg_ir::TriggerDef {
                    event: mtg_ir::EventKind::Cast,
                    filter: mtg_ir::EventFilter {
                        player: Some(mtg_ir::PlayerFilter {
                            relation: mtg_ir::PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: false,
                    do_this_once_per_turn: false,
                    is_state_trigger: false,
                    or: vec![],
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                },
                condition: None,
                grant_condition: None,
                targets,
                effect: vec![],
                modal,
                text: None,
                span: Default::default(),
            };
        let non_modal_src = CardDef {
            oracle_id: "c_synth_disappear_nm".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Watcher".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Watcher",
                ManaCost::default(),
                CardTypeMask::ENCHANTMENT,
                ColorMask::U,
                vec![AbilityDef::Triggered(cast_trig_def(
                    vec![cr_target.clone()],
                    None,
                ))],
            )],
            fully_supported: true,
        };
        let modal_src = CardDef {
            oracle_id: "c_synth_disappear_m".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ModalWatcher".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth ModalWatcher",
                ManaCost::default(),
                CardTypeMask::ENCHANTMENT,
                ColorMask::U,
                vec![AbilityDef::Triggered(cast_trig_def(
                    vec![],
                    Some(ModalDef {
                        min: 2,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![
                            ModalMode {
                                targets: vec![cr_target],
                                effect: vec![],
                                span: Default::default(),
                            },
                            ModalMode {
                                targets: vec![art_target],
                                effect: vec![],
                                span: Default::default(),
                            },
                        ],
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
                    }),
                ))],
            )],
            fully_supported: true,
        };
        let bear_card = CardDef {
            oracle_id: "c_synth_bear_dis".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let art_card = CardDef {
            oracle_id: "c_synth_art_dis".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Relic".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Relic",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };
        let opt_spell = CardDef {
            oracle_id: "c_synth_bolt_dis".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bolt",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![non_modal_src, modal_src, bear_card, art_card, opt_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(4); 10],
            sideboard: vec![],
        };

        // Case A: Non-modal trigger enters stack while sole Creature exists; Creature disappears before ChooseTargets.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                171,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let bear_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            let sid = state.allocate_stack_id();
            let obj = state.players()[0].hand[0];
            emit_event(
                &mut state,
                AtomicEventKind::Cast {
                    player: PlayerId(0),
                    object: obj,
                    stack_item: sid,
                    is_copy: false,
                    targets: vec![],
                    mana_spent: 0,
                    colored_mana_spent: 0,
                    kicked: false,
                    from_zone: Zone::Hand,
                },
            );
            flush_triggers(&mut state);
            assert_eq!(state.stack().len(), 1);
            move_object(
                &mut state,
                bear_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            let _ = prepare_next_untargeted_stack_trigger(&mut state).unwrap();
            assert!(state.stack().is_empty());
            assert!(state.casting().is_none());
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // Case B: Modal trigger (min:2, max:2) enters stack while Creature + Artifact exist; Creature disappears before ChooseModes.
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck],
                172,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            let bear_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(3));
            let sid = state.allocate_stack_id();
            let obj = state.players()[0].hand[0];
            emit_event(
                &mut state,
                AtomicEventKind::Cast {
                    player: PlayerId(0),
                    object: obj,
                    stack_item: sid,
                    is_copy: false,
                    targets: vec![],
                    mana_spent: 0,
                    colored_mana_spent: 0,
                    kicked: false,
                    from_zone: Zone::Hand,
                },
            );
            flush_triggers(&mut state);
            assert_eq!(state.stack().len(), 1);
            move_object(
                &mut state,
                bear_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            let _ = prepare_next_untargeted_stack_trigger(&mut state).unwrap();
            assert!(state.stack().is_empty());
            assert!(state.casting().is_none());
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }
    }

    #[test]
    fn synth_bug_k_graveyard_x_fiend_and_siblings() {
        use mtg_ir::{
            ActivatedAbility, Amount, CostObjectTarget, CounterId, CounterTarget, Timing,
        };

        let mut tables = InternTables::default();
        tables.subtypes.push("Swamp".into());
        let sub_swamp = SubtypeId(0);
        tables.subtypes.push("Mountain".into());
        let sub_mtn = SubtypeId(1);
        tables.counters.push("+1/+1".into());
        let cid_p1 = CounterId(0);

        let swamp = CardDef {
            oracle_id: "k_swamp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Swamp".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_swamp]),
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
        let mountain = CardDef {
            oracle_id: "k_mtn".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Mountain".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Mountain".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_mtn]),
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

        // Card 2: Synth Graveyard X Fiend ({X}, {T}, Exile X cards from your graveyard: Draw)
        let gy_fiend = CardDef {
            oracle_id: "k_fiend".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Graveyard X Fiend".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Graveyard X Fiend",
                ManaCost::parse("{2}{B}"),
                CardTypeMask::CREATURE,
                ColorMask::B,
                vec![AbilityDef::Activated(ActivatedAbility {
                    id: "fiend_act".into(),
                    cost: vec![
                        CostDef::Mana {
                            cost: ManaCost::parse("{X}"),
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                        },
                        CostDef::TapSelf,
                        CostDef::ExileFrom {
                            zone: Zone::Graveyard,
                            count: Box::new(Amount::X),
                            target: None,
                            from_top: false,
                            any_graveyard: false,
                            single_graveyard: false,
                            min_total_mana_value: None,
                            min_total_mana_value_amount: None,
                            min_colored_mana_symbols: None,
                            collect_evidence: false,
                            one_or_more: false,
                            total_mana_value_equals_x: false,
                            and_other_same_name_count: None,
                        },
                    ],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    timing: Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 3: Synth DoubleX Discard ({X}, Discard 2*X cards)
        let double_x_discard = CardDef {
            oracle_id: "k_dblx".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth DoubleX Discard".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth DoubleX Discard",
                ManaCost::parse("{X}"),
                CardTypeMask::SORCERY,
                ColorMask::B,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Discard {
                        count: Box::new(Amount::Mul {
                            factor: 2,
                            of: Box::new(Amount::X),
                        }),
                        target: None,
                        random: false,
                        same_name_pair: false,
                        same_name_as_controlled: None,
                        different_names: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 4: Synth X Targets ({X}{R}, dynamic_count: X, !dynamic_up_to)
        let x_targets_spell = CardDef {
            oracle_id: "k_xtgt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth X Targets".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth X Targets",
                ManaCost::parse("{X}{R}"),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 0,
                        max: 99,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: None,
                        with_single_target: false,
                        dynamic_count: Some(Box::new(Amount::X)),
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 5: Synth Counter Remover (Remove 3 +1/+1 counters from a creature you control, !among)
        let counter_remover = CardDef {
            oracle_id: "k_cnt".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Counter Remover".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Counter Remover",
                ManaCost::default(),
                CardTypeMask::SORCERY,
                ColorMask::B,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::RemoveCounters {
                        counter: CounterTarget::Specific(cid_p1),
                        count: Box::new(Amount::Const { n: 3 }),
                        remove_all: false,
                        from: CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        among: false,
                        one_or_more: false,
                        or_suspended_you_own: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 6: Synth Bear
        let bear = CardDef {
            oracle_id: "k_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    swamp,
                    mountain,
                    gy_fiend,
                    double_x_discard,
                    x_targets_spell,
                    counter_remover,
                    bear,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(3), CardDefId(4), CardDefId(5), CardDefId(0)],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 8],
            sideboard: vec![],
        };

        // 1. Synth Graveyard X Fiend: 5 Swamps + 1 card in graveyard -> max X = 1
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                101,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            for _ in 0..5 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            let fiend_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            let gy_card = {
                let ts = state.allocate_timestamp();
                let oid = state.insert_object(
                    crate::state::GameObject::new(
                        CardDefId(0),
                        PlayerId(0),
                        Zone::Graveyard,
                        ts,
                        ObjectFlags::empty(),
                    ),
                    None,
                );
                state.player_mut(PlayerId(0)).unwrap().graveyard.push(oid);
                oid
            };
            state.mark_dirty(DirtyMask::TOPOLOGY);

            begin_activate_ability(&mut state, PlayerId(0), fiend_bf, 0).unwrap();
            let (pid, min, max) = match state.decision() {
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseNumber {
                            prompt_id,
                            min,
                            max,
                        },
                } => (*prompt_id, *min, *max),
                other => panic!("expected ChooseNumber prompt, got {other:?}"),
            };
            assert_eq!((min, max), (0, 1));
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickNumber { value: 0 },
            }));
            assert!(legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickNumber { value: 1 },
            }));
            assert_eq!(legal.len(), 3); // 0, 1, Concede

            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::PickNumber { value: 5 },
                )
                .is_err()
            );
            assert!(state.pending_prompt().is_some());

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 1 },
            )
            .unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            let (sel_pid, sel_min, sel_max) = match state.decision() {
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            min,
                            max,
                            ..
                        },
                } => (*prompt_id, *min, *max),
                other => panic!("expected CostObjectSelection prompt, got {other:?}"),
            };
            assert_eq!((sel_min, sel_max), (1, 1));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                sel_pid,
                ChoiceAnswer::PickObject { id: gy_card },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.stack_entries()[0].x_value, 1);
        }

        // 2. Synth DoubleX Discard: 5 mana + 3 remaining cards in hand -> max X = 1
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                102,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);
            for _ in 0..5 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            let dbl_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            assert_eq!(state.players()[0].hand.len(), 4);
            begin_cast_spell(&mut state, PlayerId(0), dbl_id, 0, Zone::Hand).unwrap();
            match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { min, max, .. },
                } => assert_eq!((*min, *max), (0, 1)),
                other => panic!("expected ChooseNumber prompt, got {other:?}"),
            }
        }

        // 3. Synth X Targets: 5 mana + 2 creatures -> max X = 2
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                103,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);
            for _ in 0..5 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            }
            add_synth_bf(&mut state, PlayerId(1), CardDefId(6));
            add_synth_bf(&mut state, PlayerId(1), CardDefId(6));
            let xtgt_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(4))
                .unwrap();
            begin_cast_spell(&mut state, PlayerId(0), xtgt_id, 0, Zone::Hand).unwrap();
            match state.decision() {
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseNumber { min, max, .. },
                } => assert_eq!((*min, *max), (0, 2)),
                other => panic!("expected ChooseNumber prompt, got {other:?}"),
            }
        }

        // 4. Synth Counter Remover: 1 creature with 5 counters -> min: 1, max: 1 and succeeds
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck, p1_deck],
                104,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);
            let bear_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(6));
            state.add_counter(bear_bf, cid_p1, 5);
            state.mark_dirty(DirtyMask::COUNTERS);

            let cnt_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(5))
                .unwrap();
            begin_cast_spell(&mut state, PlayerId(0), cnt_id, 0, Zone::Hand).unwrap();
            let (sel_pid, sel_min, sel_max) = match state.decision() {
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            min,
                            max,
                            ..
                        },
                } => (*prompt_id, *min, *max),
                other => panic!("expected CostObjectSelection prompt, got {other:?}"),
            };
            assert_eq!((sel_min, sel_max), (1, 1));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                sel_pid,
                ChoiceAnswer::PickObject { id: bear_bf },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.counter_count(bear_bf, cid_p1), 2);
        }
    }

    #[test]
    fn synth_bug_n_convoke_colossus_and_delve_parity() {
        use mtg_ir::{Amount, KeywordAbility};

        let mut tables = InternTables::default();
        tables.keywords.push("convoke".into());
        let kw_convoke = mtg_ir::KeywordId(0);
        tables.keywords.push("delve".into());
        let kw_delve = mtg_ir::KeywordId(1);

        let red_cr = CardDef {
            oracle_id: "n_red".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Red Creature".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Red Creature",
                ManaCost::parse("{R}"),
                CardTypeMask::CREATURE,
                ColorMask::R,
                vec![],
            )],
            fully_supported: true,
        };
        let green_cr = CardDef {
            oracle_id: "n_green".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Green Creature".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Green Creature",
                ManaCost::parse("{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        let white_cr = CardDef {
            oracle_id: "n_white".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth White Creature".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth White Creature",
                ManaCost::parse("{W}"),
                CardTypeMask::CREATURE,
                ColorMask::W,
                vec![],
            )],
            fully_supported: true,
        };
        let convoke_colossus = CardDef {
            oracle_id: "n_colossus".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Convoke Colossus".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Convoke Colossus",
                ManaCost::parse("{X}{G}{W}"),
                CardTypeMask::CREATURE,
                ColorMask::G | ColorMask::W,
                vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_keyword_def(kw_convoke, None, vec![]),
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![],
                        effect: vec![],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Default::default(),
                    }),
                ],
            )],
            fully_supported: true,
        };
        let delve_exile_spell = CardDef {
            oracle_id: "n_delve_ex".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Delve Exile".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Delve Exile",
                ManaCost::parse("{2}{B}"),
                CardTypeMask::SORCERY,
                ColorMask::B,
                vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: make_keyword_def(kw_delve, None, vec![]),
                        id: None,
                        span: Default::default(),
                    }),
                    AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![CostDef::ExileFrom {
                            zone: Zone::Graveyard,
                            count: Box::new(Amount::Const { n: 1 }),
                            target: None,
                            from_top: false,
                            any_graveyard: false,
                            single_graveyard: false,
                            min_total_mana_value: None,
                            min_total_mana_value_amount: None,
                            min_colored_mana_symbols: None,
                            collect_evidence: false,
                            one_or_more: false,
                            total_mana_value_equals_x: false,
                            and_other_same_name_count: None,
                        }],
                        targets: vec![],
                        effect: vec![],
                        modal: None,
                        cast_restrictions: vec![],
                        cast_condition: None,
                        x_cant_be_zero: false,
                        span: Default::default(),
                    }),
                ],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    red_cr,
                    green_cr,
                    white_cr,
                    convoke_colossus,
                    delve_exile_spell,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(3), CardDefId(4), CardDefId(0), CardDefId(0)],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 8],
            sideboard: vec![],
        };

        // 1. Convoke emission parity on X=0 ({G}{W} remaining) and X=1 ({1}{G}{W} remaining)
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                201,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let col_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();

            // 0 lands + 4 Red creatures only -> uncastable
            let mut reds = Vec::new();
            for _ in 0..4 {
                reds.push(add_synth_bf(&mut state, PlayerId(0), CardDefId(0)));
            }
            assert!(can_cast_spell_option(&state, PlayerId(0), col_id, 0).is_none());

            // Add 1 Green creature + {W} in pool
            let green_id = add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            state.player_mut(PlayerId(0)).unwrap().mana_pool
                [crate::mana::color_to_index(ManaColor::W)] = 1;
            assert!(can_cast_spell_option(&state, PlayerId(0), col_id, 0).is_some());

            // X = 0 -> {G}{W} remaining -> Green Convoke emitted, Red Convoke NOT emitted
            begin_cast_spell(&mut state, PlayerId(0), col_id, 0, Zone::Hand).unwrap();
            let pid = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickNumber { value: 0 },
            )
            .unwrap();
            let legal = legal_casting_actions(&state, PlayerId(0));
            assert!(legal.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Convoke,
                object: green_id,
            }));
            for r in &reds {
                assert!(!legal.contains(&Action::SelectCostAid {
                    aid_kind: CostAidKind::Convoke,
                    object: *r,
                }));
            }
            cancel_active_cast(&mut state, PlayerId(0)).unwrap();

            // X = 1 -> {1}{G}{W} remaining -> Red Convoke IS emitted, and after tapping 1 Red (generic->0),
            // remaining Red creatures are NO LONGER emitted.
            let col_id2 = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            begin_cast_spell(&mut state, PlayerId(0), col_id2, 0, Zone::Hand).unwrap();
            let pid2 = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid2,
                ChoiceAnswer::PickNumber { value: 1 },
            )
            .unwrap();
            let legal_x1 = legal_casting_actions(&state, PlayerId(0));
            assert!(legal_x1.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Convoke,
                object: reds[0],
            }));
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::SelectCostAid {
                    aid_kind: CostAidKind::Convoke,
                    object: reds[0],
                },
            )
            .unwrap();
            let legal_after_red = legal_casting_actions(&state, PlayerId(0));
            assert!(legal_after_red.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Convoke,
                object: green_id,
            }));
            for r in &reds[1..] {
                assert!(!legal_after_red.contains(&Action::SelectCostAid {
                    aid_kind: CostAidKind::Convoke,
                    object: *r,
                }));
            }
        }

        // 2. Delve graveyard reservation parity against pending ExileFrom(Graveyard, 1)
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck, p1_deck],
                202,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            state.player_mut(PlayerId(0)).unwrap().mana_pool
                [crate::mana::color_to_index(ManaColor::B)] = 2;
            let mut gy_ids = Vec::new();
            for _ in 0..2 {
                let ts = state.allocate_timestamp();
                let oid = state.insert_object(
                    crate::state::GameObject::new(
                        CardDefId(0),
                        PlayerId(0),
                        Zone::Graveyard,
                        ts,
                        ObjectFlags::empty(),
                    ),
                    None,
                );
                state.player_mut(PlayerId(0)).unwrap().graveyard.push(oid);
                gy_ids.push(oid);
            }
            state.mark_dirty(DirtyMask::TOPOLOGY);

            let delve_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(4))
                .unwrap();
            begin_cast_spell(&mut state, PlayerId(0), delve_id, 0, Zone::Hand).unwrap();
            let legal1 = legal_casting_actions(&state, PlayerId(0));
            assert!(legal1.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Delve,
                object: gy_ids[0],
            }));
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::SelectCostAid {
                    aid_kind: CostAidKind::Delve,
                    object: gy_ids[0],
                },
            )
            .unwrap();
            // Only 1 card left in GY, reserved for ExileFrom(Graveyard, 1) -> Delve must NOT emit
            let legal2 = legal_casting_actions(&state, PlayerId(0));
            assert!(!legal2.contains(&Action::SelectCostAid {
                aid_kind: CostAidKind::Delve,
                object: gy_ids[1],
            }));
            assert!(
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::SelectCostAid {
                        aid_kind: CostAidKind::Delve,
                        object: gy_ids[1],
                    },
                )
                .is_err()
            );
        }
    }

    #[test]
    fn synth_bug_o_choice_branch_mana_and_optional() {
        use mtg_ir::{Amount, CostObjectTarget};

        let mut tables = InternTables::default();
        tables.subtypes.push("Plains".into());
        let sub_plains = SubtypeId(0);

        let plains = CardDef {
            oracle_id: "o_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Plains".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::from_slice(&[sub_plains]),
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
            oracle_id: "o_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };
        // Card 2: Synth Choice Aspirant ({W}, Choice([[Behold(Creature)], [Mana({2})]]))
        let choice_aspirant = CardDef {
            oracle_id: "o_asp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Choice Aspirant".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Choice Aspirant",
                ManaCost::parse("{W}"),
                CardTypeMask::CREATURE,
                ColorMask::W,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Choice {
                        options: vec![
                            vec![CostDef::Behold {
                                count: Box::new(Amount::Const { n: 1 }),
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                                exile: false,
                            }],
                            vec![CostDef::Mana {
                                cost: ManaCost::parse("{2}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        ],
                        forage: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Card 3: Synth Eaten Choice ({W}, Choice([[Sacrifice(creature)], [Mana({2})]]))
        let eaten_choice = CardDef {
            oracle_id: "o_eaten".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Eaten Choice".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Eaten Choice",
                ManaCost::parse("{W}"),
                CardTypeMask::SORCERY,
                ColorMask::W,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Choice {
                        options: vec![
                            vec![CostDef::Sacrifice {
                                count: Box::new(Amount::Const { n: 1 }),
                                target: CostObjectTarget::Filter {
                                    filter: Box::new(ObjectFilter {
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                },
                                sacrifice_all: false,
                                half_rounded_up: false,
                                attached_creature: false,
                                one_or_more: false,
                                up_to: None,
                                different_names: false,
                            }],
                            vec![CostDef::Mana {
                                cost: ManaCost::parse("{2}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        ],
                        forage: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };
        // Card 4: Synth Optional Choice ({W}, Optional([Choice([[Sacrifice(creature)], [Mana({2})]])]))
        let optional_choice = CardDef {
            oracle_id: "o_opt_ch".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Optional Choice".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Optional Choice",
                ManaCost::parse("{W}"),
                CardTypeMask::SORCERY,
                ColorMask::W,
                vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Optional {
                        cost: vec![CostDef::Choice {
                            options: vec![
                                vec![CostDef::Sacrifice {
                                    count: Box::new(Amount::Const { n: 1 }),
                                    target: CostObjectTarget::Filter {
                                        filter: Box::new(ObjectFilter {
                                            types_any: CardTypeMask::CREATURE,
                                            ..Default::default()
                                        }),
                                    },
                                    sacrifice_all: false,
                                    half_rounded_up: false,
                                    attached_creature: false,
                                    one_or_more: false,
                                    up_to: None,
                                    different_names: false,
                                }],
                                vec![CostDef::Mana {
                                    cost: ManaCost::parse("{2}"),
                                    x_spend_colors: ColorMask::empty(),
                                    x_colored_only: false,
                                    x_max_one_per_color: false,
                                }],
                            ],
                            forage: false,
                        }],
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains, bear, choice_aspirant, eaten_choice, optional_choice],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(2), CardDefId(3), CardDefId(4), CardDefId(0)],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 8],
            sideboard: vec![],
        };

        // 1. Synth Choice Aspirant: 1 Plains -> None; 3 Plains -> CostBranchChoice emits ONLY 1, rejects 0 cleanly, completes {2} via AutoPayMana!
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                301,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let asp_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
                .unwrap();
            assert!(can_cast_spell_option(&state, PlayerId(0), asp_id, 0).is_none());

            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            assert!(can_cast_spell_option(&state, PlayerId(0), asp_id, 0).is_some());

            // Also verify MacroAction::CastSpell pauses on CostBranchChoice and rejects extraneous modes/targets with atomic rollback.
            let pre_macro = state.clone();
            assert!(
                apply_macro_cast_spell(
                    &mut state,
                    PlayerId(0),
                    &MacroAction::CastSpell {
                        object: asp_id,
                        option_id: 0,
                        from_zone: Zone::Hand,
                        modes: vec![0],
                        x_value: None,
                        targets: vec![],
                    },
                )
                .is_err()
            );
            assert_eq!(state.players()[0].hand, pre_macro.players()[0].hand);
            assert!(state.casting().is_none());

            begin_cast_spell(&mut state, PlayerId(0), asp_id, 0, Zone::Hand).unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            let pid = state.pending_prompt().unwrap().prompt_id;
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(!legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 0 },
            }));
            assert!(legal.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 1 },
            }));

            // Reject PickMode 0 without mutating prompt or payment plan
            assert!(
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::PickMode { index: 0 },
                )
                .is_err()
            );
            assert_eq!(state.pending_prompt().unwrap().prompt_id, pid);

            // Accept PickMode 1 -> folds {2} into Step 1 -> AutoPayMana taps remaining 2 Plains and commits!
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(state.decision(), &DecisionState::CastingSequence);
            assert_eq!(
                state
                    .casting()
                    .unwrap()
                    .payment_plan
                    .as_ref()
                    .unwrap()
                    .remaining_mana,
                ManaCost {
                    generic: 2,
                    ..Default::default()
                }
            );
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack_entries().len(), 1);
        }

        // 2. Synth Eaten Choice: (a) 1 Plains + 1 Creature -> [0], (b) 3 Plains + 0 Creatures -> [1], (c) 3 Plains + 1 Creature -> [0, 1]
        for (num_plains, has_creature, expected) in [
            (1usize, true, vec![0u16]),
            (3, false, vec![1]),
            (3, true, vec![0, 1]),
        ] {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck.clone(), p1_deck.clone()],
                302,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            for _ in 0..num_plains {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            if has_creature {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            }
            let eaten_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            begin_cast_spell(&mut state, PlayerId(0), eaten_id, 0, Zone::Hand).unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            let pid = state.pending_prompt().unwrap().prompt_id;
            let legal = crate::choices::legal_choice_actions(&state, PlayerId(0));
            for idx in 0u16..2 {
                let act = Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickMode { index: idx },
                };
                assert_eq!(legal.contains(&act), expected.contains(&idx));
            }
            if !has_creature {
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    pid,
                    ChoiceAnswer::PickMode { index: 1 },
                )
                .unwrap();
                apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
                assert_eq!(state.decision(), &DecisionState::Priority);
                assert_eq!(state.stack_entries().len(), 1);
            }
        }

        // 3. Synth Optional Choice: 1 Plains + 0 Creatures -> castable, skips YesNo(true) optional prompt
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[p0_deck, p1_deck],
                303,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let opt_id = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(4))
                .unwrap();
            assert!(can_cast_spell_option(&state, PlayerId(0), opt_id, 0).is_some());
            begin_cast_spell(&mut state, PlayerId(0), opt_id, 0, Zone::Hand).unwrap();
            assert!(state.pending_prompt().is_none());
            assert_eq!(state.casting().unwrap().step, CastStep::PayCosts);
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack_entries().len(), 1);
        }
    }

    #[test]
    fn synth_divide_total_triggers_activated_and_divisions_no_softlock() {
        use mtg_ir::{
            ActivatedAbility, Amount, CounterId, CounterTarget, EffectOp, EventDetail, EventFilter,
            EventKind, Ref, Selector, Timing, TriggerDef, TriggeredAbility,
        };

        let mut tables = InternTables::default();
        tables.counters.push("charge".into());
        let cid_charge = CounterId(0);

        let bear = CardDef {
            oracle_id: "l_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Bear",
                ManaCost::parse("{1}{G}"),
                CardTypeMask::CREATURE,
                ColorMask::G,
                vec![],
            )],
            fully_supported: true,
        };

        let div_dmg_step = make_synth_step(
            Selector::Target { index: 0 },
            EffectOp::Damage {
                source: Some(Box::new(Selector::SelfObj)),
                also_players: None,
                divided: true,
                excess_to_controller: false,
                excess_to_controller_if_trample: false,
                or_planeswalker_controlled_by: None,
                damage_attacked_target_by_self: false,
                soulfire_exile_damage_per_target: false,
                each_creature_deals: None,
            },
        );
        let etb_filter = EventFilter {
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
        };

        // Card 1: Fury-like ETB trigger (min: 0, max: 99, divide_total: Const(4))
        let etb_div4 = CardDef {
            oracle_id: "l_fury".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Fury ETB".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Fury ETB",
                ManaCost::default(),
                CardTypeMask::CREATURE,
                ColorMask::R,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::ZoneChange,
                        filter: etb_filter.clone(),
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 0,
                        max: 99,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::Const { n: 4 })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![div_dmg_step.clone()],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 2: Activated ability (min: 0, max: 99, divide_total: Const(3))
        let act_div3 = CardDef {
            oracle_id: "l_act".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Divided Blaster".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Divided Blaster",
                ManaCost::default(),
                CardTypeMask::ARTIFACT,
                ColorMask::empty(),
                vec![AbilityDef::Activated(ActivatedAbility {
                    id: "act_div3".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 0,
                        max: 99,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::Const { n: 3 })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![div_dmg_step.clone()],
                    modal: None,
                    timing: Timing::Instant,
                    is_mana: false,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: false,
                    text: None,
                    grant_condition: None,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        // Card 3: Triggered ability (min: 1, max: 3, divide_total: CountersOn(SelfObj))
        let etb_counters_div = CardDef {
            oracle_id: "l_cnt_trig".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Counter Trigger".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Counter Trigger",
                ManaCost::default(),
                CardTypeMask::CREATURE,
                ColorMask::R,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::ZoneChange,
                        filter: etb_filter,
                        scope: None,
                        from_zones: None,
                        only_once_per_turn: false,
                        do_this_once_per_turn: false,
                        is_state_trigger: false,
                        or: vec![],
                        chapter_numbers: SmallVec::new(),
                        is_chapter_ability: false,
                        spell_delayed: None,
                    },
                    condition: None,
                    grant_condition: None,
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 1,
                        max: 3,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::CountersOn {
                            counter: CounterTarget::Specific(cid_charge),
                            of: Ref::SelfObj,
                        })),
                        with_single_target: false,
                        dynamic_count: None,
                        dynamic_up_to: false,
                        max_total_mana_value: None,
                        also_spells: false,
                        per_opponent: false,
                        different_controllers: false,
                        chosen_by_event_player: false,
                        shares_card_type_with_target: None,
                        zones: ZoneMask::BATTLEFIELD,
                    }],
                    effect: vec![div_dmg_step],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bear, etb_div4, act_div3, etb_counters_div],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1), CardDefId(3), CardDefId(0), CardDefId(0)],
            sideboard: vec![],
        };

        // Case 1 & 2: Triggered ETB (4 targets auto-assign [1,1,1,1], 5th rejected; 3 targets -> ChooseDivisions works)
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                401,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let mut bears = Vec::new();
            for _ in 0..6 {
                bears.push(add_synth_bf(&mut state, PlayerId(1), CardDefId(0)));
            }
            let fury_hand = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
                .unwrap();
            move_object(
                &mut state,
                fury_hand,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(PlayerId(0)),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            prepare_next_untargeted_stack_trigger(&mut state).unwrap();

            for b in &bears[..4] {
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::SelectTarget {
                        target: TargetRef::Object { id: *b },
                    },
                )
                .unwrap();
            }
            // 4th target hits max_req=4 and auto-assigns [1, 1, 1, 1] -> commits cleanly!
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .divisions
                    .as_slice(),
                &[
                    (TargetRef::Object { id: bears[0] }, 1),
                    (TargetRef::Object { id: bears[1] }, 1),
                    (TargetRef::Object { id: bears[2] }, 1),
                    (TargetRef::Object { id: bears[3] }, 1),
                ]
            );

            // Re-test with 3 targets pausing in ChooseDivisions
            state.stack_mut().clear();
            state.stack_entries_mut().clear();
            let fury_bf = state.players()[0].battlefield[0];
            let sid = state.allocate_stack_id();
            let meta_targets = vec![TargetSpec {
                kind: TargetKind::Object,
                filter: ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                },
                player: None,
                min: 0,
                max: 99,
                same_owner: false,
                condition: None,
                other_target: false,
                divide_total: Some(Box::new(Amount::Const { n: 4 })),
                with_single_target: false,
                dynamic_count: None,
                dynamic_up_to: false,
                max_total_mana_value: None,
                also_spells: false,
                per_opponent: false,
                different_controllers: false,
                chosen_by_event_player: false,
                shares_card_type_with_target: None,
                zones: ZoneMask::BATTLEFIELD,
            }];
            let lki = build_source_lki_snapshot(&state, fury_bf);
            state.stack_mut().push(fury_bf);
            state.stack_entries_mut().push(StackEntry {
                id: sid,
                kind: StackEntryKind::Triggered {
                    source: fury_bf,
                    ability_idx: 0,
                    lki,
                },
                controller: PlayerId(0),
                choices: crate::state::StackChoices::default(),
                targets: vec![],
                x_value: 0,
                paid_mana_colors: ColorMask::empty(),
            });
            state.push_stack_trigger_meta(
                sid,
                crate::triggers::StackTriggerMeta {
                    source: fury_bf,
                    face_index: 0,
                    ability_idx: 0,
                    kind: crate::triggers::TriggerSourceKind::Innate {
                        component_index: 0,
                        face_index: 0,
                        ability_idx: 0,
                    },
                    condition: None,
                    targets: meta_targets,
                    effect: vec![],
                    modal: None,
                    event_object: None,
                    event_lki: None,
                    event_player: None,
                    event_amount: None,
                    defending_player: None,
                    last_tapped_mana_colors: SmallVec::new(),
                    is_look_back: false,
                    x_value: 0,
                    bindings: Vec::new(),
                },
            );
            prepare_next_untargeted_stack_trigger(&mut state).unwrap();
            for b in &bears[..3] {
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::SelectTarget {
                        target: TargetRef::Object { id: *b },
                    },
                )
                .unwrap();
            }
            // Reject 4th target? Here let's finish at 3 targets -> ChooseDivisions!
            apply_casting_action(&mut state, PlayerId(0), &Action::DoneTargets).unwrap();
            assert_eq!(state.casting().unwrap().step, CastStep::ChooseDivisions);
            let div_legal = legal_casting_actions(&state, PlayerId(0));
            assert!(div_legal.contains(&Action::AssignDivision {
                target: TargetRef::Object { id: bears[0] },
                amount: 2,
            }));
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::AssignDivision {
                    target: TargetRef::Object { id: bears[0] },
                    amount: 2,
                },
            )
            .unwrap();
            // Remaining 2 across 2 targets auto-assigns 1 each and commits!
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(
                state
                    .stack_entries()
                    .last()
                    .unwrap()
                    .choices
                    .divisions
                    .as_slice(),
                &[
                    (TargetRef::Object { id: bears[0] }, 2),
                    (TargetRef::Object { id: bears[1] }, 1),
                    (TargetRef::Object { id: bears[2] }, 1),
                ]
            );
        }

        // Case 3: Activated ability divide_total: 3 caps at 3 targets across 5 creatures
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                402,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let mut bears = Vec::new();
            for _ in 0..5 {
                bears.push(add_synth_bf(&mut state, PlayerId(1), CardDefId(0)));
            }
            let blaster_bf = add_synth_bf(&mut state, PlayerId(0), CardDefId(2));
            begin_activate_ability(&mut state, PlayerId(0), blaster_bf, 0).unwrap();
            for b in &bears[..3] {
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::SelectTarget {
                        target: TargetRef::Object { id: *b },
                    },
                )
                .unwrap();
            }
            assert_eq!(state.decision(), &DecisionState::Priority);
            assert_eq!(state.stack_entries().last().unwrap().targets.len(), 3);
        }

        // Case 4: Triggered ability (min: 1, max: 3, divide_total: CountersOn(SelfObj))
        // (a) 0 counters on flush -> removed per CR 603.3d
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                403,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);
            add_synth_bf(&mut state, PlayerId(1), CardDefId(0));

            let trig_hand = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            move_object(
                &mut state,
                trig_hand,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(PlayerId(0)),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert!(state.stack().is_empty());
        }

        // (b) Counters removed after stacking, before prepare_next_untargeted_stack_trigger -> removed in ChooseTargets per CR 603.3d
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck],
                404,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);
            add_synth_bf(&mut state, PlayerId(1), CardDefId(0));

            let trig_hand = state.players()[0]
                .hand
                .iter()
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(3))
                .unwrap();
            let trig_bf = match move_object(
                &mut state,
                trig_hand,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(PlayerId(0)),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            {
                MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => unreachable!(),
            };
            state.add_counter(trig_bf, cid_charge, 2);
            state.mark_dirty(DirtyMask::COUNTERS);
            flush_triggers(&mut state);
            assert_eq!(state.stack().len(), 1);

            if let Some(cs) = state.counters_mut(trig_bf) {
                cs.clear();
            }
            state.mark_dirty(DirtyMask::COUNTERS);
            let _ = prepare_next_untargeted_stack_trigger(&mut state).unwrap();
            assert!(state.stack().is_empty());
            assert!(state.casting().is_none());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }
    }

    fn bug_t_make_kw(
        kw: mtg_ir::KeywordId,
        cost: Option<ManaCost>,
        cost_defs: Vec<CostDef>,
    ) -> mtg_ir::KeywordDef {
        mtg_ir::KeywordDef {
            keyword: kw,
            cost,
            param: None,
            n: None,
            cost_defs,
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

    fn bug_t_make_land(name: &str, color: ManaColor) -> CardDef {
        let mana_step = make_synth_step(
            mtg_ir::Selector::SelfObj,
            mtg_ir::EffectOp::AddMana {
                production: mtg_ir::ManaProduction::Fixed {
                    symbols: smallvec::smallvec![color],
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
        );
        CardDef {
            oracle_id: format!("o_bt_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                name,
                ManaCost::default(),
                CardTypeMask::LAND,
                ColorMask::empty(),
                vec![make_synth_activated_ability(
                    "mana",
                    vec![CostDef::TapSelf],
                    vec![mana_step],
                    mtg_ir::Timing::Mana,
                    true,
                )],
            )],
            fully_supported: true,
        }
    }

    fn bug_t_clear_hand_to_library(state: &mut GameState, player: PlayerId) {
        let hand_ids: Vec<ObjectId> = state.player(player).unwrap().hand.clone();
        for id in hand_ids {
            let _ = move_object(state, id, Zone::Library, MoveRawOptions::default(), None).unwrap();
        }
    }

    fn bug_t_spawn_in_zone(
        state: &mut GameState,
        player: PlayerId,
        def: CardDefId,
        zone: Zone,
    ) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            crate::state::GameObject::new(def, player, zone, ts, ObjectFlags::empty()),
            None,
        );
        let p = state.player_mut(player).unwrap();
        match zone {
            Zone::Hand => p.hand.push(oid),
            Zone::Graveyard => p.graveyard.push(oid),
            Zone::Battlefield => p.battlefield.push(oid),
            Zone::Library => p.library.push(oid),
            Zone::Exile => p.exile.push(oid),
            Zone::Command => p.command.push(oid),
            Zone::Stack => {}
        }
        state.mark_dirty(DirtyMask::TOPOLOGY);
        oid
    }

    fn bug_t_boot_main1(db: Arc<CardDatabase>, filler_def: CardDefId, seed: u64) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![filler_def; 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], seed).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(DecisionState::Priority);
        bug_t_clear_hand_to_library(&mut state, PlayerId(0));
        state
    }

    fn bug_t_finish_non_choice_steps(state: &mut GameState, player: PlayerId) {
        for _ in 0..32 {
            match state.decision().clone() {
                DecisionState::Priority => return,
                DecisionState::CastingSequence => {
                    apply_casting_action(state, player, &Action::AutoPayMana).unwrap();
                }
                DecisionState::Prompt {
                    request:
                        ChoiceRequest::ChooseObjects {
                            prompt_id,
                            candidates,
                            min,
                            ..
                        },
                } => {
                    let answer = if min > 0 {
                        assert!(
                            !candidates.is_empty(),
                            "not enough candidates left: min={min}, cands={candidates:?}"
                        );
                        ChoiceAnswer::PickObject { id: candidates[0] }
                    } else {
                        ChoiceAnswer::DonePicking
                    };
                    crate::choices::apply_choice_answer(state, player, prompt_id, answer).unwrap();
                }
                other => panic!("unexpected decision while finishing casting: {other:?}"),
            }
        }
        panic!("did not return to Priority: {:?}", state.decision());
    }

    #[test]
    fn synth_bug_t_flamekin_reveal_from_hand_choice_cr601_2a() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Elemental".into());
        let sub_elemental = SubtypeId(0);
        tables.subtypes.push("Warrior".into());
        let sub_warrior = SubtypeId(1);

        let mountain = bug_t_make_land("Synth Mountain", ManaColor::R);
        let bladewhirl = CardDef {
            oracle_id: "o_bt_blade".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bladewhirl".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bladewhirl".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: smallvec::smallvec![sub_elemental, sub_warrior],
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 1,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Choice {
                        options: vec![
                            vec![CostDef::RevealFromHand {
                                count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                target: Some(mtg_ir::CostObjectTarget::Filter {
                                    filter: Box::new(ObjectFilter {
                                        subtypes_any: smallvec::smallvec![sub_elemental],
                                        ..Default::default()
                                    }),
                                }),
                                share_color: false,
                            }],
                            vec![CostDef::Mana {
                                cost: ManaCost::parse("{3}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                        ],
                        forage: false,
                    }],
                    targets: vec![],
                    effect: vec![],
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
        let other_elem = CardDef {
            oracle_id: "o_bt_elem".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Spark Elemental".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Spark Elemental".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: smallvec::smallvec![sub_elemental],
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
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
                cards: vec![mountain, bladewhirl, other_elem],
                decks: vec![],
            })
            .unwrap(),
        );

        // (a) 1 Mountain + 0 other Elementals in hand -> cannot reveal self; illegal everywhere.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 901);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let blade_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);

            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), blade_id, 0).is_none()
            );
            assert!(
                !crate::legality::collect_priority_actions(&state, PlayerId(0))
                    .iter()
                    .any(|a| matches!(a, Action::BeginCast { object, .. } if *object == blade_id))
            );
            assert!(
                !crate::auto_tap::preview_auto_tap_for_request(
                    &state,
                    PlayerId(0),
                    &crate::action::CastRequest {
                        object: blade_id,
                        option_id: 0,
                        x_value: None,
                    },
                )
                .can_pay
            );
            assert!(begin_cast_spell(&mut state, PlayerId(0), blade_id, 0, Zone::Hand).is_err());
        }

        // (b) 1 Mountain + 1 other Elemental in hand -> branch 0 legal, resolves onto Battlefield.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 902);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let blade_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);
            let spark_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);

            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), blade_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), blade_id, 0, Zone::Hand).unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();

            let pid = state.pending_prompt().unwrap().prompt_id;
            let legal_choices = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(legal_choices.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 0 },
            }));
            assert!(!legal_choices.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 1 },
            }));

            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickMode { index: 0 },
            )
            .unwrap();
            if let DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseObjects {
                        prompt_id,
                        candidates,
                        ..
                    },
            } = state.decision().clone()
            {
                assert_eq!(candidates, vec![spark_id]);
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    prompt_id,
                    ChoiceAnswer::PickObject { id: spark_id },
                )
                .unwrap();
            }
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            crate::stack::resolve_top_of_stack(&mut state).unwrap();
            assert!(
                state.players()[0]
                    .battlefield
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            );
        }

        // (c) 4 Mountains + 0 other Elementals in hand -> branch 1 ({3}) legal, resolves onto Battlefield.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 903);
            for _ in 0..4 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            let blade_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);

            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), blade_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), blade_id, 0, Zone::Hand).unwrap();
            apply_casting_action(&mut state, PlayerId(0), &Action::AutoPayMana).unwrap();

            let pid = state.pending_prompt().unwrap().prompt_id;
            let legal_choices = crate::choices::legal_choice_actions(&state, PlayerId(0));
            assert!(!legal_choices.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 0 },
            }));
            assert!(legal_choices.contains(&Action::Answer {
                prompt_id: pid,
                answer: ChoiceAnswer::PickMode { index: 1 },
            }));
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            crate::stack::resolve_top_of_stack(&mut state).unwrap();
            assert!(
                state.players()[0]
                    .battlefield
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            );
        }
    }

    #[test]
    fn synth_bug_t_discard_and_x_discard_spell_cr601_2a() {
        let mountain = bug_t_make_land("Synth Mountain", ManaColor::R);
        let fixed_discard_spell = CardDef {
            oracle_id: "o_bt_disc1".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Discard Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Discard Bolt".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{R}"),
                    types: CardTypeMask::SORCERY,
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
                    additional_costs: vec![CostDef::Discard {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: None,
                        random: false,
                        same_name_pair: false,
                        same_name_as_controlled: None,
                        different_names: false,
                    }],
                    targets: vec![],
                    effect: vec![],
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
        let x_discard_spell = CardDef {
            oracle_id: "o_bt_discx".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth X Discard".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth X Discard".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{X}"),
                    types: CardTypeMask::SORCERY,
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
                    additional_costs: vec![CostDef::Discard {
                        count: Box::new(mtg_ir::Amount::X),
                        target: None,
                        random: false,
                        same_name_pair: false,
                        same_name_as_controlled: None,
                        different_names: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: true,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let spare = CardDef {
            oracle_id: "o_bt_spare".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Spare".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Spare",
                ManaCost::parse("{R}"),
                CardTypeMask::SORCERY,
                ColorMask::R,
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![mountain, fixed_discard_spell, x_discard_spell, spare],
                decks: vec![],
            })
            .unwrap(),
        );

        // Solo in hand: neither spell can discard itself
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 904);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let bolt_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), bolt_id, 0).is_none()
            );

            let x_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);
            // Make sure x_id is tested solo by moving bolt_id out first
            let _ = move_object(
                &mut state,
                bolt_id,
                Zone::Library,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            assert!(crate::legality::can_cast_spell_option(&state, PlayerId(0), x_id, 0).is_none());
            let x_cost = [CostDef::Discard {
                count: Box::new(mtg_ir::Amount::X),
                target: None,
                random: false,
                same_name_pair: false,
                same_name_as_controlled: None,
                different_names: false,
            }];
            assert_eq!(
                crate::costs::max_x_for_cost_list_with_ctx_cached(
                    &state,
                    PlayerId(0),
                    x_id,
                    &x_cost,
                    crate::costs::CostListSelfContext::for_spell_cast(Zone::Hand),
                    state.evaluate_board(),
                ),
                0
            );
        }

        // With 1 other card in hand: fixed discard casts cleanly, X caps at 1 and discards spare
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 905);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let bolt_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);
            let _spare_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);

            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), bolt_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), bolt_id, 0, Zone::Hand).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert!(
                state.players()[0]
                    .graveyard
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(3))
            );
        }
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 906);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let x_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);
            let _spare_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);

            let x_cost = [CostDef::Discard {
                count: Box::new(mtg_ir::Amount::X),
                target: None,
                random: false,
                same_name_pair: false,
                same_name_as_controlled: None,
                different_names: false,
            }];
            assert_eq!(
                crate::costs::max_x_for_cost_list_with_ctx_cached(
                    &state,
                    PlayerId(0),
                    x_id,
                    &x_cost,
                    crate::costs::CostListSelfContext::for_spell_cast(Zone::Hand),
                    state.evaluate_board(),
                ),
                1
            );
            assert!(crate::legality::can_cast_spell_option(&state, PlayerId(0), x_id, 0).is_some());
            begin_cast_spell(&mut state, PlayerId(0), x_id, 0, Zone::Hand).unwrap();
            let DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseNumber {
                        prompt_id,
                        min: 1,
                        max: 1,
                    },
            } = state.decision().clone()
            else {
                panic!(
                    "expected ChooseNumber min=1 max=1, got {:?}",
                    state.decision()
                );
            };
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickNumber { value: 1 },
            )
            .unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.stack_entries()[0].x_value, 1);
            assert!(
                state.players()[0]
                    .graveyard
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(3))
            );
        }
    }

    #[test]
    fn synth_bug_t_exile_from_hand_and_put_on_top_cr601_2a() {
        let island = bug_t_make_land("Synth Island", ManaColor::U);
        let pitch_spell = CardDef {
            oracle_id: "o_bt_pitch".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Pitch".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Pitch".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{U}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::ExileFrom {
                        zone: Zone::Hand,
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: Some(mtg_ir::CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                colors_any: ColorMask::U,
                                ..Default::default()
                            }),
                        }),
                        from_top: false,
                        any_graveyard: false,
                        single_graveyard: false,
                        min_total_mana_value: None,
                        min_total_mana_value_amount: None,
                        min_colored_mana_symbols: None,
                        collect_evidence: false,
                        one_or_more: false,
                        total_mana_value_equals_x: false,
                        and_other_same_name_count: None,
                    }],
                    targets: vec![],
                    effect: vec![],
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
        let top_spell = CardDef {
            oracle_id: "o_bt_top".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Top".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Top".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{U}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::PutFromHandOnTopOfLibrary {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        filter: None,
                    }],
                    targets: vec![],
                    effect: vec![],
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
        let blue_filler = CardDef {
            oracle_id: "o_bt_bfill".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Blue Foil".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Blue Foil",
                ManaCost::parse("{U}"),
                CardTypeMask::SORCERY,
                ColorMask::U,
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![island, pitch_spell, top_spell, blue_filler],
                decks: vec![],
            })
            .unwrap(),
        );

        // Solo in hand -> both return None; with another blue card -> Some and execute cleanly.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 907);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let pitch_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Hand);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), pitch_id, 0).is_none()
            );

            let _foil_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), pitch_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), pitch_id, 0, Zone::Hand).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert!(
                state.players()[0]
                    .exile
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(3))
            );
        }
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 908);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let top_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), top_id, 0).is_none()
            );

            let _foil_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), top_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), top_id, 0, Zone::Hand).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            let lib_top = *state.players()[0].library.last().unwrap();
            assert_eq!(state.object(lib_top).unwrap().card_def, CardDefId(3));
        }
    }

    #[test]
    fn synth_bug_t_graveyard_cast_exile_forage_delve_cr601_2a() {
        let mut tables = InternTables::default();
        tables.keywords.push("escape".into());
        let kw_escape = mtg_ir::KeywordId(0);
        tables.keywords.push("delve".into());
        let kw_delve = mtg_ir::KeywordId(1);

        let swamp = bug_t_make_land("Synth Swamp", ManaColor::B);
        let escape_cre = CardDef {
            oracle_id: "o_bt_esc".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Escape Beast".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Escape Beast".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{B}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 3,
                        toughness: 3,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: bug_t_make_kw(
                        kw_escape,
                        Some(ManaCost::parse("{2}{B}")),
                        vec![CostDef::ExileFrom {
                            zone: Zone::Graveyard,
                            count: Box::new(mtg_ir::Amount::Const { n: 3 }),
                            target: None,
                            from_top: false,
                            any_graveyard: false,
                            single_graveyard: false,
                            min_total_mana_value: None,
                            min_total_mana_value_amount: None,
                            min_colored_mana_symbols: None,
                            collect_evidence: false,
                            one_or_more: false,
                            total_mana_value_equals_x: false,
                            and_other_same_name_count: None,
                        }],
                    ),
                    id: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let forage_sorc = CardDef {
            oracle_id: "o_bt_for".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Forage".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth GY Forage".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{B}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Choice {
                        options: vec![],
                        forage: true,
                    }],
                    targets: vec![],
                    effect: vec![],
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
        let delve_sorc = CardDef {
            oracle_id: "o_bt_dlv".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Delve".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth GY Delve".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{3}{B}"),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(mtg_ir::KeywordAbility {
                    keyword: bug_t_make_kw(kw_delve, None, vec![]),
                    id: None,
                    span: Default::default(),
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let gy_perm = CardDef {
            oracle_id: "o_bt_gyp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Permission".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth GY Permission".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        body: mtg_ir::StaticBody::ZonePlayPermission {
                            zone: Zone::Graveyard,
                            allow_lands: false,
                            allow_spells: true,
                            spell_filter: Some(Box::new(ObjectFilter {
                                types_any: CardTypeMask::SORCERY,
                                ..Default::default()
                            })),
                            self_only: false,
                            during_your_turn_only: false,
                            slot_mode: None,
                            exile_if_put_into_graveyard: false,
                            exiled_with_self: false,
                            require_counter: None,
                            card_owner: None,
                            spend_mana_as_any_color: false,
                            enters_tapped: false,
                            grant_haste_until_eot: false,
                            put_into_graveyard_from_non_battlefield_this_turn: false,
                            alt_cost_defs: vec![],
                            grant_flash: false,
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
        let filler = CardDef {
            oracle_id: "o_bt_gyfill".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Filler".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth GY Filler",
                ManaCost::parse("{1}"),
                CardTypeMask::SORCERY,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![swamp, escape_cre, forage_sorc, delve_sorc, gy_perm, filler],
                decks: vec![],
            })
            .unwrap(),
        );

        // 1. Escape creature: 3 cards total in GY (2 fillers + self) -> None; 4 cards -> casts onto BF.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 909);
            for _ in 0..3 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            let esc_id =
                bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(1), Zone::Graveyard);
            let _f1 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            let _f2 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), esc_id, 0).is_none()
            );

            let _f3 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), esc_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), esc_id, 0, Zone::Graveyard).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            crate::stack::resolve_top_of_stack(&mut state).unwrap();
            assert!(
                state.players()[0]
                    .battlefield
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            );
        }

        // 2. Forage sorcery cast from GY via permission: 3 cards total in GY -> None; 4 cards -> Some & commits.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 910);
            for _ in 0..2 {
                add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            add_synth_bf(&mut state, PlayerId(0), CardDefId(4));
            let for_id =
                bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Graveyard);
            let _f1 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            let _f2 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), for_id, 0).is_none()
            );

            let _f3 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), for_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), for_id, 0, Zone::Graveyard).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.players()[0].exile.len(), 3);
        }

        // 3. Delve sorcery {3}{B} cast from GY with 1 Swamp: 3 cards total in GY -> None; 4 cards -> Some & delves 3 fillers.
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 911);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(4));
            let dlv_id =
                bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Graveyard);
            let f1 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            let f2 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), dlv_id, 0).is_none()
            );

            let f3 = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(
                crate::legality::can_cast_spell_option(&state, PlayerId(0), dlv_id, 0).is_some()
            );
            begin_cast_spell(&mut state, PlayerId(0), dlv_id, 0, Zone::Graveyard).unwrap();
            for fid in [f1, f2, f3] {
                apply_casting_action(
                    &mut state,
                    PlayerId(0),
                    &Action::SelectCostAid {
                        aid_kind: crate::action::CostAidKind::Delve,
                        object: fid,
                    },
                )
                .unwrap();
            }
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.players()[0].exile.len(), 3);
        }
    }

    #[test]
    fn synth_bug_t_channel_cycling_forecast_unearth_preserved() {
        let plains = bug_t_make_land("Synth Plains", ManaColor::W);
        let swamp = bug_t_make_land("Synth Swamp", ManaColor::B);
        let make_zone_act_card =
            |name: &str, oid: &str, zones: ZoneMask, timing: mtg_ir::Timing, cost: Vec<CostDef>| {
                CardDef {
                    oracle_id: oid.into(),
                    compiler_version: 46,
                    text_hash: "0".into(),
                    name: name.into(),
                    layout: CardLayout::Normal,
                    faces: vec![make_basic_face(
                        name,
                        ManaCost::parse("{2}"),
                        CardTypeMask::CREATURE,
                        ColorMask::empty(),
                        vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                            id: "act".into(),
                            cost,
                            targets: vec![],
                            effect: vec![],
                            modal: None,
                            timing,
                            is_mana: false,
                            activation_restrictions: vec![],
                            activation_condition: None,
                            activator_who: None,
                            power_up_reduction: false,
                            x_cant_be_zero: false,
                            activation_cost_reduction: None,
                            activation_zones: zones,
                            cost_keyword: None,
                            intrinsic: false,
                            text: None,
                            grant_condition: None,
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                            span: Default::default(),
                        })],
                    )],
                    fully_supported: true,
                }
            };

        let channel_card = make_zone_act_card(
            "Synth Channeler",
            "o_bt_ch",
            ZoneMask::HAND,
            mtg_ir::Timing::Instant,
            vec![
                CostDef::Mana {
                    cost: ManaCost::parse("{1}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                },
                CostDef::Discard {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: Some(mtg_ir::CostObjectTarget::SelfObj),
                    random: false,
                    same_name_pair: false,
                    same_name_as_controlled: None,
                    different_names: false,
                },
            ],
        );
        let composite_channel = make_zone_act_card(
            "Synth Composite Channel",
            "o_bt_comp",
            ZoneMask::HAND,
            mtg_ir::Timing::Instant,
            vec![
                CostDef::Discard {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: Some(mtg_ir::CostObjectTarget::SelfObj),
                    random: false,
                    same_name_pair: false,
                    same_name_as_controlled: None,
                    different_names: false,
                },
                CostDef::RevealFromHand {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: None,
                    share_color: false,
                },
            ],
        );
        let forecast_card = make_zone_act_card(
            "Synth Forecaster",
            "o_bt_fc",
            ZoneMask::HAND,
            mtg_ir::Timing::Instant,
            vec![
                CostDef::Mana {
                    cost: ManaCost::parse("{W}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                },
                CostDef::RevealFromHand {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: Some(mtg_ir::CostObjectTarget::SelfObj),
                    share_color: false,
                },
            ],
        );
        let unearth_card = make_zone_act_card(
            "Synth Unearther",
            "o_bt_un",
            ZoneMask::GRAVEYARD,
            mtg_ir::Timing::Sorcery,
            vec![
                CostDef::Mana {
                    cost: ManaCost::parse("{1}{B}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                },
                CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: Some(mtg_ir::CostObjectTarget::SelfObj),
                    from_top: false,
                    any_graveyard: false,
                    single_graveyard: false,
                    min_total_mana_value: None,
                    min_total_mana_value_amount: None,
                    min_colored_mana_symbols: None,
                    collect_evidence: false,
                    one_or_more: false,
                    total_mana_value_equals_x: false,
                    and_other_same_name_count: None,
                },
            ],
        );
        let dummy = CardDef {
            oracle_id: "o_bt_dum".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Dummy".into(),
            layout: CardLayout::Normal,
            faces: vec![make_basic_face(
                "Synth Dummy",
                ManaCost::parse("{1}"),
                CardTypeMask::SORCERY,
                ColorMask::empty(),
                vec![],
            )],
            fully_supported: true,
        };

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    plains,
                    swamp,
                    channel_card,
                    composite_channel,
                    forecast_card,
                    unearth_card,
                    dummy,
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        // Solo Channel / Cycling: Discard(SelfObj) works solo in Hand
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 912);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let ch_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(2), Zone::Hand);
            assert!(crate::legality::can_activate_ability(&state, PlayerId(0), ch_id, 0).is_some());
            begin_activate_ability(&mut state, PlayerId(0), ch_id, 0).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.players()[0].hand.len(), 0);
            assert_eq!(state.players()[0].graveyard.len(), 1);
        }

        // Solo Forecast: RevealFromHand(SelfObj) works solo in Hand
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 913);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            let fc_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(4), Zone::Hand);
            assert!(crate::legality::can_activate_ability(&state, PlayerId(0), fc_id, 0).is_some());
            begin_activate_ability(&mut state, PlayerId(0), fc_id, 0).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert!(state.players()[0].hand.contains(&fc_id));
        }

        // Solo Unearth: ExileFrom(GY, SelfObj) works solo in Graveyard
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 914);
            add_synth_bf(&mut state, PlayerId(0), CardDefId(0));
            add_synth_bf(&mut state, PlayerId(0), CardDefId(1));
            let un_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(5), Zone::Graveyard);
            assert!(crate::legality::can_activate_ability(&state, PlayerId(0), un_id, 0).is_some());
            begin_activate_ability(&mut state, PlayerId(0), un_id, 0).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert!(state.players()[0].graveyard.is_empty());
            assert_eq!(state.players()[0].exile.len(), 1);
        }

        // Composite Channel [Discard(SelfObj), RevealFromHand(1)]: None when solo, Some with 1 other card
        {
            let mut state = bug_t_boot_main1(Arc::clone(&db), CardDefId(0), 915);
            let comp_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);
            assert!(
                crate::legality::can_activate_ability(&state, PlayerId(0), comp_id, 0).is_none()
            );

            let dum_id = bug_t_spawn_in_zone(&mut state, PlayerId(0), CardDefId(6), Zone::Hand);
            assert!(
                crate::legality::can_activate_ability(&state, PlayerId(0), comp_id, 0).is_some()
            );
            begin_activate_ability(&mut state, PlayerId(0), comp_id, 0).unwrap();
            bug_t_finish_non_choice_steps(&mut state, PlayerId(0));
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(state.players()[0].hand, vec![dum_id]);
            assert!(
                state.players()[0]
                    .graveyard
                    .iter()
                    .any(|&id| state.object(id).unwrap().card_def == CardDefId(3))
            );
        }
    }
}
