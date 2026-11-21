use std::sync::Arc;

use mtg_ir::{
    AbilityDef, Amount, CardDefId, CardTypeMask, ColorMask, CostDef, CostDelta, CostDeltaMode,
    CostObjectTarget, CounterId, CounterTarget, FirstActivationZeroKind, KeywordId,
    KickerCostOption, LoyaltySign, ManaColor, ManaCost, ManaPip, ManaSourceRestrictionKind,
    ManaSpendScope, PlayerFilter, RestrictionKind, RoundMode, RuleMod, SpellOrAbilityFilter,
    StaticBody, SupertypeMask, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::action::{ChoiceAnswer, ChoiceRequest, CostAidKind, DecisionState, TargetRef};
use crate::auto_tap::{
    AutoTapConstraints, AutoTapPolicy, ReservedGroupConflict, ReservedObjectGroup,
    TapSubsetSumConstraint, plan_auto_tap_with_constraints_cached,
};
use crate::commander;
use crate::eval::{BoardView, EvalContext, evaluate_amount, evaluate_condition, object_mana_value};
use crate::events::{AtomicEventKind, emit_event};
use crate::faces::{active_abilities_with_card_for_object, face_prototype_spec};
use crate::filter::{find_objects, matches_object_filter, matches_player_filter};
use crate::id::{ObjectId, PlayerId};
use crate::mana::{self, ManaSpendContext, color_to_index, matches_affected};
use crate::replace::{self, ReplacementOutcome, ReplacementResult};
use crate::state::{
    AltCastKind, CastOption, CostPaymentPromptState, CostPromptKind, CostSelectionKind, DirtyMask,
    EngineError, GameState, LayerCache, LkiSnapshot, NONE_OBJECT_ID, ObjectFlags,
    PendingCostPaymentPlan, PendingEffectReplaceKind, PendingNonManaCostStep, PendingPromptContext,
    PendingZoneCostKind, PromptOwner, ReplaceableEventKind,
};
use crate::zones::{self, MoveObjectOutcome, MoveRawOptions, ZonePosition};

#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveCostBreakdown {
    pub base_mana: ManaCost,
    pub locked_mana: ManaCost,
    pub non_mana_costs: Vec<CostDef>,
    pub optional_costs: Vec<(u16, KickerCostOption)>,
    pub allows_convoke: bool,
    pub allows_improvise: bool,
    pub allows_delve: bool,
    pub allows_emerge: bool,
    pub x_spend_colors: ColorMask,
    pub x_colored_only: bool,
    pub x_max_one_per_color: bool,
    pub source_restriction: Option<ManaSourceRestrictionKind>,
    pub defiler_phyrexian_available: Option<(ManaColor, u16)>,
    pub commander_tax_as_life_available: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CostStepOutcome {
    Paid,
    Paused { prompt_id: u32, chooser: PlayerId },
}

fn object_might_have_static<F>(state: &GameState, id: ObjectId, pred: F) -> bool
where
    F: Fn(&mtg_ir::StaticDef) -> bool,
{
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.is_face_down() {
        return false;
    }
    if state
        .rare(id)
        .is_some_and(|r| r.copiable.is_some() || !r.merged_components.is_empty())
    {
        return true;
    }
    let Some(card) = state.db().get(obj.card_def) else {
        return false;
    };
    card.faces.iter().any(|f| {
        f.abilities
            .iter()
            .any(|ab| matches!(ab, AbilityDef::Static(st) if pred(&st.static_def)))
    })
}

fn check_alt_keyword_matches(
    db: &mtg_ir::CardDatabase,
    kw: KeywordId,
    alt_kind: AltCastKind,
) -> bool {
    let Some(name) = db.keyword_name(kw) else {
        return false;
    };
    let low = name.to_ascii_lowercase();
    match alt_kind {
        AltCastKind::Flashback => low == "flashback",
        AltCastKind::Escape => low == "escape",
        AltCastKind::Disturb => low == "disturb",
        AltCastKind::Encore => low == "encore",
        AltCastKind::Harmonize => low == "harmonize",
        AltCastKind::JumpStart => low == "jump-start",
        AltCastKind::Madness => low == "madness",
        AltCastKind::Miracle => low == "miracle",
        AltCastKind::ForetellCast => low == "foretell",
        AltCastKind::PlotCast => low == "plot",
        AltCastKind::Warp => low == "warp",
        AltCastKind::Blitz => low == "blitz",
        AltCastKind::Dash => low == "dash",
        AltCastKind::Evoke => low == "evoke",
        AltCastKind::Overload => low == "overload",
        AltCastKind::Cleave => low == "cleave",
        AltCastKind::Fuse => low == "fuse",
        AltCastKind::Spree => low == "spree",
        AltCastKind::Emerge => low == "emerge",
        AltCastKind::Mutate => low == "mutate",
        AltCastKind::Bestow => low == "bestow",
        AltCastKind::Prototype => low == "prototype",
        AltCastKind::MorphFaceDown => low == "morph" || low == "megamorph",
        AltCastKind::DisguiseFaceDown => low == "disguise",
        _ => false,
    }
}

pub fn spell_x_cant_be_zero(state: &GameState, object: ObjectId, option: &CastOption) -> bool {
    let Some(obj) = state.object(object) else {
        return false;
    };
    let Some(card) = state.db().get(obj.card_def) else {
        return false;
    };
    let Some(face) = card.faces.get(option.face_idx as usize) else {
        return false;
    };
    for ab in &face.abilities {
        match ab {
            AbilityDef::Spell(sp) if sp.x_cant_be_zero => return true,
            AbilityDef::Static(st) => {
                if let StaticBody::SelfCastRestriction { x_cant_be_zero, .. } = &st.static_def.body
                    && *x_cant_be_zero
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn add_mana_cost(dst: &mut ManaCost, add: &ManaCost) {
    dst.generic = dst.generic.saturating_add(add.generic);
    dst.x_count = dst.x_count.saturating_add(add.x_count);
    dst.pips.extend(add.pips.iter().copied());
}

fn pip_matches_color(pip: ManaPip, color: ManaColor) -> bool {
    match pip {
        ManaPip::Colored { color: c }
        | ManaPip::TwoBrid { color: c }
        | ManaPip::Phyrexian { color: c }
        | ManaPip::ColorlessHybrid { color: c } => c == color,
        ManaPip::Hybrid { a, b } | ManaPip::HybridPhyrexian { a, b } => a == color || b == color,
        ManaPip::Snow => false,
    }
}

fn apply_colored_reduction_pips(
    cost: &mut ManaCost,
    color: ManaColor,
    mut amount: u32,
    colored_only: bool,
) {
    while amount > 0 {
        if let Some(pos) = cost
            .pips
            .iter()
            .position(|p| matches!(p, ManaPip::Colored { color: c } if *c == color))
        {
            cost.pips.remove(pos);
            amount -= 1;
            continue;
        }
        if let Some(pos) = cost.pips.iter().position(|p| pip_matches_color(*p, color)) {
            cost.pips.remove(pos);
            amount -= 1;
            continue;
        }
        break;
    }
    if !colored_only && amount > 0 {
        cost.generic = cost.generic.saturating_sub(amount);
    }
}

#[derive(Default)]
struct CostDeltaAccumulator {
    set_zero: bool,
    generic_increase: u32,
    colored_increase: Vec<(ManaColor, u32)>,
    hybrid_increase: Vec<(ManaColor, ManaColor)>,
    increase_min_total: u16,
    generic_reduction: u32,
    colored_reduction: [u32; 6],
    hybrid_reduction: Vec<(ManaColor, ManaColor)>,
    colored_only: bool,
    reduce_min_total: u16,
    extra_non_mana_costs: Vec<CostDef>,
}

impl CostDeltaAccumulator {
    fn push_delta(
        &mut self,
        state: &GameState,
        delta: &CostDelta,
        ctx: &EvalContext<'_>,
        chosen_targets: &[TargetRef],
    ) {
        let factor = if let Some(per) = delta.per.as_deref() {
            evaluate_amount(state, per, ctx).max(0) as u32
        } else {
            1
        };
        if let Some(per_life) = delta.per_target_life
            && !chosen_targets.is_empty()
        {
            let n_targets = i32::try_from(chosen_targets.len()).unwrap_or(i32::MAX);
            let total_life = i32::from(per_life).saturating_mul(n_targets);
            if total_life > 0 {
                self.extra_non_mana_costs.push(CostDef::PayLife {
                    amount: Box::new(mtg_ir::Amount::Const { n: total_life }),
                    half_life_rounded_up: false,
                });
            }
        }
        if delta.per.is_some() && factor == 0 {
            return;
        }

        match delta.mode {
            CostDeltaMode::SetZero => {
                self.set_zero = true;
            }
            CostDeltaMode::Increase => {
                self.generic_increase = self
                    .generic_increase
                    .saturating_add(u32::from(delta.generic).saturating_mul(factor));
                for cc in &delta.colored {
                    self.colored_increase
                        .push((cc.color, u32::from(cc.count).saturating_mul(factor)));
                }
                for _ in 0..factor {
                    for &(a, b) in &delta.hybrid_colored {
                        self.hybrid_increase.push((a, b));
                    }
                }
                if let Some(m) = delta.min_total_mana {
                    self.increase_min_total = self.increase_min_total.max(m);
                }
                self.extra_non_mana_costs
                    .extend(delta.additional_costs.iter().cloned());
            }
            CostDeltaMode::Reduce => {
                if delta.generic > 0 {
                    let mut gen_red = u32::from(delta.generic).saturating_mul(factor);
                    if let Some(cap) = delta.max_reduction {
                        gen_red = gen_red.min(u32::from(cap));
                    }
                    self.generic_reduction = self.generic_reduction.saturating_add(gen_red);
                }
                for cc in &delta.colored {
                    let idx = color_to_index(cc.color);
                    self.colored_reduction[idx] = self.colored_reduction[idx]
                        .saturating_add(u32::from(cc.count).saturating_mul(factor));
                }
                for _ in 0..factor {
                    for &(a, b) in &delta.hybrid_colored {
                        self.hybrid_reduction.push((a, b));
                    }
                }
                if delta.colored_only {
                    self.colored_only = true;
                }
                if let Some(m) = delta.min_total_mana {
                    self.reduce_min_total = self.reduce_min_total.max(m);
                }
            }
        }
    }

    fn apply_to_cost(
        &self,
        cost: &mut ManaCost,
        additional_mana: &ManaCost,
        extra_generic_before_reductions: u32,
    ) {
        // CR 118.9d: SetZero (e.g. "without paying its mana cost") zeroes ONLY the base mana cost;
        // additional costs (kicker, Spree, Escalate, commander tax, cost increases) are added after.
        if self.set_zero {
            cost.generic = 0;
            cost.x_count = 0;
            cost.pips.clear();
        }
        add_mana_cost(cost, additional_mana);
        cost.generic = cost
            .generic
            .saturating_add(extra_generic_before_reductions)
            .saturating_add(self.generic_increase);

        for &(c, count) in &self.colored_increase {
            for _ in 0..count {
                cost.pips.push(ManaPip::Colored { color: c });
            }
        }
        for &(a, b) in &self.hybrid_increase {
            cost.pips.push(ManaPip::Hybrid { a, b });
        }

        let base_mana_before_reduction = cost.mana_value();

        // Apply colored reductions first
        for (idx, &amt) in self.colored_reduction.iter().enumerate() {
            if amt > 0 {
                let c = mana::index_to_color(idx);
                apply_colored_reduction_pips(cost, c, amt, self.colored_only);
            }
        }
        for &(a, b) in &self.hybrid_reduction {
            if let Some(pos) = cost.pips.iter().position(|p| match *p {
                ManaPip::Hybrid { a: pa, b: pb } => (pa == a && pb == b) || (pa == b && pb == a),
                ManaPip::Colored { color: c } => c == a || c == b,
                _ => false,
            }) {
                cost.pips.remove(pos);
            } else if !self.colored_only {
                cost.generic = cost.generic.saturating_sub(1);
            }
        }

        // Generic reductions NEVER reduce colored pips (CR 118.7a)
        cost.generic = cost.generic.saturating_sub(self.generic_reduction);

        // CR 118.7b: min_total_mana from Reduce clamps ONLY if pre-reduction total >= floor
        if self.reduce_min_total > 0 {
            let floor = u32::from(self.reduce_min_total);
            if base_mana_before_reduction >= floor && cost.mana_value() < floor {
                cost.generic = cost.generic.saturating_add(floor - cost.mana_value());
            }
        }

        // Unconditional min_total_mana from Increase (Trinisphere) applies LAST
        if self.increase_min_total > 0 {
            let floor = u32::from(self.increase_min_total);
            if cost.mana_value() < floor {
                cost.generic = cost.generic.saturating_add(floor - cost.mana_value());
            }
        }
    }
}

fn matches_who_player_filter(
    state: &GameState,
    caster_or_activator: PlayerId,
    source_controller: PlayerId,
    who: &PlayerFilter,
    who_enchanted_player: bool,
    source_id: ObjectId,
    ctx: &EvalContext<'_>,
) -> bool {
    if who_enchanted_player {
        let host_player = state
            .object(source_id)
            .and_then(|o| o.attached_to)
            .and_then(|h| state.object(h))
            .map(|h| h.controller);
        return host_player == Some(caster_or_activator);
    }
    let _ = source_controller;
    matches_player_filter(state, caster_or_activator, who, ctx)
}

fn spell_matches_cost_mod_filter(
    state: &GameState,
    caster: PlayerId,
    spell_obj: ObjectId,
    option: &CastOption,
    affects: &SpellOrAbilityFilter,
    mod_ctx: &EvalContext<'_>,
) -> bool {
    match affects {
        SpellOrAbilityFilter::SelfObj => mod_ctx.source == Some(spell_obj),
        SpellOrAbilityFilter::Spell {
            filter,
            alt_cost_keyword,
        } => {
            if let Some(kw) = alt_cost_keyword
                && !check_alt_keyword_matches(state.db(), *kw, option.alt_kind)
            {
                return false;
            }
            let _ = caster;
            let spell_ctx = EvalContext {
                zone_override: Some(Zone::Stack),
                ..*mod_ctx
            };
            matches_object_filter(state, spell_obj, filter, &spell_ctx)
        }
        SpellOrAbilityFilter::Ability { .. } => false,
    }
}

pub fn compute_effective_spell_cost(
    state: &GameState,
    caster: PlayerId,
    object: ObjectId,
    option: &CastOption,
    x_value: Option<u32>,
    chosen_optional: &[u16],
    chosen_targets: &[TargetRef],
) -> Result<EffectiveCostBreakdown, EngineError> {
    compute_effective_spell_cost_cached(
        state,
        caster,
        object,
        option,
        x_value,
        chosen_optional,
        chosen_targets,
        state.evaluate_board(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_effective_spell_cost_cached(
    state: &GameState,
    caster: PlayerId,
    object: ObjectId,
    option: &CastOption,
    x_value: Option<u32>,
    chosen_optional: &[u16],
    chosen_targets: &[TargetRef],
    cache: &LayerCache,
) -> Result<EffectiveCostBreakdown, EngineError> {
    compute_effective_spell_cost_with_modes_cached(
        state,
        caster,
        object,
        option,
        &[],
        x_value,
        chosen_optional,
        chosen_targets,
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn compute_effective_spell_cost_with_modes(
    state: &GameState,
    caster: PlayerId,
    object: ObjectId,
    option: &CastOption,
    chosen_modes: &[u16],
    x_value: Option<u32>,
    chosen_optional: &[u16],
    chosen_targets: &[TargetRef],
) -> Result<EffectiveCostBreakdown, EngineError> {
    compute_effective_spell_cost_with_modes_cached(
        state,
        caster,
        object,
        option,
        chosen_modes,
        x_value,
        chosen_optional,
        chosen_targets,
        state.evaluate_board(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_effective_spell_cost_with_modes_cached(
    state: &GameState,
    caster: PlayerId,
    object: ObjectId,
    option: &CastOption,
    chosen_modes: &[u16],
    x_value: Option<u32>,
    chosen_optional: &[u16],
    chosen_targets: &[TargetRef],
    cache: &LayerCache,
) -> Result<EffectiveCostBreakdown, EngineError> {
    let obj = state
        .object(object)
        .ok_or_else(|| EngineError::IllegalAction(format!("spell object {object:?} missing")))?;
    let card = state
        .db()
        .get(obj.card_def)
        .ok_or(EngineError::UnknownCardDef(obj.card_def))?;
    let face = card
        .faces
        .get(option.face_idx as usize)
        .ok_or_else(|| EngineError::IllegalAction("invalid spell face index".into()))?;

    let is_special_alt_base = matches!(
        option.alt_kind,
        AltCastKind::MorphFaceDown
            | AltCastKind::DisguiseFaceDown
            | AltCastKind::Prototype
            | AltCastKind::PlotCast
            | AltCastKind::Fuse
            | AltCastKind::Spree
    );

    // Phase 1 (CR 118.9d): Determine base_mana ONLY (before additional costs, SetZero, and deltas).
    let mut base_mana = if let Some(ov) = &option.mana_cost_override {
        ov.clone()
    } else if option.alt_kind == AltCastKind::Prototype {
        face_prototype_spec(face, state.db())
            .map(|p| p.mana_cost)
            .unwrap_or_else(|| face.characteristics.mana_cost.clone())
    } else if matches!(
        option.alt_kind,
        AltCastKind::MorphFaceDown | AltCastKind::DisguiseFaceDown
    ) {
        ManaCost::parse("{3}")
    } else if option.alt_kind == AltCastKind::PlotCast {
        ManaCost::default()
    } else if option.fused {
        let mut combined = face.characteristics.mana_cost.clone();
        if let Some(f1) = card.faces.get(1) {
            add_mana_cost(&mut combined, &f1.characteristics.mana_cost);
        }
        combined
    } else {
        let mut alt_kw_cost: Option<ManaCost> = None;
        if !is_special_alt_base {
            for ab in &face.abilities {
                if let AbilityDef::Keyword(kw) = ab
                    && check_alt_keyword_matches(state.db(), kw.keyword.keyword, option.alt_kind)
                    && let Some(alt_mc) = &kw.keyword.cost
                {
                    alt_kw_cost = Some(alt_mc.clone());
                    break;
                }
            }
        }
        alt_kw_cost.unwrap_or_else(|| face.characteristics.mana_cost.clone())
    };

    if let Some(xv) = x_value
        && base_mana.x_count > 0
    {
        let x_add = xv.saturating_mul(u32::from(base_mana.x_count));
        base_mana.generic = base_mana.generic.saturating_add(x_add);
        base_mana.x_count = 0;
    }

    // Phase 2: Accumulate additional_mana, non_mana_costs, optional_costs, and cost flags.
    let mut additional_mana = ManaCost::default();
    let mut non_mana_costs: Vec<CostDef> = Vec::new();
    let mut optional_costs: Vec<(u16, KickerCostOption)> = Vec::new();
    let mut allows_convoke = false;
    let mut allows_improvise = false;
    let mut allows_delve = false;
    let mut allows_emerge = option.alt_kind == AltCastKind::Emerge;
    let mut x_spend_colors = ColorMask::empty();
    let mut x_colored_only = false;
    let mut x_max_one_per_color = false;
    let mut source_restriction: Option<ManaSourceRestrictionKind> = None;
    let mut defiler_phyrexian_available: Option<(ManaColor, u16)> = None;
    let mut commander_tax_as_life_available: Option<u16> = None;

    let mut affinity_reduction: u32 = 0;
    let caster_ctx = EvalContext::for_source(caster, object).with_board(BoardView::Cached(cache));

    // Inspect face abilities for additional costs, Spree per_mode_costs, Escalate, kicker/optional costs, and keywords
    let mut opt_idx_counter: u16 = 0;
    for ab in &face.abilities {
        match ab {
            AbilityDef::Spell(sp) => {
                if (!option.cleaved || face.cleaved_spell_spec.is_none())
                    && (!option.gift_promised || face.gift_promised_spell_spec.is_none())
                {
                    for c in &sp.additional_costs {
                        match c {
                            CostDef::Mana {
                                cost,
                                x_spend_colors: xc,
                                x_colored_only: xco,
                                x_max_one_per_color: xmo,
                            } => {
                                add_mana_cost(&mut additional_mana, cost);
                                x_spend_colors |= *xc;
                                x_colored_only |= *xco;
                                x_max_one_per_color |= *xmo;
                            }
                            CostDef::Optional { cost } => {
                                let mut opt_mana = ManaCost::default();
                                let mut opt_defs = Vec::new();
                                for inner in cost {
                                    if let CostDef::Mana { cost: mc, .. } = inner {
                                        add_mana_cost(&mut opt_mana, mc);
                                    } else {
                                        opt_defs.push(inner.clone());
                                    }
                                }
                                optional_costs.push((
                                    opt_idx_counter,
                                    KickerCostOption {
                                        cost: opt_mana,
                                        cost_defs: opt_defs,
                                        tags: Vec::new(),
                                    },
                                ));
                                opt_idx_counter = opt_idx_counter.saturating_add(1);
                            }
                            other => non_mana_costs.push(other.clone()),
                        }
                    }
                }
                if let Some(modal) = &sp.modal {
                    for &m_idx in chosen_modes {
                        if let Some(defs) = modal.per_mode_costs.get(usize::from(m_idx)) {
                            for c in defs {
                                match c {
                                    CostDef::Mana {
                                        cost,
                                        x_spend_colors: xc,
                                        x_colored_only: xco,
                                        x_max_one_per_color: xmo,
                                    } => {
                                        add_mana_cost(&mut additional_mana, cost);
                                        x_spend_colors |= *xc;
                                        x_colored_only |= *xco;
                                        x_max_one_per_color |= *xmo;
                                    }
                                    other => non_mana_costs.push(other.clone()),
                                }
                            }
                        }
                    }
                    if chosen_modes.len() > 1 {
                        let extra_modes = chosen_modes.len() - 1;
                        for kw_ab in &face.abilities {
                            if let AbilityDef::Keyword(kw) = kw_ab
                                && state
                                    .db()
                                    .keyword_name(kw.keyword.keyword)
                                    .is_some_and(|n| n.eq_ignore_ascii_case("escalate"))
                            {
                                for _ in 0..extra_modes {
                                    if let Some(esc_mc) = &kw.keyword.cost {
                                        add_mana_cost(&mut additional_mana, esc_mc);
                                    }
                                    for cd in &kw.keyword.cost_defs {
                                        if let CostDef::Mana {
                                            cost,
                                            x_spend_colors: xc,
                                            x_colored_only: xco,
                                            x_max_one_per_color: xmo,
                                        } = cd
                                        {
                                            add_mana_cost(&mut additional_mana, cost);
                                            x_spend_colors |= *xc;
                                            x_colored_only |= *xco;
                                            x_max_one_per_color |= *xmo;
                                        } else {
                                            non_mana_costs.push(cd.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            AbilityDef::Keyword(kw) => {
                if let Some(kw_name) = state.db().keyword_name(kw.keyword.keyword) {
                    match kw_name.to_ascii_lowercase().as_str() {
                        "convoke" => allows_convoke = true,
                        "improvise" => allows_improvise = true,
                        "delve" => allows_delve = true,
                        "emerge" => allows_emerge = true,
                        "affinity" => {
                            if let Some(aff_f) = kw.keyword.affinity_filter.as_deref() {
                                let mut f = aff_f.clone();
                                if f.zones.is_empty() {
                                    f.zones = ZoneMask::BATTLEFIELD;
                                }
                                let cnt = find_objects(state, &f, &caster_ctx).len() as u32;
                                affinity_reduction = affinity_reduction.saturating_add(cnt);
                            }
                        }
                        _ => {}
                    }
                }
                for kc in &kw.keyword.kicker_costs {
                    optional_costs.push((opt_idx_counter, kc.clone()));
                    opt_idx_counter = opt_idx_counter.saturating_add(1);
                }
                if !is_special_alt_base
                    && option.extra_costs.is_empty()
                    && check_alt_keyword_matches(state.db(), kw.keyword.keyword, option.alt_kind)
                {
                    for cd in &kw.keyword.cost_defs {
                        if let CostDef::Mana {
                            cost,
                            x_spend_colors: xc,
                            x_colored_only: xco,
                            x_max_one_per_color: xmo,
                        } = cd
                        {
                            add_mana_cost(&mut additional_mana, cost);
                            x_spend_colors |= *xc;
                            x_colored_only |= *xco;
                            x_max_one_per_color |= *xmo;
                        } else {
                            non_mana_costs.push(cd.clone());
                        }
                    }
                }
            }
            AbilityDef::Static(st) => {
                if let StaticBody::AdditionalCastCost { costs } = &st.static_def.body
                    && st
                        .static_def
                        .condition
                        .as_deref()
                        .is_none_or(|cond| evaluate_condition(state, cond, &caster_ctx))
                {
                    for c in costs {
                        match c {
                            CostDef::Mana {
                                cost,
                                x_spend_colors: xc,
                                x_colored_only: xco,
                                x_max_one_per_color: xmo,
                            } => {
                                add_mana_cost(&mut additional_mana, cost);
                                x_spend_colors |= *xc;
                                x_colored_only |= *xco;
                                x_max_one_per_color |= *xmo;
                            }
                            CostDef::Optional { cost } => {
                                let mut opt_mana = ManaCost::default();
                                let mut opt_defs = Vec::new();
                                for inner in cost {
                                    if let CostDef::Mana { cost: mc, .. } = inner {
                                        add_mana_cost(&mut opt_mana, mc);
                                    } else {
                                        opt_defs.push(inner.clone());
                                    }
                                }
                                optional_costs.push((
                                    opt_idx_counter,
                                    KickerCostOption {
                                        cost: opt_mana,
                                        cost_defs: opt_defs,
                                        tags: Vec::new(),
                                    },
                                ));
                                opt_idx_counter = opt_idx_counter.saturating_add(1);
                            }
                            other => non_mana_costs.push(other.clone()),
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if option.cleaved
        && let Some(cleaved) = &face.cleaved_spell_spec
    {
        for c in &cleaved.additional_costs {
            if let CostDef::Mana { cost, .. } = c {
                add_mana_cost(&mut additional_mana, cost);
            } else {
                non_mana_costs.push(c.clone());
            }
        }
    }
    if option.gift_promised
        && let Some(gift) = &face.gift_promised_spell_spec
    {
        for c in &gift.additional_costs {
            if let CostDef::Mana { cost, .. } = c {
                add_mana_cost(&mut additional_mana, cost);
            } else {
                non_mana_costs.push(c.clone());
            }
        }
    }

    // Apply extra_costs carried by CastOption (e.g. Escape/Flashback/Commander alt-cast costs)
    for c in &option.extra_costs {
        if let CostDef::Mana { cost, .. } = c {
            add_mana_cost(&mut additional_mana, cost);
        } else {
            non_mana_costs.push(c.clone());
        }
    }

    // Apply chosen optional / kicker costs (supports Multikicker via repeated indices)
    for &opt_id in chosen_optional {
        if let Some((_, kc)) = optional_costs.iter().find(|(idx, _)| *idx == opt_id) {
            add_mana_cost(&mut additional_mana, &kc.cost);
            for cd in &kc.cost_defs {
                if let CostDef::Mana { cost, .. } = cd {
                    add_mana_cost(&mut additional_mana, cost);
                } else {
                    non_mana_costs.push(cd.clone());
                }
            }
        }
    }

    let mut acc = CostDeltaAccumulator {
        generic_reduction: affinity_reduction,
        ..Default::default()
    };

    // Scan SelfObj statics on the spell itself (e.g. "This spell costs {1} less to cast for each...")
    for ab in &face.abilities {
        let AbilityDef::Static(st) = ab else {
            continue;
        };
        if let Some(cond) = st.static_def.condition.as_deref()
            && !evaluate_condition(state, cond, &caster_ctx)
        {
            continue;
        }
        match &st.static_def.body {
            StaticBody::CostModifier { affects, delta, .. } => {
                if matches!(affects, SpellOrAbilityFilter::SelfObj)
                    || spell_matches_cost_mod_filter(
                        state,
                        caster,
                        object,
                        option,
                        affects,
                        &caster_ctx,
                    )
                {
                    acc.push_delta(state, delta, &caster_ctx, chosen_targets);
                }
            }
            StaticBody::ManaSpendRestriction {
                scope: ManaSpendScope::SelfSpell | ManaSpendScope::SelfBoth,
                x_colors,
                x_colored_only: xco,
                x_max_one_per_color: xmo,
                source_kind,
            } => {
                x_spend_colors |= *x_colors;
                x_colored_only |= *xco;
                x_max_one_per_color |= *xmo;
                if source_kind.is_some() {
                    source_restriction = *source_kind;
                }
            }
            _ => {}
        }
    }

    // Scan Battlefield, Command (Emblems), and non-empty `active_zones` for CostModifier / Defiler / CommanderTaxAsLife
    for p in state.players() {
        if p.lost {
            continue;
        }
        let zone_lists: [(&[ObjectId], Zone); 5] = [
            (&p.battlefield, Zone::Battlefield),
            (&p.command, Zone::Command),
            (&p.hand, Zone::Hand),
            (&p.graveyard, Zone::Graveyard),
            (&p.exile, Zone::Exile),
        ];
        for (ids, zone) in zone_lists {
            for &src_id in ids {
                let Some(src_obj) = state.object(src_id) else {
                    continue;
                };
                if zone == Zone::Battlefield && src_obj.is_phased_out() {
                    continue;
                }
                if zone == Zone::Battlefield
                    && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
                {
                    continue;
                }
                let is_bf_or_emblem = zone == Zone::Battlefield
                    || (zone == Zone::Command && src_obj.flags.contains(ObjectFlags::IS_EMBLEM));
                let might_apply = object_might_have_static(state, src_id, |st| match &st.body {
                    StaticBody::CostModifier { active_zones, .. } => {
                        if active_zones.is_empty() {
                            is_bf_or_emblem
                        } else {
                            active_zones.contains(ZoneMask::from_zone(zone))
                        }
                    }
                    StaticBody::DefilerCostReduction { .. }
                    | StaticBody::CommanderTaxAsLife { .. }
                    | StaticBody::SpellGrantKeyword { .. } => zone == Zone::Battlefield,
                    _ => false,
                });
                if !might_apply {
                    continue;
                }
                let src_ctrl = if zone == Zone::Battlefield {
                    cache
                        .get(src_id)
                        .map_or(src_obj.controller, |e| e.controller)
                } else {
                    src_obj.owner
                };
                let src_ctx = EvalContext {
                    controller: src_ctrl,
                    source: Some(src_id),
                    targets: chosen_targets,
                    zone_override: Some(zone),
                    board: BoardView::Cached(cache),
                    ..EvalContext::new(src_ctrl)
                };
                let abilities = active_abilities_with_card_for_object(state, src_id, &src_ctx);
                for ab_ref in abilities {
                    let AbilityDef::Static(st) = ab_ref.ability else {
                        continue;
                    };
                    match &st.static_def.body {
                        StaticBody::CostModifier {
                            affects,
                            delta,
                            who,
                            who_enchanted_player,
                            active_zones,
                        } => {
                            let zone_ok = if active_zones.is_empty() {
                                zone == Zone::Battlefield
                                    || (zone == Zone::Command
                                        && src_obj.flags.contains(ObjectFlags::IS_EMBLEM))
                            } else {
                                active_zones.contains(ZoneMask::from_zone(zone))
                            };
                            if !zone_ok {
                                continue;
                            }
                            if let Some(cond) = st.static_def.condition.as_deref()
                                && !evaluate_condition(state, cond, &src_ctx)
                            {
                                continue;
                            }
                            if !matches_who_player_filter(
                                state,
                                caster,
                                src_ctrl,
                                who,
                                *who_enchanted_player,
                                src_id,
                                &src_ctx,
                            ) {
                                continue;
                            }
                            if spell_matches_cost_mod_filter(
                                state, caster, object, option, affects, &src_ctx,
                            ) {
                                acc.push_delta(state, delta, &src_ctx, chosen_targets);
                            }
                        }
                        StaticBody::DefilerCostReduction {
                            color,
                            life_amount,
                            filter,
                        } => {
                            if zone != Zone::Battlefield || src_ctrl != caster {
                                continue;
                            }
                            let sp_ctx = EvalContext {
                                zone_override: Some(Zone::Stack),
                                ..src_ctx
                            };
                            if matches_object_filter(state, object, filter, &sp_ctx) {
                                defiler_phyrexian_available = Some((*color, *life_amount));
                            }
                        }
                        StaticBody::CommanderTaxAsLife { life_per_cast } => {
                            if zone == Zone::Battlefield
                                && src_ctrl == caster
                                && option.from_zone == Zone::Command
                                && commander::is_commander_object(state, object)
                            {
                                commander_tax_as_life_available = Some(*life_per_cast);
                            }
                        }
                        StaticBody::SpellGrantKeyword {
                            filter, keywords, ..
                        } => {
                            if zone != Zone::Battlefield || src_ctrl != caster {
                                continue;
                            }
                            let sp_ctx = EvalContext {
                                zone_override: Some(Zone::Stack),
                                ..src_ctx
                            };
                            if matches_object_filter(state, object, filter, &sp_ctx) {
                                for &kw in keywords {
                                    if let Some(nm) = state.db().keyword_name(kw) {
                                        match nm.to_ascii_lowercase().as_str() {
                                            "convoke" => allows_convoke = true,
                                            "improvise" => allows_improvise = true,
                                            "delve" => allows_delve = true,
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if let Some(xv) = x_value
        && additional_mana.x_count > 0
    {
        let x_add = xv.saturating_mul(u32::from(additional_mana.x_count));
        additional_mana.generic = additional_mana.generic.saturating_add(x_add);
        additional_mana.x_count = 0;
    }

    let mut working = base_mana.clone();
    let cmd_tax = commander::commander_tax_generic(state, object, option.from_zone);
    acc.apply_to_cost(&mut working, &additional_mana, cmd_tax);
    non_mana_costs.extend(acc.extra_non_mana_costs);

    Ok(EffectiveCostBreakdown {
        base_mana,
        locked_mana: working,
        non_mana_costs,
        optional_costs,
        allows_convoke,
        allows_improvise,
        allows_delve,
        allows_emerge,
        x_spend_colors,
        x_colored_only,
        x_max_one_per_color,
        source_restriction,
        defiler_phyrexian_available,
        commander_tax_as_life_available,
    })
}

pub fn compute_effective_ability_cost(
    state: &GameState,
    controller: PlayerId,
    object: ObjectId,
    ability_index: u16,
    x_value: Option<u32>,
    chosen_targets: &[TargetRef],
) -> Result<EffectiveCostBreakdown, EngineError> {
    compute_effective_ability_cost_cached(
        state,
        controller,
        object,
        ability_index,
        x_value,
        chosen_targets,
        state.evaluate_board(),
    )
}

pub(crate) fn compute_effective_ability_cost_cached(
    state: &GameState,
    controller: PlayerId,
    object: ObjectId,
    ability_index: u16,
    x_value: Option<u32>,
    chosen_targets: &[TargetRef],
    cache: &LayerCache,
) -> Result<EffectiveCostBreakdown, EngineError> {
    let obj_ctrl = cache
        .get(object)
        .map(|e| e.controller)
        .or_else(|| state.object(object).map(|o| o.controller))
        .unwrap_or(controller);
    let ctx = EvalContext {
        controller: obj_ctrl,
        source: Some(object),
        targets: chosen_targets,
        board: BoardView::Cached(cache),
        ..EvalContext::new(obj_ctrl)
    };
    let abilities = mana::resolve_object_activated_abilities_cached(state, object, &ctx, cache);
    let Some(res_ab) = abilities
        .into_iter()
        .find(|a| a.ability_index == ability_index)
    else {
        return Err(EngineError::IllegalAction(format!(
            "activated ability {ability_index} not found on {object:?}"
        )));
    };
    let Some(act) = res_ab.activated else {
        // Intrinsic basic mana ability has {T} cost only
        return Ok(EffectiveCostBreakdown {
            base_mana: ManaCost::default(),
            locked_mana: ManaCost::default(),
            non_mana_costs: vec![CostDef::TapSelf],
            optional_costs: Vec::new(),
            allows_convoke: false,
            allows_improvise: false,
            allows_delve: false,
            allows_emerge: false,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
            source_restriction: None,
            defiler_phyrexian_available: None,
            commander_tax_as_life_available: None,
        });
    };

    let mut base_mana = ManaCost::default();
    let mut non_mana_costs = Vec::new();
    let mut x_spend_colors = act.x_spend_colors;
    let mut x_colored_only = act.x_colored_only;
    let mut x_max_one_per_color = act.x_max_one_per_color;

    for c in &act.cost {
        match c {
            CostDef::Mana {
                cost,
                x_spend_colors: xc,
                x_colored_only: xco,
                x_max_one_per_color: xmo,
            } => {
                add_mana_cost(&mut base_mana, cost);
                x_spend_colors |= *xc;
                x_colored_only |= *xco;
                x_max_one_per_color |= *xmo;
            }
            other => non_mana_costs.push(other.clone()),
        }
    }

    if let Some(xv) = x_value
        && base_mana.x_count > 0
    {
        base_mana.generic = base_mana
            .generic
            .saturating_add(xv.saturating_mul(u32::from(base_mana.x_count)));
        base_mana.x_count = 0;
    }

    let mut working = base_mana.clone();
    let mut acc = CostDeltaAccumulator::default();

    // Check activation_cost_reduction on the ability itself
    if let Some(red) = &act.activation_cost_reduction
        && red
            .condition
            .as_deref()
            .is_none_or(|c| evaluate_condition(state, c, &ctx))
    {
        acc.push_delta(state, &red.delta, &ctx, chosen_targets);
    }

    // Check FirstActivationZeroCost & CostModifier for abilities
    let cost_kw_name = act
        .cost_keyword
        .and_then(|kw| state.db().keyword_name(kw))
        .map(str::to_ascii_lowercase);
    let p_zero_mask = state
        .player(controller)
        .map_or(0, |p| p.used_first_activation_zero_mask);

    for p in state.players() {
        if p.lost {
            continue;
        }
        for &src_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(src_obj) = state.object(src_id) else {
                continue;
            };
            if src_obj.zone == Zone::Battlefield && src_obj.is_phased_out() {
                continue;
            }
            if src_obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if !object_might_have_static(state, src_id, |st| {
                matches!(
                    st.body,
                    StaticBody::FirstActivationZeroCost { .. } | StaticBody::CostModifier { .. }
                )
            }) {
                continue;
            }
            let src_ctrl = cache
                .get(src_id)
                .map_or(src_obj.controller, |e| e.controller);
            let src_ctx = EvalContext {
                controller: src_ctrl,
                source: Some(src_id),
                targets: chosen_targets,
                board: BoardView::Cached(cache),
                ..EvalContext::new(src_ctrl)
            };
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &src_ctx) {
                let AbilityDef::Static(st) = ab_ref.ability else {
                    continue;
                };
                match &st.static_def.body {
                    StaticBody::FirstActivationZeroCost {
                        ability_keyword,
                        during_your_turn_only,
                    } => {
                        if src_ctrl != controller {
                            continue;
                        }
                        if *during_your_turn_only && state.active_player() != controller {
                            continue;
                        }
                        let (bit, matches_kw) = match ability_keyword {
                            FirstActivationZeroKind::Equip => {
                                (1u8 << 0, cost_kw_name.as_deref() == Some("equip"))
                            }
                            FirstActivationZeroKind::PowerUp => (1u8 << 1, act.power_up_reduction),
                            FirstActivationZeroKind::Cycling => {
                                (1u8 << 2, cost_kw_name.as_deref() == Some("cycling"))
                            }
                        };
                        if matches_kw && (p_zero_mask & bit) == 0 {
                            acc.set_zero = true;
                        }
                    }
                    StaticBody::CostModifier {
                        affects,
                        delta,
                        who,
                        who_enchanted_player,
                        ..
                    } => {
                        if let Some(cond) = st.static_def.condition.as_deref()
                            && !evaluate_condition(state, cond, &src_ctx)
                        {
                            continue;
                        }
                        if !matches_who_player_filter(
                            state,
                            controller,
                            src_ctrl,
                            who,
                            *who_enchanted_player,
                            src_id,
                            &src_ctx,
                        ) {
                            continue;
                        }
                        if ability_matches_cost_mod_filter(state, object, act, affects, &src_ctx) {
                            acc.push_delta(state, delta, &src_ctx, chosen_targets);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    acc.apply_to_cost(&mut working, &ManaCost::default(), 0);
    non_mana_costs.extend(acc.extra_non_mana_costs);

    Ok(EffectiveCostBreakdown {
        base_mana,
        locked_mana: working,
        non_mana_costs,
        optional_costs: Vec::new(),
        allows_convoke: false,
        allows_improvise: false,
        allows_delve: false,
        allows_emerge: false,
        x_spend_colors,
        x_colored_only,
        x_max_one_per_color,
        source_restriction: None,
        defiler_phyrexian_available: None,
        commander_tax_as_life_available: None,
    })
}

fn ability_matches_cost_mod_filter(
    state: &GameState,
    source_obj: ObjectId,
    act: &mtg_ir::ActivatedAbility,
    affects: &SpellOrAbilityFilter,
    mod_ctx: &EvalContext<'_>,
) -> bool {
    match affects {
        SpellOrAbilityFilter::SelfObj => mod_ctx.source == Some(source_obj),
        SpellOrAbilityFilter::Spell { .. } => false,
        SpellOrAbilityFilter::Ability {
            keyword,
            filter,
            non_mana_only,
            loyalty_only,
            source_zone,
            self_source_only,
            attached_source_only,
            ..
        } => {
            if *non_mana_only && mana::is_activated_mana_ability(act) {
                return false;
            }
            if *loyalty_only
                && !act
                    .cost
                    .iter()
                    .any(|c| matches!(c, CostDef::Loyalty { .. }))
            {
                return false;
            }
            if let Some(kw) = keyword
                && act.cost_keyword != Some(*kw)
            {
                return false;
            }
            if let Some(sz) = source_zone
                && state.object(source_obj).is_none_or(|o| o.zone != *sz)
            {
                return false;
            }
            if *self_source_only && mod_ctx.source != Some(source_obj) {
                return false;
            }
            if *attached_source_only
                && mod_ctx
                    .source
                    .and_then(|s| state.object(s))
                    .is_none_or(|s| s.attached_to != Some(source_obj))
            {
                return false;
            }
            if let Some(f) = filter
                && !matches_object_filter(state, source_obj, f, mod_ctx)
            {
                return false;
            }
            true
        }
    }
}

fn resolve_cost_object_candidates_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    target: &CostObjectTarget,
    default_zone: ZoneMask,
    default_controller_you: bool,
    cache: &LayerCache,
) -> Vec<ObjectId> {
    match target {
        CostObjectTarget::SelfObj => {
            if state.object(source).is_some_and(|o| {
                default_zone.is_empty() || default_zone.contains(ZoneMask::from_zone(o.zone))
            }) {
                vec![source]
            } else {
                Vec::new()
            }
        }
        CostObjectTarget::Filter { filter } => {
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = default_zone;
            }
            if default_controller_you && f.controller.is_none() && f.owner.is_none() {
                f.controller = Some(mtg_ir::RelativeControl::You);
            }
            let ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
            find_objects(state, &f, &ctx)
        }
    }
}

pub(crate) fn cant_sacrifice_object(
    state: &GameState,
    payer: PlayerId,
    target: ObjectId,
    is_mana_ab: bool,
) -> bool {
    cant_sacrifice_object_cached(state, payer, target, is_mana_ab, state.evaluate_board())
}

pub(crate) fn cant_sacrifice_object_cached(
    state: &GameState,
    payer: PlayerId,
    target: ObjectId,
    is_mana_ab: bool,
    cache: &LayerCache,
) -> bool {
    let ctx = EvalContext::new(payer).with_board(BoardView::Cached(cache));

    // Check RuleMod::CantPayLifeOrSacrificeForCosts
    if let Some(ep) = cache.player(payer) {
        for rm in &ep.active_rule_mods {
            if let RuleMod::CantPayLifeOrSacrificeForCosts {
                non_mana_abilities_only,
                sacrifice_filter,
                ..
            } = &rm.rule
            {
                if *non_mana_abilities_only && is_mana_ab {
                    continue;
                }
                if let Some(sf) = sacrifice_filter
                    && matches_object_filter(state, target, sf, &ctx)
                {
                    return true;
                }
            }
        }
    }

    // Check StaticBody::Restriction { CantBeSacrificed }
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &src_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(obj) = state.object(src_id) else {
                continue;
            };
            if obj.zone == Zone::Battlefield && obj.is_phased_out() {
                continue;
            }
            if obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if !object_might_have_static(state, src_id, |st| {
                matches!(
                    st.body,
                    StaticBody::Restriction {
                        restriction: RestrictionKind::CantBeSacrificed,
                        ..
                    }
                )
            }) {
                continue;
            }
            let src_ctrl = cache.get(src_id).map_or(obj.controller, |e| e.controller);
            let sctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &sctx) {
                if let AbilityDef::Static(st) = ab_ref.ability
                    && let StaticBody::Restriction {
                        affected,
                        restriction: RestrictionKind::CantBeSacrificed,
                    } = &st.static_def.body
                {
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &sctx)
                    {
                        continue;
                    }
                    if matches_affected(state, target, affected, &sctx) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn cant_crew_vehicle_cached(state: &GameState, creature: ObjectId, cache: &LayerCache) -> bool {
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &src_id in p.battlefield.iter() {
            let Some(obj) = state.object(src_id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            if !object_might_have_static(state, src_id, |st| {
                matches!(
                    st.body,
                    StaticBody::Restriction {
                        restriction: RestrictionKind::CantCrewVehicles,
                        ..
                    }
                )
            }) {
                continue;
            }
            let src_ctrl = cache.get(src_id).map_or(obj.controller, |e| e.controller);
            let sctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &sctx) {
                if let AbilityDef::Static(st) = ab_ref.ability
                    && let StaticBody::Restriction {
                        affected,
                        restriction: RestrictionKind::CantCrewVehicles,
                    } = &st.static_def.body
                    && matches_affected(state, creature, affected, &sctx)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn can_pay_branch_with_pool_mana_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    branch: &[CostDef],
    x_value: Option<u32>,
    cache: &LayerCache,
) -> bool {
    if !can_pay_non_mana_costs_cached(state, payer, source, branch, x_value, cache) {
        return false;
    }
    let mut combined_mana = ManaCost::default();
    let mut has_mana = false;
    for c in branch {
        match c {
            CostDef::Mana { cost: mc, .. } => {
                has_mana = true;
                combined_mana.generic = combined_mana.generic.saturating_add(mc.generic);
                if mc.x_count > 0
                    && let Some(xv) = x_value
                {
                    combined_mana.generic = combined_mana
                        .generic
                        .saturating_add(xv.saturating_mul(u32::from(mc.x_count)));
                }
                combined_mana.pips.extend(mc.pips.iter().copied());
            }
            CostDef::Choice { options, forage }
                if !*forage
                    && !options.iter().any(|sub| {
                        can_pay_branch_with_pool_mana_cached(
                            state, payer, source, sub, x_value, cache,
                        )
                    }) =>
            {
                return false;
            }
            _ => {}
        }
    }
    if has_mana {
        let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
        if !mana::can_pay_mana_from_pool_cached(state, payer, &combined_mana, &spend_ctx, cache) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CostAidCaps {
    pub allows_convoke: bool,
    pub allows_improvise: bool,
    pub allows_delve: bool,
    pub allows_emerge: bool,
}

impl CostAidCaps {
    pub(crate) fn from_breakdown(bd: &EffectiveCostBreakdown) -> Self {
        Self {
            allows_convoke: bd.allows_convoke,
            allows_improvise: bd.allows_improvise,
            allows_delve: bd.allows_delve,
            allows_emerge: bd.allows_emerge,
        }
    }

    pub(crate) fn any(self) -> bool {
        self.allows_convoke || self.allows_improvise || self.allows_delve || self.allows_emerge
    }
}

pub(crate) fn convoke_can_reduce_mana(
    remaining_mana: &ManaCost,
    creature_colors: ColorMask,
) -> bool {
    if remaining_mana.generic > 0 {
        return true;
    }
    let colors = creature_colors & ColorMask::ALL_COLORS;
    mana::FIVE_COLORS.iter().any(|&c| {
        colors.contains(ColorMask::from_mana_color(c))
            && remaining_mana.pips.iter().any(|p| pip_matches_color(*p, c))
    })
}

pub(crate) fn delve_reserved_gy_cards_cached(
    state: &GameState,
    payer: PlayerId,
    rem_non_mana: &[CostDef],
    cache: &LayerCache,
) -> usize {
    let (source, x_value) = state
        .casting()
        .filter(|cs| cs.caster == payer)
        .map_or((NONE_OBJECT_ID, None), |cs| (cs.object, cs.x_value));
    delve_reserved_gy_cards_with_x_cached(
        state,
        payer,
        source,
        rem_non_mana,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

fn delve_reserved_gy_cards_with_x_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    rem_non_mana: &[CostDef],
    x_value: Option<u32>,
    self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> usize {
    let mut ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
    ctx.x_value = x_value;
    let mut reserved = 0usize;
    for cost in rem_non_mana {
        match cost {
            CostDef::ExileFrom {
                zone: Zone::Graveyard,
                count,
                from_top,
                any_graveyard,
                min_total_mana_value,
                min_total_mana_value_amount,
                collect_evidence,
                one_or_more,
                ..
            } if !*from_top => {
                let mut need = if *one_or_more {
                    1
                } else {
                    evaluate_amount(state, count, &ctx).max(0) as usize
                };
                if need == 0
                    && (*collect_evidence
                        || min_total_mana_value.is_some()
                        || min_total_mana_value_amount.is_some())
                {
                    let req_mv = min_total_mana_value.map(u32::from).unwrap_or_else(|| {
                        min_total_mana_value_amount
                            .as_deref()
                            .map_or(0, |a| evaluate_amount(state, a, &ctx).max(0) as u32)
                    });
                    if req_mv > 0 {
                        need = 1;
                    }
                }
                if *any_graveyard {
                    let opp_gy: usize = state
                        .players()
                        .iter()
                        .filter(|p| !p.lost && p.id != payer)
                        .map(|p| p.graveyard.len())
                        .sum();
                    need = need.saturating_sub(opp_gy);
                }
                reserved = reserved.saturating_add(need);
            }
            CostDef::Choice { options, forage } => {
                if *forage {
                    if !has_controlled_food_cached(state, payer, cache) {
                        reserved = reserved.saturating_add(3);
                    }
                } else if !options.is_empty() {
                    let spend_ctx = ManaSpendContext::for_generic_cost(payer);
                    let mut min_branch: Option<usize> = None;
                    for branch in options {
                        if can_pay_costs_and_mana_with_self_ctx_cached(
                            state,
                            payer,
                            source,
                            &ManaCost::default(),
                            branch,
                            &spend_ctx,
                            CostAidCaps::default(),
                            x_value,
                            self_ctx,
                            cache,
                        ) {
                            let br = delve_reserved_gy_cards_with_x_cached(
                                state, payer, source, branch, x_value, self_ctx, cache,
                            );
                            min_branch = Some(min_branch.map_or(br, |m| m.min(br)));
                        }
                    }
                    if let Some(br) = min_branch {
                        reserved = reserved.saturating_add(br);
                    }
                }
            }
            _ => {}
        }
    }
    reserved
}

pub(crate) fn amount_uses_x(amt: &Amount) -> bool {
    match amt {
        Amount::X | Amount::AnyNumber => true,
        Amount::Negate { of } | Amount::Mul { of, .. } | Amount::Half { of, .. } => {
            amount_uses_x(of)
        }
        Amount::Sum { args } | Amount::Min { args } | Amount::Max { args } => {
            args.iter().any(amount_uses_x)
        }
        Amount::Difference { left, right } => amount_uses_x(left) || amount_uses_x(right),
        Amount::Conditional {
            then_amt, else_amt, ..
        } => amount_uses_x(then_amt) || amount_uses_x(else_amt),
        _ => false,
    }
}

pub(crate) fn cost_uses_x(cost: &CostDef) -> bool {
    match cost {
        CostDef::Mana { cost, .. } | CostDef::Waterbend { cost } => cost.x_count > 0,
        CostDef::Loyalty { is_x, .. } => *is_x,
        CostDef::ExileFrom {
            count,
            min_total_mana_value_amount,
            total_mana_value_equals_x,
            one_or_more,
            ..
        } => {
            *total_mana_value_equals_x
                || (!*one_or_more && amount_uses_x(count))
                || min_total_mana_value_amount
                    .as_deref()
                    .is_some_and(amount_uses_x)
        }
        CostDef::Sacrifice {
            count,
            sacrifice_all,
            one_or_more,
            up_to,
            ..
        } => !*sacrifice_all && !*one_or_more && up_to.is_none() && amount_uses_x(count),
        CostDef::RemoveCounters {
            count,
            remove_all,
            one_or_more,
            ..
        } => !*remove_all && !*one_or_more && amount_uses_x(count),
        CostDef::PayLife {
            amount,
            half_life_rounded_up,
        } => !*half_life_rounded_up && amount_uses_x(amount),
        CostDef::OpponentGainsLife { amount, .. } => amount_uses_x(amount),
        CostDef::TapSubsetSum { min_total, .. } => amount_uses_x(min_total),
        CostDef::Discard { count, .. }
        | CostDef::RevealFromHand { count, .. }
        | CostDef::ReturnToHand { count, .. }
        | CostDef::TapOthers { count, .. }
        | CostDef::UntapOthers { count, .. }
        | CostDef::PutCounters { count, .. }
        | CostDef::Mill { count }
        | CostDef::PayEnergy { count }
        | CostDef::PutFromHandOnTopOfLibrary { count, .. }
        | CostDef::PutOnBottomOfLibrary { count, .. } => amount_uses_x(count),
        CostDef::Choice { options, forage } => {
            !*forage && options.iter().any(|b| b.iter().any(cost_uses_x))
        }
        CostDef::Optional { cost } => cost.iter().any(cost_uses_x),
        _ => false,
    }
}

fn invert_amount_x_bound(state: &GameState, amt: &Amount, cap: i32, ctx: &EvalContext<'_>) -> i32 {
    let cap = cap.max(0);
    match amt {
        Amount::X | Amount::AnyNumber => cap,
        Amount::Mul { factor, of } => {
            if *factor <= 0 {
                i32::MAX
            } else {
                invert_amount_x_bound(state, of, cap / *factor, ctx)
            }
        }
        Amount::Half { of, round } => {
            let sub_cap = match round {
                RoundMode::Down => cap.saturating_mul(2).saturating_add(1),
                RoundMode::Up => cap.saturating_mul(2),
            };
            invert_amount_x_bound(state, of, sub_cap, ctx)
        }
        Amount::Sum { args } => {
            let mut fixed = 0i32;
            let mut x_args: SmallVec<[&Amount; 4]> = SmallVec::new();
            for a in args {
                if amount_uses_x(a) {
                    x_args.push(a);
                } else {
                    fixed = fixed.saturating_add(evaluate_amount(state, a, ctx));
                }
            }
            if x_args.is_empty() {
                return i32::MAX;
            }
            let rem = cap.saturating_sub(fixed).max(0);
            if x_args
                .iter()
                .all(|a| matches!(a, Amount::X | Amount::AnyNumber))
            {
                rem / (x_args.len() as i32)
            } else {
                x_args
                    .into_iter()
                    .map(|a| invert_amount_x_bound(state, a, rem, ctx))
                    .min()
                    .unwrap_or(i32::MAX)
            }
        }
        Amount::Difference { left, right } => {
            if !amount_uses_x(right) {
                let r = evaluate_amount(state, right, ctx);
                invert_amount_x_bound(state, left, cap.saturating_add(r).max(0), ctx)
            } else {
                i32::MAX
            }
        }
        Amount::Min { args } => {
            let mut has_non_x_ok = false;
            let mut best = 0i32;
            for a in args {
                if !amount_uses_x(a) {
                    if evaluate_amount(state, a, ctx) <= cap {
                        has_non_x_ok = true;
                    }
                } else {
                    best = best.max(invert_amount_x_bound(state, a, cap, ctx));
                }
            }
            if has_non_x_ok { i32::MAX } else { best }
        }
        Amount::Max { args } => args
            .iter()
            .filter(|a| amount_uses_x(a))
            .map(|a| invert_amount_x_bound(state, a, cap, ctx))
            .min()
            .unwrap_or(i32::MAX),
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            if evaluate_condition(state, condition, ctx) {
                invert_amount_x_bound(state, then_amt, cap, ctx)
            } else {
                invert_amount_x_bound(state, else_amt, cap, ctx)
            }
        }
        Amount::Negate { .. } => i32::MAX,
        _ => i32::MAX,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CostListSelfContext {
    taps_self: bool,
    untaps_self: bool,
    tap_attached_host: Option<ObjectId>,
    leaves_bf_self: bool,
    discards_self: bool,
    exiles_self_from_gy: bool,
    cast_left_zone: Option<Zone>,
}

impl CostListSelfContext {
    pub(crate) fn for_spell_cast(from_zone: Zone) -> Self {
        Self {
            cast_left_zone: Some(from_zone),
            ..Self::default()
        }
    }

    fn from_costs(state: &GameState, source: ObjectId, costs: &[CostDef]) -> Self {
        let mut ctx = Self::default();
        for cost in costs {
            match cost {
                CostDef::TapSelf => ctx.taps_self = true,
                CostDef::UntapSelf { .. } => ctx.untaps_self = true,
                CostDef::TapAttached => {
                    ctx.tap_attached_host = state.object(source).and_then(|o| o.attached_to);
                }
                CostDef::Sacrifice {
                    target: CostObjectTarget::SelfObj,
                    attached_creature: false,
                    ..
                }
                | CostDef::ExileFrom {
                    zone: Zone::Battlefield,
                    target: Some(CostObjectTarget::SelfObj),
                    from_top: false,
                    ..
                }
                | CostDef::ReturnToHand {
                    target: CostObjectTarget::SelfObj,
                    ..
                }
                | CostDef::PutOnBottomOfLibrary {
                    from: Zone::Battlefield,
                    target: Some(CostObjectTarget::SelfObj),
                    ..
                } => {
                    ctx.leaves_bf_self = true;
                }
                CostDef::Discard {
                    target: Some(CostObjectTarget::SelfObj),
                    ..
                }
                | CostDef::PutOnBottomOfLibrary {
                    from: Zone::Hand,
                    target: Some(CostObjectTarget::SelfObj),
                    ..
                } => {
                    ctx.discards_self = true;
                }
                CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    target: Some(CostObjectTarget::SelfObj),
                    from_top: false,
                    ..
                }
                | CostDef::PutOnBottomOfLibrary {
                    from: Zone::Graveyard,
                    target: Some(CostObjectTarget::SelfObj),
                    ..
                } => {
                    ctx.exiles_self_from_gy = true;
                }
                _ => {}
            }
        }
        ctx
    }

    fn merged(self, other: Self) -> Self {
        Self {
            taps_self: self.taps_self || other.taps_self,
            untaps_self: self.untaps_self || other.untaps_self,
            tap_attached_host: self.tap_attached_host.or(other.tap_attached_host),
            leaves_bf_self: self.leaves_bf_self || other.leaves_bf_self,
            discards_self: self.discards_self || other.discards_self,
            exiles_self_from_gy: self.exiles_self_from_gy || other.exiles_self_from_gy,
            cast_left_zone: self.cast_left_zone.or(other.cast_left_zone),
        }
    }

    fn excludes_self_in_zone(self, zone: Zone, target: Option<&CostObjectTarget>) -> bool {
        if self.cast_left_zone == Some(zone) {
            return true;
        }
        if matches!(target, Some(CostObjectTarget::SelfObj)) {
            return false;
        }
        match zone {
            Zone::Battlefield => self.leaves_bf_self,
            Zone::Hand => self.discards_self,
            Zone::Graveyard => self.exiles_self_from_gy,
            _ => false,
        }
    }

    fn excludes_object_in_zone(
        self,
        source: ObjectId,
        zone: Zone,
        target: Option<&CostObjectTarget>,
        id: ObjectId,
    ) -> bool {
        id == source && self.excludes_self_in_zone(zone, target)
    }

    fn excludes_gy_self(self) -> bool {
        self.cast_left_zone == Some(Zone::Graveyard) || self.exiles_self_from_gy
    }

    fn excludes_lib_self(self) -> bool {
        self.cast_left_zone == Some(Zone::Library)
    }
}

fn effective_gy_len_for_cost(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    self_ctx: CostListSelfContext,
) -> usize {
    let raw = state.player(payer).map_or(0, |p| p.graveyard.len());
    if self_ctx.excludes_gy_self()
        && state
            .object(source)
            .is_some_and(|o| o.zone == Zone::Graveyard && o.owner == payer)
    {
        raw.saturating_sub(1)
    } else {
        raw
    }
}

fn effective_lib_len_for_cost(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    self_ctx: CostListSelfContext,
) -> usize {
    let raw = state.player(payer).map_or(0, |p| p.library.len());
    if self_ctx.excludes_lib_self()
        && state
            .object(source)
            .is_some_and(|o| o.zone == Zone::Library && o.owner == payer)
    {
        raw.saturating_sub(1)
    } else {
        raw
    }
}

pub(crate) fn max_x_for_cost_list_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    cache: &LayerCache,
) -> i32 {
    max_x_for_cost_list_with_ctx_cached(
        state,
        payer,
        source,
        costs,
        CostListSelfContext::default(),
        cache,
    )
}

pub(crate) fn max_x_for_cost_list_with_ctx_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> i32 {
    let ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
    let self_ctx = outer_self_ctx.merged(CostListSelfContext::from_costs(state, source, costs));
    let mut best = i32::MAX;

    for cost in costs {
        match cost {
            CostDef::Mana { .. } | CostDef::Waterbend { .. } | CostDef::Optional { .. } => {}
            CostDef::Choice { options, forage } => {
                if !*forage && !options.is_empty() {
                    let spend_ctx = ManaSpendContext::for_generic_cost(payer);
                    let mut choice_max = 0i32;
                    for branch in options {
                        if !branch.iter().any(cost_uses_x) {
                            if can_pay_costs_and_mana_with_self_ctx_cached(
                                state,
                                payer,
                                source,
                                &ManaCost::default(),
                                branch,
                                &spend_ctx,
                                CostAidCaps::default(),
                                None,
                                self_ctx,
                                cache,
                            ) {
                                choice_max = i32::MAX;
                                break;
                            }
                        } else {
                            let br = max_x_for_cost_list_with_ctx_cached(
                                state, payer, source, branch, self_ctx, cache,
                            );
                            choice_max = choice_max.max(br);
                        }
                    }
                    best = best.min(choice_max);
                }
            }
            CostDef::Loyalty { is_x, sign, delta } => {
                if *is_x && (matches!(sign, Some(LoyaltySign::Minus)) || *delta < 0) {
                    let cur_loy = state
                        .db()
                        .counter_id("loyalty")
                        .map_or(0, |cid| state.counter_count(source, cid) as i32);
                    best = best.min(cur_loy.max(0));
                }
            }
            CostDef::ExileFrom {
                zone,
                count,
                target,
                from_top,
                any_graveyard,
                min_total_mana_value_amount,
                one_or_more,
                total_mana_value_equals_x,
                ..
            } => {
                if *from_top && *zone == Zone::Library {
                    if !*one_or_more && amount_uses_x(count) {
                        let lib_len =
                            effective_lib_len_for_cost(state, payer, source, self_ctx) as i32;
                        best = best.min(invert_amount_x_bound(state, count, lib_len, &ctx));
                    }
                } else {
                    let zm = ZoneMask::from_zone(*zone);
                    let mut cands: Vec<ObjectId> = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            zm,
                            !*any_graveyard,
                            cache,
                        ),
                        None => state
                            .players()
                            .iter()
                            .filter(|p| !p.lost && (*any_graveyard || p.id == payer))
                            .flat_map(|p| match zone {
                                Zone::Graveyard => p.graveyard.iter().copied(),
                                Zone::Hand => p.hand.iter().copied(),
                                Zone::Library => p.library.iter().copied(),
                                Zone::Battlefield => p.battlefield.iter().copied(),
                                Zone::Exile => p.exile.iter().copied(),
                                Zone::Command => p.command.iter().copied(),
                                Zone::Stack => [].iter().copied(),
                            })
                            .collect(),
                    };
                    cands.retain(|&id| {
                        !self_ctx.excludes_object_in_zone(source, *zone, target.as_ref(), id)
                    });
                    if *total_mana_value_equals_x {
                        let total_mv: i32 = cands
                            .iter()
                            .map(|&id| object_mana_value(state, id, &ctx) as i32)
                            .sum();
                        best = best.min(total_mv.max(0));
                    }
                    if let Some(mv_amt) = min_total_mana_value_amount.as_deref()
                        && amount_uses_x(mv_amt)
                    {
                        let total_mv: i32 = cands
                            .iter()
                            .map(|&id| object_mana_value(state, id, &ctx) as i32)
                            .sum();
                        best =
                            best.min(invert_amount_x_bound(state, mv_amt, total_mv.max(0), &ctx));
                    }
                    if !*one_or_more && amount_uses_x(count) {
                        best = best.min(invert_amount_x_bound(
                            state,
                            count,
                            cands.len() as i32,
                            &ctx,
                        ));
                    }
                }
            }
            CostDef::Sacrifice {
                count,
                target,
                sacrifice_all,
                attached_creature,
                one_or_more,
                up_to,
                ..
            } => {
                if !*attached_creature
                    && !*sacrifice_all
                    && !*one_or_more
                    && up_to.is_none()
                    && amount_uses_x(count)
                {
                    let exclude_self =
                        self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
                    let cands_len = resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        target,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .into_iter()
                    .filter(|&id| {
                        (!exclude_self || id != source)
                            && state
                                .object(id)
                                .is_some_and(|o| o.zone == Zone::Battlefield)
                            && !cant_sacrifice_object_cached(state, payer, id, false, cache)
                    })
                    .count() as i32;
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::Discard { count, target, .. } => {
                if amount_uses_x(count) {
                    let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
                    let cands_len = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            ZoneMask::HAND,
                            false,
                            cache,
                        )
                        .into_iter()
                        .filter(|&id| !exclude_self || id != source)
                        .count() as i32,
                        None => state.player(payer).map_or(0, |p| {
                            p.hand
                                .iter()
                                .filter(|&&id| !exclude_self || id != source)
                                .count() as i32
                        }),
                    };
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::RevealFromHand { count, target, .. } => {
                if amount_uses_x(count) {
                    let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
                    let cands_len = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            ZoneMask::HAND,
                            false,
                            cache,
                        )
                        .into_iter()
                        .filter(|&id| !exclude_self || id != source)
                        .count() as i32,
                        None => state.player(payer).map_or(0, |p| {
                            p.hand
                                .iter()
                                .filter(|&&id| !exclude_self || id != source)
                                .count() as i32
                        }),
                    };
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::ReturnToHand { count, target } => {
                if amount_uses_x(count) {
                    let exclude_self =
                        self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
                    let cands_len = resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        target,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .into_iter()
                    .filter(|&id| !exclude_self || id != source)
                    .count() as i32;
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::PutFromHandOnTopOfLibrary { count, filter } => {
                if amount_uses_x(count) {
                    let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, None);
                    let cands_len = match filter {
                        Some(f) => {
                            let mut hf = (**f).clone();
                            hf.zones = ZoneMask::HAND;
                            find_objects(state, &hf, &ctx)
                                .into_iter()
                                .filter(|&id| !exclude_self || id != source)
                                .count() as i32
                        }
                        None => state.player(payer).map_or(0, |p| {
                            p.hand
                                .iter()
                                .filter(|&&id| !exclude_self || id != source)
                                .count() as i32
                        }),
                    };
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::PutOnBottomOfLibrary {
                from,
                count,
                target,
            } => {
                if amount_uses_x(count) {
                    let zm = ZoneMask::from_zone(*from);
                    let exclude_self = self_ctx.excludes_self_in_zone(*from, target.as_ref());
                    let cands_len = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state, payer, source, t, zm, true, cache,
                        )
                        .into_iter()
                        .filter(|&id| !exclude_self || id != source)
                        .count() as i32,
                        None => state.player(payer).map_or(0, |p| {
                            let slice = match from {
                                Zone::Hand => p.hand.as_slice(),
                                Zone::Graveyard => p.graveyard.as_slice(),
                                Zone::Battlefield => p.battlefield.as_slice(),
                                _ => &[],
                            };
                            slice
                                .iter()
                                .filter(|&&id| !exclude_self || id != source)
                                .count() as i32
                        }),
                    };
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::TapOthers { count, filter, .. } => {
                if amount_uses_x(count) {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let cands_len = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.taps_self && id == source)
                                && self_ctx.tap_attached_host != Some(id)
                                && state
                                    .object(id)
                                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .count() as i32;
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::UntapOthers { count, filter } => {
                if amount_uses_x(count) {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let cands_len = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.untaps_self && id == source)
                                && state
                                    .object(id)
                                    .is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .count() as i32;
                    best = best.min(invert_amount_x_bound(state, count, cands_len, &ctx));
                }
            }
            CostDef::TapSubsetSum {
                min_total,
                filter,
                exclude_self,
                ..
            } => {
                if amount_uses_x(min_total) {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let total_avail: i32 = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !((*exclude_self || self_ctx.taps_self) && id == source)
                                && self_ctx.tap_attached_host != Some(id)
                                && state
                                    .object(id)
                                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                                && !cant_crew_vehicle_cached(state, id, cache)
                        })
                        .filter_map(|id| {
                            crate::eval::object_pt(state, id, &ctx).map(|pt| pt.power.max(0))
                        })
                        .sum();
                    best = best.min(invert_amount_x_bound(
                        state,
                        min_total,
                        total_avail.max(0),
                        &ctx,
                    ));
                }
            }
            CostDef::RemoveCounters {
                counter,
                count,
                remove_all,
                from,
                among,
                one_or_more,
                ..
            } => {
                if !*remove_all && !*one_or_more && amount_uses_x(count) {
                    let cands = resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        from,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    );
                    let mut total_avail = 0u32;
                    let mut max_single_avail = 0u32;
                    for id in cands {
                        let avail: u32 = match counter {
                            CounterTarget::Specific(cid) => state.counter_count(id, *cid),
                            CounterTarget::Any | CounterTarget::Each => {
                                state.counters(id).iter().map(|(_, n)| *n).sum()
                            }
                        };
                        total_avail = total_avail.saturating_add(avail);
                        max_single_avail = max_single_avail.max(avail);
                    }
                    let cap = if *among {
                        total_avail
                    } else {
                        max_single_avail
                    };
                    best = best.min(invert_amount_x_bound(state, count, cap as i32, &ctx));
                }
            }
            CostDef::PayLife {
                amount,
                half_life_rounded_up,
            } => {
                if !*half_life_rounded_up && amount_uses_x(amount) {
                    let spend_ctx = ManaSpendContext::for_generic_cost(payer);
                    if mana::is_cant_pay_life_cached(state, payer, &spend_ctx, cache) {
                        best = 0;
                    } else {
                        let life = state.player(payer).map_or(0, |p| p.life).max(0);
                        best = best.min(invert_amount_x_bound(state, amount, life, &ctx));
                    }
                }
            }
            CostDef::PayEnergy { count } => {
                if amount_uses_x(count) {
                    let energy = state.player(payer).map_or(0, |p| i32::from(p.energy));
                    best = best.min(invert_amount_x_bound(state, count, energy, &ctx));
                }
            }
            CostDef::Mill { count } => {
                if amount_uses_x(count) {
                    let lib_len = effective_lib_len_for_cost(state, payer, source, self_ctx) as i32;
                    best = best.min(invert_amount_x_bound(state, count, lib_len, &ctx));
                }
            }
            CostDef::PutCounters { count, on, .. }
                if amount_uses_x(count)
                    && resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        on,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .is_empty() =>
            {
                best = best.min(invert_amount_x_bound(state, count, 0, &ctx));
            }
            _ => {}
        }
    }

    best.max(0)
}

fn subset_sum_exists(weights: &[u32], target: u32) -> bool {
    if target == 0 {
        return true;
    }
    let total: u32 = weights.iter().copied().sum();
    if total < target {
        return false;
    }
    let target_usize = target as usize;
    if target_usize > 256 {
        return true;
    }
    let mut dp = vec![false; target_usize + 1];
    dp[0] = true;
    for &w in weights {
        let wu = w as usize;
        if wu == 0 || wu > target_usize {
            continue;
        }
        for s in (wu..=target_usize).rev() {
            if dp[s - wu] {
                dp[s] = true;
            }
        }
        if dp[target_usize] {
            return true;
        }
    }
    dp[target_usize]
}

#[derive(Clone, Copy)]
struct AidTapCand {
    id: ObjectId,
    colors: ColorMask,
    can_convoke: bool,
    can_improvise: bool,
    is_mana_source: bool,
    in_reserved: bool,
}

fn commit_aid_tap(constraints: &mut AutoTapConstraints, id: ObjectId) {
    push_unique_oid(&mut constraints.untappable_objects, id);
    for grp in &mut constraints.reserved_groups {
        if grp.conflict == ReservedGroupConflict::MustStayUntappedAndOnBf {
            grp.candidates.retain(|c| *c != id);
        }
    }
    for tss in &mut constraints.tap_subset_sum {
        tss.candidates.retain(|(c, _)| *c != id);
    }
}

fn mana_cost_is_zero(cost: &ManaCost) -> bool {
    cost.generic == 0 && cost.x_count == 0 && cost.pips.is_empty()
}

fn can_pay_mana_directly_cached(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    spend_ctx: &ManaSpendContext,
    constraints: &AutoTapConstraints,
    cache: &LayerCache,
) -> bool {
    if mana_cost_is_zero(cost) {
        return true;
    }
    if mana::can_pay_mana_from_pool_with_life_reserve_cached(
        state,
        player,
        cost,
        spend_ctx,
        constraints.reserved_life,
        cache,
    ) {
        return true;
    }
    plan_auto_tap_with_constraints_cached(
        state,
        player,
        cost,
        spend_ctx,
        AutoTapPolicy::default(),
        constraints,
        cache,
    )
    .can_pay
}

#[allow(clippy::too_many_arguments)]
fn can_pay_convoke_improvise_dfs(
    state: &GameState,
    player: PlayerId,
    rem_cost: &ManaCost,
    pip_cursor: usize,
    spend_ctx: &ManaSpendContext,
    constraints: &AutoTapConstraints,
    cands: &[AidTapCand],
    used: &mut [bool],
    has_mana_source_or_pool: bool,
    node_budget: &mut usize,
    auto_tap_budget: &mut usize,
    cache: &LayerCache,
) -> bool {
    if mana_cost_is_zero(rem_cost) {
        return true;
    }
    if *node_budget == 0 {
        return false;
    }
    *node_budget -= 1;

    if pip_cursor < rem_cost.pips.len() {
        let pip = rem_cost.pips[pip_cursor];
        let mut rem_pip_colors = ColorMask::empty();
        for &p in &rem_cost.pips[pip_cursor..] {
            for c in mana::FIVE_COLORS {
                if pip_matches_color(p, c) {
                    rem_pip_colors |= ColorMask::from_mana_color(c);
                }
            }
        }
        let mut seen_classes: SmallVec<[(ColorMask, bool); 8]> = SmallVec::new();
        for (idx, cand) in cands.iter().enumerate() {
            if used[idx] || !cand.can_convoke || !constraints.allows_tap(cand.id) {
                continue;
            }
            let matches_this_pip = mana::FIVE_COLORS.iter().any(|&c| {
                cand.colors.contains(ColorMask::from_mana_color(c)) && pip_matches_color(pip, c)
            });
            if !matches_this_pip {
                continue;
            }
            if !cand.in_reserved {
                let cls = (cand.colors & rem_pip_colors, cand.is_mana_source);
                if seen_classes.contains(&cls) {
                    continue;
                }
                seen_classes.push(cls);
            }
            let mut next_cost = rem_cost.clone();
            next_cost.pips.remove(pip_cursor);
            let mut next_constraints = constraints.clone();
            commit_aid_tap(&mut next_constraints, cand.id);
            used[idx] = true;
            let ok = can_pay_convoke_improvise_dfs(
                state,
                player,
                &next_cost,
                pip_cursor,
                spend_ctx,
                &next_constraints,
                cands,
                used,
                has_mana_source_or_pool,
                node_budget,
                auto_tap_budget,
                cache,
            );
            used[idx] = false;
            if ok {
                return true;
            }
        }

        let is_phy_pip = matches!(
            pip,
            ManaPip::Phyrexian { .. } | ManaPip::HybridPhyrexian { .. }
        );
        let can_pay_phy_life = is_phy_pip
            && !mana::is_cant_pay_life_cached(state, player, spend_ctx, cache)
            && state
                .player(player)
                .is_some_and(|p| p.life.max(0) as u32 >= constraints.reserved_life + 2);
        if has_mana_source_or_pool || can_pay_phy_life {
            return can_pay_convoke_improvise_dfs(
                state,
                player,
                rem_cost,
                pip_cursor + 1,
                spend_ctx,
                constraints,
                cands,
                used,
                has_mana_source_or_pool,
                node_budget,
                auto_tap_budget,
                cache,
            );
        }
        return false;
    }

    // All pips decided: greedily apply pure-aid taps to generic, then check pool/auto-tap,
    // and only if still needed try tapping mana-source aid permanents for generic.
    let mut leaf_cost = rem_cost.clone();
    let mut leaf_constraints = constraints.clone();
    let mut local_used: SmallVec<[usize; 16]> = SmallVec::new();
    for (idx, cand) in cands.iter().enumerate() {
        if leaf_cost.generic == 0 {
            break;
        }
        if !used[idx]
            && !cand.is_mana_source
            && (cand.can_convoke || cand.can_improvise)
            && leaf_constraints.allows_tap(cand.id)
        {
            leaf_cost.generic -= 1;
            commit_aid_tap(&mut leaf_constraints, cand.id);
            local_used.push(idx);
        }
    }

    if mana_cost_is_zero(&leaf_cost) {
        return true;
    }
    if mana::can_pay_mana_from_pool_with_life_reserve_cached(
        state,
        player,
        &leaf_cost,
        spend_ctx,
        leaf_constraints.reserved_life,
        cache,
    ) {
        return true;
    }
    if *auto_tap_budget > 0 {
        *auto_tap_budget -= 1;
        if plan_auto_tap_with_constraints_cached(
            state,
            player,
            &leaf_cost,
            spend_ctx,
            AutoTapPolicy::default(),
            &leaf_constraints,
            cache,
        )
        .can_pay
        {
            return true;
        }
    }

    if leaf_cost.generic > 0 {
        for (idx, cand) in cands.iter().enumerate() {
            if leaf_cost.generic == 0 {
                break;
            }
            if !used[idx]
                && cand.is_mana_source
                && !local_used.contains(&idx)
                && (cand.can_convoke || cand.can_improvise)
                && leaf_constraints.allows_tap(cand.id)
            {
                leaf_cost.generic -= 1;
                commit_aid_tap(&mut leaf_constraints, cand.id);
                if mana_cost_is_zero(&leaf_cost) {
                    return true;
                }
                if mana::can_pay_mana_from_pool_with_life_reserve_cached(
                    state,
                    player,
                    &leaf_cost,
                    spend_ctx,
                    leaf_constraints.reserved_life,
                    cache,
                ) {
                    return true;
                }
                if *auto_tap_budget > 0 {
                    *auto_tap_budget -= 1;
                    if plan_auto_tap_with_constraints_cached(
                        state,
                        player,
                        &leaf_cost,
                        spend_ctx,
                        AutoTapPolicy::default(),
                        &leaf_constraints,
                        cache,
                    )
                    .can_pay
                    {
                        return true;
                    }
                }
            }
        }
    }

    false
}

#[allow(clippy::too_many_arguments)]
fn can_pay_mana_with_cost_aids_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    cost: &ManaCost,
    spend_ctx: &ManaSpendContext,
    constraints: &AutoTapConstraints,
    aids: CostAidCaps,
    rem_non_mana: &[CostDef],
    x_value: Option<u32>,
    self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> bool {
    if can_pay_mana_directly_cached(state, player, cost, spend_ctx, constraints, cache) {
        return true;
    }
    if !aids.any() {
        return false;
    }

    let mut cost_after_delve = cost.clone();
    if aids.allows_delve && cost_after_delve.generic > 0 {
        let reserved_gy = delve_reserved_gy_cards_with_x_cached(
            state,
            player,
            source,
            rem_non_mana,
            x_value,
            self_ctx,
            cache,
        );
        let gy_len = effective_gy_len_for_cost(state, player, source, self_ctx);
        let delve_avail = gy_len.saturating_sub(reserved_gy) as u32;
        cost_after_delve.generic = cost_after_delve.generic.saturating_sub(delve_avail);
        if delve_avail > 0
            && can_pay_mana_directly_cached(
                state,
                player,
                &cost_after_delve,
                spend_ctx,
                constraints,
                cache,
            )
        {
            return true;
        }
    }

    let mut emerge_choices: SmallVec<[Option<(ObjectId, u32)>; 8]> = SmallVec::new();
    emerge_choices.push(None);
    if aids.allows_emerge
        && let Some(p) = state.player(player)
    {
        let mut seen_plain_mv: SmallVec<[u32; 8]> = SmallVec::new();
        for &id in &p.battlefield {
            if !constraints.allows_leave_battlefield(id)
                || !state.object(id).is_some_and(|o| !o.is_phased_out())
                || cant_sacrifice_object_cached(state, player, id, false, cache)
            {
                continue;
            }
            let Some(ev) = cache.get(id) else {
                continue;
            };
            if ev.controller != player || !ev.types.contains(CardTypeMask::CREATURE) {
                continue;
            }
            let mv = ev.mana_value;
            let is_special =
                ev.types.contains(CardTypeMask::ARTIFACT) || !seen_plain_mv.contains(&mv);
            if is_special {
                seen_plain_mv.push(mv);
                emerge_choices.push(Some((id, mv)));
                if emerge_choices.len() >= 16 {
                    break;
                }
            }
        }
    }

    let mana_cands = mana::enumerate_mana_abilities_cached(state, player, cache);
    let has_pool = mana::SimulatedManaPool::from_state_cached(state, player, spend_ctx, cache)
        .total_available()
        > 0;
    let has_mana_source_or_pool = has_pool || !mana_cands.is_empty();

    for emerge_choice in emerge_choices {
        let mut working_cost = cost_after_delve.clone();
        let mut working_constraints = constraints.clone();
        let mut excluded_sac: Option<ObjectId> = None;
        if let Some((sac_id, mv)) = emerge_choice {
            excluded_sac = Some(sac_id);
            working_cost.generic = working_cost.generic.saturating_sub(mv);
            push_unique_oid(&mut working_constraints.keep_on_battlefield, sac_id);
            for grp in &mut working_constraints.reserved_groups {
                grp.candidates.retain(|c| *c != sac_id);
            }
            for tss in &mut working_constraints.tap_subset_sum {
                tss.candidates.retain(|(c, _)| *c != sac_id);
            }
            if can_pay_mana_directly_cached(
                state,
                player,
                &working_cost,
                spend_ctx,
                &working_constraints,
                cache,
            ) {
                return true;
            }
        }

        if !aids.allows_convoke && !aids.allows_improvise {
            continue;
        }

        let Some(p) = state.player(player) else {
            continue;
        };
        let mut aid_cands: Vec<AidTapCand> = Vec::new();
        for &id in &p.battlefield {
            if excluded_sac == Some(id) || !working_constraints.allows_tap(id) {
                continue;
            }
            if !state
                .object(id)
                .is_some_and(|o| !o.is_phased_out() && !o.flags.contains(ObjectFlags::TAPPED))
            {
                continue;
            }
            let Some(ev) = cache.get(id) else {
                continue;
            };
            if ev.controller != player {
                continue;
            }
            let can_convoke = aids.allows_convoke && ev.types.contains(CardTypeMask::CREATURE);
            let can_improvise = aids.allows_improvise && ev.types.contains(CardTypeMask::ARTIFACT);
            if !can_convoke && !can_improvise {
                continue;
            }
            let is_mana_source = mana_cands
                .iter()
                .any(|mc| mc.object == id && (mc.requires_tap || mc.has_sacrifice_self));
            let in_reserved = working_constraints.reserved_groups.iter().any(|g| {
                g.conflict == ReservedGroupConflict::MustStayUntappedAndOnBf
                    && g.candidates.contains(&id)
            }) || working_constraints
                .tap_subset_sum
                .iter()
                .any(|t| t.candidates.iter().any(|&(cid, _)| cid == id));
            aid_cands.push(AidTapCand {
                id,
                colors: if can_convoke {
                    ev.colors & ColorMask::ALL_COLORS
                } else {
                    ColorMask::empty()
                },
                can_convoke,
                can_improvise,
                is_mana_source,
                in_reserved,
            });
        }
        aid_cands.sort_by_key(|c| (c.is_mana_source, c.in_reserved));
        let mut used = vec![false; aid_cands.len()];
        let mut node_budget = 128usize;
        let mut auto_tap_budget = 16usize;
        if can_pay_convoke_improvise_dfs(
            state,
            player,
            &working_cost,
            0,
            spend_ctx,
            &working_constraints,
            &aid_cands,
            &mut used,
            has_mana_source_or_pool,
            &mut node_budget,
            &mut auto_tap_budget,
            cache,
        ) {
            return true;
        }
    }

    false
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn can_pay_costs_and_mana_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    base_mana: &ManaCost,
    non_mana_costs: &[CostDef],
    spend_ctx: &ManaSpendContext,
    aids: CostAidCaps,
    x_value: Option<u32>,
    cache: &LayerCache,
) -> bool {
    can_pay_costs_and_mana_with_self_ctx_cached(
        state,
        player,
        source,
        base_mana,
        non_mana_costs,
        spend_ctx,
        aids,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn can_pay_costs_and_mana_with_self_ctx_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    base_mana: &ManaCost,
    non_mana_costs: &[CostDef],
    spend_ctx: &ManaSpendContext,
    aids: CostAidCaps,
    x_value: Option<u32>,
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> bool {
    can_pay_costs_and_mana_depth_cached(
        state,
        player,
        source,
        base_mana,
        non_mana_costs,
        spend_ctx,
        aids,
        x_value,
        outer_self_ctx,
        cache,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn can_pay_costs_and_mana_depth_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    base_mana: &ManaCost,
    non_mana_costs: &[CostDef],
    spend_ctx: &ManaSpendContext,
    aids: CostAidCaps,
    x_value: Option<u32>,
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
    depth: u8,
) -> bool {
    if depth >= 8 {
        return false;
    }

    if let Some((idx, options)) = non_mana_costs
        .iter()
        .enumerate()
        .find_map(|(i, c)| match c {
            CostDef::Choice { options, forage } if !*forage => Some((i, options)),
            _ => None,
        })
    {
        if options.is_empty() {
            return false;
        }
        return options.iter().any(|branch| {
            let mut expanded = Vec::with_capacity(non_mana_costs.len() - 1 + branch.len());
            expanded.extend_from_slice(&non_mana_costs[..idx]);
            expanded.extend_from_slice(branch);
            expanded.extend_from_slice(&non_mana_costs[idx + 1..]);
            can_pay_costs_and_mana_depth_cached(
                state,
                player,
                source,
                base_mana,
                &expanded,
                spend_ctx,
                aids,
                x_value,
                outer_self_ctx,
                cache,
                depth + 1,
            )
        });
    }

    let mut eff_spend_ctx = spend_ctx.clone();
    let mut combined_mana = base_mana.clone();
    if combined_mana.x_count > 0 {
        if let Some(xv) = x_value {
            combined_mana.generic = combined_mana
                .generic
                .saturating_add(xv.saturating_mul(u32::from(combined_mana.x_count)));
        }
        combined_mana.x_count = 0;
    }
    let mut filtered_non_mana: Vec<CostDef> = Vec::with_capacity(non_mana_costs.len());
    for c in non_mana_costs {
        match c {
            CostDef::Mana {
                cost: mc,
                x_spend_colors,
                x_colored_only,
                x_max_one_per_color,
            } => {
                combined_mana.generic = combined_mana.generic.saturating_add(mc.generic);
                if mc.x_count > 0 {
                    if let Some(xv) = x_value {
                        combined_mana.generic = combined_mana
                            .generic
                            .saturating_add(xv.saturating_mul(u32::from(mc.x_count)));
                    }
                    eff_spend_ctx.has_x_in_cost = true;
                }
                combined_mana.pips.extend(mc.pips.iter().copied());
                eff_spend_ctx.x_spend_colors |= *x_spend_colors;
                eff_spend_ctx.x_colored_only |= *x_colored_only;
                eff_spend_ctx.x_max_one_per_color |= *x_max_one_per_color;
            }
            CostDef::Optional { .. } => {}
            other => filtered_non_mana.push(other.clone()),
        }
    }

    let eff_self_ctx = outer_self_ctx.merged(CostListSelfContext::from_costs(
        state,
        source,
        &filtered_non_mana,
    ));
    if !can_pay_non_mana_costs_with_ctx_cached(
        state,
        player,
        source,
        &filtered_non_mana,
        x_value,
        eff_self_ctx,
        cache,
    ) {
        return false;
    }
    let constraints = auto_tap_constraints_for_costs_with_ctx_cached(
        state,
        player,
        source,
        &filtered_non_mana,
        x_value,
        eff_self_ctx,
        cache,
    );
    if !constraints.is_satisfiable_cached(state, player, cache) {
        return false;
    }

    can_pay_mana_with_cost_aids_cached(
        state,
        player,
        source,
        &combined_mana,
        &eff_spend_ctx,
        &constraints,
        aids,
        &filtered_non_mana,
        x_value,
        eff_self_ctx,
        cache,
    )
}

fn can_pay_single_cost_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    x_value: Option<u32>,
    cache: &LayerCache,
) -> bool {
    can_pay_single_cost_with_ctx_cached(
        state,
        payer,
        source,
        cost,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

fn can_pay_single_cost_with_ctx_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    x_value: Option<u32>,
    self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> bool {
    let mut ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
    ctx.x_value = x_value;

    match cost {
        CostDef::Mana { .. } => true,
        CostDef::TapSelf => {
            let Some(obj) = state.object(source) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || obj.flags.contains(ObjectFlags::TAPPED) {
                return false;
            }
            let ev = cache.get(source);
            if ev.is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
                && obj.flags.contains(ObjectFlags::SUMMONING_SICK)
            {
                let haste_kw = state.db().keyword_id("haste");
                let has_haste =
                    haste_kw.is_some_and(|kw| ev.is_some_and(|e| e.keywords.contains(&kw)));
                if !has_haste && !mana::has_activate_as_though_haste_cached(state, source, cache) {
                    return false;
                }
            }
            true
        }
        CostDef::UntapSelf { via_symbol } => {
            let Some(obj) = state.object(source) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || !obj.flags.contains(ObjectFlags::TAPPED) {
                return false;
            }
            if *via_symbol {
                let ev = cache.get(source);
                if ev.is_some_and(|e| e.types.contains(CardTypeMask::CREATURE))
                    && obj.flags.contains(ObjectFlags::SUMMONING_SICK)
                {
                    let haste_kw = state.db().keyword_id("haste");
                    let has_haste =
                        haste_kw.is_some_and(|kw| ev.is_some_and(|e| e.keywords.contains(&kw)));
                    if !has_haste
                        && !mana::has_activate_as_though_haste_cached(state, source, cache)
                    {
                        return false;
                    }
                }
            }
            true
        }
        CostDef::TapOthers { count, filter, .. } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            let cands: Vec<ObjectId> = find_objects(state, &f, &ctx)
                .into_iter()
                .filter(|&id| {
                    !(self_ctx.taps_self && id == source)
                        && self_ctx.tap_attached_host != Some(id)
                        && state
                            .object(id)
                            .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                })
                .collect();
            cands.len() >= need
        }
        CostDef::UntapOthers { count, filter } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            let cands: Vec<ObjectId> = find_objects(state, &f, &ctx)
                .into_iter()
                .filter(|&id| {
                    !(self_ctx.untaps_self && id == source)
                        && state
                            .object(id)
                            .is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED))
                })
                .collect();
            cands.len() >= need
        }
        CostDef::TapAttached => state
            .object(source)
            .and_then(|o| o.attached_to)
            .and_then(|h| state.object(h))
            .is_some_and(|h| h.zone == Zone::Battlefield && !h.flags.contains(ObjectFlags::TAPPED)),
        CostDef::ExertSelf => state.object(source).is_some_and(|o| {
            o.zone == Zone::Battlefield && !o.flags.contains(ObjectFlags::EXERTED)
        }),
        CostDef::Sacrifice {
            count,
            target,
            sacrifice_all,
            attached_creature,
            one_or_more,
            up_to,
            ..
        } => {
            if *attached_creature {
                return state
                    .object(source)
                    .and_then(|o| o.attached_to)
                    .is_some_and(|h| !cant_sacrifice_object_cached(state, payer, h, false, cache));
            }
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
            let cands: Vec<ObjectId> = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                target,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .into_iter()
            .filter(|&id| {
                (!exclude_self || id != source)
                    && state
                        .object(id)
                        .is_some_and(|o| o.zone == Zone::Battlefield)
                    && !cant_sacrifice_object_cached(state, payer, id, false, cache)
            })
            .collect();
            if *sacrifice_all || up_to.is_some() {
                return !cands.is_empty() || !*one_or_more;
            }
            let need = if *one_or_more {
                1
            } else {
                evaluate_amount(state, count, &ctx).max(0) as usize
            };
            cands.len() >= need
        }
        CostDef::Discard { count, target, .. } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    ZoneMask::HAND,
                    false,
                    cache,
                ),
                None => state
                    .player(payer)
                    .map(|p| p.hand.clone())
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            cands.len() >= need
        }
        CostDef::DiscardHand => state.player(payer).is_some(),
        CostDef::PayLife {
            amount,
            half_life_rounded_up,
        } => {
            let Some(ps) = state.player(payer) else {
                return false;
            };
            let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
            if mana::is_cant_pay_life_cached(state, payer, &spend_ctx, cache) {
                return false;
            }
            let amt = if *half_life_rounded_up {
                (ps.life.max(0) + 1) / 2
            } else {
                evaluate_amount(state, amount, &ctx).max(0)
            };
            ps.life >= amt
        }
        CostDef::OpponentGainsLife { .. } => {
            state.players().iter().any(|p| !p.lost && p.id != payer)
        }
        CostDef::Loyalty { delta, is_x, sign } => {
            let Some(obj) = state.object(source) else {
                return false;
            };
            if obj.zone != Zone::Battlefield {
                return false;
            }
            // Check once-per-turn loyalty activation limit (or StaticBody::LoyaltyActivationsPerTurn)
            if state.rare(source).is_some_and(|r| r.used_loyalty_this_turn) {
                return false;
            }
            let eff_delta = if *is_x {
                let xv = x_value.unwrap_or(0) as i32;
                match sign {
                    Some(LoyaltySign::Minus) => -xv,
                    _ => xv,
                }
            } else {
                *delta
            };
            if eff_delta >= 0 {
                true
            } else if let Some(loy_cid) = state.db().counter_id("loyalty") {
                state.counter_count(source, loy_cid) >= (-eff_delta) as u32
            } else {
                false
            }
        }
        CostDef::RemoveCounters {
            counter,
            count,
            remove_all,
            from,
            among,
            one_or_more,
            ..
        } => {
            let need = if *one_or_more {
                1
            } else {
                evaluate_amount(state, count, &ctx).max(0) as u32
            };
            let cands = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                from,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            );
            let mut total_avail = 0u32;
            let mut max_single_avail = 0u32;
            for id in cands {
                let avail: u32 = match counter {
                    CounterTarget::Specific(cid) => state.counter_count(id, *cid),
                    CounterTarget::Any | CounterTarget::Each => {
                        state.counters(id).iter().map(|(_, n)| *n).sum()
                    }
                };
                total_avail = total_avail.saturating_add(avail);
                max_single_avail = max_single_avail.max(avail);
            }
            if *remove_all {
                total_avail > 0 || !*one_or_more
            } else if !*among && !*one_or_more {
                max_single_avail >= need
            } else {
                total_avail >= need
            }
        }
        CostDef::PutCounters { count, on, .. } => {
            let amt = evaluate_amount(state, count, &ctx).max(0);
            if amt == 0 {
                return true;
            }
            !resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                on,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .is_empty()
        }
        CostDef::ExileFrom {
            zone,
            count,
            target,
            any_graveyard,
            min_total_mana_value,
            min_total_mana_value_amount,
            collect_evidence,
            one_or_more,
            total_mana_value_equals_x,
            ..
        } => {
            let zm = ZoneMask::from_zone(*zone);
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    zm,
                    !*any_graveyard,
                    cache,
                ),
                None => state
                    .players()
                    .iter()
                    .filter(|p| !p.lost && (*any_graveyard || p.id == payer))
                    .flat_map(|p| match zone {
                        Zone::Graveyard => p.graveyard.iter().copied(),
                        Zone::Hand => p.hand.iter().copied(),
                        Zone::Library => p.library.iter().copied(),
                        Zone::Battlefield => p.battlefield.iter().copied(),
                        Zone::Exile => p.exile.iter().copied(),
                        Zone::Command => p.command.iter().copied(),
                        Zone::Stack => [].iter().copied(),
                    })
                    .collect(),
            }
            .into_iter()
            .filter(|&id| !self_ctx.excludes_object_in_zone(source, *zone, target.as_ref(), id))
            .collect();
            if *collect_evidence
                || min_total_mana_value.is_some()
                || min_total_mana_value_amount.is_some()
            {
                let req_mv = min_total_mana_value.map(u32::from).unwrap_or_else(|| {
                    min_total_mana_value_amount
                        .as_deref()
                        .map_or(0, |a| evaluate_amount(state, a, &ctx).max(0) as u32)
                });
                let total_mv: u32 = cands
                    .iter()
                    .map(|&id| object_mana_value(state, id, &ctx))
                    .sum();
                return total_mv >= req_mv;
            }
            if *total_mana_value_equals_x {
                let x_target = x_value.unwrap_or(0);
                if x_target == 0 {
                    return true;
                }
                let mvs: Vec<u32> = cands
                    .iter()
                    .map(|&id| object_mana_value(state, id, &ctx))
                    .collect();
                return subset_sum_exists(&mvs, x_target);
            }
            let need = if *one_or_more {
                1
            } else {
                evaluate_amount(state, count, &ctx).max(0) as usize
            };
            cands.len() >= need
        }
        CostDef::RevealFromHand { count, target, .. } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    ZoneMask::HAND,
                    false,
                    cache,
                ),
                None => state
                    .player(payer)
                    .map(|p| p.hand.clone())
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            cands.len() >= need
        }
        CostDef::RevealHand => state.player(payer).is_some(),
        CostDef::RevealSelfFromHand => {
            self_ctx.cast_left_zone != Some(Zone::Hand)
                && state
                    .object(source)
                    .is_some_and(|o| o.zone == Zone::Hand && o.owner == payer)
        }
        CostDef::Choice { options, forage } => {
            if *forage {
                let gy_len = effective_gy_len_for_cost(state, payer, source, self_ctx);
                let has_food = has_controlled_food_cached(state, payer, cache);
                return gy_len >= 3 || has_food;
            }
            let spend_ctx = ManaSpendContext::for_generic_cost(payer);
            options.iter().any(|branch| {
                can_pay_costs_and_mana_with_self_ctx_cached(
                    state,
                    payer,
                    source,
                    &ManaCost::default(),
                    branch,
                    &spend_ctx,
                    CostAidCaps::default(),
                    x_value,
                    self_ctx,
                    cache,
                )
            })
        }
        CostDef::Optional { .. } => true,
        CostDef::TapSubsetSum {
            min_total,
            filter,
            exclude_self,
            ..
        } => {
            let req = evaluate_amount(state, min_total, &ctx).max(0);
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            let total_avail: i32 = find_objects(state, &f, &ctx)
                .into_iter()
                .filter(|&id| {
                    !((*exclude_self || self_ctx.taps_self) && id == source)
                        && self_ctx.tap_attached_host != Some(id)
                        && state
                            .object(id)
                            .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                        && !cant_crew_vehicle_cached(state, id, cache)
                })
                .filter_map(|id| crate::eval::object_pt(state, id, &ctx).map(|pt| pt.power.max(0)))
                .sum();
            total_avail >= req
        }
        CostDef::PayEnergy { count } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as u32;
            state
                .player(payer)
                .is_some_and(|p| u32::from(p.energy) >= need)
        }
        CostDef::ReturnToHand { count, target } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
            let cands: Vec<ObjectId> = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                target,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            cands.len() >= need
        }
        CostDef::Mill { count } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            effective_lib_len_for_cost(state, payer, source, self_ctx) >= need
        }
        CostDef::Waterbend { cost } => {
            let tap_avail = waterbend_tap_candidates_cached(state, payer, cache).len() as u32;
            let pool_tot = mana::SimulatedManaPool::from_state_cached(
                state,
                payer,
                &mana::ManaSpendContext::for_generic_cost(payer),
                cache,
            )
            .total_available();
            pool_tot + tap_avail >= cost.mana_value()
        }
        CostDef::PutFromHandOnTopOfLibrary { count, filter } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, None);
            let cands: Vec<ObjectId> = match filter {
                Some(f) => {
                    let mut hf = (**f).clone();
                    hf.zones = ZoneMask::HAND;
                    find_objects(state, &hf, &ctx)
                }
                None => state
                    .player(payer)
                    .map(|p| p.hand.clone())
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            cands.len() >= need
        }
        CostDef::PutOnBottomOfLibrary {
            from,
            count,
            target,
        } => {
            let need = evaluate_amount(state, count, &ctx).max(0) as usize;
            let zm = ZoneMask::from_zone(*from);
            let cands: Vec<ObjectId> = match target {
                Some(t) => {
                    resolve_cost_object_candidates_cached(state, payer, source, t, zm, true, cache)
                }
                None => match from {
                    Zone::Hand => state
                        .player(payer)
                        .map(|p| p.hand.clone())
                        .unwrap_or_default(),
                    Zone::Graveyard => state
                        .player(payer)
                        .map(|p| p.graveyard.clone())
                        .unwrap_or_default(),
                    Zone::Battlefield => state
                        .player(payer)
                        .map(|p| p.battlefield.clone())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                },
            }
            .into_iter()
            .filter(|&id| !self_ctx.excludes_object_in_zone(source, *from, target.as_ref(), id))
            .collect();
            cands.len() >= need
        }
        CostDef::UnattachSelf => state
            .object(source)
            .is_some_and(|o| o.attached_to.is_some()),
        CostDef::ChooseCreatureType
        | CostDef::ChooseNumberBetween { .. }
        | CostDef::ChooseFromKeywordList { .. } => true,
        // Rare Phase-2-only CostDefs
        CostDef::DiscardLastDrawnThisTurn
        | CostDef::Behold { .. }
        | CostDef::BeholdOrChoose { .. }
        | CostDef::ProcessOpponentExile { .. }
        | CostDef::PutExiledWithSelfToGraveyard { .. }
        | CostDef::UnattachOther { .. }
        | CostDef::RollDie { .. }
        | CostDef::FlipCoin
        | CostDef::RevealChosenPlayer
        | CostDef::RevealChosenSubType
        | CostDef::PayAttachedManaCost
        | CostDef::ExileFromGraveyardAndPayManaCost { .. }
        | CostDef::ExileFaceDownPermanentFaceUp
        | CostDef::PayManaPerCounter { .. }
        | CostDef::RepeatableOptionalMana { .. } => false,
    }
}

pub fn can_pay_non_mana_costs(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    x_value: Option<u32>,
) -> bool {
    can_pay_non_mana_costs_cached(state, payer, source, costs, x_value, state.evaluate_board())
}

pub(crate) fn can_pay_non_mana_costs_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    x_value: Option<u32>,
    cache: &LayerCache,
) -> bool {
    can_pay_non_mana_costs_with_ctx_cached(
        state,
        payer,
        source,
        costs,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

pub(crate) fn can_pay_non_mana_costs_with_ctx_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    x_value: Option<u32>,
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> bool {
    let self_ctx = outer_self_ctx.merged(CostListSelfContext::from_costs(state, source, costs));
    costs.iter().all(|c| {
        can_pay_single_cost_with_ctx_cached(state, payer, source, c, x_value, self_ctx, cache)
    })
}

fn push_unique_oid(list: &mut SmallVec<[ObjectId; 4]>, id: ObjectId) {
    if id != NONE_OBJECT_ID && !list.contains(&id) {
        list.push(id);
    }
}

fn merge_auto_tap_constraints(dst: &mut AutoTapConstraints, src: AutoTapConstraints) {
    for id in src.untappable_objects {
        push_unique_oid(&mut dst.untappable_objects, id);
    }
    for id in src.ununtappable_objects {
        push_unique_oid(&mut dst.ununtappable_objects, id);
    }
    for id in src.keep_on_battlefield {
        push_unique_oid(&mut dst.keep_on_battlefield, id);
    }
    dst.reserved_groups.extend(src.reserved_groups);
    dst.reserved_life = dst.reserved_life.saturating_add(src.reserved_life);
    dst.reserved_energy = dst.reserved_energy.saturating_add(src.reserved_energy);
    dst.tap_subset_sum.extend(src.tap_subset_sum);
}

fn intersect_auto_tap_constraints(branches: &[AutoTapConstraints]) -> AutoTapConstraints {
    if branches.is_empty() {
        return AutoTapConstraints::default();
    }
    if branches.len() == 1 {
        return branches[0].clone();
    }
    let first = &branches[0];
    let rest = &branches[1..];
    let mut out = AutoTapConstraints {
        reserved_life: branches.iter().map(|b| b.reserved_life).min().unwrap_or(0),
        reserved_energy: branches
            .iter()
            .map(|b| b.reserved_energy)
            .min()
            .unwrap_or(0),
        ..AutoTapConstraints::default()
    };
    for &id in &first.untappable_objects {
        if rest.iter().all(|b| b.untappable_objects.contains(&id)) {
            push_unique_oid(&mut out.untappable_objects, id);
        }
    }
    for &id in &first.ununtappable_objects {
        if rest.iter().all(|b| b.ununtappable_objects.contains(&id)) {
            push_unique_oid(&mut out.ununtappable_objects, id);
        }
    }
    for &id in &first.keep_on_battlefield {
        if rest.iter().all(|b| b.keep_on_battlefield.contains(&id)) {
            push_unique_oid(&mut out.keep_on_battlefield, id);
        }
    }
    for grp in &first.reserved_groups {
        if rest.iter().all(|b| b.reserved_groups.contains(grp)) {
            out.reserved_groups.push(grp.clone());
        }
    }
    for tss in &first.tap_subset_sum {
        if rest.iter().all(|b| b.tap_subset_sum.contains(tss)) {
            out.tap_subset_sum.push(tss.clone());
        }
    }
    out
}

pub(crate) fn auto_tap_constraints_for_costs_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    x_value: Option<u32>,
    cache: &LayerCache,
) -> AutoTapConstraints {
    auto_tap_constraints_for_costs_with_ctx_cached(
        state,
        payer,
        source,
        costs,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

pub(crate) fn auto_tap_constraints_for_costs_with_ctx_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    costs: &[CostDef],
    x_value: Option<u32>,
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> AutoTapConstraints {
    let mut out = AutoTapConstraints::default();
    let mut ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
    ctx.x_value = x_value;
    let self_ctx = outer_self_ctx.merged(CostListSelfContext::from_costs(state, source, costs));

    for cost in costs {
        match cost {
            CostDef::TapSelf => {
                push_unique_oid(&mut out.untappable_objects, source);
                push_unique_oid(&mut out.keep_on_battlefield, source);
            }
            CostDef::UntapSelf { .. } => {
                push_unique_oid(&mut out.ununtappable_objects, source);
                push_unique_oid(&mut out.keep_on_battlefield, source);
            }
            CostDef::ExertSelf | CostDef::Loyalty { .. } | CostDef::UnattachSelf => {
                push_unique_oid(&mut out.keep_on_battlefield, source);
            }
            CostDef::UnattachOther { from_self, .. } => {
                if *from_self {
                    push_unique_oid(&mut out.keep_on_battlefield, source);
                }
            }
            CostDef::TapAttached => {
                push_unique_oid(&mut out.keep_on_battlefield, source);
                if let Some(host) = state.object(source).and_then(|o| o.attached_to) {
                    push_unique_oid(&mut out.untappable_objects, host);
                    push_unique_oid(&mut out.keep_on_battlefield, host);
                }
            }
            CostDef::TapOthers { count, filter, .. } => {
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                if need > 0 {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let cands: SmallVec<[ObjectId; 8]> = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.taps_self && id == source)
                                && self_ctx.tap_attached_host != Some(id)
                                && state
                                    .object(id)
                                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .collect();
                    out.reserved_groups.push(ReservedObjectGroup {
                        candidates: cands,
                        min_needed: need,
                        conflict: ReservedGroupConflict::MustStayUntappedAndOnBf,
                    });
                }
            }
            CostDef::UntapOthers { count, filter } => {
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                if need > 0 {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let cands: SmallVec<[ObjectId; 8]> = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.untaps_self && id == source)
                                && state
                                    .object(id)
                                    .is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .collect();
                    out.reserved_groups.push(ReservedObjectGroup {
                        candidates: cands,
                        min_needed: need,
                        conflict: ReservedGroupConflict::MustStayTappedAndOnBf,
                    });
                }
            }
            CostDef::TapSubsetSum {
                min_total,
                filter,
                exclude_self,
                ..
            } => {
                let req = evaluate_amount(state, min_total, &ctx).max(0);
                if req > 0 {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let cands: SmallVec<[(ObjectId, i32); 8]> = find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !((*exclude_self || self_ctx.taps_self) && id == source)
                                && self_ctx.tap_attached_host != Some(id)
                                && state
                                    .object(id)
                                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                                && !cant_crew_vehicle_cached(state, id, cache)
                        })
                        .map(|id| {
                            let pwr = crate::eval::object_pt(state, id, &ctx)
                                .map_or(0, |pt| pt.power.max(0));
                            (id, pwr)
                        })
                        .collect();
                    out.tap_subset_sum.push(TapSubsetSumConstraint {
                        candidates: cands,
                        min_total: req,
                    });
                }
            }
            CostDef::Sacrifice {
                count,
                target,
                sacrifice_all,
                attached_creature,
                one_or_more,
                up_to,
                ..
            } => {
                if *attached_creature {
                    if let Some(host) = state.object(source).and_then(|o| o.attached_to) {
                        push_unique_oid(&mut out.keep_on_battlefield, host);
                    }
                } else if matches!(target, CostObjectTarget::SelfObj) {
                    // Self-sacrifice without TapSelf allows tapping self for mana before sacrificing it.
                    push_unique_oid(&mut out.keep_on_battlefield, source);
                } else if *one_or_more || (!*sacrifice_all && up_to.is_none()) {
                    let need = if *one_or_more {
                        1
                    } else {
                        evaluate_amount(state, count, &ctx).max(0) as usize
                    };
                    if need > 0 {
                        let cands: SmallVec<[ObjectId; 8]> = resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            target,
                            ZoneMask::BATTLEFIELD,
                            true,
                            cache,
                        )
                        .into_iter()
                        .filter(|&id| {
                            !self_ctx.excludes_object_in_zone(
                                source,
                                Zone::Battlefield,
                                Some(target),
                                id,
                            ) && state
                                .object(id)
                                .is_some_and(|o| o.zone == Zone::Battlefield)
                                && !cant_sacrifice_object_cached(state, payer, id, false, cache)
                        })
                        .collect();
                        out.reserved_groups.push(ReservedObjectGroup {
                            candidates: cands,
                            min_needed: need,
                            conflict: ReservedGroupConflict::MustStayOnBf,
                        });
                    }
                }
            }
            CostDef::ExileFrom {
                zone,
                count,
                target,
                from_top,
                any_graveyard,
                min_total_mana_value,
                min_total_mana_value_amount,
                collect_evidence,
                one_or_more,
                ..
            } => {
                if *zone == Zone::Battlefield && !*from_top {
                    if matches!(target, Some(CostObjectTarget::SelfObj)) {
                        push_unique_oid(&mut out.keep_on_battlefield, source);
                    } else {
                        let cands: SmallVec<[ObjectId; 8]> = match target {
                            Some(t) => resolve_cost_object_candidates_cached(
                                state,
                                payer,
                                source,
                                t,
                                ZoneMask::BATTLEFIELD,
                                !*any_graveyard,
                                cache,
                            ),
                            None => state
                                .players()
                                .iter()
                                .filter(|p| !p.lost && (*any_graveyard || p.id == payer))
                                .flat_map(|p| p.battlefield.iter().copied())
                                .collect(),
                        }
                        .into_iter()
                        .filter(|&id| {
                            !self_ctx.excludes_object_in_zone(source, *zone, target.as_ref(), id)
                        })
                        .collect();
                        if *collect_evidence
                            || min_total_mana_value.is_some()
                            || min_total_mana_value_amount.is_some()
                        {
                            let req_mv = min_total_mana_value.map(u32::from).unwrap_or_else(|| {
                                min_total_mana_value_amount
                                    .as_deref()
                                    .map_or(0, |a| evaluate_amount(state, a, &ctx).max(0) as u32)
                            });
                            if req_mv > 0 {
                                let total_mv: u32 = cands
                                    .iter()
                                    .map(|&id| object_mana_value(state, id, &ctx))
                                    .sum();
                                for &id in &cands {
                                    let mv = object_mana_value(state, id, &ctx);
                                    if total_mv.saturating_sub(mv) < req_mv {
                                        push_unique_oid(&mut out.keep_on_battlefield, id);
                                    }
                                }
                                out.reserved_groups.push(ReservedObjectGroup {
                                    candidates: cands,
                                    min_needed: 1,
                                    conflict: ReservedGroupConflict::MustStayOnBf,
                                });
                            }
                        } else {
                            let need = if *one_or_more {
                                1
                            } else {
                                evaluate_amount(state, count, &ctx).max(0) as usize
                            };
                            if need > 0 {
                                out.reserved_groups.push(ReservedObjectGroup {
                                    candidates: cands,
                                    min_needed: need,
                                    conflict: ReservedGroupConflict::MustStayOnBf,
                                });
                            }
                        }
                    }
                }
            }
            CostDef::ReturnToHand { count, target } => {
                if matches!(target, CostObjectTarget::SelfObj) {
                    push_unique_oid(&mut out.keep_on_battlefield, source);
                } else {
                    let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                    if need > 0 {
                        let cands: SmallVec<[ObjectId; 8]> = resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            target,
                            ZoneMask::BATTLEFIELD,
                            true,
                            cache,
                        )
                        .into_iter()
                        .filter(|&id| {
                            !self_ctx.excludes_object_in_zone(
                                source,
                                Zone::Battlefield,
                                Some(target),
                                id,
                            )
                        })
                        .collect();
                        out.reserved_groups.push(ReservedObjectGroup {
                            candidates: cands,
                            min_needed: need,
                            conflict: ReservedGroupConflict::MustStayOnBf,
                        });
                    }
                }
            }
            CostDef::RemoveCounters {
                counter,
                count,
                remove_all,
                from,
                one_or_more,
                ..
            } => {
                if matches!(from, CostObjectTarget::SelfObj) {
                    push_unique_oid(&mut out.keep_on_battlefield, source);
                } else {
                    let need = if *one_or_more {
                        1
                    } else if *remove_all {
                        0
                    } else {
                        evaluate_amount(state, count, &ctx).max(0) as u32
                    };
                    if need > 0 {
                        let cands = resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            from,
                            ZoneMask::BATTLEFIELD,
                            true,
                            cache,
                        );
                        let mut nonzero: SmallVec<[ObjectId; 8]> = SmallVec::new();
                        let mut avail_pairs: SmallVec<[(ObjectId, u32); 8]> = SmallVec::new();
                        let mut total_avail = 0u32;
                        for id in cands {
                            let avail: u32 = match counter {
                                CounterTarget::Specific(cid) => state.counter_count(id, *cid),
                                CounterTarget::Any | CounterTarget::Each => {
                                    state.counters(id).iter().map(|(_, n)| *n).sum()
                                }
                            };
                            if avail > 0 {
                                nonzero.push(id);
                                avail_pairs.push((id, avail));
                                total_avail = total_avail.saturating_add(avail);
                            }
                        }
                        for (id, avail) in avail_pairs {
                            if total_avail.saturating_sub(avail) < need {
                                push_unique_oid(&mut out.keep_on_battlefield, id);
                            }
                        }
                        out.reserved_groups.push(ReservedObjectGroup {
                            candidates: nonzero,
                            min_needed: 1,
                            conflict: ReservedGroupConflict::MustStayOnBf,
                        });
                    }
                }
            }
            CostDef::PutCounters { count, on, .. } => {
                let amt = evaluate_amount(state, count, &ctx).max(0);
                if amt > 0 {
                    if matches!(on, CostObjectTarget::SelfObj) {
                        push_unique_oid(&mut out.keep_on_battlefield, source);
                    } else {
                        let cands: SmallVec<[ObjectId; 8]> = resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            on,
                            ZoneMask::BATTLEFIELD,
                            true,
                            cache,
                        )
                        .into();
                        out.reserved_groups.push(ReservedObjectGroup {
                            candidates: cands,
                            min_needed: 1,
                            conflict: ReservedGroupConflict::MustStayOnBf,
                        });
                    }
                }
            }
            CostDef::PutOnBottomOfLibrary {
                from,
                count,
                target,
            } => {
                if *from == Zone::Battlefield {
                    if matches!(target, Some(CostObjectTarget::SelfObj)) {
                        push_unique_oid(&mut out.keep_on_battlefield, source);
                    } else {
                        let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                        if need > 0 {
                            let cands: SmallVec<[ObjectId; 8]> = match target {
                                Some(t) => resolve_cost_object_candidates_cached(
                                    state,
                                    payer,
                                    source,
                                    t,
                                    ZoneMask::BATTLEFIELD,
                                    true,
                                    cache,
                                ),
                                None => state
                                    .player(payer)
                                    .map(|p| p.battlefield.clone())
                                    .unwrap_or_default(),
                            }
                            .into_iter()
                            .filter(|&id| {
                                !self_ctx.excludes_object_in_zone(
                                    source,
                                    *from,
                                    target.as_ref(),
                                    id,
                                )
                            })
                            .collect();
                            out.reserved_groups.push(ReservedObjectGroup {
                                candidates: cands,
                                min_needed: need,
                                conflict: ReservedGroupConflict::MustStayOnBf,
                            });
                        }
                    }
                }
            }
            CostDef::PayLife {
                amount,
                half_life_rounded_up,
            } => {
                let amt = if *half_life_rounded_up {
                    let life = state.player(payer).map_or(0, |p| p.life);
                    ((life.max(0) + 1) / 2) as u32
                } else {
                    evaluate_amount(state, amount, &ctx).max(0) as u32
                };
                out.reserved_life = out.reserved_life.saturating_add(amt);
            }
            CostDef::PayEnergy { count } => {
                let need = evaluate_amount(state, count, &ctx).max(0) as u32;
                out.reserved_energy = out.reserved_energy.saturating_add(need);
            }
            CostDef::Choice { options, forage } => {
                if *forage {
                    let gy_len = effective_gy_len_for_cost(state, payer, source, self_ctx);
                    if gy_len < 3 {
                        let foods: SmallVec<[ObjectId; 8]> =
                            if let Some(food_id) = state.db().subtype_id("Food") {
                                state
                                    .player(payer)
                                    .map(|p| {
                                        p.battlefield
                                            .iter()
                                            .copied()
                                            .filter(|&id| {
                                                cache
                                                    .get(id)
                                                    .is_some_and(|e| e.subtypes.contains(&food_id))
                                                    && !cant_sacrifice_object_cached(
                                                        state, payer, id, false, cache,
                                                    )
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            } else {
                                SmallVec::new()
                            };
                        out.reserved_groups.push(ReservedObjectGroup {
                            candidates: foods,
                            min_needed: 1,
                            conflict: ReservedGroupConflict::MustStayOnBf,
                        });
                    }
                } else {
                    let spend_ctx = ManaSpendContext::for_generic_cost(payer);
                    let mut payable_branches: Vec<AutoTapConstraints> = Vec::new();
                    for branch in options {
                        if can_pay_costs_and_mana_with_self_ctx_cached(
                            state,
                            payer,
                            source,
                            &ManaCost::default(),
                            branch,
                            &spend_ctx,
                            CostAidCaps::default(),
                            x_value,
                            self_ctx,
                            cache,
                        ) {
                            payable_branches.push(auto_tap_constraints_for_costs_with_ctx_cached(
                                state, payer, source, branch, x_value, self_ctx, cache,
                            ));
                        }
                    }
                    let intersected = intersect_auto_tap_constraints(&payable_branches);
                    merge_auto_tap_constraints(&mut out, intersected);
                }
            }
            CostDef::Optional { .. }
            | CostDef::Mana { .. }
            | CostDef::Discard { .. }
            | CostDef::DiscardHand
            | CostDef::DiscardLastDrawnThisTurn
            | CostDef::OpponentGainsLife { .. }
            | CostDef::RevealFromHand { .. }
            | CostDef::RevealHand
            | CostDef::RevealSelfFromHand
            | CostDef::Mill { .. }
            | CostDef::Waterbend { .. }
            | CostDef::Behold { .. }
            | CostDef::BeholdOrChoose { .. }
            | CostDef::PutFromHandOnTopOfLibrary { .. }
            | CostDef::ProcessOpponentExile { .. }
            | CostDef::PutExiledWithSelfToGraveyard { .. }
            | CostDef::RollDie { .. }
            | CostDef::FlipCoin
            | CostDef::ChooseCreatureType
            | CostDef::ChooseNumberBetween { .. }
            | CostDef::RevealChosenPlayer
            | CostDef::RevealChosenSubType
            | CostDef::ChooseFromKeywordList { .. }
            | CostDef::PayAttachedManaCost
            | CostDef::ExileFromGraveyardAndPayManaCost { .. }
            | CostDef::ExileFaceDownPermanentFaceUp
            | CostDef::PayManaPerCounter { .. }
            | CostDef::RepeatableOptionalMana { .. } => {}
        }
    }

    out
}

fn has_controlled_food_cached(state: &GameState, player: PlayerId, cache: &LayerCache) -> bool {
    let Some(food_id) = state.db().subtype_id("Food") else {
        return false;
    };
    state.player(player).is_some_and(|p| {
        p.battlefield.iter().any(|&id| {
            cache.get(id).is_some_and(|e| e.subtypes.contains(&food_id))
                && !cant_sacrifice_object_cached(state, player, id, false, cache)
        })
    })
}

fn waterbend_tap_candidates_cached(
    state: &GameState,
    payer: PlayerId,
    cache: &LayerCache,
) -> Vec<ObjectId> {
    state
        .player(payer)
        .map(|p| {
            p.battlefield
                .iter()
                .copied()
                .filter(|&id| {
                    state.object(id).is_some_and(|o| {
                        !o.is_phased_out() && !o.flags.contains(ObjectFlags::TAPPED)
                    }) && cache.get(id).is_some_and(|e| {
                        e.types
                            .intersects(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn non_mana_cost_candidates(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    x_value: Option<u32>,
) -> Option<(CostSelectionKind, Vec<ObjectId>, u16, u16)> {
    non_mana_cost_candidates_cached(state, payer, source, cost, x_value, state.evaluate_board())
}

pub(crate) fn non_mana_cost_candidates_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    x_value: Option<u32>,
    cache: &LayerCache,
) -> Option<(CostSelectionKind, Vec<ObjectId>, u16, u16)> {
    non_mana_cost_candidates_in_list_cached(state, payer, source, cost, &[], x_value, cache)
}

pub(crate) fn non_mana_cost_candidates_in_list_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    all_costs: &[CostDef],
    x_value: Option<u32>,
    cache: &LayerCache,
) -> Option<(CostSelectionKind, Vec<ObjectId>, u16, u16)> {
    non_mana_cost_candidates_in_list_with_ctx_cached(
        state,
        payer,
        source,
        cost,
        all_costs,
        x_value,
        CostListSelfContext::default(),
        cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn non_mana_cost_candidates_in_list_with_ctx_cached(
    state: &GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    all_costs: &[CostDef],
    x_value: Option<u32>,
    outer_self_ctx: CostListSelfContext,
    cache: &LayerCache,
) -> Option<(CostSelectionKind, Vec<ObjectId>, u16, u16)> {
    let mut ctx = EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
    ctx.x_value = x_value;
    let self_ctx = outer_self_ctx.merged(CostListSelfContext::from_costs(state, source, all_costs));

    match cost {
        CostDef::Sacrifice {
            count,
            target,
            sacrifice_all,
            attached_creature,
            one_or_more,
            up_to,
            ..
        } => {
            if *attached_creature {
                let cands = state
                    .object(source)
                    .and_then(|o| o.attached_to)
                    .into_iter()
                    .filter(|&id| !cant_sacrifice_object_cached(state, payer, id, false, cache))
                    .collect();
                return Some((CostSelectionKind::Sacrifice, cands, 1, 1));
            }
            if matches!(target, CostObjectTarget::SelfObj) || *sacrifice_all {
                return None;
            }
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
            let cands: Vec<ObjectId> = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                target,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .into_iter()
            .filter(|&id| {
                (!exclude_self || id != source)
                    && !cant_sacrifice_object_cached(state, payer, id, false, cache)
            })
            .collect();
            let (min, max) = if let Some(u) = up_to {
                (u16::from(*one_or_more), *u)
            } else if *one_or_more {
                (1, cands.len() as u16)
            } else {
                let n = evaluate_amount(state, count, &ctx).max(0) as u16;
                (n, n)
            };
            Some((CostSelectionKind::Sacrifice, cands, min, max))
        }
        CostDef::Discard {
            count,
            target,
            random,
            ..
        } => {
            if *random || matches!(target, Some(CostObjectTarget::SelfObj)) {
                return None;
            }
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    ZoneMask::HAND,
                    false,
                    cache,
                ),
                None => state
                    .player(payer)
                    .map(|p| p.hand.clone())
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            let n = evaluate_amount(state, count, &ctx).max(0) as u16;
            Some((CostSelectionKind::Discard, cands, n, n))
        }
        CostDef::ExileFrom {
            zone,
            count,
            target,
            from_top,
            any_graveyard,
            one_or_more,
            ..
        } => {
            if *from_top {
                return None;
            }
            let zm = ZoneMask::from_zone(*zone);
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    zm,
                    !*any_graveyard,
                    cache,
                ),
                None => state
                    .players()
                    .iter()
                    .filter(|p| !p.lost && (*any_graveyard || p.id == payer))
                    .flat_map(|p| match zone {
                        Zone::Graveyard => p.graveyard.iter().copied(),
                        Zone::Hand => p.hand.iter().copied(),
                        Zone::Battlefield => p.battlefield.iter().copied(),
                        _ => [].iter().copied(),
                    })
                    .collect(),
            }
            .into_iter()
            .filter(|&id| !self_ctx.excludes_object_in_zone(source, *zone, target.as_ref(), id))
            .collect();
            let kind = match zone {
                Zone::Hand => CostSelectionKind::ExileFromHand,
                _ => CostSelectionKind::ExileFromGraveyard,
            };
            let (min, max) = if *one_or_more {
                (1, cands.len() as u16)
            } else {
                let n = evaluate_amount(state, count, &ctx).max(0) as u16;
                (n, n)
            };
            Some((kind, cands, min, max))
        }
        CostDef::ReturnToHand { count, target } => {
            if matches!(target, CostObjectTarget::SelfObj) {
                return None;
            }
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Battlefield, Some(target));
            let cands: Vec<ObjectId> = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                target,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            let n = evaluate_amount(state, count, &ctx).max(0) as u16;
            Some((CostSelectionKind::ReturnToHand, cands, n, n))
        }
        CostDef::RevealFromHand { count, target, .. } => {
            let exclude_self = self_ctx.excludes_self_in_zone(Zone::Hand, target.as_ref());
            let cands: Vec<ObjectId> = match target {
                Some(t) => resolve_cost_object_candidates_cached(
                    state,
                    payer,
                    source,
                    t,
                    ZoneMask::HAND,
                    false,
                    cache,
                ),
                None => state
                    .player(payer)
                    .map(|p| p.hand.clone())
                    .unwrap_or_default(),
            }
            .into_iter()
            .filter(|&id| !exclude_self || id != source)
            .collect();
            let n = evaluate_amount(state, count, &ctx).max(0) as u16;
            Some((CostSelectionKind::RevealFromHand, cands, n, n))
        }
        CostDef::TapOthers { count, filter, .. } => {
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            let cands: Vec<ObjectId> = find_objects(state, &f, &ctx)
                .into_iter()
                .filter(|&id| {
                    !(self_ctx.taps_self && id == source)
                        && self_ctx.tap_attached_host != Some(id)
                        && state
                            .object(id)
                            .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                })
                .collect();
            let n = evaluate_amount(state, count, &ctx).max(0) as u16;
            Some((CostSelectionKind::TapUntapped, cands, n, n))
        }
        CostDef::RemoveCounters {
            counter,
            count,
            remove_all,
            from,
            among,
            one_or_more,
            ..
        } => {
            if matches!(from, CostObjectTarget::SelfObj) || *remove_all {
                return None;
            }
            let cands_with_avail: Vec<(ObjectId, u32)> = resolve_cost_object_candidates_cached(
                state,
                payer,
                source,
                from,
                ZoneMask::BATTLEFIELD,
                true,
                cache,
            )
            .into_iter()
            .filter_map(|id| {
                let avail: u32 = match counter {
                    CounterTarget::Specific(cid) => state.counter_count(id, *cid),
                    CounterTarget::Any | CounterTarget::Each => {
                        state.counters(id).iter().map(|(_, n)| *n).sum()
                    }
                };
                (avail > 0).then_some((id, avail))
            })
            .collect();
            if *one_or_more {
                let cands: Vec<ObjectId> = cands_with_avail.into_iter().map(|(id, _)| id).collect();
                let max = cands.len() as u16;
                return Some((CostSelectionKind::RemoveCounters, cands, 1, max));
            }
            let need = evaluate_amount(state, count, &ctx).max(0) as u32;
            if need == 0 {
                return None;
            }
            if !*among {
                let cands: Vec<ObjectId> = cands_with_avail
                    .into_iter()
                    .filter(|&(_, avail)| avail >= need)
                    .map(|(id, _)| id)
                    .collect();
                return Some((CostSelectionKind::RemoveCounters, cands, 1, 1));
            }
            let mut sorted_avail: Vec<u32> = cands_with_avail.iter().map(|&(_, a)| a).collect();
            sorted_avail.sort_unstable_by(|a, b| b.cmp(a));
            let mut sum = 0u32;
            let mut min_objs = 0u16;
            for a in sorted_avail {
                sum = sum.saturating_add(a);
                min_objs = min_objs.saturating_add(1);
                if sum >= need {
                    break;
                }
            }
            let cands: Vec<ObjectId> = cands_with_avail.into_iter().map(|(id, _)| id).collect();
            let max = cands.len() as u16;
            let min = min_objs.max(1);
            Some((CostSelectionKind::RemoveCounters, cands, min, max))
        }
        CostDef::TapSubsetSum {
            filter,
            exclude_self,
            ..
        } => {
            let mut f = (**filter).clone();
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            let cands: Vec<ObjectId> = find_objects(state, &f, &ctx)
                .into_iter()
                .filter(|&id| {
                    !((*exclude_self || self_ctx.taps_self) && id == source)
                        && self_ctx.tap_attached_host != Some(id)
                        && state
                            .object(id)
                            .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                        && !cant_crew_vehicle_cached(state, id, cache)
                })
                .collect();
            let max = cands.len() as u16;
            Some((CostSelectionKind::TapUntapped, cands, 1, max))
        }
        _ => None,
    }
}

pub fn apply_cost_aid(
    state: &mut GameState,
    payer: PlayerId,
    plan: &mut PendingCostPaymentPlan,
    aid_kind: CostAidKind,
    target_object: ObjectId,
) -> Result<(), EngineError> {
    let constraints = {
        let cache = state.evaluate_board();
        let rem = plan
            .remaining_non_mana_costs
            .get(plan.non_mana_cursor..)
            .unwrap_or(&[]);
        auto_tap_constraints_for_costs_cached(state, payer, NONE_OBJECT_ID, rem, None, cache)
    };
    apply_cost_aid_with_constraints(state, payer, plan, aid_kind, target_object, &constraints)
}

pub(crate) fn apply_cost_aid_with_constraints(
    state: &mut GameState,
    payer: PlayerId,
    plan: &mut PendingCostPaymentPlan,
    aid_kind: CostAidKind,
    target_object: ObjectId,
    constraints: &AutoTapConstraints,
) -> Result<(), EngineError> {
    let cache = state.evaluate_board();
    let Some(obj) = state.object(target_object) else {
        return Err(EngineError::IllegalAction("cost aid target missing".into()));
    };
    let Some(ev) = cache.get(target_object) else {
        return Err(EngineError::IllegalAction(
            "cost aid target not evaluated".into(),
        ));
    };

    match aid_kind {
        CostAidKind::Convoke => {
            if obj.zone != Zone::Battlefield
                || obj.is_phased_out()
                || obj.flags.contains(ObjectFlags::TAPPED)
                || ev.controller != payer
                || !ev.types.contains(CardTypeMask::CREATURE)
            {
                return Err(EngineError::IllegalAction(
                    "Convoke requires an untapped creature you control".into(),
                ));
            }
            if !constraints.allows_tap(target_object) {
                return Err(EngineError::IllegalAction(
                    "Convoke target is reserved for pending non-mana costs".into(),
                ));
            }
            if !convoke_can_reduce_mana(&plan.remaining_mana, ev.colors) {
                return Err(EngineError::IllegalAction(
                    "Convoke target cannot reduce remaining mana cost".into(),
                ));
            }
            let colors = ev.colors & ColorMask::ALL_COLORS;
            // Prefer reducing a matching colored/hybrid/two-brid pip, else reduce generic by 1.
            let mut reduced_pip = false;
            for c in mana::FIVE_COLORS {
                if colors.contains(ColorMask::from_mana_color(c))
                    && let Some(pos) = plan
                        .remaining_mana
                        .pips
                        .iter()
                        .position(|p| pip_matches_color(*p, c))
                {
                    plan.remaining_mana.pips.remove(pos);
                    reduced_pip = true;
                    break;
                }
            }
            if !reduced_pip {
                if plan.remaining_mana.generic == 0 {
                    return Err(EngineError::IllegalAction(
                        "Convoke target cannot reduce remaining mana cost".into(),
                    ));
                }
                plan.remaining_mana.generic -= 1;
            }
            if let Some(o) = state.object_mut(target_object) {
                o.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            plan.convoke_tapped.push(target_object);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: target_object,
                    now_tapped: true,
                    actor: Some(payer),
                },
            );
            Ok(())
        }
        CostAidKind::Improvise => {
            if obj.zone != Zone::Battlefield
                || obj.is_phased_out()
                || obj.flags.contains(ObjectFlags::TAPPED)
                || ev.controller != payer
                || !ev.types.contains(CardTypeMask::ARTIFACT)
                || plan.remaining_mana.generic == 0
            {
                return Err(EngineError::IllegalAction(
                    "Improvise requires an untapped artifact you control and generic mana remaining".into(),
                ));
            }
            if !constraints.allows_tap(target_object) {
                return Err(EngineError::IllegalAction(
                    "Improvise target is reserved for pending non-mana costs".into(),
                ));
            }
            plan.remaining_mana.generic -= 1;
            if let Some(o) = state.object_mut(target_object) {
                o.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            plan.improvise_tapped.push(target_object);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: target_object,
                    now_tapped: true,
                    actor: Some(payer),
                },
            );
            Ok(())
        }
        CostAidKind::Delve => {
            if obj.zone != Zone::Graveyard || obj.owner != payer || plan.remaining_mana.generic == 0
            {
                return Err(EngineError::IllegalAction(
                    "Delve requires a card in your graveyard and generic mana remaining".into(),
                ));
            }
            let rem_non_mana = plan
                .remaining_non_mana_costs
                .get(plan.non_mana_cursor..)
                .unwrap_or(&[]);
            let reserved_gy = delve_reserved_gy_cards_cached(state, payer, rem_non_mana, cache);
            let gy_len = state.player(payer).map_or(0, |p| p.graveyard.len());
            if gy_len.saturating_sub(1) < reserved_gy {
                return Err(EngineError::IllegalAction(
                    "Delve target is reserved for pending non-mana costs".into(),
                ));
            }
            plan.remaining_mana.generic -= 1;
            let mv = zones::move_object(
                state,
                target_object,
                Zone::Exile,
                MoveRawOptions::default(),
                None,
            )?;
            if let MoveObjectOutcome::Moved(raw) = mv
                && let Some(nid) = raw.new_id
            {
                plan.delve_exiled.push(nid);
            }
            Ok(())
        }
        CostAidKind::Emerge => {
            if obj.zone != Zone::Battlefield
                || obj.is_phased_out()
                || ev.controller != payer
                || !ev.types.contains(CardTypeMask::CREATURE)
                || plan.emerge_sacrificed.is_some()
                || cant_sacrifice_object_cached(state, payer, target_object, false, cache)
            {
                return Err(EngineError::IllegalAction(
                    "Emerge requires a creature you control to sacrifice".into(),
                ));
            }
            if !constraints.allows_leave_battlefield(target_object) {
                return Err(EngineError::IllegalAction(
                    "Emerge target is reserved for pending non-mana costs".into(),
                ));
            }
            let mv = ev.mana_value;
            let res = zones::move_object(
                state,
                target_object,
                Zone::Graveyard,
                MoveRawOptions {
                    capture_lki: Some(true),
                    ..Default::default()
                },
                None,
            )?;
            if let MoveObjectOutcome::Moved(raw) = res {
                let lki = raw
                    .lki
                    .unwrap_or_else(|| Arc::new(fallback_lki(state, target_object)));
                plan.sacrificed_lki.push(Arc::clone(&lki));
                plan.emerge_sacrificed = Some(target_object);
                plan.remaining_mana.generic = plan.remaining_mana.generic.saturating_sub(mv);
                emit_event(
                    state,
                    AtomicEventKind::Sacrificed {
                        player: payer,
                        object: target_object,
                        lki,
                    },
                );
            }
            Ok(())
        }
        CostAidKind::Waterbend => {
            if obj.zone != Zone::Battlefield
                || obj.is_phased_out()
                || obj.flags.contains(ObjectFlags::TAPPED)
                || ev.controller != payer
                || !ev
                    .types
                    .intersects(CardTypeMask::ARTIFACT | CardTypeMask::CREATURE)
                || plan.remaining_mana.generic == 0
            {
                return Err(EngineError::IllegalAction(
                    "Waterbend requires an untapped artifact or creature you control".into(),
                ));
            }
            if !constraints.allows_tap(target_object) {
                return Err(EngineError::IllegalAction(
                    "Waterbend target is reserved for pending non-mana costs".into(),
                ));
            }
            plan.remaining_mana.generic -= 1;
            if let Some(o) = state.object_mut(target_object) {
                o.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            plan.waterbend_tapped.push(target_object);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: target_object,
                    now_tapped: true,
                    actor: Some(payer),
                },
            );
            Ok(())
        }
    }
}

fn fallback_lki(state: &GameState, id: ObjectId) -> LkiSnapshot {
    if let Some(existing) = state.lki(id) {
        return existing.clone();
    }
    let obj = state.object(id);
    let card_def = obj.map_or(CardDefId(0), |o| o.card_def);
    let owner = obj.map_or(PlayerId(0), |o| o.owner);
    let controller = obj.map_or(PlayerId(0), |o| o.controller);
    let zone = obj.map_or(Zone::Battlefield, |o| o.zone);
    let face_idx = obj.map_or(0, |o| o.active_face_index());
    let face = state
        .db()
        .get(card_def)
        .and_then(|c| c.faces.get(face_idx as usize));
    LkiSnapshot {
        object_id: id,
        card_def,
        name: face.map_or_else(String::new, |f| f.name.clone()),
        owner,
        controller,
        last_zone: zone,
        pt: None,
        types: face.map_or(CardTypeMask::empty(), |f| f.characteristics.types),
        subtypes: face
            .map(|f| f.characteristics.subtypes.iter().copied().collect())
            .unwrap_or_default(),
        supertypes: face.map_or(SupertypeMask::empty(), |f| f.characteristics.supertypes),
        colors: face.map_or(ColorMask::empty(), |f| f.characteristics.colors),
        mana_value: face.map_or(0, |f| f.characteristics.mana_cost.mana_value()),
        keywords: SmallVec::new(),
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.map_or(0, |o| o.damage_marked),
        is_token: obj.is_some_and(crate::state::GameObject::is_token),
        is_commander: obj.is_some_and(crate::state::GameObject::is_commander),
        tapped: obj.is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED)),
        phased_out: obj.is_some_and(crate::state::GameObject::is_phased_out),
        face_down: obj.is_some_and(crate::state::GameObject::is_face_down),
        lost_all_abilities: false,
        has_all_creature_types: false,
        attached_ids: SmallVec::new(),
        chosen_color: state.rare(id).and_then(|r| r.chosen_color),
        chosen_subtype: state.rare(id).and_then(|r| r.chosen_subtype),
        chosen_card_name: state.rare(id).and_then(|r| r.chosen_card_name.clone()),
        active_face_index: face_idx,
    }
}

pub fn execute_non_mana_cost_step(
    state: &mut GameState,
    payer: PlayerId,
    source: ObjectId,
    cost: &CostDef,
    chosen_objects: &[ObjectId],
    x_value: Option<u32>,
    mut plan: Option<&mut PendingCostPaymentPlan>,
) -> Result<CostStepOutcome, EngineError> {
    match cost {
        CostDef::Mana { cost: mc, .. } => {
            let mut eff = mc.clone();
            if eff.x_count > 0
                && let Some(xv) = x_value
            {
                eff.generic = eff
                    .generic
                    .saturating_add(xv.saturating_mul(u32::from(eff.x_count)));
                eff.x_count = 0;
            }
            let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
            mana::deduct_mana_for_context(state, payer, &eff, &spend_ctx, &[])?;
            Ok(CostStepOutcome::Paid)
        }
        CostDef::TapSelf => {
            if !can_pay_single_cost_cached(
                state,
                payer,
                source,
                cost,
                x_value,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction(
                    "cannot tap self for cost".into(),
                ));
            }
            if let Some(o) = state.object_mut(source) {
                o.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: source,
                    now_tapped: true,
                    actor: Some(payer),
                },
            );
            Ok(CostStepOutcome::Paid)
        }
        CostDef::UntapSelf { via_symbol } => {
            if !can_pay_single_cost_cached(
                state,
                payer,
                source,
                cost,
                x_value,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction(
                    "cannot untap self for cost".into(),
                ));
            }
            let _ = via_symbol;
            match replace::resolve_untap(state, source)? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                            kind: PendingEffectReplaceKind::Untap {
                                current_object: source,
                                remaining_objects: Vec::new(),
                            },
                        });
                    }
                    Ok(CostStepOutcome::Paused { prompt_id, chooser })
                }
                ReplacementResult::Resolved(out) => {
                    if !out.mods.skipped {
                        if let Some(o) = state.object_mut(source) {
                            o.flags.remove(ObjectFlags::TAPPED);
                        }
                        state.mark_dirty(DirtyMask::TAPPED);
                        emit_event(
                            state,
                            AtomicEventKind::TapStateChanged {
                                object: source,
                                now_tapped: false,
                                actor: Some(payer),
                            },
                        );
                    }
                    Ok(CostStepOutcome::Paid)
                }
            }
        }
        CostDef::TapOthers { count, filter, .. } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.taps_self && id == source)
                                && self_ctx.tap_attached_host != Some(id)
                                && state
                                    .object(id)
                                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .take(need)
                        .collect()
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough untapped permanents to tap".into(),
                ));
            }
            for id in targets {
                if let Some(o) = state.object_mut(id) {
                    o.flags.insert(ObjectFlags::TAPPED);
                }
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object: id,
                        now_tapped: true,
                        actor: Some(payer),
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::UntapOthers { count, filter } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    find_objects(state, &f, &ctx)
                        .into_iter()
                        .filter(|&id| {
                            !(self_ctx.untaps_self && id == source)
                                && state
                                    .object(id)
                                    .is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED))
                        })
                        .take(need)
                        .collect()
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough tapped permanents to untap".into(),
                ));
            }
            for (idx, &id) in targets.iter().enumerate() {
                match replace::resolve_untap(state, id)? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                                kind: PendingEffectReplaceKind::Untap {
                                    current_object: id,
                                    remaining_objects: targets[idx + 1..].to_vec(),
                                },
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped {
                            if let Some(o) = state.object_mut(id) {
                                o.flags.remove(ObjectFlags::TAPPED);
                            }
                            state.mark_dirty(DirtyMask::TAPPED);
                            emit_event(
                                state,
                                AtomicEventKind::TapStateChanged {
                                    object: id,
                                    now_tapped: false,
                                    actor: Some(payer),
                                },
                            );
                        }
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::TapAttached => {
            let Some(host) = state.object(source).and_then(|o| o.attached_to) else {
                return Err(EngineError::IllegalAction("no attached host to tap".into()));
            };
            if state
                .object(host)
                .is_none_or(|h| h.flags.contains(ObjectFlags::TAPPED))
            {
                return Err(EngineError::IllegalAction(
                    "attached host already tapped".into(),
                ));
            }
            if let Some(h) = state.object_mut(host) {
                h.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object: host,
                    now_tapped: true,
                    actor: Some(payer),
                },
            );
            Ok(CostStepOutcome::Paid)
        }
        CostDef::ExertSelf => {
            if !can_pay_single_cost_cached(
                state,
                payer,
                source,
                cost,
                x_value,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction("cannot exert self".into()));
            }
            if let Some(o) = state.object_mut(source) {
                o.flags.insert(ObjectFlags::EXERTED);
            }
            if let Some(r) = state.rare_or_default(source) {
                r.doesnt_untap_next_untap = true;
            }
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            emit_event(
                state,
                AtomicEventKind::Exerted {
                    player: payer,
                    object: source,
                },
            );
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Sacrifice {
            count,
            target,
            sacrifice_all,
            attached_creature,
            one_or_more,
            ..
        } => {
            let (targets, req_min) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let targets: Vec<ObjectId> = if *attached_creature {
                    state
                        .object(source)
                        .and_then(|o| o.attached_to)
                        .into_iter()
                        .collect()
                } else if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let cands: Vec<ObjectId> = resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        target,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .into_iter()
                    .filter(|&id| !cant_sacrifice_object_cached(state, payer, id, false, cache))
                    .collect();
                    if *sacrifice_all {
                        cands
                    } else {
                        let need = if *one_or_more {
                            1
                        } else {
                            evaluate_amount(state, count, &ctx).max(0) as usize
                        };
                        cands.into_iter().take(need).collect()
                    }
                };
                let req_min = if *sacrifice_all {
                    0
                } else if *one_or_more {
                    1
                } else {
                    evaluate_amount(state, count, &ctx).max(0) as usize
                };
                if targets.len() < req_min {
                    return Err(EngineError::IllegalAction(
                        "insufficient objects to sacrifice".into(),
                    ));
                }
                if let Some(&first_id) = targets.first()
                    && cant_sacrifice_object_cached(state, payer, first_id, false, cache)
                {
                    return Err(EngineError::IllegalAction(
                        "object cannot be sacrificed".into(),
                    ));
                }
                (targets, req_min)
            };
            let _ = req_min;
            for (idx, &id) in targets.iter().enumerate() {
                if idx > 0 && cant_sacrifice_object(state, payer, id, false) {
                    return Err(EngineError::IllegalAction(
                        "object cannot be sacrificed".into(),
                    ));
                }
                match zones::move_object(
                    state,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions {
                        capture_lki: Some(true),
                        ..Default::default()
                    },
                    None,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                kind: PendingZoneCostKind::Sacrifice,
                                current_object: id,
                                remaining_objects: targets[idx + 1..].to_vec(),
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        let lki = raw.lki.unwrap_or_else(|| Arc::new(fallback_lki(state, id)));
                        if let Some(p) = plan.as_deref_mut() {
                            p.sacrificed_lki.push(Arc::clone(&lki));
                        }
                        emit_event(
                            state,
                            AtomicEventKind::Sacrificed {
                                player: payer,
                                object: id,
                                lki,
                            },
                        );
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Discard {
            count,
            target,
            random,
            ..
        } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, rand_pool, fixed_targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                if *random {
                    let mut pool = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            ZoneMask::HAND,
                            false,
                            cache,
                        ),
                        None => state
                            .player(payer)
                            .map(|p| p.hand.clone())
                            .unwrap_or_default(),
                    };
                    pool.retain(|&id| {
                        !self_ctx.excludes_object_in_zone(source, Zone::Hand, target.as_ref(), id)
                    });
                    (need, Some(pool), Vec::new())
                } else if !chosen_objects.is_empty() {
                    (need, None, chosen_objects.to_vec())
                } else {
                    let cands = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            ZoneMask::HAND,
                            false,
                            cache,
                        ),
                        None => state
                            .player(payer)
                            .map(|p| p.hand.clone())
                            .unwrap_or_default(),
                    };
                    (
                        need,
                        None,
                        cands
                            .into_iter()
                            .filter(|&id| {
                                !self_ctx.excludes_object_in_zone(
                                    source,
                                    Zone::Hand,
                                    target.as_ref(),
                                    id,
                                )
                            })
                            .take(need)
                            .collect(),
                    )
                }
            };
            let targets: Vec<ObjectId> = if let Some(mut pool) = rand_pool {
                if pool.len() < need {
                    return Err(EngineError::IllegalAction(
                        "not enough cards in hand to discard at random".into(),
                    ));
                }
                let mut picked = Vec::with_capacity(need);
                for _ in 0..need {
                    let idx = state.rng_mut().next_index(pool.len());
                    picked.push(pool.remove(idx));
                }
                picked
            } else {
                fixed_targets
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough cards in hand to discard".into(),
                ));
            }
            for (idx, &id) in targets.iter().enumerate() {
                match zones::move_object(
                    state,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions {
                        is_discard: true,
                        discard_caused_by: Some(payer),
                        ..Default::default()
                    },
                    None,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                kind: PendingZoneCostKind::Discard,
                                current_object: id,
                                remaining_objects: targets[idx + 1..].to_vec(),
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.discarded_cards.push(id);
                        }
                        emit_event(
                            state,
                            AtomicEventKind::Discard {
                                player: payer,
                                object: raw.new_id.unwrap_or(id),
                                dest_zone: raw.to_zone,
                                is_cycle: false,
                                cause_controller: Some(payer),
                            },
                        );
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::DiscardHand => {
            let hand = state
                .player(payer)
                .map(|p| p.hand.clone())
                .unwrap_or_default();
            for (idx, &id) in hand.iter().enumerate() {
                match zones::move_object(
                    state,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions {
                        is_discard: true,
                        discard_caused_by: Some(payer),
                        ..Default::default()
                    },
                    None,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                kind: PendingZoneCostKind::Discard,
                                current_object: id,
                                remaining_objects: hand[idx + 1..].to_vec(),
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.discarded_cards.push(id);
                        }
                        emit_event(
                            state,
                            AtomicEventKind::Discard {
                                player: payer,
                                object: raw.new_id.unwrap_or(id),
                                dest_zone: raw.to_zone,
                                is_cycle: false,
                                cause_controller: Some(payer),
                            },
                        );
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::PayLife {
            amount,
            half_life_rounded_up,
        } => {
            let amt = {
                let cache = state.evaluate_board();
                if !can_pay_single_cost_cached(state, payer, source, cost, x_value, cache) {
                    return Err(EngineError::IllegalAction(
                        "insufficient life or cannot pay life".into(),
                    ));
                }
                let cur_life = state.player(payer).map_or(0, |p| p.life);
                if *half_life_rounded_up {
                    (cur_life.max(0) + 1) / 2
                } else {
                    let mut ctx =
                        EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                    ctx.x_value = x_value;
                    evaluate_amount(state, amount, &ctx).max(0)
                }
            };
            if let Some(p) = state.player_mut(payer) {
                p.life -= amt;
            }
            state.mark_dirty(DirtyMask::LIFE);
            if amt > 0 {
                emit_event(
                    state,
                    AtomicEventKind::LifeChange {
                        player: payer,
                        delta: -amt,
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::OpponentGainsLife {
            amount,
            each_other_player,
        } => {
            let (amt, recipients) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let amt = evaluate_amount(state, amount, &ctx).max(0) as u32;
                let recipients: Vec<PlayerId> = state
                    .players()
                    .iter()
                    .filter(|p| !p.lost && p.id != payer)
                    .map(|p| p.id)
                    .take(if *each_other_player { usize::MAX } else { 1 })
                    .collect();
                (amt, recipients)
            };
            for (idx, &opp) in recipients.iter().enumerate() {
                match replace::resolve_life_gain(state, opp, amt, Some(source))? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                                kind: PendingEffectReplaceKind::OpponentGainsLife {
                                    current_opponent: opp,
                                    remaining_opponents: recipients[idx + 1..].to_vec(),
                                    amount: amt,
                                },
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped
                            && let ReplaceableEventKind::LifeGain {
                                player: lp,
                                amount: final_amt,
                                ..
                            } = out.event
                            && final_amt > 0
                            && !state
                                .evaluate_board()
                                .player(lp)
                                .is_some_and(|ep| ep.cant_gain_life || ep.life_cant_change)
                        {
                            if let Some(p) = state.player_mut(lp) {
                                p.life += final_amt as i32;
                            }
                            state.mark_dirty(DirtyMask::LIFE);
                            emit_event(
                                state,
                                AtomicEventKind::LifeChange {
                                    player: lp,
                                    delta: final_amt as i32,
                                },
                            );
                        }
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Loyalty { delta, is_x, sign } => {
            if !can_pay_single_cost_cached(
                state,
                payer,
                source,
                cost,
                x_value,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction("cannot pay loyalty cost".into()));
            }
            let loy_cid = state.db().counter_id("loyalty").ok_or_else(|| {
                EngineError::IllegalAction("loyalty counter missing in DB".into())
            })?;
            let eff_delta = if *is_x {
                let xv = x_value.unwrap_or(0) as i32;
                match sign {
                    Some(LoyaltySign::Minus) => -xv,
                    _ => xv,
                }
            } else {
                *delta
            };
            if eff_delta > 0 {
                match replace::resolve_put_counters(
                    state,
                    source,
                    loy_cid,
                    eff_delta as u32,
                    Some(payer),
                    Some(source),
                )? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                                kind: PendingEffectReplaceKind::LoyaltyPositive {
                                    counter: loy_cid,
                                },
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped
                            && let ReplaceableEventKind::PutCounters {
                                object,
                                counter,
                                count,
                                ..
                            } = out.event
                            && count > 0
                        {
                            let prev = state.counter_count(object, counter);
                            state.add_counter(object, counter, count);
                            emit_event(
                                state,
                                AtomicEventKind::CounterChanged {
                                    object,
                                    counter,
                                    previous_count: prev,
                                    new_count: prev + count,
                                    actor: Some(payer),
                                },
                            );
                        }
                    }
                }
            } else if eff_delta < 0 {
                let rem = (-eff_delta) as u32;
                let prev = state.counter_count(source, loy_cid);
                let actually = state.remove_counter(source, loy_cid, rem);
                if actually > 0 {
                    emit_event(
                        state,
                        AtomicEventKind::CounterChanged {
                            object: source,
                            counter: loy_cid,
                            previous_count: prev,
                            new_count: prev.saturating_sub(actually),
                            actor: Some(payer),
                        },
                    );
                }
            }
            if let Some(r) = state.rare_or_default(source) {
                r.used_loyalty_this_turn = true;
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::RemoveCounters {
            counter,
            count,
            remove_all,
            from,
            one_or_more,
            ..
        } => {
            let (cands, mut need) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let cands: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        from,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                };
                let need = if *remove_all {
                    u32::MAX
                } else if *one_or_more {
                    1
                } else {
                    evaluate_amount(state, count, &ctx).max(0) as u32
                };
                (cands, need)
            };
            let mut removed_total = 0u32;
            for id in cands {
                if need == 0 {
                    break;
                }
                let list: SmallVec<[(CounterId, u32); 2]> = match counter {
                    CounterTarget::Specific(cid) => {
                        let cnt = state.counter_count(id, *cid);
                        if cnt > 0 {
                            SmallVec::from_slice(&[(*cid, cnt)])
                        } else {
                            SmallVec::new()
                        }
                    }
                    CounterTarget::Any | CounterTarget::Each => {
                        state.counters(id).iter().copied().collect()
                    }
                };
                for (cid, cnt) in list {
                    if need == 0 {
                        break;
                    }
                    let take = cnt.min(need);
                    let prev = state.counter_count(id, cid);
                    let got = state.remove_counter(id, cid, take);
                    if got > 0 {
                        removed_total = removed_total.saturating_add(got);
                        if !*remove_all {
                            need = need.saturating_sub(got);
                        }
                        emit_event(
                            state,
                            AtomicEventKind::CounterChanged {
                                object: id,
                                counter: cid,
                                previous_count: prev,
                                new_count: prev.saturating_sub(got),
                                actor: Some(payer),
                            },
                        );
                    }
                }
            }
            if !*remove_all && need > 0 {
                return Err(EngineError::IllegalAction(
                    "not enough counters to remove".into(),
                ));
            }
            if let Some(p) = plan.as_deref_mut() {
                p.counters_removed = p.counters_removed.saturating_add(removed_total);
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::PutCounters { count, counter, on } => {
            let (amt, target) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let amt = evaluate_amount(state, count, &ctx).max(0) as u32;
                if amt == 0 {
                    return Ok(CostStepOutcome::Paid);
                }
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        on,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .into_iter()
                    .take(1)
                    .collect()
                };
                let Some(&target) = targets.first() else {
                    return Err(EngineError::IllegalAction(
                        "no valid target to put counters on".into(),
                    ));
                };
                (amt, target)
            };
            match replace::resolve_put_counters(
                state,
                target,
                *counter,
                amt,
                Some(payer),
                Some(source),
            )? {
                ReplacementResult::Paused { prompt_id, chooser } => {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                            kind: PendingEffectReplaceKind::PutCounters,
                        });
                    }
                    Ok(CostStepOutcome::Paused { prompt_id, chooser })
                }
                ReplacementResult::Resolved(out) => {
                    if !out.mods.skipped
                        && let ReplaceableEventKind::PutCounters {
                            object,
                            counter: final_c,
                            count: final_n,
                            ..
                        } = out.event
                        && final_n > 0
                    {
                        let prev = state.counter_count(object, final_c);
                        state.add_counter(object, final_c, final_n);
                        emit_event(
                            state,
                            AtomicEventKind::CounterChanged {
                                object,
                                counter: final_c,
                                previous_count: prev,
                                new_count: prev + final_n,
                                actor: Some(payer),
                            },
                        );
                    }
                    Ok(CostStepOutcome::Paid)
                }
            }
        }
        CostDef::ExileFrom {
            zone,
            count,
            target,
            from_top,
            any_graveyard,
            collect_evidence,
            one_or_more,
            ..
        } => {
            let (targets, first_mv) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = if *one_or_more {
                    1
                } else {
                    evaluate_amount(state, count, &ctx).max(0) as usize
                };
                let targets: Vec<ObjectId> = if *from_top && *zone == Zone::Library {
                    state
                        .player(payer)
                        .map(|p| p.library.iter().rev().copied().take(need).collect())
                        .unwrap_or_default()
                } else if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let zm = ZoneMask::from_zone(*zone);
                    let cands = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            zm,
                            !*any_graveyard,
                            cache,
                        ),
                        None => state
                            .player(payer)
                            .map(|p| match zone {
                                Zone::Graveyard => p.graveyard.clone(),
                                Zone::Hand => p.hand.clone(),
                                Zone::Battlefield => p.battlefield.clone(),
                                _ => Vec::new(),
                            })
                            .unwrap_or_default(),
                    };
                    cands.into_iter().take(need).collect()
                };
                let first_mv = if *collect_evidence {
                    targets
                        .first()
                        .map_or(0, |&id| object_mana_value(state, id, &ctx))
                } else {
                    0
                };
                (targets, first_mv)
            };

            let mut total_mv = 0u32;
            for (idx, &id) in targets.iter().enumerate() {
                let item_mv = if *collect_evidence {
                    if idx == 0 {
                        first_mv
                    } else {
                        let cache = state.evaluate_board();
                        let mut mv_ctx = EvalContext::for_source(payer, source)
                            .with_board(BoardView::Cached(cache));
                        mv_ctx.x_value = x_value;
                        object_mana_value(state, id, &mv_ctx)
                    }
                } else {
                    0
                };
                match zones::move_object(state, id, Zone::Exile, MoveRawOptions::default(), None)? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        if let Some(p) = plan.as_deref_mut() {
                            p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                kind: PendingZoneCostKind::ExileFrom {
                                    source,
                                    collect_evidence: *collect_evidence,
                                    accumulated_mv: total_mv,
                                },
                                current_object: id,
                                remaining_objects: targets[idx + 1..].to_vec(),
                            });
                        }
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        total_mv = total_mv.saturating_add(item_mv);
                        if let Some(new_id) = raw.new_id
                            && let Some(r) = state.rare_or_default(new_id)
                        {
                            r.exiled_by = Some(source);
                            state.mark_dirty(DirtyMask::EXILE);
                        }
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            if *collect_evidence {
                emit_event(
                    state,
                    AtomicEventKind::CollectEvidence {
                        player: payer,
                        total_mv,
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::RevealFromHand { count, target, .. } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let cands = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state,
                            payer,
                            source,
                            t,
                            ZoneMask::HAND,
                            false,
                            cache,
                        ),
                        None => state
                            .player(payer)
                            .map(|p| p.hand.clone())
                            .unwrap_or_default(),
                    };
                    cands
                        .into_iter()
                        .filter(|&id| {
                            !self_ctx.excludes_object_in_zone(
                                source,
                                Zone::Hand,
                                target.as_ref(),
                                id,
                            )
                        })
                        .take(need)
                        .collect()
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough cards to reveal from hand".into(),
                ));
            }
            for id in targets {
                zones::reveal_to_all(state, id);
                if let Some(p) = plan.as_deref_mut() {
                    p.revealed_cards.push(id);
                    if let Some(o) = state.object(id) {
                        p.revealed_card_defs.push(o.card_def);
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::RevealHand => {
            let hand = state
                .player(payer)
                .map(|p| p.hand.clone())
                .unwrap_or_default();
            for id in hand {
                zones::reveal_to_all(state, id);
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::RevealSelfFromHand => {
            if !can_pay_single_cost_cached(
                state,
                payer,
                source,
                cost,
                x_value,
                state.evaluate_board(),
            ) {
                return Err(EngineError::IllegalAction(
                    "self not in hand to reveal".into(),
                ));
            }
            zones::reveal_to_all(state, source);
            if let Some(p) = plan.as_deref_mut() {
                p.revealed_cards.push(source);
                if let Some(o) = state.object(source) {
                    p.revealed_card_defs.push(o.card_def);
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Choice { options, forage } => {
            if *forage {
                let gy = state
                    .player(payer)
                    .map(|p| p.graveyard.clone())
                    .unwrap_or_default();
                if gy.len() >= 3 {
                    let exile_targets: Vec<ObjectId> = gy.into_iter().take(3).collect();
                    for (idx, &id) in exile_targets.iter().enumerate() {
                        if let MoveObjectOutcome::Paused { prompt_id, chooser } =
                            zones::move_object(
                                state,
                                id,
                                Zone::Exile,
                                MoveRawOptions::default(),
                                None,
                            )?
                        {
                            if let Some(p) = plan.as_deref_mut() {
                                p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                    kind: PendingZoneCostKind::ForageExile,
                                    current_object: id,
                                    remaining_objects: exile_targets[idx + 1..].to_vec(),
                                });
                            }
                            return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                        }
                    }
                    emit_event(state, AtomicEventKind::Forage { player: payer });
                    return Ok(CostStepOutcome::Paid);
                }
                if let Some(food_id) = state.db().subtype_id("Food") {
                    let food = {
                        let cache = state.evaluate_board();
                        state.player(payer).and_then(|p| {
                            p.battlefield.iter().copied().find(|&id| {
                                cache.get(id).is_some_and(|e| e.subtypes.contains(&food_id))
                                    && !cant_sacrifice_object_cached(state, payer, id, false, cache)
                            })
                        })
                    };
                    if let Some(fid) = food {
                        match zones::move_object(
                            state,
                            fid,
                            Zone::Graveyard,
                            MoveRawOptions {
                                capture_lki: Some(true),
                                ..Default::default()
                            },
                            None,
                        )? {
                            MoveObjectOutcome::Paused { prompt_id, chooser } => {
                                if let Some(p) = plan.as_deref_mut() {
                                    p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                                        kind: PendingZoneCostKind::ForageFood,
                                        current_object: fid,
                                        remaining_objects: Vec::new(),
                                    });
                                }
                                return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                            }
                            MoveObjectOutcome::Moved(raw) => {
                                let lki = raw
                                    .lki
                                    .unwrap_or_else(|| Arc::new(fallback_lki(state, fid)));
                                emit_event(
                                    state,
                                    AtomicEventKind::Sacrificed {
                                        player: payer,
                                        object: fid,
                                        lki,
                                    },
                                );
                            }
                            _ => {}
                        }
                        emit_event(state, AtomicEventKind::Forage { player: payer });
                        return Ok(CostStepOutcome::Paid);
                    }
                }
                return Err(EngineError::IllegalAction("cannot pay Forage cost".into()));
            }
            // Pick first payable branch (checking both non-mana costs and pool mana for CostDef::Mana)
            let branch = {
                let cache = state.evaluate_board();
                options.iter().find(|b| {
                    can_pay_branch_with_pool_mana_cached(state, payer, source, b, x_value, cache)
                })
            };
            let Some(branch) = branch else {
                return Err(EngineError::IllegalAction(
                    "no payable option in Choice cost".into(),
                ));
            };
            for sub in branch {
                match execute_non_mana_cost_step(
                    state,
                    payer,
                    source,
                    sub,
                    chosen_objects,
                    x_value,
                    plan.as_deref_mut(),
                )? {
                    CostStepOutcome::Paused { prompt_id, chooser } => {
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    CostStepOutcome::Paid => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Optional { cost: inner } => {
            let can_pay = {
                let cache = state.evaluate_board();
                can_pay_branch_with_pool_mana_cached(state, payer, source, inner, x_value, cache)
            };
            if can_pay {
                for sub in inner {
                    match execute_non_mana_cost_step(
                        state,
                        payer,
                        source,
                        sub,
                        chosen_objects,
                        x_value,
                        plan.as_deref_mut(),
                    )? {
                        CostStepOutcome::Paused { prompt_id, chooser } => {
                            return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                        }
                        CostStepOutcome::Paid => {}
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::TapSubsetSum {
            min_total,
            filter,
            exclude_self,
            ..
        } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (req, chosen, sum_pow) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let req = evaluate_amount(state, min_total, &ctx).max(0);
                let chosen: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let mut f = (**filter).clone();
                    if f.zones.is_empty() {
                        f.zones = ZoneMask::BATTLEFIELD;
                    }
                    let mut acc = 0i32;
                    let mut picked = Vec::new();
                    for id in find_objects(state, &f, &ctx) {
                        if ((*exclude_self || self_ctx.taps_self) && id == source)
                            || self_ctx.tap_attached_host == Some(id)
                        {
                            continue;
                        }
                        if state
                            .object(id)
                            .is_none_or(|o| o.flags.contains(ObjectFlags::TAPPED))
                        {
                            continue;
                        }
                        if cant_crew_vehicle_cached(state, id, cache) {
                            continue;
                        }
                        let pow =
                            crate::eval::object_pt(state, id, &ctx).map_or(0, |pt| pt.power.max(0));
                        picked.push(id);
                        acc += pow;
                        if acc >= req {
                            break;
                        }
                    }
                    picked
                };
                let sum_pow: i32 = chosen
                    .iter()
                    .filter_map(|&id| {
                        crate::eval::object_pt(state, id, &ctx).map(|pt| pt.power.max(0))
                    })
                    .sum();
                (req, chosen, sum_pow)
            };
            if sum_pow < req {
                return Err(EngineError::IllegalAction(
                    "tapped creatures do not meet minimum power".into(),
                ));
            }
            for &id in &chosen {
                if let Some(o) = state.object_mut(id) {
                    o.flags.insert(ObjectFlags::TAPPED);
                }
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object: id,
                        now_tapped: true,
                        actor: Some(payer),
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::PayEnergy { count } => {
            let need = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                evaluate_amount(state, count, &ctx).max(0) as u16
            };
            let cur = state.player(payer).map_or(0, |p| p.energy);
            if cur < need {
                return Err(EngineError::IllegalAction("insufficient energy".into()));
            }
            if let Some(p) = state.player_mut(payer) {
                p.energy -= need;
            }
            state.mark_dirty(DirtyMask::PLAYER_STATE);
            Ok(CostStepOutcome::Paid)
        }
        CostDef::ReturnToHand { count, target } => {
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    resolve_cost_object_candidates_cached(
                        state,
                        payer,
                        source,
                        target,
                        ZoneMask::BATTLEFIELD,
                        true,
                        cache,
                    )
                    .into_iter()
                    .take(need)
                    .collect()
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough permanents to return to hand".into(),
                ));
            }
            for (idx, &id) in targets.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } =
                    zones::move_object(state, id, Zone::Hand, MoveRawOptions::default(), None)?
                {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::ReturnToHand,
                            current_object: id,
                            remaining_objects: targets[idx + 1..].to_vec(),
                        });
                    }
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Mill { count } => {
            let need = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                evaluate_amount(state, count, &ctx).max(0) as usize
            };
            let lib_len = state.player(payer).map_or(0, |p| p.library.len());
            if lib_len < need {
                return Err(EngineError::IllegalAction(
                    "not enough cards in library to mill for cost".into(),
                ));
            }
            for i in 0..need {
                let top = state.player(payer).and_then(|p| p.library.last().copied());
                let Some(card_id) = top else { break };
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    card_id,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )? {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::Mill {
                                remaining_after_current: need - 1 - i,
                            },
                            current_object: card_id,
                            remaining_objects: Vec::new(),
                        });
                    }
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::Waterbend { cost: mc } => {
            let mut rem_cost = mc.clone();
            let tap_pool = if !chosen_objects.is_empty() {
                chosen_objects.to_vec()
            } else {
                waterbend_tap_candidates_cached(state, payer, state.evaluate_board())
            };
            for id in tap_pool {
                if rem_cost.generic == 0 {
                    break;
                }
                rem_cost.generic -= 1;
                if let Some(o) = state.object_mut(id) {
                    o.flags.insert(ObjectFlags::TAPPED);
                }
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object: id,
                        now_tapped: true,
                        actor: Some(payer),
                    },
                );
            }
            let spend_ctx = mana::ManaSpendContext::for_generic_cost(payer);
            mana::deduct_mana_for_context(state, payer, &rem_cost, &spend_ctx, &[])?;
            Ok(CostStepOutcome::Paid)
        }
        CostDef::PutFromHandOnTopOfLibrary { count, filter } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    match filter {
                        Some(f) => {
                            let mut hf = (**f).clone();
                            hf.zones = ZoneMask::HAND;
                            find_objects(state, &hf, &ctx)
                                .into_iter()
                                .filter(|&id| {
                                    !self_ctx.excludes_object_in_zone(source, Zone::Hand, None, id)
                                })
                                .take(need)
                                .collect()
                        }
                        None => state
                            .player(payer)
                            .map(|p| {
                                p.hand
                                    .iter()
                                    .copied()
                                    .filter(|&id| {
                                        !self_ctx.excludes_object_in_zone(
                                            source,
                                            Zone::Hand,
                                            None,
                                            id,
                                        )
                                    })
                                    .take(need)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    }
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough cards in hand for top of library".into(),
                ));
            }
            for (idx, &id) in targets.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    id,
                    Zone::Library,
                    MoveRawOptions {
                        position: ZonePosition::Top,
                        ..Default::default()
                    },
                    None,
                )? {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::PutOnTopOfLibrary,
                            current_object: id,
                            remaining_objects: targets[idx + 1..].to_vec(),
                        });
                    }
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::PutOnBottomOfLibrary {
            from,
            count,
            target,
        } => {
            let self_ctx = plan
                .as_deref()
                .and_then(|p| p.remaining_non_mana_costs.get(p.non_mana_cursor..))
                .map_or_else(CostListSelfContext::default, |rem| {
                    CostListSelfContext::from_costs(state, source, rem)
                });
            let (need, targets) = {
                let cache = state.evaluate_board();
                let mut ctx =
                    EvalContext::for_source(payer, source).with_board(BoardView::Cached(cache));
                ctx.x_value = x_value;
                let need = evaluate_amount(state, count, &ctx).max(0) as usize;
                let targets: Vec<ObjectId> = if !chosen_objects.is_empty() {
                    chosen_objects.to_vec()
                } else {
                    let zm = ZoneMask::from_zone(*from);
                    let cands = match target {
                        Some(t) => resolve_cost_object_candidates_cached(
                            state, payer, source, t, zm, true, cache,
                        ),
                        None => match from {
                            Zone::Hand => state
                                .player(payer)
                                .map(|p| p.hand.clone())
                                .unwrap_or_default(),
                            Zone::Graveyard => state
                                .player(payer)
                                .map(|p| p.graveyard.clone())
                                .unwrap_or_default(),
                            Zone::Battlefield => state
                                .player(payer)
                                .map(|p| p.battlefield.clone())
                                .unwrap_or_default(),
                            _ => Vec::new(),
                        },
                    };
                    cands
                        .into_iter()
                        .filter(|&id| {
                            !self_ctx.excludes_object_in_zone(source, *from, target.as_ref(), id)
                        })
                        .take(need)
                        .collect()
                };
                (need, targets)
            };
            if targets.len() < need {
                return Err(EngineError::IllegalAction(
                    "not enough objects for bottom of library".into(),
                ));
            }
            for (idx, &id) in targets.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    id,
                    Zone::Library,
                    MoveRawOptions {
                        position: ZonePosition::Bottom,
                        ..Default::default()
                    },
                    None,
                )? {
                    if let Some(p) = plan.as_deref_mut() {
                        p.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::PutOnBottomOfLibrary,
                            current_object: id,
                            remaining_objects: targets[idx + 1..].to_vec(),
                        });
                    }
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        CostDef::UnattachSelf => {
            let Some(host) = state.object(source).and_then(|o| o.attached_to) else {
                return Err(EngineError::IllegalAction("not attached".into()));
            };
            if let Some(o) = state.object_mut(source) {
                o.attached_to = None;
            }
            state.mark_dirty(DirtyMask::TOPOLOGY);
            emit_event(
                state,
                AtomicEventKind::AttachmentChanged {
                    attachment: source,
                    host,
                    attached: false,
                },
            );
            Ok(CostStepOutcome::Paid)
        }
        CostDef::ChooseCreatureType => {
            let default_st = state
                .db()
                .tables
                .subtypes
                .first()
                .map(|_| mtg_ir::SubtypeId(0));
            if let Some(r) = state.rare_or_default(source) {
                r.chosen_subtype = default_st;
            }
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            Ok(CostStepOutcome::Paid)
        }
        CostDef::ChooseNumberBetween { min, max } => {
            let prompt_id = state.allocate_prompt_id();
            let resume_decision = state.decision().clone();
            let resume_priority_player = state.priority_player();
            let request = ChoiceRequest::ChooseNumber {
                prompt_id,
                min: *min,
                max: *max,
            };
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: payer,
                request: request.clone(),
                owner: PromptOwner::CostPayment(CostPaymentPromptState {
                    controller: payer,
                    source,
                    kind: CostPromptKind::ChooseNumber {
                        cost_index: 0,
                        min: *min,
                        max: *max,
                    },
                    resume_decision,
                    resume_priority_player,
                }),
            }));
            state.set_decision(DecisionState::Prompt { request });
            state.set_priority_player(payer);
            Ok(CostStepOutcome::Paused {
                prompt_id,
                chooser: payer,
            })
        }
        CostDef::ChooseFromKeywordList { keywords } => {
            if let Some(&kw) = keywords.first()
                && let Some(r) = state.rare_or_default(source)
                && !r.granted_keywords.contains(&kw)
            {
                r.granted_keywords.push(kw);
            }
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            Ok(CostStepOutcome::Paid)
        }
        // Rare Phase-2-only CostDefs
        CostDef::DiscardLastDrawnThisTurn
        | CostDef::Behold { .. }
        | CostDef::BeholdOrChoose { .. }
        | CostDef::ProcessOpponentExile { .. }
        | CostDef::PutExiledWithSelfToGraveyard { .. }
        | CostDef::UnattachOther { .. }
        | CostDef::RollDie { .. }
        | CostDef::FlipCoin
        | CostDef::RevealChosenPlayer
        | CostDef::RevealChosenSubType
        | CostDef::PayAttachedManaCost
        | CostDef::ExileFromGraveyardAndPayManaCost { .. }
        | CostDef::ExileFaceDownPermanentFaceUp
        | CostDef::PayManaPerCounter { .. }
        | CostDef::RepeatableOptionalMana { .. } => {
            Err(EngineError::NotImplemented("rare Phase-2 CostDef"))
        }
    }
}

pub(crate) fn resume_paused_non_mana_cost_zone_step(
    state: &mut GameState,
    payer: PlayerId,
    _source: ObjectId,
    plan: &mut PendingCostPaymentPlan,
    mv: MoveObjectOutcome,
) -> Result<CostStepOutcome, EngineError> {
    let Some(PendingNonManaCostStep::ZoneMove {
        kind,
        current_object,
        remaining_objects,
    }) = plan.pending_step.take()
    else {
        return Err(EngineError::IllegalAction(
            "no paused non-mana zone cost step".into(),
        ));
    };

    if let MoveObjectOutcome::Paused { prompt_id, chooser } = mv {
        plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
            kind,
            current_object,
            remaining_objects,
        });
        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
    }

    match kind {
        PendingZoneCostKind::Sacrifice => {
            if let MoveObjectOutcome::Moved(raw) = mv {
                let lki = raw
                    .lki
                    .unwrap_or_else(|| Arc::new(fallback_lki(state, current_object)));
                plan.sacrificed_lki.push(Arc::clone(&lki));
                emit_event(
                    state,
                    AtomicEventKind::Sacrificed {
                        player: payer,
                        object: current_object,
                        lki,
                    },
                );
            }
            for (idx, &id) in remaining_objects.iter().enumerate() {
                if cant_sacrifice_object(state, payer, id, false) {
                    return Err(EngineError::IllegalAction(
                        "object cannot be sacrificed".into(),
                    ));
                }
                match zones::move_object(
                    state,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions {
                        capture_lki: Some(true),
                        ..Default::default()
                    },
                    None,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::Sacrifice,
                            current_object: id,
                            remaining_objects: remaining_objects[idx + 1..].to_vec(),
                        });
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        let lki = raw.lki.unwrap_or_else(|| Arc::new(fallback_lki(state, id)));
                        plan.sacrificed_lki.push(Arc::clone(&lki));
                        emit_event(
                            state,
                            AtomicEventKind::Sacrificed {
                                player: payer,
                                object: id,
                                lki,
                            },
                        );
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::Discard => {
            if let MoveObjectOutcome::Moved(raw) = mv {
                plan.discarded_cards.push(current_object);
                emit_event(
                    state,
                    AtomicEventKind::Discard {
                        player: payer,
                        object: raw.new_id.unwrap_or(current_object),
                        dest_zone: raw.to_zone,
                        is_cycle: false,
                        cause_controller: Some(payer),
                    },
                );
            }
            for (idx, &id) in remaining_objects.iter().enumerate() {
                match zones::move_object(
                    state,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions {
                        is_discard: true,
                        discard_caused_by: Some(payer),
                        ..Default::default()
                    },
                    None,
                )? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::Discard,
                            current_object: id,
                            remaining_objects: remaining_objects[idx + 1..].to_vec(),
                        });
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        plan.discarded_cards.push(id);
                        emit_event(
                            state,
                            AtomicEventKind::Discard {
                                player: payer,
                                object: raw.new_id.unwrap_or(id),
                                dest_zone: raw.to_zone,
                                is_cycle: false,
                                cause_controller: Some(payer),
                            },
                        );
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::ExileFrom {
            source: cost_source,
            collect_evidence,
            mut accumulated_mv,
        } => {
            if let MoveObjectOutcome::Moved(raw) = mv {
                if collect_evidence {
                    let mv_ctx = EvalContext::for_source(payer, cost_source)
                        .with_board(BoardView::Cached(state.evaluate_board()));
                    let item_mv = raw.new_id.map_or_else(
                        || raw.lki.as_ref().map_or(0, |l| l.mana_value),
                        |nid| object_mana_value(state, nid, &mv_ctx),
                    );
                    accumulated_mv = accumulated_mv.saturating_add(item_mv);
                }
                if let Some(new_id) = raw.new_id
                    && let Some(r) = state.rare_or_default(new_id)
                {
                    r.exiled_by = Some(cost_source);
                    state.mark_dirty(DirtyMask::EXILE);
                }
            }
            for (idx, &id) in remaining_objects.iter().enumerate() {
                let item_mv = if collect_evidence {
                    let mv_ctx = EvalContext::for_source(payer, cost_source)
                        .with_board(BoardView::Cached(state.evaluate_board()));
                    object_mana_value(state, id, &mv_ctx)
                } else {
                    0
                };
                match zones::move_object(state, id, Zone::Exile, MoveRawOptions::default(), None)? {
                    MoveObjectOutcome::Paused { prompt_id, chooser } => {
                        plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                            kind: PendingZoneCostKind::ExileFrom {
                                source: cost_source,
                                collect_evidence,
                                accumulated_mv,
                            },
                            current_object: id,
                            remaining_objects: remaining_objects[idx + 1..].to_vec(),
                        });
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    MoveObjectOutcome::Moved(raw) => {
                        accumulated_mv = accumulated_mv.saturating_add(item_mv);
                        if let Some(new_id) = raw.new_id
                            && let Some(r) = state.rare_or_default(new_id)
                        {
                            r.exiled_by = Some(cost_source);
                            state.mark_dirty(DirtyMask::EXILE);
                        }
                    }
                    MoveObjectOutcome::Skipped { .. }
                    | MoveObjectOutcome::ExecuteInstead { .. } => {}
                }
            }
            if collect_evidence {
                emit_event(
                    state,
                    AtomicEventKind::CollectEvidence {
                        player: payer,
                        total_mv: accumulated_mv,
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::ForageExile => {
            for (idx, &id) in remaining_objects.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } =
                    zones::move_object(state, id, Zone::Exile, MoveRawOptions::default(), None)?
                {
                    plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                        kind: PendingZoneCostKind::ForageExile,
                        current_object: id,
                        remaining_objects: remaining_objects[idx + 1..].to_vec(),
                    });
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            emit_event(state, AtomicEventKind::Forage { player: payer });
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::ForageFood => {
            if let MoveObjectOutcome::Moved(raw) = mv {
                let lki = raw
                    .lki
                    .unwrap_or_else(|| Arc::new(fallback_lki(state, current_object)));
                emit_event(
                    state,
                    AtomicEventKind::Sacrificed {
                        player: payer,
                        object: current_object,
                        lki,
                    },
                );
            }
            emit_event(state, AtomicEventKind::Forage { player: payer });
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::ReturnToHand => {
            for (idx, &id) in remaining_objects.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } =
                    zones::move_object(state, id, Zone::Hand, MoveRawOptions::default(), None)?
                {
                    plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                        kind: PendingZoneCostKind::ReturnToHand,
                        current_object: id,
                        remaining_objects: remaining_objects[idx + 1..].to_vec(),
                    });
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::Mill {
            remaining_after_current,
        } => {
            for i in 0..remaining_after_current {
                let top = state.player(payer).and_then(|p| p.library.last().copied());
                let Some(card_id) = top else { break };
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    card_id,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )? {
                    plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                        kind: PendingZoneCostKind::Mill {
                            remaining_after_current: remaining_after_current - 1 - i,
                        },
                        current_object: card_id,
                        remaining_objects: Vec::new(),
                    });
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::PutOnTopOfLibrary => {
            for (idx, &id) in remaining_objects.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    id,
                    Zone::Library,
                    MoveRawOptions {
                        position: ZonePosition::Top,
                        ..Default::default()
                    },
                    None,
                )? {
                    plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                        kind: PendingZoneCostKind::PutOnTopOfLibrary,
                        current_object: id,
                        remaining_objects: remaining_objects[idx + 1..].to_vec(),
                    });
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingZoneCostKind::PutOnBottomOfLibrary => {
            for (idx, &id) in remaining_objects.iter().enumerate() {
                if let MoveObjectOutcome::Paused { prompt_id, chooser } = zones::move_object(
                    state,
                    id,
                    Zone::Library,
                    MoveRawOptions {
                        position: ZonePosition::Bottom,
                        ..Default::default()
                    },
                    None,
                )? {
                    plan.pending_step = Some(PendingNonManaCostStep::ZoneMove {
                        kind: PendingZoneCostKind::PutOnBottomOfLibrary,
                        current_object: id,
                        remaining_objects: remaining_objects[idx + 1..].to_vec(),
                    });
                    return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                }
            }
            Ok(CostStepOutcome::Paid)
        }
    }
}

pub(crate) fn resume_paused_non_mana_cost_effect_step(
    state: &mut GameState,
    payer: PlayerId,
    source: ObjectId,
    plan: &mut PendingCostPaymentPlan,
    outcome: ReplacementOutcome,
) -> Result<CostStepOutcome, EngineError> {
    let Some(PendingNonManaCostStep::EffectReplace { kind }) = plan.pending_step.take() else {
        return Err(EngineError::IllegalAction(
            "no paused non-mana effect cost step".into(),
        ));
    };

    match kind {
        PendingEffectReplaceKind::Untap {
            current_object,
            remaining_objects,
        } => {
            if !outcome.mods.skipped {
                if let Some(o) = state.object_mut(current_object) {
                    o.flags.remove(ObjectFlags::TAPPED);
                }
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object: current_object,
                        now_tapped: false,
                        actor: Some(payer),
                    },
                );
            }
            for (idx, &id) in remaining_objects.iter().enumerate() {
                match replace::resolve_untap(state, id)? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        plan.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                            kind: PendingEffectReplaceKind::Untap {
                                current_object: id,
                                remaining_objects: remaining_objects[idx + 1..].to_vec(),
                            },
                        });
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped {
                            if let Some(o) = state.object_mut(id) {
                                o.flags.remove(ObjectFlags::TAPPED);
                            }
                            state.mark_dirty(DirtyMask::TAPPED);
                            emit_event(
                                state,
                                AtomicEventKind::TapStateChanged {
                                    object: id,
                                    now_tapped: false,
                                    actor: Some(payer),
                                },
                            );
                        }
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingEffectReplaceKind::OpponentGainsLife {
            current_opponent: _,
            remaining_opponents,
            amount,
        } => {
            if !outcome.mods.skipped
                && let ReplaceableEventKind::LifeGain {
                    player: lp,
                    amount: final_amt,
                    ..
                } = outcome.event
                && final_amt > 0
                && !state
                    .evaluate_board()
                    .player(lp)
                    .is_some_and(|ep| ep.cant_gain_life || ep.life_cant_change)
            {
                if let Some(p) = state.player_mut(lp) {
                    p.life += final_amt as i32;
                }
                state.mark_dirty(DirtyMask::LIFE);
                emit_event(
                    state,
                    AtomicEventKind::LifeChange {
                        player: lp,
                        delta: final_amt as i32,
                    },
                );
            }
            for (idx, &opp) in remaining_opponents.iter().enumerate() {
                match replace::resolve_life_gain(state, opp, amount, Some(source))? {
                    ReplacementResult::Paused { prompt_id, chooser } => {
                        plan.pending_step = Some(PendingNonManaCostStep::EffectReplace {
                            kind: PendingEffectReplaceKind::OpponentGainsLife {
                                current_opponent: opp,
                                remaining_opponents: remaining_opponents[idx + 1..].to_vec(),
                                amount,
                            },
                        });
                        return Ok(CostStepOutcome::Paused { prompt_id, chooser });
                    }
                    ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped
                            && let ReplaceableEventKind::LifeGain {
                                player: lp,
                                amount: final_amt,
                                ..
                            } = out.event
                            && final_amt > 0
                            && !state
                                .evaluate_board()
                                .player(lp)
                                .is_some_and(|ep| ep.cant_gain_life || ep.life_cant_change)
                        {
                            if let Some(p) = state.player_mut(lp) {
                                p.life += final_amt as i32;
                            }
                            state.mark_dirty(DirtyMask::LIFE);
                            emit_event(
                                state,
                                AtomicEventKind::LifeChange {
                                    player: lp,
                                    delta: final_amt as i32,
                                },
                            );
                        }
                    }
                }
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingEffectReplaceKind::LoyaltyPositive { counter: _ } => {
            if !outcome.mods.skipped
                && let ReplaceableEventKind::PutCounters {
                    object,
                    counter,
                    count,
                    ..
                } = outcome.event
                && count > 0
            {
                let prev = state.counter_count(object, counter);
                state.add_counter(object, counter, count);
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object,
                        counter,
                        previous_count: prev,
                        new_count: prev + count,
                        actor: Some(payer),
                    },
                );
            }
            if let Some(r) = state.rare_or_default(source) {
                r.used_loyalty_this_turn = true;
            }
            Ok(CostStepOutcome::Paid)
        }
        PendingEffectReplaceKind::PutCounters => {
            if !outcome.mods.skipped
                && let ReplaceableEventKind::PutCounters {
                    object,
                    counter: final_c,
                    count: final_n,
                    ..
                } = outcome.event
                && final_n > 0
            {
                let prev = state.counter_count(object, final_c);
                state.add_counter(object, final_c, final_n);
                emit_event(
                    state,
                    AtomicEventKind::CounterChanged {
                        object,
                        counter: final_c,
                        previous_count: prev,
                        new_count: prev + final_n,
                        actor: Some(payer),
                    },
                );
            }
            Ok(CostStepOutcome::Paid)
        }
    }
}

pub fn resume_cost_prompt(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<CostStepOutcome, EngineError> {
    let Some(pending) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction("no pending cost prompt".into()));
    };
    if pending.prompt_id != prompt_id || pending.chooser != actor {
        return Err(EngineError::IllegalAction("cost prompt mismatch".into()));
    }
    let PromptOwner::CostPayment(cp) = &pending.owner else {
        return Err(EngineError::IllegalAction(
            "pending prompt is not a cost payment prompt".into(),
        ));
    };
    let source = cp.source;
    let kind = cp.kind.clone();
    let resume_decision = cp.resume_decision.clone();
    let resume_priority = cp.resume_priority_player;

    match kind {
        CostPromptKind::ChooseNumber { min, max, .. } => {
            let ChoiceAnswer::PickNumber { value } = answer else {
                return Err(EngineError::IllegalAction("expected PickNumber".into()));
            };
            if value < min || value > max {
                return Err(EngineError::IllegalAction(
                    "chosen number out of bounds".into(),
                ));
            }
            state.take_pending_prompt();
            state.set_decision(resume_decision);
            state.set_priority_player(resume_priority);
            if let Some(r) = state.rare_or_default(source) {
                r.chosen_number = Some(value);
            }
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            Ok(CostStepOutcome::Paid)
        }
        _ => Err(EngineError::IllegalAction(
            "unsupported cost prompt kind".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{GameObject, ObjectFlags};
    use mtg_ir::{
        AbilityDef, CardDef, CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask,
        ColoredCount, CostDelta, CostDeltaMode, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle,
        KeywordAbility, KeywordDef, KickerCostOption, ManaCost, ObjectFilter, PlayerFilter,
        PlayerRelation, SpellAbility, SpellOrAbilityFilter, StaticAbility, StaticBody, StaticDef,
        SupertypeMask, Zone, ZoneMask,
    };
    use std::sync::Arc;

    fn make_spell(
        name: &str,
        cost_raw: &str,
        types: CardTypeMask,
        abilities: Vec<AbilityDef>,
    ) -> CardDef {
        CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "test".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse(cost_raw),
                    types,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
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

    fn make_static_perm(name: &str, body: StaticBody) -> CardDef {
        CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "test".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
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
                        body,
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
        }
    }

    fn boot(cards: Vec<CardDef>, tables: InternTables) -> GameState {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables,
            cards,
            decks: vec![],
        };
        let db = Arc::new(mtg_ir::CardDatabase::from_bundle(bundle).unwrap());
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 40],
            sideboard: vec![],
        };
        GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 23).unwrap()
    }

    fn normal_opt(from_zone: Zone) -> CastOption {
        CastOption {
            option_id: 0,
            face_idx: 0,
            from_zone,
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
        }
    }

    #[test]
    fn cost_modifiers_cr_118_7a_118_7b_and_trinisphere() {
        let spell = make_spell("Synth Spell", "{2}{U}{U}", CardTypeMask::SORCERY, vec![]);
        let reducer_3_min_2 = make_static_perm(
            "Synth ReducerFloor",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter::default()),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::Reduce,
                    generic: 3,
                    per: None,
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: Some(3),
                    max_reduction: None,
                    per_target_life: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );
        let trinisphere = make_static_perm(
            "Synth ThreeSphere",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter::default()),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::Increase,
                    generic: 0,
                    per: None,
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: Some(3),
                    max_reduction: None,
                    per_target_life: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );
        let bolt = make_spell("Synth Bolt", "{R}", CardTypeMask::INSTANT, vec![]);

        let mut state = boot(
            vec![spell, reducer_3_min_2, trinisphere, bolt],
            InternTables::default(),
        );
        let ts = state.allocate_timestamp();
        let sp_id = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(sp_id);
        let bolt_id = state.insert_object(
            GameObject::new(
                CardDefId(3),
                PlayerId(0),
                Zone::Hand,
                ts + 1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(bolt_id);

        // Place reducer on battlefield: {2}{U}{U} (MV=4 >= 3): generic reduced by 3 -> 0,
        // colored {U}{U} NEVER reduced by generic reduction (CR 118.7a), then min_total_mana=3 raises generic to {1}{U}{U}!
        let red_id = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts + 2,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(red_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let bd = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            sp_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(bd.locked_mana.generic, 1);
        assert_eq!(bd.locked_mana.pips.len(), 2);

        // Bolt {R} has pre-reduction MV=1 < 3 -> CR 118.7b min_total_mana=3 from Reduce does NOT raise it!
        let bd_bolt = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            bolt_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(bd_bolt.locked_mana.generic, 0);
        assert_eq!(bd_bolt.locked_mana.mana_value(), 1);

        // Now add Trinisphere (Increase min_total_mana=3) -> unconditionally raises Bolt {R} to {2}{R} (MV=3)
        let tri_id = state.insert_object(
            GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Battlefield,
                ts + 3,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(tri_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let bd_bolt_tri = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            bolt_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(bd_bolt_tri.locked_mana.generic, 2);
        assert_eq!(bd_bolt_tri.locked_mana.mana_value(), 3);
    }

    #[test]
    fn cost_locking_before_sacrifice_and_pay_life_bounds() {
        let mut tables = InternTables::default();
        tables.keywords.push("kicker".into());
        let kicker_kw = KeywordId(0);

        let sac_draw = make_spell(
            "Synth SacDraw",
            "{2}{U}",
            CardTypeMask::SORCERY,
            vec![
                AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Sacrifice {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: CostObjectTarget::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
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
                }),
                AbilityDef::Keyword(KeywordAbility {
                    keyword: KeywordDef {
                        keyword: kicker_kw,
                        cost: None,
                        param: None,
                        n: None,
                        cost_defs: vec![],
                        kicker_costs: vec![KickerCostOption {
                            cost: ManaCost::parse("{R}"),
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
                }),
            ],
        );

        let reducer = make_static_perm(
            "Synth Reducer",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter::default()),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::Reduce,
                    generic: 2,
                    per: None,
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );

        let mut state = boot(vec![sac_draw, reducer], tables);
        let ts = state.allocate_timestamp();
        let sp_id = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(sp_id);
        let red_id = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts + 1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(red_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // CR 601.2f: lock cost BEFORE sacrificing Synth Reducer (with kicker {R} -> {U}{R})
        let bd = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            sp_id,
            &normal_opt(Zone::Hand),
            None,
            &[0],
            &[],
        )
        .unwrap();
        assert_eq!(bd.locked_mana.generic, 0);
        assert_eq!(bd.locked_mana.pips.len(), 2); // {U} + {R}

        // Now execute sacrifice of Synth Reducer (CR 601.2h) -> locked cost remains {U}{R}
        let mut plan = PendingCostPaymentPlan {
            locked_mana_cost: bd.locked_mana.clone(),
            remaining_mana: bd.locked_mana.clone(),
            remaining_non_mana_costs: bd.non_mana_costs.clone(),
            non_mana_cursor: 0,
            pending_step: None,
            chosen_optional_costs: SmallVec::new(),
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
        let sac_res = execute_non_mana_cost_step(
            &mut state,
            PlayerId(0),
            sp_id,
            &bd.non_mana_costs[0],
            &[red_id],
            None,
            Some(&mut plan),
        )
        .unwrap();
        assert_eq!(sac_res, CostStepOutcome::Paid);
        assert_eq!(plan.sacrificed_lki.len(), 1);
        assert_eq!(plan.remaining_mana.mana_value(), 2);

        // PayLife > current life rejected (CR 119.4)
        let pay25 = CostDef::PayLife {
            amount: Box::new(mtg_ir::Amount::Const { n: 25 }),
            half_life_rounded_up: false,
        };
        assert!(!can_pay_non_mana_costs(
            &state,
            PlayerId(0),
            sp_id,
            std::slice::from_ref(&pay25),
            None
        ));
        assert!(
            execute_non_mana_cost_step(&mut state, PlayerId(0), sp_id, &pay25, &[], None, None)
                .is_err()
        );

        // SetZero + colored reduction test
        let _ = ColoredCount {
            color: ManaColor::U,
            count: 1,
        };
    }

    #[test]
    fn cost_aids_convoke_improvise_delve() {
        let creature = make_spell("Synth Elf", "{1}{G}", CardTypeMask::CREATURE, vec![]);
        let mut state = boot(vec![creature], InternTables::default());
        let ts = state.allocate_timestamp();
        let elf = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().battlefield.push(elf);
        let gy_card = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Graveyard,
                ts + 1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .graveyard
            .push(gy_card);
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::GRAVEYARDS);

        let mut plan = PendingCostPaymentPlan {
            locked_mana_cost: ManaCost::parse("{2}{G}"),
            remaining_mana: ManaCost::parse("{2}{G}"),
            remaining_non_mana_costs: vec![],
            non_mana_cursor: 0,
            pending_step: None,
            chosen_optional_costs: SmallVec::new(),
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

        apply_cost_aid(
            &mut state,
            PlayerId(0),
            &mut plan,
            CostAidKind::Convoke,
            elf,
        )
        .unwrap();
        assert_eq!(plan.remaining_mana.generic, 1);
        assert!(
            state
                .object(elf)
                .unwrap()
                .flags
                .contains(ObjectFlags::TAPPED)
        );

        apply_cost_aid(
            &mut state,
            PlayerId(0),
            &mut plan,
            CostAidKind::Delve,
            gy_card,
        )
        .unwrap();
        assert_eq!(plan.remaining_mana.generic, 0);
        assert_eq!(state.player(PlayerId(0)).unwrap().graveyard.len(), 0);
    }

    fn make_kw_def(kw: KeywordId, cost: Option<ManaCost>, cost_defs: Vec<CostDef>) -> KeywordDef {
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

    fn assert_mana_eq(actual: &ManaCost, expected: &str) {
        let exp = ManaCost::parse(expected);
        assert_eq!(
            (actual.generic, actual.x_count, &actual.pips),
            (exp.generic, exp.x_count, &exp.pips),
            "expected mana {expected}, got {actual:?}"
        );
    }

    #[test]
    fn cr_118_9d_set_zero_spree_escalate_non_double_count_and_morph_non_leak() {
        let mut tables = InternTables::default();
        tables.keywords.push("kicker".into()); // 0
        tables.keywords.push("escape".into()); // 1
        tables.keywords.push("morph".into()); // 2
        tables.keywords.push("spree".into()); // 3
        tables.keywords.push("escalate".into()); // 4

        let mut kicker_kw = make_kw_def(KeywordId(0), None, vec![]);
        kicker_kw.kicker_costs.push(KickerCostOption {
            cost: ManaCost::parse("{R}"),
            cost_defs: vec![],
            tags: vec!["kicked".into()],
        });

        let kicked_spell = make_spell(
            "Synth KickedSpell",
            "{3}{U}{U}",
            CardTypeMask::SORCERY,
            vec![
                AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![CostDef::Mana {
                        cost: ManaCost::parse("{1}"),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                    targets: vec![],
                    effect: vec![],
                    modal: None,
                    cast_restrictions: vec![],
                    cast_condition: None,
                    x_cant_be_zero: false,
                    span: Default::default(),
                }),
                AbilityDef::Keyword(KeywordAbility {
                    keyword: kicker_kw,
                    id: None,
                    span: Default::default(),
                }),
            ],
        );
        let static_tax = make_static_perm(
            "Synth Taxer",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter::default()),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::Increase,
                    generic: 1,
                    per: None,
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );
        let escape_exile = CostDef::ExileFrom {
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
        };
        let escape_spell = make_spell(
            "Synth Escaper",
            "{1}{B}",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: make_kw_def(
                    KeywordId(1),
                    Some(ManaCost::parse("{3}{B}{B}")),
                    vec![escape_exile.clone()],
                ),
                id: None,
                span: Default::default(),
            })],
        );
        let morph_creature = make_spell(
            "Synth Morpher",
            "{2}{U}",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: make_kw_def(
                    KeywordId(2),
                    Some(ManaCost::parse("{4}{U}")),
                    vec![CostDef::RevealSelfFromHand],
                ),
                id: None,
                span: Default::default(),
            })],
        );
        let spree_spell = make_spell(
            "Synth Spree",
            "{U}",
            CardTypeMask::INSTANT,
            vec![
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
                                cost: ManaCost::parse("{W}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            }],
                            vec![
                                CostDef::Mana {
                                    cost: ManaCost::parse("{1}{U}"),
                                    x_spend_colors: ColorMask::empty(),
                                    x_colored_only: false,
                                    x_max_one_per_color: false,
                                },
                                CostDef::PayLife {
                                    amount: Box::new(mtg_ir::Amount::Const { n: 2 }),
                                    half_life_rounded_up: false,
                                },
                            ],
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
                AbilityDef::Keyword(KeywordAbility {
                    keyword: make_kw_def(KeywordId(3), None, vec![]),
                    id: None,
                    span: Default::default(),
                }),
            ],
        );
        let escalate_spell = make_spell(
            "Synth Escalate",
            "{1}{W}",
            CardTypeMask::SORCERY,
            vec![
                AbilityDef::Spell(SpellAbility {
                    id: None,
                    additional_costs: vec![],
                    targets: vec![],
                    effect: vec![],
                    modal: Some(mtg_ir::ModalDef {
                        min: 1,
                        max: 3,
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
                            mtg_ir::ModalMode {
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
                AbilityDef::Keyword(KeywordAbility {
                    keyword: make_kw_def(KeywordId(4), Some(ManaCost::parse("{1}")), vec![]),
                    id: None,
                    span: Default::default(),
                }),
            ],
        );
        let set_zero_perm = make_static_perm(
            "Synth FreeCast",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::SORCERY,
                        ..Default::default()
                    }),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::SetZero,
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
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );

        let mut state = boot(
            vec![
                kicked_spell,
                static_tax,
                escape_spell,
                morph_creature,
                spree_spell,
                escalate_spell,
                set_zero_perm,
            ],
            tables,
        );
        let mut insert_in = |def: CardDefId, zone: Zone| {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(def, PlayerId(0), zone, ts, ObjectFlags::empty()),
                None,
            );
            let p = state.player_mut(PlayerId(0)).unwrap();
            match zone {
                Zone::Hand => p.hand.push(id),
                Zone::Battlefield => p.battlefield.push(id),
                Zone::Graveyard => p.graveyard.push(id),
                _ => {}
            }
            id
        };

        let kicked_id = insert_in(CardDefId(0), Zone::Hand);
        let _tax_id = insert_in(CardDefId(1), Zone::Battlefield);
        let escape_id = insert_in(CardDefId(2), Zone::Graveyard);
        let morph_id = insert_in(CardDefId(3), Zone::Hand);
        let spree_id = insert_in(CardDefId(4), Zone::Hand);
        let escalate_id = insert_in(CardDefId(5), Zone::Hand);
        let _free_id = insert_in(CardDefId(6), Zone::Battlefield);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // 1. CR 118.9d: SetZero zeroes base {3}{U}{U}, but preserves SpellAbility additional {1},
        //    Kicker {R}, and StaticBody::CostModifier {1} -> {2}{R}.
        let bd_kicked = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            kicked_id,
            &normal_opt(Zone::Hand),
            None,
            &[0],
            &[],
        )
        .unwrap();
        assert_mana_eq(&bd_kicked.locked_mana, "{2}{R}");

        // 2. Escape with option.extra_costs populated does NOT double-count kw.keyword.cost_defs;
        //    also verify additional {1} from StaticBody::CostModifier is preserved.
        let mut esc_opt = normal_opt(Zone::Graveyard);
        esc_opt.alt_kind = AltCastKind::Escape;
        esc_opt.extra_costs = vec![escape_exile];
        let bd_esc =
            compute_effective_spell_cost(&state, PlayerId(0), escape_id, &esc_opt, None, &[], &[])
                .unwrap();
        assert_eq!(bd_esc.non_mana_costs.len(), 1);
        assert_mana_eq(&bd_esc.locked_mana, "{4}{B}{B}");

        // 3. Morph face-down does NOT leak turn-up cost {4}{U} or turn-up cost_defs!
        let mut morph_opt = normal_opt(Zone::Hand);
        morph_opt.alt_kind = AltCastKind::MorphFaceDown;
        let bd_morph =
            compute_effective_spell_cost(&state, PlayerId(0), morph_id, &morph_opt, None, &[], &[])
                .unwrap();
        assert_mana_eq(&bd_morph.locked_mana, "{4}"); // {3} base + {1} tax
        assert!(bd_morph.non_mana_costs.is_empty());

        // 4. Spree per_mode_costs adds chosen modes' mana + non-mana costs.
        let mut spree_opt = normal_opt(Zone::Hand);
        spree_opt.alt_kind = AltCastKind::Spree;
        let bd_spree = compute_effective_spell_cost_with_modes(
            &state,
            PlayerId(0),
            spree_id,
            &spree_opt,
            &[0, 1],
            None,
            &[],
            &[],
        )
        .unwrap();
        // {U} base + {W} mode 0 + {1}{U} mode 1 + {1} tax = {2}{U}{W}{U}
        assert_eq!(bd_spree.locked_mana.generic, 2);
        assert_eq!(bd_spree.locked_mana.pips.len(), 3);
        assert_eq!(bd_spree.non_mana_costs.len(), 1);
        assert!(matches!(
            bd_spree.non_mana_costs[0],
            CostDef::PayLife { .. }
        ));

        // 5. Escalate with 3 chosen modes adds (3 - 1) * {1} = {2}; SetZero zeroes {1}{W} base,
        //    preserving {2} escalate + {1} tax = {3}.
        let bd_esc_modes = compute_effective_spell_cost_with_modes(
            &state,
            PlayerId(0),
            escalate_id,
            &normal_opt(Zone::Hand),
            &[0, 1, 2],
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_mana_eq(&bd_esc_modes.locked_mana, "{3}");
    }

    #[test]
    fn per_target_life_emits_pay_life_without_mutating_mana_factor() {
        let spell = make_spell("Synth TargetSpell", "{3}{B}", CardTypeMask::INSTANT, vec![]);
        let perm = make_static_perm(
            "Synth LifeWard",
            StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::Spell {
                    filter: Box::new(ObjectFilter::default()),
                    alt_cost_keyword: None,
                },
                delta: CostDelta {
                    mode: CostDeltaMode::Reduce,
                    generic: 1,
                    per: None,
                    additional_costs: vec![],
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: Some(2),
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
        );
        let mut state = boot(vec![spell, perm], InternTables::default());
        let ts = state.allocate_timestamp();
        let sp_id = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(sp_id);
        let perm_id = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts + 1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(perm_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let targets = [
            TargetRef::Player { id: PlayerId(0) },
            TargetRef::Player { id: PlayerId(1) },
            TargetRef::Object { id: perm_id },
        ];
        let bd = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            sp_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &targets,
        )
        .unwrap();

        // Generic reduction stays 1 (NOT 3), and PayLife { 2 * 3 = 6 } is emitted!
        assert_mana_eq(&bd.locked_mana, "{2}{B}");
        assert_eq!(bd.non_mana_costs.len(), 1);
        match &bd.non_mana_costs[0] {
            CostDef::PayLife {
                amount,
                half_life_rounded_up,
            } => {
                assert!(!half_life_rounded_up);
                assert_eq!(**amount, mtg_ir::Amount::Const { n: 6 });
            }
            other => panic!("expected PayLife, got {other:?}"),
        }
    }

    #[test]
    fn multi_object_sacrifice_pause_resumes_and_unsupported_prompt_errors() {
        let make_repl_perm = |name: &str, dest: Zone| {
            let mut c = make_spell(name, "", CardTypeMask::ARTIFACT, vec![]);
            c.faces[0]
                .abilities
                .push(AbilityDef::Replacement(mtg_ir::ReplacementAbility {
                    replacement: mtg_ir::ReplacementDef {
                        replaces: mtg_ir::EventPattern {
                            event: mtg_ir::EventKind::ZoneChange,
                            filter: mtg_ir::EventFilter {
                                detail: Some(mtg_ir::EventDetail::ZoneChange {
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
                        with: mtg_ir::ReplacementWith::ZoneChange {
                            mod_spec: Box::new(mtg_ir::ZoneChangeMod {
                                to: Some(dest),
                                ..Default::default()
                            }),
                        },
                        self_replacement: false,
                        who: None,
                        condition: None,
                    },
                    span: Default::default(),
                }));
            c
        };

        let fodder = make_spell("Synth Fodder", "", CardTypeMask::CREATURE, vec![]);
        let repl_a = make_repl_perm("Synth ReplExileA", Zone::Exile);
        let repl_b = make_repl_perm("Synth ReplHandB", Zone::Hand);

        let mut state = boot(vec![fodder, repl_a, repl_b], InternTables::default());
        let mut add_bf = |def: CardDefId| {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(
                    def,
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            id
        };
        let fod1 = add_bf(CardDefId(0));
        let fod2 = add_bf(CardDefId(0));
        let _r1 = add_bf(CardDefId(1));
        let _r2 = add_bf(CardDefId(2));
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let sac_two = CostDef::Sacrifice {
            count: Box::new(mtg_ir::Amount::Const { n: 2 }),
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
        };
        let mut plan = PendingCostPaymentPlan {
            locked_mana_cost: ManaCost::default(),
            remaining_mana: ManaCost::default(),
            remaining_non_mana_costs: vec![sac_two.clone()],
            non_mana_cursor: 0,
            pending_step: None,
            chosen_optional_costs: SmallVec::new(),
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

        // Pausing on item 0 records PendingNonManaCostStep::ZoneMove { Sacrifice, fod1, [fod2] }
        let out1 = execute_non_mana_cost_step(
            &mut state,
            PlayerId(0),
            fod1,
            &sac_two,
            &[fod1, fod2],
            None,
            Some(&mut plan),
        )
        .unwrap();
        let CostStepOutcome::Paused {
            prompt_id: pid1,
            chooser: ch1,
        } = out1
        else {
            panic!("expected pause on first sacrifice");
        };
        assert_eq!(
            plan.pending_step,
            Some(PendingNonManaCostStep::ZoneMove {
                kind: PendingZoneCostKind::Sacrifice,
                current_object: fod1,
                remaining_objects: vec![fod2],
            })
        );

        let crate::replace::ReplacementResumeOutcome::ZoneMove(mv1) =
            crate::replace::resume_and_finalize_replacement(
                &mut state,
                ch1,
                pid1,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
        else {
            panic!("expected ZoneMove outcome for fod1");
        };

        // Resuming item 0 records fod1 LKI and immediately pauses on item 1 (fod2)!
        let out2 =
            resume_paused_non_mana_cost_zone_step(&mut state, PlayerId(0), fod1, &mut plan, mv1)
                .unwrap();
        let CostStepOutcome::Paused {
            prompt_id: pid2,
            chooser: ch2,
        } = out2
        else {
            panic!("expected pause on second sacrifice");
        };
        assert_eq!(plan.sacrificed_lki.len(), 1);
        assert_eq!(
            plan.pending_step,
            Some(PendingNonManaCostStep::ZoneMove {
                kind: PendingZoneCostKind::Sacrifice,
                current_object: fod2,
                remaining_objects: vec![],
            })
        );

        let crate::replace::ReplacementResumeOutcome::ZoneMove(mv2) =
            crate::replace::resume_and_finalize_replacement(
                &mut state,
                ch2,
                pid2,
                ChoiceAnswer::PickReplacement { index: 1 },
            )
            .unwrap()
        else {
            panic!("expected ZoneMove outcome for fod2");
        };

        let out3 =
            resume_paused_non_mana_cost_zone_step(&mut state, PlayerId(0), fod1, &mut plan, mv2)
                .unwrap();
        assert_eq!(out3, CostStepOutcome::Paid);
        assert_eq!(plan.sacrificed_lki.len(), 2);
        assert!(plan.pending_step.is_none());

        // Harden resume_cost_prompt: unsupported CostPromptKind returns Err and preserves prompt
        let bad_pid = state.allocate_prompt_id();
        state.set_pending_prompt(Some(PendingPromptContext {
            prompt_id: bad_pid,
            chooser: PlayerId(0),
            request: ChoiceRequest::YesNo {
                prompt_id: bad_pid,
                label: "unused".into(),
            },
            owner: PromptOwner::CostPayment(CostPaymentPromptState {
                controller: PlayerId(0),
                source: fod1,
                kind: CostPromptKind::OptionalPay {
                    cost_index: 0,
                    label: "unused".into(),
                },
                resume_decision: DecisionState::Priority,
                resume_priority_player: PlayerId(0),
            }),
        }));
        assert!(
            resume_cost_prompt(
                &mut state,
                PlayerId(0),
                bad_pid,
                ChoiceAnswer::YesNo { value: true }
            )
            .is_err()
        );
        assert!(state.pending_prompt().is_some());
    }

    #[test]
    fn cached_spell_cost_and_reveal_from_hand_parity_with_synth_nexus_64bf() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Dragon".into());
        let dragon_sub = mtg_ir::SubtypeId(0);

        let synth_nexus = make_static_perm(
            "Synth Nexus",
            StaticBody::OffBattlefieldCharacteristics {
                zones: ZoneMask::HAND,
                filter: None,
                all_players: false,
                add_all_creature_types: true,
                add_chosen_subtype: false,
                add_types: CardTypeMask::empty(),
                add_subtypes: SmallVec::new(),
                add_chosen_color: false,
                add_colors: ColorMask::empty(),
            },
        );
        let filler_perm = make_spell("Filler", "", CardTypeMask::ARTIFACT, vec![]);
        let bear = make_spell("Vanilla Bear", "{1}{G}", CardTypeMask::CREATURE, vec![]);
        let curiosity_spell = make_spell(
            "Draconic Curiosity",
            "{3}{R}",
            CardTypeMask::SORCERY,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::AdditionalCastCost {
                        costs: vec![
                            CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            },
                            CostDef::RevealFromHand {
                                count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                                target: Some(CostObjectTarget::Filter {
                                    filter: Box::new(ObjectFilter {
                                        subtypes_any: SmallVec::from_slice(&[dragon_sub]),
                                        ..Default::default()
                                    }),
                                }),
                                share_color: false,
                            },
                        ],
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let mut state = boot(
            vec![synth_nexus, filler_perm, bear, curiosity_spell],
            tables,
        );
        let ts0 = state.allocate_timestamp();
        let mw_id = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts0,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(mw_id);
        for _ in 0..63 {
            let ts = state.allocate_timestamp();
            let fid = state.insert_object(
                GameObject::new(
                    CardDefId(1),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(fid);
        }
        for _ in 0..63 {
            let ts = state.allocate_timestamp();
            let fid = state.insert_object(
                GameObject::new(
                    CardDefId(1),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(fid);
        }
        let ts_bear = state.allocate_timestamp();
        let bear_id = state.insert_object(
            GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Hand,
                ts_bear,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(bear_id);
        let ts_sp = state.allocate_timestamp();
        let spell_id = state.insert_object(
            GameObject::new(
                CardDefId(3),
                PlayerId(0),
                Zone::Hand,
                ts_sp,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(spell_id);
        state.mark_dirty(crate::state::DirtyMask::TOPOLOGY);

        let opt = normal_opt(Zone::Hand);
        let uncached_bd =
            compute_effective_spell_cost(&state, PlayerId(0), spell_id, &opt, None, &[], &[])
                .unwrap();
        let cache = state.evaluate_board();
        let cached_bd = compute_effective_spell_cost_cached(
            &state,
            PlayerId(0),
            spell_id,
            &opt,
            None,
            &[],
            &[],
            cache,
        )
        .unwrap();

        assert_eq!(cached_bd.locked_mana.generic, 4);
        assert_eq!(cached_bd.locked_mana.mana_value(), 5);
        assert_eq!(cached_bd.non_mana_costs.len(), 1);
        assert_eq!(cached_bd.locked_mana, uncached_bd.locked_mana);
        assert_eq!(cached_bd.non_mana_costs, uncached_bd.non_mana_costs);

        let uncached_can_pay = can_pay_non_mana_costs(
            &state,
            PlayerId(0),
            spell_id,
            &cached_bd.non_mana_costs,
            None,
        );
        let cached_can_pay = can_pay_non_mana_costs_cached(
            &state,
            PlayerId(0),
            spell_id,
            &cached_bd.non_mana_costs,
            None,
            cache,
        );
        assert!(cached_can_pay);
        assert_eq!(cached_can_pay, uncached_can_pay);

        let uncached_cands = non_mana_cost_candidates(
            &state,
            PlayerId(0),
            spell_id,
            &cached_bd.non_mana_costs[0],
            None,
        );
        let cached_cands = non_mana_cost_candidates_cached(
            &state,
            PlayerId(0),
            spell_id,
            &cached_bd.non_mana_costs[0],
            None,
            cache,
        );
        assert_eq!(cached_cands, uncached_cands);
        assert_eq!(
            cached_cands.as_ref().map(|c| c.1.as_slice()),
            Some(&[bear_id][..])
        );
    }

    #[test]
    fn bf_permanent_additional_cast_cost_does_not_leak_to_other_spells() {
        let bear = make_spell("Synth Bear", "{1}{G}", CardTypeMask::CREATURE, vec![]);
        let self_exile_creature = make_spell(
            "Synth SelfExile Creature",
            "{3}{U}",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::AdditionalCastCost {
                        costs: vec![CostDef::ExileFrom {
                            zone: Zone::Graveyard,
                            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                            target: Some(CostObjectTarget::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
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
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let self_mana_artifact = make_spell(
            "Synth SelfMana Artifact",
            "{2}",
            CardTypeMask::ARTIFACT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::AdditionalCastCost {
                        costs: vec![CostDef::Mana {
                            cost: ManaCost::parse("{1}"),
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                        }],
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        let mut state = boot(
            vec![bear, self_exile_creature, self_mana_artifact],
            InternTables::default(),
        );
        let mut put_in = |def: CardDefId, zone: Zone| {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(def, PlayerId(0), zone, ts, ObjectFlags::empty()),
                None,
            );
            let p = state.player_mut(PlayerId(0)).unwrap();
            match zone {
                Zone::Battlefield => p.battlefield.push(id),
                Zone::Hand => p.hand.push(id),
                _ => {}
            }
            id
        };

        let _bf_exile = put_in(CardDefId(1), Zone::Battlefield);
        let _bf_mana = put_in(CardDefId(2), Zone::Battlefield);
        let bear_id = put_in(CardDefId(0), Zone::Hand);
        let self_exile_hand_id = put_in(CardDefId(1), Zone::Hand);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // Graveyard is empty; {1}{G} in pool ([W, U, B, R, G, C]).
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 1, 1];

        let bd_bear = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            bear_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(bd_bear.locked_mana.generic, 1);
        assert_eq!(bd_bear.locked_mana.mana_value(), 2);
        assert_mana_eq(&bd_bear.locked_mana, "{1}{G}");
        assert!(bd_bear.non_mana_costs.is_empty());
        assert!(crate::legality::can_cast_spell_option(&state, PlayerId(0), bear_id, 0).is_some());

        // Control: self-spell AdditionalCastCost on hand copy still requires exiling a creature from GY.
        let bd_self = compute_effective_spell_cost(
            &state,
            PlayerId(0),
            self_exile_hand_id,
            &normal_opt(Zone::Hand),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(bd_self.non_mana_costs.len(), 1);
        assert_mana_eq(&bd_self.locked_mana, "{3}{U}");

        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 1, 0, 0, 0, 3];
        assert!(
            crate::legality::can_cast_spell_option(&state, PlayerId(0), self_exile_hand_id, 0)
                .is_none()
        );
    }

    #[test]
    fn synth_choice_mana_branch_pool_check_and_eaten_legality() {
        let eaten = make_spell(
            "Synth Eaten",
            "{B}",
            CardTypeMask::SORCERY,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::AdditionalCastCost {
                        costs: vec![CostDef::Choice {
                            options: vec![
                                vec![CostDef::Sacrifice {
                                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
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
                                vec![CostDef::Mana {
                                    cost: ManaCost::parse("{3}{B}"),
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
        );
        let swamp = CardDef {
            oracle_id: "synth_cost_swamp".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Cost Swamp".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Cost Swamp".into(),
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
                    id: "mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![mtg_ir::EffectStep {
                        op: mtg_ir::EffectOp::AddMana {
                            production: mtg_ir::ManaProduction::Fixed {
                                symbols: smallvec::smallvec![ManaColor::B],
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
                    timing: mtg_ir::Timing::Mana,
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

        let mut state = boot(vec![eaten, swamp], InternTables::default());
        let spell_id = {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(id);
            id
        };
        for _ in 0..5 {
            let ts = state.allocate_timestamp();
            let lid = state.insert_object(
                GameObject::new(
                    CardDefId(1),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(lid);
        }
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // 1. Empty mana pool + 5 untapped Swamps: can_cast_spell_option MUST still succeed for Synth Eaten!
        assert!(crate::legality::can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_some());

        // 2. Choice([[Mana("{B}")], [Mana("{3}")]]) with {3} in pool selects second branch cleanly!
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 0, 3];
        let choice_cost = CostDef::Choice {
            options: vec![
                vec![CostDef::Mana {
                    cost: ManaCost::parse("{B}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
                vec![CostDef::Mana {
                    cost: ManaCost::parse("{3}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
            ],
            forage: false,
        };
        assert_eq!(
            execute_non_mana_cost_step(
                &mut state,
                PlayerId(0),
                spell_id,
                &choice_cost,
                &[],
                None,
                None,
            )
            .unwrap(),
            CostStepOutcome::Paid
        );
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
    }

    #[test]
    fn synth_exile_from_cost_marks_dirty_exile_for_verify_layer_cache() {
        let mut tables = InternTables::default();
        tables.keywords.push("flying".into());
        let flying_kw = KeywordId(0);

        let watcher = make_spell(
            "Synth Exile Watcher",
            "{2}",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: mtg_ir::Layer::Layer6,
                        affected: mtg_ir::Affected::SelfObj,
                        modifier: mtg_ir::LayerMod::GrantAbilitiesFromObjects {
                            from: mtg_ir::AbilityGrantSource::ExiledWithSelf {
                                filter: Box::new(ObjectFilter::default()),
                            },
                            mana_as_any_color: false,
                            once_each_turn: false,
                            loyalty_only: false,
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let flyer = make_spell(
            "Synth Flyer",
            "{1}",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: flying_kw,
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
        );

        let mut state = boot(vec![watcher, flyer], tables);
        let watcher_bf = {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(
                    CardDefId(0),
                    PlayerId(0),
                    Zone::Battlefield,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().battlefield.push(id);
            id
        };
        let flyer_gy = {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(
                    CardDefId(1),
                    PlayerId(0),
                    Zone::Graveyard,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().graveyard.push(id);
            id
        };
        state.mark_dirty(DirtyMask::TOPOLOGY);
        assert!(
            !state
                .evaluate_board()
                .get(watcher_bf)
                .unwrap()
                .keywords
                .contains(&flying_kw)
        );

        let exile_cost = CostDef::ExileFrom {
            zone: Zone::Graveyard,
            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
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
        };
        assert_eq!(
            execute_non_mana_cost_step(
                &mut state,
                PlayerId(0),
                watcher_bf,
                &exile_cost,
                &[flyer_gy],
                None,
                None,
            )
            .unwrap(),
            CostStepOutcome::Paid
        );
        assert!(
            state
                .evaluate_board()
                .get(watcher_bf)
                .unwrap()
                .keywords
                .contains(&flying_kw)
        );
    }

    #[test]
    fn synth_x_cost_inspection_and_bounds() {
        use mtg_ir::{ActivatedAbility, Amount, RoundMode, Timing};

        let mut tables = InternTables::default();
        tables.counters.push("+1/+1".into());
        let p1_cid = CounterId(0);

        let fiend = CardDef {
            oracle_id: "o_synth_gy_x_fiend".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Graveyard X Fiend".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Graveyard X Fiend".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{4}{B}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::B,
                    color_indicator: None,
                    pt: Some(mtg_ir::PrintedPt::Fixed {
                        power: 4,
                        toughness: 5,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "shrink".into(),
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
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let two_drop = make_spell("Synth TwoDrop", "{2}", CardTypeMask::CREATURE, vec![]);
        let three_drop = make_spell("Synth ThreeDrop", "{3}", CardTypeMask::CREATURE, vec![]);

        let mut state = boot(vec![fiend, two_drop, three_drop], tables);
        let ts1 = state.allocate_timestamp();
        let fiend_bf = state.insert_object(
            GameObject::new(
                CardDefId(0),
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
            .push(fiend_bf);

        let ts2 = state.allocate_timestamp();
        let gy_two = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Graveyard,
                ts2,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .graveyard
            .push(gy_two);

        let ts3 = state.allocate_timestamp();
        let three_bf = state.insert_object(
            GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Battlefield,
                ts3,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(three_bf);
        state.add_counter(three_bf, p1_cid, 5);

        state.player_mut(PlayerId(0)).unwrap().hand.clear();
        for _ in 0..3 {
            let ts = state.allocate_timestamp();
            let h = state.insert_object(
                GameObject::new(
                    CardDefId(1),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(h);
        }
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let cache = state.evaluate_board();

        // 1. amount_uses_x & cost_uses_x across combinators
        let double_x = Amount::Mul {
            factor: 2,
            of: Box::new(Amount::X),
        };
        let half_x = Amount::Half {
            of: Box::new(Amount::X),
            round: RoundMode::Down,
        };
        let sum_x = Amount::Sum {
            args: vec![Amount::X, Amount::Const { n: 1 }],
        };
        assert!(amount_uses_x(&double_x));
        assert!(amount_uses_x(&half_x));
        assert!(amount_uses_x(&sum_x));
        assert!(!amount_uses_x(&Amount::Const { n: 3 }));

        let gy_x_cost = CostDef::ExileFrom {
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
        };
        assert!(cost_uses_x(&gy_x_cost));

        // 2. max_x_for_cost_list_cached iterates ALL costs (Mana + ExileFrom(GY, X) -> 1)
        let AbilityDef::Activated(act) =
            &state.db().get(CardDefId(0)).unwrap().faces[0].abilities[0]
        else {
            panic!("expected activated ability");
        };
        assert_eq!(
            max_x_for_cost_list_cached(&state, PlayerId(0), fiend_bf, &act.cost, cache),
            1
        );

        // 3. DoubleX Discard with 3 cards in hand -> max_x == 1
        let double_discard = [CostDef::Discard {
            count: Box::new(double_x),
            target: None,
            random: false,
            same_name_pair: false,
            same_name_as_controlled: None,
            different_names: false,
        }];
        assert_eq!(
            max_x_for_cost_list_cached(&state, PlayerId(0), fiend_bf, &double_discard, cache),
            1
        );

        // 4. RemoveCounters (!among) with 1 creature having 5 counters, need 3 -> (1, 1) candidates
        let rem_3 = CostDef::RemoveCounters {
            counter: CounterTarget::Specific(p1_cid),
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
        };
        let (kind, cands, min_c, max_c) =
            non_mana_cost_candidates_cached(&state, PlayerId(0), fiend_bf, &rem_3, None, cache)
                .unwrap();
        assert_eq!(kind, CostSelectionKind::RemoveCounters);
        assert_eq!(cands, vec![three_bf]);
        assert_eq!((min_c, max_c), (1, 1));
        assert!(can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &rem_3,
            None,
            cache
        ));

        // Need 6 from single object (!among) fails even if another object had 2 counters
        state.add_counter(fiend_bf, p1_cid, 2);
        let cache = state.evaluate_board();
        let mk_rem = |count_n: i32, among: bool| CostDef::RemoveCounters {
            counter: CounterTarget::Specific(p1_cid),
            count: Box::new(Amount::Const { n: count_n }),
            remove_all: false,
            from: CostObjectTarget::Filter {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
            },
            among,
            one_or_more: false,
            or_suspended_you_own: false,
        };
        let rem_6 = mk_rem(6, false);
        assert!(!can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &rem_6,
            None,
            cache
        ));
        // Among: true with 5 + 2 = 7 counters and need 6 -> min 2 objects, max 2
        let rem_6_among = mk_rem(6, true);
        assert!(can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &rem_6_among,
            None,
            cache
        ));
        let (_, among_cands, among_min, among_max) = non_mana_cost_candidates_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &rem_6_among,
            None,
            cache,
        )
        .unwrap();
        assert_eq!(among_cands.len(), 2);
        assert_eq!((among_min, among_max), (2, 2));

        // 5. ExileFrom(Battlefield, target: None) returns battlefield objects in candidates
        let exile_bf = CostDef::ExileFrom {
            zone: Zone::Battlefield,
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
        };
        let (_, bf_cands, bf_min, bf_max) =
            non_mana_cost_candidates_cached(&state, PlayerId(0), fiend_bf, &exile_bf, None, cache)
                .unwrap();
        assert_eq!(bf_cands.len(), 2);
        assert_eq!((bf_min, bf_max), (1, 1));

        // 6. total_mana_value_equals_x subset-sum: GY has [MV=2], X=2 passes, X=1/3 fails
        let mv_eq_x = CostDef::ExileFrom {
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
        };
        assert!(can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &mv_eq_x,
            Some(2),
            cache
        ));
        assert!(!can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &mv_eq_x,
            Some(1),
            cache
        ));
        assert!(!can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            fiend_bf,
            &mv_eq_x,
            Some(3),
            cache
        ));
    }

    #[test]
    fn synth_convoke_delve_and_choice_joint_cost_aids() {
        let make_creature = |name: &str, colors: ColorMask| CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types: CardTypeMask::CREATURE,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors,
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

        let red_cre = make_creature("Synth Red Token", ColorMask::R);
        let green_cre = make_creature("Synth Green Token", ColorMask::G);
        let white_cre = make_creature("Synth White Token", ColorMask::W);

        let mut state = boot(vec![red_cre, green_cre, white_cre], InternTables::default());
        fn spawn(state: &mut GameState, def: CardDefId, zone: Zone) -> ObjectId {
            let ts = state.allocate_timestamp();
            let id = state.insert_object(
                GameObject::new(def, PlayerId(0), zone, ts, ObjectFlags::empty()),
                None,
            );
            let p = state.player_mut(PlayerId(0)).unwrap();
            match zone {
                Zone::Battlefield => p.battlefield.push(id),
                Zone::Graveyard => p.graveyard.push(id),
                _ => {}
            }
            id
        }

        let r1 = spawn(&mut state, CardDefId(0), Zone::Battlefield);
        let _r2 = spawn(&mut state, CardDefId(0), Zone::Battlefield);
        let g1 = spawn(&mut state, CardDefId(1), Zone::Battlefield);
        let w1 = spawn(&mut state, CardDefId(2), Zone::Battlefield);
        let gy1 = spawn(&mut state, CardDefId(0), Zone::Graveyard);
        let gy2 = spawn(&mut state, CardDefId(0), Zone::Graveyard);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        let cache = state.evaluate_board();

        // 1. convoke_can_reduce_mana & tight Convoke estimation
        let gw = ManaCost::parse("{G}{W}");
        assert!(!convoke_can_reduce_mana(&gw, ColorMask::R));
        assert!(convoke_can_reduce_mana(&gw, ColorMask::G));
        let one_gw = ManaCost::parse("{1}{G}{W}");
        assert!(convoke_can_reduce_mana(&one_gw, ColorMask::R));

        let spend_ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let convoke_aids = CostAidCaps {
            allows_convoke: true,
            ..Default::default()
        };
        // 2 Red + 1 Green + 1 White -> can pay {2}{G}{W} via Convoke, CANNOT pay {3}{G}{W}
        assert!(can_pay_costs_and_mana_cached(
            &state,
            PlayerId(0),
            NONE_OBJECT_ID,
            &ManaCost::parse("{2}{G}{W}"),
            &[],
            &spend_ctx,
            convoke_aids,
            None,
            cache,
        ));
        assert!(!can_pay_costs_and_mana_cached(
            &state,
            PlayerId(0),
            NONE_OBJECT_ID,
            &ManaCost::parse("{3}{G}{W}"),
            &[],
            &spend_ctx,
            convoke_aids,
            None,
            cache,
        ));

        // 2. Delve GY reservation against pending ExileFrom(Graveyard, 2)
        let exile_2_gy = [CostDef::ExileFrom {
            zone: Zone::Graveyard,
            count: Box::new(mtg_ir::Amount::Const { n: 2 }),
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
        }];
        assert_eq!(
            delve_reserved_gy_cards_cached(&state, PlayerId(0), &exile_2_gy, cache),
            2
        );
        let mk_plan = |mana: ManaCost, non_mana: Vec<CostDef>| PendingCostPaymentPlan {
            locked_mana_cost: mana.clone(),
            remaining_mana: mana,
            remaining_non_mana_costs: non_mana,
            non_mana_cursor: 0,
            pending_step: None,
            chosen_optional_costs: SmallVec::new(),
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
        let mut plan = mk_plan(ManaCost::parse("{1}"), exile_2_gy.to_vec());
        // With 2 cards in GY and 2 reserved, Delve must reject!
        assert!(
            apply_cost_aid(&mut state, PlayerId(0), &mut plan, CostAidKind::Delve, gy1).is_err()
        );
        // With 3rd card in GY, 1st Delve succeeds and 2nd rejects!
        let gy3 = spawn(&mut state, CardDefId(0), Zone::Graveyard);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        plan.remaining_mana = ManaCost::parse("{2}");
        assert!(
            apply_cost_aid(&mut state, PlayerId(0), &mut plan, CostAidKind::Delve, gy3).is_ok()
        );
        assert!(
            apply_cost_aid(&mut state, PlayerId(0), &mut plan, CostAidKind::Delve, gy2).is_err()
        );

        // 3. Convoke apply_cost_aid rejects off-color creature when generic == 0, accepts matching
        let mut convoke_plan = mk_plan(ManaCost::parse("{G}{W}"), Vec::new());
        assert!(
            apply_cost_aid(
                &mut state,
                PlayerId(0),
                &mut convoke_plan,
                CostAidKind::Convoke,
                r1,
            )
            .is_err()
        );
        assert!(
            apply_cost_aid(
                &mut state,
                PlayerId(0),
                &mut convoke_plan,
                CostAidKind::Convoke,
                g1,
            )
            .is_ok()
        );
        assert!(
            apply_cost_aid(
                &mut state,
                PlayerId(0),
                &mut convoke_plan,
                CostAidKind::Convoke,
                w1,
            )
            .is_ok()
        );
        assert!(
            convoke_plan.remaining_mana.generic == 0
                && convoke_plan.remaining_mana.x_count == 0
                && convoke_plan.remaining_mana.pips.is_empty()
        );

        // 4. Joint Choice([[TapSelf], [Mana({5})]]) with 0 mana preserves TapSelf constraint
        let cache = state.evaluate_board();
        let choice_tap_or_mana = [CostDef::Choice {
            options: vec![
                vec![CostDef::TapSelf],
                vec![CostDef::Mana {
                    cost: ManaCost::parse("{5}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }],
            ],
            forage: false,
        }];
        let constraints = auto_tap_constraints_for_costs_cached(
            &state,
            PlayerId(0),
            r1,
            &choice_tap_or_mana,
            None,
            cache,
        );
        assert!(!constraints.allows_tap(r1));
        assert!(!can_pay_single_cost_cached(
            &state,
            PlayerId(0),
            r1,
            &CostDef::Choice {
                options: vec![vec![CostDef::Mana {
                    cost: ManaCost::parse("{5}"),
                    x_spend_colors: ColorMask::empty(),
                    x_colored_only: false,
                    x_max_one_per_color: false,
                }]],
                forage: false,
            },
            None,
            cache,
        ));
    }

    #[test]
    fn synth_cost_list_self_exclusion_and_cr_118_3() {
        let make_cre = |name: &str| CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "0".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
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
        let mut state = boot(
            vec![
                make_cre("Synth Tap Sentinel"),
                make_cre("Synth Vanilla Bear"),
            ],
            InternTables::default(),
        );
        let ts1 = state.allocate_timestamp();
        let src = state.insert_object(
            GameObject::new(
                CardDefId(0),
                PlayerId(0),
                Zone::Battlefield,
                ts1,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().battlefield.push(src);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let untapped_cre = Box::new(ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::UNTAPPED,
            ..Default::default()
        });
        let tapped_cre = Box::new(ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::TAPPED,
            ..Default::default()
        });
        let tap_other_1 = CostDef::TapOthers {
            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
            filter: untapped_cre.clone(),
            share_creature_type: false,
        };
        let tap_other_x = CostDef::TapOthers {
            count: Box::new(mtg_ir::Amount::X),
            filter: untapped_cre,
            share_creature_type: false,
        };
        let untap_other_1 = CostDef::UntapOthers {
            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
            filter: tapped_cre,
        };
        let sac_cre_1 = CostDef::Sacrifice {
            count: Box::new(mtg_ir::Amount::Const { n: 1 }),
            target: CostObjectTarget::Filter {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
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
        };

        let fwd_costs = [CostDef::TapSelf, tap_other_1.clone()];
        let rev_costs = [tap_other_1.clone(), CostDef::TapSelf];
        let x_costs = [CostDef::TapSelf, tap_other_x];
        let tap_sac_costs = [CostDef::TapSelf, sac_cre_1.clone()];

        {
            let cache = state.evaluate_board();
            // 1. With only `src` on BF, [TapSelf, TapOthers] and [TapOthers, TapSelf] are NOT payable/satisfiable
            for list in [&fwd_costs[..], &rev_costs[..]] {
                assert!(!can_pay_non_mana_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    list,
                    None,
                    cache,
                ));
                let constraints = auto_tap_constraints_for_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    list,
                    None,
                    cache,
                );
                assert!(!constraints.is_satisfiable_cached(&state, PlayerId(0), cache));
                let (_, cands, min_c, _) = non_mana_cost_candidates_in_list_cached(
                    &state,
                    PlayerId(0),
                    src,
                    &tap_other_1,
                    list,
                    None,
                    cache,
                )
                .unwrap();
                assert!(cands.is_empty());
                assert_eq!(min_c, 1);
            }
            assert_eq!(
                max_x_for_cost_list_cached(&state, PlayerId(0), src, &x_costs, cache),
                0
            );

            // 2. CR 118.3: [TapSelf, Sacrifice(Creature)] STILL allows `src` itself!
            assert!(can_pay_non_mana_costs_cached(
                &state,
                PlayerId(0),
                src,
                &tap_sac_costs,
                None,
                cache,
            ));
            assert!(
                auto_tap_constraints_for_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    &tap_sac_costs,
                    None,
                    cache,
                )
                .is_satisfiable_cached(&state, PlayerId(0), cache)
            );
            let (_, sac_cands, _, _) = non_mana_cost_candidates_in_list_cached(
                &state,
                PlayerId(0),
                src,
                &sac_cre_1,
                &tap_sac_costs,
                None,
                cache,
            )
            .unwrap();
            assert_eq!(sac_cands, vec![src]);
        }

        // 3. Add a second creature -> [TapSelf, TapOthers] & [TapOthers, TapSelf] become payable with only bear in candidates
        let ts2 = state.allocate_timestamp();
        let bear = state.insert_object(
            GameObject::new(
                CardDefId(1),
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
            .push(bear);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        {
            let cache = state.evaluate_board();
            for list in [&fwd_costs[..], &rev_costs[..]] {
                assert!(can_pay_non_mana_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    list,
                    None,
                    cache,
                ));
                let (_, cands, _, _) = non_mana_cost_candidates_in_list_cached(
                    &state,
                    PlayerId(0),
                    src,
                    &tap_other_1,
                    list,
                    None,
                    cache,
                )
                .unwrap();
                assert_eq!(cands, vec![bear]);
            }
            assert_eq!(
                max_x_for_cost_list_cached(&state, PlayerId(0), src, &x_costs, cache),
                1
            );
        }

        // 4. [UntapSelf, UntapOthers] with 1 vs 2 tapped creatures
        state
            .object_mut(src)
            .unwrap()
            .flags
            .insert(ObjectFlags::TAPPED);
        state.mark_dirty(DirtyMask::TAPPED);
        let untap_costs = [CostDef::UntapSelf { via_symbol: true }, untap_other_1];
        {
            let cache = state.evaluate_board();
            assert!(!can_pay_non_mana_costs_cached(
                &state,
                PlayerId(0),
                src,
                &untap_costs,
                None,
                cache,
            ));
            assert!(
                !auto_tap_constraints_for_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    &untap_costs,
                    None,
                    cache,
                )
                .is_satisfiable_cached(&state, PlayerId(0), cache)
            );
        }

        state
            .object_mut(bear)
            .unwrap()
            .flags
            .insert(ObjectFlags::TAPPED);
        state.mark_dirty(DirtyMask::TAPPED);
        {
            let cache = state.evaluate_board();
            assert!(can_pay_non_mana_costs_cached(
                &state,
                PlayerId(0),
                src,
                &untap_costs,
                None,
                cache,
            ));
            assert!(
                auto_tap_constraints_for_costs_cached(
                    &state,
                    PlayerId(0),
                    src,
                    &untap_costs,
                    None,
                    cache,
                )
                .is_satisfiable_cached(&state, PlayerId(0), cache)
            );
        }
    }
}
