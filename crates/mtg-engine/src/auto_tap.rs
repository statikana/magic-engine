use mtg_ir::{CardTypeMask, ColorMask, ManaColor, ManaCost, ManaPip};
use smallvec::SmallVec;

use crate::action::{AutoTapPreview, CastRequest};
use crate::costs;
use crate::id::{ObjectId, PlayerId};
use crate::legality;
use crate::mana::{
    self, ManaAbilityCandidate, ManaSpendContext, SimBucket, SimulatedManaPool, color_to_index,
    colors_from_mask, solve_pool_deduction,
};
use crate::state::{EngineError, GameState, LayerCache};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoTapPolicy {
    pub allow_life_payment: bool,
    pub allow_phyrexian_life: bool,
    pub allow_sacrifice_treasures: bool,
    pub allow_sacrifice_other: bool,
    pub max_beam_width: usize,
    pub exact_backtrack_limit: usize,
}

impl Default for AutoTapPolicy {
    fn default() -> Self {
        Self {
            allow_life_payment: true,
            allow_phyrexian_life: true,
            allow_sacrifice_treasures: true,
            allow_sacrifice_other: false,
            max_beam_width: 32,
            exact_backtrack_limit: 12,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoTapStep {
    pub object: ObjectId,
    pub ability_index: u16,
    pub chosen_color: Option<ManaColor>,
    pub bundle_index: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoTapPlan {
    pub can_pay: bool,
    pub steps: SmallVec<[AutoTapStep; 8]>,
    pub sources_to_tap: SmallVec<[ObjectId; 8]>,
    pub phyrexian_life_flags: SmallVec<[bool; 4]>,
    pub life_cost_total: u32,
}

impl AutoTapPlan {
    fn unpayable() -> Self {
        Self {
            can_pay: false,
            steps: SmallVec::new(),
            sources_to_tap: SmallVec::new(),
            phyrexian_life_flags: SmallVec::new(),
            life_cost_total: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservedGroupConflict {
    /// Needs objects to remain untapped and on battlefield (e.g. `TapOthers`).
    MustStayUntappedAndOnBf,
    /// Needs objects to remain tapped and on battlefield (e.g. `UntapOthers`).
    MustStayTappedAndOnBf,
    /// Needs objects to remain on battlefield (e.g. `Sacrifice`, `ExileFrom(Battlefield)`).
    MustStayOnBf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedObjectGroup {
    pub candidates: SmallVec<[ObjectId; 8]>,
    pub min_needed: usize,
    pub conflict: ReservedGroupConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapSubsetSumConstraint {
    pub candidates: SmallVec<[(ObjectId, i32); 8]>,
    pub min_total: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoTapConstraints {
    pub untappable_objects: SmallVec<[ObjectId; 4]>,
    pub ununtappable_objects: SmallVec<[ObjectId; 4]>,
    pub keep_on_battlefield: SmallVec<[ObjectId; 4]>,
    pub reserved_groups: SmallVec<[ReservedObjectGroup; 2]>,
    pub reserved_life: u32,
    pub reserved_energy: u32,
    pub tap_subset_sum: SmallVec<[TapSubsetSumConstraint; 1]>,
}

impl AutoTapConstraints {
    pub fn allows_tap(&self, id: ObjectId) -> bool {
        if self.untappable_objects.contains(&id) {
            return false;
        }
        for group in &self.reserved_groups {
            if group.conflict == ReservedGroupConflict::MustStayUntappedAndOnBf
                && group.candidates.contains(&id)
            {
                let remaining = group
                    .candidates
                    .iter()
                    .filter(|&&c| c != id && !self.untappable_objects.contains(&c))
                    .count();
                if remaining < group.min_needed {
                    return false;
                }
            }
        }
        for tss in &self.tap_subset_sum {
            if tss.candidates.iter().any(|&(c, _)| c == id) {
                let rem_sum: i32 = tss
                    .candidates
                    .iter()
                    .filter(|&&(c, _)| c != id && !self.untappable_objects.contains(&c))
                    .map(|&(_, p)| p)
                    .sum();
                if rem_sum < tss.min_total {
                    return false;
                }
            }
        }
        true
    }

    pub fn allows_untap(&self, id: ObjectId) -> bool {
        if self.ununtappable_objects.contains(&id) {
            return false;
        }
        for group in &self.reserved_groups {
            if group.conflict == ReservedGroupConflict::MustStayTappedAndOnBf
                && group.candidates.contains(&id)
            {
                let remaining = group
                    .candidates
                    .iter()
                    .filter(|&&c| c != id && !self.ununtappable_objects.contains(&c))
                    .count();
                if remaining < group.min_needed {
                    return false;
                }
            }
        }
        true
    }

    pub fn allows_leave_battlefield(&self, id: ObjectId) -> bool {
        if self.keep_on_battlefield.contains(&id) {
            return false;
        }
        for group in &self.reserved_groups {
            if group.candidates.contains(&id) {
                let remaining = match group.conflict {
                    ReservedGroupConflict::MustStayUntappedAndOnBf => group
                        .candidates
                        .iter()
                        .filter(|&&c| c != id && !self.untappable_objects.contains(&c))
                        .count(),
                    ReservedGroupConflict::MustStayTappedAndOnBf => group
                        .candidates
                        .iter()
                        .filter(|&&c| c != id && !self.ununtappable_objects.contains(&c))
                        .count(),
                    ReservedGroupConflict::MustStayOnBf => {
                        group.candidates.iter().filter(|&&c| c != id).count()
                    }
                };
                if remaining < group.min_needed {
                    return false;
                }
            }
        }
        for tss in &self.tap_subset_sum {
            if tss.candidates.iter().any(|&(c, _)| c == id) {
                let rem_sum: i32 = tss
                    .candidates
                    .iter()
                    .filter(|&&(c, _)| c != id && !self.untappable_objects.contains(&c))
                    .map(|&(_, p)| p)
                    .sum();
                if rem_sum < tss.min_total {
                    return false;
                }
            }
        }
        true
    }

    pub fn allows_mana_candidate(
        &self,
        state: &GameState,
        cand: &ManaAbilityCandidate,
        player: PlayerId,
        _cache: &LayerCache,
    ) -> bool {
        if cand.requires_tap && !self.allows_tap(cand.object) {
            return false;
        }
        if cand.requires_untap && !self.allows_untap(cand.object) {
            return false;
        }
        if cand.has_sacrifice_self && !self.allows_leave_battlefield(cand.object) {
            return false;
        }
        if cand.has_life_cost > 0 {
            let avail_life = state
                .player(player)
                .map_or(0, |p| p.life.max(0) as u32)
                .saturating_sub(self.reserved_life);
            if avail_life < cand.has_life_cost {
                return false;
            }
        }
        if cand.has_energy_cost > 0 {
            let avail_energy = state
                .player(player)
                .map_or(0, |p| u32::from(p.energy))
                .saturating_sub(self.reserved_energy);
            if avail_energy < cand.has_energy_cost {
                return false;
            }
        }
        true
    }

    fn allows_state_step(
        &self,
        tapped: &[ObjectId],
        untapped: &[ObjectId],
        sacrificed: &[ObjectId],
        cand: &ManaAbilityCandidate,
    ) -> bool {
        if cand.requires_tap && self.untappable_objects.contains(&cand.object) {
            return false;
        }
        if cand.requires_untap && self.ununtappable_objects.contains(&cand.object) {
            return false;
        }
        if cand.has_sacrifice_self && self.keep_on_battlefield.contains(&cand.object) {
            return false;
        }
        for group in &self.reserved_groups {
            match group.conflict {
                ReservedGroupConflict::MustStayUntappedAndOnBf => {
                    if (cand.requires_tap || cand.has_sacrifice_self)
                        && group.candidates.contains(&cand.object)
                    {
                        let remaining = group
                            .candidates
                            .iter()
                            .filter(|&&id| {
                                id != cand.object
                                    && !self.untappable_objects.contains(&id)
                                    && !tapped.contains(&id)
                                    && !sacrificed.contains(&id)
                            })
                            .count();
                        if remaining < group.min_needed {
                            return false;
                        }
                    }
                }
                ReservedGroupConflict::MustStayTappedAndOnBf => {
                    if (cand.requires_untap || cand.has_sacrifice_self)
                        && group.candidates.contains(&cand.object)
                    {
                        let remaining = group
                            .candidates
                            .iter()
                            .filter(|&&id| {
                                id != cand.object
                                    && !self.ununtappable_objects.contains(&id)
                                    && !untapped.contains(&id)
                                    && !sacrificed.contains(&id)
                            })
                            .count();
                        if remaining < group.min_needed {
                            return false;
                        }
                    }
                }
                ReservedGroupConflict::MustStayOnBf => {
                    if cand.has_sacrifice_self && group.candidates.contains(&cand.object) {
                        let remaining = group
                            .candidates
                            .iter()
                            .filter(|&&id| id != cand.object && !sacrificed.contains(&id))
                            .count();
                        if remaining < group.min_needed {
                            return false;
                        }
                    }
                }
            }
        }
        for tss in &self.tap_subset_sum {
            if (cand.requires_tap || cand.has_sacrifice_self)
                && tss.candidates.iter().any(|&(id, _)| id == cand.object)
            {
                let rem_sum: i32 = tss
                    .candidates
                    .iter()
                    .filter(|&&(id, _)| {
                        id != cand.object
                            && !self.untappable_objects.contains(&id)
                            && !tapped.contains(&id)
                            && !sacrificed.contains(&id)
                    })
                    .map(|&(_, p)| p)
                    .sum();
                if rem_sum < tss.min_total {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn is_satisfiable_cached(
        &self,
        state: &GameState,
        player: PlayerId,
        cache: &LayerCache,
    ) -> bool {
        if self.reserved_life > 0 {
            let spend_ctx = mana::ManaSpendContext::for_generic_cost(player);
            if mana::is_cant_pay_life_cached(state, player, &spend_ctx, cache) {
                return false;
            }
            let life = state.player(player).map_or(0, |p| p.life);
            if life < 0 || (life as u32) < self.reserved_life {
                return false;
            }
        }
        if self.reserved_energy > 0 {
            let energy = state.player(player).map_or(0, |p| u32::from(p.energy));
            if energy < self.reserved_energy {
                return false;
            }
        }
        for group in &self.reserved_groups {
            let avail = match group.conflict {
                ReservedGroupConflict::MustStayUntappedAndOnBf => group
                    .candidates
                    .iter()
                    .filter(|id| !self.untappable_objects.contains(id))
                    .count(),
                ReservedGroupConflict::MustStayTappedAndOnBf => group
                    .candidates
                    .iter()
                    .filter(|id| !self.ununtappable_objects.contains(id))
                    .count(),
                ReservedGroupConflict::MustStayOnBf => group.candidates.len(),
            };
            if avail < group.min_needed {
                return false;
            }
        }
        for tss in &self.tap_subset_sum {
            let total_power: i32 = tss
                .candidates
                .iter()
                .filter(|(id, _)| !self.untappable_objects.contains(id))
                .map(|&(_, p)| p)
                .sum();
            if total_power < tss.min_total {
                return false;
            }
        }
        true
    }
}

#[derive(Clone)]
struct SearchState {
    pool: SimulatedManaPool,
    used_objects: SmallVec<[ObjectId; 12]>,
    untapped_objects: SmallVec<[ObjectId; 4]>,
    sacrificed_objects: SmallVec<[ObjectId; 4]>,
    steps: SmallVec<[AutoTapStep; 8]>,
    sources_to_tap: SmallVec<[ObjectId; 8]>,
    energy_remaining: u32,
    penalty: u32,
}

fn needed_color_mask(cost: &ManaCost, ctx: &ManaSpendContext) -> ColorMask {
    let mut mask = ColorMask::empty();
    for &pip in &cost.pips {
        match pip {
            ManaPip::Colored { color }
            | ManaPip::Phyrexian { color }
            | ManaPip::TwoBrid { color } => {
                mask |= ColorMask::from_mana_color(color);
            }
            ManaPip::Hybrid { a, b } | ManaPip::HybridPhyrexian { a, b } => {
                mask |= ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b);
            }
            ManaPip::ColorlessHybrid { color } => {
                mask |= ColorMask::C | ColorMask::from_mana_color(color);
            }
            ManaPip::Snow => {}
        }
    }
    if cost.generic > 0 {
        if !ctx.x_spend_colors.is_empty() {
            mask |= ctx.x_spend_colors;
        } else if ctx.x_colored_only {
            mask |= ColorMask::ALL_COLORS;
        } else {
            mask |= ColorMask::ALL_COLORS | ColorMask::C;
        }
    }
    mask
}

fn candidate_allowed_by_policy_cached(
    state: &GameState,
    cand: &ManaAbilityCandidate,
    ctx: &ManaSpendContext,
    policy: AutoTapPolicy,
    cant_pay_life: bool,
    cache: &LayerCache,
) -> bool {
    if cand.only_as_instant {
        return false;
    }
    if cand.has_non_mana_downside {
        return false;
    }
    if cand.has_life_cost > 0 && (!policy.allow_life_payment || cant_pay_life) {
        return false;
    }
    if cand.has_sacrifice_other && !policy.allow_sacrifice_other {
        return false;
    }
    if cand.has_sacrifice_self {
        if cand.priority_tier == 4 {
            if !policy.allow_sacrifice_treasures {
                return false;
            }
        } else if !policy.allow_sacrifice_other {
            return false;
        }
    }
    if let Some(spec) = &cand.restriction
        && !mana::can_spend_restriction_for_context_cached(
            state,
            spec,
            Some(cand.object),
            None,
            0xFF,
            ctx,
            cache,
        )
    {
        return false;
    }
    if let Some(src_req) = ctx.source_restriction {
        let ok = match src_req {
            mtg_ir::ManaSourceRestrictionKind::Creature => cache
                .get(cand.object)
                .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE)),
            mtg_ir::ManaSourceRestrictionKind::BasicLand => {
                cache.get(cand.object).is_some_and(|e| {
                    e.supertypes.contains(mtg_ir::SupertypeMask::BASIC)
                        && e.types.contains(CardTypeMask::LAND)
                })
            }
            mtg_ir::ManaSourceRestrictionKind::Snow => cand.is_snow_source,
            mtg_ir::ManaSourceRestrictionKind::Treasure => cand.is_treasure,
            mtg_ir::ManaSourceRestrictionKind::ChosenColor => state
                .rare(cand.object)
                .is_some_and(|r| r.chosen_color.is_some()),
        };
        if !ok {
            return false;
        }
    }
    true
}

#[derive(Clone, Copy)]
struct CandidateChoice {
    chosen_color: Option<ManaColor>,
    bundle_index: Option<u8>,
}

fn bundle_multiset_counts(bundle: &[ManaColor]) -> [u32; 6] {
    let mut counts = [0u32; 6];
    for &c in bundle {
        let idx = color_to_index(c);
        counts[idx] = counts[idx].saturating_add(1);
    }
    counts
}

fn color_options_for_candidate(
    cand: &ManaAbilityCandidate,
    needed_mask: ColorMask,
) -> SmallVec<[CandidateChoice; 8]> {
    let mut opts: SmallVec<[CandidateChoice; 8]> = SmallVec::new();
    if !cand.bundle_options.is_empty() {
        let mut seen_multisets: SmallVec<[[u32; 6]; 8]> = SmallVec::new();
        for (bi, b) in cand.bundle_options.iter().enumerate() {
            let counts = bundle_multiset_counts(b);
            if !seen_multisets.contains(&counts) {
                seen_multisets.push(counts);
                opts.push(CandidateChoice {
                    chosen_color: b.first().copied(),
                    bundle_index: u8::try_from(bi).ok(),
                });
            }
        }
        return opts;
    }
    if cand.max_any_color_amount > 0 {
        let useful = cand.produces_colors & needed_mask;
        let pick_from = if !useful.is_empty() {
            useful
        } else {
            cand.produces_colors
        };
        for c in colors_from_mask(pick_from) {
            opts.push(CandidateChoice {
                chosen_color: Some(c),
                bundle_index: None,
            });
        }
        if opts.is_empty() {
            opts.push(CandidateChoice {
                chosen_color: Some(ManaColor::C),
                bundle_index: None,
            });
        }
        return opts;
    }
    opts.push(CandidateChoice {
        chosen_color: None,
        bundle_index: None,
    });
    opts
}

fn apply_candidate_to_pool(
    pool: &mut SimulatedManaPool,
    cand: &ManaAbilityCandidate,
    choice: CandidateChoice,
) -> bool {
    // Check & pay extra_mana_cost (e.g. filter lands / signets) from unrestricted simulated pool first!
    // Outer spell-restricted buckets (e.g. creature-only mana) cannot pay mana ability activation costs.
    if let Some(in_cost) = &cand.extra_mana_cost {
        let unrestricted_pool = SimulatedManaPool {
            buckets: Vec::new(),
            ..pool.clone()
        };
        let Some(ded) = solve_pool_deduction(&unrestricted_pool, in_cost, &[], false) else {
            return false;
        };
        for (i, &amt) in ded.pool_spent.iter().enumerate() {
            pool.unrestricted[i] = pool.unrestricted[i].saturating_sub(amt);
            pool.snow[i] = pool.snow[i]
                .saturating_sub(ded.snow_spent[i])
                .min(pool.unrestricted[i]);
        }
    }

    if cand.has_life_cost > 0 {
        if pool.cant_pay_life || pool.life_available < cand.has_life_cost {
            return false;
        }
        pool.life_available -= cand.has_life_cost;
    }

    let mut produced = [0u32; 6];
    for (i, &amt) in cand.produces_amounts.iter().enumerate() {
        produced[i] = produced[i].saturating_add(amt);
    }
    if !cand.bundle_options.is_empty() {
        let chosen_bundle = choice
            .bundle_index
            .and_then(|bi| cand.bundle_options.get(usize::from(bi)))
            .or_else(|| {
                choice.chosen_color.and_then(|c| {
                    cand.bundle_options
                        .iter()
                        .find(|b| b.iter().all(|x| *x == c))
                        .or_else(|| cand.bundle_options.iter().find(|b| b.contains(&c)))
                })
            })
            .or_else(|| cand.bundle_options.first());
        if let Some(b) = chosen_bundle {
            for &c in b {
                let idx = color_to_index(c);
                produced[idx] = produced[idx].saturating_add(1);
            }
        }
    } else if cand.max_any_color_amount > 0 {
        let c = choice
            .chosen_color
            .or_else(|| colors_from_mask(cand.produces_colors).into_iter().next())
            .unwrap_or(ManaColor::C);
        let idx = color_to_index(c);
        produced[idx] = produced[idx].saturating_add(cand.max_any_color_amount);
    }

    if let Some(spec) = &cand.restriction {
        let next_idx = pool.buckets.len() + 10_000;
        pool.buckets.push(SimBucket {
            orig_idx: next_idx,
            colors: produced,
            disallow_generic: spec.disallow_generic,
            is_snow: cand.is_snow_source,
        });
    } else {
        for (i, &amt) in produced.iter().enumerate() {
            pool.unrestricted[i] = pool.unrestricted[i].saturating_add(amt);
            if cand.is_snow_source {
                pool.snow[i] = pool.snow[i].saturating_add(amt);
            }
        }
    }
    true
}

fn candidate_step_penalty(cand: &ManaAbilityCandidate) -> u32 {
    let mut pen = 10u32;
    pen += u32::from(cand.priority_tier) * 5;
    if cand.has_sacrifice_cost {
        pen += 10_000;
    }
    if cand.has_life_cost > 0 {
        pen += cand.has_life_cost * 100;
    }
    if cand.has_energy_cost > 0 {
        pen += cand.has_energy_cost * 25;
    }
    if cand.extra_mana_cost.is_some() {
        pen += 15;
    }
    pen
}

fn estimate_remaining_deficit(pool: &SimulatedManaPool, cost: &ManaCost) -> u32 {
    let mut avail = pool.unrestricted;
    for b in &pool.buckets {
        for (i, &v) in b.colors.iter().enumerate() {
            avail[i] = avail[i].saturating_add(v);
        }
    }
    let mut snow_avail: u32 = pool.snow.iter().sum::<u32>()
        + pool
            .buckets
            .iter()
            .filter(|b| b.is_snow)
            .map(|b| b.colors.iter().sum::<u32>())
            .sum::<u32>();
    let mut deficit = 0u32;
    let mut total_used = 0u32;
    for &pip in &cost.pips {
        match pip {
            ManaPip::Colored { color } => {
                let idx = color_to_index(color);
                if avail[idx] > 0 {
                    avail[idx] -= 1;
                    total_used += 1;
                } else if pool.any_color && color != ManaColor::C {
                    deficit += 1;
                } else {
                    deficit += 2;
                }
            }
            ManaPip::Snow => {
                if snow_avail > 0 {
                    snow_avail -= 1;
                    total_used += 1;
                } else {
                    deficit += 2;
                }
            }
            ManaPip::Hybrid { a, b } => {
                let ia = color_to_index(a);
                let ib = color_to_index(b);
                if avail[ia] > 0 {
                    avail[ia] -= 1;
                    total_used += 1;
                } else if avail[ib] > 0 {
                    avail[ib] -= 1;
                    total_used += 1;
                } else {
                    deficit += 1;
                }
            }
            ManaPip::ColorlessHybrid { color } => {
                let ic = color_to_index(ManaColor::C);
                let io = color_to_index(color);
                if avail[ic] > 0 {
                    avail[ic] -= 1;
                    total_used += 1;
                } else if avail[io] > 0 {
                    avail[io] -= 1;
                    total_used += 1;
                } else {
                    deficit += 1;
                }
            }
            ManaPip::TwoBrid { color } => {
                let io = color_to_index(color);
                if avail[io] > 0 {
                    avail[io] -= 1;
                    total_used += 1;
                } else {
                    deficit += 1;
                }
            }
            ManaPip::Phyrexian { color } => {
                let io = color_to_index(color);
                if avail[io] > 0 {
                    avail[io] -= 1;
                    total_used += 1;
                } else {
                    deficit += 1;
                }
            }
            ManaPip::HybridPhyrexian { a, b } => {
                let ia = color_to_index(a);
                let ib = color_to_index(b);
                if avail[ia] > 0 {
                    avail[ia] -= 1;
                    total_used += 1;
                } else if avail[ib] > 0 {
                    avail[ib] -= 1;
                    total_used += 1;
                } else {
                    deficit += 1;
                }
            }
        }
    }
    let total_pool: u32 = pool.total_available();
    let rem_pool = total_pool.saturating_sub(total_used);
    deficit + cost.generic.saturating_sub(rem_pool)
}

fn finalize_plan(
    cost: &ManaCost,
    s: &SearchState,
    allow_phyrexian_life: bool,
    candidates: &[ManaAbilityCandidate],
) -> Option<AutoTapPlan> {
    let ded = solve_pool_deduction(&s.pool, cost, &[], allow_phyrexian_life)?;
    let phy_flags = ded.phyrexian_life_flags;
    let mut life_cost_total = ded.life_paid;
    for step in &s.steps {
        if let Some(c) = candidates
            .iter()
            .find(|cand| cand.object == step.object && cand.ability_index == step.ability_index)
        {
            life_cost_total = life_cost_total.saturating_add(c.has_life_cost);
        }
    }
    Some(AutoTapPlan {
        can_pay: true,
        steps: s.steps.clone(),
        sources_to_tap: s.sources_to_tap.clone(),
        phyrexian_life_flags: phy_flags,
        life_cost_total,
    })
}

pub fn plan_auto_tap(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    policy: AutoTapPolicy,
) -> AutoTapPlan {
    plan_auto_tap_cached(state, player, cost, ctx, policy, state.evaluate_board())
}

pub(crate) fn plan_auto_tap_cached(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    policy: AutoTapPolicy,
    cache: &LayerCache,
) -> AutoTapPlan {
    plan_auto_tap_with_constraints_cached(
        state,
        player,
        cost,
        ctx,
        policy,
        &AutoTapConstraints::default(),
        cache,
    )
}

pub(crate) fn plan_auto_tap_with_constraints_cached(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    policy: AutoTapPolicy,
    constraints: &AutoTapConstraints,
    cache: &LayerCache,
) -> AutoTapPlan {
    if !constraints.is_satisfiable_cached(state, player, cache) {
        return AutoTapPlan::unpayable();
    }
    let mut base_pool = SimulatedManaPool::from_state_cached(state, player, ctx, cache);
    base_pool.life_available = base_pool
        .life_available
        .saturating_sub(constraints.reserved_life);

    // 1. Try paying purely from current pool (preferring without Phyrexian life first).
    if let Some(ded) = solve_pool_deduction(&base_pool, cost, &[], false) {
        return AutoTapPlan {
            can_pay: true,
            steps: SmallVec::new(),
            sources_to_tap: SmallVec::new(),
            phyrexian_life_flags: ded.phyrexian_life_flags,
            life_cost_total: 0,
        };
    }

    let cant_pay_life = base_pool.cant_pay_life;
    let mut candidates: Vec<ManaAbilityCandidate> =
        mana::enumerate_mana_abilities_cached(state, player, cache)
            .into_iter()
            .filter(|c| {
                candidate_allowed_by_policy_cached(state, c, ctx, policy, cant_pay_life, cache)
                    && constraints.allows_mana_candidate(state, c, player, cache)
            })
            .collect();

    // Sort: non-filter lands before filter lands, then by priority tier, then higher output, then deterministic ID.
    candidates.sort_by(|a, b| {
        let a_filt = a.extra_mana_cost.is_some();
        let b_filt = b.extra_mana_cost.is_some();
        let a_out: u32 = a.produces_amounts.iter().sum::<u32>() + a.max_any_color_amount;
        let b_out: u32 = b.produces_amounts.iter().sum::<u32>() + b.max_any_color_amount;
        a_filt
            .cmp(&b_filt)
            .then_with(|| a.priority_tier.cmp(&b.priority_tier))
            .then_with(|| b_out.cmp(&a_out))
            .then_with(|| a.object.index.cmp(&b.object.index))
            .then_with(|| a.ability_index.cmp(&b.ability_index))
    });

    let needed_mask = needed_color_mask(cost, ctx);
    let initial_energy = state
        .player(player)
        .map_or(0, |p| u32::from(p.energy))
        .saturating_sub(constraints.reserved_energy);
    let start_state = SearchState {
        pool: base_pool,
        used_objects: SmallVec::new(),
        untapped_objects: SmallVec::new(),
        sacrificed_objects: SmallVec::new(),
        steps: SmallVec::new(),
        sources_to_tap: SmallVec::new(),
        energy_remaining: initial_energy,
        penalty: 0,
    };

    // Sound O(candidates.len()) upper-bound feasibility pre-check before entering search.
    let mut upper_bound_total = start_state.pool.total_available();
    let mut upper_bound_snow: u32 = start_state.pool.snow.iter().sum::<u32>()
        + start_state
            .pool
            .buckets
            .iter()
            .filter(|b| b.is_snow)
            .map(|b| b.colors.iter().sum::<u32>())
            .sum::<u32>();
    let mut avail_colors = ColorMask::empty();
    for i in 0..6 {
        if start_state.pool.unrestricted[i] > 0
            || start_state.pool.buckets.iter().any(|b| b.colors[i] > 0)
        {
            avail_colors |= ColorMask::from_mana_color(mana::index_to_color(i));
        }
    }
    let mut single_use_caps: SmallVec<[(ObjectId, u32, u32); 32]> = SmallVec::new();
    for cand in &candidates {
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
        } else {
            upper_bound_total = upper_bound_total.saturating_add(cand_gain);
            upper_bound_snow = upper_bound_snow.saturating_add(cand_snow_gain);
        }
    }
    for (_, max_tot, max_sn) in single_use_caps {
        upper_bound_total = upper_bound_total.saturating_add(max_tot);
        upper_bound_snow = upper_bound_snow.saturating_add(max_sn);
    }

    let mut snow_pips_count: u32 = 0;
    let mut phy_pips_count: u32 = 0;
    let mut non_phy_pips_count: u32 = 0;
    for &pip in &cost.pips {
        match pip {
            ManaPip::Snow => {
                snow_pips_count = snow_pips_count.saturating_add(1);
                non_phy_pips_count = non_phy_pips_count.saturating_add(1);
            }
            ManaPip::Colored { color } => {
                non_phy_pips_count = non_phy_pips_count.saturating_add(1);
                if (!start_state.pool.any_color || color == ManaColor::C)
                    && !avail_colors.contains(ColorMask::from_mana_color(color))
                {
                    return AutoTapPlan::unpayable();
                }
            }
            ManaPip::Hybrid { a, b } => {
                non_phy_pips_count = non_phy_pips_count.saturating_add(1);
                if !start_state.pool.any_color
                    && !avail_colors
                        .intersects(ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b))
                {
                    return AutoTapPlan::unpayable();
                }
            }
            ManaPip::ColorlessHybrid { color } => {
                non_phy_pips_count = non_phy_pips_count.saturating_add(1);
                if !start_state.pool.any_color
                    && !avail_colors.intersects(ColorMask::C | ColorMask::from_mana_color(color))
                {
                    return AutoTapPlan::unpayable();
                }
            }
            ManaPip::TwoBrid { .. } => {
                non_phy_pips_count = non_phy_pips_count.saturating_add(1);
            }
            ManaPip::Phyrexian { .. } | ManaPip::HybridPhyrexian { .. } => {
                phy_pips_count = phy_pips_count.saturating_add(1);
            }
        }
    }
    if upper_bound_snow < snow_pips_count {
        return AutoTapPlan::unpayable();
    }
    let try_phy_life = phy_pips_count > 0
        && policy.allow_phyrexian_life
        && !cant_pay_life
        && start_state.pool.life_available >= 2;

    // Try solving without Phyrexian life first; if unpayable and allowed, retry with Phyrexian life.
    for allow_phy_life in [false, true] {
        if allow_phy_life && !try_phy_life {
            continue;
        }
        let max_life_payments = if allow_phy_life {
            start_state.pool.life_available / 2
        } else {
            0
        };
        let min_mana_needed = cost
            .generic
            .saturating_add(non_phy_pips_count)
            .saturating_add(phy_pips_count.saturating_sub(max_life_payments));
        if upper_bound_total < min_mana_needed {
            continue;
        }
        if !allow_phy_life && phy_pips_count > 0 {
            let missing_phy_color = cost.pips.iter().any(|&pip| match pip {
                ManaPip::Phyrexian { color } => {
                    (!start_state.pool.any_color || color == ManaColor::C)
                        && !avail_colors.contains(ColorMask::from_mana_color(color))
                }
                ManaPip::HybridPhyrexian { a, b } => {
                    !start_state.pool.any_color
                        && !avail_colors.intersects(
                            ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b),
                        )
                }
                _ => false,
            });
            if missing_phy_color {
                continue;
            }
        }
        let found = if candidates.len() <= policy.exact_backtrack_limit {
            search_exact_backtrack(
                cost,
                &candidates,
                needed_mask,
                &start_state,
                allow_phy_life,
                constraints,
            )
        } else {
            search_beam(
                cost,
                &candidates,
                needed_mask,
                &start_state,
                allow_phy_life,
                policy.max_beam_width.max(1),
                constraints,
            )
        };
        if let Some(best) = found
            && let Some(plan) = finalize_plan(cost, &best, allow_phy_life, &candidates)
        {
            return plan;
        }
        if allow_phy_life
            && solve_pool_deduction(&start_state.pool, cost, &[], true).is_some()
            && let Some(plan) = finalize_plan(cost, &start_state, true, &candidates)
        {
            return plan;
        }
    }

    AutoTapPlan::unpayable()
}

fn search_exact_backtrack(
    cost: &ManaCost,
    candidates: &[ManaAbilityCandidate],
    needed_mask: ColorMask,
    start: &SearchState,
    allow_phy_life: bool,
    constraints: &AutoTapConstraints,
) -> Option<SearchState> {
    let mut best: Option<(SearchState, u32)> = None;
    let max_depth = (cost.mana_value() as usize + 4).min(candidates.len() + 2);
    let mut node_budget: usize = 4096;

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        cost: &ManaCost,
        candidates: &[ManaAbilityCandidate],
        needed_mask: ColorMask,
        cur: &SearchState,
        allow_phy_life: bool,
        constraints: &AutoTapConstraints,
        max_depth: usize,
        node_budget: &mut usize,
        best: &mut Option<(SearchState, u32)>,
    ) {
        if *node_budget == 0 {
            return;
        }
        *node_budget -= 1;

        if let Some((_, best_pen)) = best
            && cur.penalty >= *best_pen
        {
            return;
        }

        if let Some(ded) = solve_pool_deduction(&cur.pool, cost, &[], allow_phy_life) {
            let total_pen = cur
                .penalty
                .saturating_add(ded.life_paid.saturating_mul(100));
            if best.as_ref().is_none_or(|(_, bp)| total_pen < *bp) {
                *best = Some((cur.clone(), total_pen));
            }
            if ded.life_paid == 0 {
                return;
            }
        }

        if cur.steps.len() >= max_depth {
            return;
        }

        // To allow topological feeder-then-filter ordering while avoiding duplicate permutations of
        // independent sources, we enforce index ordering among non-filter sources and allow filter
        // lands whenever their input mana becomes available.
        let last_non_filter_pos = cur.steps.iter().rev().find_map(|st| {
            candidates
                .iter()
                .position(|c| c.object == st.object && c.ability_index == st.ability_index)
                .filter(|&idx| candidates[idx].extra_mana_cost.is_none())
        });

        for (ci, cand) in candidates.iter().enumerate() {
            if cand.extra_mana_cost.is_none()
                && let Some(prev_idx) = last_non_filter_pos
                && ci <= prev_idx
            {
                continue;
            }
            if cur.sacrificed_objects.contains(&cand.object) {
                continue;
            }
            if (cand.requires_tap || cand.requires_untap || cand.has_sacrifice_self)
                && cur.used_objects.contains(&cand.object)
            {
                continue;
            }
            if cur
                .steps
                .iter()
                .any(|s| s.object == cand.object && s.ability_index == cand.ability_index)
            {
                continue;
            }
            if cand.has_energy_cost > cur.energy_remaining {
                continue;
            }
            if !constraints.allows_state_step(
                &cur.sources_to_tap,
                &cur.untapped_objects,
                &cur.sacrificed_objects,
                cand,
            ) {
                continue;
            }
            let step_pen = candidate_step_penalty(cand);
            if let Some((_, best_pen)) = best
                && cur.penalty + step_pen >= *best_pen
            {
                continue;
            }

            let color_opts = color_options_for_candidate(cand, needed_mask);
            for choice in color_opts {
                let mut next = cur.clone();
                if !apply_candidate_to_pool(&mut next.pool, cand, choice) {
                    continue;
                }
                next.energy_remaining = next.energy_remaining.saturating_sub(cand.has_energy_cost);
                next.penalty = next.penalty.saturating_add(step_pen);
                if cand.requires_tap || cand.requires_untap || cand.has_sacrifice_self {
                    next.used_objects.push(cand.object);
                }
                if cand.requires_untap && !next.untapped_objects.contains(&cand.object) {
                    next.untapped_objects.push(cand.object);
                }
                if cand.has_sacrifice_self && !next.sacrificed_objects.contains(&cand.object) {
                    next.sacrificed_objects.push(cand.object);
                }
                if cand.requires_tap && !next.sources_to_tap.contains(&cand.object) {
                    next.sources_to_tap.push(cand.object);
                }
                next.steps.push(AutoTapStep {
                    object: cand.object,
                    ability_index: cand.ability_index,
                    chosen_color: choice.chosen_color,
                    bundle_index: choice.bundle_index,
                });

                dfs(
                    cost,
                    candidates,
                    needed_mask,
                    &next,
                    allow_phy_life,
                    constraints,
                    max_depth,
                    node_budget,
                    best,
                );
            }
        }
    }

    dfs(
        cost,
        candidates,
        needed_mask,
        start,
        allow_phy_life,
        constraints,
        max_depth,
        &mut node_budget,
        &mut best,
    );
    best.map(|(s, _)| s)
}

fn search_beam(
    cost: &ManaCost,
    candidates: &[ManaAbilityCandidate],
    needed_mask: ColorMask,
    start: &SearchState,
    allow_phy_life: bool,
    beam_width: usize,
    constraints: &AutoTapConstraints,
) -> Option<SearchState> {
    let max_depth = (cost.mana_value() as usize + 4).min(candidates.len() + 2);
    let mut frontier: Vec<SearchState> = vec![start.clone()];
    let mut best_solution: Option<(SearchState, u32)> = if allow_phy_life {
        solve_pool_deduction(&start.pool, cost, &[], true).map(|ded| {
            let total_pen = start
                .penalty
                .saturating_add(ded.life_paid.saturating_mul(100));
            (start.clone(), total_pen)
        })
    } else {
        None
    };

    for _depth in 0..max_depth {
        let mut next_layer: Vec<SearchState> = Vec::new();
        for cur in &frontier {
            let last_non_filter_pos = cur.steps.iter().rev().find_map(|st| {
                candidates
                    .iter()
                    .position(|c| c.object == st.object && c.ability_index == st.ability_index)
                    .filter(|&idx| candidates[idx].extra_mana_cost.is_none())
            });
            for (ci, cand) in candidates.iter().enumerate() {
                if cand.extra_mana_cost.is_none()
                    && let Some(prev_idx) = last_non_filter_pos
                    && ci <= prev_idx
                {
                    continue;
                }
                if cur.sacrificed_objects.contains(&cand.object) {
                    continue;
                }
                if (cand.requires_tap || cand.requires_untap || cand.has_sacrifice_self)
                    && cur.used_objects.contains(&cand.object)
                {
                    continue;
                }
                if cur
                    .steps
                    .iter()
                    .any(|s| s.object == cand.object && s.ability_index == cand.ability_index)
                {
                    continue;
                }
                if cand.has_energy_cost > cur.energy_remaining {
                    continue;
                }
                if !constraints.allows_state_step(
                    &cur.sources_to_tap,
                    &cur.untapped_objects,
                    &cur.sacrificed_objects,
                    cand,
                ) {
                    continue;
                }
                let step_pen = candidate_step_penalty(cand);
                if let Some((_, best_pen)) = &best_solution
                    && cur.penalty + step_pen >= *best_pen
                {
                    continue;
                }

                for choice in color_options_for_candidate(cand, needed_mask) {
                    let mut next = cur.clone();
                    if !apply_candidate_to_pool(&mut next.pool, cand, choice) {
                        continue;
                    }
                    next.energy_remaining =
                        next.energy_remaining.saturating_sub(cand.has_energy_cost);
                    next.penalty = next.penalty.saturating_add(step_pen);
                    if cand.requires_tap || cand.requires_untap || cand.has_sacrifice_self {
                        next.used_objects.push(cand.object);
                    }
                    if cand.requires_untap && !next.untapped_objects.contains(&cand.object) {
                        next.untapped_objects.push(cand.object);
                    }
                    if cand.has_sacrifice_self && !next.sacrificed_objects.contains(&cand.object) {
                        next.sacrificed_objects.push(cand.object);
                    }
                    if cand.requires_tap && !next.sources_to_tap.contains(&cand.object) {
                        next.sources_to_tap.push(cand.object);
                    }
                    next.steps.push(AutoTapStep {
                        object: cand.object,
                        ability_index: cand.ability_index,
                        chosen_color: choice.chosen_color,
                        bundle_index: choice.bundle_index,
                    });

                    if let Some(ded) = solve_pool_deduction(&next.pool, cost, &[], allow_phy_life) {
                        let total_pen = next
                            .penalty
                            .saturating_add(ded.life_paid.saturating_mul(100));
                        if best_solution.as_ref().is_none_or(|(_, bp)| total_pen < *bp) {
                            best_solution = Some((next.clone(), total_pen));
                        }
                        if ded.life_paid > 0
                            && best_solution
                                .as_ref()
                                .is_none_or(|(_, bp)| next.penalty < *bp)
                        {
                            next_layer.push(next);
                        }
                    } else {
                        next_layer.push(next);
                    }
                }
            }
        }
        if next_layer.is_empty() {
            break;
        }
        next_layer.sort_by_key(|s| s.penalty + estimate_remaining_deficit(&s.pool, cost) * 20);
        next_layer.truncate(beam_width);
        frontier = next_layer;
    }

    best_solution.map(|(s, _)| s)
}

pub fn preview_auto_tap_for_request(
    state: &GameState,
    player: PlayerId,
    req: &CastRequest,
) -> AutoTapPreview {
    let cache = state.evaluate_board();
    let Some(option) =
        legality::can_cast_spell_option_cached(state, player, req.object, req.option_id, cache)
    else {
        return AutoTapPreview::default();
    };
    let x_val = req.x_value.map(u32::from);

    if x_val == Some(0) && costs::spell_x_cant_be_zero(state, req.object, &option) {
        return AutoTapPreview::default();
    }

    let Ok(breakdown) = costs::compute_effective_spell_cost_cached(
        state,
        player,
        req.object,
        &option,
        x_val,
        &option.optional_cost_indices,
        &[],
        cache,
    ) else {
        return AutoTapPreview::default();
    };

    let spell_self_ctx = costs::CostListSelfContext::for_spell_cast(option.from_zone);
    if !costs::can_pay_non_mana_costs_with_ctx_cached(
        state,
        player,
        req.object,
        &breakdown.non_mana_costs,
        x_val,
        spell_self_ctx,
        cache,
    ) {
        return AutoTapPreview::default();
    }

    let mut ctx =
        legality::build_mana_spend_context_cached(state, player, req.object, &option, cache);
    ctx.x_spend_colors = breakdown.x_spend_colors;
    ctx.x_colored_only = breakdown.x_colored_only;
    ctx.x_max_one_per_color = breakdown.x_max_one_per_color;
    ctx.source_restriction = breakdown.source_restriction;

    let constraints = costs::auto_tap_constraints_for_costs_with_ctx_cached(
        state,
        player,
        req.object,
        &breakdown.non_mana_costs,
        x_val,
        spell_self_ctx,
        cache,
    );

    let plan = plan_auto_tap_with_constraints_cached(
        state,
        player,
        &breakdown.locked_mana,
        &ctx,
        AutoTapPolicy::default(),
        &constraints,
        cache,
    );
    if !plan.can_pay {
        return AutoTapPreview::default();
    }
    AutoTapPreview {
        can_pay: true,
        sources_to_tap: plan.sources_to_tap,
    }
}

pub fn execute_auto_tap_plan(
    state: &mut GameState,
    player: PlayerId,
    plan: &AutoTapPlan,
) -> Result<(), EngineError> {
    if !plan.can_pay {
        return Err(EngineError::IllegalAction(
            "cannot execute unpayable auto-tap plan".into(),
        ));
    }
    for step in &plan.steps {
        match mana::activate_mana_ability_with_bundle(
            state,
            player,
            step.object,
            step.ability_index,
            step.chosen_color,
            step.bundle_index.map(usize::from),
        )? {
            mana::ManaActivationOutcome::Produced => {}
            mana::ManaActivationOutcome::Paused { .. } => {
                return Err(EngineError::IllegalAction(
                    "auto-tap step paused unexpectedly on mana choice".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{DirtyMask, GameObject, ObjectFlags};
    use mtg_ir::{
        AbilityDef, ActivatedAbility, ActivationRestriction, CardDef, CardDefId, CardLayout,
        CardTypeMask, Characteristics, ColorMask, CostDef, EffectOp, EffectStep, FaceDef,
        IR_V2_VERSION, InternTables, IrV2Bundle, ManaCost, ManaProduction, Selector, SupertypeMask,
        Timing, Zone,
    };
    use std::sync::Arc;

    fn make_tap_mana_card(
        name: &str,
        extra_costs: Vec<CostDef>,
        prod: ManaProduction,
        restrictions: Vec<ActivationRestriction>,
    ) -> CardDef {
        let mut cost = vec![CostDef::TapSelf];
        cost.extend(extra_costs);
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
                    types: CardTypeMask::LAND,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "mana".into(),
                    cost,
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: prod,
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
                    timing: Timing::Mana,
                    is_mana: true,
                    activation_restrictions: restrictions,
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
        }
    }

    fn boot(cards: Vec<CardDef>) -> GameState {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards,
            decks: vec![],
        };
        let db = Arc::new(mtg_ir::CardDatabase::from_bundle(bundle).unwrap());
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 40],
            sideboard: vec![],
        };
        GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap()
    }

    fn add_bf(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            GameObject::new(def, player, Zone::Battlefield, ts, ObjectFlags::empty()),
            None,
        );
        state.player_mut(player).unwrap().battlefield.push(oid);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        oid
    }

    #[test]
    fn filter_land_topological_ordering_feeder_first() {
        let feeder_card = make_tap_mana_card(
            "Synth Wastes",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::C]),
            },
            vec![],
        );
        let filter_card = make_tap_mana_card(
            "Synth Filter",
            vec![CostDef::Mana {
                cost: ManaCost::parse("{1}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            ManaProduction::Bundles {
                options: vec![
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::W]),
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::B]),
                    SmallVec::from_slice(&[ManaColor::B, ManaColor::B]),
                ],
            },
            vec![],
        );

        let mut state = boot(vec![feeder_card, filter_card]);
        // Put filter land BEFORE feeder land in battlefield vector to prove topological ordering
        let filter_id = add_bf(&mut state, PlayerId(0), CardDefId(1));
        let feeder_id = add_bf(&mut state, PlayerId(0), CardDefId(0));

        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let plan = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{W}{W}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        assert!(plan.can_pay);
        assert_eq!(plan.steps.len(), 2);
        // Feeder MUST come before filter land in steps!
        assert_eq!(plan.steps[0].object, feeder_id);
        assert_eq!(plan.steps[1].object, filter_id);
        assert_eq!(plan.steps[1].chosen_color, Some(ManaColor::W));

        execute_auto_tap_plan(&mut state, PlayerId(0), &plan).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [2, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn only_as_instant_mana_skipped_by_auto_tap_and_unpayable_clears_sources() {
        let instant_only_land = make_tap_mana_card(
            "Synth InstantOnly",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::R]),
            },
            vec![ActivationRestriction::OnlyAsInstant],
        );
        let mut state = boot(vec![instant_only_land]);
        add_bf(&mut state, PlayerId(0), CardDefId(0));

        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let plan = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{R}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        assert!(!plan.can_pay);
        assert!(plan.sources_to_tap.is_empty());
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn multi_ability_prism_tapped_at_most_once() {
        let prism = CardDef {
            oracle_id: "oracle_prism".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Prism".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Prism".into(),
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
                abilities: [ManaColor::W, ManaColor::U, ManaColor::B]
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| {
                        AbilityDef::Activated(ActivatedAbility {
                            id: format!("m_{i}"),
                            cost: vec![CostDef::TapSelf],
                            targets: vec![],
                            effect: vec![EffectStep {
                                subject: Selector::SelfObj,
                                op: EffectOp::AddMana {
                                    production: ManaProduction::Fixed {
                                        symbols: SmallVec::from_slice(&[c]),
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
                            timing: Timing::Mana,
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
                        })
                    })
                    .collect(),
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        let mut state = boot(vec![prism]);
        add_bf(&mut state, PlayerId(0), CardDefId(0));

        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let plan = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{W}{U}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        // Only one prism on board -> cannot pay both W and U!
        assert!(!plan.can_pay);
        assert!(plan.sources_to_tap.is_empty());
    }

    #[test]
    fn filter_land_mixed_wb_and_ur_bundles_solved_and_executed() {
        let feeder_card = make_tap_mana_card(
            "Synth Wastes",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::C]),
            },
            vec![],
        );
        let wb_filter = make_tap_mana_card(
            "Synth Fetid",
            vec![CostDef::Mana {
                cost: ManaCost::parse("{1}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            ManaProduction::Bundles {
                options: vec![
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::W]),
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::B]),
                    SmallVec::from_slice(&[ManaColor::B, ManaColor::B]),
                ],
            },
            vec![],
        );
        let ur_filter = make_tap_mana_card(
            "Synth Cascade",
            vec![CostDef::Mana {
                cost: ManaCost::parse("{1}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            ManaProduction::Bundles {
                options: vec![
                    SmallVec::from_slice(&[ManaColor::U, ManaColor::U]),
                    SmallVec::from_slice(&[ManaColor::U, ManaColor::R]),
                    SmallVec::from_slice(&[ManaColor::R, ManaColor::R]),
                ],
            },
            vec![],
        );

        let mut state = boot(vec![feeder_card, wb_filter, ur_filter]);
        let wb_id = add_bf(&mut state, PlayerId(0), CardDefId(1));
        let feeder_wb = add_bf(&mut state, PlayerId(0), CardDefId(0));

        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let plan_wb = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{W}{B}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        assert!(plan_wb.can_pay);
        assert_eq!(plan_wb.steps.len(), 2);
        assert_eq!(plan_wb.steps[0].object, feeder_wb);
        assert_eq!(plan_wb.steps[1].object, wb_id);
        assert_eq!(plan_wb.steps[1].bundle_index, Some(1));

        execute_auto_tap_plan(&mut state, PlayerId(0), &plan_wb).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 0, 1, 0, 0, 0]
        );

        // Reset pool and verify {U}{R} mixed bundle
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let ur_id = add_bf(&mut state, PlayerId(0), CardDefId(2));
        let feeder_ur = add_bf(&mut state, PlayerId(0), CardDefId(0));
        let plan_ur = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{U}{R}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        assert!(plan_ur.can_pay);
        assert_eq!(plan_ur.steps.len(), 2);
        assert_eq!(plan_ur.steps[0].object, feeder_ur);
        assert_eq!(plan_ur.steps[1].object, ur_id);
        assert_eq!(plan_ur.steps[1].bundle_index, Some(1));

        execute_auto_tap_plan(&mut state, PlayerId(0), &plan_ur).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 1, 0, 1, 0, 0]
        );
    }

    #[test]
    fn non_self_sacrifice_artifact_rejected_by_default_policy() {
        let altar = CardDef {
            oracle_id: "oracle_altar".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Altar".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Altar".into(),
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
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "sac_other_mana".into(),
                    cost: vec![CostDef::Sacrifice {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: mtg_ir::CostObjectTarget::Filter {
                            filter: Box::new(mtg_ir::ObjectFilter {
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
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[ManaColor::C, ManaColor::C]),
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
                    timing: Timing::Mana,
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
        let mut state = boot(vec![altar]);
        add_bf(&mut state, PlayerId(0), CardDefId(0));

        let cands = mana::enumerate_mana_abilities(&state, PlayerId(0));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].priority_tier, 3);
        assert!(cands[0].has_sacrifice_other);
        assert!(!cands[0].has_sacrifice_self);

        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let default_plan = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{2}"),
            &ctx,
            AutoTapPolicy::default(),
        );
        assert!(!default_plan.can_pay);

        let opt_in_plan = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{2}"),
            &ctx,
            AutoTapPolicy {
                allow_sacrifice_other: true,
                ..AutoTapPolicy::default()
            },
        );
        assert!(opt_in_plan.can_pay);
    }

    #[test]
    fn auto_tap_snow_restricted_sg_and_multi_phyrexian_and_large_board() {
        let make_custom_land =
            |name: &str,
             color: ManaColor,
             supertypes: SupertypeMask,
             restriction_spec: Option<mtg_ir::ManaSpendingRestriction>| {
                let mut card = make_tap_mana_card(
                    name,
                    vec![],
                    ManaProduction::Fixed {
                        symbols: SmallVec::from_slice(&[color]),
                    },
                    vec![],
                );
                card.faces[0].characteristics.supertypes = supertypes;
                if let AbilityDef::Activated(act) = &mut card.faces[0].abilities[0]
                    && let EffectOp::AddMana {
                        restriction_spec: rs,
                        ..
                    } = &mut act.effect[0].op
                {
                    *rs = restriction_spec.map(Box::new);
                }
                card
            };

        let make_spell = |name: &str, cost_str: &str, types: CardTypeMask| CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "test".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: name.into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse(cost_str),
                    types,
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

        let creature_only_dg_false = mtg_ir::ManaSpendingRestriction {
            allow_spells: Some(mtg_ir::AllowSpellsRestriction {
                filter: Some(mtg_ir::PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disallow_generic: false,
            ..Default::default()
        };
        let creature_only_dg_true = mtg_ir::ManaSpendingRestriction {
            disallow_generic: true,
            ..creature_only_dg_false.clone()
        };

        let snow_cav_f = make_custom_land(
            "Synth Snow Cavern F",
            ManaColor::G,
            SupertypeMask::SNOW,
            Some(creature_only_dg_false),
        );
        let snow_cav_t = make_custom_land(
            "Synth Snow Cavern T",
            ManaColor::G,
            SupertypeMask::SNOW,
            Some(creature_only_dg_true),
        );
        let snow_wastes =
            make_custom_land("Synth Snow Wastes", ManaColor::C, SupertypeMask::SNOW, None);
        let plains = make_custom_land("Synth Plains", ManaColor::W, SupertypeMask::empty(), None);
        let mountain =
            make_custom_land("Synth Mountain", ManaColor::R, SupertypeMask::empty(), None);
        let creature_spell = make_spell("Synth Snow Bear", "{S}{G}", CardTypeMask::CREATURE);
        let instant_spell = make_spell("Synth Ice Bolt", "{S}{G}", CardTypeMask::INSTANT);

        // 1. Solve {S}{G} on creature spell (both disallow_generic true & false) and reject non-creature spell.
        for use_dg_true in [false, true] {
            let mut state = boot(vec![
                snow_cav_f.clone(),
                snow_cav_t.clone(),
                snow_wastes.clone(),
                creature_spell.clone(),
                instant_spell.clone(),
            ]);
            let cav_def = if use_dg_true {
                CardDefId(1)
            } else {
                CardDefId(0)
            };
            add_bf(&mut state, PlayerId(0), cav_def);
            add_bf(&mut state, PlayerId(0), CardDefId(2));

            let ts = state.allocate_timestamp();
            let bear_id = state.insert_object(
                GameObject::new(
                    CardDefId(3),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(bear_id);
            let ts2 = state.allocate_timestamp();
            let bolt_id = state.insert_object(
                GameObject::new(
                    CardDefId(4),
                    PlayerId(0),
                    Zone::Hand,
                    ts2,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(bolt_id);
            state.mark_dirty(DirtyMask::TOPOLOGY);

            let bear_ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: mana::ManaSpendContextKind::Spell,
                source_object: Some(bear_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };
            let bolt_ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: mana::ManaSpendContextKind::Spell,
                source_object: Some(bolt_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };

            let cost_sg = ManaCost::parse("{S}{G}");
            assert!(
                !plan_auto_tap(
                    &state,
                    PlayerId(0),
                    &cost_sg,
                    &bolt_ctx,
                    AutoTapPolicy::default()
                )
                .can_pay
            );

            let plan = plan_auto_tap(
                &state,
                PlayerId(0),
                &cost_sg,
                &bear_ctx,
                AutoTapPolicy::default(),
            );
            assert!(plan.can_pay);
            assert_eq!(plan.steps.len(), 2);
            execute_auto_tap_plan(&mut state, PlayerId(0), &plan).unwrap();
            let ded = mana::deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &cost_sg,
                &bear_ctx,
                &plan.phyrexian_life_flags,
            )
            .unwrap();
            assert_eq!(ded.snow_spent, 2);
            assert_eq!(ded.total_spent, 2);
        }

        // 2. {W/P}{G/P} with 1 Plains returns phyrexian_life_flags == [false, true] and deducts cleanly.
        {
            let mut state = boot(vec![plains.clone(), creature_spell.clone()]);
            add_bf(&mut state, PlayerId(0), CardDefId(0));
            let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
            let cost_phy = ManaCost::parse("{W/P}{G/P}");
            let plan = plan_auto_tap(
                &state,
                PlayerId(0),
                &cost_phy,
                &ctx,
                AutoTapPolicy::default(),
            );
            assert!(plan.can_pay);
            assert_eq!(plan.phyrexian_life_flags.as_slice(), &[false, true]);
            assert_eq!(plan.life_cost_total, 2);
            execute_auto_tap_plan(&mut state, PlayerId(0), &plan).unwrap();
            let ded = mana::deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &cost_phy,
                &ctx,
                &plan.phyrexian_life_flags,
            )
            .unwrap();
            assert_eq!(ded.life_paid_for_phyrexian, 2);
            assert_eq!(ded.total_spent, 1);
        }

        // 3. 32-land board (30 Mountains + 1 snow creature {G} + 1 snow {C}):
        //    solves {S}{G} cleanly and rejects {G}{G} cleanly.
        {
            let mut state = boot(vec![mountain, snow_cav_t, snow_wastes, creature_spell]);
            for _ in 0..30 {
                add_bf(&mut state, PlayerId(0), CardDefId(0));
            }
            add_bf(&mut state, PlayerId(0), CardDefId(1));
            add_bf(&mut state, PlayerId(0), CardDefId(2));

            let ts = state.allocate_timestamp();
            let bear_id = state.insert_object(
                GameObject::new(
                    CardDefId(3),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(bear_id);
            state.mark_dirty(DirtyMask::TOPOLOGY);

            let bear_ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: mana::ManaSpendContextKind::Spell,
                source_object: Some(bear_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };

            let sg_plan = plan_auto_tap(
                &state,
                PlayerId(0),
                &ManaCost::parse("{S}{G}"),
                &bear_ctx,
                AutoTapPolicy::default(),
            );
            assert!(sg_plan.can_pay);
            assert_eq!(sg_plan.steps.len(), 2);

            let gg_plan = plan_auto_tap(
                &state,
                PlayerId(0),
                &ManaCost::parse("{G}{G}"),
                &bear_ctx,
                AutoTapPolicy::default(),
            );
            assert!(!gg_plan.can_pay);
        }
    }

    #[test]
    fn plan_auto_tap_cached_parity_with_64bf_and_synth_nexus() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Dragon".into());
        let dragon_sub = mtg_ir::SubtypeId(0);

        let mountain = make_tap_mana_card(
            "Mountain",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::R]),
            },
            vec![],
        );

        let dragon_land = CardDef {
            oracle_id: "oracle_dragon_land".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Dragon Shrine".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Dragon Shrine".into(),
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
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "dragon_mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[ManaColor::R]),
                            },
                            restriction: None,
                            restriction_spec: Some(Box::new(mtg_ir::ManaSpendingRestriction {
                                allow_spells: Some(mtg_ir::AllowSpellsRestriction {
                                    filter: Some(mtg_ir::PlainManaFilter {
                                        subtypes_any: SmallVec::from_slice(&[dragon_sub]),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            })),
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

        let synth_nexus = CardDef {
            oracle_id: "oracle_synth_nexus".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Nexus".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Nexus".into(),
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
                abilities: vec![AbilityDef::Static(mtg_ir::StaticAbility {
                    static_def: mtg_ir::StaticDef {
                        body: mtg_ir::StaticBody::OffBattlefieldCharacteristics {
                            zones: mtg_ir::ZoneMask::HAND,
                            filter: None,
                            all_players: false,
                            add_all_creature_types: true,
                            add_chosen_subtype: false,
                            add_types: CardTypeMask::empty(),
                            add_subtypes: SmallVec::new(),
                            add_chosen_color: false,
                            add_colors: ColorMask::empty(),
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

        let filler_perm = CardDef {
            oracle_id: "oracle_filler".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Filler Trinket".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Filler Trinket".into(),
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let creature_spell = CardDef {
            oracle_id: "oracle_bear".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Bear".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}{R}"),
                    types: CardTypeMask::CREATURE,
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
        };

        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables,
            cards: vec![
                mountain,
                dragon_land,
                synth_nexus,
                filler_perm,
                creature_spell,
            ],
            decks: vec![],
        };
        let db = Arc::new(mtg_ir::CardDatabase::from_bundle(bundle).unwrap());
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 40],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap();

        add_bf(&mut state, PlayerId(0), CardDefId(0));
        add_bf(&mut state, PlayerId(0), CardDefId(1));
        add_bf(&mut state, PlayerId(0), CardDefId(2));
        for _ in 0..61 {
            add_bf(&mut state, PlayerId(0), CardDefId(3));
        }

        let ts = state.allocate_timestamp();
        let bear_id = state.insert_object(
            GameObject::new(
                CardDefId(4),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(bear_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let bear_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: mana::ManaSpendContextKind::Spell,
            source_object: Some(bear_id),
            from_zone: Zone::Hand,
            ..Default::default()
        };

        let uncached = plan_auto_tap(
            &state,
            PlayerId(0),
            &ManaCost::parse("{1}{R}"),
            &bear_ctx,
            AutoTapPolicy::default(),
        );
        let cache = state.evaluate_board();
        let cached = plan_auto_tap_cached(
            &state,
            PlayerId(0),
            &ManaCost::parse("{1}{R}"),
            &bear_ctx,
            AutoTapPolicy::default(),
            cache,
        );

        assert!(cached.can_pay);
        assert_eq!(cached.steps.len(), 2);
        assert_eq!(cached, uncached);
    }

    #[test]
    fn synth_auto_pay_mana_untap_self_and_treasure_sac_filter_conflict() {
        let make_step = |subject: mtg_ir::Selector, op: EffectOp| EffectStep {
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
        };
        let make_ab = |id: &str,
                       cost: Vec<CostDef>,
                       effect: Vec<EffectStep>,
                       timing: Timing,
                       is_mana: bool| {
            AbilityDef::Activated(ActivatedAbility {
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
                activation_zones: mtg_ir::ZoneMask::empty(),
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
        let draw_step = make_step(
            mtg_ir::Selector::Player {
                filter: mtg_ir::PlayerFilter {
                    relation: mtg_ir::PlayerRelation::You,
                },
            },
            EffectOp::Draw {
                up_to: false,
                reveal_drawn: false,
            },
        );
        let add_mana_step = |c: ManaColor| {
            make_step(
                mtg_ir::Selector::SelfObj,
                EffectOp::AddMana {
                    production: ManaProduction::Fixed {
                        symbols: SmallVec::from_slice(&[c]),
                    },
                    restriction: None,
                    restriction_spec: None,
                    retain_until_eot: false,
                    delayed_rider: None,
                },
            )
        };

        let mut tables = InternTables::default();
        tables.subtypes.push("Treasure".into());
        let sub_treasure = mtg_ir::SubtypeId(0);

        // Card 0: Synth Untapper ({Q}: Add {C}; {1}, {Q}: Draw 1)
        let untapper_def = CardDef {
            oracle_id: "c_synth_untapper".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Untapper".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Untapper".into(),
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
                abilities: vec![
                    make_ab(
                        "q_mana",
                        vec![CostDef::UntapSelf { via_symbol: true }],
                        vec![add_mana_step(ManaColor::C)],
                        Timing::Mana,
                        true,
                    ),
                    make_ab(
                        "q_draw",
                        vec![
                            CostDef::Mana {
                                cost: ManaCost::parse("{1}"),
                                x_spend_colors: ColorMask::empty(),
                                x_colored_only: false,
                                x_max_one_per_color: false,
                            },
                            CostDef::UntapSelf { via_symbol: true },
                        ],
                        vec![draw_step.clone()],
                        Timing::Instant,
                        false,
                    ),
                ],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 1: Synth Swamp ({T}: Add {B})
        let swamp_def = make_tap_mana_card(
            "Synth Swamp",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::B]),
            },
            vec![],
        );

        // Card 2: Synth Treasure ({T}, Sacrifice ~: Add {C})
        let treasure_def = CardDef {
            oracle_id: "c_synth_treasure".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Treasure".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Treasure".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ARTIFACT,
                    subtypes: SmallVec::from_slice(&[sub_treasure]),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![make_ab(
                    "sac_mana",
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
                    vec![add_mana_step(ManaColor::C)],
                    Timing::Mana,
                    true,
                )],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        // Card 3: Synth Tinkercraft ({1}, Sacrifice an artifact: Draw 1)
        let tinkercraft_def = CardDef {
            oracle_id: "c_synth_tinkercraft".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Tinkercraft".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Tinkercraft".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{1}"),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(mtg_ir::SpellAbility {
                    id: Some("spell".into()),
                    additional_costs: vec![CostDef::Sacrifice {
                        count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                        target: mtg_ir::CostObjectTarget::Filter {
                            filter: Box::new(mtg_ir::ObjectFilter {
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
                    effect: vec![draw_step.clone()],
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

        // Card 4: Synth Painland ({T}, Pay 1 life: Add {W})
        let painland_def = make_tap_mana_card(
            "Synth Painland",
            vec![CostDef::PayLife {
                amount: Box::new(mtg_ir::Amount::Const { n: 1 }),
                half_life_rounded_up: false,
            }],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::W]),
            },
            vec![],
        );

        // Card 5: Synth Bloodpact ({2}, Pay 2 life: Draw 1)
        let bloodpact_def = CardDef {
            oracle_id: "c_synth_bloodpact".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Bloodpact".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bloodpact".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::parse("{2}"),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(mtg_ir::SpellAbility {
                    id: Some("spell".into()),
                    additional_costs: vec![CostDef::PayLife {
                        amount: Box::new(mtg_ir::Amount::Const { n: 2 }),
                        half_life_rounded_up: false,
                    }],
                    targets: vec![],
                    effect: vec![draw_step],
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
            mtg_ir::CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    untapper_def,
                    swamp_def,
                    treasure_def,
                    tinkercraft_def,
                    painland_def,
                    bloodpact_def,
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

        // Subtest A: UntapSelf conflict ({Q}: Add {C} vs {1}, {Q}: Draw 1)
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                201,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);

            let untapper = add_bf(&mut state, PlayerId(0), CardDefId(0));
            state
                .object_mut(untapper)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);

            assert!(
                legality::can_activate_ability(&state, PlayerId(0), untapper, 1).is_none(),
                "cannot pay {{1}} using own {{Q}} mana ability when Ability 1 also requires {{Q}}"
            );

            let swamp = add_bf(&mut state, PlayerId(0), CardDefId(1));
            crate::casting::begin_activate_ability(&mut state, PlayerId(0), untapper, 1).unwrap();
            let cast_acts = crate::casting::legal_casting_actions(&state, PlayerId(0));
            assert!(!cast_acts.iter().any(|a| matches!(
                a,
                crate::action::Action::TapMana {
                    object,
                    ability_index: 0,
                    ..
                } if *object == untapper
            )));
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert!(
                state
                    .object(swamp)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                !state
                    .object(untapper)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.stack().len(), 1);
        }

        // Subtest B: Treasure vs Sacrifice(Artifact) filter conflict
        {
            let mut state = GameState::new(
                Arc::clone(&db),
                GameConfig::standard_1v1(),
                &[deck.clone(), deck.clone()],
                202,
            )
            .unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);

            let ts = state.allocate_timestamp();
            let spell_id = state.insert_object(
                GameObject::new(
                    CardDefId(3),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(spell_id);
            let treasure = add_bf(&mut state, PlayerId(0), CardDefId(2));

            assert!(
                legality::can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_none(),
                "single Treasure cannot pay both {{1}} and Sacrifice(Artifact)"
            );

            let swamp = add_bf(&mut state, PlayerId(0), CardDefId(1));
            crate::casting::begin_cast_spell(&mut state, PlayerId(0), spell_id, 0, Zone::Hand)
                .unwrap();
            let cast_acts = crate::casting::legal_casting_actions(&state, PlayerId(0));
            assert!(!cast_acts.iter().any(|a| matches!(
                a,
                crate::action::Action::TapMana { object, .. } if *object == treasure
            )));
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert!(
                state
                    .object(swamp)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            let pid = state.pending_prompt().unwrap().prompt_id;
            crate::choices::apply_choice_answer(
                &mut state,
                PlayerId(0),
                pid,
                crate::action::ChoiceAnswer::PickObject { id: treasure },
            )
            .unwrap();
            assert_eq!(state.stack().len(), 1);
            assert!(
                state
                    .player(PlayerId(0))
                    .unwrap()
                    .battlefield
                    .contains(&swamp)
            );
            assert!(
                !state
                    .player(PlayerId(0))
                    .unwrap()
                    .battlefield
                    .contains(&treasure)
            );
        }

        // Subtest C: PayLife(2) + Painland at 2 life
        {
            let mut state =
                GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 203).unwrap();
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(crate::action::DecisionState::Priority);
            state.player_mut(PlayerId(0)).unwrap().life = 2;

            let ts = state.allocate_timestamp();
            let spell_id = state.insert_object(
                GameObject::new(
                    CardDefId(5),
                    PlayerId(0),
                    Zone::Hand,
                    ts,
                    ObjectFlags::empty(),
                ),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(spell_id);
            let pain = add_bf(&mut state, PlayerId(0), CardDefId(4));
            let swamp1 = add_bf(&mut state, PlayerId(0), CardDefId(1));

            assert!(
                legality::can_cast_spell_option(&state, PlayerId(0), spell_id, 0).is_none(),
                "at 2 life with PayLife(2), Painland cannot be tapped for mana"
            );

            let swamp2 = add_bf(&mut state, PlayerId(0), CardDefId(1));
            crate::casting::begin_cast_spell(&mut state, PlayerId(0), spell_id, 0, Zone::Hand)
                .unwrap();
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert!(
                !state
                    .object(pain)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED),
                "Painland must stay untapped; basic lands must be tapped instead"
            );
            assert!(
                state
                    .object(swamp1)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(swamp2)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().life, 0);
            assert_eq!(state.stack().len(), 1);
        }
    }

    #[test]
    fn synth_unsatisfiable_constraints_reject_auto_pay_and_pool() {
        let life_lock_def = CardDef {
            oracle_id: "c_synth_lifelock".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Lifelock".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Lifelock".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
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
                        body: mtg_ir::StaticBody::RuleModifier {
                            rule: mtg_ir::RuleMod::LifeTotalCantChange,
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
        let swamp_def = make_tap_mana_card(
            "Synth Swamp",
            vec![],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::B]),
            },
            vec![],
        );

        let db = Arc::new(
            mtg_ir::CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards: vec![swamp_def, life_lock_def],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 701).unwrap();
        state.set_active_player(PlayerId(0));
        state.set_phase(mtg_ir::GamePhase::Main1);
        state.set_priority_player(PlayerId(0));
        state.set_decision(crate::action::DecisionState::Priority);

        // Give player {1}{B} directly in pool so mana itself is already payable.
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 1, 0, 0, 1];
        state.player_mut(PlayerId(0)).unwrap().life = 3;
        state.player_mut(PlayerId(0)).unwrap().energy = 2;

        let cost = ManaCost::parse("{1}{B}");
        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));

        // (a) reserved_groups empty (< min_needed)
        let c_group = AutoTapConstraints {
            reserved_groups: smallvec::smallvec![ReservedObjectGroup {
                candidates: SmallVec::new(),
                min_needed: 1,
                conflict: ReservedGroupConflict::MustStayOnBf,
            }],
            ..Default::default()
        };
        // (b) reserved_life > player life
        let c_life = AutoTapConstraints {
            reserved_life: 5,
            ..Default::default()
        };
        // (d) reserved_energy > player energy
        let c_energy = AutoTapConstraints {
            reserved_energy: 3,
            ..Default::default()
        };
        // (e) tap_subset_sum total power < min_total
        let c_tss = AutoTapConstraints {
            tap_subset_sum: smallvec::smallvec![TapSubsetSumConstraint {
                candidates: smallvec::smallvec![(
                    ObjectId {
                        index: 999,
                        generation: 1
                    },
                    2
                )],
                min_total: 4,
            }],
            ..Default::default()
        };

        {
            let cache = state.evaluate_board();
            for c in [&c_group, &c_life, &c_energy, &c_tss] {
                assert!(!c.is_satisfiable_cached(&state, PlayerId(0), cache));
                let plan = plan_auto_tap_with_constraints_cached(
                    &state,
                    PlayerId(0),
                    &cost,
                    &ctx,
                    AutoTapPolicy::default(),
                    c,
                    cache,
                );
                assert!(!plan.can_pay);
            }

            // Satisfiable baseline succeeds
            let c_ok = AutoTapConstraints {
                reserved_life: 2,
                reserved_energy: 2,
                reserved_groups: smallvec::smallvec![ReservedObjectGroup {
                    candidates: smallvec::smallvec![ObjectId {
                        index: 998,
                        generation: 1
                    }],
                    min_needed: 1,
                    conflict: ReservedGroupConflict::MustStayOnBf,
                }],
                tap_subset_sum: smallvec::smallvec![TapSubsetSumConstraint {
                    candidates: smallvec::smallvec![
                        (
                            ObjectId {
                                index: 996,
                                generation: 1
                            },
                            2
                        ),
                        (
                            ObjectId {
                                index: 997,
                                generation: 1
                            },
                            2
                        ),
                    ],
                    min_total: 4,
                }],
                ..Default::default()
            };
            assert!(c_ok.is_satisfiable_cached(&state, PlayerId(0), cache));
            let plan_ok = plan_auto_tap_with_constraints_cached(
                &state,
                PlayerId(0),
                &cost,
                &ctx,
                AutoTapPolicy::default(),
                &c_ok,
                cache,
            );
            assert!(plan_ok.can_pay);
        }

        // (c) reserved_life: 1 with LifeTotalCantChange
        let ts = state.allocate_timestamp();
        let lock_id = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Battlefield,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .battlefield
            .push(lock_id);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let c_cant_life = AutoTapConstraints {
            reserved_life: 1,
            ..Default::default()
        };
        let cache = state.evaluate_board();
        assert!(!c_cant_life.is_satisfiable_cached(&state, PlayerId(0), cache));
        let plan_cant_life = plan_auto_tap_with_constraints_cached(
            &state,
            PlayerId(0),
            &cost,
            &ctx,
            AutoTapPolicy::default(),
            &c_cant_life,
            cache,
        );
        assert!(!plan_cant_life.can_pay);
    }
}
