use std::borrow::Cow;
use std::sync::Arc;

use mtg_ir::{
    AbilityDef, ActivationRestriction, ActivatorWho, Affected, AllowAbilitiesRestriction,
    AllowSpellsRestriction, CardDefId, CardTypeMask, ColorMask, CostDef, DelayedTriggerPayload,
    EffectOp, GamePhase, ManaChoiceKind, ManaColor, ManaCost, ManaPip, ManaProduction,
    ManaRestrictionTag, ManaSourceRestrictionKind, ManaSpendingRestriction, PlainManaFilter,
    RuleMod, StaticBody, SupertypeMask, Timing, Zone,
};
use smallvec::SmallVec;

use crate::action::{ChoiceAnswer, ChoiceRequest, DecisionState};
use crate::costs;
use crate::eval::{
    BoardView, EvalContext, evaluate_amount, evaluate_condition, resolve_player_ref,
};
use crate::events::{AtomicEventKind, emit_event};
use crate::faces::{
    active_abilities_with_card_for_object, face_opts_for_object, resolve_object_faces,
};
use crate::filter::{matches_object_filter, matches_plain_mana_filter};
use crate::id::{ObjectId, PlayerId};
use crate::keywords::synthesize_keyword_activated_ability;
use crate::layers::predicates::GrantedAbilityEntry;
use crate::state::{
    DirtyMask, EngineError, GameState, LayerCache, LkiSnapshot, ManaAbilityPromptState,
    ObjectFlags, PendingManaDelayedRider, PendingPromptContext, PlayerState, PromptOwner,
    RestrictedManaBucket,
};
use crate::triggers::{self, DelayedExpiry, DelayedTriggerEntry};
use crate::zones::{self, MoveObjectOutcome, MoveRawOptions};

pub(crate) const ALL_MANA_COLORS: [ManaColor; 6] = [
    ManaColor::W,
    ManaColor::U,
    ManaColor::B,
    ManaColor::R,
    ManaColor::G,
    ManaColor::C,
];

pub(crate) const FIVE_COLORS: [ManaColor; 5] = [
    ManaColor::W,
    ManaColor::U,
    ManaColor::B,
    ManaColor::R,
    ManaColor::G,
];

const MAX_MANA_PRODUCED_DEPTH: u8 = 4;

#[inline]
pub(crate) const fn color_to_index(c: ManaColor) -> usize {
    match c {
        ManaColor::W => 0,
        ManaColor::U => 1,
        ManaColor::B => 2,
        ManaColor::R => 3,
        ManaColor::G => 4,
        ManaColor::C => 5,
    }
}

#[inline]
pub(crate) const fn index_to_color(idx: usize) -> ManaColor {
    match idx {
        0 => ManaColor::W,
        1 => ManaColor::U,
        2 => ManaColor::B,
        3 => ManaColor::R,
        4 => ManaColor::G,
        _ => ManaColor::C,
    }
}

pub(crate) fn colors_from_mask(mask: ColorMask) -> SmallVec<[ManaColor; 6]> {
    let mut out = SmallVec::new();
    for c in ALL_MANA_COLORS {
        if mask.contains(ColorMask::from_mana_color(c)) {
            out.push(c);
        }
    }
    out
}

pub(crate) fn matches_affected(
    state: &GameState,
    id: ObjectId,
    aff: &Affected,
    ctx: &EvalContext<'_>,
) -> bool {
    match aff {
        Affected::SelfObj => ctx.source == Some(id),
        Affected::AttachedTo => ctx
            .source
            .and_then(|s| state.object(s))
            .is_some_and(|s| s.attached_to == Some(id)),
        Affected::Filter { filter } => matches_object_filter(state, id, filter, ctx),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManaSpendContextKind {
    Spell,
    ActivatedAbility,
    TriggeredAbility,
    SpecialAction,
    ForetellAction,
    WardOrCost,
}

#[derive(Debug, Clone)]
pub struct ManaSpendContext {
    pub player: PlayerId,
    pub kind: ManaSpendContextKind,
    pub source_object: Option<ObjectId>,
    pub face_idx: u8,
    pub from_zone: Zone,
    pub is_commander: bool,
    pub has_x_in_cost: bool,
    pub is_fused: bool,
    pub is_prototyped: bool,
    pub is_bestowed: bool,
    pub is_face_down: bool,
    pub alt_cast_spend_any_color: bool,
    pub mana_as_any_color_ability: bool,
    pub x_spend_colors: ColorMask,
    pub x_colored_only: bool,
    pub x_max_one_per_color: bool,
    pub source_restriction: Option<ManaSourceRestrictionKind>,
}

impl Default for ManaSpendContext {
    fn default() -> Self {
        Self {
            player: PlayerId(0),
            kind: ManaSpendContextKind::WardOrCost,
            source_object: None,
            face_idx: 0,
            from_zone: Zone::Stack,
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
        }
    }
}

impl ManaSpendContext {
    pub fn for_generic_cost(player: PlayerId) -> Self {
        Self {
            player,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManaAbilityCandidate {
    pub object: ObjectId,
    pub ability_index: u16,
    pub is_intrinsic_basic: Option<ManaColor>,
    pub produces_colors: ColorMask,
    pub produces_amounts: [u32; 6],
    pub max_any_color_amount: u32,
    pub bundle_options: Vec<SmallVec<[ManaColor; 3]>>,
    pub restriction: Option<ManaSpendingRestriction>,
    pub has_delayed_rider: bool,
    pub extra_mana_cost: Option<ManaCost>,
    pub requires_tap: bool,
    pub requires_untap: bool,
    pub has_sacrifice_cost: bool,
    pub has_sacrifice_self: bool,
    pub has_sacrifice_other: bool,
    pub has_life_cost: u32,
    pub has_energy_cost: u32,
    pub has_non_mana_downside: bool,
    pub only_as_instant: bool,
    pub is_snow_source: bool,
    pub is_treasure: bool,
    pub priority_tier: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManaPoolDeduction {
    pub spent_colors: ColorMask,
    pub spent_counts: [u32; 6],
    pub total_spent: u32,
    pub snow_spent: u32,
    pub life_paid_for_phyrexian: u32,
    pub cant_be_countered_from_mana: bool,
    pub commander_bonus_counters: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManaActivationOutcome {
    Produced,
    Paused { prompt_id: u32, chooser: PlayerId },
}

/// Internal resolved representation of any activated ability on a permanent (innate, Mutate, or Layer 6 granted),
/// keeping `ability_index` strictly identical across `mana.rs`, `costs.rs`, and `legality.rs`.
pub(crate) struct ResolvedManaAbility<'a> {
    pub ability_index: u16,
    pub is_intrinsic_basic: Option<ManaColor>,
    pub activated: Option<Cow<'a, mtg_ir::ActivatedAbility>>,
    pub mana_as_any_color: bool,
    pub grant_once_each_turn: bool,
}

#[cfg(test)]
pub(crate) fn resolve_object_activated_abilities<'a>(
    state: &'a GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> Vec<ResolvedManaAbility<'a>> {
    resolve_object_activated_abilities_cached(state, id, ctx, state.evaluate_board())
}

pub(crate) fn resolve_object_activated_abilities_cached<'a>(
    state: &'a GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
    cache: &'a LayerCache,
) -> Vec<ResolvedManaAbility<'a>> {
    let mut out = Vec::new();
    let Some(obj) = state.object(id) else {
        return out;
    };
    let Some(opts) = face_opts_for_object(state, id) else {
        return out;
    };
    let extras = cache.extra(id);

    let innate_removed = extras.is_some_and(|e| e.innate_abilities_removed);
    let innate_mana_removed = extras.is_some_and(|e| e.innate_mana_abilities_removed);

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

        let innate_abilities = active_abilities_with_card_for_object(state, id, ctx);
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
            let activated = match ab_ref.ability {
                AbilityDef::Activated(act) => {
                    let is_mana_ab = act.is_mana || act.timing == Timing::Mana;
                    if is_mana_ab {
                        if innate_mana_removed {
                            continue;
                        }
                    } else if innate_removed {
                        continue;
                    }
                    Cow::Borrowed(act)
                }
                AbilityDef::Keyword(kw_ab) => {
                    if innate_removed {
                        continue;
                    }
                    let Some(synth) =
                        synthesize_keyword_activated_ability(state.db(), kw_ab, shared_idx)
                    else {
                        continue;
                    };
                    Cow::Owned(synth)
                }
                _ => continue,
            };
            out.push(ResolvedManaAbility {
                ability_index,
                is_intrinsic_basic: None,
                activated: Some(activated),
                mana_as_any_color: false,
                grant_once_each_turn: false,
            });
        }
    }

    if let Some(ex) = extras {
        for (gi, granted) in ex.granted_abilities.iter().enumerate() {
            let shared_idx = innate_total_len.saturating_add(gi);
            let Ok(ability_index) = u16::try_from(shared_idx) else {
                continue;
            };
            match granted {
                GrantedAbilityEntry::IntrinsicBasicMana { color, .. } => {
                    out.push(ResolvedManaAbility {
                        ability_index,
                        is_intrinsic_basic: Some(*color),
                        activated: None,
                        mana_as_any_color: false,
                        grant_once_each_turn: false,
                    });
                }
                GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index: donor_ab_idx,
                    mana_as_any_color,
                    once_each_turn,
                    ..
                } => {
                    if let Some(card) = state.db().get(*card_def)
                        && let Some(face) = card.faces.get(*face_index as usize)
                        && let Some(ab) = face.abilities.get(*donor_ab_idx)
                    {
                        let activated = match ab {
                            AbilityDef::Activated(act) => Cow::Borrowed(act),
                            AbilityDef::Keyword(kw_ab) => {
                                let Some(synth) = synthesize_keyword_activated_ability(
                                    state.db(),
                                    kw_ab,
                                    *donor_ab_idx,
                                ) else {
                                    continue;
                                };
                                Cow::Owned(synth)
                            }
                            _ => continue,
                        };
                        out.push(ResolvedManaAbility {
                            ability_index,
                            is_intrinsic_basic: None,
                            activated: Some(activated),
                            mana_as_any_color: *mana_as_any_color,
                            grant_once_each_turn: *once_each_turn,
                        });
                    }
                }
                GrantedAbilityEntry::Inline { ability, .. } => {
                    let activated = match ability.as_ref() {
                        AbilityDef::Activated(act) => Cow::Borrowed(act),
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
                    out.push(ResolvedManaAbility {
                        ability_index,
                        is_intrinsic_basic: None,
                        activated: Some(activated),
                        mana_as_any_color: false,
                        grant_once_each_turn: false,
                    });
                }
                GrantedAbilityEntry::DynamicWard { .. } => {}
            }
        }
    }

    // CR 305.6: Any permanent on the battlefield with `CardTypeMask::LAND` and a basic land subtype
    // has the intrinsic ability "{T}: Add {c}" unless its mana abilities were removed or an
    // equivalent intrinsic/innate basic mana ability is already present.
    if !innate_mana_removed
        && obj.zone == Zone::Battlefield
        && !obj.is_phased_out()
        && let Some(ev) = cache.get(id)
        && ev.types.contains(CardTypeMask::LAND)
    {
        let granted_len = extras.map_or(0, |e| e.granted_abilities.len());
        let mut next_synth_idx = innate_total_len.saturating_add(granted_len);
        let db = state.db();
        let basic_subtypes = [
            ("Plains", ManaColor::W),
            ("Island", ManaColor::U),
            ("Swamp", ManaColor::B),
            ("Mountain", ManaColor::R),
            ("Forest", ManaColor::G),
        ];
        for (sub_name, basic_color) in basic_subtypes {
            let Some(sid) = db.subtype_id(sub_name) else {
                continue;
            };
            if !ev.subtypes.contains(&sid) {
                continue;
            }
            let already_present = out.iter().any(|res| {
                if res.is_intrinsic_basic == Some(basic_color) {
                    return true;
                }
                let Some(act) = res.activated.as_deref() else {
                    return false;
                };
                is_activated_mana_ability(act)
                    && act.cost.len() == 1
                    && matches!(act.cost[0], CostDef::TapSelf)
                    && act.effect.iter().any(|step| match &step.op {
                        EffectOp::AddMana { production, .. } => match production {
                            ManaProduction::Fixed { symbols } => symbols.contains(&basic_color),
                            ManaProduction::Choice {
                                kind: ManaChoiceKind::Colors,
                                colors,
                            } => colors.contains(ColorMask::from_mana_color(basic_color)),
                            _ => false,
                        },
                        _ => false,
                    })
            });
            if already_present {
                continue;
            }
            if let Ok(ability_index) = u16::try_from(next_synth_idx) {
                out.push(ResolvedManaAbility {
                    ability_index,
                    is_intrinsic_basic: Some(basic_color),
                    activated: None,
                    mana_as_any_color: false,
                    grant_once_each_turn: false,
                });
                next_synth_idx = next_synth_idx.saturating_add(1);
            }
        }
    }

    out
}

pub(crate) fn is_activated_mana_ability(ab: &mtg_ir::ActivatedAbility) -> bool {
    ab.is_mana || ab.timing == Timing::Mana
}

fn object_might_have_mana_static<F>(state: &GameState, id: ObjectId, pred: F) -> bool
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

/// Checks if a permanent has `StaticBody::ActivateAsThoughHaste` applying to it.
pub(crate) fn has_activate_as_though_haste_cached(
    state: &GameState,
    id: ObjectId,
    cache: &LayerCache,
) -> bool {
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
            if obj.zone == Zone::Command && !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            if obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if !object_might_have_mana_static(state, src_id, |st| {
                matches!(st.body, StaticBody::ActivateAsThoughHaste { .. })
            }) {
                continue;
            }
            let src_ctrl = cache.get(src_id).map_or(obj.controller, |e| e.controller);
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &ctx) {
                if let AbilityDef::Static(st) = ab_ref.ability
                    && let StaticBody::ActivateAsThoughHaste { filter } = &st.static_def.body
                {
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &ctx)
                    {
                        continue;
                    }
                    if matches_object_filter(state, id, filter, &ctx) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn is_cant_activate_blocked_cached(
    state: &GameState,
    id: ObjectId,
    is_mana: bool,
    cache: &LayerCache,
) -> bool {
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
            if obj.zone == Zone::Command && !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            if obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if !object_might_have_mana_static(state, src_id, |st| {
                matches!(
                    st.body,
                    StaticBody::CantActivateAbilities {
                        allow_mana_abilities,
                        ..
                    } if !(is_mana && allow_mana_abilities)
                )
            }) {
                continue;
            }
            let src_ctrl = cache.get(src_id).map_or(obj.controller, |e| e.controller);
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &ctx) {
                if let AbilityDef::Static(st) = ab_ref.ability
                    && let StaticBody::CantActivateAbilities {
                        affected,
                        allow_mana_abilities,
                    } = &st.static_def.body
                {
                    if is_mana && *allow_mana_abilities {
                        continue;
                    }
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &ctx)
                    {
                        continue;
                    }
                    if matches_affected(state, id, affected, &ctx) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub(crate) fn check_activation_restrictions(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    restrictions: &[ActivationRestriction],
    grant_once_each_turn: bool,
) -> bool {
    let Some(pstate) = state.player(player) else {
        return false;
    };
    if grant_once_each_turn
        && pstate
            .once_per_turn_abilities_used()
            .contains(&(object, ability_index))
    {
        return false;
    }
    let phase = state.phase();
    let is_own_turn = state.active_player() == player;
    for r in restrictions {
        let ok = match r {
            ActivationRestriction::OnceEachTurn => !pstate
                .once_per_turn_abilities_used()
                .contains(&(object, ability_index)),
            ActivationRestriction::OncePerGame => !pstate
                .once_per_game_abilities_used()
                .contains(&(object, ability_index)),
            ActivationRestriction::DuringYourTurn => is_own_turn,
            ActivationRestriction::DuringOpponentsTurn => !is_own_turn,
            ActivationRestriction::DuringYourUpkeep => is_own_turn && phase == GamePhase::Upkeep,
            ActivationRestriction::DuringCombat => matches!(
                phase,
                GamePhase::BeginCombat
                    | GamePhase::DeclareAttackers
                    | GamePhase::DeclareBlockers
                    | GamePhase::FirstStrikeDamage
                    | GamePhase::CombatDamage
                    | GamePhase::EndCombat
            ),
            ActivationRestriction::BeforeAttackers => matches!(
                phase,
                GamePhase::Untap
                    | GamePhase::Upkeep
                    | GamePhase::Draw
                    | GamePhase::Main1
                    | GamePhase::BeginCombat
            ),
            ActivationRestriction::AfterAttackers => matches!(
                phase,
                GamePhase::DeclareAttackers
                    | GamePhase::DeclareBlockers
                    | GamePhase::FirstStrikeDamage
                    | GamePhase::CombatDamage
                    | GamePhase::EndCombat
                    | GamePhase::Main2
                    | GamePhase::EndStep
                    | GamePhase::Cleanup
            ),
            ActivationRestriction::BeforeBlockers => matches!(
                phase,
                GamePhase::Untap
                    | GamePhase::Upkeep
                    | GamePhase::Draw
                    | GamePhase::Main1
                    | GamePhase::BeginCombat
                    | GamePhase::DeclareAttackers
            ),
            ActivationRestriction::DuringBlockers => phase == GamePhase::DeclareBlockers,
            ActivationRestriction::AfterBlockers => matches!(
                phase,
                GamePhase::DeclareBlockers
                    | GamePhase::FirstStrikeDamage
                    | GamePhase::CombatDamage
                    | GamePhase::EndCombat
                    | GamePhase::Main2
                    | GamePhase::EndStep
                    | GamePhase::Cleanup
            ),
            ActivationRestriction::AfterCombat => {
                matches!(
                    phase,
                    GamePhase::Main2 | GamePhase::EndStep | GamePhase::Cleanup
                )
            }
            ActivationRestriction::OnlyAsInstant => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn check_activator_who(who: Option<ActivatorWho>, player: PlayerId, controller: PlayerId) -> bool {
    match who.unwrap_or(ActivatorWho::You) {
        ActivatorWho::You => player == controller,
        ActivatorWho::Any => true,
        ActivatorWho::Opponent => player != controller,
    }
}

fn can_tap_self_for_ability_inner_cached(
    state: &GameState,
    id: ObjectId,
    any_activate_haste: bool,
    cache: &LayerCache,
) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.flags.contains(ObjectFlags::TAPPED) {
        return false;
    }
    let Some(ev) = cache.get(id) else {
        return false;
    };
    if ev.types.contains(CardTypeMask::CREATURE) && obj.flags.contains(ObjectFlags::SUMMONING_SICK)
    {
        let haste_kw = state.db().keyword_id("haste");
        let has_haste = haste_kw.is_some_and(|kw| ev.keywords.contains(&kw));
        if !has_haste
            && (!any_activate_haste || !has_activate_as_though_haste_cached(state, id, cache))
        {
            return false;
        }
    }
    true
}

fn can_untap_self_for_ability_cached(
    state: &GameState,
    id: ObjectId,
    via_symbol: bool,
    cache: &LayerCache,
) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if !obj.flags.contains(ObjectFlags::TAPPED) {
        return false;
    }
    if via_symbol {
        let Some(ev) = cache.get(id) else {
            return false;
        };
        if ev.types.contains(CardTypeMask::CREATURE)
            && obj.flags.contains(ObjectFlags::SUMMONING_SICK)
        {
            let haste_kw = state.db().keyword_id("haste");
            let has_haste = haste_kw.is_some_and(|kw| ev.keywords.contains(&kw));
            if !has_haste && !has_activate_as_though_haste_cached(state, id, cache) {
                return false;
            }
        }
    }
    true
}

fn is_treasure_object_cached(state: &GameState, id: ObjectId, cache: &LayerCache) -> bool {
    let Some(ev) = cache.get(id) else {
        return false;
    };
    if let Some(t_id) = state.db().subtype_id("Treasure")
        && ev.subtypes.contains(&t_id)
    {
        return true;
    }
    false
}

fn land_producible_colors_with_depth_cached(
    state: &GameState,
    land_id: ObjectId,
    depth: u8,
    cache: &LayerCache,
) -> ColorMask {
    if depth >= MAX_MANA_PRODUCED_DEPTH {
        return ColorMask::empty();
    }
    let Some(obj) = state.object(land_id) else {
        return ColorMask::empty();
    };
    if obj.zone != Zone::Battlefield || obj.is_phased_out() {
        return ColorMask::empty();
    }
    let Some(ev) = cache.get(land_id) else {
        return ColorMask::empty();
    };
    if !ev.types.contains(CardTypeMask::LAND) {
        return ColorMask::empty();
    }
    let ctx = EvalContext::for_source(ev.controller, land_id).with_board(BoardView::Cached(cache));
    let mut mask = ColorMask::empty();
    for res_ab in resolve_object_activated_abilities_cached(state, land_id, &ctx, cache) {
        if let Some(c) = res_ab.is_intrinsic_basic {
            mask |= ColorMask::from_mana_color(c);
            continue;
        }
        let Some(act) = res_ab.activated.as_deref() else {
            continue;
        };
        if !is_activated_mana_ability(act) {
            continue;
        }
        for step in &act.effect {
            if let EffectOp::AddMana { production, .. } = &step.op {
                mask |= production_color_mask_cached(
                    state,
                    ev.controller,
                    land_id,
                    production,
                    depth + 1,
                    cache,
                );
            }
        }
    }
    mask
}

fn choice_colors_for_kind_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    kind: ManaChoiceKind,
    colors: ColorMask,
    depth: u8,
    cache: &LayerCache,
) -> SmallVec<[ManaColor; 6]> {
    match kind {
        ManaChoiceKind::Any => SmallVec::from_slice(&FIVE_COLORS),
        ManaChoiceKind::Colors => {
            let cs = colors_from_mask(colors);
            if cs.is_empty() {
                SmallVec::from_slice(&FIVE_COLORS)
            } else {
                cs
            }
        }
        ManaChoiceKind::ChosenColor => {
            let mut cs = SmallVec::new();
            if let Some(rare) = state.rare(source) {
                if let Some(cm) = rare.chosen_color {
                    cs.extend(colors_from_mask(cm));
                }
                for &mc in &rare.chosen_colors {
                    if !cs.contains(&mc) {
                        cs.push(mc);
                    }
                }
            }
            cs
        }
        ManaChoiceKind::CommanderIdentity => {
            let ci = state
                .player(player)
                .map_or(ColorMask::empty(), |p| p.commander_color_identity);
            let cs = colors_from_mask(ci & ColorMask::ALL_COLORS);
            if cs.is_empty() {
                SmallVec::from_slice(&[ManaColor::C])
            } else {
                cs
            }
        }
        ManaChoiceKind::OpponentLandColors => {
            if depth >= MAX_MANA_PRODUCED_DEPTH {
                return SmallVec::new();
            }
            let mut mask = ColorMask::empty();
            for p in state.players() {
                if p.lost || p.id == player {
                    continue;
                }
                for &id in &p.battlefield {
                    mask |= land_producible_colors_with_depth_cached(state, id, depth + 1, cache);
                }
            }
            colors_from_mask(mask & ColorMask::ALL_COLORS)
        }
        ManaChoiceKind::ControlledLandProducedTypes
        | ManaChoiceKind::ControlledLandProducedColors => {
            if depth >= MAX_MANA_PRODUCED_DEPTH {
                return SmallVec::new();
            }
            let mut mask = ColorMask::empty();
            if let Some(p) = state.player(player) {
                for &id in &p.battlefield {
                    if id == source && depth > 0 {
                        continue;
                    }
                    mask |= land_producible_colors_with_depth_cached(state, id, depth + 1, cache);
                }
            }
            let filter = if matches!(kind, ManaChoiceKind::ControlledLandProducedColors) {
                mask & ColorMask::ALL_COLORS
            } else {
                mask
            };
            colors_from_mask(filter)
        }
        ManaChoiceKind::EventProducedTypes | ManaChoiceKind::EventProducedColors => {
            if !colors.is_empty() {
                colors_from_mask(colors)
            } else {
                SmallVec::from_slice(&FIVE_COLORS)
            }
        }
    }
}

fn production_color_mask_cached(
    state: &GameState,
    player: PlayerId,
    source: ObjectId,
    prod: &ManaProduction,
    depth: u8,
    cache: &LayerCache,
) -> ColorMask {
    match prod {
        ManaProduction::Fixed { symbols } => {
            let mut m = ColorMask::empty();
            for &s in symbols {
                m |= ColorMask::from_mana_color(s);
            }
            m
        }
        ManaProduction::Choice { kind, colors } => {
            let mut m = ColorMask::empty();
            for c in
                choice_colors_for_kind_cached(state, player, source, *kind, *colors, depth, cache)
            {
                m |= ColorMask::from_mana_color(c);
            }
            m
        }
        ManaProduction::Bundles { options } => {
            let mut m = ColorMask::empty();
            for b in options {
                for &c in b {
                    m |= ColorMask::from_mana_color(c);
                }
            }
            m
        }
        ManaProduction::Combination { any_color, colors } => {
            if *any_color {
                ColorMask::ALL_COLORS
            } else {
                *colors
            }
        }
        ManaProduction::OrChosenColor { colors } | ManaProduction::WithChosenColor { colors } => {
            let mut m = *colors;
            if let Some(cm) = state.rare(source).and_then(|r| r.chosen_color) {
                m |= cm;
            }
            m
        }
        ManaProduction::FromExiledColors => {
            let mut m = ColorMask::empty();
            let ctx = EvalContext::for_source(player, source).with_board(BoardView::Cached(cache));
            for p in state.players() {
                for &eid in &p.exile {
                    if state.rare(eid).is_some_and(|r| r.exiled_by == Some(source))
                        && let Some(e_obj) = state.object(eid)
                    {
                        let ev_chars = crate::eval::off_battlefield_characteristics(
                            state,
                            eid,
                            e_obj,
                            Zone::Exile,
                            &ctx,
                        );
                        m |= ev_chars.colors & ColorMask::ALL_COLORS;
                    }
                }
            }
            m
        }
        ManaProduction::PerColorControlled => {
            let mut m = ColorMask::empty();
            if let Some(p) = state.player(player) {
                for &bid in &p.battlefield {
                    if let Some(ev) = cache.get(bid) {
                        m |= ev.colors & ColorMask::ALL_COLORS;
                    }
                }
            }
            m
        }
        ManaProduction::LandProducedTypes => choice_colors_for_kind_cached(
            state,
            player,
            source,
            ManaChoiceKind::ControlledLandProducedTypes,
            ColorMask::empty(),
            depth,
            cache,
        )
        .into_iter()
        .fold(ColorMask::empty(), |acc, c| {
            acc | ColorMask::from_mana_color(c)
        }),
        ManaProduction::ChosenColorAmount { .. } => state
            .rare(source)
            .and_then(|r| r.chosen_color)
            .unwrap_or(ColorMask::empty()),
        ManaProduction::FromProducedEvent => ColorMask::empty(),
    }
}

/// Enumerates all mana ability candidates on `player`'s battlefield (and any opponent permanent
/// granting `ActivatorWho::Any/Opponent`).
///
/// Note: `extra_mana_cost` (e.g. filter lands / signets) is populated on each candidate rather than
/// rejecting it here so `auto_tap` can order feeder mana sources before filter lands; use
/// [`can_activate_mana_ability`] to check immediate activation against the current mana pool.
pub fn enumerate_mana_abilities(state: &GameState, player: PlayerId) -> Vec<ManaAbilityCandidate> {
    enumerate_mana_abilities_cached(state, player, state.evaluate_board())
}

pub(crate) fn enumerate_mana_abilities_cached(
    state: &GameState,
    player: PlayerId,
    cache: &LayerCache,
) -> Vec<ManaAbilityCandidate> {
    let mut out = Vec::new();
    let Some(pstate) = state.player(player) else {
        return out;
    };
    if pstate.lost {
        return out;
    }

    let activate_own_turn_only = cache
        .player(player)
        .is_some_and(|ep| ep.activate_only_during_own_turn);
    if activate_own_turn_only && state.active_player() != player {
        return out;
    }

    // Single zero-alloc pre-scan over active battlefield permanents & command emblems to detect
    // whether any global cant-activate, mana-multiplier, or activate-as-though-haste static exists.
    let mut any_cant_activate_mana = false;
    let mut any_tap_mana_multiplier = false;
    let mut any_activate_as_though_haste = false;
    'prescan: for p in state.players() {
        if p.lost {
            continue;
        }
        for &src_id in p.battlefield.iter().chain(p.command.iter()) {
            let Some(obj) = state.object(src_id) else {
                continue;
            };
            if obj.is_face_down() {
                continue;
            }
            if obj.zone == Zone::Battlefield && obj.is_phased_out() {
                continue;
            }
            if obj.zone == Zone::Command && !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            if obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if state
                .rare(src_id)
                .is_some_and(|r| r.copiable.is_some() || !r.merged_components.is_empty())
            {
                any_cant_activate_mana = true;
                any_tap_mana_multiplier = true;
                any_activate_as_though_haste = true;
                break 'prescan;
            }
            let Some(card) = state.db().get(obj.card_def) else {
                continue;
            };
            for f in &card.faces {
                for ab in &f.abilities {
                    let AbilityDef::Static(st) = ab else {
                        continue;
                    };
                    match &st.static_def.body {
                        StaticBody::CantActivateAbilities {
                            allow_mana_abilities,
                            ..
                        } if !*allow_mana_abilities => {
                            any_cant_activate_mana = true;
                        }
                        StaticBody::TapForManaMultiplier { .. } => {
                            any_tap_mana_multiplier = true;
                        }
                        StaticBody::ActivateAsThoughHaste { .. } => {
                            any_activate_as_though_haste = true;
                        }
                        _ => {}
                    }
                    if any_cant_activate_mana
                        && any_tap_mana_multiplier
                        && any_activate_as_though_haste
                    {
                        break 'prescan;
                    }
                }
            }
        }
    }

    for p in state.players() {
        if p.lost {
            continue;
        }
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let Some(ev) = cache.get(id) else {
                continue;
            };
            let extras = cache.extra(id);
            let has_granted = extras.is_some_and(|e| !e.granted_abilities.is_empty());
            let has_rare_override = state
                .rare(id)
                .is_some_and(|r| r.copiable.is_some() || !r.merged_components.is_empty());
            if ev.controller != player {
                let has_opp_mana = has_granted
                    || has_rare_override
                    || (!obj.is_face_down()
                        && state.db().get(obj.card_def).is_some_and(|card| {
                            card.faces.iter().any(|f| {
                                f.abilities.iter().any(|ab| {
                                    matches!(
                                        ab,
                                        AbilityDef::Activated(act)
                                            if is_activated_mana_ability(act)
                                                && !matches!(
                                                    act.activator_who.unwrap_or(ActivatorWho::You),
                                                    ActivatorWho::You
                                                )
                                    )
                                })
                            })
                        }));
                if !has_opp_mana {
                    continue;
                }
            } else if !ev.types.contains(CardTypeMask::LAND) && !has_granted && !has_rare_override {
                let has_innate_mana = !obj.is_face_down()
                    && state.db().get(obj.card_def).is_some_and(|card| {
                        card.faces.iter().any(|f| {
                            f.abilities.iter().any(|ab| {
                                matches!(ab, AbilityDef::Activated(act) if is_activated_mana_ability(act))
                            })
                        })
                    });
                if !has_innate_mana {
                    continue;
                }
            }
            let ctx =
                EvalContext::for_source(ev.controller, id).with_board(BoardView::Cached(cache));
            let abilities = resolve_object_activated_abilities_cached(state, id, &ctx, cache);

            for res_ab in abilities {
                if let Some(basic_color) = res_ab.is_intrinsic_basic {
                    if ev.controller != player {
                        continue;
                    }
                    if any_cant_activate_mana
                        && is_cant_activate_blocked_cached(state, id, true, cache)
                    {
                        continue;
                    }
                    if !can_tap_self_for_ability_inner_cached(
                        state,
                        id,
                        any_activate_as_though_haste,
                        cache,
                    ) {
                        continue;
                    }
                    let mult = if any_tap_mana_multiplier {
                        triggers::tap_for_mana_multiplier(state, player, id)
                    } else {
                        1
                    };
                    let mut amounts = [0u32; 6];
                    amounts[color_to_index(basic_color)] = mult;
                    out.push(ManaAbilityCandidate {
                        object: id,
                        ability_index: res_ab.ability_index,
                        is_intrinsic_basic: Some(basic_color),
                        produces_colors: ColorMask::from_mana_color(basic_color),
                        produces_amounts: amounts,
                        max_any_color_amount: 0,
                        bundle_options: Vec::new(),
                        restriction: None,
                        has_delayed_rider: false,
                        extra_mana_cost: None,
                        requires_tap: true,
                        requires_untap: false,
                        has_sacrifice_cost: false,
                        has_sacrifice_self: false,
                        has_sacrifice_other: false,
                        has_life_cost: 0,
                        has_energy_cost: 0,
                        has_non_mana_downside: false,
                        only_as_instant: false,
                        is_snow_source: ev.supertypes.contains(SupertypeMask::SNOW),
                        is_treasure: false,
                        priority_tier: 1,
                    });
                    continue;
                }

                let Some(act) = res_ab.activated.as_deref() else {
                    continue;
                };
                if !is_activated_mana_ability(act) {
                    continue;
                }
                if !check_activator_who(act.activator_who, player, ev.controller) {
                    continue;
                }
                if any_cant_activate_mana && is_cant_activate_blocked_cached(state, id, true, cache)
                {
                    continue;
                }
                if !check_activation_restrictions(
                    state,
                    player,
                    id,
                    res_ab.ability_index,
                    &act.activation_restrictions,
                    res_ab.grant_once_each_turn,
                ) {
                    continue;
                }
                if let Some(cond) = act.activation_condition.as_deref() {
                    let act_ctx =
                        EvalContext::for_source(player, id).with_board(BoardView::Cached(cache));
                    if !evaluate_condition(state, cond, &act_ctx) {
                        continue;
                    }
                }

                let mut requires_tap = false;
                let mut requires_untap = false;
                let mut untap_via_symbol = false;
                let mut has_sacrifice_cost = false;
                let mut has_sacrifice_self = false;
                let mut has_sacrifice_other = false;
                let mut has_life_cost: u32 = 0;
                let mut has_energy_cost: u32 = 0;
                let mut has_non_mana_downside = false;
                let mut extra_mana_cost: Option<ManaCost> = None;
                let mut non_mana_costs: SmallVec<[CostDef; 4]> = SmallVec::new();

                let cost_ctx =
                    EvalContext::for_source(player, id).with_board(BoardView::Cached(cache));
                for c in &act.cost {
                    match c {
                        CostDef::TapSelf => requires_tap = true,
                        CostDef::UntapSelf { via_symbol } => {
                            requires_untap = true;
                            untap_via_symbol = *via_symbol;
                        }
                        CostDef::Mana { cost, .. } => {
                            if let Some(prev) = &mut extra_mana_cost {
                                prev.generic = prev.generic.saturating_add(cost.generic);
                                prev.pips.extend(cost.pips.iter().copied());
                            } else {
                                extra_mana_cost = Some(cost.clone());
                            }
                        }
                        CostDef::Sacrifice {
                            target,
                            attached_creature,
                            ..
                        } => {
                            has_sacrifice_cost = true;
                            if matches!(target, mtg_ir::CostObjectTarget::SelfObj)
                                && !*attached_creature
                            {
                                has_sacrifice_self = true;
                            } else {
                                has_sacrifice_other = true;
                            }
                            non_mana_costs.push(c.clone());
                        }
                        CostDef::PayLife {
                            amount,
                            half_life_rounded_up,
                        } => {
                            let amt = if *half_life_rounded_up {
                                ((pstate.life.max(0) + 1) / 2) as u32
                            } else {
                                evaluate_amount(state, amount, &cost_ctx).max(0) as u32
                            };
                            has_life_cost = has_life_cost.saturating_add(amt);
                            non_mana_costs.push(c.clone());
                        }
                        CostDef::PayEnergy { count } => {
                            let amt = evaluate_amount(state, count, &cost_ctx).max(0) as u32;
                            has_energy_cost = has_energy_cost.saturating_add(amt);
                            non_mana_costs.push(c.clone());
                        }
                        _ => {
                            has_non_mana_downside = true;
                            non_mana_costs.push(c.clone());
                        }
                    }
                }

                if requires_tap
                    && !can_tap_self_for_ability_inner_cached(
                        state,
                        id,
                        any_activate_as_though_haste,
                        cache,
                    )
                {
                    continue;
                }
                if requires_untap
                    && !can_untap_self_for_ability_cached(state, id, untap_via_symbol, cache)
                {
                    continue;
                }
                if !non_mana_costs.is_empty() {
                    let full_non_mana: SmallVec<[CostDef; 6]> = act
                        .cost
                        .iter()
                        .filter(|c| !matches!(c, CostDef::Mana { .. }))
                        .cloned()
                        .collect();
                    if !costs::can_pay_non_mana_costs_cached(
                        state,
                        player,
                        id,
                        &full_non_mana,
                        None,
                        cache,
                    ) {
                        continue;
                    }
                    let constraints = costs::auto_tap_constraints_for_costs_cached(
                        state,
                        player,
                        id,
                        &full_non_mana,
                        None,
                        cache,
                    );
                    if !constraints.is_satisfiable_cached(state, player, cache) {
                        continue;
                    }
                }

                let mult = if requires_tap && any_tap_mana_multiplier {
                    triggers::tap_for_mana_multiplier(state, player, id)
                } else {
                    1
                };
                let mut produces_colors = ColorMask::empty();
                let mut produces_amounts = [0u32; 6];
                let mut max_any_color_amount: u32 = 0;
                let mut bundle_options: Vec<SmallVec<[ManaColor; 3]>> = Vec::new();
                let mut restriction: Option<ManaSpendingRestriction> = None;
                let mut has_tag_restriction = false;
                let mut has_delayed_rider = false;

                let eff_ctx =
                    EvalContext::for_source(player, id).with_board(BoardView::Cached(cache));
                for step in &act.effect {
                    if let Some(mtg_ir::StepGate::Condition { condition }) = &step.gate
                        && !evaluate_condition(state, condition, &eff_ctx)
                    {
                        continue;
                    }
                    let EffectOp::AddMana {
                        production,
                        restriction: tag,
                        restriction_spec,
                        delayed_rider,
                        ..
                    } = &step.op
                    else {
                        continue;
                    };
                    has_delayed_rider |= delayed_rider.is_some();
                    if let Some(r_tag) = tag {
                        has_tag_restriction = true;
                        let tag_spec = restriction_for_tag(*r_tag);
                        restriction = Some(merge_restrictions(restriction.as_ref(), &tag_spec));
                    }
                    if let Some(spec) = restriction_spec {
                        let mut merged = spec.as_ref().clone();
                        if merged.source_chosen_subtype.is_none()
                            && let Some(st_id) = state.rare(id).and_then(|r| r.chosen_subtype)
                            && let Some(st_name) = state.db().subtype_name(st_id)
                        {
                            merged.source_chosen_subtype = Some(st_name.to_string());
                        }
                        restriction = Some(merge_restrictions(restriction.as_ref(), &merged));
                    }

                    let base_mult = if add_mana_ignores_step_amount(production) {
                        1
                    } else {
                        step.amount
                            .as_deref()
                            .map_or(1u32, |a| evaluate_amount(state, a, &eff_ctx).max(0) as u32)
                    };
                    let step_mult = base_mult.saturating_mul(mult);
                    if step_mult == 0 {
                        continue;
                    }
                    match production {
                        ManaProduction::Fixed { symbols } => {
                            for &s in symbols {
                                produces_colors |= ColorMask::from_mana_color(s);
                                let idx = color_to_index(s);
                                produces_amounts[idx] =
                                    produces_amounts[idx].saturating_add(step_mult);
                            }
                        }
                        ManaProduction::Choice { kind, colors } => {
                            let choices = choice_colors_for_kind_cached(
                                state, player, id, *kind, *colors, 0, cache,
                            );
                            for c in choices {
                                produces_colors |= ColorMask::from_mana_color(c);
                            }
                            max_any_color_amount = max_any_color_amount.saturating_add(step_mult);
                        }
                        ManaProduction::Bundles { options } => {
                            for b in options {
                                let mut scaled: SmallVec<[ManaColor; 3]> = SmallVec::new();
                                for &c in b {
                                    produces_colors |= ColorMask::from_mana_color(c);
                                    for _ in 0..step_mult {
                                        scaled.push(c);
                                    }
                                }
                                bundle_options.push(scaled);
                            }
                        }
                        ManaProduction::Combination { any_color, colors } => {
                            let m = if *any_color {
                                ColorMask::ALL_COLORS
                            } else {
                                *colors
                            };
                            produces_colors |= m;
                            max_any_color_amount = max_any_color_amount.saturating_add(step_mult);
                        }
                        ManaProduction::OrChosenColor { colors } => {
                            let mut m = *colors;
                            if let Some(cm) = state.rare(id).and_then(|r| r.chosen_color) {
                                m |= cm;
                            }
                            produces_colors |= m;
                            max_any_color_amount = max_any_color_amount.saturating_add(step_mult);
                        }
                        ManaProduction::WithChosenColor { colors } => {
                            for c in colors_from_mask(*colors) {
                                produces_colors |= ColorMask::from_mana_color(c);
                                let idx = color_to_index(c);
                                produces_amounts[idx] =
                                    produces_amounts[idx].saturating_add(step_mult);
                            }
                            if let Some(cm) = state.rare(id).and_then(|r| r.chosen_color) {
                                for c in colors_from_mask(cm) {
                                    produces_colors |= ColorMask::from_mana_color(c);
                                    let idx = color_to_index(c);
                                    produces_amounts[idx] =
                                        produces_amounts[idx].saturating_add(step_mult);
                                }
                            }
                        }
                        ManaProduction::FromExiledColors => {
                            let m = production_color_mask_cached(
                                state, player, id, production, 0, cache,
                            );
                            produces_colors |= m;
                            max_any_color_amount = max_any_color_amount.saturating_add(step_mult);
                        }
                        ManaProduction::PerColorControlled => {
                            let m = production_color_mask_cached(
                                state, player, id, production, 0, cache,
                            );
                            for c in colors_from_mask(m) {
                                produces_colors |= ColorMask::from_mana_color(c);
                                let idx = color_to_index(c);
                                produces_amounts[idx] =
                                    produces_amounts[idx].saturating_add(step_mult);
                            }
                        }
                        ManaProduction::LandProducedTypes => {
                            let m = production_color_mask_cached(
                                state, player, id, production, 0, cache,
                            );
                            produces_colors |= m;
                            max_any_color_amount = max_any_color_amount.saturating_add(step_mult);
                        }
                        ManaProduction::ChosenColorAmount { amount } => {
                            if let Some(cm) = state.rare(id).and_then(|r| r.chosen_color) {
                                for c in colors_from_mask(cm) {
                                    let n = evaluate_chosen_color_amount(
                                        state,
                                        amount,
                                        ColorMask::from_mana_color(c),
                                        &eff_ctx,
                                    )
                                    .max(0) as u32;
                                    produces_colors |= ColorMask::from_mana_color(c);
                                    let idx = color_to_index(c);
                                    produces_amounts[idx] = produces_amounts[idx]
                                        .saturating_add(n.saturating_mul(step_mult));
                                }
                            }
                        }
                        ManaProduction::FromProducedEvent => {}
                    }
                }

                let only_as_instant = act
                    .activation_restrictions
                    .contains(&ActivationRestriction::OnlyAsInstant);
                let actual_is_treasure = is_treasure_object_cached(state, id, cache);
                let is_sac_artifact_or_treasure = !ev.types.contains(CardTypeMask::CREATURE)
                    && (actual_is_treasure || ev.types.contains(CardTypeMask::ARTIFACT));

                let priority_tier =
                    if has_sacrifice_self && !has_sacrifice_other && is_sac_artifact_or_treasure {
                        4
                    } else if has_sacrifice_cost
                        || extra_mana_cost.is_some()
                        || has_life_cost > 0
                        || has_energy_cost > 0
                        || has_non_mana_downside
                        || ev.types.contains(CardTypeMask::CREATURE)
                    {
                        3
                    } else if has_tag_restriction
                        || restriction.as_ref().is_some_and(|r| {
                            r.allow_spells.is_some() || r.allow_abilities.is_some()
                        })
                    {
                        0
                    } else if produces_colors.color_count() <= 1 && max_any_color_amount == 0 {
                        1
                    } else {
                        2
                    };

                out.push(ManaAbilityCandidate {
                    object: id,
                    ability_index: res_ab.ability_index,
                    is_intrinsic_basic: None,
                    produces_colors,
                    produces_amounts,
                    max_any_color_amount,
                    bundle_options,
                    restriction,
                    has_delayed_rider,
                    extra_mana_cost,
                    requires_tap,
                    requires_untap,
                    has_sacrifice_cost,
                    has_sacrifice_self,
                    has_sacrifice_other,
                    has_life_cost,
                    has_energy_cost,
                    has_non_mana_downside,
                    only_as_instant,
                    is_snow_source: ev.supertypes.contains(SupertypeMask::SNOW),
                    is_treasure: actual_is_treasure,
                    priority_tier,
                });
            }
        }
    }

    out
}

fn restriction_for_tag(tag: ManaRestrictionTag) -> ManaSpendingRestriction {
    match tag {
        ManaRestrictionTag::ArtifactSpellOrNonSpell => ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::ARTIFACT,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            allow_abilities: Some(AllowAbilitiesRestriction {
                any_ability: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn merge_restrictions(
    existing: Option<&ManaSpendingRestriction>,
    next: &ManaSpendingRestriction,
) -> ManaSpendingRestriction {
    let Some(prev) = existing else {
        return next.clone();
    };
    ManaSpendingRestriction {
        allow_spells: next
            .allow_spells
            .clone()
            .or_else(|| prev.allow_spells.clone()),
        allow_abilities: next
            .allow_abilities
            .clone()
            .or_else(|| prev.allow_abilities.clone()),
        source_chosen_subtype: next
            .source_chosen_subtype
            .clone()
            .or_else(|| prev.source_chosen_subtype.clone()),
        disallow_generic: prev.disallow_generic || next.disallow_generic,
        on_spend_cant_be_countered: prev.on_spend_cant_be_countered
            || next.on_spend_cant_be_countered,
        on_spend_commander_bonus_counters: prev.on_spend_commander_bonus_counters
            || next.on_spend_commander_bonus_counters,
    }
}

pub fn can_activate_mana_ability(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
) -> bool {
    can_activate_mana_ability_cached(state, player, object, ability_index, state.evaluate_board())
}

pub(crate) fn can_activate_mana_candidate_now_cached(
    state: &GameState,
    player: PlayerId,
    cand: &ManaAbilityCandidate,
    cache: &LayerCache,
) -> bool {
    if let Some(extra) = &cand.extra_mana_cost {
        let ctx = ManaSpendContext::for_generic_cost(player);
        if !can_pay_mana_from_pool_cached(state, player, extra, &ctx, cache) {
            return false;
        }
    }
    true
}

pub(crate) fn can_activate_mana_ability_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    cache: &LayerCache,
) -> bool {
    let candidates = enumerate_mana_abilities_cached(state, player, cache);
    let Some(cand) = candidates
        .into_iter()
        .find(|c| c.object == object && c.ability_index == ability_index)
    else {
        return false;
    };
    can_activate_mana_candidate_now_cached(state, player, &cand, cache)
}

pub fn activate_mana_ability(
    state: &mut GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    chosen_color: Option<ManaColor>,
) -> Result<ManaActivationOutcome, EngineError> {
    activate_mana_ability_with_bundle(state, player, object, ability_index, chosen_color, None)
}

enum ManaPreActivation {
    IntrinsicBasic {
        color: ManaColor,
        is_land: bool,
    },
    ChoiceRequired {
        single_colors: SmallVec<[ManaColor; 6]>,
        bundle_options: Vec<SmallVec<[ManaColor; 3]>>,
    },
    Execute {
        act: Box<mtg_ir::ActivatedAbility>,
        is_land: bool,
        mana_as_any: bool,
    },
}

pub(crate) fn activate_mana_ability_with_bundle(
    state: &mut GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    chosen_color: Option<ManaColor>,
    bundle_index: Option<usize>,
) -> Result<ManaActivationOutcome, EngineError> {
    let pre = {
        let cache = state.evaluate_board();
        if !can_activate_mana_ability_cached(state, player, object, ability_index, cache) {
            return Err(EngineError::IllegalAction(format!(
                "cannot activate mana ability {ability_index} on {object:?}"
            )));
        }
        let Some(ev) = cache.get(object) else {
            return Err(EngineError::IllegalAction("mana source missing".into()));
        };
        let is_land = ev.types.contains(CardTypeMask::LAND);
        let ctx =
            EvalContext::for_source(ev.controller, object).with_board(BoardView::Cached(cache));
        let abilities = resolve_object_activated_abilities_cached(state, object, &ctx, cache);
        let Some(res_ab) = abilities
            .into_iter()
            .find(|a| a.ability_index == ability_index)
        else {
            return Err(EngineError::IllegalAction("mana ability not found".into()));
        };
        if let Some(color) = res_ab.is_intrinsic_basic {
            ManaPreActivation::IntrinsicBasic { color, is_land }
        } else {
            let Some(act) = res_ab.activated.as_deref() else {
                return Err(EngineError::IllegalAction(
                    "missing activated ability".into(),
                ));
            };
            if let Some(bidx) = bundle_index {
                let eff_ctx =
                    EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
                for step in &act.effect {
                    if let Some(mtg_ir::StepGate::Condition { condition }) = &step.gate
                        && !evaluate_condition(state, condition, &eff_ctx)
                    {
                        continue;
                    }
                    if let EffectOp::AddMana {
                        production: ManaProduction::Bundles { options },
                        ..
                    } = &step.op
                        && bidx >= options.len()
                    {
                        return Err(EngineError::IllegalAction(format!(
                            "mana bundle index {bidx} out of range (len {})",
                            options.len()
                        )));
                    }
                }
            }
            if chosen_color.is_none()
                && bundle_index.is_none()
                && let Some((single_colors, bundle_options)) =
                    detect_mana_choice_options_cached(state, player, object, act, cache)
            {
                let total_options = if !bundle_options.is_empty() {
                    bundle_options.len()
                } else {
                    single_colors.len()
                };
                if total_options > 1 {
                    ManaPreActivation::ChoiceRequired {
                        single_colors,
                        bundle_options,
                    }
                } else {
                    ManaPreActivation::Execute {
                        act: Box::new(act.clone()),
                        is_land,
                        mana_as_any: res_ab.mana_as_any_color,
                    }
                }
            } else {
                ManaPreActivation::Execute {
                    act: Box::new(act.clone()),
                    is_land,
                    mana_as_any: res_ab.mana_as_any_color,
                }
            }
        }
    };

    match pre {
        ManaPreActivation::IntrinsicBasic {
            color: basic_color,
            is_land,
        } => {
            // Intrinsic basic mana ability: {T}: Add {basic_color}
            if let Some(obj) = state.object_mut(object) {
                obj.flags.insert(ObjectFlags::TAPPED);
            }
            state.mark_dirty(DirtyMask::TAPPED);
            emit_event(
                state,
                AtomicEventKind::TapStateChanged {
                    object,
                    now_tapped: true,
                    actor: Some(player),
                },
            );
            let mult = triggers::tap_for_mana_multiplier(state, player, object);
            let prod = ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[basic_color]),
            };
            let added = add_mana_from_production(
                state,
                player,
                Some(object),
                &prod,
                None,
                None,
                false,
                None,
                chosen_color,
                mult,
            )?;
            let total_amt = added.len() as u32;
            emit_event(
                state,
                AtomicEventKind::AbilityActivated {
                    player,
                    source: object,
                    stack_item: None,
                    is_mana: true,
                    is_loyalty: false,
                    life_paid: 0,
                },
            );
            emit_event(
                state,
                AtomicEventKind::TappedForMana {
                    player,
                    source: object,
                    produced_colors: added.clone(),
                    amount: total_amt,
                },
            );
            if is_land {
                emit_event(
                    state,
                    AtomicEventKind::ManaAddedByLandAbility {
                        player,
                        source: object,
                        produced_colors: added,
                        amount: total_amt,
                    },
                );
            }
            triggers::flush_inline_triggers(state);
            Ok(ManaActivationOutcome::Produced)
        }
        ManaPreActivation::ChoiceRequired {
            single_colors,
            bundle_options,
        } => {
            let total_options = if !bundle_options.is_empty() {
                bundle_options.len()
            } else {
                single_colors.len()
            };
            let prompt_id = state.allocate_prompt_id();
            let resume_decision = state.decision().clone();
            let resume_priority_player = state.priority_player();
            let request = ChoiceRequest::ChooseMode {
                prompt_id,
                option_count: total_options as u16,
                min: 1,
                max: 1,
            };
            state.set_pending_prompt(Some(PendingPromptContext {
                prompt_id,
                chooser: player,
                request: request.clone(),
                owner: PromptOwner::ManaAbility(ManaAbilityPromptState {
                    source: object,
                    ability_index,
                    controller: player,
                    single_colors,
                    bundle_options,
                    resume_decision,
                    resume_priority_player,
                }),
            }));
            state.set_decision(DecisionState::Prompt { request });
            state.set_priority_player(player);
            Ok(ManaActivationOutcome::Paused {
                prompt_id,
                chooser: player,
            })
        }
        ManaPreActivation::Execute {
            act,
            is_land,
            mana_as_any,
        } => {
            let _ = mana_as_any; // preserved on ManaSpendContext during casting/activation
            execute_mana_ability_body_with_bundle_idx(
                state,
                player,
                object,
                ability_index,
                &act,
                is_land,
                chosen_color,
                bundle_index,
            )
        }
    }
}

type ManaChoiceOptions = (SmallVec<[ManaColor; 6]>, Vec<SmallVec<[ManaColor; 3]>>);

fn detect_mana_choice_options_cached(
    state: &GameState,
    player: PlayerId,
    object: ObjectId,
    act: &mtg_ir::ActivatedAbility,
    cache: &LayerCache,
) -> Option<ManaChoiceOptions> {
    let eff_ctx = EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
    for step in &act.effect {
        if let Some(mtg_ir::StepGate::Condition { condition }) = &step.gate
            && !evaluate_condition(state, condition, &eff_ctx)
        {
            continue;
        }
        let EffectOp::AddMana { production, .. } = &step.op else {
            continue;
        };
        match production {
            ManaProduction::Choice { kind, colors } => {
                let cs =
                    choice_colors_for_kind_cached(state, player, object, *kind, *colors, 0, cache);
                if cs.len() > 1 {
                    return Some((cs, Vec::new()));
                }
            }
            ManaProduction::Bundles { options } => {
                if options.len() > 1 {
                    return Some((SmallVec::new(), options.clone()));
                }
            }
            ManaProduction::Combination { any_color, colors } => {
                let cs = if *any_color {
                    SmallVec::from_slice(&FIVE_COLORS)
                } else {
                    colors_from_mask(*colors)
                };
                if cs.len() > 1 {
                    return Some((cs, Vec::new()));
                }
            }
            ManaProduction::OrChosenColor { colors } => {
                let mut m = *colors;
                if let Some(cm) = state.rare(object).and_then(|r| r.chosen_color) {
                    m |= cm;
                }
                let cs = colors_from_mask(m);
                if cs.len() > 1 {
                    return Some((cs, Vec::new()));
                }
            }
            ManaProduction::LandProducedTypes | ManaProduction::FromExiledColors => {
                let m = production_color_mask_cached(state, player, object, production, 0, cache);
                let cs = colors_from_mask(m);
                if cs.len() > 1 {
                    return Some((cs, Vec::new()));
                }
            }
            ManaProduction::ChosenColorAmount { .. }
                if state.rare(object).and_then(|r| r.chosen_color).is_none() =>
            {
                return Some((SmallVec::from_slice(&FIVE_COLORS), Vec::new()));
            }
            _ => {}
        }
    }
    None
}

pub fn resume_mana_color_choice(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<ManaActivationOutcome, EngineError> {
    let Some(pending) = state.pending_prompt() else {
        return Err(EngineError::IllegalAction("no pending prompt".into()));
    };
    if pending.prompt_id != prompt_id || pending.chooser != actor {
        return Err(EngineError::IllegalAction(
            "mana color choice prompt mismatch".into(),
        ));
    }
    let PromptOwner::ManaAbility(mp) = &pending.owner else {
        return Err(EngineError::IllegalAction(
            "pending prompt is not a mana ability color choice".into(),
        ));
    };
    let ChoiceAnswer::PickMode { index } = answer else {
        return Err(EngineError::IllegalAction(
            "expected PickMode answer for mana color choice".into(),
        ));
    };
    let idx = usize::from(index);
    let chosen_color = if !mp.bundle_options.is_empty() {
        let Some(bundle) = mp.bundle_options.get(idx) else {
            return Err(EngineError::IllegalAction(
                "bundle option out of range".into(),
            ));
        };
        bundle.first().copied()
    } else {
        let Some(&c) = mp.single_colors.get(idx) else {
            return Err(EngineError::IllegalAction(
                "color option out of range".into(),
            ));
        };
        Some(c)
    };
    let source = mp.source;
    let ability_index = mp.ability_index;
    let controller = mp.controller;
    let resume_decision = mp.resume_decision.clone();
    let resume_priority = mp.resume_priority_player;

    state.take_pending_prompt();
    state.set_decision(resume_decision);
    state.set_priority_player(resume_priority);

    let (act_clone, is_land) = {
        let cache = state.evaluate_board();
        let is_land = cache
            .get(source)
            .is_some_and(|e| e.types.contains(CardTypeMask::LAND));
        let ctrl = cache.get(source).map_or(controller, |e| e.controller);
        let ctx = EvalContext::for_source(ctrl, source).with_board(BoardView::Cached(cache));
        let abilities = resolve_object_activated_abilities_cached(state, source, &ctx, cache);
        let Some(res_ab) = abilities
            .into_iter()
            .find(|a| a.ability_index == ability_index)
        else {
            return Err(EngineError::IllegalAction("mana ability vanished".into()));
        };
        let Some(act) = res_ab.activated else {
            return Err(EngineError::IllegalAction(
                "missing activated ability".into(),
            ));
        };
        (act.into_owned(), is_land)
    };

    // If a bundle was chosen specifically by index (e.g. [W, B] vs [W, W]), pass bundle directly
    execute_mana_ability_body_with_bundle_idx(
        state,
        controller,
        source,
        ability_index,
        &act_clone,
        is_land,
        chosen_color,
        Some(idx),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_mana_ability_body_with_bundle_idx(
    state: &mut GameState,
    player: PlayerId,
    object: ObjectId,
    ability_index: u16,
    act: &mtg_ir::ActivatedAbility,
    is_land: bool,
    chosen_color: Option<ManaColor>,
    bundle_idx: Option<usize>,
) -> Result<ManaActivationOutcome, EngineError> {
    let mut requires_tap = false;
    let mut life_paid: u32 = 0;

    // 1. Pay activation costs: pay TapSelf / UntapSelf / Mana before PayLife / other
    // non-mana costs (CR 118.3; prevents [TapOthers, TapSelf] or [Sacrifice, TapSelf]
    // from double-using or prematurely removing `object`).
    for cost in &act.cost {
        match cost {
            CostDef::TapSelf => {
                if state.object(object).is_none_or(|o| {
                    o.zone != Zone::Battlefield || o.flags.contains(ObjectFlags::TAPPED)
                }) {
                    return Err(EngineError::IllegalAction(
                        "cannot tap self for mana ability".into(),
                    ));
                }
                requires_tap = true;
                if let Some(obj) = state.object_mut(object) {
                    obj.flags.insert(ObjectFlags::TAPPED);
                }
                state.mark_dirty(DirtyMask::TAPPED);
                emit_event(
                    state,
                    AtomicEventKind::TapStateChanged {
                        object,
                        now_tapped: true,
                        actor: Some(player),
                    },
                );
            }
            CostDef::UntapSelf { .. } => {
                if state.object(object).is_none_or(|o| {
                    o.zone != Zone::Battlefield || !o.flags.contains(ObjectFlags::TAPPED)
                }) {
                    return Err(EngineError::IllegalAction(
                        "cannot untap self for mana ability".into(),
                    ));
                }
                match crate::replace::resolve_untap(state, object)? {
                    crate::replace::ReplacementResult::Paused { prompt_id, chooser } => {
                        return Ok(ManaActivationOutcome::Paused { prompt_id, chooser });
                    }
                    crate::replace::ReplacementResult::Resolved(out) => {
                        if !out.mods.skipped {
                            if let Some(obj) = state.object_mut(object) {
                                obj.flags.remove(ObjectFlags::TAPPED);
                            }
                            state.mark_dirty(DirtyMask::TAPPED);
                            emit_event(
                                state,
                                AtomicEventKind::TapStateChanged {
                                    object,
                                    now_tapped: false,
                                    actor: Some(player),
                                },
                            );
                        }
                    }
                }
            }
            CostDef::Mana { cost: mc, .. } => {
                let spend_ctx = ManaSpendContext::for_generic_cost(player);
                deduct_mana_for_context(state, player, mc, &spend_ctx, &[])?;
            }
            _ => {}
        }
    }

    for cost in &act.cost {
        match cost {
            CostDef::TapSelf | CostDef::UntapSelf { .. } | CostDef::Mana { .. } => {}
            CostDef::PayLife {
                amount,
                half_life_rounded_up,
            } => {
                let amt = if *half_life_rounded_up {
                    let cur = state.player(player).map_or(0, |p| p.life).max(0);
                    ((cur + 1) / 2) as u32
                } else {
                    let cache = state.evaluate_board();
                    let ctx = EvalContext::for_source(player, object)
                        .with_board(BoardView::Cached(cache));
                    evaluate_amount(state, amount, &ctx).max(0) as u32
                };
                if let Some(p) = state.player_mut(player) {
                    p.life -= amt as i32;
                }
                life_paid = life_paid.saturating_add(amt);
                state.mark_dirty(DirtyMask::LIFE);
                if amt > 0 {
                    emit_event(
                        state,
                        AtomicEventKind::LifeChange {
                            player,
                            delta: -(amt as i32),
                        },
                    );
                }
            }
            other => {
                match costs::execute_non_mana_cost_step(
                    state,
                    player,
                    object,
                    other,
                    &[],
                    None,
                    None,
                )? {
                    costs::CostStepOutcome::Paused { prompt_id, chooser } => {
                        return Ok(ManaActivationOutcome::Paused { prompt_id, chooser });
                    }
                    costs::CostStepOutcome::Paid => {}
                }
            }
        }
    }

    // Track once-per-turn / once-per-game usage (read-only check before CoW allocation)
    let is_once_turn = act
        .activation_restrictions
        .contains(&ActivationRestriction::OnceEachTurn);
    let is_once_game = act
        .activation_restrictions
        .contains(&ActivationRestriction::OncePerGame);
    if is_once_turn || is_once_game {
        let need_turn = is_once_turn
            && !state.player(player).is_some_and(|p| {
                p.once_per_turn_abilities_used()
                    .contains(&(object, ability_index))
            });
        let need_game = is_once_game
            && !state.player(player).is_some_and(|p| {
                p.once_per_game_abilities_used()
                    .contains(&(object, ability_index))
            });
        if (need_turn || need_game)
            && let Some(p) = state.player_mut(player)
        {
            let ext = p.mana_ext_mut();
            if need_turn {
                ext.once_per_turn_abilities_used
                    .push((object, ability_index));
            }
            if need_game {
                ext.once_per_game_abilities_used
                    .push((object, ability_index));
            }
            state.mark_dirty(DirtyMask::PLAYER_STATE);
        }
    }

    // 2. Compute TapForManaMultiplier (CR 106.12)
    let tap_mult = if requires_tap {
        triggers::tap_for_mana_multiplier(state, player, object)
    } else {
        1
    };

    // 3. Execute AddMana effect steps
    let mut produced_all: SmallVec<[ManaColor; 6]> = SmallVec::new();
    for step in &act.effect {
        let pre_step = {
            let cache = state.evaluate_board();
            let eff_ctx =
                EvalContext::for_source(player, object).with_board(BoardView::Cached(cache));
            if let Some(mtg_ir::StepGate::Condition { condition }) = &step.gate
                && !evaluate_condition(state, condition, &eff_ctx)
            {
                None
            } else {
                let recipient = step
                    .actor
                    .as_ref()
                    .and_then(|pr| resolve_player_ref(state, pr, &eff_ctx))
                    .unwrap_or(player);
                let ignores_step_amount = match &step.op {
                    EffectOp::AddMana { production, .. } => {
                        add_mana_ignores_step_amount(production)
                    }
                    _ => false,
                };
                let base_mult = if ignores_step_amount {
                    1
                } else {
                    step.amount
                        .as_deref()
                        .map_or(1u32, |a| evaluate_amount(state, a, &eff_ctx).max(0) as u32)
                };
                let step_mult = base_mult.saturating_mul(tap_mult);
                if step_mult == 0 {
                    None
                } else {
                    Some((recipient, step_mult))
                }
            }
        };
        let Some((recipient, step_mult)) = pre_step else {
            continue;
        };
        match &step.op {
            EffectOp::AddMana {
                production,
                restriction,
                restriction_spec,
                retain_until_eot,
                delayed_rider,
            } => {
                let effective_prod = match (production, bundle_idx) {
                    (ManaProduction::Bundles { options }, Some(bidx)) => {
                        if let Some(chosen_bundle) = options.get(bidx) {
                            ManaProduction::Fixed {
                                symbols: chosen_bundle.iter().copied().collect(),
                            }
                        } else {
                            production.clone()
                        }
                    }
                    _ => production.clone(),
                };
                let added = add_mana_from_production(
                    state,
                    recipient,
                    Some(object),
                    &effective_prod,
                    *restriction,
                    restriction_spec.as_deref(),
                    *retain_until_eot,
                    delayed_rider.as_deref(),
                    chosen_color,
                    step_mult,
                )?;
                produced_all.extend(added);
            }
            EffectOp::RetainMana { colors } => {
                if !colors.is_empty()
                    && let Some(p) = state.player_mut(recipient)
                {
                    p.mana_ext_mut().unemptied_mana_colors_this_turn |= *colors;
                    state.mark_dirty(DirtyMask::PLAYER_STATE);
                }
            }
            _ => {}
        }
    }

    let total_amt = produced_all.len() as u32;
    emit_event(
        state,
        AtomicEventKind::AbilityActivated {
            player,
            source: object,
            stack_item: None,
            is_mana: true,
            is_loyalty: false,
            life_paid,
        },
    );
    if requires_tap {
        emit_event(
            state,
            AtomicEventKind::TappedForMana {
                player,
                source: object,
                produced_colors: produced_all.clone(),
                amount: total_amt,
            },
        );
    }
    if is_land {
        emit_event(
            state,
            AtomicEventKind::ManaAddedByLandAbility {
                player,
                source: object,
                produced_colors: produced_all,
                amount: total_amt,
            },
        );
    }

    triggers::flush_inline_triggers(state);
    Ok(ManaActivationOutcome::Produced)
}

/// Returns `true` when TS runtime ignores `step.amount` (`loops = 1`) while still
/// applying `tap_mult`: multi-symbol `Fixed`, `Bundles`, `ChosenColorAmount`,
/// `OrChosenColor`, and `WithChosenColor`.
pub(crate) fn add_mana_ignores_step_amount(production: &ManaProduction) -> bool {
    match production {
        ManaProduction::Fixed { symbols } => symbols.len() > 1,
        ManaProduction::Bundles { .. }
        | ManaProduction::ChosenColorAmount { .. }
        | ManaProduction::OrChosenColor { .. }
        | ManaProduction::WithChosenColor { .. } => true,
        ManaProduction::Choice { .. }
        | ManaProduction::Combination { .. }
        | ManaProduction::FromExiledColors
        | ManaProduction::PerColorControlled
        | ManaProduction::LandProducedTypes
        | ManaProduction::FromProducedEvent => false,
    }
}

/// Evaluates an [`mtg_ir::Amount`] for [`ManaProduction::ChosenColorAmount`], overriding
/// `Amount::Devotion.colors` with `chosen & ColorMask::ALL_COLORS` while respecting `.player`
/// and matching `eval::evaluate_amount`'s recursion depth guard.
pub(crate) fn evaluate_chosen_color_amount(
    state: &GameState,
    amount: &mtg_ir::Amount,
    chosen: ColorMask,
    ctx: &EvalContext<'_>,
) -> i32 {
    use mtg_ir::{Amount, RoundMode};
    let Some(child_ctx) = ctx.nested() else {
        return 0;
    };
    let chosen_colors = chosen & ColorMask::ALL_COLORS;
    match amount {
        Amount::Devotion { player, .. } => {
            let pid = player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx))
                .unwrap_or(child_ctx.controller);
            crate::eval::compute_devotion(state, pid, chosen_colors, &child_ctx)
        }
        Amount::Negate { of } => {
            evaluate_chosen_color_amount(state, of, chosen, &child_ctx).saturating_neg()
        }
        Amount::Sum { args } => args.iter().fold(0i32, |acc, a| {
            acc.saturating_add(evaluate_chosen_color_amount(state, a, chosen, &child_ctx))
        }),
        Amount::Min { args } => args
            .iter()
            .map(|a| evaluate_chosen_color_amount(state, a, chosen, &child_ctx))
            .min()
            .unwrap_or(0),
        Amount::Max { args } => args
            .iter()
            .map(|a| evaluate_chosen_color_amount(state, a, chosen, &child_ctx))
            .max()
            .unwrap_or(0),
        Amount::Mul { factor, of } => {
            factor.saturating_mul(evaluate_chosen_color_amount(state, of, chosen, &child_ctx))
        }
        Amount::Half { of, round } => {
            let v = evaluate_chosen_color_amount(state, of, chosen, &child_ctx);
            match round {
                RoundMode::Up => v.saturating_add(1).div_euclid(2),
                RoundMode::Down => v.div_euclid(2),
            }
        }
        Amount::Difference { left, right } => {
            evaluate_chosen_color_amount(state, left, chosen, &child_ctx).saturating_sub(
                evaluate_chosen_color_amount(state, right, chosen, &child_ctx),
            )
        }
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            if evaluate_condition(state, condition, &child_ctx) {
                evaluate_chosen_color_amount(state, then_amt, chosen, &child_ctx)
            } else {
                evaluate_chosen_color_amount(state, else_amt, chosen, &child_ctx)
            }
        }
        _ => evaluate_amount(state, amount, ctx),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn add_mana_from_production(
    state: &mut GameState,
    player: PlayerId,
    source: Option<ObjectId>,
    prod: &ManaProduction,
    restriction: Option<ManaRestrictionTag>,
    restriction_spec: Option<&ManaSpendingRestriction>,
    retain_until_eot: bool,
    delayed_rider: Option<&DelayedTriggerPayload>,
    chosen_color: Option<ManaColor>,
    mult: u32,
) -> Result<SmallVec<[ManaColor; 6]>, EngineError> {
    if mult == 0 {
        return Ok(SmallVec::new());
    }
    let src_id = source.unwrap_or(crate::state::NONE_OBJECT_ID);

    let (colors_to_add, (source_kind_flags, is_snow, chosen_st_name)) = {
        let cache = state.evaluate_board();
        let mut colors_to_add: SmallVec<[ManaColor; 6]> = SmallVec::new();

        match prod {
            ManaProduction::Fixed { symbols } => {
                for _ in 0..mult {
                    for &s in symbols {
                        colors_to_add.push(s);
                    }
                }
            }
            ManaProduction::Choice { kind, colors } => {
                let options = if let Some(sid) = source {
                    choice_colors_for_kind_cached(state, player, sid, *kind, *colors, 0, cache)
                } else {
                    colors_from_mask(*colors)
                };
                let picked = chosen_color
                    .filter(|c| options.is_empty() || options.contains(c))
                    .or_else(|| options.first().copied())
                    .unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(picked);
                }
            }
            ManaProduction::Bundles { options } => {
                let chosen_bundle = chosen_color
                    .and_then(|c| {
                        options
                            .iter()
                            .find(|b| b.iter().all(|x| *x == c))
                            .or_else(|| options.iter().find(|b| b.contains(&c)))
                    })
                    .or_else(|| options.first());
                if let Some(bundle) = chosen_bundle {
                    for _ in 0..mult {
                        for &c in bundle {
                            colors_to_add.push(c);
                        }
                    }
                }
            }
            ManaProduction::Combination { any_color, colors } => {
                let options = if *any_color {
                    SmallVec::from_slice(&FIVE_COLORS)
                } else {
                    colors_from_mask(*colors)
                };
                let picked = chosen_color
                    .filter(|c| options.is_empty() || options.contains(c))
                    .or_else(|| options.first().copied())
                    .unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(picked);
                }
            }
            ManaProduction::OrChosenColor { colors } => {
                let mut m = *colors;
                if let Some(sid) = source
                    && let Some(cm) = state.rare(sid).and_then(|r| r.chosen_color)
                {
                    m |= cm;
                }
                let options = colors_from_mask(m);
                let picked = chosen_color
                    .filter(|c| options.contains(c))
                    .or_else(|| options.first().copied())
                    .unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(picked);
                }
            }
            ManaProduction::WithChosenColor { colors } => {
                for _ in 0..mult {
                    for c in colors_from_mask(*colors) {
                        colors_to_add.push(c);
                    }
                    if let Some(sid) = source
                        && let Some(cm) = state.rare(sid).and_then(|r| r.chosen_color)
                    {
                        for c in colors_from_mask(cm) {
                            colors_to_add.push(c);
                        }
                    }
                }
            }
            ManaProduction::FromExiledColors => {
                let m = if let Some(sid) = source {
                    production_color_mask_cached(state, player, sid, prod, 0, cache)
                } else {
                    ColorMask::empty()
                };
                let options = colors_from_mask(m);
                let picked = chosen_color
                    .filter(|c| options.contains(c))
                    .or_else(|| options.first().copied())
                    .unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(picked);
                }
            }
            ManaProduction::PerColorControlled => {
                let m = if let Some(sid) = source {
                    production_color_mask_cached(state, player, sid, prod, 0, cache)
                } else {
                    ColorMask::empty()
                };
                for _ in 0..mult {
                    for c in colors_from_mask(m) {
                        colors_to_add.push(c);
                    }
                }
            }
            ManaProduction::LandProducedTypes => {
                let options = if let Some(sid) = source {
                    choice_colors_for_kind_cached(
                        state,
                        player,
                        sid,
                        ManaChoiceKind::ControlledLandProducedTypes,
                        ColorMask::empty(),
                        0,
                        cache,
                    )
                } else {
                    SmallVec::new()
                };
                let picked = chosen_color
                    .filter(|c| options.contains(c))
                    .or_else(|| options.first().copied())
                    .unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(picked);
                }
            }
            ManaProduction::ChosenColorAmount { amount } => {
                let chosen_from_rare = source
                    .and_then(|sid| state.rare(sid))
                    .and_then(|r| r.chosen_color)
                    .and_then(|cm| colors_from_mask(cm).into_iter().next());
                if let Some(c) = chosen_from_rare.or(chosen_color) {
                    let ctx = if let Some(sid) = source {
                        EvalContext::for_source(player, sid).with_board(BoardView::Cached(cache))
                    } else {
                        EvalContext::new(player).with_board(BoardView::Cached(cache))
                    };
                    let n = evaluate_chosen_color_amount(
                        state,
                        amount,
                        ColorMask::from_mana_color(c),
                        &ctx,
                    )
                    .max(0) as u32;
                    for _ in 0..n.saturating_mul(mult) {
                        colors_to_add.push(c);
                    }
                }
            }
            ManaProduction::FromProducedEvent => {
                let c = chosen_color.unwrap_or(ManaColor::C);
                for _ in 0..mult {
                    colors_to_add.push(c);
                }
            }
        }

        if colors_to_add.is_empty() {
            return Ok(colors_to_add);
        }

        // Compute source metadata flags
        let meta = if let Some(sid) = source {
            let ev_opt = cache.get(sid);
            let mut flags = 0u8;
            let mut snow = false;
            if let Some(ev) = ev_opt {
                if ev.types.contains(CardTypeMask::CREATURE) {
                    flags |= 1 << 0;
                }
                if ev.supertypes.contains(SupertypeMask::BASIC)
                    && ev.types.contains(CardTypeMask::LAND)
                {
                    flags |= 1 << 1;
                }
                if ev.supertypes.contains(SupertypeMask::SNOW) {
                    flags |= 1 << 2;
                    snow = true;
                }
                if let Some(tr_id) = state.db().subtype_id("Treasure")
                    && ev.subtypes.contains(&tr_id)
                {
                    flags |= 1 << 3;
                }
            }
            let mut st_name = None;
            if let Some(rare) = state.rare(sid) {
                if rare.chosen_color.is_some() {
                    flags |= 1 << 4;
                }
                if let Some(st_id) = rare.chosen_subtype {
                    st_name = state.db().subtype_name(st_id).map(ToString::to_string);
                }
            }
            (flags, snow, st_name)
        } else {
            let _ = src_id;
            (0u8, false, None)
        };
        (colors_to_add, meta)
    };

    let has_bucket = restriction.is_some() || restriction_spec.is_some() || delayed_rider.is_some();

    if has_bucket {
        let mut eff_spec = restriction_spec.cloned().unwrap_or_default();
        if eff_spec.source_chosen_subtype.is_none() {
            eff_spec.source_chosen_subtype = chosen_st_name;
        }
        let mut counts = [0u32; 6];
        for &c in &colors_to_add {
            counts[color_to_index(c)] = counts[color_to_index(c)].saturating_add(1);
        }
        let arc_spec = Arc::new(eff_spec);
        let arc_rider = delayed_rider.map(|r| Arc::new(r.clone()));
        let p = state
            .player_mut(player)
            .ok_or_else(|| EngineError::IllegalAction("invalid mana recipient".into()))?;
        let ext = p.mana_ext_mut();
        if let Some(existing) = ext.restricted_mana.iter_mut().find(|b| {
            b.restriction.as_ref() == arc_spec.as_ref()
                && b.source_id == source
                && b.source_tag == restriction
                && b.source_kind_flags == source_kind_flags
                && b.is_snow == is_snow
                && b.retain_until_eot == retain_until_eot
                && b.delayed_rider.as_deref() == delayed_rider
        }) {
            for (dst, src) in existing.colors.iter_mut().zip(counts.iter()) {
                *dst = dst.saturating_add(*src);
            }
        } else {
            ext.restricted_mana.push(RestrictedManaBucket {
                colors: counts,
                restriction: arc_spec,
                source_id: source,
                source_tag: restriction,
                source_kind_flags,
                is_snow,
                retain_until_eot,
                delayed_rider: arc_rider,
            });
        }
    } else {
        let p = state
            .player_mut(player)
            .ok_or_else(|| EngineError::IllegalAction("invalid mana recipient".into()))?;
        for &c in &colors_to_add {
            let idx = color_to_index(c);
            p.mana_pool[idx] = p.mana_pool[idx].saturating_add(1);
        }
        if is_snow || retain_until_eot {
            let ext = p.mana_ext_mut();
            for &c in &colors_to_add {
                let idx = color_to_index(c);
                if is_snow {
                    ext.snow_mana_pool[idx] = ext.snow_mana_pool[idx].saturating_add(1);
                }
                if retain_until_eot {
                    ext.retained_mana_counts[idx] = ext.retained_mana_counts[idx].saturating_add(1);
                }
            }
        }
    }

    state.mark_dirty(DirtyMask::PLAYER_STATE);
    Ok(colors_to_add)
}

pub(crate) fn can_spend_restriction_for_context_cached(
    state: &GameState,
    spec: &ManaSpendingRestriction,
    source_id: Option<ObjectId>,
    source_tag: Option<ManaRestrictionTag>,
    source_kind_flags: u8,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    if let Some(src_req) = ctx.source_restriction {
        let bit = match src_req {
            ManaSourceRestrictionKind::Creature => 1 << 0,
            ManaSourceRestrictionKind::BasicLand => 1 << 1,
            ManaSourceRestrictionKind::Snow => 1 << 2,
            ManaSourceRestrictionKind::Treasure => 1 << 3,
            ManaSourceRestrictionKind::ChosenColor => 1 << 4,
        };
        if (source_kind_flags & bit) == 0 {
            return false;
        }
    }

    if let Some(ManaRestrictionTag::ArtifactSpellOrNonSpell) = source_tag
        && ctx.kind == ManaSpendContextKind::Spell
    {
        let is_artifact = ctx.source_object.is_some_and(|oid| {
            spell_has_type_cached(
                state,
                oid,
                ctx.face_idx,
                ctx.from_zone,
                CardTypeMask::ARTIFACT,
                cache,
            )
        });
        if !is_artifact {
            return false;
        }
    }

    if spec.allow_spells.is_none() && spec.allow_abilities.is_none() {
        return true;
    }

    match ctx.kind {
        ManaSpendContextKind::Spell => {
            let Some(allow) = &spec.allow_spells else {
                return false;
            };
            allow_spells_matches_cached(
                state,
                ctx,
                allow,
                source_id,
                spec.source_chosen_subtype.as_deref(),
                cache,
            )
        }
        ManaSpendContextKind::ActivatedAbility | ManaSpendContextKind::TriggeredAbility => {
            let Some(allow) = &spec.allow_abilities else {
                return false;
            };
            if allow.any_ability {
                return true;
            }
            let Some(sf) = &allow.source_filter else {
                return false;
            };
            let Some(oid) = ctx.source_object else {
                return false;
            };
            let eval_ctx = EvalContext {
                controller: ctx.player,
                source: source_id,
                board: BoardView::Cached(cache),
                ..EvalContext::new(ctx.player)
            };
            plain_mana_filter_matches_with_chosen_fallback(
                state,
                oid,
                ctx.face_idx,
                ctx.from_zone,
                sf,
                &eval_ctx,
                spec.source_chosen_subtype.as_deref(),
            )
        }
        ManaSpendContextKind::ForetellAction => {
            spec.allow_abilities.as_ref().is_some_and(|a| a.foretell)
        }
        ManaSpendContextKind::SpecialAction | ManaSpendContextKind::WardOrCost => false,
    }
}

pub fn can_spend_bucket_for_context(
    state: &GameState,
    bucket: &RestrictedManaBucket,
    ctx: &ManaSpendContext,
) -> bool {
    can_spend_bucket_for_context_cached(state, bucket, ctx, state.evaluate_board())
}

pub(crate) fn can_spend_bucket_for_context_cached(
    state: &GameState,
    bucket: &RestrictedManaBucket,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    can_spend_restriction_for_context_cached(
        state,
        &bucket.restriction,
        bucket.source_id,
        bucket.source_tag,
        bucket.source_kind_flags,
        ctx,
        cache,
    )
}

fn spell_has_type_cached(
    state: &GameState,
    id: ObjectId,
    face_idx: u8,
    from_zone: Zone,
    mask: CardTypeMask,
    cache: &LayerCache,
) -> bool {
    if from_zone == Zone::Stack
        && let Some(ev) = cache.get(id)
    {
        return ev.types.intersects(mask);
    }
    let Some(obj) = state.object(id) else {
        return false;
    };
    let mut opts = face_opts_for_object(state, id).unwrap_or_default();
    opts.zone = from_zone;
    opts.face_index = face_idx;
    resolve_object_faces(state.db(), obj.card_def, &opts).is_some_and(|r| r.types.intersects(mask))
}

fn allow_spells_matches_cached(
    state: &GameState,
    ctx: &ManaSpendContext,
    allow: &AllowSpellsRestriction,
    source_id: Option<ObjectId>,
    source_chosen_subtype: Option<&str>,
    cache: &LayerCache,
) -> bool {
    let primary_ok = check_spell_filter_branch_cached(
        state,
        ctx,
        allow.filter.as_ref(),
        allow.has_x_in_cost,
        allow.without_x_in_cost,
        allow.is_commander,
        source_id,
        source_chosen_subtype,
        cache,
    );
    if primary_ok {
        return true;
    }
    if allow.alt_filter.is_some() || allow.alt_has_x_in_cost {
        return check_spell_filter_branch_cached(
            state,
            ctx,
            allow.alt_filter.as_ref(),
            allow.alt_has_x_in_cost,
            false,
            false,
            source_id,
            source_chosen_subtype,
            cache,
        );
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn check_spell_filter_branch_cached(
    state: &GameState,
    ctx: &ManaSpendContext,
    filter: Option<&PlainManaFilter>,
    req_x: bool,
    req_no_x: bool,
    req_commander: bool,
    source_id: Option<ObjectId>,
    source_chosen_subtype: Option<&str>,
    cache: &LayerCache,
) -> bool {
    if req_x && !ctx.has_x_in_cost {
        return false;
    }
    if req_no_x && ctx.has_x_in_cost {
        return false;
    }
    if req_commander && !ctx.is_commander {
        return false;
    }
    let Some(f) = filter else {
        return true;
    };
    let Some(oid) = ctx.source_object else {
        return false;
    };
    let eval_ctx = EvalContext {
        controller: ctx.player,
        source: source_id,
        board: BoardView::Cached(cache),
        ..EvalContext::new(ctx.player)
    };
    plain_mana_filter_matches_with_chosen_fallback(
        state,
        oid,
        ctx.face_idx,
        ctx.from_zone,
        f,
        &eval_ctx,
        source_chosen_subtype,
    )
}

fn filter_uses_of_chosen_type(f: &PlainManaFilter) -> bool {
    f.of_chosen_type || f.any_of.iter().any(filter_uses_of_chosen_type)
}

fn matches_plain_mana_filter_with_chosen_id(
    state: &GameState,
    oid: ObjectId,
    face_idx: u8,
    from_zone: Zone,
    filter: &PlainManaFilter,
    eval_ctx: &EvalContext<'_>,
    st_id: mtg_ir::SubtypeId,
) -> bool {
    if !filter.any_of.is_empty()
        && !filter.any_of.iter().any(|sub| {
            matches_plain_mana_filter_with_chosen_id(
                state, oid, face_idx, from_zone, sub, eval_ctx, st_id,
            )
        })
    {
        return false;
    }

    if filter.any_of.is_empty() && !filter.of_chosen_type {
        return matches_plain_mana_filter(state, oid, face_idx, from_zone, filter, eval_ctx);
    }

    let mut f_node = filter.clone();
    f_node.any_of.clear();
    if !f_node.of_chosen_type {
        return matches_plain_mana_filter(state, oid, face_idx, from_zone, &f_node, eval_ctx);
    }
    if matches_plain_mana_filter(state, oid, face_idx, from_zone, &f_node, eval_ctx) {
        return true;
    }
    f_node.of_chosen_type = false;
    if f_node.subtypes_any.is_empty() {
        f_node.subtypes_any.push(st_id);
        return matches_plain_mana_filter(state, oid, face_idx, from_zone, &f_node, eval_ctx);
    }
    let st_filter = PlainManaFilter {
        subtypes_any: smallvec::smallvec![st_id],
        ..Default::default()
    };
    matches_plain_mana_filter(state, oid, face_idx, from_zone, &f_node, eval_ctx)
        && matches_plain_mana_filter(state, oid, face_idx, from_zone, &st_filter, eval_ctx)
}

fn plain_mana_filter_matches_with_chosen_fallback(
    state: &GameState,
    oid: ObjectId,
    face_idx: u8,
    from_zone: Zone,
    filter: &PlainManaFilter,
    eval_ctx: &EvalContext<'_>,
    source_chosen_subtype: Option<&str>,
) -> bool {
    if matches_plain_mana_filter(state, oid, face_idx, from_zone, filter, eval_ctx) {
        return true;
    }
    if filter_uses_of_chosen_type(filter)
        && let Some(st_name) = source_chosen_subtype
        && let Some(st_id) = state.db().subtype_id(st_name)
    {
        return matches_plain_mana_filter_with_chosen_id(
            state, oid, face_idx, from_zone, filter, eval_ctx, st_id,
        );
    }
    false
}

pub fn can_spend_mana_as_any_color(
    state: &GameState,
    player: PlayerId,
    ctx: &ManaSpendContext,
) -> bool {
    can_spend_mana_as_any_color_cached(state, player, ctx, state.evaluate_board())
}

pub(crate) fn can_spend_mana_as_any_color_cached(
    state: &GameState,
    player: PlayerId,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    if ctx.alt_cast_spend_any_color || ctx.mana_as_any_color_ability {
        return true;
    }
    let Some(ep) = cache.player(player) else {
        return false;
    };
    for rm in &ep.active_rule_mods {
        if let RuleMod::SpendManaAsAnyColor {
            abilities_only,
            source_filter,
        } = &rm.rule
        {
            if *abilities_only
                && !matches!(
                    ctx.kind,
                    ManaSpendContextKind::ActivatedAbility | ManaSpendContextKind::TriggeredAbility
                )
            {
                continue;
            }
            if let Some(sf) = source_filter {
                let Some(oid) = ctx.source_object else {
                    continue;
                };
                let eval_ctx = EvalContext::new(player).with_board(BoardView::Cached(cache));
                if !matches_object_filter(state, oid, sf, &eval_ctx) {
                    continue;
                }
            }
            return true;
        }
    }
    false
}

/// Internal restricted-bucket representation for pool deduction and auto-tap simulation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SimBucket {
    pub orig_idx: usize,
    pub colors: [u32; 6],
    pub disallow_generic: bool,
    pub is_snow: bool,
    pub has_spend_restriction: bool,
}

/// Internal pool snapshot used by both pool payment verification and auto-tap simulation.
#[derive(Debug, Clone)]
pub(crate) struct SimulatedManaPool {
    pub unrestricted: [u32; 6],
    pub snow: [u32; 6],
    pub buckets: Vec<SimBucket>,
    pub any_color: bool,
    pub life_available: u32,
    pub cant_pay_life: bool,
    pub x_spend_colors: ColorMask,
    pub x_colored_only: bool,
    pub x_max_one_per_color: bool,
}

impl SimulatedManaPool {
    pub(crate) fn from_state_cached(
        state: &GameState,
        player: PlayerId,
        ctx: &ManaSpendContext,
        cache: &LayerCache,
    ) -> Self {
        let p = state.player(player);
        let unrestricted = p.map_or([0; 6], |ps| ps.mana_pool);
        let snow = p.map_or([0; 6], PlayerState::snow_mana_pool);
        let mut buckets = Vec::new();
        if let Some(ps) = p {
            for (bi, b) in ps.restricted_mana().iter().enumerate() {
                if can_spend_bucket_for_context_cached(state, b, ctx, cache)
                    && b.colors.iter().any(|&v| v > 0)
                {
                    buckets.push(SimBucket {
                        orig_idx: bi,
                        colors: b.colors,
                        disallow_generic: b.restriction.disallow_generic,
                        is_snow: b.is_snow,
                        has_spend_restriction: b.source_tag.is_some()
                            || b.restriction.allow_spells.is_some()
                            || b.restriction.allow_abilities.is_some(),
                    });
                }
            }
        }
        let life_raw = p.map_or(0, |ps| ps.life);
        let cant_pay_life = is_cant_pay_life_cached(state, player, ctx, cache);
        Self {
            unrestricted,
            snow,
            buckets,
            any_color: can_spend_mana_as_any_color_cached(state, player, ctx, cache),
            life_available: life_raw.max(0) as u32,
            cant_pay_life,
            x_spend_colors: ctx.x_spend_colors,
            x_colored_only: ctx.x_colored_only,
            x_max_one_per_color: ctx.x_max_one_per_color,
        }
    }

    pub(crate) fn total_available(&self) -> u32 {
        let mut tot = 0u32;
        for v in self.unrestricted {
            tot = tot.saturating_add(v);
        }
        for b in &self.buckets {
            for v in b.colors {
                tot = tot.saturating_add(v);
            }
        }
        tot
    }
}

pub(crate) fn is_cant_pay_life_cached(
    _state: &GameState,
    player: PlayerId,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    let Some(ep) = cache.player(player) else {
        return false;
    };
    if ep.life_cant_change {
        return true;
    }
    let is_mana_ab =
        matches!(ctx.kind, ManaSpendContextKind::ActivatedAbility) && ctx.source_object.is_some();
    for rm in &ep.active_rule_mods {
        if let RuleMod::CantPayLifeOrSacrificeForCosts {
            cant_pay_life,
            non_mana_abilities_only,
            ..
        } = &rm.rule
            && *cant_pay_life
        {
            if *non_mana_abilities_only && is_mana_ab {
                continue;
            }
            return true;
        }
    }
    false
}

/// Checks whether `cost` can be paid purely from `player`'s current mana pool (and Phyrexian life if allowed).
pub fn can_pay_mana_from_pool(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
) -> bool {
    can_pay_mana_from_pool_cached(state, player, cost, ctx, state.evaluate_board())
}

pub(crate) fn can_pay_mana_from_pool_cached(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    cache: &LayerCache,
) -> bool {
    can_pay_mana_from_pool_with_life_reserve_cached(state, player, cost, ctx, 0, cache)
}

pub(crate) fn can_pay_mana_from_pool_with_life_reserve_cached(
    state: &GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    reserved_life: u32,
    cache: &LayerCache,
) -> bool {
    let mut sim = SimulatedManaPool::from_state_cached(state, player, ctx, cache);
    sim.life_available = sim.life_available.saturating_sub(reserved_life);
    let default_flags = SmallVec::<[bool; 4]>::new();
    solve_pool_deduction(&sim, cost, &default_flags, true).is_some()
}

/// Solves a specific pip/generic allocation over `sim`, returning the per-source deductions or `None` if unpayable.
#[derive(Debug, Clone)]
pub(crate) struct PoolAllocationPlan {
    pub bucket_spent: Vec<(usize, [u32; 6])>,
    pub pool_spent: [u32; 6],
    pub snow_spent: [u32; 6],
    pub life_paid: u32,
    pub phyrexian_life_flags: SmallVec<[bool; 4]>,
}

#[derive(Clone, Copy)]
struct FlexPipItem {
    pip: ManaPip,
    phy_idx: Option<usize>,
}

pub(crate) fn solve_pool_deduction(
    sim: &SimulatedManaPool,
    cost: &ManaCost,
    phyrexian_pay_life: &[bool],
    allow_auto_phyrexian_life: bool,
) -> Option<PoolAllocationPlan> {
    // Expand pips into concrete payment options ordered most-constrained first:
    // Colored / explicit Phyrexian -> flexible (Hybrid / TwoBrid / ColorlessHybrid / HybridPhyrexian / auto-life Phyrexian) -> Snow -> Generic.
    let mut work = sim.clone();
    let mut bucket_spent: Vec<(usize, [u32; 6])> = work
        .buckets
        .iter()
        .map(|b| (b.orig_idx, [0u32; 6]))
        .collect();
    let mut pool_spent = [0u32; 6];
    let mut snow_spent = [0u32; 6];
    let mut life_paid = 0u32;

    let has_snow_pips = cost.pips.iter().any(|p| matches!(p, ManaPip::Snow));

    // 1. Check strict colored and explicit-decision Phyrexian pips first; defer Snow & flexible pips.
    let mut flex_pips: SmallVec<[FlexPipItem; 4]> = SmallVec::new();
    let mut snow_count: u32 = 0;
    let mut resolved_phy_flags: SmallVec<[bool; 4]> = SmallVec::new();

    for &pip in &cost.pips {
        match pip {
            ManaPip::Colored { color } => {
                if !consume_pip_color(
                    &mut work,
                    color,
                    false,
                    has_snow_pips,
                    &mut bucket_spent,
                    &mut pool_spent,
                    &mut snow_spent,
                ) {
                    return None;
                }
            }
            ManaPip::Snow => {
                snow_count = snow_count.saturating_add(1);
            }
            ManaPip::Phyrexian { color } => {
                let p_idx = resolved_phy_flags.len();
                resolved_phy_flags.push(false);
                let explicit_life = phyrexian_pay_life.get(p_idx).copied();
                match explicit_life {
                    Some(true) => {
                        if work.cant_pay_life || work.life_available < 2 {
                            return None;
                        }
                        work.life_available -= 2;
                        life_paid += 2;
                        resolved_phy_flags[p_idx] = true;
                    }
                    Some(false) => {
                        if !consume_pip_color(
                            &mut work,
                            color,
                            false,
                            has_snow_pips,
                            &mut bucket_spent,
                            &mut pool_spent,
                            &mut snow_spent,
                        ) {
                            return None;
                        }
                    }
                    None => {
                        if !allow_auto_phyrexian_life
                            || work.cant_pay_life
                            || work.life_available < 2
                        {
                            if !consume_pip_color(
                                &mut work,
                                color,
                                false,
                                has_snow_pips,
                                &mut bucket_spent,
                                &mut pool_spent,
                                &mut snow_spent,
                            ) {
                                return None;
                            }
                        } else {
                            flex_pips.push(FlexPipItem {
                                pip,
                                phy_idx: Some(p_idx),
                            });
                        }
                    }
                }
            }
            ManaPip::HybridPhyrexian { .. } => {
                let p_idx = resolved_phy_flags.len();
                resolved_phy_flags.push(false);
                flex_pips.push(FlexPipItem {
                    pip,
                    phy_idx: Some(p_idx),
                });
            }
            ManaPip::Hybrid { .. } | ManaPip::TwoBrid { .. } | ManaPip::ColorlessHybrid { .. } => {
                flex_pips.push(FlexPipItem { pip, phy_idx: None });
            }
        }
    }

    // 2. Backtrack over flexible pips + deferred Snow + generic requirement.
    let mut rem_generic = cost.generic;
    if !solve_flex_pips_and_generic(
        &mut work,
        &flex_pips,
        0,
        snow_count,
        &mut rem_generic,
        has_snow_pips,
        phyrexian_pay_life,
        allow_auto_phyrexian_life,
        &mut bucket_spent,
        &mut pool_spent,
        &mut snow_spent,
        &mut life_paid,
        &mut resolved_phy_flags,
    ) {
        return None;
    }

    Some(PoolAllocationPlan {
        bucket_spent,
        pool_spent,
        snow_spent,
        life_paid,
        phyrexian_life_flags: resolved_phy_flags,
    })
}

fn consume_one_non_snow_from_color_idx(
    work: &mut SimulatedManaPool,
    c_idx: usize,
    bucket_spent: &mut [(usize, [u32; 6])],
    pool_spent: &mut [u32; 6],
) -> bool {
    for dg_first in [true, false] {
        for sr_first in [true, false] {
            for (bi, b) in work.buckets.iter_mut().enumerate() {
                if !b.is_snow
                    && b.disallow_generic == dg_first
                    && b.has_spend_restriction == sr_first
                    && b.colors[c_idx] > 0
                {
                    b.colors[c_idx] -= 1;
                    bucket_spent[bi].1[c_idx] += 1;
                    return true;
                }
            }
        }
    }
    let non_snow = work.unrestricted[c_idx].saturating_sub(work.snow[c_idx]);
    if non_snow > 0 {
        work.unrestricted[c_idx] -= 1;
        pool_spent[c_idx] += 1;
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn consume_one_from_color_idx(
    work: &mut SimulatedManaPool,
    c_idx: usize,
    prefer_snow: bool,
    conserve_snow_buckets: bool,
    bucket_spent: &mut [(usize, [u32; 6])],
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
) -> bool {
    if conserve_snow_buckets {
        // 1. Spend from eligible non-snow restricted buckets first (`dg=true` before `dg=false`, `sr=true` before `sr=false`).
        for dg_first in [true, false] {
            for sr_first in [true, false] {
                for (bi, b) in work.buckets.iter_mut().enumerate() {
                    if !b.is_snow
                        && b.disallow_generic == dg_first
                        && b.has_spend_restriction == sr_first
                        && b.colors[c_idx] > 0
                    {
                        b.colors[c_idx] -= 1;
                        bucket_spent[bi].1[c_idx] += 1;
                        return true;
                    }
                }
            }
        }
        // 2. Spend from non-snow unrestricted pool before touching snow restricted buckets.
        if !prefer_snow {
            let non_snow = work.unrestricted[c_idx].saturating_sub(work.snow[c_idx]);
            if non_snow > 0 {
                work.unrestricted[c_idx] -= 1;
                pool_spent[c_idx] += 1;
                return true;
            }
        }
        // 3. Spend from eligible snow restricted buckets (`dg=true` before `dg=false`, `sr=true` before `sr=false`).
        for dg_first in [true, false] {
            for sr_first in [true, false] {
                for (bi, b) in work.buckets.iter_mut().enumerate() {
                    if b.is_snow
                        && b.disallow_generic == dg_first
                        && b.has_spend_restriction == sr_first
                        && b.colors[c_idx] > 0
                    {
                        b.colors[c_idx] -= 1;
                        bucket_spent[bi].1[c_idx] += 1;
                        return true;
                    }
                }
            }
        }
    } else {
        // No snow pips: prefer `disallow_generic == true` (`sr=true` then `sr=false`, non-snow then snow), then `disallow_generic == false`.
        for dg_first in [true, false] {
            for sr_first in [true, false] {
                for snow_first in [false, true] {
                    for (bi, b) in work.buckets.iter_mut().enumerate() {
                        if b.disallow_generic == dg_first
                            && b.has_spend_restriction == sr_first
                            && b.is_snow == snow_first
                            && b.colors[c_idx] > 0
                        {
                            b.colors[c_idx] -= 1;
                            bucket_spent[bi].1[c_idx] += 1;
                            return true;
                        }
                    }
                }
            }
        }
    }
    // 4. Spend from unrestricted pool, conserving snow mana unless `prefer_snow`.
    if work.unrestricted[c_idx] > 0 {
        let non_snow = work.unrestricted[c_idx].saturating_sub(work.snow[c_idx]);
        if !prefer_snow && non_snow > 0 {
            work.unrestricted[c_idx] -= 1;
            pool_spent[c_idx] += 1;
            return true;
        }
        if work.snow[c_idx] > 0 {
            work.unrestricted[c_idx] -= 1;
            work.snow[c_idx] -= 1;
            pool_spent[c_idx] += 1;
            snow_spent[c_idx] += 1;
            return true;
        }
        work.unrestricted[c_idx] -= 1;
        pool_spent[c_idx] += 1;
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn consume_pip_color(
    work: &mut SimulatedManaPool,
    target: ManaColor,
    prefer_snow: bool,
    conserve_snow_buckets: bool,
    bucket_spent: &mut [(usize, [u32; 6])],
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
) -> bool {
    let t_idx = color_to_index(target);
    // When SpendManaAsAnyColor is active and snow pips exist, prefer any non-snow mana
    // (target color first, then any color) before falling back to snow mana of `target`.
    if work.any_color && target != ManaColor::C && conserve_snow_buckets && !prefer_snow {
        if consume_one_non_snow_from_color_idx(work, t_idx, bucket_spent, pool_spent) {
            return true;
        }
        for c in ALL_MANA_COLORS {
            let idx = color_to_index(c);
            if consume_one_non_snow_from_color_idx(work, idx, bucket_spent, pool_spent) {
                return true;
            }
        }
    }
    if consume_one_from_color_idx(
        work,
        t_idx,
        prefer_snow,
        conserve_snow_buckets,
        bucket_spent,
        pool_spent,
        snow_spent,
    ) {
        return true;
    }
    // `SpendManaAsAnyColor` applies ONLY to colored pips (WUBRG) — NEVER `{C}` (CR 106.1b / 107.4).
    if work.any_color && target != ManaColor::C {
        for c in ALL_MANA_COLORS {
            let idx = color_to_index(c);
            if consume_one_from_color_idx(
                work,
                idx,
                prefer_snow,
                conserve_snow_buckets,
                bucket_spent,
                pool_spent,
                snow_spent,
            ) {
                return true;
            }
        }
    }
    false
}

fn consume_snow_pip(
    work: &mut SimulatedManaPool,
    bucket_spent: &mut [(usize, [u32; 6])],
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
) -> bool {
    let snow_color_order = [
        ManaColor::C,
        ManaColor::G,
        ManaColor::R,
        ManaColor::B,
        ManaColor::U,
        ManaColor::W,
    ];
    // Pass 1: `disallow_generic == true` restricted snow buckets (cannot pay generic anyway).
    for sr_first in [true, false] {
        for c in snow_color_order {
            let idx = color_to_index(c);
            for (bi, b) in work.buckets.iter_mut().enumerate() {
                if b.is_snow
                    && b.disallow_generic
                    && b.has_spend_restriction == sr_first
                    && b.colors[idx] > 0
                {
                    b.colors[idx] -= 1;
                    bucket_spent[bi].1[idx] += 1;
                    return true;
                }
            }
        }
    }
    // Pass 2: `ManaColor::C` snow (`disallow_generic == false` restricted snow buckets, then unrestricted snow `{C}`).
    let c_idx = color_to_index(ManaColor::C);
    for sr_first in [true, false] {
        for (bi, b) in work.buckets.iter_mut().enumerate() {
            if b.is_snow
                && !b.disallow_generic
                && b.has_spend_restriction == sr_first
                && b.colors[c_idx] > 0
            {
                b.colors[c_idx] -= 1;
                bucket_spent[bi].1[c_idx] += 1;
                return true;
            }
        }
    }
    if work.snow[c_idx] > 0 && work.unrestricted[c_idx] > 0 {
        work.snow[c_idx] -= 1;
        work.unrestricted[c_idx] -= 1;
        pool_spent[c_idx] += 1;
        snow_spent[c_idx] += 1;
        return true;
    }
    // Pass 3: Colored snow (`[G, R, B, U, W]` in `dg=false` restricted snow buckets, then unrestricted colored snow).
    let colored_snow_order = [
        ManaColor::G,
        ManaColor::R,
        ManaColor::B,
        ManaColor::U,
        ManaColor::W,
    ];
    for sr_first in [true, false] {
        for c in colored_snow_order {
            let idx = color_to_index(c);
            for (bi, b) in work.buckets.iter_mut().enumerate() {
                if b.is_snow
                    && !b.disallow_generic
                    && b.has_spend_restriction == sr_first
                    && b.colors[idx] > 0
                {
                    b.colors[idx] -= 1;
                    bucket_spent[bi].1[idx] += 1;
                    return true;
                }
            }
        }
    }
    for c in colored_snow_order {
        let idx = color_to_index(c);
        if work.snow[idx] > 0 && work.unrestricted[idx] > 0 {
            work.snow[idx] -= 1;
            work.unrestricted[idx] -= 1;
            pool_spent[idx] += 1;
            snow_spent[idx] += 1;
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn solve_flex_pips_and_generic(
    work: &mut SimulatedManaPool,
    flex: &[FlexPipItem],
    pos: usize,
    snow_count: u32,
    rem_generic: &mut u32,
    conserve_snow_buckets: bool,
    phyrexian_pay_life: &[bool],
    allow_auto_phyrexian_life: bool,
    bucket_spent: &mut Vec<(usize, [u32; 6])>,
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
    life_paid: &mut u32,
    resolved_phy_flags: &mut SmallVec<[bool; 4]>,
) -> bool {
    if pos >= flex.len() {
        for _ in 0..snow_count {
            if !consume_snow_pip(work, bucket_spent, pool_spent, snow_spent) {
                return false;
            }
        }
        return consume_generic_mana(work, *rem_generic, bucket_spent, pool_spent, snow_spent);
    }

    let item = flex[pos];
    let try_branch = |work: &mut SimulatedManaPool,
                      bucket_spent: &mut Vec<(usize, [u32; 6])>,
                      pool_spent: &mut [u32; 6],
                      snow_spent: &mut [u32; 6],
                      life_paid: &mut u32,
                      resolved_phy_flags: &mut SmallVec<[bool; 4]>,
                      new_gen: u32|
     -> bool {
        let mut ng = new_gen;
        solve_flex_pips_and_generic(
            work,
            flex,
            pos + 1,
            snow_count,
            &mut ng,
            conserve_snow_buckets,
            phyrexian_pay_life,
            allow_auto_phyrexian_life,
            bucket_spent,
            pool_spent,
            snow_spent,
            life_paid,
            resolved_phy_flags,
        )
    };

    match item.pip {
        ManaPip::Hybrid { a, b } => {
            for color in [a, b] {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                if consume_pip_color(
                    work,
                    color,
                    false,
                    conserve_snow_buckets,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                ) && try_branch(
                    work,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                    life_paid,
                    resolved_phy_flags,
                    *rem_generic,
                ) {
                    return true;
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            false
        }
        ManaPip::ColorlessHybrid { color } => {
            for c in [ManaColor::C, color] {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                if consume_pip_color(
                    work,
                    c,
                    false,
                    conserve_snow_buckets,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                ) && try_branch(
                    work,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                    life_paid,
                    resolved_phy_flags,
                    *rem_generic,
                ) {
                    return true;
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            false
        }
        ManaPip::TwoBrid { color } => {
            // Try colored first, then {2} generic
            {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                if consume_pip_color(
                    work,
                    color,
                    false,
                    conserve_snow_buckets,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                ) && try_branch(
                    work,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                    life_paid,
                    resolved_phy_flags,
                    *rem_generic,
                ) {
                    return true;
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            try_branch(
                work,
                bucket_spent,
                pool_spent,
                snow_spent,
                life_paid,
                resolved_phy_flags,
                rem_generic.saturating_add(2),
            )
        }
        ManaPip::Phyrexian { color } => {
            let p_idx = item.phy_idx.unwrap_or(0);
            {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                if consume_pip_color(
                    work,
                    color,
                    false,
                    conserve_snow_buckets,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                ) {
                    if let Some(slot) = resolved_phy_flags.get_mut(p_idx) {
                        *slot = false;
                    }
                    if try_branch(
                        work,
                        bucket_spent,
                        pool_spent,
                        snow_spent,
                        life_paid,
                        resolved_phy_flags,
                        *rem_generic,
                    ) {
                        return true;
                    }
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            if allow_auto_phyrexian_life && !work.cant_pay_life && work.life_available >= 2 {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                work.life_available -= 2;
                *life_paid += 2;
                if let Some(slot) = resolved_phy_flags.get_mut(p_idx) {
                    *slot = true;
                }
                if try_branch(
                    work,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                    life_paid,
                    resolved_phy_flags,
                    *rem_generic,
                ) {
                    return true;
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            false
        }
        ManaPip::HybridPhyrexian { a, b } => {
            let p_idx = item.phy_idx.unwrap_or(0);
            let explicit = phyrexian_pay_life.get(p_idx).copied();
            if explicit != Some(true) {
                for color in [a, b] {
                    let snap_w = work.clone();
                    let snap_b = bucket_spent.clone();
                    let snap_p = *pool_spent;
                    let snap_s = *snow_spent;
                    let snap_l = *life_paid;
                    let snap_f = resolved_phy_flags.clone();
                    if consume_pip_color(
                        work,
                        color,
                        false,
                        conserve_snow_buckets,
                        bucket_spent,
                        pool_spent,
                        snow_spent,
                    ) {
                        if let Some(slot) = resolved_phy_flags.get_mut(p_idx) {
                            *slot = false;
                        }
                        if try_branch(
                            work,
                            bucket_spent,
                            pool_spent,
                            snow_spent,
                            life_paid,
                            resolved_phy_flags,
                            *rem_generic,
                        ) {
                            return true;
                        }
                    }
                    *work = snap_w;
                    *bucket_spent = snap_b;
                    *pool_spent = snap_p;
                    *snow_spent = snap_s;
                    *life_paid = snap_l;
                    *resolved_phy_flags = snap_f;
                }
            }
            if (explicit == Some(true) || (explicit.is_none() && allow_auto_phyrexian_life))
                && !work.cant_pay_life
                && work.life_available >= 2
            {
                let snap_w = work.clone();
                let snap_b = bucket_spent.clone();
                let snap_p = *pool_spent;
                let snap_s = *snow_spent;
                let snap_l = *life_paid;
                let snap_f = resolved_phy_flags.clone();
                work.life_available -= 2;
                *life_paid += 2;
                if let Some(slot) = resolved_phy_flags.get_mut(p_idx) {
                    *slot = true;
                }
                if try_branch(
                    work,
                    bucket_spent,
                    pool_spent,
                    snow_spent,
                    life_paid,
                    resolved_phy_flags,
                    *rem_generic,
                ) {
                    return true;
                }
                *work = snap_w;
                *bucket_spent = snap_b;
                *pool_spent = snap_p;
                *snow_spent = snap_s;
                *life_paid = snap_l;
                *resolved_phy_flags = snap_f;
            }
            false
        }
        _ => try_branch(
            work,
            bucket_spent,
            pool_spent,
            snow_spent,
            life_paid,
            resolved_phy_flags,
            *rem_generic,
        ),
    }
}

fn consume_one_from_unrestricted_idx(
    work: &mut SimulatedManaPool,
    c_idx: usize,
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
) -> bool {
    if work.unrestricted[c_idx] == 0 {
        return false;
    }
    let non_snow = work.unrestricted[c_idx].saturating_sub(work.snow[c_idx]);
    work.unrestricted[c_idx] -= 1;
    pool_spent[c_idx] += 1;
    if non_snow == 0 && work.snow[c_idx] > 0 {
        work.snow[c_idx] -= 1;
        snow_spent[c_idx] += 1;
    }
    true
}

fn consume_generic_mana(
    work: &mut SimulatedManaPool,
    mut rem: u32,
    bucket_spent: &mut [(usize, [u32; 6])],
    pool_spent: &mut [u32; 6],
    snow_spent: &mut [u32; 6],
) -> bool {
    if rem == 0 {
        return true;
    }

    // Check X spend restrictions if any (`x_spend_colors` / `x_colored_only` / `x_max_one_per_color`)
    let restrict_colors = work.x_colored_only || !work.x_spend_colors.is_empty();
    let allowed_mask = if !work.x_spend_colors.is_empty() {
        work.x_spend_colors
    } else if work.x_colored_only {
        ColorMask::ALL_COLORS
    } else {
        ColorMask::ALL_COLORS | ColorMask::C
    };
    let mut per_color_used = [0u32; 6];

    // Tier 1: Spend from generic-eligible target-restricted buckets first (`!disallow_generic && has_spend_restriction`), colorless first.
    let order = [
        ManaColor::C,
        ManaColor::G,
        ManaColor::R,
        ManaColor::B,
        ManaColor::U,
        ManaColor::W,
    ];
    for (bi, b) in work.buckets.iter_mut().enumerate() {
        if b.disallow_generic || !b.has_spend_restriction {
            continue;
        }
        for c in order {
            if rem == 0 {
                return true;
            }
            if restrict_colors && !allowed_mask.contains(ColorMask::from_mana_color(c)) {
                continue;
            }
            let idx = color_to_index(c);
            while rem > 0 && b.colors[idx] > 0 {
                if work.x_max_one_per_color && per_color_used[idx] >= 1 {
                    break;
                }
                b.colors[idx] -= 1;
                bucket_spent[bi].1[idx] += 1;
                per_color_used[idx] += 1;
                rem -= 1;
            }
        }
    }

    // Tier 2: Spend colorless `{C}` from unrestricted pool first (Tier 2a), then non-spend-restricted `{C}` buckets (Tier 2b, non-snow before snow).
    if rem > 0 {
        let c_idx = color_to_index(ManaColor::C);
        if !restrict_colors || allowed_mask.contains(ColorMask::C) {
            while rem > 0 && work.unrestricted[c_idx] > 0 {
                if work.x_max_one_per_color && per_color_used[c_idx] >= 1 {
                    break;
                }
                if !consume_one_from_unrestricted_idx(work, c_idx, pool_spent, snow_spent) {
                    break;
                }
                per_color_used[c_idx] += 1;
                rem -= 1;
            }
            for snow_first in [false, true] {
                if rem == 0 || (work.x_max_one_per_color && per_color_used[c_idx] >= 1) {
                    break;
                }
                for (bi, b) in work.buckets.iter_mut().enumerate() {
                    if b.disallow_generic || b.has_spend_restriction || b.is_snow != snow_first {
                        continue;
                    }
                    while rem > 0 && b.colors[c_idx] > 0 {
                        if work.x_max_one_per_color && per_color_used[c_idx] >= 1 {
                            break;
                        }
                        b.colors[c_idx] -= 1;
                        bucket_spent[bi].1[c_idx] += 1;
                        per_color_used[c_idx] += 1;
                        rem -= 1;
                    }
                }
            }
        }
    }

    // Tier 3: Spend colored `{WUBRG}` across combined `unrestricted + !disallow_generic && !has_spend_restriction` buckets,
    // preferring `unrestricted[best_idx]` first, else non-snow then snow `!disallow_generic && !has_spend_restriction` bucket.
    while rem > 0 {
        let mut best_idx = None;
        let mut best_amt = 0u32;
        for c in FIVE_COLORS {
            if restrict_colors && !allowed_mask.contains(ColorMask::from_mana_color(c)) {
                continue;
            }
            let idx = color_to_index(c);
            if work.x_max_one_per_color && per_color_used[idx] >= 1 {
                continue;
            }
            let bucket_sum: u32 = work
                .buckets
                .iter()
                .filter(|b| !b.disallow_generic && !b.has_spend_restriction)
                .map(|b| b.colors[idx])
                .sum();
            let total_amt = work.unrestricted[idx].saturating_add(bucket_sum);
            if total_amt > best_amt {
                best_amt = total_amt;
                best_idx = Some(idx);
            }
        }
        let Some(idx) = best_idx else {
            break;
        };
        let mut consumed = consume_one_from_unrestricted_idx(work, idx, pool_spent, snow_spent);
        if !consumed {
            'bucket_pick: for snow_first in [false, true] {
                for (bi, b) in work.buckets.iter_mut().enumerate() {
                    if !b.disallow_generic
                        && !b.has_spend_restriction
                        && b.is_snow == snow_first
                        && b.colors[idx] > 0
                    {
                        b.colors[idx] -= 1;
                        bucket_spent[bi].1[idx] += 1;
                        consumed = true;
                        break 'bucket_pick;
                    }
                }
            }
        }
        if !consumed {
            break;
        }
        per_color_used[idx] += 1;
        rem -= 1;
    }

    rem == 0
}

pub fn deduct_mana_for_context(
    state: &mut GameState,
    player: PlayerId,
    cost: &ManaCost,
    ctx: &ManaSpendContext,
    phyrexian_pay_life: &[bool],
) -> Result<ManaPoolDeduction, EngineError> {
    let sim = SimulatedManaPool::from_state_cached(state, player, ctx, state.evaluate_board());
    let Some(plan) = solve_pool_deduction(&sim, cost, phyrexian_pay_life, true) else {
        return Err(EngineError::IllegalAction(
            "insufficient mana in pool to pay cost".into(),
        ));
    };

    let mut spent_counts = [0u32; 6];
    let mut spent_colors = ColorMask::empty();
    let mut total_spent = 0u32;
    let mut snow_spent_total = 0u32;
    let mut cant_be_countered_from_mana = false;
    let mut commander_bonus_counters = 0u32;
    let mut triggered_riders: Vec<(Option<ObjectId>, Arc<DelayedTriggerPayload>)> = Vec::new();

    {
        let Some(p) = state.player_mut(player) else {
            return Err(EngineError::IllegalAction("invalid player".into()));
        };

        let any_bucket_spent = plan
            .bucket_spent
            .iter()
            .any(|(_, counts)| counts.iter().any(|&v| v > 0));
        let has_ext_pool = p
            .mana_ext()
            .is_some_and(|e| e.retained_mana_counts != [0; 6] || e.snow_mana_pool != [0; 6]);

        // Deduct from restricted buckets
        if any_bucket_spent {
            let ext = p.mana_ext_mut();
            for &(orig_bi, ref counts) in &plan.bucket_spent {
                let bucket_total: u32 = counts.iter().sum();
                if bucket_total == 0 {
                    continue;
                }
                if let Some(bucket) = ext.restricted_mana.get_mut(orig_bi) {
                    for (i, &c_amt) in counts.iter().enumerate() {
                        if c_amt > 0 {
                            bucket.colors[i] = bucket.colors[i].saturating_sub(c_amt);
                            spent_counts[i] = spent_counts[i].saturating_add(c_amt);
                            total_spent = total_spent.saturating_add(c_amt);
                            spent_colors |= ColorMask::from_mana_color(index_to_color(i));
                            if bucket.is_snow {
                                snow_spent_total = snow_spent_total.saturating_add(c_amt);
                            }
                        }
                    }
                    if bucket.restriction.on_spend_cant_be_countered
                        && ctx.kind == ManaSpendContextKind::Spell
                    {
                        cant_be_countered_from_mana = true;
                    }
                    if bucket.restriction.on_spend_commander_bonus_counters
                        && ctx.kind == ManaSpendContextKind::Spell
                        && ctx.is_commander
                    {
                        commander_bonus_counters = commander_bonus_counters.saturating_add(1);
                    }
                    if let Some(rider) = &bucket.delayed_rider {
                        triggered_riders.push((bucket.source_id, Arc::clone(rider)));
                    }
                }
            }

            // Prune emptied buckets
            ext.restricted_mana
                .retain(|b| b.colors.iter().any(|&v| v > 0));
        }

        // Deduct from unrestricted pool & snow pool & retained counts
        for (i, &amt) in plan.pool_spent.iter().enumerate() {
            if amt > 0 {
                p.mana_pool[i] = p.mana_pool[i].saturating_sub(amt);
                let s_amt = plan.snow_spent[i];
                snow_spent_total = snow_spent_total.saturating_add(s_amt);
                spent_counts[i] = spent_counts[i].saturating_add(amt);
                total_spent = total_spent.saturating_add(amt);
                spent_colors |= ColorMask::from_mana_color(index_to_color(i));
            }
        }
        if has_ext_pool {
            let pool_after = p.mana_pool;
            let ext = p.mana_ext_mut();
            for (i, &amt) in plan.pool_spent.iter().enumerate() {
                if amt > 0 {
                    ext.retained_mana_counts[i] = ext.retained_mana_counts[i].min(pool_after[i]);
                    let s_amt = plan.snow_spent[i];
                    ext.snow_mana_pool[i] = ext.snow_mana_pool[i]
                        .saturating_sub(s_amt)
                        .min(pool_after[i]);
                }
            }
        }
        p.prune_mana_ext_if_default();
    }

    if plan.life_paid > 0 {
        if let Some(p) = state.player_mut(player) {
            p.life -= plan.life_paid as i32;
        }
        state.mark_dirty(DirtyMask::LIFE);
        emit_event(
            state,
            AtomicEventKind::LifeChange {
                player,
                delta: -(plan.life_paid as i32),
            },
        );
    }

    // Queue delayed riders from spent mana
    for (src_id, payload) in triggered_riders {
        let src_card = src_id
            .and_then(|id| state.object(id))
            .map_or(CardDefId(0), |o| o.card_def);
        let expiry = if payload.next_turn {
            DelayedExpiry::UntilYourNextTurn {
                creator: player,
                created_turn: state.turn_number(),
            }
        } else {
            DelayedExpiry::ThisTurn
        };
        triggers::register_delayed_trigger(
            state,
            DelayedTriggerEntry {
                id: 0,
                controller: player,
                source_card: src_card,
                source_object: src_id,
                source_lki: None,
                trigger: (*payload.trigger).clone(),
                condition: None,
                targets: payload.targets.clone(),
                effect: payload.effect.clone(),
                expiry,
                watched_objects: ctx.source_object.into_iter().collect(),
                x_value: 0,
            },
        );
        state.push_pending_mana_delayed_rider(PendingManaDelayedRider {
            controller: player,
            source: src_id,
            payload,
        });
    }

    if total_spent > 0 {
        let (prev_tot, new_tot) = if let Some(p) = state.player(player) {
            let prev = p.mana_expended_this_turn;
            (prev, prev.saturating_add(total_spent))
        } else {
            (0, total_spent)
        };
        state.mark_dirty(DirtyMask::PLAYER_STATE);
        emit_event(
            state,
            AtomicEventKind::ManaExpended {
                player,
                previous_total: prev_tot,
                new_total: new_tot,
                amount_spent: total_spent,
            },
        );
    } else {
        state.mark_dirty(DirtyMask::PLAYER_STATE);
    }

    Ok(ManaPoolDeduction {
        spent_colors,
        spent_counts,
        total_spent,
        snow_spent: snow_spent_total,
        life_paid_for_phyrexian: plan.life_paid,
        cant_be_countered_from_mana,
        commander_bonus_counters,
    })
}

fn static_retain_mask_and_convert_cached(
    state: &GameState,
    player: PlayerId,
    cache: &LayerCache,
) -> (ColorMask, Option<ManaColor>) {
    let mut retained = ColorMask::empty();
    let mut convert_to: Option<ManaColor> = None;

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
            if obj.zone == Zone::Command && !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            if obj.zone == Zone::Battlefield
                && cache.get(src_id).is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            if !object_might_have_mana_static(state, src_id, |st| {
                matches!(st.body, StaticBody::RetainUnspentMana { .. })
            }) {
                continue;
            }
            let src_ctrl = cache.get(src_id).map_or(obj.controller, |e| e.controller);
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            for ab_ref in active_abilities_with_card_for_object(state, src_id, &ctx) {
                if let AbilityDef::Static(st) = ab_ref.ability
                    && let StaticBody::RetainUnspentMana {
                        all_players,
                        colors,
                        convert_to: ct,
                    } = &st.static_def.body
                {
                    if !*all_players && src_ctrl != player {
                        continue;
                    }
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &ctx)
                    {
                        continue;
                    }
                    if let Some(target_c) = ct {
                        convert_to = Some(*target_c);
                    } else {
                        retained |= *colors;
                    }
                }
            }
        }
    }

    (retained, convert_to)
}

pub fn empty_mana_pools_for_step(state: &mut GameState) {
    let player_count = state.players().len();
    let mut static_configs: SmallVec<[(ColorMask, Option<ManaColor>); 4]> = SmallVec::new();
    {
        let cache = state.evaluate_board();
        for seat in 0..player_count {
            let pid = PlayerId(seat as u8);
            if state.player(pid).is_some_and(|p| !p.lost) {
                static_configs.push(static_retain_mask_and_convert_cached(state, pid, cache));
            } else {
                static_configs.push((ColorMask::empty(), None));
            }
        }
    }

    let mut changed = false;
    for (seat, (static_mask, convert_to)) in static_configs.into_iter().enumerate() {
        let pid = PlayerId(seat as u8);
        let Some(p) = state.player_mut(pid) else {
            continue;
        };
        let combined_retain = static_mask
            | p.unemptied_mana_colors_this_turn()
            | convert_to
                .map(ColorMask::from_mana_color)
                .unwrap_or_else(ColorMask::empty);
        let retained_counts = p.retained_mana_counts();

        let mut converted_amt = 0u32;
        let mut need_ext_sync = false;
        for (i, &c) in ALL_MANA_COLORS.iter().enumerate() {
            let cur = p.mana_pool[i];
            let cm = ColorMask::from_mana_color(c);
            if combined_retain.contains(cm) {
                continue;
            }
            let keep = retained_counts[i].min(cur);
            if cur > keep {
                if convert_to.is_some() {
                    converted_amt = converted_amt.saturating_add(cur - keep);
                }
                p.mana_pool[i] = keep;
                need_ext_sync = true;
                changed = true;
            }
        }

        if let Some(target_c) = convert_to
            && converted_amt > 0
        {
            let t_idx = color_to_index(target_c);
            p.mana_pool[t_idx] = p.mana_pool[t_idx].saturating_add(converted_amt);
            changed = true;
        }

        if let Some(ext_ro) = p.mana_ext() {
            let has_expired_buckets = ext_ro
                .restricted_mana
                .iter()
                .any(|b| !(b.retain_until_eot && b.colors.iter().any(|&v| v > 0)));
            let ext_needs_clamp = need_ext_sync
                && (0..6).any(|i| {
                    ext_ro.retained_mana_counts[i] > p.mana_pool[i]
                        || ext_ro.snow_mana_pool[i] > p.mana_pool[i]
                });
            if has_expired_buckets || ext_needs_clamp {
                let pool_after = p.mana_pool;
                let ext = p.mana_ext_mut();
                for i in 0..6 {
                    ext.retained_mana_counts[i] = ext.retained_mana_counts[i].min(pool_after[i]);
                    ext.snow_mana_pool[i] = ext.snow_mana_pool[i].min(pool_after[i]);
                }
                let before_buckets = ext.restricted_mana.len();
                ext.restricted_mana
                    .retain(|b| b.retain_until_eot && b.colors.iter().any(|&v| v > 0));
                if ext.restricted_mana.len() != before_buckets {
                    changed = true;
                }
                p.prune_mana_ext_if_default();
            }
        }
    }

    if changed {
        state.mark_dirty(DirtyMask::PLAYER_STATE);
    }
}

pub fn clear_mana_pools_at_cleanup(state: &mut GameState) {
    let player_count = state.players().len();
    let mut changed = false;
    for seat in 0..player_count {
        if let Some(p) = state.player_mut(PlayerId(seat as u8)) {
            if p.mana_pool != [0; 6] {
                p.mana_pool = [0; 6];
                changed = true;
            }
            if let Some(ext) = p.mana_ext()
                && (ext.snow_mana_pool != [0; 6]
                    || ext.retained_mana_counts != [0; 6]
                    || !ext.restricted_mana.is_empty()
                    || !ext.unemptied_mana_colors_this_turn.is_empty())
            {
                let m = p.mana_ext_mut();
                m.snow_mana_pool = [0; 6];
                m.retained_mana_counts = [0; 6];
                m.restricted_mana.clear();
                m.unemptied_mana_colors_this_turn = ColorMask::empty();
                p.prune_mana_ext_if_default();
                changed = true;
            }
        }
    }
    if changed {
        state.mark_dirty(DirtyMask::PLAYER_STATE);
    }
}

#[allow(dead_code)]
fn _build_lki_fallback(state: &GameState, id: ObjectId) -> Option<LkiSnapshot> {
    let obj = state.object(id)?;
    let card = state.db().get(obj.card_def)?;
    let face = card.faces.get(obj.active_face_index() as usize)?;
    Some(LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name: face.name.clone(),
        owner: obj.owner,
        controller: obj.controller,
        last_zone: obj.zone,
        pt: None,
        types: face.characteristics.types,
        subtypes: face.characteristics.subtypes.iter().copied().collect(),
        supertypes: face.characteristics.supertypes,
        colors: face.characteristics.colors,
        mana_value: face.characteristics.mana_cost.mana_value(),
        keywords: SmallVec::new(),
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.damage_marked,
        is_token: obj.is_token(),
        is_commander: obj.is_commander(),
        tapped: obj.flags.contains(ObjectFlags::TAPPED),
        phased_out: obj.is_phased_out(),
        face_down: obj.is_face_down(),
        lost_all_abilities: false,
        has_all_creature_types: false,
        attached_ids: SmallVec::new(),
        chosen_color: state.rare(id).and_then(|r| r.chosen_color),
        chosen_subtype: state.rare(id).and_then(|r| r.chosen_subtype),
        chosen_card_name: state.rare(id).and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.active_face_index(),
    })
}

#[allow(dead_code)]
fn _move_helper_unused(_o: MoveObjectOutcome, _r: MoveRawOptions) {
    let _ = zones::cleanup_tokens_outside_battlefield;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{GameObject, ObjectFlags};
    use mtg_ir::{
        AbilityDef, ActivatedAbility, CardDef, CardDefId, CardLayout, CardTypeMask,
        Characteristics, ColorMask, EffectStep, FaceDef, IR_V2_VERSION, InternTables, IrV2Bundle,
        Layer, LayerMod, ManaCost, PlainManaFilter, Selector, StaticAbility, StaticBody, StaticDef,
        SubtypeId, SupertypeMask, Timing, Zone,
    };
    use std::sync::Arc;

    fn make_land_card(
        name: &str,
        color: ManaColor,
        supertypes: SupertypeMask,
        subtypes: SmallVec<[SubtypeId; 3]>,
        restriction_spec: Option<ManaSpendingRestriction>,
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
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::LAND,
                    subtypes,
                    supertypes,
                    colors: ColorMask::empty(),
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "tap_mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: SmallVec::from_slice(&[color]),
                            },
                            restriction: None,
                            restriction_spec: restriction_spec.map(Box::new),
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
        }
    }

    fn make_spell_card(name: &str, cost_raw: &str, types: CardTypeMask) -> CardDef {
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        }
    }

    fn boot_state(cards: Vec<CardDef>, tables: InternTables) -> GameState {
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
        GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 7).unwrap()
    }

    fn put_on_battlefield(state: &mut GameState, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let obj = GameObject::new(def, owner, Zone::Battlefield, ts, ObjectFlags::empty());
        let oid = state.insert_object(obj, None);
        state.player_mut(owner).unwrap().battlefield.push(oid);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        oid
    }

    fn put_in_hand(state: &mut GameState, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let obj = GameObject::new(def, owner, Zone::Hand, ts, ObjectFlags::empty());
        let oid = state.insert_object(obj, None);
        state.player_mut(owner).unwrap().hand.push(oid);
        state.mark_dirty(DirtyMask::TOPOLOGY);
        oid
    }

    #[test]
    fn restricted_mana_creature_only_and_disallow_generic_and_cant_be_countered() {
        let restriction = ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disallow_generic: true,
            on_spend_cant_be_countered: true,
            ..Default::default()
        };
        let cards = vec![
            make_land_card(
                "Synth Cavern",
                ManaColor::G,
                SupertypeMask::empty(),
                SmallVec::new(),
                Some(restriction),
            ),
            make_spell_card("Synth Bear", "{1}{G}", CardTypeMask::CREATURE),
            make_spell_card("Synth Bolt", "{G}", CardTypeMask::INSTANT),
        ];
        let mut state = boot_state(cards, InternTables::default());
        let land = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let bear = put_in_hand(&mut state, PlayerId(0), CardDefId(1));
        let bolt = put_in_hand(&mut state, PlayerId(0), CardDefId(2));

        let out = activate_mana_ability(&mut state, PlayerId(0), land, 0, None).unwrap();
        assert_eq!(out, ManaActivationOutcome::Produced);
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
        assert_eq!(
            state.player(PlayerId(0)).unwrap().restricted_mana().len(),
            1
        );

        let bolt_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(bolt),
            from_zone: Zone::Hand,
            ..Default::default()
        };
        assert!(!can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{G}"),
            &bolt_ctx
        ));

        let bear_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(bear),
            from_zone: Zone::Hand,
            ..Default::default()
        };
        // disallow_generic prevents paying {1}{G} with two restricted {G}
        let r_spec = (*state.player(PlayerId(0)).unwrap().restricted_mana()[0].restriction).clone();
        add_mana_from_production(
            &mut state,
            PlayerId(0),
            Some(land),
            &ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::G]),
            },
            None,
            Some(&r_spec),
            false,
            None,
            None,
            1,
        )
        .unwrap();
        assert!(!can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{1}{G}"),
            &bear_ctx
        ));
        // But adding 1 unrestricted {C} lets it pay {1}{G}, granting cant_be_countered
        state.player_mut(PlayerId(0)).unwrap().mana_pool[5] = 1;
        let ded = deduct_mana_for_context(
            &mut state,
            PlayerId(0),
            &ManaCost::parse("{1}{G}"),
            &bear_ctx,
            &[],
        )
        .unwrap();
        assert!(ded.cant_be_countered_from_mana);
        assert_eq!(ded.total_spent, 2);
    }

    #[test]
    fn snow_mana_conservation_and_spend_as_any_color_excludes_colorless() {
        let cards = vec![
            make_land_card(
                "Synth Snow Forest",
                ManaColor::G,
                SupertypeMask::SNOW,
                SmallVec::new(),
                None,
            ),
            make_land_card(
                "Synth Normal Forest",
                ManaColor::G,
                SupertypeMask::empty(),
                SmallVec::new(),
                None,
            ),
            make_spell_card("Synth AnyColor Grant", "{0}", CardTypeMask::ENCHANTMENT),
        ];
        let mut state = boot_state(cards, InternTables::default());
        let snow_land = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let norm_land = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        activate_mana_ability(&mut state, PlayerId(0), snow_land, 0, None).unwrap();
        activate_mana_ability(&mut state, PlayerId(0), norm_land, 0, None).unwrap();
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 2);
        assert_eq!(state.player(PlayerId(0)).unwrap().snow_mana_pool()[4], 1);

        // Paying {G}{S} consumes non-snow G first, then snow G
        let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
        let ded = deduct_mana_for_context(
            &mut state,
            PlayerId(0),
            &ManaCost::parse("{G}{S}"),
            &ctx,
            &[],
        )
        .unwrap();
        assert_eq!(ded.snow_spent, 1);
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 0);

        // SpendManaAsAnyColor allows {G} to pay {U}, but NOT {C}
        state.player_mut(PlayerId(0)).unwrap().mana_pool[4] = 1;
        let any_ctx = ManaSpendContext {
            player: PlayerId(0),
            alt_cast_spend_any_color: true,
            ..Default::default()
        };
        assert!(can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{U}"),
            &any_ctx
        ));
        assert!(!can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{C}"),
            &any_ctx
        ));
    }

    #[test]
    fn humility_keep_mana_abilities_vs_blood_moon_innate_mana_removed() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Mountain".into());
        let mountain_sub = SubtypeId(0);

        let dual_card = make_land_card(
            "Synth Dual",
            ManaColor::U,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        let blood_moon = CardDef {
            oracle_id: "oracle_bm".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Moon".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Moon".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::ENCHANTMENT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::R,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer4,
                            affected: mtg_ir::Affected::Filter {
                                filter: Box::new(mtg_ir::ObjectFilter {
                                    types_any: CardTypeMask::LAND,
                                    supertypes_none: SupertypeMask::BASIC,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::SetSubtypes {
                                subtypes: SmallVec::from_slice(&[mountain_sub]),
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
        let humility_keep_mana = CardDef {
            oracle_id: "oracle_hum".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth KeepMana".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth KeepMana".into(),
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
                abilities: vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: mtg_ir::Affected::Filter {
                                filter: Box::new(mtg_ir::ObjectFilter {
                                    types_any: CardTypeMask::LAND,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::RemoveAllAbilities {
                                keep_mana_abilities: true,
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

        let mut state = boot_state(vec![dual_card, blood_moon, humility_keep_mana], tables);
        let dual = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));

        // With KeepManaAbilities, innate mana ability survives
        let hum = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        let cands = enumerate_mana_abilities(&state, PlayerId(0));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].object, dual);
        assert_eq!(cands[0].ability_index, 0);
        assert!(cands[0].produces_colors.contains(ColorMask::U));
        let _ = hum;

        // With Blood Moon (SetSubtypes -> Mountain), innate mana ability is removed and Mountain {R} granted at index 1
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        let cands_bm = enumerate_mana_abilities(&state, PlayerId(0));
        assert_eq!(cands_bm.len(), 1);
        assert_eq!(cands_bm[0].is_intrinsic_basic, Some(ManaColor::R));
        assert_eq!(cands_bm[0].ability_index, 1);
        activate_mana_ability(&mut state, PlayerId(0), dual, 1, None).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 0, 1, 0, 0]
        );
    }

    #[test]
    fn mana_choice_pauses_and_resumes_with_prompt() {
        let choice_land = CardDef {
            oracle_id: "oracle_any".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Orchard".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Orchard".into(),
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
                    id: "any_mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Choice {
                                kind: ManaChoiceKind::Any,
                                colors: ColorMask::empty(),
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
        let mut state = boot_state(vec![choice_land], InternTables::default());
        state.set_decision(DecisionState::Priority);
        let orchard = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));

        let res = activate_mana_ability(&mut state, PlayerId(0), orchard, 0, None).unwrap();
        let ManaActivationOutcome::Paused { prompt_id, chooser } = res else {
            panic!("expected pause for color choice");
        };
        assert_eq!(chooser, PlayerId(0));
        assert!(matches!(
            state.decision(),
            DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode {
                    option_count: 5,
                    ..
                }
            }
        ));

        // Pick index 1 (U)
        let res2 = resume_mana_color_choice(
            &mut state,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::PickMode { index: 1 },
        )
        .unwrap();
        assert_eq!(res2, ManaActivationOutcome::Produced);
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 1, 0, 0, 0, 0]
        );
        assert_eq!(state.decision(), &DecisionState::Priority);
    }

    #[test]
    fn tap_for_mana_multiplier_and_step_emptying() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Forest".into());
        let forest = make_land_card(
            "Synth Forest",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::from_slice(&[SubtypeId(0)]),
            None,
        );
        let doubler = CardDef {
            oracle_id: "oracle_dbl".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Doubler".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Doubler".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
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
                        body: StaticBody::TapForManaMultiplier {
                            factor: 2,
                            filter: Box::new(mtg_ir::ObjectFilter {
                                types_any: CardTypeMask::LAND,
                                ..Default::default()
                            }),
                            all_players: false,
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
        let mut state = boot_state(vec![forest, doubler], tables);
        let f_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

        activate_mana_ability(&mut state, PlayerId(0), f_id, 0, None).unwrap();
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 2);

        empty_mana_pools_for_step(&mut state);
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 0);
    }

    #[test]
    fn restricted_snow_mana_pays_snow_pip_and_preserves_cr_106_6_order() {
        let restriction = ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disallow_generic: true,
            ..Default::default()
        };
        let snow_cavern = make_land_card(
            "Synth Snow Cavern",
            ManaColor::G,
            SupertypeMask::SNOW,
            SmallVec::new(),
            Some(restriction),
        );
        let normal_forest = make_land_card(
            "Synth Plain Forest",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        let bear = make_spell_card("Synth Snow Bear", "{G}{S}", CardTypeMask::CREATURE);
        let snow_only = make_spell_card("Synth Ice Bear", "{S}", CardTypeMask::CREATURE);
        let instant = make_spell_card("Synth Ice Bolt", "{S}", CardTypeMask::INSTANT);

        let mut state = boot_state(
            vec![snow_cavern, normal_forest, bear, snow_only, instant],
            InternTables::default(),
        );
        let cavern_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let forest_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        let bear_id = put_in_hand(&mut state, PlayerId(0), CardDefId(2));
        let snow_only_id = put_in_hand(&mut state, PlayerId(0), CardDefId(3));
        let instant_id = put_in_hand(&mut state, PlayerId(0), CardDefId(4));

        activate_mana_ability(&mut state, PlayerId(0), cavern_id, 0, None).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().restricted_mana().len(),
            1
        );
        assert!(state.player(PlayerId(0)).unwrap().restricted_mana()[0].is_snow);

        let snow_only_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(snow_only_id),
            from_zone: Zone::Hand,
            ..Default::default()
        };
        let instant_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(instant_id),
            from_zone: Zone::Hand,
            ..Default::default()
        };
        // Non-creature spell cannot spend creature-restricted snow bucket on {S}
        assert!(!can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{S}"),
            &instant_ctx
        ));
        // Creature spell CAN pay {S} using restricted snow bucket
        assert!(can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{S}"),
            &snow_only_ctx
        ));

        // Add 1 unrestricted non-snow {G} -> pay {G}{S} on creature:
        // {G} uses unrestricted non-snow {G}, {S} uses restricted snow {G}
        activate_mana_ability(&mut state, PlayerId(0), forest_id, 0, None).unwrap();
        let bear_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(bear_id),
            from_zone: Zone::Hand,
            ..Default::default()
        };
        let ded = deduct_mana_for_context(
            &mut state,
            PlayerId(0),
            &ManaCost::parse("{G}{S}"),
            &bear_ctx,
            &[],
        )
        .unwrap();
        assert_eq!(ded.snow_spent, 1);
        assert_eq!(ded.total_spent, 2);
        assert!(
            state
                .player(PlayerId(0))
                .unwrap()
                .restricted_mana()
                .is_empty()
        );
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
    }

    #[test]
    fn plain_mana_and_cleanup_prune_mana_ext_to_none() {
        let normal_forest = make_land_card(
            "Synth Plain Forest",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        let snow_forest = make_land_card(
            "Synth Snow Forest",
            ManaColor::G,
            SupertypeMask::SNOW,
            SmallVec::new(),
            None,
        );
        let mut state = boot_state(vec![normal_forest, snow_forest], InternTables::default());
        let norm_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let snow_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

        // Plain mana must NOT allocate mana_ext
        activate_mana_ability(&mut state, PlayerId(0), norm_id, 0, None).unwrap();
        assert!(state.player(PlayerId(0)).unwrap().mana_ext().is_none());

        empty_mana_pools_for_step(&mut state);
        assert!(state.player(PlayerId(0)).unwrap().mana_ext().is_none());

        // Snow mana allocates mana_ext, and cleanup prunes it back to None
        activate_mana_ability(&mut state, PlayerId(0), snow_id, 0, None).unwrap();
        assert!(state.player(PlayerId(0)).unwrap().mana_ext().is_some());

        clear_mana_pools_at_cleanup(&mut state);
        assert!(state.player(PlayerId(0)).unwrap().mana_ext().is_none());
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
    }

    #[test]
    fn restricted_snow_sg_mixed_pool_and_combinations() {
        let creature_only_dg_false = ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disallow_generic: false,
            ..Default::default()
        };
        let creature_only_dg_true = ManaSpendingRestriction {
            disallow_generic: true,
            ..creature_only_dg_false.clone()
        };
        let snow_cavern_dg_false = make_land_card(
            "Synth Snow Cavern F",
            ManaColor::G,
            SupertypeMask::SNOW,
            SmallVec::new(),
            Some(creature_only_dg_false.clone()),
        );
        let snow_cavern_dg_true = make_land_card(
            "Synth Snow Cavern T",
            ManaColor::G,
            SupertypeMask::SNOW,
            SmallVec::new(),
            Some(creature_only_dg_true.clone()),
        );
        let snow_wastes = make_land_card(
            "Synth Snow Wastes",
            ManaColor::C,
            SupertypeMask::SNOW,
            SmallVec::new(),
            None,
        );
        let bear = make_spell_card("Synth Bear", "{S}{G}", CardTypeMask::CREATURE);

        // 1. {S}{G} succeeds with 1 snow creature-only {G} (both dg=false and dg=true) + 1 unrestricted snow {C}
        for use_dg_true in [false, true] {
            let mut state = boot_state(
                vec![
                    snow_cavern_dg_false.clone(),
                    snow_cavern_dg_true.clone(),
                    snow_wastes.clone(),
                    bear.clone(),
                ],
                InternTables::default(),
            );
            let cavern_def = if use_dg_true {
                CardDefId(1)
            } else {
                CardDefId(0)
            };
            let cav_id = put_on_battlefield(&mut state, PlayerId(0), cavern_def);
            let waste_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let bear_id = put_in_hand(&mut state, PlayerId(0), CardDefId(3));

            activate_mana_ability(&mut state, PlayerId(0), cav_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), waste_id, 0, None).unwrap();

            let ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: ManaSpendContextKind::Spell,
                source_object: Some(bear_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };
            assert!(can_pay_mana_from_pool(
                &state,
                PlayerId(0),
                &ManaCost::parse("{S}{G}"),
                &ctx
            ));
            let ded = deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &ManaCost::parse("{S}{G}"),
                &ctx,
                &[],
            )
            .unwrap();
            assert_eq!(ded.snow_spent, 2);
            assert_eq!(ded.total_spent, 2);
            assert!(
                state
                    .player(PlayerId(0))
                    .unwrap()
                    .restricted_mana()
                    .is_empty()
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
        }

        // 2. {S}{S}{W}{U}: 1 dg=true restricted snow {W} + 1 unrestricted non-snow {W}
        //    + 1 restricted non-snow {U} + 1 unrestricted snow {C}
        {
            let snow_w_land = make_land_card(
                "Synth Snow W",
                ManaColor::W,
                SupertypeMask::SNOW,
                SmallVec::new(),
                Some(creature_only_dg_true.clone()),
            );
            let plain_u_land = make_land_card(
                "Synth Plain U",
                ManaColor::U,
                SupertypeMask::empty(),
                SmallVec::new(),
                Some(creature_only_dg_false.clone()),
            );
            let plain_w_land = make_land_card(
                "Synth Plain W",
                ManaColor::W,
                SupertypeMask::empty(),
                SmallVec::new(),
                None,
            );
            let mut state = boot_state(
                vec![
                    snow_w_land,
                    plain_u_land,
                    plain_w_land,
                    snow_wastes.clone(),
                    bear.clone(),
                ],
                InternTables::default(),
            );
            let sw_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            let pu_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            let pw_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let sc_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
            let b_id = put_in_hand(&mut state, PlayerId(0), CardDefId(4));

            activate_mana_ability(&mut state, PlayerId(0), sw_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), pu_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), pw_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), sc_id, 0, None).unwrap();

            let ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: ManaSpendContextKind::Spell,
                source_object: Some(b_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };
            let cost = ManaCost::parse("{S}{S}{W}{U}");
            assert!(can_pay_mana_from_pool(&state, PlayerId(0), &cost, &ctx));
            let ded = deduct_mana_for_context(&mut state, PlayerId(0), &cost, &ctx, &[]).unwrap();
            assert_eq!(ded.snow_spent, 2);
            assert_eq!(ded.total_spent, 4);
            assert!(
                state
                    .player(PlayerId(0))
                    .unwrap()
                    .restricted_mana()
                    .is_empty()
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
        }

        // 3. {2}{S}{G}: 1 dg=true restricted snow {G} + 1 unrestricted snow {C}
        //    + 1 dg=false restricted non-snow {W} + 1 non-snow {R}
        {
            let plain_w_res = make_land_card(
                "Synth Plain W Res",
                ManaColor::W,
                SupertypeMask::empty(),
                SmallVec::new(),
                Some(creature_only_dg_false.clone()),
            );
            let plain_r = make_land_card(
                "Synth Plain R",
                ManaColor::R,
                SupertypeMask::empty(),
                SmallVec::new(),
                None,
            );
            let mut state = boot_state(
                vec![
                    snow_cavern_dg_true.clone(),
                    snow_wastes.clone(),
                    plain_w_res,
                    plain_r,
                    bear.clone(),
                ],
                InternTables::default(),
            );
            let g_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            let sc_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            let w_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let r_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
            let b_id = put_in_hand(&mut state, PlayerId(0), CardDefId(4));

            activate_mana_ability(&mut state, PlayerId(0), g_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), sc_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), w_id, 0, None).unwrap();
            activate_mana_ability(&mut state, PlayerId(0), r_id, 0, None).unwrap();

            let ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: ManaSpendContextKind::Spell,
                source_object: Some(b_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };
            let cost = ManaCost::parse("{2}{S}{G}");
            assert!(can_pay_mana_from_pool(&state, PlayerId(0), &cost, &ctx));
            let ded = deduct_mana_for_context(&mut state, PlayerId(0), &cost, &ctx, &[]).unwrap();
            assert_eq!(ded.snow_spent, 2);
            assert_eq!(ded.total_spent, 4);
            assert!(
                state
                    .player(PlayerId(0))
                    .unwrap()
                    .restricted_mana()
                    .is_empty()
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
        }

        // 4. Hybrid {S}{W/U} & {S}{U/W}, TwoBrid {S}{2/G}, ColorlessHybrid {S}{C/G},
        //    Phyrexian {S}{G/P}, and {X=1}{S} (x_colored_only = true)
        {
            let snow_w_land = make_land_card(
                "Synth Snow W",
                ManaColor::W,
                SupertypeMask::SNOW,
                SmallVec::new(),
                None,
            );
            let plain_u_land = make_land_card(
                "Synth Plain U",
                ManaColor::U,
                SupertypeMask::empty(),
                SmallVec::new(),
                None,
            );
            let snow_g_land = make_land_card(
                "Synth Snow G",
                ManaColor::G,
                SupertypeMask::SNOW,
                SmallVec::new(),
                None,
            );
            let mut state = boot_state(
                vec![
                    snow_w_land,
                    plain_u_land,
                    snow_g_land,
                    snow_wastes,
                    snow_cavern_dg_false,
                    bear,
                ],
                InternTables::default(),
            );
            let sw_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            let pu_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            let sg_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let sc_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
            let cav_f_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            let b_id = put_in_hand(&mut state, PlayerId(0), CardDefId(5));

            let ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: ManaSpendContextKind::Spell,
                source_object: Some(b_id),
                from_zone: Zone::Hand,
                ..Default::default()
            };

            // {S}{W/U} and {S}{U/W} with 1 snow {W} + 1 non-snow {U}
            for cost_str in ["{S}{W/U}", "{S}{U/W}"] {
                clear_mana_pools_at_cleanup(&mut state);
                state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
                // Manually populate pool: 1 snow W + 1 non-snow U
                add_mana_from_production(
                    &mut state,
                    PlayerId(0),
                    Some(sw_id),
                    &ManaProduction::Fixed {
                        symbols: SmallVec::from_slice(&[ManaColor::W]),
                    },
                    None,
                    None,
                    false,
                    None,
                    None,
                    1,
                )
                .unwrap();
                add_mana_from_production(
                    &mut state,
                    PlayerId(0),
                    Some(pu_id),
                    &ManaProduction::Fixed {
                        symbols: SmallVec::from_slice(&[ManaColor::U]),
                    },
                    None,
                    None,
                    false,
                    None,
                    None,
                    1,
                )
                .unwrap();
                let ded = deduct_mana_for_context(
                    &mut state,
                    PlayerId(0),
                    &ManaCost::parse(cost_str),
                    &ctx,
                    &[],
                )
                .unwrap();
                assert_eq!(ded.snow_spent, 1);
                assert_eq!(ded.total_spent, 2);
            }

            // {S}{2/G} with 1 snow {G} + 1 non-snow {U} -> wait: {S} needs snow, {2/G} needs {G}
            // Wait: 1 snow {G} + 1 snow {C}: {2/G} takes {G} or {2}, {S} takes {C}!
            clear_mana_pools_at_cleanup(&mut state);
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sg_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sc_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            let ded_2g = deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &ManaCost::parse("{S}{2/G}"),
                &ctx,
                &[],
            )
            .unwrap();
            assert_eq!(ded_2g.snow_spent, 2);
            assert_eq!(ded_2g.total_spent, 2);

            // {S}{C/G} with 1 snow {G} + 1 non-snow {W} -> {C/G} uses snow {G}? No, with 1 snow {C} + 1 non-snow {U}:
            // wait: with 1 snow {G} + 1 snow {C}, {S}{C/G} uses snow {C} (for C/G) and snow {G} (for S) cleanly!
            clear_mana_pools_at_cleanup(&mut state);
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sg_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sc_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            let ded_cg = deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &ManaCost::parse("{S}{C/G}"),
                &ctx,
                &[],
            )
            .unwrap();
            assert_eq!(ded_cg.snow_spent, 2);
            assert_eq!(ded_cg.total_spent, 2);

            // {S}{G/P} with 1 snow {G} + 20 life -> pays {G/P} with 2 life and {S} with snow {G}!
            clear_mana_pools_at_cleanup(&mut state);
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sg_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            let cost_phy = ManaCost::parse("{S}{G/P}");
            assert!(can_pay_mana_from_pool(&state, PlayerId(0), &cost_phy, &ctx));
            let ded_phy =
                deduct_mana_for_context(&mut state, PlayerId(0), &cost_phy, &ctx, &[]).unwrap();
            assert_eq!(ded_phy.snow_spent, 1);
            assert_eq!(ded_phy.life_paid_for_phyrexian, 2);
            assert_eq!(ded_phy.total_spent, 1);

            // {X=1}{S} (x_colored_only = true) with 1 dg=false restricted snow {G} + 1 unrestricted snow {C}
            clear_mana_pools_at_cleanup(&mut state);
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(cav_f_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                None,
                Some(&creature_only_dg_false),
                false,
                None,
                None,
                1,
            )
            .unwrap();
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(sc_id),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::C]),
                },
                None,
                None,
                false,
                None,
                None,
                1,
            )
            .unwrap();
            let x_ctx = ManaSpendContext {
                x_colored_only: true,
                ..ctx.clone()
            };
            let cost_xs = ManaCost::parse("{1}{S}");
            assert!(can_pay_mana_from_pool(
                &state,
                PlayerId(0),
                &cost_xs,
                &x_ctx
            ));
            let ded_xs =
                deduct_mana_for_context(&mut state, PlayerId(0), &cost_xs, &x_ctx, &[]).unwrap();
            assert_eq!(ded_xs.snow_spent, 2);
            assert_eq!(ded_xs.total_spent, 2);
        }
    }

    #[test]
    fn allow_abilities_chosen_subtype_fallback_after_source_leaves_battlefield() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Elf".into());
        tables.subtypes.push("Goblin".into());

        let ability_restriction = ManaSpendingRestriction {
            allow_abilities: Some(AllowAbilitiesRestriction {
                source_filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    of_chosen_type: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let tribal_land = make_land_card(
            "Synth Tribal Reservoir",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::new(),
            Some(ability_restriction),
        );
        let mut elf_creature = make_spell_card("Synth Elf", "{G}", CardTypeMask::CREATURE);
        elf_creature.faces[0].characteristics.subtypes = SmallVec::from_slice(&[SubtypeId(0)]);
        let mut goblin_creature = make_spell_card("Synth Goblin", "{R}", CardTypeMask::CREATURE);
        goblin_creature.faces[0].characteristics.subtypes = SmallVec::from_slice(&[SubtypeId(1)]);

        let mut state = boot_state(vec![tribal_land, elf_creature, goblin_creature], tables);
        let land_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        state.rare_or_default(land_id).unwrap().chosen_subtype = Some(SubtypeId(0));
        let elf_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        let goblin_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));

        // Produce ability-restricted mana from `land_id` while chosen_subtype == Elf
        activate_mana_ability(&mut state, PlayerId(0), land_id, 0, None).unwrap();
        assert_eq!(
            state.player(PlayerId(0)).unwrap().restricted_mana()[0]
                .restriction
                .source_chosen_subtype
                .as_deref(),
            Some("Elf")
        );

        // Move/exile `land_id` off the battlefield (CR 400.7 bumps generation / clears rare)
        zones::move_object_raw(
            &mut state,
            land_id,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let elf_ab_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::ActivatedAbility,
            source_object: Some(elf_id),
            from_zone: Zone::Battlefield,
            ..Default::default()
        };
        let goblin_ab_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::ActivatedAbility,
            source_object: Some(goblin_id),
            from_zone: Zone::Battlefield,
            ..Default::default()
        };

        assert!(can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{G}"),
            &elf_ab_ctx
        ));
        assert!(!can_pay_mana_from_pool(
            &state,
            PlayerId(0),
            &ManaCost::parse("{G}"),
            &goblin_ab_ctx
        ));
    }

    #[test]
    fn cached_mana_enumeration_and_pool_payment_parity_with_synth_nexus_64bf() {
        let mut tables = InternTables::default();
        tables.subtypes.push("Dragon".into());
        let dragon_sub = SubtypeId(0);

        let forest = make_land_card(
            "Forest",
            ManaColor::G,
            SupertypeMask::BASIC,
            SmallVec::new(),
            None,
        );
        let dragon_land = make_land_card(
            "Dragon Haven",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::new(),
            Some(ManaSpendingRestriction {
                allow_spells: Some(AllowSpellsRestriction {
                    filter: Some(PlainManaFilter {
                        subtypes_any: SmallVec::from_slice(&[dragon_sub]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        let mut synth_nexus = make_spell_card("Synth Nexus", "", CardTypeMask::ARTIFACT);
        synth_nexus.faces[0].abilities = vec![AbilityDef::Static(StaticAbility {
            static_def: StaticDef {
                body: StaticBody::OffBattlefieldCharacteristics {
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
        })];
        let filler = make_spell_card("Filler", "", CardTypeMask::ARTIFACT);
        let bear = make_spell_card("Bear", "{1}{G}", CardTypeMask::CREATURE);

        let mut state = boot_state(vec![forest, dragon_land, synth_nexus, filler, bear], tables);
        let forest_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
        let dland_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
        put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
        for _ in 0..61 {
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
        }
        let bear_id = put_in_hand(&mut state, PlayerId(0), CardDefId(4));

        let r_spec = ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    subtypes_any: SmallVec::from_slice(&[dragon_sub]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        add_mana_from_production(
            &mut state,
            PlayerId(0),
            Some(dland_id),
            &ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::G]),
            },
            None,
            Some(&r_spec),
            false,
            None,
            None,
            1,
        )
        .unwrap();

        let bear_ctx = ManaSpendContext {
            player: PlayerId(0),
            kind: ManaSpendContextKind::Spell,
            source_object: Some(bear_id),
            from_zone: Zone::Hand,
            ..Default::default()
        };

        let uncached_cands = enumerate_mana_abilities(&state, PlayerId(0));
        let uncached_can_pay =
            can_pay_mana_from_pool(&state, PlayerId(0), &ManaCost::parse("{G}"), &bear_ctx);

        let cache = state.evaluate_board();
        let cached_cands = enumerate_mana_abilities_cached(&state, PlayerId(0), cache);
        let cached_can_pay = can_pay_mana_from_pool_cached(
            &state,
            PlayerId(0),
            &ManaCost::parse("{G}"),
            &bear_ctx,
            cache,
        );

        assert_eq!(cached_cands.len(), 2);
        assert_eq!(cached_cands.len(), uncached_cands.len());
        let cached_objs: Vec<_> = cached_cands.iter().map(|c| c.object).collect();
        let uncached_objs: Vec<_> = uncached_cands.iter().map(|c| c.object).collect();
        assert_eq!(cached_objs, uncached_objs);
        assert!(cached_objs.contains(&forest_id));
        assert!(cached_objs.contains(&dland_id));
        assert!(cached_can_pay);
        assert_eq!(cached_can_pay, uncached_can_pay);
    }

    #[test]
    fn synth_tap_self_plus_tap_others_mana_ability() {
        let make_creature_mana = |name: &str, cost: Vec<CostDef>| CardDef {
            oracle_id: format!("oracle_{name}"),
            compiler_version: 46,
            text_hash: "test".into(),
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
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Activated(ActivatedAbility {
                    id: "mana_ab".into(),
                    cost,
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Choice {
                                kind: mtg_ir::ManaChoiceKind::Any,
                                colors: ColorMask::empty(),
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

        let untapped_cre_filter = Box::new(mtg_ir::ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::UNTAPPED,
            ..Default::default()
        });
        let tapped_cre_filter = Box::new(mtg_ir::ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(mtg_ir::RelativeControl::You),
            flags: mtg_ir::FilterFlags::TAPPED,
            ..Default::default()
        });

        let sentinel_fwd = make_creature_mana(
            "Synth Mana Sentinel",
            vec![
                CostDef::TapSelf,
                CostDef::TapOthers {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    filter: untapped_cre_filter.clone(),
                    share_creature_type: false,
                },
            ],
        );
        let sentinel_rev = make_creature_mana(
            "Synth Reversed Sentinel",
            vec![
                CostDef::TapOthers {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    filter: untapped_cre_filter,
                    share_creature_type: false,
                },
                CostDef::TapSelf,
            ],
        );
        let sac_mana_cre = make_creature_mana(
            "Synth Sac Sentinel",
            vec![
                CostDef::TapSelf,
                CostDef::Sacrifice {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    target: mtg_ir::CostObjectTarget::Filter {
                        filter: Box::new(mtg_ir::ObjectFilter {
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
                },
            ],
        );
        let untap_mana_cre = make_creature_mana(
            "Synth Untap Sentinel",
            vec![
                CostDef::UntapSelf { via_symbol: true },
                CostDef::UntapOthers {
                    count: Box::new(mtg_ir::Amount::Const { n: 1 }),
                    filter: tapped_cre_filter,
                },
            ],
        );
        let bear = CardDef {
            oracle_id: "oracle_synth_vanilla_bear".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Vanilla Bear".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Vanilla Bear".into(),
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

        // Case A & B: [TapSelf, TapOthers] alone vs with Synth Vanilla Bear
        {
            let mut state = boot_state(
                vec![
                    sentinel_fwd.clone(),
                    sentinel_rev.clone(),
                    sac_mana_cre.clone(),
                    untap_mana_cre.clone(),
                    bear.clone(),
                ],
                InternTables::default(),
            );
            state.set_decision(DecisionState::Priority);
            let s_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));

            assert!(enumerate_mana_abilities(&state, PlayerId(0)).is_empty());
            assert!(!can_activate_mana_ability(&state, PlayerId(0), s_id, 0));
            assert!(activate_mana_ability(&mut state, PlayerId(0), s_id, 0, None).is_err());

            let b_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            assert_eq!(enumerate_mana_abilities(&state, PlayerId(0)).len(), 1);
            let res = activate_mana_ability(&mut state, PlayerId(0), s_id, 0, None).unwrap();
            let ManaActivationOutcome::Paused { prompt_id, .. } = res else {
                panic!("expected color choice pause");
            };
            let res2 = resume_mana_color_choice(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(res2, ManaActivationOutcome::Produced);
            assert!(
                state
                    .object(s_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(b_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 1, 0, 0, 0, 0]
            );
        }

        // Case C: Reversed [TapOthers, TapSelf] alone vs with Bear
        {
            let mut state = boot_state(
                vec![
                    sentinel_fwd.clone(),
                    sentinel_rev,
                    sac_mana_cre.clone(),
                    untap_mana_cre.clone(),
                    bear.clone(),
                ],
                InternTables::default(),
            );
            state.set_decision(DecisionState::Priority);
            let s_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            assert!(enumerate_mana_abilities(&state, PlayerId(0)).is_empty());
            assert!(activate_mana_ability(&mut state, PlayerId(0), s_id, 0, None).is_err());

            let b_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            let res = activate_mana_ability(&mut state, PlayerId(0), s_id, 0, Some(ManaColor::G))
                .unwrap();
            assert_eq!(res, ManaActivationOutcome::Produced);
            assert!(
                state
                    .object(s_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                state
                    .object(b_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 1);
        }

        // Case D: [TapSelf, Sacrifice(Creature)] sole creature on BF IS legal per CR 118.3
        {
            let mut state = boot_state(
                vec![
                    sentinel_fwd.clone(),
                    sentinel_fwd.clone(),
                    sac_mana_cre,
                    untap_mana_cre.clone(),
                    bear.clone(),
                ],
                InternTables::default(),
            );
            state.set_decision(DecisionState::Priority);
            let s_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            assert_eq!(enumerate_mana_abilities(&state, PlayerId(0)).len(), 1);
            let res = activate_mana_ability(&mut state, PlayerId(0), s_id, 0, Some(ManaColor::R))
                .unwrap();
            assert_eq!(res, ManaActivationOutcome::Produced);
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[3], 1);
            assert!(state.player(PlayerId(0)).unwrap().battlefield.is_empty());
        }

        // Case E: [UntapSelf, UntapOthers] alone vs with 2nd tapped creature
        {
            let mut state = boot_state(
                vec![
                    sentinel_fwd.clone(),
                    sentinel_fwd.clone(),
                    sentinel_fwd,
                    untap_mana_cre,
                    bear,
                ],
                InternTables::default(),
            );
            state.set_decision(DecisionState::Priority);
            let u_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
            state
                .object_mut(u_id)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);

            assert!(enumerate_mana_abilities(&state, PlayerId(0)).is_empty());

            let b_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            state
                .object_mut(b_id)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            state.mark_dirty(DirtyMask::TAPPED);

            assert_eq!(enumerate_mana_abilities(&state, PlayerId(0)).len(), 1);
            let res = activate_mana_ability(&mut state, PlayerId(0), u_id, 0, Some(ManaColor::W))
                .unwrap();
            assert_eq!(res, ManaActivationOutcome::Produced);
            assert!(
                !state
                    .object(u_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert!(
                !state
                    .object(b_id)
                    .unwrap()
                    .flags
                    .contains(ObjectFlags::TAPPED)
            );
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[0], 1);
        }
    }

    #[test]
    fn synth_multi_symbol_fixed_bundles_and_chosen_color_amount_ignore_redundant_step_amount() {
        // 0. Direct add_mana_ignores_step_amount matrix
        assert!(!add_mana_ignores_step_amount(&ManaProduction::Fixed {
            symbols: SmallVec::from_slice(&[ManaColor::G]),
        }));
        assert!(add_mana_ignores_step_amount(&ManaProduction::Fixed {
            symbols: SmallVec::from_slice(&[ManaColor::W, ManaColor::U]),
        }));
        assert!(add_mana_ignores_step_amount(&ManaProduction::Bundles {
            options: vec![SmallVec::from_slice(&[ManaColor::W])],
        }));
        assert!(add_mana_ignores_step_amount(
            &ManaProduction::ChosenColorAmount {
                amount: Box::new(mtg_ir::Amount::Const { n: 3 }),
            }
        ));
        assert!(add_mana_ignores_step_amount(
            &ManaProduction::OrChosenColor {
                colors: ColorMask::W,
            }
        ));
        assert!(add_mana_ignores_step_amount(
            &ManaProduction::WithChosenColor {
                colors: ColorMask::B,
            }
        ));
        assert!(!add_mana_ignores_step_amount(&ManaProduction::Choice {
            kind: mtg_ir::ManaChoiceKind::Any,
            colors: ColorMask::empty(),
        }));
        assert!(!add_mana_ignores_step_amount(
            &ManaProduction::Combination {
                any_color: true,
                colors: ColorMask::empty(),
            }
        ));
        assert!(!add_mana_ignores_step_amount(
            &ManaProduction::FromExiledColors
        ));
        assert!(!add_mana_ignores_step_amount(
            &ManaProduction::PerColorControlled
        ));
        assert!(!add_mana_ignores_step_amount(
            &ManaProduction::LandProducedTypes
        ));
        assert!(!add_mana_ignores_step_amount(
            &ManaProduction::FromProducedEvent
        ));

        let make_mana_land = |name: &str,
                              cost: Vec<CostDef>,
                              production: ManaProduction,
                              step_amount: Option<mtg_ir::Amount>| {
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
                        id: "mana_ab".into(),
                        cost,
                        targets: vec![],
                        effect: vec![EffectStep {
                            subject: Selector::SelfObj,
                            op: EffectOp::AddMana {
                                production,
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                                delayed_rider: None,
                            },
                            actor: None,
                            amount: step_amount.map(Box::new),
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
            }
        };

        let mana_1_cost = CostDef::Mana {
            cost: ManaCost::parse("{1}"),
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
        };
        let filter_dual = make_mana_land(
            "Synth Filter Dual",
            vec![mana_1_cost.clone(), CostDef::TapSelf],
            ManaProduction::Fixed {
                symbols: SmallVec::from_slice(&[ManaColor::W, ManaColor::U]),
            },
            Some(mtg_ir::Amount::Const { n: 2 }),
        );
        let hybrid_filter = make_mana_land(
            "Synth Hybrid Filter",
            vec![mana_1_cost, CostDef::TapSelf],
            ManaProduction::Bundles {
                options: vec![
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::W]),
                    SmallVec::from_slice(&[ManaColor::W, ManaColor::B]),
                    SmallVec::from_slice(&[ManaColor::B, ManaColor::B]),
                ],
            },
            Some(mtg_ir::Amount::Const { n: 2 }),
        );
        let shrine = make_mana_land(
            "Synth Shrine",
            vec![CostDef::TapSelf],
            ManaProduction::ChosenColorAmount {
                amount: Box::new(mtg_ir::Amount::Devotion {
                    colors: ColorMask::G,
                    player: None,
                }),
            },
            Some(mtg_ir::Amount::Devotion {
                colors: ColorMask::G,
                player: None,
            }),
        );
        let plains = make_land_card(
            "Synth Plains",
            ManaColor::W,
            SupertypeMask::BASIC,
            SmallVec::new(),
            None,
        );
        let green_perm = make_spell_card("Synth Triple Green", "{G}{G}{G}", CardTypeMask::CREATURE);
        let doubler = CardDef {
            oracle_id: "oracle_synth_doubler_q".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Mana Doubler".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Mana Doubler".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
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
                        body: StaticBody::TapForManaMultiplier {
                            factor: 2,
                            filter: Box::new(mtg_ir::ObjectFilter::default()),
                            all_players: false,
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
        let or_chosen = make_mana_land(
            "Synth OrChosen Land",
            vec![CostDef::TapSelf],
            ManaProduction::OrChosenColor {
                colors: ColorMask::W,
            },
            Some(mtg_ir::Amount::Const { n: 2 }),
        );
        let with_chosen = make_mana_land(
            "Synth WithChosen Land",
            vec![CostDef::TapSelf],
            ManaProduction::WithChosenColor {
                colors: ColorMask::B,
            },
            Some(mtg_ir::Amount::Const { n: 2 }),
        );

        let cards = vec![
            filter_dual,
            hybrid_filter,
            shrine,
            plains,
            green_perm,
            doubler,
            or_chosen,
            with_chosen,
        ];

        // 1. Synth Filter Dual without and with TapForManaMultiplier
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_decision(DecisionState::Priority);
            let dual_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            let _plains_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let dual_cand = cands.iter().find(|c| c.object == dual_id).unwrap();
            assert_eq!(dual_cand.produces_amounts, [1, 1, 0, 0, 0, 0]);

            // Auto-tap: {W}{U} succeeds (Plains -> {W}, Filter Dual -> {W}{U}), {2}{W}{U} fails
            let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
            assert!(
                crate::auto_tap::plan_auto_tap(
                    &state,
                    PlayerId(0),
                    &ManaCost::parse("{W}{U}"),
                    &ctx,
                    crate::auto_tap::AutoTapPolicy::default(),
                )
                .can_pay
            );
            assert!(
                !crate::auto_tap::plan_auto_tap(
                    &state,
                    PlayerId(0),
                    &ManaCost::parse("{2}{W}{U}"),
                    &ctx,
                    crate::auto_tap::AutoTapPolicy::default(),
                )
                .can_pay
            );

            state.player_mut(PlayerId(0)).unwrap().mana_pool[5] = 1;
            activate_mana_ability(&mut state, PlayerId(0), dual_id, 0, None).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [1, 1, 0, 0, 0, 0]
            );

            // Untap dual and add TapForManaMultiplier(2) -> produces [2, 2, 0, 0, 0, 0]
            state
                .object_mut(dual_id)
                .unwrap()
                .flags
                .remove(ObjectFlags::TAPPED);
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 0, 1];
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            let cands2 = enumerate_mana_abilities(&state, PlayerId(0));
            let dual_cand2 = cands2.iter().find(|c| c.object == dual_id).unwrap();
            assert_eq!(dual_cand2.produces_amounts, [2, 2, 0, 0, 0, 0]);
            activate_mana_ability(&mut state, PlayerId(0), dual_id, 0, None).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [2, 2, 0, 0, 0, 0]
            );
        }

        // 2. Synth Hybrid Filter (Bundles) without and with TapForManaMultiplier
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_decision(DecisionState::Priority);
            let hf_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let hf_cand = cands.iter().find(|c| c.object == hf_id).unwrap();
            assert_eq!(hf_cand.bundle_options.len(), 3);
            assert!(hf_cand.bundle_options.iter().all(|b| b.len() == 2));

            state.player_mut(PlayerId(0)).unwrap().mana_pool[5] = 1;
            let res = activate_mana_ability(&mut state, PlayerId(0), hf_id, 0, None).unwrap();
            let ManaActivationOutcome::Paused { prompt_id, .. } = res else {
                panic!("expected bundle choice pause");
            };
            // Index 1 is [W, B]
            resume_mana_color_choice(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [1, 0, 1, 0, 0, 0]
            );

            // With TapForManaMultiplier(2) -> [2, 0, 2, 0, 0, 0]
            state
                .object_mut(hf_id)
                .unwrap()
                .flags
                .remove(ObjectFlags::TAPPED);
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 0, 1];
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            let cands2 = enumerate_mana_abilities(&state, PlayerId(0));
            let hf_cand2 = cands2.iter().find(|c| c.object == hf_id).unwrap();
            assert!(hf_cand2.bundle_options.iter().all(|b| b.len() == 4));
            let res2 = activate_mana_ability(&mut state, PlayerId(0), hf_id, 0, None).unwrap();
            let ManaActivationOutcome::Paused {
                prompt_id: pid2, ..
            } = res2
            else {
                panic!("expected bundle choice pause");
            };
            resume_mana_color_choice(
                &mut state,
                PlayerId(0),
                pid2,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [2, 0, 2, 0, 0, 0]
            );
        }

        // 3. Synth Shrine (ChosenColorAmount with Devotion = 3 -> 3 {G}, not 9; 6 {G} with factor=2)
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_decision(DecisionState::Priority);
            let shrine_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            state.rare_or_default(shrine_id).unwrap().chosen_color = Some(ColorMask::G);
            let _ggg_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let shrine_cand = cands.iter().find(|c| c.object == shrine_id).unwrap();
            assert_eq!(shrine_cand.produces_amounts, [0, 0, 0, 0, 3, 0]);

            activate_mana_ability(&mut state, PlayerId(0), shrine_id, 0, None).unwrap();
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 3);

            state
                .object_mut(shrine_id)
                .unwrap()
                .flags
                .remove(ObjectFlags::TAPPED);
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            activate_mana_ability(&mut state, PlayerId(0), shrine_id, 0, None).unwrap();
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool[4], 6);
        }

        // 4. OrChosenColor & WithChosenColor ignore redundant step.amount
        {
            let mut state = boot_state(cards, InternTables::default());
            state.set_decision(DecisionState::Priority);
            let or_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(6));
            state.rare_or_default(or_id).unwrap().chosen_color = Some(ColorMask::R);
            let with_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(7));
            state.rare_or_default(with_id).unwrap().chosen_color = Some(ColorMask::U);
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            activate_mana_ability(&mut state, PlayerId(0), or_id, 0, Some(ManaColor::W)).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [1, 0, 0, 0, 0, 0]
            );
            activate_mana_ability(&mut state, PlayerId(0), with_id, 0, None).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [1, 1, 1, 0, 0, 0]
            );
        }
    }

    #[test]
    fn synth_chosen_color_amount_devotion_choose_on_resolve_all_colors() {
        use crate::DecisionKind;
        use crate::action::{Action, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
        use crate::session::MatchSession;
        use mtg_ir::{
            Amount, DesignationKind, ObjectFilter, PrintedPt, RelativeControl, SpellAbility,
            TargetKind, TargetSpec, ZoneMask,
        };
        use smallvec::smallvec;

        let wastes = make_land_card(
            "Synth Wastes",
            ManaColor::C,
            SupertypeMask::BASIC,
            SmallVec::new(),
            None,
        );

        let shrine_choose_on_resolve = CardDef {
            oracle_id: "oracle_synth_devotion_shrine".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Devotion Shrine".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Devotion Shrine".into(),
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
                    id: "shrine_devotion".into(),
                    cost: vec![
                        CostDef::Mana {
                            cost: ManaCost::parse("{2}"),
                            x_spend_colors: ColorMask::empty(),
                            x_colored_only: false,
                            x_max_one_per_color: false,
                        },
                        CostDef::TapSelf,
                    ],
                    targets: vec![],
                    effect: vec![
                        EffectStep {
                            subject: Selector::None,
                            op: EffectOp::ChooseDesignation {
                                kind: DesignationKind::Color,
                            },
                            actor: None,
                            amount: None,
                            optional: None,
                            gate: None,
                            bind_as: None,
                            do_this_once_per_turn: false,
                            reflexive_trigger: None,
                            span: Default::default(),
                        },
                        EffectStep {
                            subject: Selector::SelfObj,
                            op: EffectOp::AddMana {
                                production: ManaProduction::ChosenColorAmount {
                                    amount: Box::new(Amount::Devotion {
                                        colors: ColorMask::W,
                                        player: None,
                                    }),
                                },
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                                delayed_rider: None,
                            },
                            actor: None,
                            amount: Some(Box::new(Amount::Devotion {
                                colors: ColorMask::W,
                                player: None,
                            })),
                            optional: None,
                            gate: None,
                            bind_as: None,
                            do_this_once_per_turn: false,
                            reflexive_trigger: None,
                            span: Default::default(),
                        },
                    ],
                    modal: None,
                    timing: Timing::Instant,
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
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let battery = CardDef {
            oracle_id: "oracle_synth_battery".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Devotion Battery".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Devotion Battery".into(),
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
                    id: "battery_mana".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::Fixed {
                                symbols: smallvec![
                                    ManaColor::C,
                                    ManaColor::C,
                                    ManaColor::W,
                                    ManaColor::U,
                                    ManaColor::U,
                                    ManaColor::B,
                                    ManaColor::B,
                                    ManaColor::B,
                                    ManaColor::R,
                                    ManaColor::G,
                                    ManaColor::G,
                                    ManaColor::G,
                                    ManaColor::G,
                                ],
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

        let make_cre = |name: &str, cost: &str, color: ColorMask| {
            let mut c = make_spell_card(name, cost, CardTypeMask::CREATURE);
            c.faces[0].characteristics.colors = color;
            c.faces[0].characteristics.pt = Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            });
            c
        };

        let bounce_instant = CardDef {
            oracle_id: "oracle_synth_bounce".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Bounce Instant".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Bounce Instant".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::INSTANT,
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::U,
                    color_indicator: None,
                    pt: None,
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![AbilityDef::Spell(SpellAbility {
                    id: Some("bounce".into()),
                    additional_costs: vec![],
                    targets: vec![TargetSpec {
                        kind: TargetKind::Object,
                        filter: ObjectFilter {
                            types_any: CardTypeMask::ARTIFACT,
                            controller: Some(RelativeControl::You),
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
                    }],
                    effect: vec![EffectStep {
                        subject: Selector::Target { index: 0 },
                        op: EffectOp::Bounce {
                            select: Default::default(),
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

        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards: vec![
                wastes,
                shrine_choose_on_resolve,
                battery,
                make_cre("Synth White Pip", "{W}", ColorMask::W),
                make_cre("Synth Blue Pips", "{U}{U}", ColorMask::U),
                make_cre("Synth Black Pips", "{B}{B}{B}", ColorMask::B),
                make_cre("Synth Red Pip", "{R}", ColorMask::R),
                make_cre("Synth Green Pips", "{G}{G}{G}{G}", ColorMask::G),
                bounce_instant,
            ],
            decks: vec![],
        };
        let db = Arc::new(mtg_ir::CardDatabase::from_bundle(bundle).unwrap());
        let p0_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(4),
                CardDefId(5),
                CardDefId(6),
                CardDefId(7),
                CardDefId(8),
            ],
            sideboard: vec![],
        };
        let p1_deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 10],
            sideboard: vec![],
        };
        let mut session =
            MatchSession::new(db, GameConfig::standard_1v1(), vec![p0_deck, p1_deck], 42).unwrap();
        session.apply_action(PlayerId(0), Action::KeepHand).unwrap();
        session.apply_action(PlayerId(1), Action::KeepHand).unwrap();

        // Advance to P0's second turn Main1 so all 8 cards are in P0's hand.
        for _ in 0..128 {
            let kind = session.decision_info().state_kind;
            if session.state().active_player() == PlayerId(0)
                && session.state().turn_number() >= 2
                && session.state().phase() == GamePhase::Main1
                && session.current_actor() == Some(PlayerId(0))
                && kind == DecisionKind::Priority
            {
                break;
            }
            let actor = session.current_actor().unwrap();
            let act = match kind {
                DecisionKind::DeclareAttackers => Action::ConfirmAttackers,
                DecisionKind::DeclareBlockers => Action::ConfirmBlockers,
                DecisionKind::AssignCombatDamage | DecisionKind::Prompt => session
                    .legal_actions(actor)
                    .into_iter()
                    .find(|a| !matches!(a, Action::Concede))
                    .unwrap(),
                _ => Action::PassPriority,
            };
            session.apply_action(actor, act).unwrap();
        }
        assert_eq!(session.state().active_player(), PlayerId(0));
        assert!(session.state().turn_number() >= 2);
        assert_eq!(session.state().player(PlayerId(0)).unwrap().hand.len(), 8);

        let find_hand = |s: &MatchSession, def: CardDefId| -> ObjectId {
            s.state()
                .player(PlayerId(0))
                .unwrap()
                .hand
                .iter()
                .copied()
                .find(|&id| s.state().object(id).unwrap().card_def == def)
                .unwrap()
        };
        let find_bf = |s: &MatchSession, def: CardDefId| -> ObjectId {
            s.state()
                .player(PlayerId(0))
                .unwrap()
                .battlefield
                .iter()
                .copied()
                .find(|&id| s.state().object(id).unwrap().card_def == def)
                .unwrap()
        };
        let resolve_top = |s: &mut MatchSession| {
            for _ in 0..2 {
                let actor = s.current_actor().unwrap();
                s.apply_action(actor, Action::PassPriority).unwrap();
            }
        };

        // Cast Shrine and Battery ({0})
        for def in [CardDefId(1), CardDefId(2)] {
            let oid = find_hand(&session, def);
            session
                .apply_action(
                    PlayerId(0),
                    Action::BeginCast {
                        object: oid,
                        option_id: 0,
                        from_zone: Zone::Hand,
                    },
                )
                .unwrap();
            resolve_top(&mut session);
        }

        // Tap Battery to fill mana pool: [1, 2, 3, 1, 4, 2]
        let battery_id = find_bf(&session, CardDefId(2));
        session
            .apply_action(
                PlayerId(0),
                Action::TapMana {
                    object: battery_id,
                    ability_index: 0,
                    preferred_color: None,
                },
            )
            .unwrap();

        // Cast all 5 colored permanents (W=1, U=2, B=3, R=1, G=4)
        for def in [
            CardDefId(3),
            CardDefId(4),
            CardDefId(5),
            CardDefId(6),
            CardDefId(7),
        ] {
            let oid = find_hand(&session, def);
            session
                .apply_action(
                    PlayerId(0),
                    Action::BeginCast {
                        object: oid,
                        option_id: 0,
                        from_zone: Zone::Hand,
                    },
                )
                .unwrap();
            session
                .apply_action(PlayerId(0), Action::PayFromPool)
                .unwrap();
            resolve_top(&mut session);
        }
        assert_eq!(
            session.state().player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 0, 0, 0, 2]
        );

        let shrine_id = find_bf(&session, CardDefId(1));
        let base_session = session;

        // Test all 5 color choices (0=W, 1=U, 2=B, 3=R, 4=G)
        let expected_pools = [
            [1, 0, 0, 0, 0, 0],
            [0, 2, 0, 0, 0, 0],
            [0, 0, 3, 0, 0, 0],
            [0, 0, 0, 1, 0, 0],
            [0, 0, 0, 0, 4, 0],
        ];
        for (idx, expected_pool) in expected_pools.iter().enumerate() {
            let mut s = base_session.clone();
            s.apply_action(
                PlayerId(0),
                Action::BeginActivate {
                    object: shrine_id,
                    ability_index: 0,
                },
            )
            .unwrap();
            s.apply_action(PlayerId(0), Action::PayFromPool).unwrap();
            resolve_top(&mut s);

            let DecisionState::Prompt {
                request:
                    ChoiceRequest::ChooseMode {
                        prompt_id,
                        option_count: 5,
                        min: 1,
                        max: 1,
                    },
            } = s.state().decision()
            else {
                panic!("expected ChooseMode(5) for color designation");
            };
            let pid = *prompt_id;
            s.apply_action(
                PlayerId(0),
                Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickMode { index: idx as u16 },
                },
            )
            .unwrap();
            assert_eq!(
                s.state().player(PlayerId(0)).unwrap().mana_pool,
                *expected_pool,
                "color index {idx} mismatch"
            );
        }

        // Test chosen_color == None when Shrine leaves BF before ChooseDesignation resolves -> 0 mana
        {
            let mut s = base_session;
            s.apply_action(
                PlayerId(0),
                Action::BeginActivate {
                    object: shrine_id,
                    ability_index: 0,
                },
            )
            .unwrap();
            s.apply_action(PlayerId(0), Action::PayFromPool).unwrap();
            let bounce_id = find_hand(&s, CardDefId(8));
            s.apply_action(
                PlayerId(0),
                Action::BeginCast {
                    object: bounce_id,
                    option_id: 0,
                    from_zone: Zone::Hand,
                },
            )
            .unwrap();
            s.apply_action(
                PlayerId(0),
                Action::SelectTarget {
                    target: TargetRef::Object { id: shrine_id },
                },
            )
            .unwrap();
            // Resolve bounce instant, then resolve Shrine ability
            resolve_top(&mut s);
            resolve_top(&mut s);
            let DecisionState::Prompt {
                request: ChoiceRequest::ChooseMode { prompt_id, .. },
            } = s.state().decision()
            else {
                panic!("expected ChooseMode prompt even after source bounced");
            };
            let pid = *prompt_id;
            s.apply_action(
                PlayerId(0),
                Action::Answer {
                    prompt_id: pid,
                    answer: ChoiceAnswer::PickMode { index: 4 },
                },
            )
            .unwrap();
            assert_eq!(s.state().player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
        }
    }

    #[test]
    fn synth_chosen_color_amount_devotion_mana_ability_estimation_and_multiplier() {
        use crate::action::{ChoiceAnswer, ChoiceRequest, DecisionState};
        use mtg_ir::Amount;

        let make_mana_land = |name: &str, amount: Amount| CardDef {
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
                    id: "mana_ab".into(),
                    cost: vec![CostDef::TapSelf],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::SelfObj,
                        op: EffectOp::AddMana {
                            production: ManaProduction::ChosenColorAmount {
                                amount: Box::new(amount.clone()),
                            },
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        actor: None,
                        amount: Some(Box::new(amount)),
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

        let eternal_shrine = make_mana_land(
            "Synth Eternal Shrine",
            Amount::Devotion {
                colors: ColorMask::W,
                player: None,
            },
        );
        let scaled_shrine = make_mana_land(
            "Synth Scaled Shrine",
            Amount::Mul {
                factor: 2,
                of: Box::new(Amount::Devotion {
                    colors: ColorMask::W,
                    player: None,
                }),
            },
        );
        let ggg_perm = make_spell_card("Synth Triple Green", "{G}{G}{G}", CardTypeMask::CREATURE);
        let uu_perm = make_spell_card("Synth Double Blue", "{U}{U}", CardTypeMask::CREATURE);
        let doubler = CardDef {
            oracle_id: "oracle_synth_doubler_u".into(),
            compiler_version: 46,
            text_hash: "test".into(),
            name: "Synth Mana Doubler".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Mana Doubler".into(),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
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
                        body: StaticBody::TapForManaMultiplier {
                            factor: 2,
                            filter: Box::new(mtg_ir::ObjectFilter::default()),
                            all_players: false,
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
        let cards = vec![eternal_shrine, scaled_shrine, ggg_perm, uu_perm, doubler];

        // 1. Permanent chosen_color = G with placeholder Devotion { W } -> estimates 3 {G} (auto-tap + multiplier)
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_decision(DecisionState::Priority);
            let shrine_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            state.rare_or_default(shrine_id).unwrap().chosen_color = Some(ColorMask::G);
            let _ggg_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let cand = cands.iter().find(|c| c.object == shrine_id).unwrap();
            assert_eq!(cand.produces_amounts, [0, 0, 0, 0, 3, 0]);

            let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
            assert!(
                crate::auto_tap::plan_auto_tap(
                    &state,
                    PlayerId(0),
                    &ManaCost::parse("{G}{G}{G}"),
                    &ctx,
                    crate::auto_tap::AutoTapPolicy::default(),
                )
                .can_pay
            );
            assert!(
                !crate::auto_tap::plan_auto_tap(
                    &state,
                    PlayerId(0),
                    &ManaCost::parse("{G}{G}{G}{G}"),
                    &ctx,
                    crate::auto_tap::AutoTapPolicy::default(),
                )
                .can_pay
            );

            put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            let cands2 = enumerate_mana_abilities(&state, PlayerId(0));
            let cand2 = cands2.iter().find(|c| c.object == shrine_id).unwrap();
            assert_eq!(cand2.produces_amounts, [0, 0, 0, 0, 6, 0]);
            activate_mana_ability(&mut state, PlayerId(0), shrine_id, 0, None).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 0, 0, 0, 6, 0]
            );
        }

        // 2. chosen_color = None on is_mana: true -> estimates [0; 6], prompts FIVE_COLORS, PickMode(1)=U adds 2 {U} (4 {U} with doubler)
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_decision(DecisionState::Priority);
            let shrine_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            let _uu_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let cand = cands.iter().find(|c| c.object == shrine_id).unwrap();
            assert_eq!(cand.produces_amounts, [0; 6]);

            let res = activate_mana_ability(&mut state, PlayerId(0), shrine_id, 0, None).unwrap();
            let ManaActivationOutcome::Paused { prompt_id, .. } = res else {
                panic!("expected FIVE_COLORS prompt when chosen_color is None");
            };
            assert!(matches!(
                state.decision(),
                DecisionState::Prompt {
                    request: ChoiceRequest::ChooseMode {
                        option_count: 5,
                        min: 1,
                        max: 1,
                        ..
                    }
                }
            ));
            resume_mana_color_choice(
                &mut state,
                PlayerId(0),
                prompt_id,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 2, 0, 0, 0, 0]
            );

            state
                .object_mut(shrine_id)
                .unwrap()
                .flags
                .remove(ObjectFlags::TAPPED);
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            let res2 = activate_mana_ability(&mut state, PlayerId(0), shrine_id, 0, None).unwrap();
            let ManaActivationOutcome::Paused {
                prompt_id: pid2, ..
            } = res2
            else {
                panic!("expected FIVE_COLORS prompt");
            };
            resume_mana_color_choice(
                &mut state,
                PlayerId(0),
                pid2,
                ChoiceAnswer::PickMode { index: 1 },
            )
            .unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 4, 0, 0, 0, 0]
            );
        }

        // 3. Amount::Mul { factor: 2, of: Devotion { W } } + depth guard
        {
            let mut state = boot_state(cards, InternTables::default());
            state.set_decision(DecisionState::Priority);
            let scaled_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            state.rare_or_default(scaled_id).unwrap().chosen_color = Some(ColorMask::G);
            let _ggg_id = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let cand = cands.iter().find(|c| c.object == scaled_id).unwrap();
            assert_eq!(cand.produces_amounts, [0, 0, 0, 0, 6, 0]);
            activate_mana_ability(&mut state, PlayerId(0), scaled_id, 0, None).unwrap();
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 0, 0, 0, 6, 0]
            );

            // Recursion depth guard returns 0 beyond MAX_EVAL_DEPTH
            let mut deep_amt = Amount::Devotion {
                colors: ColorMask::W,
                player: None,
            };
            for _ in 0..=crate::eval::MAX_EVAL_DEPTH {
                deep_amt = Amount::Mul {
                    factor: 1,
                    of: Box::new(deep_amt),
                };
            }
            let ctx =
                EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(state.evaluate_board()));
            assert_eq!(
                evaluate_chosen_color_amount(&state, &deep_amt, ColorMask::G, &ctx),
                0
            );
        }
    }

    #[test]
    fn synth_delayed_rider_and_commander_bonus_generic_spending_order_and_auto_pay() {
        let scry_rider = DelayedTriggerPayload {
            trigger: Box::new(mtg_ir::TriggerDef {
                event: mtg_ir::EventKind::ZoneChange,
                filter: mtg_ir::EventFilter::default(),
                scope: None,
                from_zones: None,
                only_once_per_turn: false,
                do_this_once_per_turn: false,
                is_state_trigger: false,
                or: vec![],
                chapter_numbers: SmallVec::new(),
                is_chapter_ability: false,
                spell_delayed: None,
            }),
            effect: vec![],
            targets: vec![],
            next_turn: false,
        };
        let make_custom_mana_perm =
            |name: &str,
             types: CardTypeMask,
             extra_costs: Vec<CostDef>,
             symbols: Vec<ManaColor>,
             restriction_spec: Option<ManaSpendingRestriction>,
             delayed_rider: Option<DelayedTriggerPayload>| {
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
                            types,
                            subtypes: SmallVec::new(),
                            supertypes: SupertypeMask::empty(),
                            colors: ColorMask::empty(),
                            color_indicator: None,
                            pt: None,
                            loyalty: None,
                            defense: None,
                        },
                        abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                            id: "mana_ab".into(),
                            cost: {
                                let mut c = vec![CostDef::TapSelf];
                                c.extend(extra_costs);
                                c
                            },
                            targets: vec![],
                            effect: vec![EffectStep {
                                subject: Selector::SelfObj,
                                op: EffectOp::AddMana {
                                    production: ManaProduction::Fixed {
                                        symbols: SmallVec::from_vec(symbols),
                                    },
                                    restriction: None,
                                    restriction_spec: restriction_spec.map(Box::new),
                                    retain_until_eot: false,
                                    delayed_rider: delayed_rider.map(Box::new),
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
                }
            };

        let make_beacon = |name: &str, cost_raw: &str| CardDef {
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
                abilities: vec![AbilityDef::Activated(mtg_ir::ActivatedAbility {
                    id: "beacon_ab".into(),
                    cost: vec![CostDef::Mana {
                        cost: ManaCost::parse(cost_raw),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    }],
                    targets: vec![],
                    effect: vec![EffectStep {
                        subject: Selector::Player {
                            filter: mtg_ir::PlayerFilter {
                                relation: mtg_ir::PlayerRelation::You,
                            },
                        },
                        op: EffectOp::Draw {
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
                    }],
                    modal: None,
                    timing: Timing::Instant,
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
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };

        let commander_bonus_spec = ManaSpendingRestriction {
            on_spend_commander_bonus_counters: true,
            ..Default::default()
        };
        let creature_dg_true = ManaSpendingRestriction {
            allow_spells: Some(AllowSpellsRestriction {
                filter: Some(PlainManaFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            disallow_generic: true,
            ..Default::default()
        };

        // 0: Synth Sol Rock ({T}: {C}{C})
        let sol_rock = make_custom_mana_perm(
            "Synth Sol Rock",
            CardTypeMask::ARTIFACT,
            vec![],
            vec![ManaColor::C, ManaColor::C],
            None,
            None,
        );
        // 1: Synth Ancestry B ({T}: {B} + delayed_rider)
        let ancestry_b = make_custom_mana_perm(
            "Synth Ancestry B",
            CardTypeMask::LAND,
            vec![],
            vec![ManaColor::B],
            None,
            Some(scry_rider.clone()),
        );
        // 2: Synth Palace ({1}, {T}: {G} + commander_bonus)
        let palace_g = make_custom_mana_perm(
            "Synth Palace G",
            CardTypeMask::LAND,
            vec![CostDef::Mana {
                cost: ManaCost::parse("{1}"),
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
            }],
            vec![ManaColor::G],
            Some(commander_bonus_spec.clone()),
            None,
        );
        // 3: Synth U-Land ({T}: {U})
        let u_land = make_land_card(
            "Synth U-Land",
            ManaColor::U,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        // 4: Synth R-Land ({T}: {R})
        let r_land = make_land_card(
            "Synth R-Land",
            ManaColor::R,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        // 5: Synth W-Land ({T}: {W})
        let w_land = make_land_card(
            "Synth W-Land",
            ManaColor::W,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        // 6: Synth B-Land ({T}: {B})
        let b_land = make_land_card(
            "Synth B-Land",
            ManaColor::B,
            SupertypeMask::empty(),
            SmallVec::new(),
            None,
        );
        // 7: Synth Ancestry W ({T}: {W} + delayed_rider)
        let ancestry_w = make_custom_mana_perm(
            "Synth Ancestry W",
            CardTypeMask::LAND,
            vec![],
            vec![ManaColor::W],
            None,
            Some(scry_rider.clone()),
        );
        // 8: Synth WUBRG Beacon ({W}{U}{B}{R}{G}: Draw 1)
        let beacon_wubrg = make_beacon("Synth WUBRG Beacon", "{W}{U}{B}{R}{G}");
        // 9: Synth WBG Beacon ({W}{B}{G}: Draw 1)
        let beacon_wbg = make_beacon("Synth WBG Beacon", "{W}{B}{G}");
        // 10: Synth BBG Beacon ({B}{B}{G}: Draw 1)
        let beacon_bbg = make_beacon("Synth BBG Beacon", "{B}{B}{G}");
        // 11: Synth Cavern ({T}: {G} creature-only disallow_generic=true)
        let cavern_g = make_land_card(
            "Synth Cavern",
            ManaColor::G,
            SupertypeMask::empty(),
            SmallVec::new(),
            Some(creature_dg_true),
        );
        // 12: Synth Bear ({1}{G} creature)
        let synth_bear = make_spell_card("Synth Bear", "{1}{G}", CardTypeMask::CREATURE);

        let cards = vec![
            sol_rock,
            ancestry_b,
            palace_g,
            u_land,
            r_land,
            w_land,
            b_land,
            ancestry_w,
            beacon_wubrg,
            beacon_wbg,
            beacon_bbg,
            cavern_g,
            synth_bear,
        ];

        // Case (a): Synth Sol Rock ({C}{C}) + Synth Ancestry B ({B}, delayed_rider) +
        // Synth Palace G ({1},{T}:{G}) + Synth U-Land + Synth R-Land + {W} in pool paying {W}{U}{B}{R}{G}.
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            state.player_mut(PlayerId(0)).unwrap().mana_pool = [1, 0, 0, 0, 0, 0];
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(0));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(3));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(4));
            let beacon = put_on_battlefield(&mut state, PlayerId(0), CardDefId(8));

            let cands = enumerate_mana_abilities(&state, PlayerId(0));
            let anc_cand = cands
                .iter()
                .find(|c| {
                    state
                        .object(c.object)
                        .is_some_and(|o| o.card_def == CardDefId(1))
                })
                .unwrap();
            assert!(anc_cand.has_delayed_rider);
            assert_eq!(anc_cand.priority_tier, 1);

            crate::casting::begin_activate_ability(&mut state, PlayerId(0), beacon, 0).unwrap();
            let cast_acts = crate::casting::legal_casting_actions(&state, PlayerId(0));
            assert!(cast_acts.contains(&crate::action::Action::AutoPayMana));
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert_eq!(state.stack().len(), 1);
            assert_eq!(
                state.player(PlayerId(0)).unwrap().mana_pool,
                [0, 0, 0, 0, 0, 1]
            );
            assert!(
                state
                    .player(PlayerId(0))
                    .unwrap()
                    .restricted_mana()
                    .is_empty()
            );
            assert_eq!(state.delayed_triggers().len(), 1);
            assert_eq!(state.stack_mana_riders().len(), 1);
        }

        // Case (a2): Direct deduct_mana_for_context for {1} (for_generic_cost) with {C} +
        // delayed_rider {B} + commander_bonus {G} in pool consumes ONLY {C} and leaves riders untouched.
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            let anc_obj = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            let pal_obj = put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 0, 1];
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(anc_obj),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::B]),
                },
                None,
                None,
                false,
                Some(&scry_rider),
                None,
                1,
            )
            .unwrap();
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(pal_obj),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G]),
                },
                None,
                Some(&commander_bonus_spec),
                false,
                None,
                None,
                1,
            )
            .unwrap();

            let ctx = ManaSpendContext::for_generic_cost(PlayerId(0));
            let ded = deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &ManaCost::parse("{1}"),
                &ctx,
                &[],
            )
            .unwrap();
            assert_eq!(ded.spent_counts, [0, 0, 0, 0, 0, 1]);
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
            assert_eq!(
                state.player(PlayerId(0)).unwrap().restricted_mana().len(),
                2
            );
            assert!(state.pending_mana_delayed_riders().is_empty());
            assert!(state.delayed_triggers().is_empty());
        }

        // Case (b1): 2x Synth W-Land + Synth Ancestry B + Synth Palace G paying {W}{B}{G} via AutoPayMana.
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let beacon = put_on_battlefield(&mut state, PlayerId(0), CardDefId(9));

            crate::casting::begin_activate_ability(&mut state, PlayerId(0), beacon, 0).unwrap();
            assert!(
                crate::casting::legal_casting_actions(&state, PlayerId(0))
                    .contains(&crate::action::Action::AutoPayMana)
            );
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert_eq!(state.stack().len(), 1);
            assert_eq!(state.delayed_triggers().len(), 1);
        }

        // Case (b2): Synth B-Land + 2x Synth Ancestry W + Synth Palace G paying {W}{B}{G} via AutoPayMana.
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            put_on_battlefield(&mut state, PlayerId(0), CardDefId(6));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(7));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(7));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let beacon = put_on_battlefield(&mut state, PlayerId(0), CardDefId(9));

            crate::casting::begin_activate_ability(&mut state, PlayerId(0), beacon, 0).unwrap();
            assert!(
                crate::casting::legal_casting_actions(&state, PlayerId(0))
                    .contains(&crate::action::Action::AutoPayMana)
            );
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert_eq!(state.stack().len(), 1);
        }

        // Case (b3): Pre-existing Synth Ancestry B in restricted_mana + Synth W-Land + Synth B-Land
        // + Synth Palace G paying {B}{B}{G} via AutoPayMana.
        {
            let mut state = boot_state(cards.clone(), InternTables::default());
            state.set_active_player(PlayerId(0));
            state.set_phase(mtg_ir::GamePhase::Main1);
            state.set_priority_player(PlayerId(0));
            state.set_decision(DecisionState::Priority);

            let anc_obj = put_on_battlefield(&mut state, PlayerId(0), CardDefId(1));
            state
                .object_mut(anc_obj)
                .unwrap()
                .flags
                .insert(ObjectFlags::TAPPED);
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(anc_obj),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::B]),
                },
                None,
                None,
                false,
                Some(&scry_rider),
                None,
                1,
            )
            .unwrap();
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(5));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(6));
            put_on_battlefield(&mut state, PlayerId(0), CardDefId(2));
            let beacon = put_on_battlefield(&mut state, PlayerId(0), CardDefId(10));

            crate::casting::begin_activate_ability(&mut state, PlayerId(0), beacon, 0).unwrap();
            assert!(
                crate::casting::legal_casting_actions(&state, PlayerId(0))
                    .contains(&crate::action::Action::AutoPayMana)
            );
            crate::casting::apply_casting_action(
                &mut state,
                PlayerId(0),
                &crate::action::Action::AutoPayMana,
            )
            .unwrap();
            assert_eq!(state.stack().len(), 1);
        }

        // Latent disallow_generic: true regression:
        // 2x disallow_generic:true {G} + 1x unrestricted {G} paying {1}{G} on creature spell
        // must spend 1 restricted {G} + 1 unrestricted {G}, leaving 1 restricted {G} in pool.
        {
            let mut state = boot_state(cards, InternTables::default());
            let cav_obj = put_on_battlefield(&mut state, PlayerId(0), CardDefId(11));
            let bear = put_in_hand(&mut state, PlayerId(0), CardDefId(12));
            let cav_spec = creature_dg_true_spec();
            add_mana_from_production(
                &mut state,
                PlayerId(0),
                Some(cav_obj),
                &ManaProduction::Fixed {
                    symbols: SmallVec::from_slice(&[ManaColor::G, ManaColor::G]),
                },
                None,
                Some(&cav_spec),
                false,
                None,
                None,
                1,
            )
            .unwrap();
            state.player_mut(PlayerId(0)).unwrap().mana_pool = [0, 0, 0, 0, 1, 0];

            let bear_ctx = ManaSpendContext {
                player: PlayerId(0),
                kind: ManaSpendContextKind::Spell,
                source_object: Some(bear),
                from_zone: Zone::Hand,
                ..Default::default()
            };
            let ded = deduct_mana_for_context(
                &mut state,
                PlayerId(0),
                &ManaCost::parse("{1}{G}"),
                &bear_ctx,
                &[],
            )
            .unwrap();
            assert_eq!(ded.total_spent, 2);
            assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);
            assert_eq!(
                state.player(PlayerId(0)).unwrap().restricted_mana().len(),
                1
            );
            assert_eq!(
                state.player(PlayerId(0)).unwrap().restricted_mana()[0].colors[4],
                1
            );
        }

        fn creature_dg_true_spec() -> ManaSpendingRestriction {
            ManaSpendingRestriction {
                allow_spells: Some(AllowSpellsRestriction {
                    filter: Some(PlainManaFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                disallow_generic: true,
                ..Default::default()
            }
        }
    }
}
