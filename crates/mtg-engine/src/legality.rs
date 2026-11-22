use std::borrow::Cow;

use mtg_ir::{
    AbilityDef, ActivationRestriction, Affected, CardDefId, CardLayout, CardTypeMask, ColorMask,
    CostDef, FaceDef, GamePhase, KeywordDef, LoyaltySign, ManaColor, ManaCost, ManaPip, RuleMod,
    SpellCastRestrictionKind, StaticBody, TargetSpec, Timing, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{Action, SpecialActionKind, TargetRef};
use crate::auto_tap::{AutoTapPolicy, plan_auto_tap_cached};
#[cfg(test)]
use crate::costs::auto_tap_constraints_for_costs_cached;
use crate::costs::{
    CostAidCaps, CostListSelfContext, EffectiveCostBreakdown, can_pay_costs_and_mana_cached,
    can_pay_costs_and_mana_with_self_ctx_cached, can_pay_non_mana_costs_cached,
    compute_effective_ability_cost_cached, compute_effective_spell_cost_with_modes_cached,
    cost_uses_x, max_x_for_cost_list_cached, max_x_for_cost_list_with_ctx_cached,
};
use crate::eval::{BoardView, EvalContext, evaluate_condition};
use crate::faces::{
    ActiveAbilityWithCardRef, active_abilities_with_card_for_object, castable_faces,
    face_opts_for_object, is_room_card, playable_land_faces, resolve_object_faces,
};
use crate::filter::matches_object_filter;
use crate::id::{ObjectId, PlayerId};
use crate::keywords::{keyword_activated_zones, synthesize_keyword_activated_ability};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::mana::{
    ManaAbilityCandidate, ManaSpendContext, ManaSpendContextKind,
    can_activate_mana_candidate_now_cached, can_pay_mana_from_pool_cached,
    enumerate_mana_abilities_cached, index_to_color,
};
use crate::state::{
    AltCastKind, CastOption, GameState, LayerCache, NONE_OBJECT_ID, ObjectFlags,
    StaticPresenceFlags,
};
use crate::targets::{
    build_ability_target_source_info_cached, build_spell_target_source_info_cached,
    has_legal_modal_target_assignment_cached, has_legal_target_assignment_cached,
};

#[derive(Debug, Clone)]
pub struct ResolvedActivatedAbility<'a> {
    pub ability_index: u16,
    pub ability: Cow<'a, mtg_ir::ActivatedAbility>,
    pub source_card_def: CardDefId,
    pub face_index: u8,
    pub mana_as_any_color: bool,
    pub grant_once_each_turn: bool,
}

fn is_main_phase(phase: GamePhase) -> bool {
    matches!(phase, GamePhase::Main1 | GamePhase::Main2)
}

fn is_combat_phase(phase: GamePhase) -> bool {
    matches!(
        phase,
        GamePhase::BeginCombat
            | GamePhase::DeclareAttackers
            | GamePhase::DeclareBlockers
            | GamePhase::FirstStrikeDamage
            | GamePhase::CombatDamage
            | GamePhase::EndCombat
    )
}

fn phase_ordinal(phase: GamePhase) -> u8 {
    match phase {
        GamePhase::Untap => 0,
        GamePhase::Upkeep => 1,
        GamePhase::Draw => 2,
        GamePhase::Main1 => 3,
        GamePhase::BeginCombat => 4,
        GamePhase::DeclareAttackers => 5,
        GamePhase::DeclareBlockers => 6,
        GamePhase::FirstStrikeDamage => 7,
        GamePhase::CombatDamage => 8,
        GamePhase::EndCombat => 9,
        GamePhase::Main2 => 10,
        GamePhase::EndStep => 11,
        GamePhase::Cleanup => 12,
    }
}

fn spell_cast_restriction_holds(
    state: &GameState,
    player: PlayerId,
    r: SpellCastRestrictionKind,
) -> bool {
    let phase = state.phase();
    let ord = phase_ordinal(phase);
    match r {
        SpellCastRestrictionKind::DuringCombat => is_combat_phase(phase),
        SpellCastRestrictionKind::DuringYourTurn => state.active_player() == player,
        SpellCastRestrictionKind::DuringOpponentsTurn => state.active_player() != player,
        SpellCastRestrictionKind::DuringYourUpkeep => {
            state.active_player() == player && phase == GamePhase::Upkeep
        }
        SpellCastRestrictionKind::BeforeAttackers => {
            ord < phase_ordinal(GamePhase::DeclareAttackers)
        }
        SpellCastRestrictionKind::AfterAttackers => {
            ord >= phase_ordinal(GamePhase::DeclareAttackers)
        }
        SpellCastRestrictionKind::BeforeBlockers => ord < phase_ordinal(GamePhase::DeclareBlockers),
        SpellCastRestrictionKind::DuringBlockers => phase == GamePhase::DeclareBlockers,
        SpellCastRestrictionKind::AfterBlockers => ord >= phase_ordinal(GamePhase::DeclareBlockers),
        SpellCastRestrictionKind::AfterCombat => ord > phase_ordinal(GamePhase::EndCombat),
    }
}

fn activation_restriction_holds(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    r: ActivationRestriction,
) -> bool {
    let phase = state.phase();
    let ord = phase_ordinal(phase);
    match r {
        ActivationRestriction::OnceEachTurn => state.player(player).is_some_and(|p| {
            !p.once_per_turn_abilities_used()
                .contains(&(object, ability_index))
        }),
        ActivationRestriction::OncePerGame => state.player(player).is_some_and(|p| {
            !p.once_per_game_abilities_used()
                .contains(&(object, ability_index))
        }),
        ActivationRestriction::DuringYourTurn => state.active_player() == player,
        ActivationRestriction::DuringOpponentsTurn => state.active_player() != player,
        ActivationRestriction::DuringYourUpkeep => {
            state.active_player() == player && phase == GamePhase::Upkeep
        }
        ActivationRestriction::DuringCombat => is_combat_phase(phase),
        ActivationRestriction::BeforeAttackers => ord < phase_ordinal(GamePhase::DeclareAttackers),
        ActivationRestriction::AfterAttackers => ord >= phase_ordinal(GamePhase::DeclareAttackers),
        ActivationRestriction::BeforeBlockers => ord < phase_ordinal(GamePhase::DeclareBlockers),
        ActivationRestriction::DuringBlockers => phase == GamePhase::DeclareBlockers,
        ActivationRestriction::AfterBlockers => ord >= phase_ordinal(GamePhase::DeclareBlockers),
        ActivationRestriction::AfterCombat => ord > phase_ordinal(GamePhase::EndCombat),
        ActivationRestriction::OnlyAsInstant => true,
    }
}

pub(crate) fn find_keyword<'a>(
    face: &'a FaceDef,
    db: &mtg_ir::CardDatabase,
    name: &str,
) -> Option<&'a KeywordDef> {
    face.abilities.iter().find_map(|ab| match ab {
        AbilityDef::Keyword(k)
            if db
                .keyword_name(k.keyword.keyword)
                .is_some_and(|n| n.eq_ignore_ascii_case(name)) =>
        {
            Some(&k.keyword)
        }
        _ => None,
    })
}

fn has_keyword(face: &FaceDef, db: &mtg_ir::CardDatabase, name: &str) -> bool {
    find_keyword(face, db, name).is_some()
}

pub(crate) fn bestow_target_spec() -> TargetSpec {
    TargetSpec {
        kind: mtg_ir::TargetKind::Object,
        filter: mtg_ir::ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
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
        zones: ZoneMask::BATTLEFIELD,
    }
}

pub(crate) fn hand_foretell_cost(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
) -> Option<ManaCost> {
    hand_foretell_cost_cached(state, player, object, state.evaluate_board())
}

pub(crate) fn hand_foretell_cost_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    cache: &LayerCache,
) -> Option<ManaCost> {
    let obj = state.object(object)?;
    if obj.zone != Zone::Hand || obj.owner != player {
        return None;
    }
    let db = state.db();
    let eff_def = state
        .rare(object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let card = db.get(eff_def)?;
    let f0 = card.faces.first()?;
    if let Some(fk) = find_keyword(f0, db, "foretell") {
        return Some(fk.cost.clone().unwrap_or_default());
    }
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::GRANT_HAND_FORETELL)
    {
        return None;
    }
    let mut max_reduce: Option<u16> = None;
    for p in state.players() {
        for &perm_id in &p.battlefield {
            let Some(perm_obj) = state.object(perm_id) else {
                continue;
            };
            if perm_obj.is_phased_out() {
                continue;
            }
            let perm_ctrl = cache
                .get(perm_id)
                .map_or(perm_obj.controller, |e| e.controller);
            if perm_ctrl != player {
                continue;
            }
            let perm_ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            for ab in active_abilities_with_card_for_object(state, perm_id, &perm_ctx) {
                if let AbilityDef::Static(st) = ab.ability
                    && let StaticBody::GrantHandForetell {
                        filter,
                        reduce_generic,
                    } = &st.static_def.body
                {
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &perm_ctx)
                    {
                        continue;
                    }
                    if matches_object_filter(state, object, filter, &perm_ctx) {
                        max_reduce =
                            Some(max_reduce.map_or(*reduce_generic, |m| m.max(*reduce_generic)));
                    }
                }
            }
        }
    }
    max_reduce.map(|red| {
        let mut mc = f0.characteristics.mana_cost.clone();
        mc.generic = mc.generic.saturating_sub(u32::from(red));
        mc
    })
}

fn face_spell_ability(
    face: &FaceDef,
    cleaved: bool,
    gift_promised: bool,
) -> Option<&mtg_ir::SpellAbility> {
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

/// Checks `ZonePlayPermission` statics on the battlefield/command that grant casting spells from `zone`.
fn matching_zone_play_permissions_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    from_zone: Zone,
    for_land: bool,
    cache: &LayerCache,
) -> Vec<(ObjectId, bool, bool, Vec<CostDef>)> {
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::ZONE_PLAY_PERMISSION)
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Some(obj) = state.object(object) else {
        return out;
    };
    for p in state.players() {
        for &perm_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(perm_obj) = state.object(perm_id) else {
                continue;
            };
            if perm_obj.is_phased_out() {
                continue;
            }
            let perm_ctrl = cache
                .get(perm_id)
                .map_or(perm_obj.controller, |e| e.controller);
            if perm_ctrl != player {
                continue;
            }
            let perm_ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            let abilities = active_abilities_with_card_for_object(state, perm_id, &perm_ctx);
            for ab in abilities {
                if let AbilityDef::Static(st) = ab.ability
                    && let StaticBody::ZonePlayPermission {
                        zone,
                        allow_lands,
                        allow_spells,
                        spell_filter,
                        self_only,
                        during_your_turn_only,
                        exiled_with_self,
                        require_counter,
                        card_owner,
                        spend_mana_as_any_color,
                        alt_cost_defs,
                        grant_flash,
                        put_into_graveyard_from_non_battlefield_this_turn,
                        ..
                    } = &st.static_def.body
                {
                    if *zone != from_zone {
                        continue;
                    }
                    if for_land && !*allow_lands {
                        continue;
                    }
                    if !for_land && !*allow_spells {
                        continue;
                    }
                    if *self_only && perm_id != object {
                        continue;
                    }
                    if *during_your_turn_only && state.active_player() != player {
                        continue;
                    }
                    if *exiled_with_self
                        && state.rare(object).and_then(|r| r.exiled_by) != Some(perm_id)
                    {
                        continue;
                    }
                    if let Some(cid) = require_counter
                        && state.counter_count(object, *cid) == 0
                    {
                        continue;
                    }
                    if let Some(rel) = card_owner
                        && !crate::filter::matches_relative_control(obj.owner, *rel, &perm_ctx)
                    {
                        continue;
                    }
                    if *put_into_graveyard_from_non_battlefield_this_turn
                        && from_zone == Zone::Graveyard
                        && !state
                            .turn_history()
                            .put_into_graveyard_from_non_battlefield_this_turn
                            .contains(&object)
                    {
                        continue;
                    }
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &perm_ctx)
                    {
                        continue;
                    }
                    if let Some(sf) = spell_filter.as_deref()
                        && !matches_object_filter(state, object, sf, &perm_ctx)
                    {
                        continue;
                    }
                    out.push((
                        perm_id,
                        *spend_mana_as_any_color,
                        *grant_flash,
                        alt_cost_defs.clone(),
                    ));
                }
            }
        }
    }
    out
}

/// Enumerates all structurally valid [`CastOption`]s for `object` in its current zone,
/// assigning deterministic `option_id`s (`0` = primary/normal/adventure-from-exile, `1+` = secondary/alt).
pub fn enumerate_cast_options(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
) -> Vec<CastOption> {
    enumerate_cast_options_cached(state, player, object, state.evaluate_board())
}

pub(crate) fn enumerate_cast_options_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    cache: &LayerCache,
) -> Vec<CastOption> {
    enumerate_cast_options_with_perms_cached(state, player, object, cache).0
}

fn enumerate_cast_options_with_perms_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    cache: &LayerCache,
) -> (Vec<CastOption>, Option<(bool, bool)>) {
    let mut out = Vec::new();
    let Some(obj) = state.object(object) else {
        return (out, None);
    };
    let zone = obj.zone;
    if matches!(zone, Zone::Battlefield | Zone::Stack) {
        return (out, None);
    }

    let db = state.db();
    let rare = state.rare(object);
    let eff_def = rare
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let Some(card) = db.get(eff_def) else {
        return (out, None);
    };

    let (zone_perms, reused_zone_perms) = if matches!(zone, Zone::Hand | Zone::Command) {
        (Vec::new(), None)
    } else {
        let zp = matching_zone_play_permissions_cached(state, player, object, zone, false, cache);
        let any_color = zp.iter().any(|(_, ac, _, _)| *ac);
        let grant_flash = zp.iter().any(|(_, _, gf, _)| *gf);
        (zp, Some((any_color, grant_flash)))
    };
    let playable_exile = rare
        .and_then(|r| r.playable_from_exile_by.as_ref())
        .filter(|p| p.player == player && p.expires_turn.is_none_or(|t| t >= state.turn_number()));

    let mut base_zone_allowed = false;
    let mut base_cost_override: Option<ManaCost> = None;
    let mut base_extra_costs: Vec<CostDef> = Vec::new();
    let mut base_perm_source: Option<ObjectId> = None;

    match zone {
        Zone::Hand => {
            base_zone_allowed = obj.owner == player;
        }
        Zone::Command => {
            base_zone_allowed = obj.is_commander() && obj.owner == player;
        }
        Zone::Exile => {
            if rare.is_some_and(|r| r.on_an_adventure) && obj.owner == player {
                base_zone_allowed = true;
            } else if let Some(pe) = playable_exile {
                base_zone_allowed = true;
                base_cost_override = pe.cast_with_mana_cost.clone();
            } else if let Some((src, _, _, extra)) = zone_perms.first() {
                base_zone_allowed = true;
                base_perm_source = Some(*src);
                base_extra_costs = extra.clone();
            }
        }
        Zone::Graveyard => {
            if let Some((src, _, _, extra)) = zone_perms.first() {
                base_zone_allowed = true;
                base_perm_source = Some(*src);
                base_extra_costs = extra.clone();
            }
        }
        Zone::Library => {
            let is_top = state
                .player(obj.owner)
                .and_then(|p| p.library.last().copied())
                == Some(object);
            if is_top && let Some((src, _, _, extra)) = zone_perms.first() {
                base_zone_allowed = true;
                base_perm_source = Some(*src);
                base_extra_costs = extra.clone();
            }
        }
        Zone::Battlefield | Zone::Stack => {}
    }

    let push_opt = |out: &mut Vec<CastOption>, mut opt: CastOption| {
        opt.option_id = out.len() as u16;
        out.push(opt);
    };

    let face_opts = castable_faces(db, eff_def, zone);

    // 1. Primary / secondary / aftermath / fuse options from castable_faces
    for fopt in &face_opts {
        let is_aftermath_gy = fopt.is_aftermath && zone == Zone::Graveyard && obj.owner == player;
        if !base_zone_allowed && !is_aftermath_gy {
            continue;
        }
        // In Exile with on_an_adventure, only face 0 is castable (castable_faces already filters).
        let alt_kind = if zone == Zone::Command && fopt.face_index == 0 {
            AltCastKind::CommandZoneCast
        } else if fopt.is_fuse {
            AltCastKind::Fuse
        } else if fopt.is_aftermath {
            AltCastKind::Aftermath
        } else if fopt.is_adventure {
            AltCastKind::AdventureHalf
        } else if fopt.is_omen {
            AltCastKind::OmenHalf
        } else if fopt.face_index > 0 {
            match card.layout {
                CardLayout::Split => AltCastKind::SplitSecondHalf,
                CardLayout::ModalDfc => AltCastKind::ModalBackFace,
                _ => AltCastKind::Normal,
            }
        } else {
            AltCastKind::Normal
        };

        push_opt(
            &mut out,
            CastOption {
                option_id: 0,
                face_idx: fopt.face_index,
                from_zone: zone,
                alt_kind,
                kicker_indices: SmallVec::new(),
                is_kicked: false,
                gift_promised: false,
                fused: fopt.is_fuse,
                cleaved: false,
                spree_required: card
                    .faces
                    .get(usize::from(fopt.face_index))
                    .is_some_and(|f| has_keyword(f, db, "spree")),
                optional_cost_indices: SmallVec::new(),
                mana_cost_override: base_cost_override.clone(),
                extra_costs: base_extra_costs.clone(),
                permission_source: base_perm_source,
            },
        );
    }

    // 2. Prototype (face 0)
    if base_zone_allowed
        && let Some(f0) = card.faces.first()
        && let Some(proto) = f0
            .prototype_spec
            .as_ref()
            .or_else(|| find_keyword(f0, db, "prototype").and_then(|k| k.prototype_spec.as_ref()))
    {
        push_opt(
            &mut out,
            CastOption {
                option_id: 0,
                face_idx: 0,
                from_zone: zone,
                alt_kind: AltCastKind::Prototype,
                kicker_indices: SmallVec::new(),
                is_kicked: false,
                gift_promised: false,
                fused: false,
                cleaved: false,
                spree_required: false,
                optional_cost_indices: SmallVec::new(),
                mana_cost_override: Some(proto.mana_cost.clone()),
                extra_costs: base_extra_costs.clone(),
                permission_source: base_perm_source,
            },
        );
    }

    // 3. Face-down Morph / Megamorph / Disguise from Hand/ZonePerm
    if base_zone_allowed
        && let Some(f0) = card.faces.first()
        && !f0.characteristics.types.contains(CardTypeMask::LAND)
    {
        if has_keyword(f0, db, "morph") || has_keyword(f0, db, "megamorph") {
            push_opt(
                &mut out,
                CastOption {
                    option_id: 0,
                    face_idx: 0,
                    from_zone: zone,
                    alt_kind: AltCastKind::MorphFaceDown,
                    kicker_indices: SmallVec::new(),
                    is_kicked: false,
                    gift_promised: false,
                    fused: false,
                    cleaved: false,
                    spree_required: false,
                    optional_cost_indices: SmallVec::new(),
                    mana_cost_override: Some(ManaCost::parse("{3}")),
                    extra_costs: base_extra_costs.clone(),
                    permission_source: base_perm_source,
                },
            );
        }
        if has_keyword(f0, db, "disguise") {
            push_opt(
                &mut out,
                CastOption {
                    option_id: 0,
                    face_idx: 0,
                    from_zone: zone,
                    alt_kind: AltCastKind::DisguiseFaceDown,
                    kicker_indices: SmallVec::new(),
                    is_kicked: false,
                    gift_promised: false,
                    fused: false,
                    cleaved: false,
                    spree_required: false,
                    optional_cost_indices: SmallVec::new(),
                    mana_cost_override: Some(ManaCost::parse("{3}")),
                    extra_costs: base_extra_costs.clone(),
                    permission_source: base_perm_source,
                },
            );
        }
    }

    // 4. Kicker / Cleave / Gift / Overload / Bestow / Dash / Blitz / Evoke / Emerge / Warp on castable faces
    if base_zone_allowed {
        for fopt in &face_opts {
            let Some(face) = card.faces.get(usize::from(fopt.face_index)) else {
                continue;
            };
            if let Some(kk) =
                find_keyword(face, db, "kicker").or_else(|| find_keyword(face, db, "multikicker"))
            {
                for (ki, _kc) in kk.kicker_costs.iter().enumerate() {
                    let mut kick_idx = SmallVec::new();
                    kick_idx.push(ki as u16);
                    push_opt(
                        &mut out,
                        CastOption {
                            option_id: 0,
                            face_idx: fopt.face_index,
                            from_zone: zone,
                            alt_kind: AltCastKind::Normal,
                            kicker_indices: kick_idx,
                            is_kicked: true,
                            gift_promised: false,
                            fused: false,
                            cleaved: false,
                            spree_required: false,
                            optional_cost_indices: SmallVec::new(),
                            mana_cost_override: base_cost_override.clone(),
                            extra_costs: base_extra_costs.clone(),
                            permission_source: base_perm_source,
                        },
                    );
                }
            }
            if face.cleaved_spell_spec.is_some()
                && let Some(ck) = find_keyword(face, db, "cleave")
            {
                push_opt(
                    &mut out,
                    CastOption {
                        option_id: 0,
                        face_idx: fopt.face_index,
                        from_zone: zone,
                        alt_kind: AltCastKind::Cleave,
                        kicker_indices: SmallVec::new(),
                        is_kicked: false,
                        gift_promised: false,
                        fused: false,
                        cleaved: true,
                        spree_required: false,
                        optional_cost_indices: SmallVec::new(),
                        mana_cost_override: ck.cost.clone(),
                        extra_costs: ck.cost_defs.clone(),
                        permission_source: base_perm_source,
                    },
                );
            }
            if face.gift_promised_spell_spec.is_some() {
                push_opt(
                    &mut out,
                    CastOption {
                        option_id: 0,
                        face_idx: fopt.face_index,
                        from_zone: zone,
                        alt_kind: AltCastKind::Normal,
                        kicker_indices: SmallVec::new(),
                        is_kicked: false,
                        gift_promised: true,
                        fused: false,
                        cleaved: false,
                        spree_required: false,
                        optional_cost_indices: SmallVec::new(),
                        mana_cost_override: base_cost_override.clone(),
                        extra_costs: base_extra_costs.clone(),
                        permission_source: base_perm_source,
                    },
                );
            }
            for (kw_name, kind) in [
                ("overload", AltCastKind::Overload),
                ("bestow", AltCastKind::Bestow),
                ("dash", AltCastKind::Dash),
                ("blitz", AltCastKind::Blitz),
                ("evoke", AltCastKind::Evoke),
                ("emerge", AltCastKind::Emerge),
                ("warp", AltCastKind::Warp),
            ] {
                if let Some(kw) = find_keyword(face, db, kw_name) {
                    push_opt(
                        &mut out,
                        CastOption {
                            option_id: 0,
                            face_idx: fopt.face_index,
                            from_zone: zone,
                            alt_kind: kind,
                            kicker_indices: SmallVec::new(),
                            is_kicked: false,
                            gift_promised: false,
                            fused: false,
                            cleaved: false,
                            spree_required: false,
                            optional_cost_indices: SmallVec::new(),
                            mana_cost_override: kw.cost.clone(),
                            extra_costs: kw.cost_defs.clone(),
                            permission_source: base_perm_source,
                        },
                    );
                }
            }
        }
    }

    // 5. Foretell cast from Exile
    if zone == Zone::Exile
        && obj.owner == player
        && obj.flags.contains(ObjectFlags::FORETOLD)
        && !obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN)
        && let Some(f0) = card.faces.first()
    {
        let innate_fk = find_keyword(f0, db, "foretell");
        let granted_fd_cost = rare
            .and_then(|r| r.face_down.as_deref())
            .filter(|fd| fd.kind == crate::state::FaceDownKind::Foretell)
            .and_then(|fd| fd.turn_up_mana_cost.as_ref());
        if innate_fk.is_some() || granted_fd_cost.is_some() {
            push_opt(
                &mut out,
                CastOption {
                    option_id: 0,
                    face_idx: 0,
                    from_zone: Zone::Exile,
                    alt_kind: AltCastKind::ForetellCast,
                    kicker_indices: SmallVec::new(),
                    is_kicked: false,
                    gift_promised: false,
                    fused: false,
                    cleaved: false,
                    spree_required: false,
                    optional_cost_indices: SmallVec::new(),
                    mana_cost_override: innate_fk
                        .map_or_else(|| granted_fd_cost.cloned(), |fk| fk.cost.clone()),
                    extra_costs: innate_fk.map(|fk| fk.cost_defs.clone()).unwrap_or_default(),
                    permission_source: None,
                },
            );
        }
    }

    // 5b. Plot cast from Exile (CR 702.170d)
    if zone == Zone::Exile
        && obj.owner == player
        && let Some(r) = rare
        && r.plotted_by == Some(player)
        && (r.plotted_turn.is_some_and(|t| state.turn_number() > t)
            || !obj.flags.contains(ObjectFlags::ENTERED_THIS_TURN))
        && let Some(f0) = card.faces.first()
        && !f0.characteristics.types.contains(CardTypeMask::LAND)
    {
        push_opt(
            &mut out,
            CastOption {
                option_id: 0,
                face_idx: 0,
                from_zone: Zone::Exile,
                alt_kind: AltCastKind::PlotCast,
                kicker_indices: SmallVec::new(),
                is_kicked: false,
                gift_promised: false,
                fused: false,
                cleaved: false,
                spree_required: has_keyword(f0, db, "spree"),
                optional_cost_indices: SmallVec::new(),
                mana_cost_override: Some(ManaCost::default()),
                extra_costs: Vec::new(),
                permission_source: None,
            },
        );
    }

    // 6. Graveyard self-cast keywords (Flashback, Escape, Disturb, Encore, JumpStart, Harmonize)
    if zone == Zone::Graveyard
        && obj.owner == player
        && let Some(f0) = card.faces.first()
        && !f0.characteristics.types.contains(CardTypeMask::LAND)
    {
        for (kw_name, kind, target_face) in [
            ("flashback", AltCastKind::Flashback, 0u8),
            ("escape", AltCastKind::Escape, 0u8),
            ("encore", AltCastKind::Encore, 0u8),
            ("jump-start", AltCastKind::JumpStart, 0u8),
            ("harmonize", AltCastKind::Harmonize, 0u8),
        ] {
            if let Some(kw) = find_keyword(f0, db, kw_name) {
                let cost_override = kw.cost.clone().or_else(|| {
                    if kw_name == "jump-start" || kw_name == "harmonize" {
                        Some(f0.characteristics.mana_cost.clone())
                    } else {
                        None
                    }
                });
                push_opt(
                    &mut out,
                    CastOption {
                        option_id: 0,
                        face_idx: target_face,
                        from_zone: Zone::Graveyard,
                        alt_kind: kind,
                        kicker_indices: SmallVec::new(),
                        is_kicked: false,
                        gift_promised: false,
                        fused: false,
                        cleaved: false,
                        spree_required: false,
                        optional_cost_indices: SmallVec::new(),
                        mana_cost_override: cost_override,
                        extra_costs: kw.cost_defs.clone(),
                        permission_source: None,
                    },
                );
            }
        }
        if let Some(dk) = find_keyword(f0, db, "disturb") {
            let target_face = if card.faces.len() > 1 { 1 } else { 0 };
            push_opt(
                &mut out,
                CastOption {
                    option_id: 0,
                    face_idx: target_face,
                    from_zone: Zone::Graveyard,
                    alt_kind: AltCastKind::Disturb,
                    kicker_indices: SmallVec::new(),
                    is_kicked: false,
                    gift_promised: false,
                    fused: false,
                    cleaved: false,
                    spree_required: false,
                    optional_cost_indices: SmallVec::new(),
                    mana_cost_override: dk.cost.clone(),
                    extra_costs: dk.cost_defs.clone(),
                    permission_source: None,
                },
            );
        }
    }

    // 7. External AltCastPermission statics on battlefield
    if cache
        .static_presence
        .contains(StaticPresenceFlags::ALT_CAST_PERMISSION)
    {
        for p in state.players() {
            for &perm_id in p.battlefield.iter().chain(p.command.iter()) {
                let Some(perm_obj) = state.object(perm_id) else {
                    continue;
                };
                if perm_obj.is_phased_out() {
                    continue;
                }
                let perm_ctrl = cache
                    .get(perm_id)
                    .map_or(perm_obj.controller, |e| e.controller);
                let perm_ctx = EvalContext::for_source(perm_ctrl, perm_id)
                    .with_board(BoardView::Cached(cache));
                let abilities = active_abilities_with_card_for_object(state, perm_id, &perm_ctx);
                for ab in abilities {
                    if let AbilityDef::Static(st) = ab.ability
                        && let StaticBody::AltCastPermission {
                            mana_cost,
                            use_printed_mana_cost,
                            cost_defs,
                            affected,
                            allowed_zones,
                            during_your_turn_only,
                            any_player,
                            require_zone_permission,
                            ..
                        } = &st.static_def.body
                    {
                        if !*any_player && perm_ctrl != player {
                            continue;
                        }
                        if *during_your_turn_only && state.active_player() != player {
                            continue;
                        }
                        let zm = if allowed_zones.is_empty() {
                            ZoneMask::HAND
                        } else {
                            *allowed_zones
                        };
                        if !zm.contains(ZoneMask::from_zone(zone)) {
                            continue;
                        }
                        if *require_zone_permission && zone != Zone::Hand && !base_zone_allowed {
                            continue;
                        }
                        if zone == Zone::Hand && obj.owner != player && !*any_player {
                            continue;
                        }
                        let aff_ok = match affected {
                            Affected::SelfObj => perm_id == object,
                            Affected::AttachedTo => false,
                            Affected::Filter { filter } => {
                                matches_object_filter(state, object, filter, &perm_ctx)
                            }
                        };
                        if !aff_ok {
                            continue;
                        }
                        if let Some(cond) = st.static_def.condition.as_deref()
                            && !evaluate_condition(state, cond, &perm_ctx)
                        {
                            continue;
                        }
                        if let Some(f0) = card.faces.first()
                            && !f0.characteristics.types.contains(CardTypeMask::LAND)
                        {
                            let ov = if *use_printed_mana_cost {
                                None
                            } else {
                                Some(mana_cost.clone())
                            };
                            let ext_idx = out.len() as u16;
                            push_opt(
                                &mut out,
                                CastOption {
                                    option_id: 0,
                                    face_idx: 0,
                                    from_zone: zone,
                                    alt_kind: AltCastKind::ExternalPermission(ext_idx),
                                    kicker_indices: SmallVec::new(),
                                    is_kicked: false,
                                    gift_promised: false,
                                    fused: false,
                                    cleaved: false,
                                    spree_required: false,
                                    optional_cost_indices: SmallVec::new(),
                                    mana_cost_override: ov,
                                    extra_costs: cost_defs.clone(),
                                    permission_source: Some(perm_id),
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    (out, reused_zone_perms)
}

/// Builds a [`ManaSpendContext`] for casting `object` with `option`.
pub fn build_mana_spend_context(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    option: &CastOption,
) -> ManaSpendContext {
    build_mana_spend_context_cached(state, player, object, option, state.evaluate_board())
}

pub(crate) fn build_mana_spend_context_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    option: &CastOption,
    cache: &LayerCache,
) -> ManaSpendContext {
    let spend_any_color = matching_zone_play_permissions_cached(
        state,
        player,
        object,
        option.from_zone,
        false,
        cache,
    )
    .into_iter()
    .any(|(_, any_color, _, _)| any_color);
    build_mana_spend_context_with_any_color(state, player, object, option, spend_any_color)
}

fn build_mana_spend_context_with_any_color(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    option: &CastOption,
    spend_any_color: bool,
) -> ManaSpendContext {
    let is_commander = crate::commander::is_commander_object(state, object);
    let eff_def = state
        .rare(object)
        .and_then(|r| r.copiable.as_deref())
        .map_or_else(
            || state.object(object).map(|o| o.card_def),
            |c| Some(c.card_def),
        );
    let face = eff_def
        .and_then(|id| state.db().get(id))
        .and_then(|c| c.faces.get(usize::from(option.face_idx)));
    let base_cost = option
        .mana_cost_override
        .as_ref()
        .or_else(|| face.map(|f| &f.characteristics.mana_cost));
    let has_x_in_cost = base_cost.is_some_and(|mc| mc.x_count > 0);
    let (x_spend_colors, x_colored_only, x_max_one_per_color) = face
        .and_then(|f| {
            f.abilities.iter().find_map(|ab| match ab {
                AbilityDef::Static(st) => match st.static_def.body {
                    StaticBody::ManaSpendRestriction {
                        x_colors,
                        x_colored_only,
                        x_max_one_per_color,
                        ..
                    } => Some((x_colors, x_colored_only, x_max_one_per_color)),
                    _ => None,
                },
                _ => None,
            })
        })
        .unwrap_or((ColorMask::empty(), false, false));

    let source_restriction = face.and_then(|f| {
        f.abilities.iter().find_map(|ab| match ab {
            AbilityDef::Static(st) => match st.static_def.body {
                StaticBody::ManaSpendRestriction { source_kind, .. } => source_kind,
                _ => None,
            },
            _ => None,
        })
    });

    ManaSpendContext {
        player,
        kind: if option.alt_kind == AltCastKind::ForetellCast {
            ManaSpendContextKind::ForetellAction
        } else {
            ManaSpendContextKind::Spell
        },
        source_object: Some(object),
        face_idx: option.face_idx,
        from_zone: option.from_zone,
        is_commander,
        has_x_in_cost,
        is_fused: option.fused,
        is_prototyped: option.alt_kind == AltCastKind::Prototype,
        is_bestowed: option.alt_kind == AltCastKind::Bestow,
        is_face_down: matches!(
            option.alt_kind,
            AltCastKind::MorphFaceDown | AltCastKind::DisguiseFaceDown
        ),
        alt_cast_spend_any_color: spend_any_color,
        mana_as_any_color_ability: false,
        x_spend_colors,
        x_colored_only,
        x_max_one_per_color,
        source_restriction,
    }
}

fn spell_has_flash_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    face: &FaceDef,
    types: CardTypeMask,
    zone_perm_grant_flash: bool,
    cache: &LayerCache,
) -> bool {
    if types.contains(CardTypeMask::INSTANT) {
        return true;
    }
    if has_keyword(face, state.db(), "flash") {
        return true;
    }
    if zone_perm_grant_flash {
        return true;
    }
    if let Some(ep) = cache.player(player) {
        let p_ctx = EvalContext::new(player).with_board(BoardView::Cached(cache));
        for rm in &ep.active_rule_mods {
            if let RuleMod::CastAsThoughFlash {
                spells,
                self_only,
                first_matching_this_turn,
                ..
            } = &rm.rule
            {
                if *self_only && rm.source != Some(object) {
                    continue;
                }
                if *first_matching_this_turn
                    && state
                        .player(player)
                        .is_some_and(|p| p.spells_cast_this_turn > 0)
                {
                    continue;
                }
                if let Some(sf) = spells.as_deref()
                    && !matches_object_filter(state, object, sf, &p_ctx)
                {
                    continue;
                }
                return true;
            }
        }
    }
    false
}

#[allow(dead_code)]
fn spell_can_be_paid_cached(
    state: &GameState,
    player: PlayerId,
    breakdown: &EffectiveCostBreakdown,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    spell_can_be_paid_with_x_cached(state, player, breakdown, ctx, None, cache)
}

pub(crate) fn spell_can_be_paid_with_x_cached(
    state: &GameState,
    player: PlayerId,
    breakdown: &EffectiveCostBreakdown,
    spend_ctx: &ManaSpendContext,
    x_value: Option<u32>,
    cache: &LayerCache,
) -> bool {
    let source = spend_ctx.source_object.unwrap_or(NONE_OBJECT_ID);
    let mut effective_spend_ctx = spend_ctx.clone();
    effective_spend_ctx.x_spend_colors |= breakdown.x_spend_colors;
    effective_spend_ctx.x_colored_only |= breakdown.x_colored_only;
    effective_spend_ctx.x_max_one_per_color |= breakdown.x_max_one_per_color;
    if breakdown.source_restriction.is_some() {
        effective_spend_ctx.source_restriction = breakdown.source_restriction;
    }
    effective_spend_ctx.has_x_in_cost = spend_ctx.has_x_in_cost
        || breakdown.base_mana.x_count > 0
        || breakdown.locked_mana.x_count > 0
        || breakdown.non_mana_costs.iter().any(cost_uses_x);
    let aids = CostAidCaps::from_breakdown(breakdown);
    let self_ctx = if source != NONE_OBJECT_ID {
        CostListSelfContext::for_spell_cast(effective_spend_ctx.from_zone)
    } else {
        CostListSelfContext::default()
    };
    can_pay_costs_and_mana_with_self_ctx_cached(
        state,
        player,
        source,
        &breakdown.locked_mana,
        &breakdown.non_mana_costs,
        &effective_spend_ctx,
        aids,
        x_value,
        self_ctx,
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
fn spell_can_be_paid_for_modes_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    opt: &CastOption,
    spend_ctx: &ManaSpendContext,
    x_value: Option<u32>,
    chosen_modes: &[u16],
    cache: &LayerCache,
) -> bool {
    let Ok(breakdown) = compute_effective_spell_cost_with_modes_cached(
        state,
        player,
        object,
        opt,
        chosen_modes,
        x_value,
        &opt.optional_cost_indices,
        &[],
        cache,
    ) else {
        return false;
    };
    spell_can_be_paid_with_x_cached(state, player, &breakdown, spend_ctx, x_value, cache)
}

#[allow(clippy::too_many_arguments)]
fn spell_has_legal_targets_cached<F>(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    spell_ab: &mtg_ir::SpellAbility,
    x_val: Option<u32>,
    eval_ctx: &EvalContext<'_>,
    cost_ok: F,
    cache: &LayerCache,
) -> bool
where
    F: FnMut(&[u16]) -> bool,
{
    let info = build_spell_target_source_info_cached(state, player, object, x_val, &[], cache);
    has_legal_modal_target_assignment_cached(
        state,
        &spell_ab.targets,
        spell_ab.modal.as_ref(),
        &info,
        eval_ctx,
        cost_ok,
        cache,
    )
}

/// Stack-only upper bound on a player's currently achievable mana + life-for-Phyrexian capacity,
/// used to fast-reject obviously unaffordable spells in [`collect_priority_actions`] without running `plan_auto_tap`.
struct PlayerManaUpperBound {
    pool_total: u32,
    pool_snow: u32,
    avail_colors: ColorMask,
    max_additional_mana: u32,
    max_additional_snow: u32,
    unbounded_mana: bool,
    any_color_rule_mod: bool,
    cant_pay_life: bool,
    life_available: u32,
}

impl PlayerManaUpperBound {
    fn from_state(
        state: &GameState,
        player: PlayerId,
        mana_cands: &[ManaAbilityCandidate],
        cache: &LayerCache,
    ) -> Self {
        let mut pool_total = 0u32;
        let mut pool_snow = 0u32;
        let mut avail_colors = ColorMask::empty();
        let mut life_available = 0u32;

        if let Some(p) = state.player(player) {
            for (i, &amt) in p.mana_pool.iter().enumerate() {
                if amt > 0 {
                    pool_total = pool_total.saturating_add(amt);
                    avail_colors |= ColorMask::from_mana_color(index_to_color(i));
                }
            }
            pool_snow = p.snow_mana_pool().iter().sum();
            for b in p.restricted_mana() {
                let mut bsum = 0u32;
                for (i, &amt) in b.colors.iter().enumerate() {
                    if amt > 0 {
                        bsum = bsum.saturating_add(amt);
                        avail_colors |= ColorMask::from_mana_color(index_to_color(i));
                    }
                }
                pool_total = pool_total.saturating_add(bsum);
                if b.is_snow {
                    pool_snow = pool_snow.saturating_add(bsum);
                }
            }
            life_available = p.life.max(0) as u32;
        }

        let mut max_additional_mana = 0u32;
        let mut max_additional_snow = 0u32;
        let mut unbounded_mana = false;
        let mut single_use_caps: SmallVec<[(ObjectId, u32, u32); 32]> = SmallVec::new();

        for cand in mana_cands {
            let fixed_sum: u32 = cand.produces_amounts.iter().sum();
            let bundle_max: u32 = cand
                .bundle_options
                .iter()
                .map(|b| u32::try_from(b.len()).unwrap_or(u32::MAX))
                .max()
                .unwrap_or(0);
            let cand_gain = fixed_sum
                .saturating_add(cand.max_any_color_amount)
                .saturating_add(bundle_max);
            let cand_snow_gain = if cand.is_snow_source { cand_gain } else { 0 };
            avail_colors |= cand.produces_colors;
            if cand.max_any_color_amount > 0 && cand.produces_colors.is_empty() {
                avail_colors |= ColorMask::C;
            }
            if cand.requires_tap || cand.requires_untap || cand.has_sacrifice_self {
                if let Some((_, max_tot, max_sn)) = single_use_caps
                    .iter_mut()
                    .find(|(oid, _, _)| *oid == cand.object)
                {
                    *max_tot = (*max_tot).max(cand_gain);
                    *max_sn = (*max_sn).max(cand_snow_gain);
                } else {
                    single_use_caps.push((cand.object, cand_gain, cand_snow_gain));
                }
            } else if cand_gain > 0 {
                unbounded_mana = true;
            }
        }
        for (_, max_tot, max_sn) in single_use_caps {
            max_additional_mana = max_additional_mana.saturating_add(max_tot);
            max_additional_snow = max_additional_snow.saturating_add(max_sn);
        }

        let mut any_color_rule_mod = false;
        let mut cant_pay_life = false;
        if let Some(ep) = cache.player(player) {
            if ep.life_cant_change {
                cant_pay_life = true;
            }
            for rm in &ep.active_rule_mods {
                match &rm.rule {
                    RuleMod::SpendManaAsAnyColor { abilities_only, .. } if !*abilities_only => {
                        any_color_rule_mod = true;
                    }
                    RuleMod::CantPayLifeOrSacrificeForCosts {
                        cant_pay_life: true,
                        ..
                    } => {
                        cant_pay_life = true;
                    }
                    _ => {}
                }
            }
        }

        Self {
            pool_total,
            pool_snow,
            avail_colors,
            max_additional_mana,
            max_additional_snow,
            unbounded_mana,
            any_color_rule_mod,
            cant_pay_life,
            life_available,
        }
    }

    fn can_possibly_pay(
        &self,
        breakdown: &EffectiveCostBreakdown,
        spend_ctx: &ManaSpendContext,
    ) -> bool {
        if self.unbounded_mana
            || breakdown.allows_convoke
            || breakdown.allows_improvise
            || breakdown.allows_delve
            || breakdown.allows_emerge
            || breakdown.defiler_phyrexian_available.is_some()
            || breakdown.commander_tax_as_life_available.is_some()
            || breakdown.locked_mana.x_count > 0
            || breakdown
                .non_mana_costs
                .iter()
                .any(|c| matches!(c, CostDef::Choice { .. } | CostDef::Mana { .. }))
        {
            return true;
        }

        let any_color = self.any_color_rule_mod || spend_ctx.alt_cast_spend_any_color;
        let mut snow_pips_count = 0u32;
        let mut phy_pips_count = 0u32;
        let mut non_phy_min_mana = 0u32;
        let mut phy_missing_color_count = 0u32;

        for &pip in &breakdown.locked_mana.pips {
            match pip {
                ManaPip::Snow => {
                    snow_pips_count = snow_pips_count.saturating_add(1);
                    non_phy_min_mana = non_phy_min_mana.saturating_add(1);
                }
                ManaPip::Colored { color } => {
                    non_phy_min_mana = non_phy_min_mana.saturating_add(1);
                    if (!any_color || color == ManaColor::C)
                        && !self
                            .avail_colors
                            .contains(ColorMask::from_mana_color(color))
                    {
                        return false;
                    }
                }
                ManaPip::Hybrid { a, b } => {
                    non_phy_min_mana = non_phy_min_mana.saturating_add(1);
                    if !any_color
                        && !self.avail_colors.intersects(
                            ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b),
                        )
                    {
                        return false;
                    }
                }
                ManaPip::ColorlessHybrid { color } => {
                    non_phy_min_mana = non_phy_min_mana.saturating_add(1);
                    if !any_color
                        && !self
                            .avail_colors
                            .intersects(ColorMask::C | ColorMask::from_mana_color(color))
                    {
                        return false;
                    }
                }
                ManaPip::TwoBrid { color } => {
                    if any_color
                        || self
                            .avail_colors
                            .contains(ColorMask::from_mana_color(color))
                    {
                        non_phy_min_mana = non_phy_min_mana.saturating_add(1);
                    } else {
                        non_phy_min_mana = non_phy_min_mana.saturating_add(2);
                    }
                }
                ManaPip::Phyrexian { color } => {
                    phy_pips_count = phy_pips_count.saturating_add(1);
                    if (!any_color || color == ManaColor::C)
                        && !self
                            .avail_colors
                            .contains(ColorMask::from_mana_color(color))
                    {
                        phy_missing_color_count = phy_missing_color_count.saturating_add(1);
                    }
                }
                ManaPip::HybridPhyrexian { a, b } => {
                    phy_pips_count = phy_pips_count.saturating_add(1);
                    if !any_color
                        && !self.avail_colors.intersects(
                            ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b),
                        )
                    {
                        phy_missing_color_count = phy_missing_color_count.saturating_add(1);
                    }
                }
            }
        }

        let max_snow = self.pool_snow.saturating_add(self.max_additional_snow);
        if max_snow < snow_pips_count {
            return false;
        }

        let max_life_payments = if self.cant_pay_life {
            0
        } else {
            self.life_available / 2
        };
        if phy_missing_color_count > max_life_payments {
            return false;
        }

        let min_mana_needed = breakdown
            .locked_mana
            .generic
            .saturating_add(non_phy_min_mana)
            .saturating_add(phy_pips_count.saturating_sub(max_life_payments));
        let max_possible_mana = self.pool_total.saturating_add(self.max_additional_mana);
        max_possible_mana >= min_mana_needed
    }
}

/// Validates whether `player` can begin casting `object` using `option_id` right now.
pub fn can_cast_spell_option(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    option_id: u16,
) -> Option<CastOption> {
    can_cast_spell_option_cached(state, player, object, option_id, state.evaluate_board())
}

pub(crate) fn can_cast_spell_option_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    option_id: u16,
    cache: &LayerCache,
) -> Option<CastOption> {
    let p_state = state.player(player)?;
    if p_state.lost {
        return None;
    }
    let (options, reused_zone_perms) =
        enumerate_cast_options_with_perms_cached(state, player, object, cache);
    let opt = options.into_iter().find(|o| o.option_id == option_id)?;
    can_cast_spell_option_with_bound(state, player, object, opt, reused_zone_perms, None, cache)
}

#[cfg_attr(not(test), allow(dead_code))]
fn can_cast_spell_option_with(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    opt: CastOption,
    cache: &LayerCache,
) -> Option<CastOption> {
    can_cast_spell_option_with_bound(state, player, object, opt, None, None, cache)
}

fn can_cast_spell_option_with_bound(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    opt: CastOption,
    reused_zone_perms: Option<(bool, bool)>,
    mana_bound: Option<&PlayerManaUpperBound>,
    cache: &LayerCache,
) -> Option<CastOption> {
    let p_state = state.player(player)?;
    if p_state.lost {
        return None;
    }

    let obj = state.object(object)?;
    let eff_def = state
        .rare(object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    let card = state.db().get(eff_def)?;
    let face = card.faces.get(usize::from(opt.face_idx))?;

    let ep = cache.player(player);

    // Player rule-mod cast restrictions
    if let Some(ep) = ep {
        if ep.one_spell_per_turn && p_state.spells_cast_this_turn >= 1 {
            return None;
        }
        if ep.cast_only_from_hand && opt.from_zone != Zone::Hand {
            return None;
        }
        if ep.cast_only_during_own_turn && state.active_player() != player {
            return None;
        }
        if ep.cant_cast_during_combat && is_combat_phase(state.phase()) {
            return None;
        }
        let p_ctx = EvalContext::new(player).with_board(BoardView::Cached(cache));
        for rm in &ep.active_rule_mods {
            match &rm.rule {
                RuleMod::FilteredSpellCapPerTurn { filter, max } => {
                    if filter.is_none() && p_state.spells_cast_this_turn >= u32::from(*max) {
                        return None;
                    }
                }
                RuleMod::CantCastChosenName => {
                    if rm
                        .chosen_card_name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(&face.name))
                    {
                        return None;
                    }
                }
                RuleMod::CantCastSameNameAsLinkedExile => {
                    if rm
                        .linked_exile_names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case(&face.name))
                    {
                        return None;
                    }
                }
                RuleMod::CantCastMatchingSpells { filter, from_zones }
                    if (from_zones.is_empty()
                        || from_zones.contains(ZoneMask::from_zone(opt.from_zone)))
                        && filter
                            .as_deref()
                            .is_none_or(|f| matches_object_filter(state, object, f, &p_ctx)) =>
                {
                    return None;
                }
                _ => {}
            }
        }
    }

    // Opponents' OpponentsCantCastDuringYourTurn locks
    for other in state.players() {
        if other.id == player || other.lost {
            continue;
        }
        if state.active_player() == other.id
            && let Some(o_ep) = cache.player(other.id)
            && o_ep
                .active_rule_mods
                .iter()
                .any(|rm| matches!(rm.rule, RuleMod::OpponentsCantCastDuringYourTurn { .. }))
        {
            return None;
        }
    }

    let (zone_perm_spend_any_color, zone_perm_grant_flash) =
        if let Some((any_color, grant_flash)) = reused_zone_perms {
            (any_color, grant_flash)
        } else {
            let zone_perms = matching_zone_play_permissions_cached(
                state,
                player,
                object,
                opt.from_zone,
                false,
                cache,
            );
            let mut any_color_acc = false;
            let mut grant_flash_acc = false;
            for (_, any_color, grant_flash, _) in &zone_perms {
                if *any_color {
                    any_color_acc = true;
                }
                if *grant_flash {
                    grant_flash_acc = true;
                }
            }
            (any_color_acc, grant_flash_acc)
        };

    // Timing check
    let mut face_opts = face_opts_for_object(state, object).unwrap_or_default();
    if !matches!(
        opt.alt_kind,
        AltCastKind::MorphFaceDown | AltCastKind::DisguiseFaceDown
    ) {
        face_opts.is_face_down = false;
    }
    face_opts.face_index = opt.face_idx;
    face_opts.fused = opt.fused;
    face_opts.cleaved = opt.cleaved;
    face_opts.gift_promised = opt.gift_promised;
    face_opts.prototyped = opt.alt_kind == AltCastKind::Prototype;
    face_opts.bestowed = opt.alt_kind == AltCastKind::Bestow;
    let resolved = resolve_object_faces(state.db(), eff_def, &face_opts)?;
    let has_flash = spell_has_flash_cached(
        state,
        player,
        object,
        face,
        resolved.types,
        zone_perm_grant_flash,
        cache,
    );
    let sorcery_only =
        opt.alt_kind == AltCastKind::PlotCast || ep.is_some_and(|e| e.cast_only_sorcery_speed);
    if (sorcery_only || !has_flash)
        && (!is_main_phase(state.phase())
            || state.active_player() != player
            || !state.stack().is_empty())
    {
        return None;
    }

    // SpellAbility restrictions & conditions
    let spell_ab = face_spell_ability(face, opt.cleaved, opt.gift_promised);
    let eval_ctx = EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
    if let Some(sa) = spell_ab {
        for r in &sa.cast_restrictions {
            if !spell_cast_restriction_holds(state, player, *r) {
                return None;
            }
        }
        if let Some(cond) = sa.cast_condition.as_deref()
            && !evaluate_condition(state, cond, &eval_ctx)
        {
            return None;
        }
    }
    for ab in &face.abilities {
        if let AbilityDef::Static(st) = ab
            && let StaticBody::SelfCastRestriction {
                restrictions,
                cast_condition,
                ..
            } = &st.static_def.body
        {
            for r in restrictions {
                if !spell_cast_restriction_holds(state, player, *r) {
                    return None;
                }
            }
            if let Some(cond) = cast_condition.as_deref()
                && !evaluate_condition(state, cond, &eval_ctx)
            {
                return None;
            }
        }
    }

    let x_cant_be_zero = spell_ab.is_some_and(|s| s.x_cant_be_zero)
        || face.abilities.iter().any(|ab| {
            matches!(
                ab,
                AbilityDef::Static(st)
                    if matches!(
                        st.static_def.body,
                        StaticBody::SelfCastRestriction { x_cant_be_zero: true, .. }
                    )
            )
        });
    let base_breakdown = compute_effective_spell_cost_with_modes_cached(
        state,
        player,
        object,
        &opt,
        &[],
        None,
        &opt.optional_cost_indices,
        &[],
        cache,
    )
    .ok()?;
    if base_breakdown.base_mana.x_count > 0
        && base_breakdown.locked_mana.x_count == 0
        && x_cant_be_zero
    {
        return None;
    }
    let has_x = base_breakdown.locked_mana.x_count > 0
        || base_breakdown.non_mana_costs.iter().any(cost_uses_x);
    let min_x = if has_x {
        Some(if x_cant_be_zero { 1 } else { 0 })
    } else {
        None
    };

    let mut spend_ctx = build_mana_spend_context_with_any_color(
        state,
        player,
        object,
        &opt,
        zone_perm_spend_any_color,
    );
    spend_ctx.has_x_in_cost = has_x;

    if let Some(bound) = mana_bound
        && !bound.can_possibly_pay(&base_breakdown, &spend_ctx)
    {
        return None;
    }

    let spell_ok_at_x = |x_val: Option<u32>| -> bool {
        if opt.alt_kind == AltCastKind::Bestow {
            let info =
                build_spell_target_source_info_cached(state, player, object, x_val, &[], cache);
            if !has_legal_target_assignment_cached(state, &[bestow_target_spec()], &info, cache) {
                return false;
            }
        }
        let can_pay = if x_val.is_none() {
            spell_can_be_paid_with_x_cached(state, player, &base_breakdown, &spend_ctx, None, cache)
        } else {
            spell_can_be_paid_for_modes_cached(
                state,
                player,
                object,
                &opt,
                &spend_ctx,
                x_val,
                &[],
                cache,
            )
        };
        if !can_pay {
            return false;
        }
        if let Some(sa) = spell_ab {
            let mut tgt_eval_ctx = eval_ctx;
            tgt_eval_ctx.x_value = x_val;
            let mode_dependent_cost = sa.modal.as_ref().is_some_and(|m| {
                !m.per_mode_costs.is_empty() || has_keyword(face, state.db(), "escalate")
            });
            let targets_ok = if mode_dependent_cost {
                spell_has_legal_targets_cached(
                    state,
                    player,
                    object,
                    sa,
                    x_val,
                    &tgt_eval_ctx,
                    |modes| {
                        if modes.is_empty() && x_val.is_none() {
                            true
                        } else {
                            spell_can_be_paid_for_modes_cached(
                                state, player, object, &opt, &spend_ctx, x_val, modes, cache,
                            )
                        }
                    },
                    cache,
                )
            } else {
                spell_has_legal_targets_cached(
                    state,
                    player,
                    object,
                    sa,
                    x_val,
                    &tgt_eval_ctx,
                    |_| true,
                    cache,
                )
            };
            if !targets_ok {
                return false;
            }
        }
        true
    };

    if spell_ok_at_x(min_x) {
        return Some(opt);
    }
    if min_x == Some(0) {
        let max_scan = max_x_for_cost_list_with_ctx_cached(
            state,
            player,
            object,
            &base_breakdown.non_mana_costs,
            CostListSelfContext::for_spell_cast(opt.from_zone),
            cache,
        )
        .clamp(0, 16) as u32;
        for x in 1..=max_scan {
            if spell_ok_at_x(Some(x)) {
                return Some(opt);
            }
        }
    }

    None
}

/// Enumerates all active non-mana activated abilities on `object` using the shared
/// `ability_index` numbering (`0..primary_len`, Mutate `primary_len..innate_total_len`, Layer 6 `innate_total_len..`).
pub fn resolve_activated_abilities<'a>(
    state: &'a GameState,
    object: ObjectId,
) -> Vec<ResolvedActivatedAbility<'a>> {
    resolve_activated_abilities_cached(state, object, state.evaluate_board())
}

pub(crate) fn resolve_activated_abilities_cached<'a>(
    state: &'a GameState,
    object: ObjectId,
    cache: &'a LayerCache,
) -> Vec<ResolvedActivatedAbility<'a>> {
    let mut out = Vec::new();
    let Some(obj) = state.object(object) else {
        return out;
    };
    let Some(opts) = face_opts_for_object(state, object) else {
        return out;
    };
    let eval = cache.get(object);
    let extras = cache.extra(object);
    let ctrl = eval.map_or(obj.controller, |e| e.controller);
    let ctx = EvalContext::for_source(ctrl, object).with_board(BoardView::Cached(cache));

    let innate_removed = extras.is_some_and(|e| e.innate_abilities_removed)
        || eval.is_some_and(|e| e.lost_all_abilities);

    let mut innate_total_len: usize = 0;
    let mut primary_face_offsets: SmallVec<[(u8, usize); 2]> = SmallVec::new();
    let mut component_offsets: SmallVec<[(u8, usize); 4]> = SmallVec::new();

    if !opts.is_face_down
        && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        let base_id = opts.copiable_state.map_or(obj.card_def, |c| c.card_def);
        if let Some(card) = state.db().get(base_id) {
            for &fi in &view.active_face_indices {
                primary_face_offsets.push((fi, innate_total_len));
                if let Some(face) = card.faces.get(fi as usize) {
                    innate_total_len = innate_total_len.saturating_add(face.abilities.len());
                }
            }
        }
        if !opts.merged_components.is_empty() {
            for (idx_offset, comp) in opts.merged_components.iter().skip(1).enumerate() {
                if comp.is_token {
                    continue;
                }
                let comp_idx = u8::try_from(idx_offset + 1).unwrap_or(u8::MAX);
                component_offsets.push((comp_idx, innate_total_len));
                if let Some(mc_card) = state.db().get(comp.card_def)
                    && let Some(mc_face) = mc_card.faces.first()
                {
                    innate_total_len = innate_total_len.saturating_add(mc_face.abilities.len());
                }
            }
        } else if let Some(copiable) = opts.copiable_state
            && copiable.merged_components.len() > 1
        {
            for (idx_offset, &cid) in copiable.merged_components.iter().skip(1).enumerate() {
                let comp_idx = u8::try_from(idx_offset + 1).unwrap_or(u8::MAX);
                component_offsets.push((comp_idx, innate_total_len));
                if let Some(mc_card) = state.db().get(cid)
                    && let Some(mc_face) = mc_card.faces.first()
                {
                    innate_total_len = innate_total_len.saturating_add(mc_face.abilities.len());
                }
            }
        }

        if !innate_removed {
            let innate_abilities: Vec<ActiveAbilityWithCardRef<'a>> =
                active_abilities_with_card_for_object(state, object, &ctx);
            for ab_ref in innate_abilities {
                let base_offset = if ab_ref.component_index == 0 {
                    primary_face_offsets
                        .iter()
                        .find(|(fi, _)| *fi == ab_ref.face_index)
                        .map_or(0, |(_, off)| *off)
                } else {
                    component_offsets
                        .iter()
                        .find(|(ci, _)| *ci == ab_ref.component_index)
                        .map_or(0, |(_, off)| *off)
                };
                let shared_idx = base_offset.saturating_add(ab_ref.ability_index);
                let Ok(ability_index) = u16::try_from(shared_idx) else {
                    continue;
                };
                let ability = match ab_ref.ability {
                    AbilityDef::Activated(act) => {
                        if act.is_mana || act.timing == Timing::Mana {
                            continue;
                        }
                        Cow::Borrowed(act)
                    }
                    AbilityDef::Keyword(kw_ab) => {
                        let Some(synth) =
                            synthesize_keyword_activated_ability(state.db(), kw_ab, shared_idx)
                        else {
                            continue;
                        };
                        Cow::Owned(synth)
                    }
                    _ => continue,
                };
                out.push(ResolvedActivatedAbility {
                    ability_index,
                    ability,
                    source_card_def: ab_ref.card_def,
                    face_index: ab_ref.face_index,
                    mana_as_any_color: false,
                    grant_once_each_turn: false,
                });
            }
        }
    }

    if let Some(extras) = extras {
        for (gi, granted) in extras.granted_abilities.iter().enumerate() {
            let shared_idx = innate_total_len.saturating_add(gi);
            let Ok(ability_index) = u16::try_from(shared_idx) else {
                continue;
            };
            match granted {
                GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index: src_idx,
                    is_mana,
                    mana_as_any_color,
                    once_each_turn,
                    ..
                } => {
                    if *is_mana {
                        continue;
                    }
                    if let Some(card) = state.db().get(*card_def)
                        && let Some(face) = card.faces.get(usize::from(*face_index))
                        && let Some(ab) = face.abilities.get(*src_idx)
                    {
                        let ability = match ab {
                            AbilityDef::Activated(act) => {
                                if act.is_mana || act.timing == Timing::Mana {
                                    continue;
                                }
                                Cow::Borrowed(act)
                            }
                            AbilityDef::Keyword(kw_ab) => {
                                let Some(synth) = synthesize_keyword_activated_ability(
                                    state.db(),
                                    kw_ab,
                                    *src_idx,
                                ) else {
                                    continue;
                                };
                                Cow::Owned(synth)
                            }
                            _ => continue,
                        };
                        out.push(ResolvedActivatedAbility {
                            ability_index,
                            ability,
                            source_card_def: *card_def,
                            face_index: *face_index,
                            mana_as_any_color: *mana_as_any_color,
                            grant_once_each_turn: *once_each_turn,
                        });
                    }
                }
                GrantedAbilityEntry::Inline {
                    ability,
                    is_mana,
                    grant_card_def,
                    source_face_index,
                    ..
                } => {
                    if *is_mana {
                        continue;
                    }
                    let resolved = match ability.as_ref() {
                        AbilityDef::Activated(act) => {
                            if act.is_mana || act.timing == Timing::Mana {
                                continue;
                            }
                            Cow::Borrowed(act)
                        }
                        AbilityDef::Keyword(kw_ab) => {
                            let Some(synth) =
                                synthesize_keyword_activated_ability(state.db(), kw_ab, gi)
                            else {
                                continue;
                            };
                            Cow::Owned(synth)
                        }
                        _ => continue,
                    };
                    out.push(ResolvedActivatedAbility {
                        ability_index,
                        ability: resolved,
                        source_card_def: grant_card_def.unwrap_or(obj.card_def),
                        face_index: *source_face_index,
                        mana_as_any_color: false,
                        grant_once_each_turn: false,
                    });
                }
                GrantedAbilityEntry::IntrinsicBasicMana { .. }
                | GrantedAbilityEntry::DynamicWard { .. } => {}
            }
        }
    }

    out
}

fn permanent_cant_activate_non_mana_cached(
    state: &GameState,
    target: ObjectId,
    cache: &LayerCache,
) -> bool {
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::CANT_ACTIVATE_ABILITIES)
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
            let perm_ctrl = cache
                .get(perm_id)
                .map_or(perm_obj.controller, |e| e.controller);
            let ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            let abilities = active_abilities_with_card_for_object(state, perm_id, &ctx);
            for ab in abilities {
                if let AbilityDef::Static(st) = ab.ability
                    && let StaticBody::CantActivateAbilities { affected, .. } = &st.static_def.body
                {
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &ctx)
                    {
                        continue;
                    }
                    let matches = match affected {
                        Affected::SelfObj => perm_id == target,
                        Affected::AttachedTo => perm_obj.attached_to == Some(target),
                        Affected::Filter { filter } => {
                            matches_object_filter(state, target, filter, &ctx)
                        }
                    };
                    if matches {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn has_haste_or_activate_as_haste_cached(
    state: &GameState,
    object: ObjectId,
    cache: &LayerCache,
) -> bool {
    if let Some(eval) = cache.get(object)
        && eval.keywords.iter().any(|&kw| {
            state
                .db()
                .keyword_name(kw)
                .is_some_and(|n| n.eq_ignore_ascii_case("haste"))
        })
    {
        return true;
    }
    if !cache
        .static_presence
        .contains(StaticPresenceFlags::ACTIVATE_AS_THOUGH_HASTE)
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
            let ctrl = cache
                .get(perm_id)
                .map_or(perm_obj.controller, |e| e.controller);
            let ctx = EvalContext::for_source(ctrl, perm_id).with_board(BoardView::Cached(cache));
            for ab in active_abilities_with_card_for_object(state, perm_id, &ctx) {
                if let AbilityDef::Static(st) = ab.ability
                    && let StaticBody::ActivateAsThoughHaste { filter } = &st.static_def.body
                    && matches_object_filter(state, object, filter, &ctx)
                {
                    return true;
                }
            }
        }
    }
    false
}

pub fn can_activate_ability<'a>(
    state: &'a GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
) -> Option<ResolvedActivatedAbility<'a>> {
    can_activate_ability_cached(state, player, object, ability_index, state.evaluate_board())
}

pub(crate) fn can_activate_ability_cached<'a>(
    state: &'a GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    cache: &'a LayerCache,
) -> Option<ResolvedActivatedAbility<'a>> {
    let p_state = state.player(player)?;
    if p_state.lost {
        return None;
    }
    let obj = state.object(object)?;
    if obj.zone == Zone::Battlefield && obj.is_phased_out() {
        return None;
    }
    let resolved_list = resolve_activated_abilities_cached(state, object, cache);
    let res = resolved_list
        .into_iter()
        .find(|r| r.ability_index == ability_index)?;
    let act = res.ability.as_ref();

    let zones = if act.activation_zones.is_empty() {
        ZoneMask::BATTLEFIELD
    } else {
        act.activation_zones
    };
    if !zones.contains(ZoneMask::from_zone(obj.zone)) {
        return None;
    }

    let eff_ctrl = cache.get(object).map_or(obj.controller, |e| e.controller);
    let who = act.activator_who.unwrap_or(mtg_ir::ActivatorWho::You);
    match who {
        mtg_ir::ActivatorWho::You => {
            if eff_ctrl != player {
                return None;
            }
        }
        mtg_ir::ActivatorWho::Opponent => {
            if eff_ctrl == player {
                return None;
            }
        }
        mtg_ir::ActivatorWho::Any => {}
    }

    if cache
        .player(player)
        .is_some_and(|ep| ep.activate_only_during_own_turn)
        && state.active_player() != player
    {
        return None;
    }

    if obj.zone == Zone::Battlefield
        && permanent_cant_activate_non_mana_cached(state, object, cache)
    {
        return None;
    }

    let is_loyalty = act
        .cost
        .iter()
        .any(|c| matches!(c, CostDef::Loyalty { .. }));
    if (is_loyalty || act.timing == Timing::Sorcery)
        && (!is_main_phase(state.phase())
            || state.active_player() != player
            || !state.stack().is_empty())
    {
        return None;
    }
    if is_loyalty && state.rare(object).is_some_and(|r| r.used_loyalty_this_turn) {
        return None;
    }

    if res.grant_once_each_turn
        && !activation_restriction_holds(
            state,
            player,
            object,
            ability_index,
            ActivationRestriction::OnceEachTurn,
        )
    {
        return None;
    }
    for r in &act.activation_restrictions {
        if !activation_restriction_holds(state, player, object, ability_index, *r) {
            return None;
        }
    }

    let ctx = EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
    if let Some(cond) = act.activation_condition.as_deref()
        && !evaluate_condition(state, cond, &ctx)
    {
        return None;
    }

    // Tap/Untap self + summoning sickness
    let is_creature = cache
        .get(object)
        .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE));
    let sick = is_creature
        && obj.flags.contains(ObjectFlags::SUMMONING_SICK)
        && !has_haste_or_activate_as_haste_cached(state, object, cache);
    for c in &act.cost {
        match c {
            CostDef::TapSelf => {
                if obj.zone != Zone::Battlefield || obj.flags.contains(ObjectFlags::TAPPED) || sick
                {
                    return None;
                }
            }
            CostDef::UntapSelf { via_symbol } => {
                if obj.zone != Zone::Battlefield
                    || !obj.flags.contains(ObjectFlags::TAPPED)
                    || (*via_symbol && sick)
                {
                    return None;
                }
            }
            CostDef::Loyalty { delta, is_x, sign } => {
                let current_loyalty = cache.get(object).and_then(|e| e.loyalty).unwrap_or(0);
                if matches!(sign, Some(LoyaltySign::Minus)) && !*is_x && *delta > current_loyalty {
                    return None;
                }
                if *delta < 0 && (-*delta) > current_loyalty {
                    return None;
                }
            }
            _ => {}
        }
    }

    let base_breakdown = compute_effective_ability_cost_cached(
        state,
        player,
        object,
        ability_index,
        None,
        &[],
        cache,
    )
    .ok()?;
    let has_x = base_breakdown.locked_mana.x_count > 0
        || base_breakdown.non_mana_costs.iter().any(cost_uses_x)
        || act.cost.iter().any(cost_uses_x);
    if base_breakdown.base_mana.x_count > 0
        && base_breakdown.locked_mana.x_count == 0
        && act.x_cant_be_zero
    {
        return None;
    }
    let min_x = if has_x {
        Some(if act.x_cant_be_zero { 1 } else { 0 })
    } else {
        None
    };

    let res_face_idx = res.face_index;
    let res_mana_as_any_color = res.mana_as_any_color;
    let is_commander = crate::commander::is_commander_object(state, object);

    let ability_ok_at_x = |x_val: Option<u32>| -> bool {
        let tgt_info = build_ability_target_source_info_cached(
            state,
            player,
            object,
            None,
            x_val,
            &[],
            &[],
            cache,
        );
        let mut tgt_eval_ctx =
            EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
        tgt_eval_ctx.x_value = x_val;
        if !has_legal_modal_target_assignment_cached(
            state,
            &act.targets,
            act.modal.as_ref(),
            &tgt_info,
            &tgt_eval_ctx,
            |_| true,
            cache,
        ) {
            return false;
        }
        let Ok(breakdown) = compute_effective_ability_cost_cached(
            state,
            player,
            object,
            ability_index,
            x_val,
            &[],
            cache,
        ) else {
            return false;
        };
        let spend_ctx = ManaSpendContext {
            player,
            kind: ManaSpendContextKind::ActivatedAbility,
            source_object: Some(object),
            face_idx: res_face_idx,
            from_zone: obj.zone,
            is_commander,
            has_x_in_cost: has_x,
            is_fused: false,
            is_prototyped: false,
            is_bestowed: false,
            is_face_down: false,
            alt_cast_spend_any_color: false,
            mana_as_any_color_ability: res_mana_as_any_color,
            x_spend_colors: breakdown.x_spend_colors,
            x_colored_only: breakdown.x_colored_only,
            x_max_one_per_color: breakdown.x_max_one_per_color,
            source_restriction: breakdown.source_restriction,
        };
        can_pay_costs_and_mana_cached(
            state,
            player,
            object,
            &breakdown.locked_mana,
            &breakdown.non_mana_costs,
            &spend_ctx,
            CostAidCaps::default(),
            x_val,
            cache,
        )
    };

    if ability_ok_at_x(min_x) {
        return Some(res);
    }
    if min_x == Some(0) {
        let max_scan = max_x_for_cost_list_cached(
            state,
            player,
            object,
            &base_breakdown.non_mana_costs,
            cache,
        )
        .clamp(0, 16) as u32;
        for x in 1..=max_scan {
            if ability_ok_at_x(Some(x)) {
                return Some(res);
            }
        }
    }

    None
}

/// Checks whether `player` can play `object` (face `face_index`) as a land from `from_zone`.
pub fn can_play_land(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    from_zone: Zone,
    face_index: u8,
) -> bool {
    can_play_land_cached(
        state,
        player,
        object,
        from_zone,
        face_index,
        state.evaluate_board(),
    )
}

pub(crate) fn can_play_land_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    from_zone: Zone,
    face_index: u8,
    cache: &LayerCache,
) -> bool {
    let Some(p) = state.player(player) else {
        return false;
    };
    if p.lost {
        return false;
    }
    if state.active_player() != player || !is_main_phase(state.phase()) || !state.stack().is_empty()
    {
        return false;
    }
    let ep = cache.player(player);
    if ep.is_some_and(|e| e.cant_play_lands) {
        return false;
    }
    let extra_lands = ep.map_or(0, |e| e.extra_land_plays);
    let max_lands = u16::from(p.max_lands_per_turn).saturating_add(extra_lands);
    if u16::from(p.lands_played_this_turn) >= max_lands {
        return false;
    }

    let Some(obj) = state.object(object) else {
        return false;
    };
    if obj.zone != from_zone {
        return false;
    }

    let zone_ok = match from_zone {
        Zone::Hand => obj.owner == player,
        Zone::Exile => {
            let pe_ok = state
                .rare(object)
                .and_then(|r| r.playable_from_exile_by.as_ref())
                .is_some_and(|pe| {
                    pe.player == player && pe.expires_turn.is_none_or(|t| t >= state.turn_number())
                });
            pe_ok
                || !matching_zone_play_permissions_cached(
                    state,
                    player,
                    object,
                    Zone::Exile,
                    true,
                    cache,
                )
                .is_empty()
        }
        Zone::Graveyard => !matching_zone_play_permissions_cached(
            state,
            player,
            object,
            Zone::Graveyard,
            true,
            cache,
        )
        .is_empty(),
        Zone::Library => {
            state
                .player(obj.owner)
                .and_then(|op| op.library.last().copied())
                == Some(object)
                && !matching_zone_play_permissions_cached(
                    state,
                    player,
                    object,
                    Zone::Library,
                    true,
                    cache,
                )
                .is_empty()
        }
        _ => false,
    };
    if !zone_ok {
        return false;
    }

    let eff_def = state
        .rare(object)
        .and_then(|r| r.copiable.as_deref())
        .map_or(obj.card_def, |c| c.card_def);
    playable_land_faces(state.db(), eff_def).contains(&face_index)
}

pub fn legal_special_actions(state: &GameState, player: PlayerId) -> Vec<SpecialActionKind> {
    legal_special_actions_cached(state, player, state.evaluate_board())
}

pub(crate) fn legal_special_actions_cached(
    state: &GameState,
    player: PlayerId,
    cache: &LayerCache,
) -> Vec<SpecialActionKind> {
    let mut out = Vec::new();
    let Some(p) = state.player(player) else {
        return out;
    };
    if p.lost {
        return out;
    }

    // 1. TurnFaceUp (Morph/Disguise/Manifest/Cloak, any time player has priority)
    for &perm_id in &p.battlefield {
        let Some(obj) = state.object(perm_id) else {
            continue;
        };
        if obj.is_phased_out() {
            continue;
        }
        if crate::faces::can_turn_face_up(state, perm_id)
            && let Some(fd) = state.rare(perm_id).and_then(|r| r.face_down.as_deref())
        {
            let mana_ok = if let Some(cost) = &fd.turn_up_mana_cost {
                let ctx = ManaSpendContext {
                    player,
                    kind: ManaSpendContextKind::SpecialAction,
                    source_object: Some(perm_id),
                    face_idx: obj.active_face_index(),
                    from_zone: Zone::Battlefield,
                    is_commander: obj.is_commander(),
                    has_x_in_cost: cost.x_count > 0,
                    is_fused: false,
                    is_prototyped: false,
                    is_bestowed: false,
                    is_face_down: true,
                    alt_cast_spend_any_color: false,
                    mana_as_any_color_ability: false,
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                    source_restriction: None,
                };
                can_pay_mana_from_pool_cached(state, player, cost, &ctx, cache)
                    || plan_auto_tap_cached(
                        state,
                        player,
                        cost,
                        &ctx,
                        AutoTapPolicy::default(),
                        cache,
                    )
                    .can_pay
            } else {
                true
            };
            let non_mana_ok = can_pay_non_mana_costs_cached(
                state,
                player,
                perm_id,
                &fd.turn_up_cost_defs,
                None,
                cache,
            );
            if mana_ok && non_mana_ok {
                out.push(SpecialActionKind::TurnFaceUp { object: perm_id });
            }
        }
    }

    // 2. UnlockRoomDoor (CR 709.5: sorcery speed)
    if state.active_player() == player && is_main_phase(state.phase()) && state.stack().is_empty() {
        for &perm_id in &p.battlefield {
            let Some(obj) = state.object(perm_id) else {
                continue;
            };
            if obj.is_phased_out() || obj.is_face_down() {
                continue;
            }
            let rare = state.rare(perm_id);
            let eff_def = rare
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            if !is_room_card(state.db(), eff_def) {
                continue;
            }
            let Some(card) = state.db().get(eff_def) else {
                continue;
            };
            for (fi, face) in card.faces.iter().enumerate() {
                let fi_u8 = fi as u8;
                if rare.is_some_and(|r| r.unlocked_faces.contains(&fi_u8)) {
                    continue;
                }
                let cost = &face.characteristics.mana_cost;
                let ctx = ManaSpendContext {
                    player,
                    kind: ManaSpendContextKind::SpecialAction,
                    source_object: Some(perm_id),
                    face_idx: fi_u8,
                    from_zone: Zone::Battlefield,
                    is_commander: false,
                    has_x_in_cost: cost.x_count > 0,
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
                };
                if cost.is_empty()
                    || can_pay_mana_from_pool_cached(state, player, cost, &ctx, cache)
                    || plan_auto_tap_cached(
                        state,
                        player,
                        cost,
                        &ctx,
                        AutoTapPolicy::default(),
                        cache,
                    )
                    .can_pay
                {
                    out.push(SpecialActionKind::UnlockRoomDoor {
                        object: perm_id,
                        face_index: fi_u8,
                    });
                }
            }
        }
    }

    // 3. Foretell (during your turn, CR 702.143a) & Plot (sorcery speed, CR 702.170a)
    if state.active_player() == player {
        let foretell_cost = ManaCost::parse("{2}");
        let is_sorcery = is_main_phase(state.phase()) && state.stack().is_empty();
        for &hand_id in &p.hand {
            let Some(obj) = state.object(hand_id) else {
                continue;
            };
            let Some(card) = state.db().get(obj.card_def) else {
                continue;
            };
            let Some(f0) = card.faces.first() else {
                continue;
            };
            if hand_foretell_cost_cached(state, player, hand_id, cache).is_some() {
                let ctx = ManaSpendContext {
                    player,
                    kind: ManaSpendContextKind::ForetellAction,
                    source_object: Some(hand_id),
                    face_idx: 0,
                    from_zone: Zone::Hand,
                    is_commander: obj.is_commander(),
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
                };
                if can_pay_mana_from_pool_cached(state, player, &foretell_cost, &ctx, cache)
                    || plan_auto_tap_cached(
                        state,
                        player,
                        &foretell_cost,
                        &ctx,
                        AutoTapPolicy::default(),
                        cache,
                    )
                    .can_pay
                {
                    out.push(SpecialActionKind::Foretell { object: hand_id });
                }
            }
            if is_sorcery && let Some(pk) = find_keyword(f0, state.db(), "plot") {
                let plot_cost = pk.cost.clone().unwrap_or_default();
                let ctx = ManaSpendContext {
                    player,
                    kind: ManaSpendContextKind::SpecialAction,
                    source_object: Some(hand_id),
                    face_idx: 0,
                    from_zone: Zone::Hand,
                    is_commander: obj.is_commander(),
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
                };
                if (plot_cost.is_empty()
                    || can_pay_mana_from_pool_cached(state, player, &plot_cost, &ctx, cache)
                    || plan_auto_tap_cached(
                        state,
                        player,
                        &plot_cost,
                        &ctx,
                        AutoTapPolicy::default(),
                        cache,
                    )
                    .can_pay)
                    && can_pay_non_mana_costs_cached(
                        state,
                        player,
                        hand_id,
                        &pk.cost_defs,
                        None,
                        cache,
                    )
                {
                    out.push(SpecialActionKind::Plot { object: hand_id });
                }
            }
        }
    }

    out
}

fn might_have_non_mana_activated_ability(
    state: &GameState,
    oid: ObjectId,
    cache: &LayerCache,
) -> bool {
    let Some(obj) = state.object(oid) else {
        return false;
    };
    if obj.zone == Zone::Battlefield && obj.is_phased_out() {
        return false;
    }
    let zm = ZoneMask::from_zone(obj.zone);
    let extras = cache.extra(oid);
    if extras.is_some_and(|e| {
        e.granted_abilities.iter().any(|g| match g {
            GrantedAbilityEntry::CardAbility { is_mana, .. } => !*is_mana,
            GrantedAbilityEntry::Inline {
                is_mana, ability, ..
            } => {
                !*is_mana
                    && match ability.as_ref() {
                        AbilityDef::Activated(act) => !act.is_mana && act.timing != Timing::Mana,
                        AbilityDef::Keyword(kw_ab) => keyword_activated_zones(state.db(), kw_ab)
                            .is_some_and(|az| az.intersects(zm)),
                        _ => false,
                    }
            }
            GrantedAbilityEntry::IntrinsicBasicMana { .. }
            | GrantedAbilityEntry::DynamicWard { .. } => false,
        })
    }) {
        return true;
    }
    let innate_removed = extras.is_some_and(|e| e.innate_abilities_removed)
        || cache.get(oid).is_some_and(|e| e.lost_all_abilities);
    if innate_removed || obj.is_face_down() {
        return false;
    }
    if state
        .rare(oid)
        .is_some_and(|r| r.copiable.is_some() || !r.merged_components.is_empty())
    {
        return true;
    }
    state.db().get(obj.card_def).is_some_and(|card| {
        card.faces.iter().any(|f| {
            f.abilities.iter().any(|ab| match ab {
                AbilityDef::Activated(act) if !act.is_mana && act.timing != Timing::Mana => {
                    let act_zones = if act.activation_zones.is_empty() {
                        ZoneMask::BATTLEFIELD
                    } else {
                        act.activation_zones
                    };
                    act_zones.intersects(zm)
                }
                AbilityDef::Keyword(kw_ab) => {
                    keyword_activated_zones(state.db(), kw_ab).is_some_and(|az| az.intersects(zm))
                }
                _ => false,
            })
        })
    })
}

/// Enumerates all legal [`Action`]s for `player` when `DecisionState::Priority` is active.
pub fn collect_priority_actions(state: &GameState, player: PlayerId) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(p) = state.player(player) else {
        return actions;
    };
    if p.lost || state.priority_player() != player {
        return actions;
    }

    let cache = state.evaluate_board();
    actions.push(Action::PassPriority);

    // 1. PlayLand from Hand, Graveyard, Exile, Library top
    let mut land_candidates: Vec<(ObjectId, Zone)> =
        p.hand.iter().map(|&id| (id, Zone::Hand)).collect();
    for &id in &p.graveyard {
        land_candidates.push((id, Zone::Graveyard));
    }
    for &id in &p.exile {
        land_candidates.push((id, Zone::Exile));
    }
    if let Some(&top) = p.library.last() {
        land_candidates.push((top, Zone::Library));
    }
    for (oid, zone) in land_candidates {
        if let Some(obj) = state.object(oid) {
            let eff_def = state
                .rare(oid)
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            for fi in playable_land_faces(state.db(), eff_def) {
                if can_play_land_cached(state, player, oid, zone, fi, cache) {
                    actions.push(Action::PlayLand {
                        object: oid,
                        from_zone: zone,
                        face_index: fi,
                    });
                }
            }
        }
    }

    // 2. Mana abilities (preferred_color: None for SessionDriver::step compatibility)
    let mana_cands = enumerate_mana_abilities_cached(state, player, cache);
    for cand in &mana_cands {
        if !can_activate_mana_candidate_now_cached(state, player, cand, cache) {
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

    // 3. Castable spells across Hand, Command, Exile, Graveyard, Library top
    let mana_bound = PlayerManaUpperBound::from_state(state, player, &mana_cands, cache);
    let mut cast_candidates: Vec<(ObjectId, Zone)> =
        p.hand.iter().map(|&id| (id, Zone::Hand)).collect();
    for &id in &p.command {
        cast_candidates.push((id, Zone::Command));
    }
    for &id in &p.exile {
        cast_candidates.push((id, Zone::Exile));
    }
    for &id in &p.graveyard {
        cast_candidates.push((id, Zone::Graveyard));
    }
    if let Some(&top) = p.library.last() {
        cast_candidates.push((top, Zone::Library));
    }
    for (oid, from_zone) in cast_candidates {
        let (opts, reused_perms) =
            enumerate_cast_options_with_perms_cached(state, player, oid, cache);
        for opt in opts {
            if let Some(valid_opt) = can_cast_spell_option_with_bound(
                state,
                player,
                oid,
                opt,
                reused_perms,
                Some(&mana_bound),
                cache,
            ) {
                actions.push(Action::BeginCast {
                    object: oid,
                    option_id: valid_opt.option_id,
                    from_zone,
                });
            }
        }
    }

    // 4. Activated non-mana abilities on Battlefield, Graveyard, Hand
    for &oid in p
        .battlefield
        .iter()
        .chain(p.graveyard.iter())
        .chain(p.hand.iter())
    {
        if !might_have_non_mana_activated_ability(state, oid, cache) {
            continue;
        }
        for res in resolve_activated_abilities_cached(state, oid, cache) {
            if can_activate_ability_cached(state, player, oid, res.ability_index, cache).is_some() {
                actions.push(Action::BeginActivate {
                    object: oid,
                    ability_index: res.ability_index,
                });
            }
        }
    }

    // 5. Special actions
    for kind in legal_special_actions_cached(state, player, cache) {
        actions.push(Action::SpecialAction { kind });
    }

    actions.push(Action::Concede);
    actions
}

pub fn _unused_target_ref_marker(_t: TargetRef, _s: &TargetSpec, _m: ManaColor) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use mtg_ir::{
        CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, ColorMask, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility, KeywordDef, KeywordId, ManaCost,
        PrototypeSpec, SpellAbility, SupertypeMask,
    };
    use std::sync::Arc;

    #[test]
    fn enumerate_cast_options_adventure_exile_split_prototype_kicker() {
        let mut tables = InternTables::default();
        tables.keywords.push("kicker".into());
        let kw_kicker = KeywordId(0);
        tables.keywords.push("prototype".into());
        let kw_proto = KeywordId(1);

        let adventure_card = CardDef {
            oracle_id: "l_adv".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Knight // Synth Quest".into(),
            layout: CardLayout::Adventure,
            faces: vec![
                FaceDef {
                    name: "Synth Knight".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{1}{W}"),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::W,
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
                    name: "Synth Quest".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{W}"),
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
                },
            ],
            fully_supported: true,
        };

        let proto_card = CardDef {
            oracle_id: "l_proto".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Goliath".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Goliath".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{6}"),
                    types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_proto,
                        cost: None,
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
                        prototype_spec: Some(PrototypeSpec {
                            mana_cost: ManaCost::parse("{1}{G}"),
                            power: 2,
                            toughness: 2,
                            colors: ColorMask::G,
                        }),
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
                })],
                has_read_ahead: false,
                prototype_spec: Some(PrototypeSpec {
                    mana_cost: ManaCost::parse("{1}{G}"),
                    power: 2,
                    toughness: 2,
                    colors: ColorMask::G,
                }),
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let kicker_card = CardDef {
            oracle_id: "l_kick".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bolt".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bolt".into(),
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
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_kicker,
                        cost: None,
                        param: None,
                        n: None,
                        cost_defs: vec![],
                        kicker_costs: vec![mtg_ir::KickerCostOption {
                            cost: ManaCost::parse("{1}"),
                            cost_defs: vec![],
                            tags: vec!["kicked".into()],
                        }],
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
                cards: vec![adventure_card, proto_card, kicker_card],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 5).unwrap();

        let p0_hand = state.players()[0].hand.clone();
        let adv_id = p0_hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let adv_opts = enumerate_cast_options(&state, PlayerId(0), adv_id);
        assert_eq!(adv_opts.len(), 2);
        assert_eq!(adv_opts[0].option_id, 0);
        assert_eq!(adv_opts[0].alt_kind, AltCastKind::Normal);
        assert_eq!(adv_opts[1].option_id, 1);
        assert_eq!(adv_opts[1].alt_kind, AltCastKind::AdventureHalf);

        if let Some(proto_id) = p0_hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
        {
            let proto_opts = enumerate_cast_options(&state, PlayerId(0), proto_id);
            assert_eq!(proto_opts[0].option_id, 0);
            assert_eq!(proto_opts[1].option_id, 1);
            assert_eq!(proto_opts[1].alt_kind, AltCastKind::Prototype);
        }

        if let Some(kick_id) = p0_hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
        {
            let kick_opts = enumerate_cast_options(&state, PlayerId(0), kick_id);
            assert_eq!(kick_opts[0].option_id, 0);
            assert!(!kick_opts[0].is_kicked);
            assert_eq!(kick_opts[1].option_id, 1);
            assert!(kick_opts[1].is_kicked);
        }

        // t2_f06_05: Exiled adventure WITHOUT on_an_adventure must NOT be castable
        let exiled = match crate::zones::move_object(
            &mut state,
            adv_id,
            Zone::Exile,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            _ => unreachable!(),
        };
        assert!(enumerate_cast_options(&state, PlayerId(0), exiled).is_empty());

        // With on_an_adventure = true, option_id 0 is the creature half!
        state.rare_or_default(exiled).unwrap().on_an_adventure = true;
        let on_adv_opts = enumerate_cast_options(&state, PlayerId(0), exiled);
        assert_eq!(on_adv_opts.len(), 1);
        assert_eq!(on_adv_opts[0].option_id, 0);
        assert_eq!(on_adv_opts[0].face_idx, 0);
    }

    #[test]
    fn resolve_activated_abilities_matches_mana_raw_offsets_with_grant_condition_and_layer6() {
        use mtg_ir::{ActivatedAbility, Condition, Layer, LayerMod, StaticAbility};

        let mut tables = InternTables::default();
        tables.keywords.push("vigilance".into());

        // Card 0: creature with [0] inactive grant_condition non-mana ability, [1] active non-mana ability
        let gated_card = CardDef {
            oracle_id: "l_gated".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Striated".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Striated".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![
                    AbilityDef::Activated(ActivatedAbility {
                        id: "gated_off".into(),
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
                        activation_zones: ZoneMask::BATTLEFIELD,
                        cost_keyword: None,
                        intrinsic: false,
                        text: None,
                        grant_condition: Some(Box::new(Condition::NotYourTurn)),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        span: Default::default(),
                    }),
                    AbilityDef::Activated(ActivatedAbility {
                        id: "active_one".into(),
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
                        activation_zones: ZoneMask::BATTLEFIELD,
                        cost_keyword: None,
                        intrinsic: false,
                        text: None,
                        grant_condition: None,
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
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

        // Card 1: enchantment granting a non-mana activated ability to creatures
        let granter_card = CardDef {
            oracle_id: "l_granter".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Granter".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Granter".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{W}"),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(mtg_ir::ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(AbilityDef::Activated(ActivatedAbility {
                                    id: "granted_two".into(),
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
                                    activation_zones: ZoneMask::BATTLEFIELD,
                                    cost_keyword: None,
                                    intrinsic: false,
                                    text: None,
                                    grant_condition: None,
                                    x_spend_colors: ColorMask::empty(),
                                    x_colored_only: false,
                                    x_max_one_per_color: false,
                                    span: Default::default(),
                                })),
                            },
                            active_in_zone: None,
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
                cards: vec![gated_card, granter_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: [CardDefId(0), CardDefId(1)].repeat(5),
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap();

        let creature_hand = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let creature_bf = match crate::zones::move_object(
            &mut state,
            creature_hand,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            _ => unreachable!(),
        };

        let granter_hand = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let _granter_bf = crate::zones::move_object(
            &mut state,
            granter_hand,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let leg = resolve_activated_abilities(&state, creature_bf);
        let leg_indices: Vec<u16> = leg.iter().map(|r| r.ability_index).collect();
        assert_eq!(leg_indices, vec![1, 2]);

        let mana_ctx = EvalContext::for_source(PlayerId(0), creature_bf);
        let mana_ab =
            crate::mana::resolve_object_activated_abilities(&state, creature_bf, &mana_ctx);
        let mana_indices: Vec<u16> = mana_ab.iter().map(|r| r.ability_index).collect();
        assert_eq!(mana_indices, leg_indices);

        assert!(can_activate_ability(&state, PlayerId(0), creature_bf, 1).is_some());
        assert!(can_activate_ability(&state, PlayerId(0), creature_bf, 2).is_some());
        assert!(can_activate_ability(&state, PlayerId(0), creature_bf, 0).is_none());
    }

    #[test]
    fn grant_hand_foretell_and_bestow_target_spec_legality() {
        use mtg_ir::StaticAbility;

        let mut tables = InternTables::default();
        tables.keywords.push("bestow".into());
        let kw_bestow = KeywordId(0);

        // Card 0: bestow creature {2}{W} with bestow {3}{W}
        let bestow_card = CardDef {
            oracle_id: "l_bestow".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Nymph".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Nymph".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}{W}"),
                    types: CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
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
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_bestow,
                        cost: Some(ManaCost::parse("{3}{W}")),
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
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 1: permanent granting GrantHandForetell { reduce_generic: 2 }
        let foretell_granter = CardDef {
            oracle_id: "l_foretell_grant".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Devourer".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Devourer".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{3}{B}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 2,
                        toughness: 6,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        body: StaticBody::GrantHandForetell {
                            filter: Box::new(mtg_ir::ObjectFilter::default()),
                            reduce_generic: 2,
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
                cards: vec![bestow_card, foretell_granter],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: [CardDefId(0), CardDefId(1)].repeat(5),
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 13).unwrap();

        let bestow_hand = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let bestow_opt = enumerate_cast_options(&state, PlayerId(0), bestow_hand)
            .into_iter()
            .find(|o| o.alt_kind == AltCastKind::Bestow)
            .unwrap();
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [2, 0, 0, 0, 0, 5];

        // No creature on battlefield -> Bestow option cannot be cast
        assert!(
            can_cast_spell_option(&state, PlayerId(0), bestow_hand, bestow_opt.option_id).is_none()
        );
        // No GrantHandForetell on battlefield -> hand_foretell_cost is None
        assert!(hand_foretell_cost(&state, PlayerId(0), bestow_hand).is_none());

        // Move foretell_granter (a creature) onto battlefield
        let granter_hand = state.players()[0]
            .hand
            .iter()
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let _granter_bf = crate::zones::move_object(
            &mut state,
            granter_hand,
            Zone::Battlefield,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap();

        // Now Bestow has a legal creature target -> castable
        assert!(
            can_cast_spell_option(&state, PlayerId(0), bestow_hand, bestow_opt.option_id).is_some()
        );
        // And GrantHandForetell grants Foretell with cost {2}{W} reduced by {2} = {W}
        let ft_cost = hand_foretell_cost(&state, PlayerId(0), bestow_hand).unwrap();
        assert_eq!(ft_cost.generic, 0);
        assert_eq!(ft_cost.pips, ManaCost::parse("{W}").pips);
        assert!(legal_special_actions(&state, PlayerId(0)).contains(
            &SpecialActionKind::Foretell {
                object: bestow_hand,
            }
        ));
    }

    #[test]
    fn synth_filter_land_tap_mana_omitted_without_extra_mana() {
        let make_mana_land =
            |name: &str,
             extra_costs: Vec<CostDef>,
             prod: mtg_ir::ManaProduction,
             restriction_spec: Option<Box<mtg_ir::ManaSpendingRestriction>>| {
                let mut cost = vec![CostDef::TapSelf];
                cost.extend(extra_costs);
                CardDef {
                    oracle_id: format!("o_{name}"),
                    compiler_version: 46,
                    text_hash: "0".into(),
                    name: name.into(),
                    layout: CardLayout::Normal,
                    faces: vec![FaceDef {
                        name: name.into(),
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
                        abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                            id: "mana".into(),
                            cost,
                            targets: vec![],
                            effect: vec![mtg_ir::EffectStep {
                                subject: mtg_ir::Selector::SelfObj,
                                op: mtg_ir::EffectOp::AddMana {
                                    production: prod,
                                    restriction: None,
                                    restriction_spec,
                                    retain_until_eot: false,
                                    delayed_rider: None,
                                },
                                actor: None,
                                amount: None,
                                optional: None,
                                gate: None,
                                bind_as: None,
                                do_this_once_per_turn: false,
                                reflexive_trigger: None,
                                span: Default::default(),
                            }],
                            modal: None,
                            timing: Timing::Mana,
                            is_mana: true,
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
                        has_read_ahead: false,
                        prototype_spec: None,
                        cleaved_spell_spec: None,
                        gift_promised_spell_spec: None,
                    }],
                    fully_supported: true,
                }
            };

        // Card 0: Synth FilterLand ({1}, {T}: Add {W}{U})
        let filter_land = make_mana_land(
            "Synth FilterLand",
            vec![CostDef::Mana {
                cost: ManaCost::parse("{1}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            mtg_ir::ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::W, ManaColor::U]),
            },
            None,
        );
        // Card 1: Synth Island ({T}: Add {U})
        let island = make_mana_land(
            "Synth Island",
            vec![],
            mtg_ir::ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::U]),
            },
            None,
        );
        // Card 2: Synth CreatureOnlyLand ({T}: Add {G} restricted to Creature spells)
        let creature_land = make_mana_land(
            "Synth CreatureOnlyLand",
            vec![],
            mtg_ir::ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::G]),
            },
            Some(Box::new(mtg_ir::ManaSpendingRestriction {
                allow_spells: Some(mtg_ir::AllowSpellsRestriction {
                    filter: Some(mtg_ir::PlainManaFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        );
        // Card 3: Synth Spell ({W}{U} Instant)
        let spell = CardDef {
            oracle_id: "o_synth_wu".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Spell".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{W}{U}"),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W | ColorMask::U,
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
                tables: InternTables::default(),
                cards: vec![filter_land, island, creature_land, spell],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 801).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);

        let put_bf = |s: &mut GameState, def: CardDefId| {
            let ts = s.allocate_timestamp();
            let id = s.insert_object(
                crate::state::GameObject::new(
                    def,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            s.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };

        let filter_id = put_bf(&mut state, CardDefId(0));
        let ts_spell = state.allocate_timestamp();
        let spell_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(3),
                PlayerId(0),
                Zone::Hand,
                ts_spell,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(spell_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // Only Synth FilterLand + empty pool -> TapMana omitted & spell uncastable
        let acts0 = collect_priority_actions(&state, PlayerId(0));
        assert!(!acts0.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == filter_id
        )));
        assert!(can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_none());

        // Add Synth CreatureOnlyLand and tap it -> pool has Creature-only {G}, still cannot pay filter land {1}
        let creature_land_id = put_bf(&mut state, CardDefId(2));
        crate::mana::activate_mana_ability(&mut state, PlayerId(0), creature_land_id, 0, None)
            .unwrap();
        let acts_restricted = collect_priority_actions(&state, PlayerId(0));
        assert!(!acts_restricted.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == filter_id
        )));

        // Add Synth Island -> spell is castable via auto_tap; TapMana(FilterLand) omitted until Island is tapped
        let island_id = put_bf(&mut state, CardDefId(1));
        assert!(can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_some());
        let acts_before_island = collect_priority_actions(&state, PlayerId(0));
        assert!(acts_before_island.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == island_id
        )));
        assert!(!acts_before_island.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == filter_id
        )));

        // Tap Synth Island -> pool has unrestricted {U} -> TapMana(FilterLand) IS now emitted
        crate::mana::activate_mana_ability(&mut state, PlayerId(0), island_id, 0, None).unwrap();
        let acts_after_island = collect_priority_actions(&state, PlayerId(0));
        assert!(acts_after_island.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == filter_id
        )));

        // Activating Synth FilterLand succeeds and removes it from TapMana once tapped
        crate::mana::activate_mana_ability(&mut state, PlayerId(0), filter_id, 0, None).unwrap();
        let acts_after_filter = collect_priority_actions(&state, PlayerId(0));
        assert!(!acts_after_filter.iter().any(|a| matches!(
            a,
            Action::TapMana { object, .. } if *object == filter_id
        )));
    }

    #[test]
    fn synth_modal_spell_and_ability_joint_legality_cr_700_2a() {
        let mut tables = InternTables::default();
        tables.keywords.push("spree".into());
        let kw_spree = KeywordId(0);

        let creature_target = |other_target: bool| TargetSpec {
            kind: mtg_ir::TargetKind::Object,
            filter: mtg_ir::ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
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
            zones: ZoneMask::BATTLEFIELD,
        };
        let opp_target = TargetSpec {
            kind: mtg_ir::TargetKind::Player,
            filter: mtg_ir::ObjectFilter::default(),
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

        // Card 0: Synth Bear (2/2 Creature)
        let bear = CardDef {
            oracle_id: "o_synth_bear".into(),
            compiler_version: 46,
            text_hash: "0".into(),
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

        // Card 1: Synth JointModalSpell (min 2, max 2: Mode 0 creature, Mode 1 other_target creature)
        let joint_modal_spell = CardDef {
            oracle_id: "o_synth_joint_modal".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth JointModalSpell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth JointModalSpell".into(),
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
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(mtg_ir::ModalDef {
                        min: 2,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![
                            mtg_ir::ModalMode {
                                targets: vec![creature_target(false)],
                                effect: vec![],
                                span: Default::default(),
                            },
                            mtg_ir::ModalMode {
                                targets: vec![creature_target(true)],
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

        // Card 2: Synth DiffPlayerModal (min 2, max 2, different_player_targets_per_mode: true)
        let diff_player_spell = CardDef {
            oracle_id: "o_synth_diff_player".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth DiffPlayerModal".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth DiffPlayerModal".into(),
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
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(mtg_ir::ModalDef {
                        min: 2,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![
                            mtg_ir::ModalMode {
                                targets: vec![opp_target.clone()],
                                effect: vec![],
                                span: Default::default(),
                            },
                            mtg_ir::ModalMode {
                                targets: vec![opp_target],
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
                        different_player_targets_per_mode: true,
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

        // Card 3: Synth SpreeSpell ({1}{R}, min 1, per_mode_costs: [[{R}], [{2}{R}]])
        let spree_spell = CardDef {
            oracle_id: "o_synth_spree".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth SpreeSpell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth SpreeSpell".into(),
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
                abilities: vec![
                    AbilityDef::Keyword(KeywordAbility {
                        keyword: KeywordDef {
                            keyword: kw_spree,
                            cost: None,
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
                    }),
                    AbilityDef::Spell(SpellAbility {
                        id: None,
                        additional_costs: vec![],
                        targets: vec![],
                        effect: vec![],
                        modal: Some(mtg_ir::ModalDef {
                            min: 1,
                            max: 2,
                            allow_repeat: false,
                            once_per_turn_unique: false,
                            up_to: false,
                            conditional_choose_all: None,
                            modes: vec![
                                mtg_ir::ModalMode {
                                    targets: vec![],
                                    effect: vec![],
                                    span: Default::default(),
                                },
                                mtg_ir::ModalMode {
                                    targets: vec![],
                                    effect: vec![],
                                    span: Default::default(),
                                },
                            ],
                            per_mode_costs: vec![
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
                    }),
                ],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 4: Synth ModalAbilityArtifact ({1}: modal min 2 max 2 creature + other_target creature)
        let modal_ability_art = CardDef {
            oracle_id: "o_synth_modal_art".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ModalAbilityArtifact".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth ModalAbilityArtifact".into(),
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
                abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "act".into(),
                    cost: vec![CostDef::Mana {
                        cost: ManaCost::parse("{1}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(mtg_ir::ModalDef {
                        min: 2,
                        max: 2,
                        allow_repeat: false,
                        once_per_turn_unique: false,
                        up_to: false,
                        conditional_choose_all: None,
                        modes: vec![
                            mtg_ir::ModalMode {
                                targets: vec![creature_target(false)],
                                effect: vec![],
                                span: Default::default(),
                            },
                            mtg_ir::ModalMode {
                                targets: vec![creature_target(true)],
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
                    bear,
                    joint_modal_spell,
                    diff_player_spell,
                    spree_spell,
                    modal_ability_art,
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 802).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);

        let put_zone = |s: &mut GameState, def: CardDefId, zone: Zone| {
            let ts = s.allocate_timestamp();
            let id = s.insert_object(
                crate::state::GameObject::new(def, PlayerId(0), zone, ts, ObjectFlags::empty()),
                None,
            );
            match zone {
                Zone::Hand => s.player_mut(PlayerId(0)).unwrap().hand.push(id),
                Zone::Battlefield => s.player_mut(PlayerId(0)).unwrap().battlefield.push(id),
                _ => {}
            }
            s.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };

        let joint_id = put_zone(&mut state, CardDefId(1), Zone::Hand);
        let diff_p_id = put_zone(&mut state, CardDefId(2), Zone::Hand);
        let spree_id = put_zone(&mut state, CardDefId(3), Zone::Hand);
        let art_id = put_zone(&mut state, CardDefId(4), Zone::Battlefield);

        // Give plenty of {U}, {R}, {C} for target checks on joint_modal & diff_player & ability
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 2, 0, 1, 0, 2];

        // different_player_targets_per_mode in 1v1 (only 1 opponent) -> uncastable
        assert!(can_cast_spell_option(&state, PlayerId(0), diff_p_id, 0).is_none());

        // Spree {1}{R} with only {1}{R} available (1 R + 1 C, cannot afford {R} mode) -> uncastable
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 2, 0, 1, 0, 1];
        assert!(can_cast_spell_option(&state, PlayerId(0), spree_id, 0).is_none());

        // Add one more {R} -> {1}{R}{R} pays base {1}{R} + Mode 0 {R} -> castable
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 2, 0, 2, 0, 1];
        assert!(can_cast_spell_option(&state, PlayerId(0), spree_id, 0).is_some());

        // 1 creature on battlefield -> joint other_target modal spell & modal ability both uncastable/unactivatable
        let _bear1 = put_zone(&mut state, CardDefId(0), Zone::Battlefield);
        assert!(can_cast_spell_option(&state, PlayerId(0), joint_id, 0).is_none());
        assert!(can_activate_ability(&state, PlayerId(0), art_id, 0).is_none());

        // 2 creatures on battlefield -> both joint other_target modal spell & modal ability become legal
        let _bear2 = put_zone(&mut state, CardDefId(0), Zone::Battlefield);
        assert!(can_cast_spell_option(&state, PlayerId(0), joint_id, 0).is_some());
        assert!(can_activate_ability(&state, PlayerId(0), art_id, 0).is_some());
    }

    #[test]
    fn synth_unsatisfiable_forage_ability_and_spell_rejected() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Food".into());
        let food_sub = mtg_ir::SubtypeId(0);

        // Card 0: Synth Food (CantBeSacrificed Food Artifact)
        let locked_food = CardDef {
            oracle_id: "o_synth_locked_food".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth LockedFood".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth LockedFood".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::from_slice(&[food_sub]),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        body: mtg_ir::StaticBody::Restriction {
                            affected: mtg_ir::Affected::SelfObj,
                            restriction: mtg_ir::RestrictionKind::CantBeSacrificed,
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

        // Card 1: Synth ForageRelic ({0}: Forage (no mana cost))
        let forage_relic = CardDef {
            oracle_id: "o_synth_forage_relic".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ForageRelic".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth ForageRelic".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "forage".into(),
                    cost: vec![CostDef::Choice {
                        options: vec![],
                        forage: true,
                    }],
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
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 2: Synth ForageSpell ({0} Sorcery with additional Forage cost)
        let forage_spell = CardDef {
            oracle_id: "o_synth_forage_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth ForageSpell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth ForageSpell".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
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

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![locked_food, forage_relic, forage_spell],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 803).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);

        let ts1 = state.allocate_timestamp();
        let relic_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(relic_id);

        let ts2 = state.allocate_timestamp();
        let food_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts2,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(food_id);

        let ts3 = state.allocate_timestamp();
        let spell_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Hand,
                ts3,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(spell_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // With GY < 3 and only a CantBeSacrificed Food, both {0}-mana forage ability & spell are unpayable
        assert!(can_activate_ability(&state, PlayerId(0), relic_id, 0).is_none());
        assert!(can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_none());

        // Check AutoTapConstraints also holds empty candidates for the locked Food
        let cache = state.evaluate_board();
        let constraints = auto_tap_constraints_for_costs_cached(
            &state,
            PlayerId(0),
            spell_id,
            &[CostDef::Choice {
                options: vec![],
                forage: true,
            }],
            None,
            cache,
        );
        assert!(!constraints.is_satisfiable_cached(&state, PlayerId(0), cache));
    }

    #[test]
    fn synth_convoke_x_choice_and_setzero_legality() {
        use crate::action::DeckSpec;
        use crate::config::GameConfig;
        use mtg_ir::{
            ActivatedAbility, Amount, CardDatabase, CardDef, CardLayout, Characteristics,
            CostDelta, CostDeltaMode, IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility,
            KeywordId, PlayerFilter, PrintedPt, SpellAbility, SpellOrAbilityFilter, StaticAbility,
            StaticDef, SupertypeMask,
        };
        use std::sync::Arc;

        let mut tables = InternTables::default();
        tables.keywords.push("convoke".into());
        let kw_convoke = KeywordId(0);

        let kw_def_empty = |kw: KeywordId| KeywordDef {
            keyword: kw,
            cost: None,
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
        };

        // 0: Synth Convoke Colossus ({X}{G}{W}, Convoke)
        let colossus = CardDef {
            oracle_id: "o_synth_colossus".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Convoke Colossus".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Convoke Colossus".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{X}{G}{W}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G | ColorMask::W,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: kw_def_empty(kw_convoke),
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

        // 1: Synth Red Creature
        let red_cre = CardDef {
            oracle_id: "o_synth_red_cre".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Red Goblin".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Red Goblin".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{R}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
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

        // 2: Synth Choice Aspirant ({W} + Choice([[Behold], [Mana({2})]]))
        let choice_aspirant = CardDef {
            oracle_id: "o_synth_choice_aspirant".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Choice Aspirant".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Choice Aspirant".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{W}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::AdditionalCastCost {
                            costs: vec![CostDef::Choice {
                                options: vec![
                                    vec![CostDef::Behold {
                                        count: Box::new(Amount::Const { n: 1 }),
                                        filter: Box::new(mtg_ir::ObjectFilter::default()),
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

        // 3: Synth Plains
        let plains = CardDef {
            oracle_id: "o_synth_plains".into(),
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
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "tap_w".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![mtg_ir::EffectStep {
                        op: mtg_ir::EffectOp::AddMana {
                            production: mtg_ir::ManaProduction::Fixed {
                                symbols: smallvec::smallvec![ManaColor::W],
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        subject: mtg_ir::Selector::SelfObj,
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Default::default(),
                    }],
                    modal: None,
                    timing: Timing::Mana,
                    is_mana: true,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: ZoneMask::BATTLEFIELD,
                    cost_keyword: None,
                    intrinsic: true,
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

        // 4: Synth SetZero X CantBeZero ({X}{R}, x_cant_be_zero: true)
        let x_nonzero_spell = CardDef {
            oracle_id: "o_synth_x_nonzero".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth X NonZero".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth X NonZero".into(),
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

        // 5: Synth SetZero Emblem (SetZero for all your spells)
        let set_zero_perm = CardDef {
            oracle_id: "o_synth_setzero".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth SetZero Relay".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth SetZero Relay".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
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
                        body: StaticBody::CostModifier {
                            affects: SpellOrAbilityFilter::Spell {
                                filter: Box::new(mtg_ir::ObjectFilter::default()),
                                alt_cost_keyword: None,
                            },
                            delta: CostDelta {
                                mode: CostDeltaMode::SetZero,
                                generic: 0,
                                colored: SmallVec::new(),
                                hybrid_colored: SmallVec::new(),
                                per: None,
                                max_reduction: None,
                                min_total_mana: None,
                                colored_only: false,
                                per_target_life: None,
                                additional_costs: vec![],
                            },
                            who: PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            },
                            who_enchanted_player: false,
                            active_zones: ZoneMask::empty(),
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

        // 6: Synth NonMana X Scanner ({0} Sorcery, additional cost ExileFrom(GY, total_mana_value_equals_x: true) + target creature with divide_total: X, min: 1)
        let x_scan_spell = CardDef {
            oracle_id: "o_synth_x_scan".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth X Scan".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth X Scan".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::SORCERY,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::ExileFrom {
                        zone: Zone::Graveyard,
                        count: Box::new(Amount::Const { n: 0 }),
                        target: None,
                        from_top: false,
                        any_graveyard: false,
                        single_graveyard: false,
                        min_total_mana_value: None,
                        min_total_mana_value_amount: None,
                        min_colored_mana_symbols: None,
                        collect_evidence: false,
                        one_or_more: false,
                        total_mana_value_equals_x: true,
                        and_other_same_name_count: None,
                    }],
                    targets: vec![TargetSpec {
                        kind: mtg_ir::TargetKind::Object,
                        filter: mtg_ir::ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        },
                        player: None,
                        min: 1,
                        max: 1,
                        same_owner: false,
                        condition: None,
                        other_target: false,
                        divide_total: Some(Box::new(Amount::X)),
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
                    colossus,
                    red_cre,
                    choice_aspirant,
                    plains,
                    x_nonzero_spell,
                    set_zero_perm,
                    x_scan_spell,
                ],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 804).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);

        fn spawn(state: &mut GameState, def: CardDefId, zone: Zone) -> ObjectId {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(def, PlayerId(0), zone, ts, ObjectFlags::empty()),
                None,
            );
            let p = state.player_mut(PlayerId(0)).unwrap();
            match zone {
                Zone::Battlefield => p.battlefield.push(id),
                Zone::Hand => p.hand.push(id),
                Zone::Graveyard => p.graveyard.push(id),
                _ => {}
            }
            id
        }

        let colossus_id = spawn(&mut state, CardDefId(0), Zone::Hand);
        let aspirant_id = spawn(&mut state, CardDefId(2), Zone::Hand);
        let x_nz_id = spawn(&mut state, CardDefId(4), Zone::Hand);
        let x_scan_id = spawn(&mut state, CardDefId(6), Zone::Hand);
        for _ in 0..4 {
            spawn(&mut state, CardDefId(1), Zone::Battlefield);
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        // 1. Bug N: Convoke {X}{G}{W} with 0 lands and 4 Red creatures ONLY -> None!
        assert!(can_cast_spell_option(&state, PlayerId(0), colossus_id, 0).is_none());

        // 2. Bug O: Choice Aspirant ({W} + Choice([[Behold], [Mana({2})]]))
        // With 1 Plains -> None; with 3 Plains -> Some
        let _p1 = spawn(&mut state, CardDefId(3), Zone::Battlefield);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(can_cast_spell_option(&state, PlayerId(0), aspirant_id, 0).is_none());
        let _p2 = spawn(&mut state, CardDefId(3), Zone::Battlefield);
        let _p3 = spawn(&mut state, CardDefId(3), Zone::Battlefield);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(can_cast_spell_option(&state, PlayerId(0), aspirant_id, 0).is_some());

        // 3. Bug K: X=0 fails (divide_total: X, min: 1), X=1 passes via 1..=max_scan when GY has MV=1 Red creature
        assert!(can_cast_spell_option(&state, PlayerId(0), x_scan_id, 0).is_none());
        spawn(&mut state, CardDefId(1), Zone::Graveyard);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(can_cast_spell_option(&state, PlayerId(0), x_scan_id, 0).is_some());

        // 4. Bug K: SetZero + x_cant_be_zero -> rejected per CR 107.3b!
        spawn(&mut state, CardDefId(5), Zone::Battlefield);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        assert!(can_cast_spell_option(&state, PlayerId(0), x_nz_id, 0).is_none());
    }

    #[test]
    fn synth_non_mana_tap_self_plus_tap_others_and_auto_tap_reservation() {
        let draw_step = mtg_ir::EffectStep {
            subject: mtg_ir::Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::You,
                },
            },
            op: mtg_ir::EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
            actor: None,
            amount: Some(Box::new(mtg_ir::Amount::Const { n: 1 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        let make_cre_with_ability = |name: &str, abilities: Vec<AbilityDef>| CardDef {
            oracle_id: format!("o_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{G}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
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
        };
        let mk_non_mana_ab = |cost: Vec<CostDef>, effect: Vec<mtg_ir::EffectStep>| {
            AbilityDef::Activated(mtg_ir::ActivatedAbility {
                id: "ab".into(),
                cost,
                targets: vec![],
                effect,
                modal: None,
                timing: mtg_ir::Timing::Instant,
                is_mana: false,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: Default::default(),
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Default::default(),
            })
        };

        let untapped_cre = Box::new(mtg_ir::ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::UNTAPPED,
            ..Default::default()
        });
        let tapped_cre = Box::new(mtg_ir::ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::TAPPED,
            ..Default::default()
        });

        let draw_mage = make_cre_with_ability(
            "Synth Draw Mage",
            vec![mk_non_mana_ab(
                vec![
                    CostDef::TapSelf,
                    CostDef::TapOthers {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        filter: untapped_cre.clone(),
                        share_creature_type: false,
                    },
                ],
                vec![draw_step.clone()],
            )],
        );
        let taxed_mage = make_cre_with_ability(
            "Synth Taxed Mage",
            vec![mk_non_mana_ab(
                vec![
                    CostDef::Mana {
                        cost: ManaCost::parse("{1}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    },
                    CostDef::TapSelf,
                    CostDef::TapOthers {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        filter: untapped_cre,
                        share_creature_type: false,
                    },
                ],
                vec![draw_step.clone()],
            )],
        );
        let untap_twin = make_cre_with_ability(
            "Synth Untap Twin",
            vec![mk_non_mana_ab(
                vec![
                    CostDef::UntapSelf { via_symbol: true },
                    CostDef::UntapOthers {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        filter: tapped_cre,
                    },
                ],
                vec![draw_step],
            )],
        );
        let mana_elf = make_cre_with_ability(
            "Synth Mana Elf",
            vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                id: "elf_mana".into(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![mtg_ir::EffectStep {
                    subject: mtg_ir::Selector::SelfObj,
                    op: mtg_ir::EffectOp::AddMana {
                        production: mtg_ir::ManaProduction::Fixed {
                            symbols: SmallVec::from_slice(&[mtg_ir::ManaColor::G]),
                        },
                        restriction: None,
                        restriction_spec: None,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    actor: None,
                    amount: None,
                    optional: None,
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span: Default::default(),
                }],
                modal: None,
                timing: mtg_ir::Timing::Mana,
                is_mana: true,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: Default::default(),
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Default::default(),
            })],
        );
        let vanilla_bear = make_cre_with_ability("Synth Vanilla Bear", vec![]);
        let plains = CardDef {
            oracle_id: "o_synth_plains_p".into(),
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
                abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "w_mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![mtg_ir::EffectStep {
                        subject: mtg_ir::Selector::SelfObj,
                        op: mtg_ir::EffectOp::AddMana {
                            production: mtg_ir::ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[mtg_ir::ManaColor::W]),
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span: Default::default(),
                    }],
                    modal: None,
                    timing: mtg_ir::Timing::Mana,
                    is_mana: true,
                    activation_restrictions: vec![],
                    activation_condition: None,
                    activator_who: None,
                    power_up_reduction: false,
                    x_cant_be_zero: false,
                    activation_cost_reduction: None,
                    activation_zones: Default::default(),
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
                tables: InternTables::default(),
                cards: vec![
                    draw_mage,
                    taxed_mage,
                    untap_twin,
                    mana_elf,
                    vanilla_bear,
                    plains,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(4); 20],
            sideboard: vec![],
        };
        let spawn = |state: &mut GameState, def: CardDefId| -> ObjectId {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                crate::state::GameObject::new(
                    def,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
            id
        };

        // Case A: Synth Draw Mage alone vs with Synth Vanilla Bear
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                805,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);

            let mage_id = spawn(&mut state, CardDefId(0));
            assert!(can_activate_ability(&state, PlayerId(0), mage_id, 0).is_none());

            let bear_id = spawn(&mut state, CardDefId(4));
            assert!(can_activate_ability(&state, PlayerId(0), mage_id, 0).is_some());
            crate::casting::begin_activate_ability(&mut state, PlayerId(0), mage_id, 0).unwrap();
            if matches!(
                state.decision(),
                crate::action::DecisionState::Prompt { .. }
            ) {
                let prompt_id = state.pending_prompt().unwrap().prompt_id;
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    prompt_id,
                    crate::action::ChoiceAnswer::PickObject { id: bear_id },
                )
                .unwrap();
            }
            assert!(
                state
                    .object(mage_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(bear_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.stack().len(), 1);
        }

        // Case B: Synth Taxed Mage ({1}, {T}, Tap creature) + 1 Synth Mana Elf -> false; + Synth Plains -> true & taps Plains (not Elf)
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                806,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);

            let taxed_id = spawn(&mut state, CardDefId(1));
            let elf_id = spawn(&mut state, CardDefId(3));
            assert!(can_activate_ability(&state, PlayerId(0), taxed_id, 0).is_none());

            let plains_id = spawn(&mut state, CardDefId(5));
            assert!(can_activate_ability(&state, PlayerId(0), taxed_id, 0).is_some());

            crate::casting::begin_activate_ability(&mut state, PlayerId(0), taxed_id, 0).unwrap();
            let cast_acts = crate::casting::legal_casting_actions(&state, PlayerId(0));
            assert!(!cast_acts.iter().any(|a| matches!(
                a,
                crate::action::Action::TapMana { object, .. } if *object == elf_id
            )));
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            if matches!(
                state.decision(),
                crate::action::DecisionState::Prompt { .. }
            ) {
                let prompt_id = state.pending_prompt().unwrap().prompt_id;
                crate::choices::apply_choice_answer(
                    &mut state,
                    PlayerId(0),
                    prompt_id,
                    crate::action::ChoiceAnswer::PickObject { id: elf_id },
                )
                .unwrap();
            }
            assert!(
                state
                    .object(plains_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(taxed_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(elf_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.stack().len(), 1);
        }

        // Case C: Synth Untap Twin ({Q}, Untap tapped creature) tapped alone -> false; with 2nd tapped creature -> true
        {
            let mut state =
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 807).unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);

            let twin_id = spawn(&mut state, CardDefId(2));
            state
                .object_mut(twin_id)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(crate::state::DirtyMask::TAPPED);
            assert!(can_activate_ability(&state, PlayerId(0), twin_id, 0).is_none());

            let bear_id = spawn(&mut state, CardDefId(4));
            state
                .object_mut(bear_id)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(crate::state::DirtyMask::TAPPED);
            assert!(can_activate_ability(&state, PlayerId(0), twin_id, 0).is_some());
        }
    }

    fn perf_make_kw(kw: KeywordId, cost: Option<ManaCost>, cost_defs: Vec<CostDef>) -> KeywordDef {
        KeywordDef {
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

    fn perf_make_card(
        name: &str,
        mana_cost: ManaCost,
        types: CardTypeMask,
        colors: ColorMask,
        pt: Option<(i32, i32)>,
        abilities: Vec<AbilityDef>,
    ) -> CardDef {
        CardDef {
            oracle_id: format!("o_perf_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost,
                    types,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors,
                    color_indicator: None,
                    pt: pt.map(|(p, t)| mtg_ir::PrintedPt::Fixed {
                        power: p,
                        toughness: t,
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
    }

    fn perf_make_spell(
        name: &str,
        cost_str: &str,
        types: CardTypeMask,
        colors: ColorMask,
        additional_costs: Vec<CostDef>,
        x_cant_be_zero: bool,
        extra_abilities: Vec<AbilityDef>,
    ) -> CardDef {
        let mut abilities = vec![AbilityDef::Spell(SpellAbility {
            id: None,
            additional_costs,
            targets: vec![],
            effect: vec![],
            modal: None,
            cast_restrictions: vec![],
            cast_condition: None,
            x_cant_be_zero,
            span: Default::default(),
        })];
        abilities.extend(extra_abilities);
        perf_make_card(
            name,
            ManaCost::parse(cost_str),
            types,
            colors,
            if types.contains(CardTypeMask::CREATURE) {
                Some((2, 2))
            } else {
                None
            },
            abilities,
        )
    }

    fn perf_make_land(name: &str, color: mtg_ir::ManaColor) -> CardDef {
        perf_make_card(
            name,
            ManaCost::default(),
            CardTypeMask::LAND,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                id: "mana".into(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![mtg_ir::EffectStep {
                    subject: mtg_ir::Selector::SelfObj,
                    op: mtg_ir::EffectOp::AddMana {
                        production: mtg_ir::ManaProduction::Fixed {
                            symbols: SmallVec::from_slice(&[color]),
                        },
                        restriction: None,
                        restriction_spec: None,
                        retain_until_eot: false,
                        delayed_rider: None,
                    },
                    actor: None,
                    amount: None,
                    optional: None,
                    gate: None,
                    bind_as: None,
                    do_this_once_per_turn: false,
                    reflexive_trigger: None,
                    span: Default::default(),
                }],
                modal: None,
                timing: mtg_ir::Timing::Mana,
                is_mana: true,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: Default::default(),
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Default::default(),
            })],
        )
    }

    fn perf_spawn(state: &mut GameState, player: PlayerId, def: CardDefId, zone: Zone) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            crate::state::GameObject::new(def, player, zone, ts, ObjectFlags::empty()),
            None,
        );
        let p = state.player_mut(player).unwrap();
        match zone {
            Zone::Hand => p.hand.push(oid),
            Zone::Battlefield => p.battlefield.push(oid),
            Zone::Graveyard => p.graveyard.push(oid),
            Zone::Exile => p.exile.push(oid),
            Zone::Library => p.library.push(oid),
            Zone::Command => p.command.push(oid),
            Zone::Stack => {}
        }
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        oid
    }

    fn perf_clear_hand(state: &mut GameState, player: PlayerId) {
        let hand_ids: Vec<ObjectId> = state.player(player).unwrap().hand.clone();
        for id in hand_ids {
            let _ = crate::zones::move_object(
                state,
                id,
                Zone::Library,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
    }

    fn assert_collect_priority_parity(state: &GameState, player: PlayerId, label: &str) {
        let cache = state.evaluate_board();
        let actual = collect_priority_actions(state, player);

        let mut actual_casts: Vec<(ObjectId, u16, Zone)> = actual
            .iter()
            .filter_map(|a| match a {
                Action::BeginCast {
                    object,
                    option_id,
                    from_zone,
                } => Some((*object, *option_id, *from_zone)),
                _ => None,
            })
            .collect();
        actual_casts.sort_by_key(|(o, opt, z)| (*o, *opt, *z as u8));

        let mut expected_casts: Vec<(ObjectId, u16, Zone)> = Vec::new();
        let p = state.player(player).unwrap();
        let mut spell_zones: Vec<(ObjectId, Zone)> = Vec::new();
        for &id in &p.hand {
            spell_zones.push((id, Zone::Hand));
        }
        for &id in &p.command {
            spell_zones.push((id, Zone::Command));
        }
        for &id in &p.exile {
            spell_zones.push((id, Zone::Exile));
        }
        for &id in &p.graveyard {
            spell_zones.push((id, Zone::Graveyard));
        }
        if let Some(&top) = p.library.last() {
            spell_zones.push((top, Zone::Library));
        }
        for (oid, _zone) in spell_zones {
            for opt in enumerate_cast_options_cached(state, player, oid, cache) {
                if let Some(valid) = can_cast_spell_option_with(state, player, oid, opt, cache) {
                    expected_casts.push((oid, valid.option_id, valid.from_zone));
                }
            }
        }
        expected_casts.sort_by_key(|(o, opt, z)| (*o, *opt, *z as u8));
        assert_eq!(
            actual_casts, expected_casts,
            "BeginCast parity mismatch ({label})"
        );

        let mut actual_acts: Vec<(ObjectId, u16)> = actual
            .iter()
            .filter_map(|a| match a {
                Action::BeginActivate {
                    object,
                    ability_index,
                } => Some((*object, *ability_index)),
                _ => None,
            })
            .collect();
        actual_acts.sort_by_key(|(o, idx)| (*o, *idx));

        let mut expected_acts: Vec<(ObjectId, u16)> = Vec::new();
        for &oid in p
            .battlefield
            .iter()
            .chain(p.hand.iter())
            .chain(p.graveyard.iter())
        {
            for res in resolve_activated_abilities_cached(state, oid, cache) {
                if can_activate_ability_cached(state, player, oid, res.ability_index, cache)
                    .is_some()
                {
                    expected_acts.push((oid, res.ability_index));
                }
            }
        }
        expected_acts.sort_by_key(|(o, idx)| (*o, *idx));
        assert_eq!(
            actual_acts, expected_acts,
            "BeginActivate parity mismatch ({label})"
        );
    }

    #[test]
    fn synth_perf_collect_priority_actions_parity_aids_and_discounts() {
        let mut tables = InternTables::default();
        tables.keywords.push("convoke".into());
        let kw_convoke = KeywordId(0);
        tables.keywords.push("improvise".into());
        let kw_improvise = KeywordId(1);
        tables.keywords.push("delve".into());
        let kw_delve = KeywordId(2);
        tables.keywords.push("emerge".into());
        let kw_emerge = KeywordId(3);

        // Card 0: Plains, Card 1: Island, Card 2: Swamp
        let plains = perf_make_land("Synth Plains", mtg_ir::ManaColor::W);
        let island = perf_make_land("Synth Island", mtg_ir::ManaColor::U);
        let swamp = perf_make_land("Synth Swamp", mtg_ir::ManaColor::B);
        // Card 3: Green creature token/body for Convoke + Emerge sacrifice
        let green_cre = perf_make_card(
            "Synth Elk",
            ManaCost::parse("{3}{G}"),
            CardTypeMask::CREATURE,
            ColorMask::G,
            Some((3, 3)),
            vec![],
        );
        // Card 4: Vanilla non-mana Artifact for Improvise
        let relic = perf_make_card(
            "Synth Relic",
            ManaCost::parse("{0}"),
            CardTypeMask::ARTIFACT,
            ColorMask::empty(),
            None,
            vec![],
        );
        // Card 5: Convoke {3}{G}
        let convoke_spell = perf_make_spell(
            "Synth Convoke Call",
            "{3}{G}",
            CardTypeMask::SORCERY,
            ColorMask::G,
            vec![],
            false,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: perf_make_kw(kw_convoke, None, vec![]),
                id: None,
                span: Default::default(),
            })],
        );
        // Card 6: Improvise {4}{U}
        let improvise_spell = perf_make_spell(
            "Synth Improvise Draw",
            "{4}{U}",
            CardTypeMask::SORCERY,
            ColorMask::U,
            vec![],
            false,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: perf_make_kw(kw_improvise, None, vec![]),
                id: None,
                span: Default::default(),
            })],
        );
        // Card 7: Delve {4}{B}
        let delve_spell = perf_make_spell(
            "Synth Delve Cut",
            "{4}{B}",
            CardTypeMask::SORCERY,
            ColorMask::B,
            vec![],
            false,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: perf_make_kw(kw_delve, None, vec![]),
                id: None,
                span: Default::default(),
            })],
        );
        // Card 8: Emerge {5} / emerge {2}{G}
        let emerge_cre = perf_make_spell(
            "Synth Emerge Fiend",
            "{5}",
            CardTypeMask::CREATURE,
            ColorMask::empty(),
            vec![],
            false,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: perf_make_kw(kw_emerge, Some(ManaCost::parse("{2}{G}")), vec![]),
                id: None,
                span: Default::default(),
            })],
        );
        // Card 9: Defiler enchantment ({R} -> 2 life for Red creature spells)
        let defiler_perm = perf_make_card(
            "Synth Red Defiler",
            ManaCost::parse("{3}{R}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::R,
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::DefilerCostReduction {
                        color: mtg_ir::ManaColor::R,
                        life_amount: 2,
                        filter: Box::new(mtg_ir::ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // Card 10: Red creature {1}{R} (tests Defiler reduction)
        let red_cre = perf_make_spell(
            "Synth Red Hound",
            "{1}{R}",
            CardTypeMask::CREATURE,
            ColorMask::R,
            vec![],
            false,
            vec![],
        );
        // Card 11: Commander {W}
        let cmd_cre = perf_make_spell(
            "Synth White Commander",
            "{W}",
            CardTypeMask::CREATURE,
            ColorMask::W,
            vec![],
            false,
            vec![],
        );
        // Card 12: CommanderTaxAsLife static perm
        let tax_life_perm = perf_make_card(
            "Synth Tax Idol",
            ManaCost::parse("{2}"),
            CardTypeMask::ARTIFACT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::CommanderTaxAsLife { life_per_cast: 2 },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // Card 13: SetZero static perm for Sorceries
        let set_zero_perm = perf_make_card(
            "Synth Omniscience Lens",
            ManaCost::parse("{6}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::CostModifier {
                        affects: mtg_ir::SpellOrAbilityFilter::Spell {
                            filter: Box::new(mtg_ir::ObjectFilter {
                                types_any: CardTypeMask::SORCERY,
                                ..Default::default()
                            }),
                            alt_cost_keyword: None,
                        },
                        delta: mtg_ir::CostDelta {
                            mode: mtg_ir::CostDeltaMode::SetZero,
                            generic: 0,
                            per: None,
                            additional_costs: vec![],
                            colored: SmallVec::new(),
                            hybrid_colored: SmallVec::new(),
                            colored_only: false,
                            min_total_mana: None,
                            max_reduction: None,
                            per_target_life: None,
                        },
                        who: mtg_ir::PlayerFilter {
                            relation: mtg_ir::PlayerRelation::You,
                        },
                        who_enchanted_player: false,
                        active_zones: ZoneMask::empty(),
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // Card 14: Big unaffordable / SetZero Sorcery {8}{W}{W}
        let big_sorc = perf_make_spell(
            "Synth Titanic Decree",
            "{8}{W}{W}",
            CardTypeMask::SORCERY,
            ColorMask::W,
            vec![],
            false,
            vec![],
        );
        // Card 15: Phyrexian {W/P}
        let phyr_sorc = perf_make_spell(
            "Synth Phyr Probe",
            "{W/P}",
            CardTypeMask::SORCERY,
            ColorMask::W,
            vec![],
            false,
            vec![],
        );
        // Card 16: Hybrid {W/U}
        let hyb_sorc = perf_make_spell(
            "Synth Hybrid Bolt",
            "{W/U}",
            CardTypeMask::SORCERY,
            ColorMask::W | ColorMask::U,
            vec![],
            false,
            vec![],
        );
        // Card 17: CostDef::Choice { [Mana({5}), Discard(1)] } base {0}
        let choice_sorc = perf_make_spell(
            "Synth Choice Rite",
            "{0}",
            CardTypeMask::SORCERY,
            ColorMask::empty(),
            vec![CostDef::Choice {
                options: vec![
                    vec![CostDef::Mana {
                        cost: ManaCost::parse("{5}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                    vec![CostDef::Discard {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: None,
                        random: false,
                        same_name_pair: false,
                        same_name_as_controlled: None,
                        different_names: false,
                    }],
                ],
                forage: false,
            }],
            false,
            vec![],
        );
        // Card 18: {X}{W} (x_cant_be_zero = false) & Card 19: {X}{W} (x_cant_be_zero = true)
        let x0_sorc = perf_make_spell(
            "Synth X Zero OK",
            "{X}{W}",
            CardTypeMask::SORCERY,
            ColorMask::W,
            vec![],
            false,
            vec![],
        );
        let x1_sorc = perf_make_spell(
            "Synth X Cant Be Zero",
            "{X}{W}",
            CardTypeMask::SORCERY,
            ColorMask::W,
            vec![],
            true,
            vec![],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    plains,
                    island,
                    swamp,
                    green_cre,
                    relic,
                    convoke_spell,
                    improvise_spell,
                    delve_spell,
                    emerge_cre,
                    defiler_perm,
                    red_cre,
                    cmd_cre,
                    tax_life_perm,
                    set_zero_perm,
                    big_sorc,
                    phyr_sorc,
                    hyb_sorc,
                    choice_sorc,
                    x0_sorc,
                    x1_sorc,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let d0 = DeckSpec {
            commanders: vec![CardDefId(11)],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let d1 = DeckSpec {
            commanders: vec![CardDefId(11)],
            mainboard: vec![CardDefId(0); 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(Arc::clone(&db), GameConfig::commander(2), &[d0, d1], 777).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);
        perf_clear_hand(&mut state, PlayerId(0));

        // Put one of each test spell into Hand
        for def_idx in [5u32, 6, 7, 8, 10, 14, 15, 16, 17, 18, 19] {
            perf_spawn(&mut state, PlayerId(0), CardDefId(def_idx), Zone::Hand);
        }

        // Baseline: empty BF, 0 mana -> Choice (discard branch) & Phyrexian ({W/P}) castable; rest pruned!
        assert_collect_priority_parity(&state, PlayerId(0), "baseline empty bf");

        // Convoke (4 green creatures) + Improvise (4 artifacts) + Delve (4 GY) + 1 Island + 1 Swamp + 1 Plains
        for _ in 0..4 {
            perf_spawn(&mut state, PlayerId(0), CardDefId(3), Zone::Battlefield);
            perf_spawn(&mut state, PlayerId(0), CardDefId(4), Zone::Battlefield);
            perf_spawn(&mut state, PlayerId(0), CardDefId(0), Zone::Graveyard);
        }
        perf_spawn(&mut state, PlayerId(0), CardDefId(1), Zone::Battlefield);
        perf_spawn(&mut state, PlayerId(0), CardDefId(2), Zone::Battlefield);
        perf_spawn(&mut state, PlayerId(0), CardDefId(0), Zone::Battlefield);
        assert_collect_priority_parity(&state, PlayerId(0), "aids enabled");

        // Verify Convoke, Improvise, Delve, Emerge, Hybrid, X=0, X>=1 are all present cleanly
        let acts = collect_priority_actions(&state, PlayerId(0));
        for check_def in [5u32, 6, 7, 8, 15, 16, 17, 18, 19] {
            assert!(
                acts.iter().any(|a| matches!(
                    a,
                    Action::BeginCast { object, .. } if state.object(*object).unwrap().card_def == CardDefId(check_def)
                )),
                "expected CardDefId({check_def}) to be castable"
            );
        }
        // Titanic Decree {8}{W}{W} should still be pruned/unaffordable
        assert!(!acts.iter().any(|a| matches!(
            a,
            Action::BeginCast { object, .. } if state.object(*object).unwrap().card_def == CardDefId(14)
        )));

        // Commander cast once (+2 tax): with CommanderTaxAsLife + 1 Plains -> still castable!
        let cmd_id = state.players()[0].command[0];
        crate::commander::record_command_zone_cast(&mut state, cmd_id, Zone::Command);
        perf_spawn(&mut state, PlayerId(0), CardDefId(12), Zone::Battlefield);
        // Defiler + {1} colorless (remove colored lands except 1 Plains) -> Red Hound {1}{R} castable via 2 life + {W} as generic!
        perf_spawn(&mut state, PlayerId(0), CardDefId(9), Zone::Battlefield);
        assert_collect_priority_parity(&state, PlayerId(0), "defiler + commander tax as life");

        // SetZero makes Titanic Decree {8}{W}{W} castable!
        let sz_id = perf_spawn(&mut state, PlayerId(0), CardDefId(13), Zone::Battlefield);
        assert_collect_priority_parity(&state, PlayerId(0), "set_zero enabled");
        assert!(
            collect_priority_actions(&state, PlayerId(0))
                .iter()
                .any(|a| matches!(
                    a,
                    Action::BeginCast { object, .. } if state.object(*object).unwrap().card_def == CardDefId(14)
                ))
        );

        // Remove SetZero & drop life to 2 (disables Phyrexian / Defiler / CommanderTaxAsLife if <=2) -> parity holds!
        let _ = crate::zones::move_object(
            &mut state,
            sz_id,
            Zone::Graveyard,
            crate::zones::MoveRawOptions::default(),
            None,
        )
        .unwrap();
        state.player_mut(PlayerId(0)).unwrap().life = 2;
        assert_collect_priority_parity(&state, PlayerId(0), "low life & set_zero removed");
    }

    #[test]
    fn synth_perf_static_presence_flags_and_step4_fast_skips() {
        let plains = perf_make_land("Synth Plains", mtg_ir::ManaColor::W);
        let vanilla = perf_make_card(
            "Synth Vanilla Bear",
            ManaCost::parse("{1}{W}"),
            CardTypeMask::CREATURE,
            ColorMask::W,
            Some((2, 2)),
            vec![],
        );
        // Innate tapping non-mana ability creature (summoning sickness tests ActivateAsThoughHaste & CantActivateAbilities)
        let tap_mage = perf_make_card(
            "Synth Tapper",
            ManaCost::parse("{W}"),
            CardTypeMask::CREATURE,
            ColorMask::W,
            Some((1, 1)),
            vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                id: "tap_ping".into(),
                cost: vec![CostDef::TapSelf],
                targets: vec![],
                effect: vec![],
                modal: None,
                timing: mtg_ir::Timing::Instant,
                is_mana: false,
                activation_restrictions: vec![],
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: Default::default(),
                cost_keyword: None,
                intrinsic: false,
                text: None,
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Default::default(),
            })],
        );
        let gy_sorc = perf_make_spell(
            "Synth GY Sorcery",
            "{W}",
            CardTypeMask::SORCERY,
            ColorMask::W,
            vec![],
            false,
            vec![],
        );
        let gy_perm = perf_make_card(
            "Synth GY Perm",
            ManaCost::parse("{2}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::ZonePlayPermission {
                        zone: Zone::Graveyard,
                        allow_lands: false,
                        allow_spells: true,
                        spell_filter: None,
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
        );
        let foretell_grant = perf_make_card(
            "Synth Foretell Grant",
            ManaCost::parse("{2}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::GrantHandForetell {
                        filter: Box::new(mtg_ir::ObjectFilter::default()),
                        reduce_generic: 1,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let haste_act_grant = perf_make_card(
            "Synth Haste Act Grant",
            ManaCost::parse("{1}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::ActivateAsThoughHaste {
                        filter: Box::new(mtg_ir::ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let cant_act_perm = perf_make_card(
            "Synth Null Rod",
            ManaCost::parse("{2}"),
            CardTypeMask::ARTIFACT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::CantActivateAbilities {
                        affected: mtg_ir::Affected::Filter {
                            filter: Box::new(mtg_ir::ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        allow_mana_abilities: true,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let grant_ab_perm = perf_make_card(
            "Synth Ability Aura",
            ManaCost::parse("{1}"),
            CardTypeMask::ENCHANTMENT,
            ColorMask::empty(),
            None,
            vec![AbilityDef::Static(mtg_ir::StaticAbility {
                static_def: mtg_ir::StaticDef {
                    body: mtg_ir::StaticBody::Continuous {
                        layer: mtg_ir::Layer::Layer6,
                        affected: mtg_ir::Affected::Filter {
                            filter: Box::new(mtg_ir::ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: mtg_ir::LayerMod::GrantAbility {
                            ability: Box::new(AbilityDef::Activated(mtg_ir::ActivatedAbility {
                                id: "granted_ping".into(),
                                cost: vec![],
                                targets: vec![],
                                effect: vec![],
                                modal: None,
                                timing: mtg_ir::Timing::Instant,
                                is_mana: false,
                                activation_restrictions: vec![],
                                activation_condition: None,
                                activator_who: None,
                                power_up_reduction: false,
                                x_cant_be_zero: false,
                                activation_cost_reduction: None,
                                activation_zones: Default::default(),
                                cost_keyword: None,
                                intrinsic: false,
                                text: None,
                                grant_condition: None,
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                                span: Default::default(),
                            })),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![
                    plains,
                    vanilla,
                    tap_mage,
                    gy_sorc,
                    gy_perm,
                    foretell_grant,
                    haste_act_grant,
                    cant_act_perm,
                    grant_ab_perm,
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 888).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);
        perf_clear_hand(&mut state, PlayerId(0));

        perf_spawn(&mut state, PlayerId(0), CardDefId(0), Zone::Battlefield);
        let bear_id = perf_spawn(&mut state, PlayerId(0), CardDefId(1), Zone::Battlefield);
        let tapper_id = perf_spawn(&mut state, PlayerId(0), CardDefId(2), Zone::Battlefield);
        state
            .object_mut(tapper_id)
            .unwrap()
            .flags
            .insert(ObjectFlags::SUMMONING_SICK);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);
        let gy_id = perf_spawn(&mut state, PlayerId(0), CardDefId(3), Zone::Graveyard);
        let hand_sorc_id = perf_spawn(&mut state, PlayerId(0), CardDefId(3), Zone::Hand);

        // No statics on BF: all StaticPresenceFlags must be empty; summoning sick tapper cannot activate.
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::empty()
        );
        assert!(can_cast_spell_option(&state, PlayerId(0), gy_id, 0).is_none());
        assert!(hand_foretell_cost(&state, PlayerId(0), hand_sorc_id).is_none());
        assert!(can_activate_ability(&state, PlayerId(0), tapper_id, 0).is_none());
        assert_collect_priority_parity(&state, PlayerId(0), "no statics");

        // ZonePlayPermission on BF flips ZONE_PLAY_PERMISSION and lets gy_id be cast.
        let gyp_id = perf_spawn(&mut state, PlayerId(0), CardDefId(4), Zone::Battlefield);
        assert!(
            state
                .evaluate_board()
                .static_presence
                .contains(StaticPresenceFlags::ZONE_PLAY_PERMISSION)
        );
        assert!(can_cast_spell_option(&state, PlayerId(0), gy_id, 0).is_some());
        assert_collect_priority_parity(&state, PlayerId(0), "zone play permission");

        // GrantHandForetell flips GRANT_HAND_FORETELL and enables hand_foretell_cost.
        let fg_id = perf_spawn(&mut state, PlayerId(0), CardDefId(5), Zone::Battlefield);
        assert!(
            state
                .evaluate_board()
                .static_presence
                .contains(StaticPresenceFlags::GRANT_HAND_FORETELL)
        );
        assert!(hand_foretell_cost(&state, PlayerId(0), hand_sorc_id).is_some());

        // ActivateAsThoughHaste flips ACTIVATE_AS_THOUGH_HASTE and lets summoning-sick tapper activate!
        let hg_id = perf_spawn(&mut state, PlayerId(0), CardDefId(6), Zone::Battlefield);
        assert!(
            state
                .evaluate_board()
                .static_presence
                .contains(StaticPresenceFlags::ACTIVATE_AS_THOUGH_HASTE)
        );
        assert!(can_activate_ability(&state, PlayerId(0), tapper_id, 0).is_some());
        assert_collect_priority_parity(&state, PlayerId(0), "activate as though haste");

        // CantActivateAbilities flips CANT_ACTIVATE_ABILITIES and suppresses tapper activation!
        let ca_id = perf_spawn(&mut state, PlayerId(0), CardDefId(7), Zone::Battlefield);
        assert!(
            state
                .evaluate_board()
                .static_presence
                .contains(StaticPresenceFlags::CANT_ACTIVATE_ABILITIES)
        );
        assert!(can_activate_ability(&state, PlayerId(0), tapper_id, 0).is_none());
        assert_collect_priority_parity(&state, PlayerId(0), "cant activate abilities");

        // Remove CantActivateAbilities, keep bear_id + grant a non-mana activated ability via Layer6 GrantAbility:
        // Step 4 might_have_non_mana_activated_ability must NOT skip bear_id!
        for rm_id in [gyp_id, fg_id, hg_id, ca_id] {
            let _ = crate::zones::move_object(
                &mut state,
                rm_id,
                Zone::Graveyard,
                crate::zones::MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::empty()
        );
        perf_spawn(&mut state, PlayerId(0), CardDefId(8), Zone::Battlefield);
        assert_collect_priority_parity(&state, PlayerId(0), "granted non-mana ability on vanilla");
        assert!(
            collect_priority_actions(&state, PlayerId(0))
                .iter()
                .any(|a| matches!(a, Action::BeginActivate { object, .. } if *object == bear_id))
        );
    }
}
