use mtg_ir::{
    AbilityDef, Affected, CardTypeMask, ColorMask, ManaCost, ModalDef, ObjectFilter,
    ObjectPredicate, PlayerRelation, RelativeControl, RuleMod, StackTargetKind, StaticBody,
    SubtypeId, TargetKind, TargetSpec, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::TargetRef;
use crate::eval::{
    BoardView, EvalContext, effective_controller_for_object, evaluate_amount, evaluate_condition,
    object_mana_value, object_types_subtypes,
};
use crate::faces::{
    active_abilities_with_card_for_object, face_opts_for_object,
    for_each_active_ability_with_card_on_faces, resolve_object_faces,
};
use crate::filter::{matches_object_filter, matches_player_filter, matches_stack_entry};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::layers::predicates::GrantedAbilityEntry;
use crate::state::{
    EvaluatedObject, GameState, LayerCache, LkiSnapshot, ResolutionBinding, StackEntry,
    StackEntryKind,
};

#[derive(Debug, Clone)]
pub struct TargetSourceInfo<'a> {
    pub caster: PlayerId,
    pub source_object: Option<ObjectId>,
    pub source_lki: Option<&'a LkiSnapshot>,
    pub is_spell: bool,
    pub is_ability: bool,
    pub source_colors: ColorMask,
    pub source_types: CardTypeMask,
    pub source_subtypes: SmallVec<[SubtypeId; 4]>,
    pub x_value: Option<u32>,
    pub targets_by_spec: &'a [Vec<TargetRef>],
    pub bindings: &'a [ResolutionBinding],
    pub event_object: Option<ObjectId>,
    pub event_lki: Option<&'a LkiSnapshot>,
    pub event_player: Option<PlayerId>,
    pub defending_player: Option<PlayerId>,
}

impl<'a> TargetSourceInfo<'a> {
    pub fn to_eval_context(&self) -> EvalContext<'a> {
        let mut ctx = EvalContext::new(self.caster);
        ctx.source = self.source_object;
        ctx.lki_override = self.source_lki;
        ctx.x_value = self.x_value;
        ctx.bindings = self.bindings;
        ctx.event_object = self.event_object;
        ctx.event_lki = self.event_lki;
        ctx.event_player = self.event_player;
        ctx.defending_player = self.defending_player;
        ctx
    }

    #[allow(dead_code)]
    pub(crate) fn to_eval_context_cached(&self, cache: &'a LayerCache) -> EvalContext<'a> {
        self.to_eval_context().with_board(BoardView::Cached(cache))
    }

    pub(crate) fn with_eval_context_cached<R>(
        &self,
        cache: &'a LayerCache,
        f: impl FnOnce(&EvalContext<'_>) -> R,
    ) -> R {
        let scope = TargetEvalScope::new(self, Some(cache));
        let ctx = scope.ctx();
        f(&ctx)
    }
}

struct TargetEvalScope<'a> {
    info: &'a TargetSourceInfo<'a>,
    cache: Option<&'a LayerCache>,
    flat_targets: SmallVec<[TargetRef; 8]>,
    by_spec_slices: SmallVec<[&'a [TargetRef]; 4]>,
}

impl<'a> TargetEvalScope<'a> {
    fn new(info: &'a TargetSourceInfo<'a>, cache: Option<&'a LayerCache>) -> Self {
        let mut flat_targets = SmallVec::new();
        let mut by_spec_slices = SmallVec::with_capacity(info.targets_by_spec.len());
        for spec_targets in info.targets_by_spec {
            flat_targets.extend_from_slice(spec_targets);
            by_spec_slices.push(spec_targets.as_slice());
        }
        Self {
            info,
            cache,
            flat_targets,
            by_spec_slices,
        }
    }

    #[inline]
    fn ctx(&self) -> EvalContext<'_> {
        let mut ctx = self.info.to_eval_context();
        ctx.targets = &self.flat_targets;
        ctx.targets_by_spec = &self.by_spec_slices;
        if let Some(c) = self.cache {
            ctx.board = BoardView::Cached(c);
        }
        ctx
    }
}

pub fn build_spell_target_source_info<'a>(
    state: &GameState,
    caster: PlayerId,
    spell_obj: ObjectId,
    x_value: Option<u32>,
    targets_by_spec: &'a [Vec<TargetRef>],
) -> TargetSourceInfo<'a> {
    build_spell_target_source_info_cached(
        state,
        caster,
        spell_obj,
        x_value,
        targets_by_spec,
        state.evaluate_board(),
    )
}

pub(crate) fn build_spell_target_source_info_cached<'a>(
    state: &GameState,
    caster: PlayerId,
    spell_obj: ObjectId,
    x_value: Option<u32>,
    targets_by_spec: &'a [Vec<TargetRef>],
    cache: &LayerCache,
) -> TargetSourceInfo<'a> {
    let eval = cache.get(spell_obj);
    let (source_colors, source_types, source_subtypes) = if let Some(ev) = eval {
        (ev.colors, ev.types, ev.subtypes.clone())
    } else if let (Some(obj), Some(opts)) = (
        state.object(spell_obj),
        face_opts_for_object(state, spell_obj),
    ) && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        (view.colors, view.types, view.subtypes)
    } else {
        (ColorMask::empty(), CardTypeMask::empty(), SmallVec::new())
    };
    TargetSourceInfo {
        caster,
        source_object: Some(spell_obj),
        source_lki: None,
        is_spell: true,
        is_ability: false,
        source_colors,
        source_types,
        source_subtypes,
        x_value,
        targets_by_spec,
        bindings: &[],
        event_object: None,
        event_lki: None,
        event_player: None,
        defending_player: None,
    }
}

pub fn build_ability_target_source_info<'a>(
    state: &GameState,
    controller: PlayerId,
    source_obj: ObjectId,
    source_lki: Option<&'a LkiSnapshot>,
    x_value: Option<u32>,
    targets_by_spec: &'a [Vec<TargetRef>],
    bindings: &'a [ResolutionBinding],
) -> TargetSourceInfo<'a> {
    build_ability_target_source_info_cached(
        state,
        controller,
        source_obj,
        source_lki,
        x_value,
        targets_by_spec,
        bindings,
        state.evaluate_board(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_ability_target_source_info_cached<'a>(
    state: &GameState,
    controller: PlayerId,
    source_obj: ObjectId,
    source_lki: Option<&'a LkiSnapshot>,
    x_value: Option<u32>,
    targets_by_spec: &'a [Vec<TargetRef>],
    bindings: &'a [ResolutionBinding],
    cache: &LayerCache,
) -> TargetSourceInfo<'a> {
    let (source_colors, source_types, source_subtypes) = if let Some(ev) = cache.get(source_obj) {
        (ev.colors, ev.types, ev.subtypes.clone())
    } else if let Some(lki) = source_lki {
        (lki.colors, lki.types, lki.subtypes.clone())
    } else if let (Some(obj), Some(opts)) = (
        state.object(source_obj),
        face_opts_for_object(state, source_obj),
    ) && let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        (view.colors, view.types, view.subtypes)
    } else {
        (ColorMask::empty(), CardTypeMask::empty(), SmallVec::new())
    };
    TargetSourceInfo {
        caster: controller,
        source_object: Some(source_obj),
        source_lki,
        is_spell: false,
        is_ability: true,
        source_colors,
        source_types,
        source_subtypes,
        x_value,
        targets_by_spec,
        bindings,
        event_object: None,
        event_lki: None,
        event_player: None,
        defending_player: None,
    }
}

pub fn build_stack_entry_target_source_info<'a>(
    state: &GameState,
    entry: &'a StackEntry,
    targets_by_spec: &'a [Vec<TargetRef>],
) -> TargetSourceInfo<'a> {
    match &entry.kind {
        StackEntryKind::Spell { object } => build_spell_target_source_info(
            state,
            entry.controller,
            *object,
            Some(entry.x_value),
            targets_by_spec,
        ),
        StackEntryKind::Activated { source, lki, .. }
        | StackEntryKind::Triggered { source, lki, .. } => build_ability_target_source_info(
            state,
            entry.controller,
            *source,
            Some(lki.as_ref()),
            Some(entry.x_value),
            targets_by_spec,
            &[],
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRevalidation {
    AllLegal,
    PartiallyIllegal {
        legal_count: usize,
        total_count: usize,
    },
    AllIllegal,
}

/// Computes the effective `(min, max)` target count for `spec`, honoring `condition`,
/// `per_opponent`, and `dynamic_count` / `dynamic_up_to`.
pub fn effective_target_bounds(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
) -> (u16, u16) {
    effective_target_bounds_cached(state, spec, info, state.evaluate_board())
}

pub(crate) fn effective_target_bounds_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
) -> (u16, u16) {
    let scope = TargetEvalScope::new(info, Some(cache));
    let ctx = scope.ctx();
    if let Some(cond) = spec.condition.as_deref()
        && !evaluate_condition(state, cond, &ctx)
    {
        return (0, 0);
    }
    let (min_req, mut max_req) = if spec.per_opponent {
        let opp_count = state
            .players()
            .iter()
            .filter(|p| !p.lost && p.id != info.caster)
            .count() as u16;
        let min = if spec.min == 0 { 0 } else { opp_count };
        (min, opp_count)
    } else if let Some(dyn_amt) = spec.dynamic_count.as_deref() {
        let count = evaluate_amount(state, dyn_amt, &ctx)
            .max(0)
            .min(i32::from(u16::MAX)) as u16;
        if spec.dynamic_up_to {
            (0, count)
        } else {
            (count, count)
        }
    } else {
        (spec.min, spec.max)
    };
    if let Some(div_amt) = spec.divide_total.as_deref() {
        let div_total = evaluate_amount(state, div_amt, &ctx)
            .max(0)
            .min(i32::from(u16::MAX)) as u16;
        max_req = max_req.min(div_total);
    }
    (min_req, max_req)
}

fn effective_zone_mask(spec: &TargetSpec) -> ZoneMask {
    let mut zm = if !spec.zones.is_empty() {
        spec.zones
    } else if !spec.filter.zones.is_empty() {
        spec.filter.zones
    } else {
        ZoneMask::BATTLEFIELD
    };
    if spec.also_spells {
        zm |= ZoneMask::STACK;
    }
    zm
}

fn keyword_name_matches(db: &mtg_ir::CardDatabase, kw: mtg_ir::KeywordId, needle: &str) -> bool {
    db.keyword_name(kw)
        .is_some_and(|n| n.eq_ignore_ascii_case(needle))
}

fn source_has_protection_quality(
    state: &GameState,
    quality: &str,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
) -> bool {
    let live_obj = info.source_object.and_then(|id| state.object(id));
    let (supertypes, has_all_creature_types, mana_value, controller) =
        if let Some(ev) = info.source_object.and_then(|id| cache.get(id)) {
            (
                ev.supertypes,
                ev.has_all_creature_types,
                ev.mana_value,
                ev.controller,
            )
        } else if let Some(id) = info.source_object
            && let Some(obj) = live_obj
        {
            let ctx = EvalContext::new(info.caster).with_board(BoardView::Cached(cache));
            let off = crate::eval::off_battlefield_characteristics(state, id, obj, obj.zone, &ctx);
            (
                off.supertypes,
                off.has_all_creature_types,
                off.mana_value,
                obj.controller,
            )
        } else if let Some(lki) = info.source_lki {
            (
                lki.supertypes,
                lki.has_all_creature_types,
                lki.mana_value,
                lki.controller,
            )
        } else {
            (mtg_ir::SupertypeMask::empty(), false, 0, info.caster)
        };
    let counters: &[(mtg_ir::CounterId, u32)] = if let Some(id) = info.source_object
        && live_obj.is_some()
    {
        state.counters(id)
    } else if let Some(lki) = info.source_lki {
        lki.counters.as_slice()
    } else {
        &[]
    };
    let is_on_battlefield = live_obj.is_some_and(|o| o.zone == Zone::Battlefield)
        || (live_obj.is_none()
            && info
                .source_lki
                .is_some_and(|l| l.last_zone == Zone::Battlefield));
    let was_cast_this_turn = live_obj.is_some_and(|o| {
        !o.is_token()
            && o.flags.contains(
                crate::state::ObjectFlags::WAS_CAST | crate::state::ObjectFlags::ENTERED_THIS_TURN,
            )
    });
    let is_modified = if quality.to_ascii_lowercase().contains("modified") {
        info.source_object.is_some_and(|id| {
            crate::keywords::is_object_modified_for_protection(
                state,
                cache,
                id,
                info.source_lki,
                controller,
                counters,
            )
        })
    } else {
        false
    };
    let prot_ctx = crate::keywords::SourceProtContext {
        colors: info.source_colors,
        types: info.source_types,
        subtypes: &info.source_subtypes,
        supertypes,
        is_spell: info.is_spell,
        is_ability: info.is_ability,
        has_all_creature_types,
        mana_value,
        is_modified,
        was_cast_this_turn,
        is_on_battlefield,
        counters,
    };
    crate::keywords::source_matches_protection_quality_ctx(state.db(), quality, &prot_ctx)
}

struct ActiveCantBeTargeted<'a> {
    perm_id: ObjectId,
    perm_ctrl: PlayerId,
    affected: &'a Affected,
    opponent_controller_only: bool,
}

fn maybe_push_cant_be_targeted<'a>(
    state: &'a GameState,
    perm_id: ObjectId,
    perm_ctrl: PlayerId,
    static_def: &'a mtg_ir::StaticDef,
    info: &TargetSourceInfo<'_>,
    cache: &'a LayerCache,
    out: &mut SmallVec<[ActiveCantBeTargeted<'a>; 4]>,
) {
    let StaticBody::CantBeTargetedBy {
        affected,
        source_filter,
        spells_only,
        abilities_only,
        opponent_controller_only,
    } = &static_def.body
    else {
        return;
    };
    if *spells_only && !info.is_spell {
        return;
    }
    if *abilities_only && !info.is_ability {
        return;
    }
    let perm_ctx = EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
    if let Some(cond) = static_def.condition.as_deref()
        && !evaluate_condition(state, cond, &perm_ctx)
    {
        return;
    }
    if let Some(sf) = source_filter.as_deref() {
        let Some(src) = info.source_object else {
            return;
        };
        let src_ctx =
            EvalContext::for_source(info.caster, src).with_board(BoardView::Cached(cache));
        if !matches_object_filter(state, src, sf, &src_ctx) {
            return;
        }
    }
    out.push(ActiveCantBeTargeted {
        perm_id,
        perm_ctrl,
        affected,
        opponent_controller_only: *opponent_controller_only,
    });
}

fn collect_cant_be_targeted_sources<'a>(
    state: &'a GameState,
    info: &TargetSourceInfo<'_>,
    cache: &'a LayerCache,
) -> SmallVec<[ActiveCantBeTargeted<'a>; 4]> {
    let mut out = SmallVec::new();
    for player in state.players() {
        for &perm_id in &player.battlefield {
            let Some(perm_obj) = state.object(perm_id) else {
                continue;
            };
            if perm_obj.is_phased_out() {
                continue;
            }
            let perm_eval = cache.get(perm_id);
            let perm_ctrl = perm_eval.map_or(perm_obj.controller, |e| e.controller);
            let extras = cache.extra(perm_id);
            let lost_innate = perm_eval.is_some_and(|e| e.lost_all_abilities)
                || extras.is_some_and(|ex| ex.innate_abilities_removed);
            if !lost_innate && !perm_obj.is_face_down() {
                let perm_ctx = EvalContext::for_source(perm_ctrl, perm_id)
                    .with_board(BoardView::Cached(cache));
                if let Some(opts) = face_opts_for_object(state, perm_id)
                    && !opts.is_face_down
                    && let Some(view) = resolve_object_faces(state.db(), perm_obj.card_def, &opts)
                {
                    for_each_active_ability_with_card_on_faces(
                        state.db(),
                        perm_obj.card_def,
                        &opts,
                        &view.active_face_indices,
                        |cond| evaluate_condition(state, cond, &perm_ctx),
                        |ab| {
                            if let AbilityDef::Static(st) = ab.ability {
                                maybe_push_cant_be_targeted(
                                    state,
                                    perm_id,
                                    perm_ctrl,
                                    &st.static_def,
                                    info,
                                    cache,
                                    &mut out,
                                );
                            }
                        },
                    );
                }
            }
            if let Some(ex) = extras {
                for granted in &ex.granted_abilities {
                    match granted {
                        GrantedAbilityEntry::Inline {
                            ability,
                            grant_controller,
                            ..
                        } => {
                            if let AbilityDef::Static(st) = ability.as_ref() {
                                maybe_push_cant_be_targeted(
                                    state,
                                    perm_id,
                                    *grant_controller,
                                    &st.static_def,
                                    info,
                                    cache,
                                    &mut out,
                                );
                            }
                        }
                        GrantedAbilityEntry::CardAbility {
                            card_def,
                            face_index,
                            ability_index,
                            grant_controller,
                            ..
                        } => {
                            if let Some(card) = state.db().get(*card_def)
                                && let Some(face) = card.faces.get(usize::from(*face_index))
                                && let Some(AbilityDef::Static(st)) =
                                    face.abilities.get(*ability_index)
                            {
                                maybe_push_cant_be_targeted(
                                    state,
                                    perm_id,
                                    *grant_controller,
                                    &st.static_def,
                                    info,
                                    cache,
                                    &mut out,
                                );
                            }
                        }
                        GrantedAbilityEntry::IntrinsicBasicMana { .. }
                        | GrantedAbilityEntry::DynamicWard { .. } => {}
                    }
                }
            }
        }
    }
    out
}

fn matches_cant_be_targeted_list(
    state: &GameState,
    target_id: ObjectId,
    target_eval: &EvaluatedObject,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
) -> bool {
    if cant_be_targeted.is_empty() {
        return false;
    }
    for entry in cant_be_targeted {
        if entry.opponent_controller_only && target_eval.controller == info.caster {
            continue;
        }
        let affected_matches = match entry.affected {
            Affected::SelfObj => entry.perm_id == target_id,
            Affected::AttachedTo => state
                .object(entry.perm_id)
                .and_then(|o| o.attached_to)
                .is_some_and(|host| host == target_id),
            Affected::Filter { filter } => {
                let perm_ctx = EvalContext::for_source(entry.perm_ctrl, entry.perm_id)
                    .with_board(BoardView::Cached(cache));
                matches_object_filter(state, target_id, filter, &perm_ctx)
            }
        };
        if affected_matches {
            return true;
        }
    }
    false
}

fn object_blocked_by_hexproof_shroud_protection_cached(
    state: &GameState,
    target_id: ObjectId,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
) -> bool {
    let Some(obj) = state.object(target_id) else {
        return true;
    };
    if obj.zone != Zone::Battlefield {
        return false;
    }
    let Some(eval) = cache.get(target_id) else {
        return false;
    };
    let caster_ignores_hexproof = cache
        .player(info.caster)
        .is_some_and(|ep| ep.ignore_hexproof);
    let cant_have_hexproof = cache.extra(target_id).is_some_and(|ex| {
        ex.cant_have_keywords.iter().any(|&kw| {
            state
                .db()
                .keyword_name(kw)
                .is_some_and(|n| n.eq_ignore_ascii_case("hexproof"))
        })
    });

    for &kw in &eval.keywords {
        let Some(name) = state.db().keyword_name(kw) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower == "shroud" {
            return true;
        }
        if lower == "hexproof" {
            if eval.controller != info.caster && !caster_ignores_hexproof && !cant_have_hexproof {
                return true;
            }
        } else if let Some(qual) = lower.strip_prefix("hexproof from ")
            && eval.controller != info.caster
            && !caster_ignores_hexproof
            && !cant_have_hexproof
            && source_has_protection_quality(state, qual, info, cache)
        {
            return true;
        } else if let Some(qual) = lower.strip_prefix("protection from ")
            && source_has_protection_quality(state, qual, info, cache)
        {
            return true;
        }
    }

    if let Some(extras) = cache.extra(target_id) {
        for (kw, param) in &extras.parameterized_keywords {
            if extras.cant_have_keywords.contains(kw) {
                continue;
            }
            let Some(name) = state.db().keyword_name(*kw) else {
                continue;
            };
            if name.eq_ignore_ascii_case("protection") {
                if source_has_protection_quality(state, param, info, cache) {
                    return true;
                }
            } else if name.eq_ignore_ascii_case("hexproof")
                && eval.controller != info.caster
                && !caster_ignores_hexproof
                && !cant_have_hexproof
                && source_has_protection_quality(state, param, info, cache)
            {
                return true;
            }
        }
        if let Some((prot_colors, _)) = extras.protection_from_chosen_color
            && !(info.source_colors & prot_colors).is_empty()
        {
            return true;
        }
        if extras.protection_from_players.contains(&info.caster) {
            return true;
        }
        if !(info.source_types & extras.protection_from_card_types).is_empty() {
            return true;
        }
    }

    matches_cant_be_targeted_list(state, target_id, eval, info, cache, cant_be_targeted)
}

fn player_blocked_by_protection_cached(
    state: &GameState,
    target_player: PlayerId,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
) -> bool {
    let Some(p) = state.player(target_player) else {
        return true;
    };
    if p.lost {
        return true;
    }
    let caster_ignores_hexproof = cache
        .player(info.caster)
        .is_some_and(|ep| ep.ignore_hexproof);
    let Some(ep) = cache.player(target_player) else {
        return false;
    };
    if ep.protection_from_everything {
        return true;
    }
    if ep.shroud {
        return true;
    }
    if ep.hexproof && target_player != info.caster && !caster_ignores_hexproof {
        return true;
    }
    if !(info.source_types & ep.protection_from_card_types).is_empty() {
        return true;
    }
    false
}

fn candidate_satisfies_base_spec_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    candidate: TargetRef,
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
) -> bool {
    let scope = TargetEvalScope::new(info, Some(cache));
    let ctx = scope.ctx();
    candidate_satisfies_base_spec_with_ctx(
        state,
        spec,
        info,
        candidate,
        cache,
        cant_be_targeted,
        &ctx,
    )
}

fn candidate_satisfies_base_spec_with_ctx(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    candidate: TargetRef,
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
    ctx: &EvalContext<'_>,
) -> bool {
    let zm = effective_zone_mask(spec);
    match candidate {
        TargetRef::Object { id } => {
            if !matches!(spec.kind, TargetKind::Object | TargetKind::ObjectOrPlayer) {
                return false;
            }
            let Some(obj) = state.object(id) else {
                return false;
            };
            if !zm.contains(ZoneMask::from_zone(obj.zone)) {
                return false;
            }
            if obj.zone == Zone::Graveyard
                && cache
                    .player(obj.owner)
                    .is_some_and(|ep| ep.graveyard_cant_be_targeted)
            {
                return false;
            }
            if obj.zone == Zone::Stack {
                if let Some(se) = state
                    .stack()
                    .iter()
                    .position(|&s| s == id)
                    .and_then(|idx| state.stack_entries().get(idx))
                {
                    if !matches_stack_entry(state, se, &spec.filter, ctx) {
                        return false;
                    }
                    if spec.with_single_target && se.targets.len() != 1 {
                        return false;
                    }
                } else {
                    return false;
                }
            } else if !matches_object_filter(state, id, &spec.filter, ctx) {
                return false;
            }
            !(obj.zone == Zone::Battlefield
                && object_blocked_by_hexproof_shroud_protection_cached(
                    state,
                    id,
                    info,
                    cache,
                    cant_be_targeted,
                ))
        }
        TargetRef::Stack { id: stack_id } => {
            if !matches!(spec.kind, TargetKind::Object | TargetKind::ObjectOrPlayer) {
                return false;
            }
            if !zm.contains(ZoneMask::STACK) {
                return false;
            }
            let Some(se) = state.stack_entries().iter().find(|e| e.id == stack_id) else {
                return false;
            };
            if !matches_stack_entry(state, se, &spec.filter, ctx) {
                return false;
            }
            if spec.with_single_target && se.targets.len() != 1 {
                return false;
            }
            true
        }
        TargetRef::Player { id: pid } => {
            if !matches!(spec.kind, TargetKind::Player | TargetKind::ObjectOrPlayer) {
                return false;
            }
            if player_blocked_by_protection_cached(state, pid, info, cache) {
                return false;
            }
            if let Some(pf) = spec.player.as_ref() {
                matches_player_filter(state, pid, pf, ctx)
            } else {
                true
            }
        }
    }
}

fn ability_is_active_flagbearer(state: &GameState, ab: &AbilityDef, ctx: &EvalContext<'_>) -> bool {
    matches!(
        ab,
        AbilityDef::Static(st)
            if matches!(st.static_def.body, StaticBody::RuleModifier { rule: RuleMod::Flagbearer, .. })
            && st.static_def.condition.as_deref().is_none_or(|c| evaluate_condition(state, c, ctx))
    )
}

#[cfg(test)]
fn collect_flagbearer_objects(state: &GameState, caster: PlayerId) -> SmallVec<[ObjectId; 4]> {
    collect_flagbearer_objects_cached(state, caster, state.evaluate_board())
}

fn collect_flagbearer_objects_cached(
    state: &GameState,
    caster: PlayerId,
    cache: &LayerCache,
) -> SmallVec<[ObjectId; 4]> {
    let any_flagbearer = state
        .players()
        .iter()
        .any(|p| p.id != caster && cache.player(p.id).is_some_and(|ep| ep.flagbearer));
    if !any_flagbearer {
        return SmallVec::new();
    }
    let mut out = SmallVec::new();
    for player in state.players() {
        for &perm_id in &player.battlefield {
            let Some(obj) = state.object(perm_id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let eval = cache.get(perm_id);
            let perm_ctrl = eval.map_or(obj.controller, |e| e.controller);
            if perm_ctrl == caster {
                continue;
            }
            let extras = cache.extra(perm_id);
            let innate_removed = eval.is_some_and(|e| e.lost_all_abilities)
                || extras.is_some_and(|ex| ex.innate_abilities_removed);
            let ctx =
                EvalContext::for_source(perm_ctrl, perm_id).with_board(BoardView::Cached(cache));
            let mut is_flagbearer = false;
            if !innate_removed {
                let abilities = active_abilities_with_card_for_object(state, perm_id, &ctx);
                is_flagbearer = abilities
                    .iter()
                    .any(|ab| ability_is_active_flagbearer(state, ab.ability, &ctx));
            }
            if !is_flagbearer && let Some(ex) = extras {
                for granted in &ex.granted_abilities {
                    let hit = match granted {
                        GrantedAbilityEntry::CardAbility {
                            card_def,
                            face_index,
                            ability_index,
                            ..
                        } => state
                            .db()
                            .get(*card_def)
                            .and_then(|c| c.faces.get(usize::from(*face_index)))
                            .and_then(|f| f.abilities.get(*ability_index))
                            .is_some_and(|ab| ability_is_active_flagbearer(state, ab, &ctx)),
                        GrantedAbilityEntry::Inline { ability, .. } => {
                            ability_is_active_flagbearer(state, ability.as_ref(), &ctx)
                        }
                        GrantedAbilityEntry::IntrinsicBasicMana { .. }
                        | GrantedAbilityEntry::DynamicWard { .. } => false,
                    };
                    if hit {
                        is_flagbearer = true;
                        break;
                    }
                }
            }
            if is_flagbearer {
                out.push(perm_id);
            }
        }
    }
    out
}

fn target_owner(state: &GameState, t: TargetRef) -> Option<PlayerId> {
    match t {
        TargetRef::Object { id } => state.object(id).map(|o| o.owner),
        TargetRef::Player { id } => Some(id),
        TargetRef::Stack { id } => state
            .stack_entries()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.controller),
    }
}

fn target_controller(state: &GameState, t: TargetRef, ctx: &EvalContext<'_>) -> Option<PlayerId> {
    match t {
        TargetRef::Object { id } => effective_controller_for_object(state, id, ctx)
            .or_else(|| state.object(id).map(|o| o.controller)),
        TargetRef::Player { id } => Some(id),
        TargetRef::Stack { id } => state
            .stack_entries()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.controller),
    }
}

fn target_card_types(state: &GameState, t: TargetRef, ctx: &EvalContext<'_>) -> CardTypeMask {
    match t {
        TargetRef::Object { id } => object_types_subtypes(state, id, ctx).0,
        _ => CardTypeMask::empty(),
    }
}

fn target_mana_value(state: &GameState, t: TargetRef, ctx: &EvalContext<'_>) -> u32 {
    match t {
        TargetRef::Object { id } => object_mana_value(state, id, ctx),
        _ => 0,
    }
}

fn candidate_satisfies_different_player_per_mode(
    info: &TargetSourceInfo<'_>,
    spec_idx: usize,
    candidate: TargetRef,
    spec_mode_positions: Option<&[u16]>,
) -> bool {
    let Some(positions) = spec_mode_positions else {
        return true;
    };
    let TargetRef::Player { .. } = candidate else {
        return true;
    };
    let Some(&cur_pos) = positions.get(spec_idx) else {
        return true;
    };
    if cur_pos == u16::MAX {
        return true;
    }
    for (prior_si, prior_targets) in info.targets_by_spec.iter().enumerate() {
        if prior_si >= spec_idx {
            break;
        }
        let prior_pos = positions.get(prior_si).copied().unwrap_or(u16::MAX);
        if prior_pos != u16::MAX && prior_pos != cur_pos && prior_targets.contains(&candidate) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn candidate_satisfies_relational_constraints_with_modal_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    candidate: TargetRef,
    spec_idx: usize,
    spec_mode_positions: Option<&[u16]>,
    cache: &LayerCache,
) -> bool {
    if !candidate_satisfies_different_player_per_mode(
        info,
        spec_idx,
        candidate,
        spec_mode_positions,
    ) {
        return false;
    }
    candidate_satisfies_relational_constraints_cached(
        state,
        spec,
        info,
        current_spec_targets,
        candidate,
        cache,
    )
}

fn candidate_satisfies_relational_constraints_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    candidate: TargetRef,
    cache: &LayerCache,
) -> bool {
    let scope = TargetEvalScope::new(info, Some(cache));
    let ctx = scope.ctx();

    // CR 115.3: Same target cannot be chosen twice for the same TargetSpec.
    if current_spec_targets.contains(&candidate) {
        return false;
    }

    // CR 115.3: `other_target` excludes targets already chosen for earlier TargetSpecs.
    // Excluding the source object itself is handled separately by `FilterFlags::OTHER`.
    if spec.other_target
        && info
            .targets_by_spec
            .iter()
            .flatten()
            .any(|&t| t == candidate)
    {
        return false;
    }

    if spec.same_owner
        && let Some(&first) = current_spec_targets.first()
        && target_owner(state, first) != target_owner(state, candidate)
    {
        return false;
    }

    if spec.different_controllers {
        let Some(cand_ctrl) = target_controller(state, candidate, &ctx) else {
            return false;
        };
        for &prev in current_spec_targets {
            if target_controller(state, prev, &ctx) == Some(cand_ctrl) {
                return false;
            }
        }
    }

    if spec.per_opponent {
        let Some(cand_ctrl) = target_controller(state, candidate, &ctx) else {
            return false;
        };
        if cand_ctrl == info.caster {
            return false;
        }
        for &prev in current_spec_targets {
            if target_controller(state, prev, &ctx) == Some(cand_ctrl) {
                return false;
            }
        }
    }

    if let Some(ref_idx) = spec.shares_card_type_with_target {
        let Some(pt) = info
            .targets_by_spec
            .get(usize::from(ref_idx))
            .and_then(|v| v.first().copied())
        else {
            return false;
        };
        let cand_types = target_card_types(state, candidate, &ctx);
        let pt_types = target_card_types(state, pt, &ctx);
        if (cand_types & pt_types).is_empty() {
            return false;
        }
    }

    if let Some(max_mv) = spec.max_total_mana_value {
        let current_mv: u32 = current_spec_targets
            .iter()
            .map(|&t| target_mana_value(state, t, &ctx))
            .sum();
        let cand_mv = target_mana_value(state, candidate, &ctx);
        if current_mv.saturating_add(cand_mv) > u32::from(max_mv) {
            return false;
        }
    }

    true
}

fn raw_candidates_for_spec_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
) -> Vec<TargetRef> {
    let mut out = Vec::new();
    let zm = effective_zone_mask(spec);
    let scope = TargetEvalScope::new(info, Some(cache));
    let ctx = scope.ctx();

    if matches!(spec.kind, TargetKind::Object | TargetKind::ObjectOrPlayer) {
        if zm.contains(ZoneMask::BATTLEFIELD) {
            for p in state.players() {
                for &id in &p.battlefield {
                    let cand = TargetRef::Object { id };
                    if candidate_satisfies_base_spec_with_ctx(
                        state,
                        spec,
                        info,
                        cand,
                        cache,
                        cant_be_targeted,
                        &ctx,
                    ) {
                        out.push(cand);
                    }
                }
            }
        }
        if zm.contains(ZoneMask::STACK) {
            let mut seen_stack_ids: SmallVec<[StackItemId; 8]> = SmallVec::new();
            for se in state.stack_entries() {
                if seen_stack_ids.contains(&se.id) {
                    continue;
                }
                seen_stack_ids.push(se.id);
                let cand = match &se.kind {
                    StackEntryKind::Spell { object }
                        if spec.filter.stack_kind != Some(StackTargetKind::Ability) =>
                    {
                        TargetRef::Object { id: *object }
                    }
                    _ => TargetRef::Stack { id: se.id },
                };
                if candidate_satisfies_base_spec_with_ctx(
                    state,
                    spec,
                    info,
                    cand,
                    cache,
                    cant_be_targeted,
                    &ctx,
                ) && !out.contains(&cand)
                {
                    out.push(cand);
                }
            }
        }
        for (zone, getter) in [
            (
                Zone::Graveyard,
                (|p: &crate::state::PlayerState| p.graveyard.as_slice())
                    as fn(&crate::state::PlayerState) -> &[ObjectId],
            ),
            (Zone::Exile, |p| p.exile.as_slice()),
            (Zone::Hand, |p| p.hand.as_slice()),
            (Zone::Library, |p| p.library.as_slice()),
            (Zone::Command, |p| p.command.as_slice()),
        ] {
            if zm.contains(ZoneMask::from_zone(zone)) {
                for p in state.players() {
                    for &id in getter(p) {
                        let cand = TargetRef::Object { id };
                        if candidate_satisfies_base_spec_with_ctx(
                            state,
                            spec,
                            info,
                            cand,
                            cache,
                            cant_be_targeted,
                            &ctx,
                        ) {
                            out.push(cand);
                        }
                    }
                }
            }
        }
    }

    if matches!(spec.kind, TargetKind::Player | TargetKind::ObjectOrPlayer) {
        for p in state.players() {
            let cand = TargetRef::Player { id: p.id };
            if candidate_satisfies_base_spec_with_ctx(
                state,
                spec,
                info,
                cand,
                cache,
                cant_be_targeted,
                &ctx,
            ) {
                out.push(cand);
            }
        }
    }

    out
}

pub fn legal_targets_for_spec(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
) -> Vec<TargetRef> {
    legal_targets_for_spec_cached(
        state,
        spec,
        info,
        current_spec_targets,
        state.evaluate_board(),
    )
}

pub(crate) fn legal_targets_for_spec_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    cache: &LayerCache,
) -> Vec<TargetRef> {
    let (min, max) = effective_target_bounds_cached(state, spec, info, cache);
    if max == 0 || min > max || current_spec_targets.len() >= usize::from(max) {
        return Vec::new();
    }
    let cant_be_targeted = collect_cant_be_targeted_sources(state, info, cache);
    let raw = raw_candidates_for_spec_cached(state, spec, info, cache, &cant_be_targeted);
    let flagbearers = if info.is_spell || info.is_ability {
        collect_flagbearer_objects_cached(state, info.caster, cache)
    } else {
        SmallVec::new()
    };
    filter_legal_targets_from_raw(
        state,
        spec,
        info,
        current_spec_targets,
        max,
        &raw,
        cache,
        &flagbearers,
    )
}

#[allow(clippy::too_many_arguments)]
fn filter_legal_targets_from_raw(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    max: u16,
    raw: &[TargetRef],
    cache: &LayerCache,
    flagbearers: &[ObjectId],
) -> Vec<TargetRef> {
    filter_legal_targets_from_raw_with_modal(
        state,
        spec,
        info,
        current_spec_targets,
        max,
        raw,
        cache,
        flagbearers,
        info.targets_by_spec.len(),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn filter_legal_targets_from_raw_with_modal(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    max: u16,
    raw: &[TargetRef],
    cache: &LayerCache,
    flagbearers: &[ObjectId],
    spec_idx: usize,
    spec_mode_positions: Option<&[u16]>,
) -> Vec<TargetRef> {
    let mut filtered: Vec<TargetRef> = raw
        .iter()
        .copied()
        .filter(|&c| {
            candidate_satisfies_relational_constraints_with_modal_cached(
                state,
                spec,
                info,
                current_spec_targets,
                c,
                spec_idx,
                spec_mode_positions,
                cache,
            )
        })
        .collect();

    if (info.is_spell || info.is_ability) && !flagbearers.is_empty() {
        let already_picked_flagbearer = current_spec_targets
            .iter()
            .any(|t| matches!(t, TargetRef::Object { id } if flagbearers.contains(id)));
        if !already_picked_flagbearer {
            let legal_flagbearers: Vec<TargetRef> = filtered
                .iter()
                .copied()
                .filter(|t| matches!(t, TargetRef::Object { id } if flagbearers.contains(id)))
                .collect();
            if !legal_flagbearers.is_empty() {
                if current_spec_targets.len() + 1 >= usize::from(max) {
                    filtered = legal_flagbearers;
                } else {
                    filtered.retain(|&c| {
                        if legal_flagbearers.contains(&c) {
                            return true;
                        }
                        let mut next_picked = Vec::with_capacity(current_spec_targets.len() + 1);
                        next_picked.extend_from_slice(current_spec_targets);
                        next_picked.push(c);
                        legal_flagbearers.iter().any(|&fb| {
                            candidate_satisfies_relational_constraints_with_modal_cached(
                                state,
                                spec,
                                info,
                                &next_picked,
                                fb,
                                spec_idx,
                                spec_mode_positions,
                                cache,
                            )
                        })
                    });
                }
            }
        }
    }

    filtered
}

pub fn can_finish_spec_flagbearer_ok(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
) -> bool {
    can_finish_spec_flagbearer_ok_cached(
        state,
        spec,
        info,
        current_spec_targets,
        state.evaluate_board(),
    )
}

pub(crate) fn can_finish_spec_flagbearer_ok_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    cache: &LayerCache,
) -> bool {
    if !(info.is_spell || info.is_ability) || current_spec_targets.is_empty() {
        return true;
    }
    let flagbearers = collect_flagbearer_objects_cached(state, info.caster, cache);
    if flagbearers.is_empty() {
        return true;
    }
    let cant_be_targeted = collect_cant_be_targeted_sources(state, info, cache);
    can_finish_spec_flagbearer_ok_with(
        state,
        spec,
        info,
        current_spec_targets,
        cache,
        &cant_be_targeted,
        &flagbearers,
    )
}

fn can_finish_spec_flagbearer_ok_with(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    cache: &LayerCache,
    cant_be_targeted: &[ActiveCantBeTargeted<'_>],
    flagbearers: &[ObjectId],
) -> bool {
    if !(info.is_spell || info.is_ability)
        || current_spec_targets.is_empty()
        || flagbearers.is_empty()
    {
        return true;
    }
    if current_spec_targets
        .iter()
        .any(|t| matches!(t, TargetRef::Object { id } if flagbearers.contains(id)))
    {
        return true;
    }
    let (_min, max) = effective_target_bounds_cached(state, spec, info, cache);
    if current_spec_targets.len() >= usize::from(max) {
        return true;
    }
    !flagbearers.iter().any(|&fb| {
        let cand = TargetRef::Object { id: fb };
        candidate_satisfies_base_spec_cached(state, spec, info, cand, cache, cant_be_targeted)
            && candidate_satisfies_relational_constraints_cached(
                state,
                spec,
                info,
                current_spec_targets,
                cand,
                cache,
            )
    })
}

pub fn is_target_legal_for_spec(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    candidate: TargetRef,
) -> bool {
    is_target_legal_for_spec_cached(
        state,
        spec,
        info,
        current_spec_targets,
        candidate,
        state.evaluate_board(),
    )
}

pub(crate) fn is_target_legal_for_spec_cached(
    state: &GameState,
    spec: &TargetSpec,
    info: &TargetSourceInfo<'_>,
    current_spec_targets: &[TargetRef],
    candidate: TargetRef,
    cache: &LayerCache,
) -> bool {
    legal_targets_for_spec_cached(state, spec, info, current_spec_targets, cache)
        .contains(&candidate)
}

fn filter_may_depend_on_prior(filter: &ObjectFilter) -> bool {
    if filter.mana_value.is_some() || filter.power.is_some() || filter.toughness.is_some() {
        return true;
    }
    filter.extra.iter().any(predicate_may_depend_on_prior)
}

fn player_ref_may_depend_on_prior(pr: &mtg_ir::PlayerRef) -> bool {
    match pr {
        mtg_ir::PlayerRef::Target { .. }
        | mtg_ir::PlayerRef::ControllerOrControllerOfPlaneswalker { .. } => true,
        mtg_ir::PlayerRef::ControllerOf { of } | mtg_ir::PlayerRef::OwnerOf { of } => {
            matches!(of.as_ref(), mtg_ir::Ref::Target { .. })
        }
        mtg_ir::PlayerRef::You
        | mtg_ir::PlayerRef::ActivePlayer
        | mtg_ir::PlayerRef::EventPlayer
        | mtg_ir::PlayerRef::DefendingPlayer
        | mtg_ir::PlayerRef::ChosenOpponent
        | mtg_ir::PlayerRef::EachLoopPlayer
        | mtg_ir::PlayerRef::PlayerToYourRight => false,
    }
}

fn predicate_may_depend_on_prior(pred: &ObjectPredicate) -> bool {
    match pred {
        ObjectPredicate::ControllerOfTarget { .. }
        | ObjectPredicate::SameNameAsTarget { .. }
        | ObjectPredicate::PowerOrToughness { .. }
        | ObjectPredicate::BasePowerOrToughness { .. } => true,
        ObjectPredicate::ControllerRef { player } | ObjectPredicate::OwnerRef { player } => {
            player_ref_may_depend_on_prior(player)
        }
        ObjectPredicate::TargetsPlayer { player }
        | ObjectPredicate::SingleTargetPlayer { player } => {
            player.relation == PlayerRelation::Target
        }
        ObjectPredicate::AnyOf { filters } => filters.iter().any(filter_may_depend_on_prior),
        ObjectPredicate::TargetsFilter { filter }
        | ObjectPredicate::TargetsYouOrFilter { filter }
        | ObjectPredicate::SingleTargetFilter { filter }
        | ObjectPredicate::AbilitySourceFilter { filter } => filter_may_depend_on_prior(filter),
        _ => false,
    }
}

fn spec_may_depend_on_prior(spec: &TargetSpec) -> bool {
    spec.other_target
        || spec.shares_card_type_with_target.is_some()
        || spec.condition.is_some()
        || spec.dynamic_count.is_some()
        || spec.divide_total.is_some()
        || spec
            .player
            .as_ref()
            .is_some_and(|pf| pf.relation == PlayerRelation::Target)
        || filter_may_depend_on_prior(&spec.filter)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchOutcome {
    Found,
    IndependentLaterFailed,
    Backtrack,
}

struct TargetSearchShared<'a> {
    cache: &'a LayerCache,
    cant_be_targeted: &'a [ActiveCantBeTargeted<'a>],
    flagbearers: &'a [ObjectId],
    spec_independent: &'a [bool],
    later_independent: &'a [bool],
    spec_mode_positions: Option<&'a [u16]>,
}

/// Checks via bounded backtracking whether `specs` admits at least one legal target assignment.
pub fn has_legal_target_assignment(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
) -> bool {
    if specs.is_empty() {
        return true;
    }
    has_legal_target_assignment_cached(state, specs, info, state.evaluate_board())
}

pub(crate) fn has_legal_target_assignment_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    cache: &LayerCache,
) -> bool {
    has_legal_target_assignment_with_modal_cached(state, specs, info, None, cache)
}

pub(crate) fn has_legal_target_assignment_with_modal_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_mode_positions: Option<&[u16]>,
    cache: &LayerCache,
) -> bool {
    if specs.is_empty() {
        return true;
    }
    partial_targets_can_complete_from_cached(
        state,
        specs,
        info,
        0,
        &[],
        false,
        spec_mode_positions,
        cache,
    )
}

#[allow(dead_code)]
pub(crate) fn partial_targets_can_complete_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_cursor: usize,
    picked_for_cursor: &[TargetRef],
    finishing_cursor: bool,
    cache: &LayerCache,
) -> bool {
    partial_targets_can_complete_with_modal_cached(
        state,
        specs,
        info,
        spec_cursor,
        picked_for_cursor,
        finishing_cursor,
        None,
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn partial_targets_can_complete_with_modal_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_cursor: usize,
    picked_for_cursor: &[TargetRef],
    finishing_cursor: bool,
    spec_mode_positions: Option<&[u16]>,
    cache: &LayerCache,
) -> bool {
    if specs.is_empty() || spec_cursor >= specs.len() {
        return true;
    }
    if spec_mode_positions.is_some()
        && picked_for_cursor.iter().any(|&t| {
            !candidate_satisfies_different_player_per_mode(
                info,
                spec_cursor,
                t,
                spec_mode_positions,
            )
        })
    {
        return false;
    }
    if spec_cursor + 1 == specs.len() {
        let (min_req, max_req) =
            effective_target_bounds_cached(state, &specs[spec_cursor], info, cache);
        if min_req > max_req || picked_for_cursor.len() > usize::from(max_req) {
            return false;
        }
        if finishing_cursor {
            return picked_for_cursor.len() >= usize::from(min_req)
                && can_finish_spec_flagbearer_ok_cached(
                    state,
                    &specs[spec_cursor],
                    info,
                    picked_for_cursor,
                    cache,
                );
        }
        if picked_for_cursor.len() >= usize::from(min_req) {
            return true;
        }
    }
    partial_targets_can_complete_from_cached(
        state,
        specs,
        info,
        spec_cursor,
        picked_for_cursor,
        finishing_cursor,
        spec_mode_positions,
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
fn partial_targets_can_complete_from_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_cursor: usize,
    picked_for_cursor: &[TargetRef],
    finishing_cursor: bool,
    spec_mode_positions: Option<&[u16]>,
    cache: &LayerCache,
) -> bool {
    if specs.is_empty() || spec_cursor >= specs.len() {
        return true;
    }
    let cant_be_targeted = collect_cant_be_targeted_sources(state, info, cache);
    let flagbearers = if info.is_spell || info.is_ability {
        collect_flagbearer_objects_cached(state, info.caster, cache)
    } else {
        SmallVec::new()
    };

    let mut chosen_by_spec: Vec<Vec<TargetRef>> =
        if spec_cursor == 0 && picked_for_cursor.is_empty() && !finishing_cursor {
            info.targets_by_spec.to_vec()
        } else {
            info.targets_by_spec
                .iter()
                .take(spec_cursor)
                .cloned()
                .collect()
        };
    chosen_by_spec.resize(specs.len(), Vec::new());
    if !picked_for_cursor.is_empty() || finishing_cursor {
        chosen_by_spec[spec_cursor] = picked_for_cursor.to_vec();
    }

    let start_spec_idx = if finishing_cursor {
        let local_info = TargetSourceInfo {
            targets_by_spec: &chosen_by_spec[..spec_cursor],
            ..info.clone()
        };
        let (min_req, max_req) =
            effective_target_bounds_cached(state, &specs[spec_cursor], &local_info, cache);
        if picked_for_cursor.len() < usize::from(min_req)
            || picked_for_cursor.len() > usize::from(max_req)
            || !can_finish_spec_flagbearer_ok_with(
                state,
                &specs[spec_cursor],
                &local_info,
                &chosen_by_spec[spec_cursor],
                cache,
                &cant_be_targeted,
                &flagbearers,
            )
        {
            return false;
        }
        if spec_cursor + 1 >= specs.len() {
            return true;
        }
        spec_cursor + 1
    } else {
        spec_cursor
    };

    let spec_independent: SmallVec<[bool; 8]> = specs
        .iter()
        .enumerate()
        .map(|(si, s)| {
            !spec_may_depend_on_prior(s)
                && !spec_mode_positions.is_some_and(|pos| {
                    pos.get(si).copied().unwrap_or(u16::MAX) != u16::MAX
                        && s.kind != TargetKind::Object
                })
        })
        .collect();
    let mut later_independent: SmallVec<[bool; 8]> = SmallVec::with_capacity(specs.len());
    for i in 0..specs.len() {
        later_independent.push(spec_independent[i + 1..].iter().all(|&ind| ind));
    }

    let mut raw_by_spec: Vec<Option<Vec<TargetRef>>> = vec![None; specs.len()];
    for si in start_spec_idx..specs.len() {
        let spec = &specs[si];
        if spec_independent[si] && chosen_by_spec[si].is_empty() {
            let (min_req, max_req) = effective_target_bounds_cached(state, spec, info, cache);
            if min_req > max_req {
                return false;
            }
            if min_req > 0 {
                let raw =
                    raw_candidates_for_spec_cached(state, spec, info, cache, &cant_be_targeted);
                if raw.len() < usize::from(min_req) {
                    return false;
                }
                raw_by_spec[si] = Some(raw);
            }
        }
    }

    let shared = TargetSearchShared {
        cache,
        cant_be_targeted: &cant_be_targeted,
        flagbearers: &flagbearers,
        spec_independent: &spec_independent,
        later_independent: &later_independent,
        spec_mode_positions,
    };

    let mut remaining_nodes = 1024usize;
    search_target_assignment(
        state,
        specs,
        info,
        &shared,
        &mut raw_by_spec,
        start_spec_idx,
        &mut chosen_by_spec,
        0,
        &mut remaining_nodes,
    ) == SearchOutcome::Found
}

#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn filter_casting_target_choices_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_cursor: usize,
    picked: &[TargetRef],
    cands: Vec<TargetRef>,
    can_done_base: bool,
    cache: &LayerCache,
) -> (Vec<TargetRef>, bool) {
    filter_casting_target_choices_with_modal_cached(
        state,
        specs,
        info,
        spec_cursor,
        picked,
        cands,
        can_done_base,
        None,
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_casting_target_choices_with_modal_cached(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    spec_cursor: usize,
    picked: &[TargetRef],
    mut cands: Vec<TargetRef>,
    can_done_base: bool,
    spec_mode_positions: Option<&[u16]>,
    cache: &LayerCache,
) -> (Vec<TargetRef>, bool) {
    if spec_cursor >= specs.len() {
        return (cands, can_done_base);
    }
    if spec_mode_positions.is_some() {
        cands.retain(|&c| {
            candidate_satisfies_different_player_per_mode(info, spec_cursor, c, spec_mode_positions)
        });
    }
    let can_done_base = can_done_base
        && !(spec_mode_positions.is_some()
            && picked.iter().any(|&t| {
                !candidate_satisfies_different_player_per_mode(
                    info,
                    spec_cursor,
                    t,
                    spec_mode_positions,
                )
            }));
    let (min_req, max_req) =
        effective_target_bounds_cached(state, &specs[spec_cursor], info, cache);
    if min_req > max_req {
        return (Vec::new(), false);
    }
    if spec_cursor + 1 == specs.len() && picked.len() + 1 >= usize::from(min_req) {
        return (cands, can_done_base);
    }
    if cands.is_empty() && !can_done_base {
        return (cands, false);
    }

    let cant_be_targeted = collect_cant_be_targeted_sources(state, info, cache);
    let flagbearers = if info.is_spell || info.is_ability {
        collect_flagbearer_objects_cached(state, info.caster, cache)
    } else {
        SmallVec::new()
    };

    let spec_independent: SmallVec<[bool; 8]> = specs
        .iter()
        .enumerate()
        .map(|(si, s)| {
            !spec_may_depend_on_prior(s)
                && !spec_mode_positions.is_some_and(|pos| {
                    pos.get(si).copied().unwrap_or(u16::MAX) != u16::MAX
                        && s.kind != TargetKind::Object
                })
        })
        .collect();
    let mut later_independent: SmallVec<[bool; 8]> = SmallVec::with_capacity(specs.len());
    for i in 0..specs.len() {
        later_independent.push(spec_independent[i + 1..].iter().all(|&ind| ind));
    }

    let mut raw_by_spec: Vec<Option<Vec<TargetRef>>> = vec![None; specs.len()];
    for (si, spec) in specs.iter().enumerate().skip(spec_cursor + 1) {
        if spec_independent[si] {
            let (si_min, si_max) = effective_target_bounds_cached(state, spec, info, cache);
            if si_min > si_max {
                return (Vec::new(), false);
            }
            if si_min > 0 {
                let raw =
                    raw_candidates_for_spec_cached(state, spec, info, cache, &cant_be_targeted);
                if raw.len() < usize::from(si_min) {
                    return (Vec::new(), false);
                }
                raw_by_spec[si] = Some(raw);
            }
        }
    }

    let shared = TargetSearchShared {
        cache,
        cant_be_targeted: &cant_be_targeted,
        flagbearers: &flagbearers,
        spec_independent: &spec_independent,
        later_independent: &later_independent,
        spec_mode_positions,
    };

    let mut chosen_by_spec: Vec<Vec<TargetRef>> = info
        .targets_by_spec
        .iter()
        .take(spec_cursor)
        .cloned()
        .collect();
    chosen_by_spec.resize(specs.len(), Vec::new());
    chosen_by_spec[spec_cursor] = picked.to_vec();

    if later_independent[spec_cursor]
        && picked.len() + 1 >= usize::from(min_req)
        && picked.len() <= usize::from(max_req)
    {
        let mut remaining_nodes = 1024usize;
        let tail_ok = spec_cursor + 1 >= specs.len()
            || search_target_assignment(
                state,
                specs,
                info,
                &shared,
                &mut raw_by_spec,
                spec_cursor + 1,
                &mut chosen_by_spec,
                0,
                &mut remaining_nodes,
            ) == SearchOutcome::Found;
        return if tail_ok {
            (cands, can_done_base)
        } else {
            (Vec::new(), false)
        };
    }

    let can_done = can_done_base
        && (spec_cursor + 1 >= specs.len() || {
            let mut remaining_nodes = 1024usize;
            search_target_assignment(
                state,
                specs,
                info,
                &shared,
                &mut raw_by_spec,
                spec_cursor + 1,
                &mut chosen_by_spec,
                0,
                &mut remaining_nodes,
            ) == SearchOutcome::Found
        });

    let mut filtered_cands = Vec::with_capacity(cands.len());
    for t in cands {
        chosen_by_spec[spec_cursor].push(t);
        let mut remaining_nodes = 1024usize;
        let ok = search_target_assignment(
            state,
            specs,
            info,
            &shared,
            &mut raw_by_spec,
            spec_cursor,
            &mut chosen_by_spec,
            0,
            &mut remaining_nodes,
        ) == SearchOutcome::Found;
        chosen_by_spec[spec_cursor].pop();
        if ok {
            filtered_cands.push(t);
        }
    }

    (filtered_cands, can_done)
}

#[allow(clippy::too_many_arguments)]
fn search_target_assignment(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    shared: &TargetSearchShared<'_>,
    raw_by_spec: &mut [Option<Vec<TargetRef>>],
    spec_idx: usize,
    chosen_by_spec: &mut [Vec<TargetRef>],
    depth: usize,
    remaining_nodes: &mut usize,
) -> SearchOutcome {
    if spec_idx >= specs.len() {
        return SearchOutcome::Found;
    }
    if depth >= 32 || *remaining_nodes == 0 {
        return SearchOutcome::Backtrack;
    }
    *remaining_nodes -= 1;
    let spec = &specs[spec_idx];
    let picked_len = chosen_by_spec[spec_idx].len();
    let (_min_req, max_req, cands, can_finish) = {
        let local_info = TargetSourceInfo {
            targets_by_spec: &chosen_by_spec[..spec_idx],
            ..info.clone()
        };
        let (min_req, max_req) =
            effective_target_bounds_cached(state, spec, &local_info, shared.cache);
        if min_req > max_req {
            return SearchOutcome::Backtrack;
        }
        let cands = if picked_len < usize::from(max_req) {
            if shared.spec_independent[spec_idx] {
                let raw = raw_by_spec[spec_idx].get_or_insert_with(|| {
                    raw_candidates_for_spec_cached(
                        state,
                        spec,
                        &local_info,
                        shared.cache,
                        shared.cant_be_targeted,
                    )
                });
                filter_legal_targets_from_raw_with_modal(
                    state,
                    spec,
                    &local_info,
                    &chosen_by_spec[spec_idx],
                    max_req,
                    raw,
                    shared.cache,
                    shared.flagbearers,
                    spec_idx,
                    shared.spec_mode_positions,
                )
            } else {
                let raw = raw_candidates_for_spec_cached(
                    state,
                    spec,
                    &local_info,
                    shared.cache,
                    shared.cant_be_targeted,
                );
                filter_legal_targets_from_raw_with_modal(
                    state,
                    spec,
                    &local_info,
                    &chosen_by_spec[spec_idx],
                    max_req,
                    &raw,
                    shared.cache,
                    shared.flagbearers,
                    spec_idx,
                    shared.spec_mode_positions,
                )
            }
        } else {
            Vec::new()
        };
        let can_finish = picked_len >= usize::from(min_req)
            && can_finish_spec_flagbearer_ok_with(
                state,
                spec,
                &local_info,
                &chosen_by_spec[spec_idx],
                shared.cache,
                shared.cant_be_targeted,
                shared.flagbearers,
            );
        (min_req, max_req, cands, can_finish)
    };
    if can_finish {
        match search_target_assignment(
            state,
            specs,
            info,
            shared,
            raw_by_spec,
            spec_idx + 1,
            chosen_by_spec,
            depth + 1,
            remaining_nodes,
        ) {
            SearchOutcome::Found => return SearchOutcome::Found,
            SearchOutcome::IndependentLaterFailed => {
                return SearchOutcome::IndependentLaterFailed;
            }
            SearchOutcome::Backtrack => {
                if shared.later_independent[spec_idx] {
                    return SearchOutcome::IndependentLaterFailed;
                }
            }
        }
    }
    if picked_len >= usize::from(max_req) {
        return SearchOutcome::Backtrack;
    }
    for cand in cands {
        chosen_by_spec[spec_idx].push(cand);
        let outcome = search_target_assignment(
            state,
            specs,
            info,
            shared,
            raw_by_spec,
            spec_idx,
            chosen_by_spec,
            depth + 1,
            remaining_nodes,
        );
        chosen_by_spec[spec_idx].pop();
        match outcome {
            SearchOutcome::Found => return SearchOutcome::Found,
            SearchOutcome::IndependentLaterFailed => {
                return SearchOutcome::IndependentLaterFailed;
            }
            SearchOutcome::Backtrack => {}
        }
    }
    SearchOutcome::Backtrack
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectiveModalBounds {
    pub choose_all: bool,
    pub min: u16,
    pub max: u16,
    pub option_count: u16,
}

pub(crate) fn effective_modal_bounds_cached(
    state: &GameState,
    modal: &ModalDef,
    eval_ctx: &EvalContext<'_>,
) -> EffectiveModalBounds {
    let option_count = modal.modes.len() as u16;
    if let Some(cond) = modal.conditional_choose_all.as_deref()
        && evaluate_condition(state, cond, eval_ctx)
    {
        return EffectiveModalBounds {
            choose_all: true,
            min: option_count,
            max: option_count,
            option_count,
        };
    }
    let (mut min_m, mut max_m) = (modal.min, modal.max);
    if let Some(cmr) = &modal.conditional_choose_range
        && evaluate_condition(state, &cmr.condition, eval_ctx)
    {
        min_m = cmr.min;
        max_m = cmr.max;
    }
    if let Some(dmax) = modal.dynamic_max.as_deref() {
        max_m = evaluate_amount(state, dmax, eval_ctx).max(0) as u16;
    }
    if modal.up_to {
        min_m = 0;
    }
    if !modal.allow_repeat {
        max_m = max_m.min(option_count);
    }
    min_m = min_m.min(max_m);
    EffectiveModalBounds {
        choose_all: false,
        min: min_m,
        max: max_m,
        option_count,
    }
}

pub(crate) fn build_modal_target_specs(
    base_targets: &[TargetSpec],
    modal: &ModalDef,
    chosen_modes: &[u16],
) -> (Vec<TargetSpec>, Option<SmallVec<[u16; 8]>>) {
    let mut specs = base_targets.to_vec();
    let mut mode_positions: Option<SmallVec<[u16; 8]>> = if modal.different_player_targets_per_mode
    {
        Some(smallvec::smallvec![u16::MAX; base_targets.len()])
    } else {
        None
    };
    let mut ordered_modes: SmallVec<[u16; 8]> = chosen_modes.iter().copied().collect();
    ordered_modes.sort_unstable();
    for (mode_pos, &mode_idx) in ordered_modes.iter().enumerate() {
        if let Some(mode) = modal.modes.get(usize::from(mode_idx)) {
            specs.extend(mode.targets.iter().cloned());
            if let Some(positions) = mode_positions.as_mut() {
                for _ in 0..mode.targets.len() {
                    positions.push(mode_pos as u16);
                }
            }
        }
    }
    (specs, mode_positions)
}

fn pawprint_sum(modal: &ModalDef, chosen_modes: &[u16]) -> u32 {
    chosen_modes
        .iter()
        .map(|&idx| {
            u32::from(
                modal
                    .pawprint_costs
                    .get(usize::from(idx))
                    .copied()
                    .unwrap_or(0),
            )
        })
        .sum()
}

fn pawprint_budget_ok(modal: &ModalDef, chosen_modes: &[u16]) -> bool {
    modal
        .pawprint_budget
        .is_none_or(|budget| pawprint_sum(modal, chosen_modes) <= u32::from(budget))
}

#[allow(clippy::too_many_arguments)]
fn can_extend_mode_prefix<F>(
    state: &GameState,
    modal: &ModalDef,
    base_targets: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    bounds: EffectiveModalBounds,
    eligible_single: &[bool],
    current: &mut SmallVec<[u16; 8]>,
    min_next_idx: u16,
    cost_ok: &mut F,
    cache: &LayerCache,
    remaining_nodes: &mut usize,
) -> bool
where
    F: FnMut(&[u16]) -> bool,
{
    if !pawprint_budget_ok(modal, current) {
        return false;
    }
    if current.len() >= usize::from(bounds.min) {
        let (specs, pos) = build_modal_target_specs(base_targets, modal, current);
        if has_legal_target_assignment_with_modal_cached(state, &specs, info, pos.as_deref(), cache)
            && cost_ok(current)
        {
            return true;
        }
    }
    if current.len() >= usize::from(bounds.max) || *remaining_nodes == 0 {
        return false;
    }
    *remaining_nodes -= 1;

    for idx in min_next_idx..bounds.option_count {
        if !eligible_single[usize::from(idx)] {
            continue;
        }
        if !modal.allow_repeat && current.contains(&idx) {
            continue;
        }
        let next_min = if modal.allow_repeat { idx } else { idx + 1 };
        current.push(idx);
        let ok = can_extend_mode_prefix(
            state,
            modal,
            base_targets,
            info,
            bounds,
            eligible_single,
            current,
            next_min,
            cost_ok,
            cache,
            remaining_nodes,
        );
        current.pop();
        if ok {
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn legal_next_modes_and_done_cached<F>(
    state: &GameState,
    modal: &ModalDef,
    base_targets: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    bounds: EffectiveModalBounds,
    prefix: &[u16],
    mut cost_ok: F,
    cache: &LayerCache,
) -> (SmallVec<[u16; 8]>, bool)
where
    F: FnMut(&[u16]) -> bool,
{
    if bounds.choose_all {
        let all_modes: SmallVec<[u16; 8]> = (0..bounds.option_count).collect();
        let all_ok = pawprint_budget_ok(modal, &all_modes)
            && {
                let (specs, pos) = build_modal_target_specs(base_targets, modal, &all_modes);
                has_legal_target_assignment_with_modal_cached(
                    state,
                    &specs,
                    info,
                    pos.as_deref(),
                    cache,
                )
            }
            && cost_ok(&all_modes);
        if !all_ok {
            return (SmallVec::new(), false);
        }
        let mut next = SmallVec::new();
        for idx in 0..bounds.option_count {
            if !prefix.contains(&idx) {
                next.push(idx);
            }
        }
        let can_done = prefix.len() == usize::from(bounds.option_count);
        return (next, can_done);
    }

    let can_done = prefix.len() >= usize::from(bounds.min)
        && prefix.len() <= usize::from(bounds.max)
        && pawprint_budget_ok(modal, prefix)
        && {
            let (specs, pos) = build_modal_target_specs(base_targets, modal, prefix);
            has_legal_target_assignment_with_modal_cached(
                state,
                &specs,
                info,
                pos.as_deref(),
                cache,
            )
        }
        && cost_ok(prefix);

    if prefix.len() >= usize::from(bounds.max) || bounds.option_count == 0 {
        return (SmallVec::new(), can_done);
    }

    let mut eligible_single: SmallVec<[bool; 8]> =
        SmallVec::with_capacity(usize::from(bounds.option_count));
    for idx in 0..bounds.option_count {
        let single = [idx];
        let ok = pawprint_budget_ok(modal, &single) && {
            let (specs, pos) = build_modal_target_specs(base_targets, modal, &single);
            has_legal_target_assignment_with_modal_cached(
                state,
                &specs,
                info,
                pos.as_deref(),
                cache,
            )
        };
        eligible_single.push(ok);
    }

    let mut legal_next = SmallVec::new();
    for idx in 0..bounds.option_count {
        if (!modal.allow_repeat && prefix.contains(&idx)) || !eligible_single[usize::from(idx)] {
            continue;
        }
        let mut ext: SmallVec<[u16; 8]> = SmallVec::from_slice(prefix);
        ext.push(idx);
        let mut nodes = 256usize;
        if can_extend_mode_prefix(
            state,
            modal,
            base_targets,
            info,
            bounds,
            &eligible_single,
            &mut ext,
            0,
            &mut cost_ok,
            cache,
            &mut nodes,
        ) {
            legal_next.push(idx);
        }
    }

    (legal_next, can_done)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn has_legal_modal_target_assignment_cached<F>(
    state: &GameState,
    base_targets: &[TargetSpec],
    modal: Option<&ModalDef>,
    info: &TargetSourceInfo<'_>,
    eval_ctx: &EvalContext<'_>,
    mut cost_ok: F,
    cache: &LayerCache,
) -> bool
where
    F: FnMut(&[u16]) -> bool,
{
    let Some(m) = modal else {
        return has_legal_target_assignment_cached(state, base_targets, info, cache)
            && cost_ok(&[]);
    };
    let bounds = effective_modal_bounds_cached(state, m, eval_ctx);
    if bounds.choose_all {
        let all_modes: SmallVec<[u16; 8]> = (0..bounds.option_count).collect();
        let (specs, pos) = build_modal_target_specs(base_targets, m, &all_modes);
        return pawprint_budget_ok(m, &all_modes)
            && has_legal_target_assignment_with_modal_cached(
                state,
                &specs,
                info,
                pos.as_deref(),
                cache,
            )
            && cost_ok(&all_modes);
    }
    if bounds.min == 0 {
        if has_legal_target_assignment_cached(state, base_targets, info, cache) && cost_ok(&[]) {
            return true;
        }
        if bounds.max == 0 || bounds.option_count == 0 {
            return false;
        }
        let (next, can_done) = legal_next_modes_and_done_cached(
            state,
            m,
            base_targets,
            info,
            bounds,
            &[],
            cost_ok,
            cache,
        );
        return can_done || !next.is_empty();
    }
    if bounds.max == 0 || bounds.option_count == 0 {
        return false;
    }
    let (next, can_done) =
        legal_next_modes_and_done_cached(state, m, base_targets, info, bounds, &[], cost_ok, cache);
    can_done || !next.is_empty()
}

/// Re-validates previously chosen targets on resolution (`CR 608.2b`).
pub fn revalidate_targets(
    state: &GameState,
    specs: &[TargetSpec],
    info: &TargetSourceInfo<'_>,
    chosen_by_spec: &[Vec<TargetRef>],
) -> TargetRevalidation {
    let cache = state.evaluate_board();
    let cant_be_targeted = collect_cant_be_targeted_sources(state, info, cache);
    let mut total_count = 0usize;
    let mut legal_count = 0usize;

    for (si, chosen) in chosen_by_spec.iter().enumerate() {
        let Some(spec) = specs.get(si) else {
            total_count += chosen.len();
            continue;
        };
        let local_info = TargetSourceInfo {
            targets_by_spec: &chosen_by_spec[..si],
            ..info.clone()
        };
        let mut accepted: Vec<TargetRef> = Vec::new();
        for &t in chosen {
            total_count += 1;
            if candidate_satisfies_base_spec_cached(
                state,
                spec,
                &local_info,
                t,
                cache,
                &cant_be_targeted,
            ) && candidate_satisfies_relational_constraints_cached(
                state,
                spec,
                &local_info,
                &accepted,
                t,
                cache,
            ) {
                accepted.push(t);
                legal_count += 1;
            }
        }
    }

    if total_count == 0 || legal_count == total_count {
        TargetRevalidation::AllLegal
    } else if legal_count == 0 {
        TargetRevalidation::AllIllegal
    } else {
        TargetRevalidation::PartiallyIllegal {
            legal_count,
            total_count,
        }
    }
}

/// Returns Ward generic/mana cost on `target` if `target` is an opponent's permanent with Ward
/// and `caster` does not suppress ward triggers.
pub fn target_ward_cost(
    state: &GameState,
    target: TargetRef,
    caster: PlayerId,
) -> Option<(u32, Option<ManaCost>, Option<ObjectId>)> {
    let TargetRef::Object { id } = target else {
        return None;
    };
    let obj = state.object(id)?;
    if obj.zone != Zone::Battlefield {
        return None;
    }
    let cache = state.evaluate_board();
    if cache
        .player(caster)
        .is_some_and(|ep| ep.suppress_ward_triggers)
    {
        return None;
    }
    let eval = cache.get(id)?;
    if eval.controller == caster {
        return None;
    }
    if let Some(extras) = cache.extra(id) {
        for g in &extras.granted_abilities {
            if let crate::layers::predicates::GrantedAbilityEntry::DynamicWard {
                generic,
                grant_source,
            } = g
                && *generic > 0
            {
                return Some((*generic, None, *grant_source));
            }
        }
    }
    let ctx = EvalContext::for_source(eval.controller, id).with_board(BoardView::Cached(cache));
    let abilities = active_abilities_with_card_for_object(state, id, &ctx);
    for ab in abilities {
        if let AbilityDef::Keyword(k) = ab.ability
            && keyword_name_matches(state.db(), k.keyword.keyword, "ward")
        {
            if let Some(cost) = &k.keyword.cost {
                return Some((cost.mana_value(), Some(cost.clone()), Some(id)));
            }
            if let Some(n) = k.keyword.n
                && n > 0
            {
                return Some((n as u32, None, Some(id)));
            }
        }
    }
    None
}

pub fn normalize_object_filter_zones(filter: &ObjectFilter, default_zones: ZoneMask) -> ZoneMask {
    if filter.zones.is_empty() {
        default_zones
    } else {
        filter.zones
    }
}

pub fn _unused_relative_control(_r: RelativeControl) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::zones::{MoveRawOptions, move_object};
    use mtg_ir::{
        AbilityDef, CardDatabase, CardDef, CardDefId, CardLayout, Characteristics, ColorMask,
        FaceDef, FilterFlags, IR_V2_VERSION, InternTables, IrV2Bundle, KeywordAbility, KeywordDef,
        KeywordId, ManaCost, ObjectFilter, PrintedPt, StaticAbility, StaticDef, SupertypeMask,
    };
    use std::sync::Arc;

    fn build_target_test_db() -> (
        Arc<CardDatabase>,
        CardDefId,
        CardDefId,
        CardDefId,
        CardDefId,
        CardDefId,
    ) {
        let mut tables = InternTables::default();
        tables.keywords.push("hexproof".into());
        let kw_hexproof = KeywordId(0);
        tables.keywords.push("shroud".into());
        let kw_shroud = KeywordId(1);
        tables.keywords.push("protection from red".into());
        let kw_prot_red = KeywordId(2);

        let plain_creature = CardDef {
            oracle_id: "t_plain".into(),
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

        let hexproof_creature = CardDef {
            oracle_id: "t_hex".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Stalker".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Stalker".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{U}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_hexproof,
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
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let shroud_creature = CardDef {
            oracle_id: "t_shroud".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Blur".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Blur".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{U}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_shroud,
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
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let prot_red_creature = CardDef {
            oracle_id: "t_prot".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Paladin".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Paladin".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{W}{W}"),
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
                abilities: vec![AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kw_prot_red,
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
                })],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let flagbearer_creature = CardDef {
            oracle_id: "t_flag".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Banneret".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Banneret".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{W}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::W,
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
                    plain_creature,
                    hexproof_creature,
                    shroud_creature,
                    prot_red_creature,
                    flagbearer_creature,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        (
            db,
            CardDefId(0),
            CardDefId(1),
            CardDefId(2),
            CardDefId(3),
            CardDefId(4),
        )
    }

    fn move_one_to_bf(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let id = state.players()[player.0 as usize]
            .hand
            .iter()
            .chain(state.players()[player.0 as usize].library.iter())
            .copied()
            .find(|&oid| state.object(oid).is_some_and(|o| o.card_def == def))
            .unwrap();
        match move_object(
            state,
            id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            other => panic!("expected Moved, got {other:?}"),
        }
    }

    #[test]
    fn hexproof_shroud_protection_and_cr_115_3_enforced() {
        let (db, plain, hex, shroud, prot_red, _flag) = build_target_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                plain, plain, hex, shroud, prot_red, plain, plain, plain, plain, plain,
            ],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap();

        let p0_plain = move_one_to_bf(&mut state, PlayerId(0), plain);
        let p1_plain = move_one_to_bf(&mut state, PlayerId(1), plain);
        let p1_hex = move_one_to_bf(&mut state, PlayerId(1), hex);
        let p1_shroud = move_one_to_bf(&mut state, PlayerId(1), shroud);
        let p1_prot = move_one_to_bf(&mut state, PlayerId(1), prot_red);

        let red_spell_info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::R,
            source_types: CardTypeMask::INSTANT,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };

        let spec = TargetSpec {
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
        };

        let legal = legal_targets_for_spec(&state, &spec, &red_spell_info, &[]);
        assert!(legal.contains(&TargetRef::Object { id: p0_plain }));
        assert!(legal.contains(&TargetRef::Object { id: p1_plain }));
        assert!(!legal.contains(&TargetRef::Object { id: p1_hex }));
        assert!(!legal.contains(&TargetRef::Object { id: p1_shroud }));
        assert!(!legal.contains(&TargetRef::Object { id: p1_prot }));

        // CR 115.3: already-chosen target cannot be chosen again in the same TargetSpec
        let after_one = legal_targets_for_spec(
            &state,
            &spec,
            &red_spell_info,
            &[TargetRef::Object { id: p1_plain }],
        );
        assert!(!after_one.contains(&TargetRef::Object { id: p1_plain }));
        assert!(after_one.contains(&TargetRef::Object { id: p0_plain }));

        // Revalidate fizzle check
        let reval = revalidate_targets(
            &state,
            &[spec],
            &red_spell_info,
            &[vec![TargetRef::Object { id: p1_prot }]],
        );
        assert_eq!(reval, TargetRevalidation::AllIllegal);
    }

    #[test]
    fn flagbearer_enforces_mandatory_targeting_when_legal() {
        let (db, plain, _hex, _shroud, _prot, flag) = build_target_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                plain, plain, flag, plain, plain, plain, plain, plain, plain, plain,
            ],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 17).unwrap();

        let _p1_plain = move_one_to_bf(&mut state, PlayerId(1), plain);
        let p1_flag = move_one_to_bf(&mut state, PlayerId(1), flag);

        let info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::R,
            source_types: CardTypeMask::INSTANT,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };
        let spec = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                flags: FilterFlags::empty(),
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
        };
        let legal = legal_targets_for_spec(&state, &spec, &info, &[]);
        assert_eq!(legal, vec![TargetRef::Object { id: p1_flag }]);
    }

    #[test]
    fn flagbearer_min_lt_max_cannot_finish_without_flagbearer_and_handles_granted_or_lost() {
        use mtg_ir::{Layer, LayerMod};

        let (base_db, plain, _hex, _shroud, _prot, flag) = build_target_test_db();
        let mut cards = base_db.cards.clone();
        // Card 5: enchantment granting Flagbearer to opponent's creatures
        let grant_flag_def = CardDefId(cards.len() as u32);
        cards.push(CardDef {
            oracle_id: "t_grant_flag".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Banner Aura".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Banner Aura".into(),
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
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    controller: Some(RelativeControl::You),
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(AbilityDef::Static(StaticAbility {
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
        });

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: base_db.tables.clone(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 10],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                plain,
                flag,
                grant_flag_def,
                plain,
                plain,
                plain,
                plain,
                plain,
                plain,
                plain,
            ],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[p0_deck, p1_deck], 29).unwrap();

        let p1_plain = move_one_to_bf(&mut state, PlayerId(1), plain);
        let p1_flag = move_one_to_bf(&mut state, PlayerId(1), flag);

        let info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::R,
            source_types: CardTypeMask::INSTANT,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };
        let mut spec = TargetSpec {
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
        };

        // At slot 0, both p1_plain and p1_flag are legal.
        let slot0 = legal_targets_for_spec(&state, &spec, &info, &[]);
        assert!(slot0.contains(&TargetRef::Object { id: p1_plain }));
        assert!(slot0.contains(&TargetRef::Object { id: p1_flag }));

        // If p1_plain is chosen first, cannot finish yet (min < max Flagbearer bypass blocked)!
        assert!(!can_finish_spec_flagbearer_ok(
            &state,
            &spec,
            &info,
            &[TargetRef::Object { id: p1_plain }]
        ));
        assert_eq!(
            legal_targets_for_spec(&state, &spec, &info, &[TargetRef::Object { id: p1_plain }]),
            vec![TargetRef::Object { id: p1_flag }]
        );
        // Once p1_flag is chosen, can finish!
        assert!(can_finish_spec_flagbearer_ok(
            &state,
            &spec,
            &info,
            &[TargetRef::Object { id: p1_flag }]
        ));

        // With different_controllers = true, p1_plain must be excluded at slot 0 so p1_flag isn't locked out.
        spec.different_controllers = true;
        assert_eq!(
            legal_targets_for_spec(&state, &spec, &info, &[]),
            vec![TargetRef::Object { id: p1_flag }]
        );

        // Move p1_flag off battlefield and grant Flagbearer via Layer 6 Continuous effect instead.
        let _ = move_object(
            &mut state,
            p1_flag,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let _granter = move_one_to_bf(&mut state, PlayerId(1), grant_flag_def);
        assert_eq!(
            collect_flagbearer_objects(&state, PlayerId(0)).as_slice(),
            &[p1_plain]
        );
    }

    #[test]
    fn cant_be_targeted_by_variants_and_independent_spec_fail_fast() {
        let (base_db, plain, _hex, _shroud, _prot, _flag) = build_target_test_db();
        let mut cards = base_db.cards.clone();

        // Card 5: Creature with CantBeTargetedBy (spells_only: true, opponent_controller_only: true)
        let spell_shield_def = CardDefId(cards.len() as u32);
        cards.push(CardDef {
            oracle_id: "t_cbt_spell".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Wardbearer".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Wardbearer".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{U}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
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
                        body: StaticBody::CantBeTargetedBy {
                            affected: Affected::SelfObj,
                            source_filter: None,
                            spells_only: true,
                            abilities_only: false,
                            opponent_controller_only: true,
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
        });

        // Card 6: Artifact (noncreature permanent)
        let artifact_def = CardDefId(cards.len() as u32);
        cards.push(CardDef {
            oracle_id: "t_art".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Stone".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Stone".into(),
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        });

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: base_db.tables.clone(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 25],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                spell_shield_def,
                artifact_def,
                plain,
                plain,
                plain,
                plain,
                plain,
                plain,
                plain,
                plain,
            ],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[p0_deck, p1_deck], 53).unwrap();

        for _ in 0..18 {
            let _ = move_one_to_bf(&mut state, PlayerId(0), plain);
        }
        let p1_shield = move_one_to_bf(&mut state, PlayerId(1), spell_shield_def);
        let p1_art = move_one_to_bf(&mut state, PlayerId(1), artifact_def);

        let spell_info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::G,
            source_types: CardTypeMask::INSTANT,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };
        let ability_info = TargetSourceInfo {
            is_spell: false,
            is_ability: true,
            ..spell_info.clone()
        };

        let opp_creature_spec = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                controller: Some(RelativeControl::Opponent),
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
        };

        // Spell cannot target p1_shield (spells_only + opponent_controller_only), but ability CAN
        assert!(
            !legal_targets_for_spec(&state, &opp_creature_spec, &spell_info, &[])
                .contains(&TargetRef::Object { id: p1_shield })
        );
        assert!(
            legal_targets_for_spec(&state, &opp_creature_spec, &ability_info, &[])
                .contains(&TargetRef::Object { id: p1_shield })
        );

        // Two-spec independent fail-fast: spec0 matches 18 P0 creatures, spec1 matches 0 opponent creatures -> false
        let you_creature_spec = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::CREATURE,
                controller: Some(RelativeControl::You),
                ..Default::default()
            },
            ..opp_creature_spec.clone()
        };
        assert!(!has_legal_target_assignment(
            &state,
            &[you_creature_spec.clone(), opp_creature_spec.clone()],
            &spell_info,
        ));
        // With ability_info, p1_shield is legal for spec1 -> true!
        assert!(has_legal_target_assignment(
            &state,
            &[you_creature_spec.clone(), opp_creature_spec],
            &ability_info,
        ));

        // Dependent spec (other_target: true) still works accurately when spec1 targets any permanent except spec0
        let _ = p1_art;
        let other_perm_spec = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::ARTIFACT,
                ..Default::default()
            },
            player: None,
            min: 1,
            max: 1,
            same_owner: false,
            condition: None,
            other_target: true,
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
        assert!(has_legal_target_assignment(
            &state,
            &[you_creature_spec, other_perm_spec],
            &spell_info,
        ));
    }

    #[test]
    fn gaea_blessing_and_target_ref_predicates_depend_on_prior_targets() {
        let (db, plain, _, _, _, _) = build_target_test_db();
        let d0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 10],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[d0.clone(), d0], 91).unwrap();

        let spell_obj = state.players()[0].hand[0];
        let p1_card = state.players()[1].hand[0];
        let p1_gy_card = match move_object(
            &mut state,
            p1_card,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            other => panic!("unexpected MoveObjectOutcome: {other:?}"),
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

        let spec0 = TargetSpec {
            kind: TargetKind::Player,
            player: Some(mtg_ir::PlayerFilter {
                relation: PlayerRelation::Any,
            }),
            min: 1,
            max: 1,
            zones: ZoneMask::empty(),
            ..base_spec.clone()
        };
        let spec1_min1 = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                extra: vec![ObjectPredicate::ControllerRef {
                    player: mtg_ir::PlayerRef::Target { index: 0 },
                }],
                ..Default::default()
            },
            min: 1,
            max: 3,
            same_owner: true,
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };

        let empty_chosen: [Vec<TargetRef>; 0] = [];
        let info =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &empty_chosen);
        assert!(spec_may_depend_on_prior(&spec1_min1));
        assert!(has_legal_target_assignment(
            &state,
            &[spec0.clone(), spec1_min1.clone()],
            &info
        ));

        let chosen_p0 = [vec![TargetRef::Player { id: PlayerId(0) }]];
        let info_p0 =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &chosen_p0);
        assert!(legal_targets_for_spec(&state, &spec1_min1, &info_p0, &[]).is_empty());

        let chosen_p1 = [vec![TargetRef::Player { id: PlayerId(1) }]];
        let info_p1 =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &chosen_p1);
        assert_eq!(
            legal_targets_for_spec(&state, &spec1_min1, &info_p1, &[]),
            vec![TargetRef::Object { id: p1_gy_card }]
        );

        let reval = revalidate_targets(
            &state,
            &[spec0.clone(), spec1_min1],
            &info,
            &[
                vec![TargetRef::Player { id: PlayerId(1) }],
                vec![TargetRef::Object { id: p1_gy_card }],
            ],
        );
        assert_eq!(reval, TargetRevalidation::AllLegal);

        // Exact Gaea's Blessing shape (min: 0, max: 3)
        let spec1_min0 = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                extra: vec![ObjectPredicate::ControllerRef {
                    player: mtg_ir::PlayerRef::Target { index: 0 },
                }],
                ..Default::default()
            },
            min: 0,
            max: 3,
            same_owner: true,
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };
        assert!(has_legal_target_assignment(
            &state,
            &[spec0.clone(), spec1_min0.clone()],
            &info
        ));
        assert!(legal_targets_for_spec(&state, &spec1_min0, &info_p0, &[]).is_empty());
        assert_eq!(
            legal_targets_for_spec(&state, &spec1_min0, &info_p1, &[]),
            vec![TargetRef::Object { id: p1_gy_card }]
        );

        // Rite of Renewal shape: spec0 (min=0, max=2 in your GY) -> spec1 (player) -> spec2 (ControllerRef(Target{1}), min=1)
        let rite_spec0 = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                controller: Some(RelativeControl::You),
                ..Default::default()
            },
            min: 0,
            max: 2,
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };
        let rite_spec2 = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                extra: vec![ObjectPredicate::ControllerRef {
                    player: mtg_ir::PlayerRef::Target { index: 1 },
                }],
                ..Default::default()
            },
            min: 1,
            max: 4,
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };
        assert!(has_legal_target_assignment(
            &state,
            &[rite_spec0, spec0.clone(), rite_spec2.clone()],
            &info
        ));
        let rite_prior = [vec![], vec![TargetRef::Player { id: PlayerId(1) }]];
        let info_rite =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &rite_prior);
        assert_eq!(
            legal_targets_for_spec(&state, &rite_spec2, &info_rite, &[]),
            vec![TargetRef::Object { id: p1_gy_card }]
        );

        // OwnerRef(Target), OwnerRef(ControllerOf(Ref::Target)), TargetsPlayer(Target), SingleTargetPlayer(Target)
        let owner_target_spec = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                extra: vec![ObjectPredicate::OwnerRef {
                    player: mtg_ir::PlayerRef::Target { index: 0 },
                }],
                ..Default::default()
            },
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };
        assert!(spec_may_depend_on_prior(&owner_target_spec));
        assert!(has_legal_target_assignment(
            &state,
            &[spec0.clone(), owner_target_spec],
            &info
        ));

        let p1_bf_card = state.players()[1].hand[1];
        let p1_bf = match move_object(
            &mut state,
            p1_bf_card,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            other => panic!("unexpected MoveObjectOutcome: {other:?}"),
        };
        let any_perm_spec = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                ..Default::default()
            },
            zones: ZoneMask::BATTLEFIELD,
            ..base_spec.clone()
        };
        let owner_ctrl_of_target_spec = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                extra: vec![ObjectPredicate::OwnerRef {
                    player: mtg_ir::PlayerRef::ControllerOf {
                        of: Box::new(mtg_ir::Ref::Target { index: 0 }),
                    },
                }],
                ..Default::default()
            },
            zones: ZoneMask::GRAVEYARD,
            ..base_spec.clone()
        };
        assert!(spec_may_depend_on_prior(&owner_ctrl_of_target_spec));
        let info_after_bf =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &[]);
        assert!(has_legal_target_assignment(
            &state,
            &[any_perm_spec, owner_ctrl_of_target_spec],
            &info_after_bf
        ));
        let _ = p1_bf;

        let targets_player_spec = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::STACK,
                extra: vec![ObjectPredicate::TargetsPlayer {
                    player: mtg_ir::PlayerFilter {
                        relation: PlayerRelation::Target,
                    },
                }],
                ..Default::default()
            },
            zones: ZoneMask::STACK,
            ..base_spec.clone()
        };
        let single_target_player_spec = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::STACK,
                extra: vec![ObjectPredicate::SingleTargetPlayer {
                    player: mtg_ir::PlayerFilter {
                        relation: PlayerRelation::Target,
                    },
                }],
                ..Default::default()
            },
            zones: ZoneMask::STACK,
            ..base_spec
        };
        assert!(spec_may_depend_on_prior(&targets_player_spec));
        assert!(spec_may_depend_on_prior(&single_target_player_spec));
    }

    #[test]
    fn shares_card_type_with_target_does_not_cross_spec_boundaries_when_prior_empty() {
        let tables = InternTables::default();
        let gy_creature = CardDef {
            oracle_id: "t_gy_cr".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Creature".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth GY Creature".into(),
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
        let gy_artifact = CardDef {
            oracle_id: "t_gy_art".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth GY Artifact".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth GY Artifact".into(),
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let heist_spell = CardDef {
            oracle_id: "t_heist".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Heist Spell".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Heist Spell".into(),
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
                cards: vec![gy_creature, gy_artifact, heist_spell],
                decks: vec![],
            })
            .unwrap(),
        );
        let d0 = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(2),
                CardDefId(1),
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
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[d0.clone(), d0], 42).unwrap();

        let spell_obj = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();

        let move_to_gy = |st: &mut GameState, def: CardDefId| -> ObjectId {
            let id = st.players()[0]
                .hand
                .iter()
                .chain(st.players()[0].library.iter())
                .copied()
                .find(|&oid| st.object(oid).unwrap().card_def == def)
                .unwrap();
            match move_object(st, id, Zone::Graveyard, MoveRawOptions::default(), None).unwrap() {
                crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
                other => panic!("unexpected MoveObjectOutcome: {other:?}"),
            }
        };

        let art_gy1 = move_to_gy(&mut state, CardDefId(1));
        let art_gy2 = move_to_gy(&mut state, CardDefId(1));

        let base_spec = TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
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
            zones: ZoneMask::GRAVEYARD,
        };
        let spec_share0 = TargetSpec {
            shares_card_type_with_target: Some(0),
            ..base_spec.clone()
        };
        let spec_share1 = TargetSpec {
            shares_card_type_with_target: Some(1),
            ..base_spec.clone()
        };

        // 1. Empty spec 0 + populated spec 1 -> shares_card_type_with_target(0) MUST return empty (not borrow art_gy1 from spec 1)
        let prior_empty0 = [vec![], vec![TargetRef::Object { id: art_gy1 }]];
        let info_empty0 =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &prior_empty0);
        assert!(
            legal_targets_for_spec(&state, &spec_share0, &info_empty0, &[]).is_empty(),
            "must not borrow art_gy1 from spec 1 when spec 0 is empty"
        );
        assert_eq!(
            legal_targets_for_spec(&state, &spec_share1, &info_empty0, &[]),
            vec![
                TargetRef::Object { id: art_gy1 },
                TargetRef::Object { id: art_gy2 },
            ]
        );

        // 2. has_legal_target_assignment: [spec_opt_creature(min=0), spec_share0(min=1)] fails when 0 creatures in GY (only artifacts)
        let spec_opt_creature = TargetSpec {
            filter: ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            },
            min: 0,
            max: 1,
            ..base_spec.clone()
        };
        let empty_chosen: [Vec<TargetRef>; 0] = [];
        let info_empty =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &empty_chosen);
        assert!(
            !has_legal_target_assignment(
                &state,
                &[spec_opt_creature.clone(), spec_share0.clone()],
                &info_empty
            ),
            "must reject [opt_creature, share0] when 0 creatures exist in GY"
        );

        // 3. revalidate_targets returns AllIllegal on [vec![], vec![art_gy1]]
        assert_eq!(
            revalidate_targets(
                &state,
                &[spec_opt_creature.clone(), spec_share0.clone()],
                &info_empty,
                &[vec![], vec![TargetRef::Object { id: art_gy1 }]],
            ),
            TargetRevalidation::AllIllegal
        );

        // Now move 2 creatures into GY
        let cr_gy1 = move_to_gy(&mut state, CardDefId(0));
        let cr_gy2 = move_to_gy(&mut state, CardDefId(0));

        // 4. Multi-target spec0 = [cr_gy1, art_gy1], spec1 = [art_gy2] -> shares_card_type_with_target(1) indexes spec1 (Artifact), not spec0[1]
        let prior_multi0 = [
            vec![
                TargetRef::Object { id: cr_gy1 },
                TargetRef::Object { id: art_gy1 },
            ],
            vec![TargetRef::Object { id: art_gy2 }],
        ];
        let info_multi0 =
            build_spell_target_source_info(&state, PlayerId(0), spell_obj, None, &prior_multi0);
        assert_eq!(
            legal_targets_for_spec(&state, &spec_share1, &info_multi0, &[]),
            vec![
                TargetRef::Object { id: art_gy1 },
                TargetRef::Object { id: art_gy2 },
            ]
        );
        // shares_card_type_with_target(0) checks spec0.first() == cr_gy1 (Creature)
        assert_eq!(
            legal_targets_for_spec(&state, &spec_share0, &info_multi0, &[]),
            vec![
                TargetRef::Object { id: cr_gy1 },
                TargetRef::Object { id: cr_gy2 },
            ]
        );

        // 5. has_legal_target_assignment succeeds when creatures exist, and revalidate_targets returns AllLegal
        assert!(has_legal_target_assignment(
            &state,
            &[spec_opt_creature.clone(), spec_share0.clone()],
            &info_empty
        ));
        assert_eq!(
            revalidate_targets(
                &state,
                &[spec_opt_creature, spec_share0],
                &info_empty,
                &[
                    vec![TargetRef::Object { id: cr_gy1 }],
                    vec![TargetRef::Object { id: cr_gy2 }],
                ],
            ),
            TargetRevalidation::AllLegal
        );
    }

    #[test]
    fn synth_parameterized_protection_and_hexproof_with_humility_and_remove_keywords() {
        let mut tables = InternTables::default();
        let kw_prot = KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("protection".into());
        let kw_hex = KeywordId(tables.keywords.len() as u16);
        tables.keywords.push("hexproof".into());
        let sub_human = mtg_ir::SubtypeId(tables.subtypes.len() as u16);
        tables.subtypes.push("Human".into());

        let empty_kw = |keyword: KeywordId, param: Option<&str>| KeywordDef {
            keyword,
            cost: None,
            param: param.map(str::to_string),
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
        let mk_card = |name: &str,
                       types: CardTypeMask,
                       colors: ColorMask,
                       subtypes: SmallVec<[mtg_ir::SubtypeId; 3]>,
                       abilities: Vec<AbilityDef>| CardDef {
            oracle_id: name.to_ascii_lowercase(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types,
                    subtypes,
                    supertypes: SupertypeMask::empty(),
                    colors,
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
                abilities,
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let prot_black = mk_card(
            "Synth Knight",
            CardTypeMask::CREATURE,
            ColorMask::W,
            SmallVec::new(),
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: empty_kw(kw_prot, Some("black")),
                id: None,
                span: Default::default(),
            })],
        );
        let prot_multi_humans = mk_card(
            "Synth Paladin",
            CardTypeMask::CREATURE,
            ColorMask::W,
            SmallVec::new(),
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: empty_kw(kw_prot, Some("red and from Humans")),
                id: None,
                span: Default::default(),
            })],
        );
        let hex_red = mk_card(
            "Synth Hexer",
            CardTypeMask::CREATURE,
            ColorMask::U,
            SmallVec::new(),
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: empty_kw(kw_hex, Some("red")),
                id: None,
                span: Default::default(),
            })],
        );
        let black_spell = mk_card(
            "Synth Doom",
            CardTypeMask::INSTANT,
            ColorMask::B,
            SmallVec::new(),
            vec![],
        );
        let red_spell = mk_card(
            "Synth Spark",
            CardTypeMask::INSTANT,
            ColorMask::R,
            SmallVec::new(),
            vec![],
        );
        let human_green_creature = mk_card(
            "Synth Villager",
            CardTypeMask::CREATURE,
            ColorMask::G,
            smallvec::smallvec![sub_human],
            vec![],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    prot_black,
                    prot_multi_humans,
                    hex_red,
                    black_spell,
                    red_spell,
                    human_green_creature,
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 91).unwrap();

        let put_bf = |st: &mut GameState, p: PlayerId, def: CardDefId| -> ObjectId {
            let h = st.player(p).unwrap().hand[0];
            st.object_mut(h).unwrap().card_def = def;
            match move_object(
                st,
                h,
                Zone::Battlefield,
                MoveRawOptions {
                    controller: Some(p),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            {
                crate::zones::MoveObjectOutcome::Moved(r) => r.new_id.unwrap(),
                _ => panic!("expected BF move"),
            }
        };
        let knight = put_bf(&mut state, PlayerId(1), CardDefId(0));
        let paladin = put_bf(&mut state, PlayerId(1), CardDefId(1));
        let hexer = put_bf(&mut state, PlayerId(1), CardDefId(2));
        let villager = put_bf(&mut state, PlayerId(0), CardDefId(5));

        let b_spell = state.player(PlayerId(0)).unwrap().hand[0];
        state.object_mut(b_spell).unwrap().card_def = CardDefId(3);
        let r_spell = state.player(PlayerId(0)).unwrap().hand[1];
        state.object_mut(r_spell).unwrap().card_def = CardDefId(4);

        let spec = TargetSpec {
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
            zones: ZoneMask::BATTLEFIELD,
        };

        let b_info = build_spell_target_source_info(&state, PlayerId(0), b_spell, None, &[]);
        assert!(!is_target_legal_for_spec(
            &state,
            &spec,
            &b_info,
            &[],
            TargetRef::Object { id: knight }
        ));
        assert!(is_target_legal_for_spec(
            &state,
            &spec,
            &b_info,
            &[],
            TargetRef::Object { id: hexer }
        ));

        let r_info = build_spell_target_source_info(&state, PlayerId(0), r_spell, None, &[]);
        assert!(!is_target_legal_for_spec(
            &state,
            &spec,
            &r_info,
            &[],
            TargetRef::Object { id: paladin }
        ));
        assert!(!is_target_legal_for_spec(
            &state,
            &spec,
            &r_info,
            &[],
            TargetRef::Object { id: hexer }
        ));

        let villager_info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: Some(villager),
            source_lki: None,
            is_spell: false,
            is_ability: true,
            source_colors: ColorMask::G,
            source_types: CardTypeMask::CREATURE,
            source_subtypes: smallvec::smallvec![sub_human],
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };
        assert!(!is_target_legal_for_spec(
            &state,
            &spec,
            &villager_info,
            &[],
            TargetRef::Object { id: paladin }
        ));

        // RemoveKeywords("hexproof", cant_have_or_gain: true) strips parameterized hexproof from hexer!
        let eff_id = state.allocate_continuous_effect_id();
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: eff_id,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                hexer
            ]),
            modifier: mtg_ir::LayerMod::RemoveKeywords {
                keywords: smallvec::smallvec![kw_hex],
                cant_have_or_gain: true,
            },
            duration: mtg_ir::Duration::EndOfTurn,
            timestamp: ts,
            expires_turn: Some(state.turn_serial()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(is_target_legal_for_spec(
            &state,
            &spec,
            &r_info,
            &[],
            TargetRef::Object { id: hexer }
        ));

        // RemoveAllAbilities (Humility) strips parameterized protection from knight!
        let eff_id2 = state.allocate_continuous_effect_id();
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(crate::layers::predicates::TimedContinuousEffect {
            id: eff_id2,
            source: None,
            controller: PlayerId(0),
            layer: mtg_ir::Layer::Layer6,
            affected: crate::layers::predicates::ContinuousTarget::Objects(smallvec::smallvec![
                knight
            ]),
            modifier: mtg_ir::LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            duration: mtg_ir::Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: Some(state.turn_serial()),
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(is_target_legal_for_spec(
            &state,
            &spec,
            &b_info,
            &[],
            TargetRef::Object { id: knight }
        ));
    }

    #[test]
    fn synth_modal_different_players_pawprint_defending_player_and_over_16_budget() {
        use mtg_ir::{ModalMode, PlayerFilter, PlayerRef};

        let (base_db, plain, _hex, _shroud, _prot, _flag) = build_target_test_db();
        let mut cards = base_db.cards.clone();
        let art_creature_def = CardDefId(cards.len() as u32);
        cards.push(CardDef {
            oracle_id: "t_art_creature".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Golem".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Golem".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}"),
                    types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
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
        });
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: base_db.tables.clone(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        let make_target_spec = |kind: TargetKind,
                                filter: ObjectFilter,
                                player: Option<PlayerFilter>,
                                shares: Option<u16>| TargetSpec {
            kind,
            filter,
            player,
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
            shares_card_type_with_target: shares,
            zones: if kind == TargetKind::Player {
                ZoneMask::empty()
            } else {
                ZoneMask::BATTLEFIELD
            },
        };

        // 1. Over-16-candidates joint search: 20 plain creatures + 1 artifact creature at #21,
        //    followed by an artifact-only spec sharing card type with spec 0.
        let big_deck = DeckSpec {
            commanders: vec![],
            mainboard: {
                let mut m = vec![plain; 25];
                m.push(art_creature_def);
                m
            },
            sideboard: vec![],
        };
        let mut state1v1 = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[big_deck.clone(), big_deck],
            91,
        )
        .unwrap();
        for _ in 0..20 {
            let _ = move_one_to_bf(&mut state1v1, PlayerId(0), plain);
        }
        let art_cre = move_one_to_bf(&mut state1v1, PlayerId(0), art_creature_def);

        let spec0 = make_target_spec(
            TargetKind::Object,
            ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            },
            None,
            None,
        );
        let spec1 = make_target_spec(
            TargetKind::Object,
            ObjectFilter {
                types_any: CardTypeMask::ARTIFACT,
                ..Default::default()
            },
            None,
            Some(0),
        );
        let info1v1 = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::U,
            source_types: CardTypeMask::SORCERY,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: Some(PlayerId(1)),
        };
        let cache1v1 = state1v1.evaluate_board();
        assert!(has_legal_target_assignment_cached(
            &state1v1,
            &[spec0.clone(), spec1.clone()],
            &info1v1,
            cache1v1
        ));
        let mut ctx_check = EvalContext::new(PlayerId(0));
        info1v1.with_eval_context_cached(cache1v1, |ctx| {
            ctx_check.defending_player = ctx.defending_player;
        });
        assert_eq!(ctx_check.defending_player, Some(PlayerId(1)));
        assert_eq!(art_cre, art_cre);

        // 2. different_player_targets_per_mode in 1v1 vs 3p + pawprint_budget
        let opp_player_spec = make_target_spec(
            TargetKind::Player,
            ObjectFilter::default(),
            Some(PlayerFilter {
                relation: PlayerRelation::Opponent,
            }),
            None,
        );
        let def_creature_spec = make_target_spec(
            TargetKind::Object,
            ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                extra: vec![ObjectPredicate::ControllerRef {
                    player: PlayerRef::DefendingPlayer,
                }],
                ..Default::default()
            },
            None,
            None,
        );
        let modal_diff_players = ModalDef {
            min: 2,
            max: 2,
            allow_repeat: false,
            once_per_turn_unique: false,
            up_to: false,
            conditional_choose_all: None,
            modes: vec![
                ModalMode {
                    targets: vec![opp_player_spec.clone()],
                    effect: vec![],
                    span: Default::default(),
                },
                ModalMode {
                    targets: vec![opp_player_spec],
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
            pawprint_budget: Some(3),
            pawprint_costs: vec![2, 2, 1],
            dynamic_max: None,
            random_mode: false,
            prefix_effect: vec![],
            suffix_effect: vec![],
            conditional_choose_range: None,
            conditional_not_random: None,
            different_player_targets_per_mode: true,
        };
        let eval_ctx1v1 = EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(cache1v1));
        let bounds = effective_modal_bounds_cached(&state1v1, &modal_diff_players, &eval_ctx1v1);
        // Mode 0 + Mode 1 fails both different_player_targets_per_mode (1 opponent) and pawprint (4>3);
        // Mode 0 + Mode 2 passes (3 <= 3), Mode 1 + Mode 2 passes (3 <= 3).
        let (next0, done0) = legal_next_modes_and_done_cached(
            &state1v1,
            &modal_diff_players,
            &[],
            &info1v1,
            bounds,
            &[],
            |_| true,
            cache1v1,
        );
        assert!(!done0);
        assert_eq!(next0.as_slice(), &[0, 1, 2]);
        // After picking Mode 0, only Mode 2 is legal (Mode 1 fails).
        let (next_after0, done_after0) = legal_next_modes_and_done_cached(
            &state1v1,
            &modal_diff_players,
            &[],
            &info1v1,
            bounds,
            &[0],
            |_| true,
            cache1v1,
        );
        assert!(!done_after0);
        assert_eq!(next_after0.as_slice(), &[2]);

        // Also verify defending_player in TargetSpec via ObjectPredicate::ControllerRef(DefendingPlayer)
        assert!(!has_legal_target_assignment_cached(
            &state1v1,
            std::slice::from_ref(&def_creature_spec),
            &info1v1,
            cache1v1
        ));
        let _p1_cre = move_one_to_bf(&mut state1v1, PlayerId(1), plain);
        let cache1v1_after = state1v1.evaluate_board();
        assert!(has_legal_target_assignment_cached(
            &state1v1,
            &[def_creature_spec],
            &info1v1,
            cache1v1_after
        ));

        // In 3p with no pawprint budget, picking P1 in mode 0 restricts mode 1 to P2 only.
        let mut cfg3 = GameConfig::standard_1v1();
        cfg3.player_count = 3;
        let deck3 = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 10],
            sideboard: vec![],
        };
        let state3 = GameState::new(db, cfg3, &[deck3.clone(), deck3.clone(), deck3], 92).unwrap();
        let cache3 = state3.evaluate_board();
        let mut modal_3p = modal_diff_players;
        modal_3p.pawprint_budget = None;
        modal_3p.pawprint_costs.clear();
        let (specs_01, pos_01) = build_modal_target_specs(&[], &modal_3p, &[0, 1]);
        let targets_s0 = vec![vec![TargetRef::Player { id: PlayerId(1) }]];
        let info3_at_s1 = TargetSourceInfo {
            targets_by_spec: &targets_s0,
            ..info1v1.clone()
        };
        let (filtered_cands, can_done_s1) = filter_casting_target_choices_with_modal_cached(
            &state3,
            &specs_01,
            &info3_at_s1,
            1,
            &[],
            vec![
                TargetRef::Player { id: PlayerId(1) },
                TargetRef::Player { id: PlayerId(2) },
            ],
            false,
            pos_01.as_deref(),
            cache3,
        );
        assert!(!can_done_s1);
        assert_eq!(filtered_cands, vec![TargetRef::Player { id: PlayerId(2) }]);
        assert!(!partial_targets_can_complete_with_modal_cached(
            &state3,
            &specs_01,
            &info3_at_s1,
            1,
            &[TargetRef::Player { id: PlayerId(1) }],
            false,
            pos_01.as_deref(),
            cache3,
        ));
        assert!(partial_targets_can_complete_with_modal_cached(
            &state3,
            &specs_01,
            &info3_at_s1,
            1,
            &[TargetRef::Player { id: PlayerId(2) }],
            false,
            pos_01.as_deref(),
            cache3,
        ));
    }

    #[test]
    fn synth_divide_total_target_bounds_and_legality() {
        use crate::view::PowerToughness;
        use mtg_ir::{Amount, Ref, StatKind};

        let (db, plain, _, _, _, _) = build_target_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 20],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            93,
        )
        .unwrap();

        let mut creatures = Vec::new();
        for _ in 0..6 {
            creatures.push(move_one_to_bf(&mut state, PlayerId(1), plain));
        }
        let cache = state.evaluate_board();

        let base_creature_spec = |min: u16, max: u16, divide_total: Option<Amount>| TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            },
            player: None,
            min,
            max,
            same_owner: false,
            condition: None,
            other_target: false,
            divide_total: divide_total.map(Box::new),
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

        let info = TargetSourceInfo {
            caster: PlayerId(0),
            source_object: None,
            source_lki: None,
            is_spell: true,
            is_ability: false,
            source_colors: ColorMask::R,
            source_types: CardTypeMask::CREATURE,
            source_subtypes: SmallVec::new(),
            x_value: None,
            targets_by_spec: &[],
            bindings: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            defending_player: None,
        };

        // 1. min: 0, max: 99, divide_total: Const(4) with 6 creatures -> bounds (0, 4); empty after 4 picks
        let fury_spec = base_creature_spec(0, 99, Some(Amount::Const { n: 4 }));
        assert_eq!(
            effective_target_bounds_cached(&state, &fury_spec, &info, cache),
            (0, 4)
        );
        let picked4: Vec<TargetRef> = creatures[..4]
            .iter()
            .map(|&id| TargetRef::Object { id })
            .collect();
        assert_eq!(
            legal_targets_for_spec_cached(&state, &fury_spec, &info, &picked4[..3], cache).len(),
            3
        );
        assert!(
            legal_targets_for_spec_cached(&state, &fury_spec, &info, &picked4, cache).is_empty()
        );

        // 2. min: 2, max: 2, divide_total: Const(1) and Const(0) -> min > max, rejected everywhere
        for total in [1, 0] {
            let bad_spec = base_creature_spec(2, 2, Some(Amount::Const { n: total }));
            assert_eq!(
                effective_target_bounds_cached(&state, &bad_spec, &info, cache),
                (2, total as u16)
            );
            assert!(!has_legal_target_assignment_cached(
                &state,
                std::slice::from_ref(&bad_spec),
                &info,
                cache
            ));
            assert!(legal_targets_for_spec_cached(&state, &bad_spec, &info, &[], cache).is_empty());
            let (filtered, can_done) = filter_casting_target_choices_with_modal_cached(
                &state,
                std::slice::from_ref(&bad_spec),
                &info,
                0,
                &[],
                vec![TargetRef::Object { id: creatures[0] }],
                false,
                None,
                cache,
            );
            assert!(filtered.is_empty());
            assert!(!can_done);
            assert!(!partial_targets_can_complete_with_modal_cached(
                &state,
                &[bad_spec],
                &info,
                0,
                &picked4[..2],
                true,
                None,
                cache,
            ));
        }

        // 3. divide_total: X (X=0 -> (0, 0), X=2 -> (0, 2))
        let x_div_spec = base_creature_spec(0, 99, Some(Amount::X));
        let info_x0 = TargetSourceInfo {
            x_value: Some(0),
            ..info.clone()
        };
        let info_x2 = TargetSourceInfo {
            x_value: Some(2),
            ..info.clone()
        };
        assert_eq!(
            effective_target_bounds_cached(&state, &x_div_spec, &info_x0, cache),
            (0, 0)
        );
        assert_eq!(
            effective_target_bounds_cached(&state, &x_div_spec, &info_x2, cache),
            (0, 2)
        );

        // 4. divide_total: Stat(EventObject, Power, lki=true) (power = 3 -> (0, 3))
        let lki_snap = LkiSnapshot {
            object_id: creatures[0],
            card_def: plain,
            name: "Synth Bear".into(),
            owner: PlayerId(1),
            controller: PlayerId(1),
            last_zone: Zone::Battlefield,
            pt: Some(PowerToughness {
                power: 3,
                toughness: 3,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            colors: ColorMask::G,
            mana_value: 2,
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
            active_face_index: 0,
        };
        let lki_spec = base_creature_spec(
            0,
            99,
            Some(Amount::Stat {
                of: Ref::EventObject,
                stat: StatKind::Power,
                lki: true,
            }),
        );
        let info_lki = TargetSourceInfo {
            event_object: Some(creatures[0]),
            event_lki: Some(&lki_snap),
            ..info.clone()
        };
        assert_eq!(
            effective_target_bounds_cached(&state, &lki_spec, &info_lki, cache),
            (0, 3)
        );

        // 5. dynamic_count: X + divide_total: Const(8) with X = 9 -> (9, 8), impossible
        let mut meteor_spec = base_creature_spec(0, 99, Some(Amount::Const { n: 8 }));
        meteor_spec.dynamic_count = Some(Box::new(Amount::X));
        meteor_spec.dynamic_up_to = false;
        let info_x9 = TargetSourceInfo {
            x_value: Some(9),
            ..info
        };
        assert_eq!(
            effective_target_bounds_cached(&state, &meteor_spec, &info_x9, cache),
            (9, 8)
        );
        assert!(!has_legal_target_assignment_cached(
            &state,
            &[meteor_spec],
            &info_x9,
            cache
        ));
    }

    #[test]
    fn synth_other_target_allows_source_unlike_filter_flags_other() {
        let (db, plain, _, _, _, _) = build_target_test_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![plain; 20],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 94).unwrap();
        let src = move_one_to_bf(&mut state, PlayerId(0), plain);
        let bear = move_one_to_bf(&mut state, PlayerId(0), plain);

        let make_spec = |other_target: bool, flags: FilterFlags| TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                controller: Some(RelativeControl::You),
                flags,
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

        let cache = state.evaluate_board();
        let spec_plain = make_spec(false, FilterFlags::empty());
        let spec_other_target = make_spec(true, FilterFlags::empty());
        let spec_other_flag = make_spec(true, FilterFlags::OTHER);

        // With bear chosen for Spec 0, `other_target: true` (no FilterFlags::OTHER) MUST allow `src`
        // for Spec 1, while excluding `bear`.
        let bear_first = [vec![TargetRef::Object { id: bear }]];
        let info_bear_first = build_ability_target_source_info_cached(
            &state,
            PlayerId(0),
            src,
            None,
            None,
            &bear_first,
            &[],
            cache,
        );
        let cands_other_target =
            legal_targets_for_spec_cached(&state, &spec_other_target, &info_bear_first, &[], cache);
        assert_eq!(cands_other_target, vec![TargetRef::Object { id: src }]);

        // Conversely, FilterFlags::OTHER MUST exclude `src` even when `bear` is already chosen.
        let cands_other_flag =
            legal_targets_for_spec_cached(&state, &spec_other_flag, &info_bear_first, &[], cache);
        assert!(cands_other_flag.is_empty());

        // Joint feasibility with [spec_plain, spec_other_flag]: Spec 0 can only pick `src` (since
        // picking `bear` leaves Spec 1 empty).
        let empty_info = build_ability_target_source_info_cached(
            &state,
            PlayerId(0),
            src,
            None,
            None,
            &[],
            &[],
            cache,
        );
        let (spec0_filtered, _) = filter_casting_target_choices_with_modal_cached(
            &state,
            &[spec_plain, spec_other_flag],
            &empty_info,
            0,
            &[],
            vec![
                TargetRef::Object { id: src },
                TargetRef::Object { id: bear },
            ],
            false,
            None,
            cache,
        );
        assert_eq!(spec0_filtered, vec![TargetRef::Object { id: src }]);
    }
}
