pub mod cache;
pub mod deps;
pub mod predicates;

use mtg_ir::{
    AbilityDef, CardDatabase, CardDefId, CardTypeMask, ColorMask, CounterId, Duration, Layer,
    LayerMod, ManaCost, PlayerFilter, RuleMod, Span, StaticBody, StaticDef, Zone,
};
use smallvec::SmallVec;

use crate::eval::{
    BoardView, EvalContext, LayerBoardView, OffBfGrantSource, OffBfLookupIds, evaluate_condition,
    evaluate_off_battlefield_object_with_grants,
};
use crate::faces::{
    ActiveAbilityWithCardRef, FaceResolveOpts, face_opts_for_game_object, face_opts_for_object,
    for_each_active_ability_with_card_on_faces, resolve_object_faces,
};
use crate::filter::matches_player_filter;
use crate::id::{ObjectId, PlayerId};
use crate::layers::deps::{
    EffectOrigin, EffectPayload, LAYER_BIT_1, LAYER_BIT_2, LAYER_BIT_3, LAYER_BIT_4A, LAYER_BIT_4B,
    LAYER_BIT_5A, LAYER_BIT_5B, LAYER_BIT_6, LAYER_BIT_7A, LAYER_BIT_7B, LAYER_BIT_7C,
    LAYER_BIT_7D, LayerEffectItem, LayerOpKind, apply_layer_with_deps, is_cda_static_def,
    layer_mask_for_static_def,
};
use crate::layers::predicates::{
    ActiveRuleModEntry, ContinuousTarget, EvaluatedObjectExtras, EvaluatedPlayerState,
    PermWorkState, StaticGrantOrigin, StaticProvenance, SubtypeTable, TimedContinuousEffect,
    WorkAbilityItem, WorkBoard, WorkStatic, dirty_mask_for_amount, dirty_mask_for_condition,
    dirty_mask_for_filter,
};
use crate::state::{
    DirtyMask, EvaluatedObject, GameState, LayerCache, ObjectFlags, StaticPresenceFlags,
    TOKEN_CARD_DEF_ID, evaluate_token_spec_base, static_presence_flag_for_body,
};
use crate::view::{ContinuousBreakdown, PowerToughness};

pub use predicates::{
    ActiveRuleModEntry as LayerRuleModEntry, ContinuousTarget as LayerContinuousTarget,
    EvaluatedObjectExtras as ObjectLayerExtras, EvaluatedPlayerState as PlayerLayerState,
    GrantedAbilityEntry as GrantedLayerAbility, PlayerContinuousEffect as TimedPlayerEffect,
    TimedContinuousEffect as TimedLayerEffect,
};

struct ActiveOffBfStatic<'a> {
    origin: EffectOrigin,
    provenance: StaticProvenance,
    source_id: ObjectId,
    controller: PlayerId,
    label: &'a str,
    timestamp: u64,
    static_def: &'a StaticDef,
}

#[inline]
fn is_timed_effect_active(
    eff_source: Option<ObjectId>,
    eff_controller: PlayerId,
    duration: Duration,
    board: &WorkBoard,
    state: &GameState,
) -> bool {
    match duration {
        Duration::UntilSourceLeaves => eff_source.is_some_and(|s| board.perm(s).is_some()),
        Duration::ForAsLongAsYouControlSelf => {
            eff_source.is_some_and(|s| board.effective_controller(s) == Some(eff_controller))
        }
        Duration::ForAsLongAsSelfRemainsTapped => {
            board.record_read(DirtyMask::TAPPED);
            eff_source.is_some_and(|s| {
                board.perm(s).is_some()
                    && state
                        .object(s)
                        .is_some_and(|o| o.flags.contains(ObjectFlags::TAPPED))
            })
        }
        Duration::EndOfTurn
        | Duration::UntilEndOfCombat
        | Duration::UntilYourNextTurn
        | Duration::UntilEndOfYourNextTurn => {
            board.record_read(DirtyMask::TURN_HISTORY);
            true
        }
        Duration::Permanent => true,
    }
}

fn parse_pt_counter(db: &CardDatabase, cid: CounterId, count: u32) -> Option<(i32, i32)> {
    let name = db.counter_name(cid)?;
    if name == "+1/+1" {
        let n = count as i32;
        return Some((n, n));
    }
    if name == "-1/-1" {
        let n = count as i32;
        return Some((-n, -n));
    }
    let (lhs, rhs) = name.split_once('/')?;
    if !(lhs.starts_with('+') || lhs.starts_with('-'))
        || !(rhs.starts_with('+') || rhs.starts_with('-'))
    {
        return None;
    }
    let dp: i32 = lhs.parse().ok()?;
    let dt: i32 = rhs.parse().ok()?;
    let mult = count as i32;
    Some((dp.saturating_mul(mult), dt.saturating_mul(mult)))
}

#[inline]
fn layer_mask_for_timed(te: &TimedContinuousEffect) -> u16 {
    match te.layer {
        Layer::Layer1 => LAYER_BIT_1,
        Layer::Layer2 => LAYER_BIT_2,
        Layer::Layer3 => LAYER_BIT_3,
        Layer::Layer4 => {
            if te.is_cda {
                LAYER_BIT_4A
            } else {
                LAYER_BIT_4B
            }
        }
        Layer::Layer5 => {
            if te.is_cda {
                LAYER_BIT_5A
            } else {
                LAYER_BIT_5B
            }
        }
        Layer::Layer6 => LAYER_BIT_6,
        Layer::Layer7a => LAYER_BIT_7A,
        Layer::Layer7b => LAYER_BIT_7B,
        Layer::Layer7c => LAYER_BIT_7C,
        Layer::Layer7d => LAYER_BIT_7D,
    }
}

fn accumulate_static_dirty_reads(
    cond: Option<&mtg_ir::Condition>,
    st: &StaticDef,
    acc: &mut DirtyMask,
) {
    if let Some(c) = cond {
        *acc |= dirty_mask_for_condition(c);
    }
    match &st.body {
        StaticBody::Continuous {
            affected,
            modifier,
            active_in_zone,
            ..
        } => {
            if let Some(z) = active_in_zone {
                *acc |= match z {
                    Zone::Graveyard => DirtyMask::GRAVEYARDS,
                    Zone::Exile => DirtyMask::EXILE,
                    Zone::Hand | Zone::Library | Zone::Command => DirtyMask::PLAYER_STATE,
                    Zone::Battlefield | Zone::Stack => DirtyMask::TOPOLOGY,
                };
            }
            if let mtg_ir::Affected::Filter { filter } = affected {
                *acc |= dirty_mask_for_filter(filter);
            }
            accumulate_layer_mod_dirty_reads(modifier, acc);
        }
        StaticBody::DynamicWardMana { affected, generic } => {
            if let mtg_ir::Affected::Filter { filter } = affected {
                *acc |= dirty_mask_for_filter(filter);
            }
            *acc |= dirty_mask_for_amount(generic);
        }
        StaticBody::RuleModifier { rule, .. } => {
            *acc |= DirtyMask::PLAYER_STATE;
            accumulate_rule_mod_dirty_reads(rule, acc);
        }
        StaticBody::OffBattlefieldCharacteristics {
            zones,
            filter,
            add_chosen_subtype,
            add_chosen_color,
            ..
        } => {
            if zones.contains(mtg_ir::ZoneMask::GRAVEYARD) {
                *acc |= DirtyMask::GRAVEYARDS;
            }
            if zones.contains(mtg_ir::ZoneMask::EXILE) {
                *acc |= DirtyMask::EXILE;
            }
            if zones.intersects(
                mtg_ir::ZoneMask::HAND | mtg_ir::ZoneMask::LIBRARY | mtg_ir::ZoneMask::COMMAND,
            ) {
                *acc |= DirtyMask::PLAYER_STATE;
            }
            if *add_chosen_subtype || *add_chosen_color {
                *acc |= DirtyMask::DESIGNATIONS;
            }
            if let Some(f) = filter {
                *acc |= dirty_mask_for_filter(f);
            }
        }
        StaticBody::CommanderPreGameColorChoice => {
            *acc |= DirtyMask::DESIGNATIONS;
        }
        StaticBody::CostModifier { .. }
        | StaticBody::Restriction { .. }
        | StaticBody::Multiplier { .. }
        | StaticBody::PregameBeginOnBattlefield
        | StaticBody::CombatRule { .. }
        | StaticBody::CantActivateAbilities { .. }
        | StaticBody::TopOfLibraryVisibility { .. }
        | StaticBody::ZonePlayPermission { .. }
        | StaticBody::AdditionalCastCost { .. }
        | StaticBody::AltCastPermission { .. }
        | StaticBody::MayChooseNotToUntap { .. }
        | StaticBody::AssignCombatDamageByToughness { .. }
        | StaticBody::CommanderDesignation
        | StaticBody::DeckCopyLimit { .. }
        | StaticBody::StaticDamagePrevention { .. }
        | StaticBody::EntersPrepared
        | StaticBody::CompanionCondition { .. }
        | StaticBody::Offering { .. }
        | StaticBody::SelfCastRestriction { .. }
        | StaticBody::LoyaltyActivationsPerTurn { .. }
        | StaticBody::HandVisibility { .. }
        | StaticBody::RetainUnspentMana { .. }
        | StaticBody::UntapDuringOtherUntapSteps { .. }
        | StaticBody::SpellGrantKeyword { .. }
        | StaticBody::ActivateAsThoughHaste { .. }
        | StaticBody::CantBeTargetedBy { .. }
        | StaticBody::TapForManaMultiplier { .. }
        | StaticBody::DraftNoOp
        | StaticBody::ManaSpendRestriction { .. }
        | StaticBody::DefilerCostReduction { .. }
        | StaticBody::FirstActivationZeroCost { .. }
        | StaticBody::CommanderTaxAsLife { .. }
        | StaticBody::CastFromLibraryWhileSearching
        | StaticBody::SpaceSculptorSectors
        | StaticBody::StaticDamageRedirection { .. }
        | StaticBody::UnpreventableDamage { .. }
        | StaticBody::CantHaveCounterType { .. }
        | StaticBody::CountersRemainInNonHiddenZones
        | StaticBody::DamageNotRemovedInCleanup { .. }
        | StaticBody::MayLookAtExiledWithSelf
        | StaticBody::SelfGraveyardKeywordCast { .. }
        | StaticBody::GrantGraveyardEscape { .. }
        | StaticBody::GrantGraveyardKeywordAbility { .. }
        | StaticBody::GrantHandForetell { .. }
        | StaticBody::LandsCantEnterBattlefield
        | StaticBody::CantEnterFromZones { .. }
        | StaticBody::TokenReplacement { .. } => {}
    }
}

fn accumulate_layer_mod_dirty_reads(modifier: &LayerMod, acc: &mut DirtyMask) {
    match modifier {
        LayerMod::Pt { power, toughness } | LayerMod::SetBasePt { power, toughness } => {
            *acc |= dirty_mask_for_amount(power) | dirty_mask_for_amount(toughness);
        }
        LayerMod::SetBasePower { power } => *acc |= dirty_mask_for_amount(power),
        LayerMod::SetBaseToughness { toughness } => {
            *acc |= dirty_mask_for_amount(toughness);
        }
        LayerMod::AddChosenSubtype
        | LayerMod::SetChosenSubtype
        | LayerMod::SetChosenColor
        | LayerMod::AddChosenColor
        | LayerMod::ProtectionFromChosenColor { .. }
        | LayerMod::ProtectionFromChosenPlayer
        | LayerMod::ProtectionFromChosenCardType => {
            *acc |= DirtyMask::DESIGNATIONS;
        }
        LayerMod::CopyKeywordsFromExiled { .. }
        | LayerMod::CopyStatsAndSubtypesFromLastExiled { .. } => {
            *acc |= DirtyMask::EXILE;
        }
        LayerMod::GrantAbilitiesFromObjects { from, .. } => match from {
            mtg_ir::AbilityGrantSource::ExiledWithSelf { filter } => {
                *acc |= DirtyMask::EXILE | dirty_mask_for_filter(filter);
            }
            mtg_ir::AbilityGrantSource::ExiledCardsWithCounter { filter, .. } => {
                *acc |= DirtyMask::EXILE
                    | DirtyMask::COUNTERS
                    | filter
                        .as_deref()
                        .map_or(DirtyMask::empty(), dirty_mask_for_filter);
            }
            mtg_ir::AbilityGrantSource::Battlefield { filter } => {
                *acc |= DirtyMask::TOPOLOGY | dirty_mask_for_filter(filter);
            }
        },
        LayerMod::GrantAbility { ability } => {
            if let AbilityDef::Static(st) = ability.as_ref() {
                accumulate_static_dirty_reads(
                    st.static_def.condition.as_deref(),
                    &st.static_def,
                    acc,
                );
            }
        }
        LayerMod::SetController { .. }
        | LayerMod::SetName { .. }
        | LayerMod::SwitchPt
        | LayerMod::AddKeywords { .. }
        | LayerMod::RemoveKeywords { .. }
        | LayerMod::RemoveAllAbilities { .. }
        | LayerMod::AddTypes { .. }
        | LayerMod::SetTypes { .. }
        | LayerMod::RemoveTypes { .. }
        | LayerMod::SetSubtypes { .. }
        | LayerMod::AllCreatureTypes
        | LayerMod::SetColors { .. }
        | LayerMod::AddColors { .. } => {}
    }
}

fn accumulate_rule_mod_dirty_reads(rule: &RuleMod, acc: &mut DirtyMask) {
    match rule {
        RuleMod::FilteredSpellCapPerTurn { filter, .. }
        | RuleMod::CantCastMatchingSpells { filter, .. }
        | RuleMod::LegendRuleExempt { filter }
        | RuleMod::UntapStepCap { filter, .. } => {
            if let Some(f) = filter {
                *acc |= dirty_mask_for_filter(f);
            }
        }
        RuleMod::CastAsThoughFlash {
            spells: Some(f), ..
        }
        | RuleMod::SpendManaAsAnyColor {
            source_filter: Some(f),
            ..
        }
        | RuleMod::CantPayLifeOrSacrificeForCosts {
            sacrifice_filter: Some(f),
            ..
        } => {
            *acc |= dirty_mask_for_filter(f);
        }
        RuleMod::AttackTax {
            dynamic_amount,
            affected,
            ..
        }
        | RuleMod::BlockTax {
            dynamic_amount,
            affected,
            ..
        } => {
            if let Some(amt) = dynamic_amount {
                *acc |= dirty_mask_for_amount(amt);
            }
            if let Some(mtg_ir::Affected::Filter { filter }) = affected {
                *acc |= dirty_mask_for_filter(filter);
            }
        }
        RuleMod::CantCastChosenName | RuleMod::PlayerProtectionFromChosenCardType => {
            *acc |= DirtyMask::DESIGNATIONS;
        }
        RuleMod::CantCastSameNameAsLinkedExile => {
            *acc |= DirtyMask::EXILE;
        }
        _ => {}
    }
}

#[inline]
fn record_static_dirty_reads(board: &WorkBoard<'_>, st: &StaticDef) {
    let mut acc = DirtyMask::empty();
    accumulate_static_dirty_reads(st.condition.as_deref(), st, &mut acc);
    if !acc.is_empty() {
        board.record_read(acc);
    }
}

#[inline]
fn record_layer_mod_dirty_reads(board: &WorkBoard<'_>, modifier: &LayerMod) {
    let mut acc = DirtyMask::empty();
    accumulate_layer_mod_dirty_reads(modifier, &mut acc);
    if !acc.is_empty() {
        board.record_read(acc);
    }
}

#[inline]
fn record_rule_mod_dirty_reads(board: &WorkBoard<'_>, rule: &RuleMod) {
    let mut acc = DirtyMask::empty();
    accumulate_rule_mod_dirty_reads(rule, &mut acc);
    if !acc.is_empty() {
        board.record_read(acc);
    }
}

fn spans_or_affected_match(a: &WorkStatic<'_>, b: &WorkStatic<'_>) -> bool {
    if a.span != Span::default() || b.span != Span::default() {
        return a.span != Span::default() && a.span == b.span;
    }
    if a.granted_item_id != b.granted_item_id {
        return false;
    }
    if let (
        StaticBody::Continuous {
            affected: aff_a, ..
        },
        StaticBody::Continuous {
            affected: aff_b, ..
        },
    ) = (&a.static_def.body, &b.static_def.body)
    {
        return aff_a == aff_b && a.static_def.condition == b.static_def.condition;
    }
    false
}

fn check_started_in_earlier_layer(
    perm: &PermWorkState<'_>,
    current_static: &WorkStatic<'_>,
    current_layer: Layer,
) -> Option<SmallVec<[ObjectId; 4]>> {
    for prev in &perm.statics {
        if let StaticBody::Continuous { layer, .. } = &prev.static_def.body
            && *layer < current_layer
            && !prev.applied_targets.is_empty()
            && spans_or_affected_match(prev, current_static)
        {
            return Some(prev.applied_targets.clone());
        }
    }
    None
}

#[inline]
fn item_can_push_l6_static(item: &LayerEffectItem<'_>) -> bool {
    matches!(
        item.op(),
        Some(LayerOpKind::Mod(LayerMod::GrantAbility { ability }))
            if matches!(
                ability.as_ref(),
                AbilityDef::Static(st)
                    if (layer_mask_for_static_def(&st.static_def, true) & LAYER_BIT_6) != 0
            )
    )
}

#[inline]
fn try_push_perm_static_item<'a>(
    perm: &PermWorkState<'a>,
    state: &'a GameState,
    st: &WorkStatic<'a>,
    layer: Layer,
    cda_filter: Option<bool>,
    order: &mut usize,
    items: &mut Vec<LayerEffectItem<'a>>,
) {
    let origin = match st.granted_item_id {
        Some(item_id) => EffectOrigin::GrantedStatic {
            host: perm.id,
            item_id,
        },
        None => EffectOrigin::PermanentStatic {
            source: perm.id,
            static_idx: st.static_idx,
        },
    };
    match &st.static_def.body {
        StaticBody::Continuous {
            layer: st_layer,
            modifier,
            active_in_zone,
            ..
        } => {
            if active_in_zone.is_some_and(|z| z != Zone::Battlefield) {
                return;
            }
            // CopyStatsAndSubtypesFromLastExiled spans Layer 3 (name), Layer 4 (subtypes), and Layer 7b (P/T).
            let matches_layer = *st_layer == layer
                || (matches!(
                    modifier,
                    LayerMod::CopyStatsAndSubtypesFromLastExiled { .. }
                ) && matches!(layer, Layer::Layer3 | Layer::Layer4 | Layer::Layer7b));
            if !matches_layer {
                return;
            }
            let is_cda = is_cda_static_def(st.static_def);
            if let Some(want_cda) = cda_filter
                && is_cda != want_cda
            {
                return;
            }
            let locked_targets = check_started_in_earlier_layer(perm, st, layer);
            let started_in_earlier_layer = locked_targets.is_some();
            items.push(LayerEffectItem {
                origin,
                provenance: st.provenance,
                source_id: Some(perm.id),
                source_controller: perm.eval.controller,
                source_label: None,
                timestamp: st.timestamp,
                order: *order,
                is_cda,
                layer,
                payload: EffectPayload::StaticRef(st.static_def),
                started_in_earlier_layer,
                locked_targets,
                timed_chosen_color: None,
                timed_chosen_subtype: None,
                timed_chosen_player: None,
                timed_chosen_card_type: None,
                timed_locked_pt: None,
            });
            *order += 1;
        }
        StaticBody::DynamicWardMana { .. }
            if layer == Layer::Layer6 && cda_filter != Some(true) =>
        {
            items.push(LayerEffectItem {
                origin,
                provenance: st.provenance,
                source_id: Some(perm.id),
                source_controller: perm.eval.controller,
                source_label: None,
                timestamp: st.timestamp,
                order: *order,
                is_cda: false,
                layer,
                payload: EffectPayload::StaticRef(st.static_def),
                started_in_earlier_layer: false,
                locked_targets: None,
                timed_chosen_color: None,
                timed_chosen_subtype: None,
                timed_chosen_player: None,
                timed_chosen_card_type: None,
                timed_locked_pt: None,
            });
            *order += 1;
        }
        StaticBody::CommanderPreGameColorChoice
            if layer == Layer::Layer5
                && cda_filter != Some(true)
                && state.object(perm.id).is_some_and(|o| o.is_commander())
                && state.rare(perm.id).and_then(|r| r.chosen_color).is_some() =>
        {
            items.push(LayerEffectItem {
                origin,
                provenance: st.provenance,
                source_id: Some(perm.id),
                source_controller: perm.eval.controller,
                source_label: None,
                timestamp: st.timestamp,
                order: *order,
                is_cda: false,
                layer,
                payload: EffectPayload::StaticRef(st.static_def),
                started_in_earlier_layer: false,
                locked_targets: None,
                timed_chosen_color: None,
                timed_chosen_subtype: None,
                timed_chosen_player: None,
                timed_chosen_card_type: None,
                timed_locked_pt: None,
            });
            *order += 1;
        }
        _ => {}
    }
}

fn collect_layer_items<'a, 'b>(
    board: &WorkBoard<'a>,
    state: &'a GameState,
    off_bf_statics: &'b [ActiveOffBfStatic<'a>],
    active_timed: &'b [(usize, &'a TimedContinuousEffect)],
    layer: Layer,
    cda_filter: Option<bool>,
    items: &mut Vec<LayerEffectItem<'a>>,
) {
    items.clear();
    let mut order: usize = 0;

    for perm in &board.perms {
        for st in &perm.statics {
            try_push_perm_static_item(perm, state, st, layer, cda_filter, &mut order, items);
        }
    }

    for off in off_bf_statics {
        let (matches_layer, is_cda) = match &off.static_def.body {
            StaticBody::Continuous {
                layer: st_layer, ..
            } if *st_layer == layer => (true, is_cda_static_def(off.static_def)),
            StaticBody::DynamicWardMana { .. } if layer == Layer::Layer6 => (true, false),
            _ => (false, false),
        };
        if matches_layer {
            if let Some(want_cda) = cda_filter
                && is_cda != want_cda
            {
                continue;
            }
            items.push(LayerEffectItem {
                origin: off.origin,
                provenance: off.provenance,
                source_id: Some(off.source_id),
                source_controller: off.controller,
                source_label: Some(off.label),
                timestamp: off.timestamp,
                order,
                is_cda,
                layer,
                payload: EffectPayload::StaticRef(off.static_def),
                started_in_earlier_layer: false,
                locked_targets: None,
                timed_chosen_color: None,
                timed_chosen_subtype: None,
                timed_chosen_player: None,
                timed_chosen_card_type: None,
                timed_locked_pt: None,
            });
            order += 1;
        }
    }

    for &(t_idx, te) in active_timed {
        if te.layer != layer {
            continue;
        }
        if let Some(want_cda) = cda_filter
            && te.is_cda != want_cda
        {
            continue;
        }
        let source_card_def = te.source.and_then(|s| {
            let obj = state.object(s)?;
            Some(
                state
                    .rare(s)
                    .and_then(|r| r.copiable.as_deref())
                    .map_or(obj.card_def, |c| c.card_def),
            )
        });
        items.push(LayerEffectItem {
            origin: EffectOrigin::TimedEffect { index: t_idx },
            provenance: StaticProvenance {
                origin: StaticGrantOrigin::TimedEffect,
                card_def: source_card_def,
                face_index: 0,
                component_index: 0,
                ability_index: (te.timestamp & 0xFFFF_FFFF) as u32,
                effect_id: te.id,
            },
            source_id: te.source,
            source_controller: te.controller,
            source_label: None,
            timestamp: te.timestamp,
            order,
            is_cda: te.is_cda,
            layer,
            payload: EffectPayload::Timed(te),
            started_in_earlier_layer: true,
            locked_targets: None,
            timed_chosen_color: te.chosen_color,
            timed_chosen_subtype: te.chosen_subtype,
            timed_chosen_player: te.chosen_player,
            timed_chosen_card_type: te.chosen_card_type,
            timed_locked_pt: te.locked_pt,
        });
        order += 1;
    }
}

struct RuleEffectEntry<'a> {
    timestamp: u64,
    order: usize,
    source: Option<ObjectId>,
    controller: PlayerId,
    who: PlayerFilter,
    rule: &'a RuleMod,
    chosen_card_name: Option<String>,
    chosen_card_type: Option<CardTypeMask>,
    linked_exile_names: SmallVec<[String; 2]>,
}

fn apply_rule_mod(
    player_state: &mut EvaluatedPlayerState,
    entry: &RuleEffectEntry<'_>,
    all_off_bf_colorless: &mut bool,
) {
    let mut push_entry = false;
    match entry.rule {
        RuleMod::NoMaxHandSize => player_state.no_max_hand_size = true,
        RuleMod::MaxHandSizeDelta { delta } => {
            player_state.max_hand_size_delta =
                player_state.max_hand_size_delta.saturating_add(*delta);
        }
        RuleMod::SetMaxHandSize { size } => {
            player_state.max_hand_size_set = Some(*size);
        }
        RuleMod::CantLose => player_state.cant_lose = true,
        RuleMod::DontLoseAtZeroOrLessLife => player_state.dont_lose_at_zero_life = true,
        RuleMod::OpponentsCantWin => player_state.opponents_cant_win = true,
        RuleMod::CastOnlySorcerySpeed => player_state.cast_only_sorcery_speed = true,
        RuleMod::OneSpellPerTurn => player_state.one_spell_per_turn = true,
        RuleMod::CastOnlyFromHand => player_state.cast_only_from_hand = true,
        RuleMod::CastOnlyDuringOwnTurn => player_state.cast_only_during_own_turn = true,
        RuleMod::ActivateOnlyDuringOwnTurn => player_state.activate_only_during_own_turn = true,
        RuleMod::PlayerHexproof => player_state.hexproof = true,
        RuleMod::PlayerShroud => player_state.shroud = true,
        RuleMod::LifeTotalCantChange => player_state.life_cant_change = true,
        RuleMod::ExtraLandPlays { count } => {
            player_state.extra_land_plays = player_state.extra_land_plays.saturating_add(*count);
        }
        RuleMod::AllOffBattlefieldAndSpellsColorless => {
            player_state.all_off_bf_colorless = true;
            *all_off_bf_colorless = true;
        }
        RuleMod::DamageCantBePrevented => player_state.damage_cant_be_prevented = true,
        RuleMod::CantGainLife => player_state.cant_gain_life = true,
        RuleMod::SkipDrawStep => player_state.skip_draw_step = true,
        RuleMod::CantCastDuringCombat => player_state.cant_cast_during_combat = true,
        RuleMod::CantPlayLands => player_state.cant_play_lands = true,
        RuleMod::CantDrawCards => player_state.cant_draw_cards = true,
        RuleMod::CantDrawExtraCards => player_state.cant_draw_extra_cards = true,
        RuleMod::Flagbearer => player_state.flagbearer = true,
        RuleMod::GraveyardCantBeTargeted => player_state.graveyard_cant_be_targeted = true,
        RuleMod::DamageToYouAsInfect => player_state.damage_to_you_as_infect = true,
        RuleMod::AllDamageHasWither => player_state.all_damage_has_wither = true,
        RuleMod::SkipExtraTurns => player_state.skip_extra_turns = true,
        RuleMod::ImmuneToOpponentForcedSacrifice => {
            player_state.immune_to_opp_forced_sacrifice = true;
        }
        RuleMod::ImmuneToOpponentForcedDiscard => {
            player_state.immune_to_opp_forced_discard = true;
        }
        RuleMod::PlayerProtectionFromChosenCardType => {
            if let Some(ct) = entry.chosen_card_type {
                player_state.protection_from_card_types |= ct;
            }
            push_entry = true;
        }
        RuleMod::IgnoreHexproof => player_state.ignore_hexproof = true,
        RuleMod::SuppressWardTriggers => player_state.suppress_ward_triggers = true,
        RuleMod::LegendRuleExempt { filter } => {
            if filter.is_none() {
                player_state.legend_rule_exempt_all = true;
            }
            push_entry = true;
        }
        RuleMod::PlayerCantGetCounters { counter } => {
            if counter.is_none() {
                player_state.cant_get_any_counters = true;
            }
            push_entry = true;
        }
        RuleMod::FilteredSpellCapPerTurn { .. }
        | RuleMod::CastAsThoughFlash { .. }
        | RuleMod::AttackTax { .. }
        | RuleMod::BlockTax { .. }
        | RuleMod::SearchControl { .. }
        | RuleMod::SpendManaAsAnyColor { .. }
        | RuleMod::OpponentsCantCastDuringYourTurn { .. }
        | RuleMod::CantCastChosenName
        | RuleMod::CantCastSameNameAsLinkedExile
        | RuleMod::CantCastMatchingSpells { .. }
        | RuleMod::CantSearchLibraries { .. }
        | RuleMod::CantPayLifeOrSacrificeForCosts { .. }
        | RuleMod::UntapStepCap { .. } => {
            push_entry = true;
        }
    }
    if push_entry {
        player_state.active_rule_mods.push(ActiveRuleModEntry {
            source: entry.source,
            controller: entry.controller,
            rule: entry.rule.clone(),
            chosen_card_name: entry.chosen_card_name.clone(),
            chosen_card_type: entry.chosen_card_type,
            linked_exile_names: entry.linked_exile_names.clone(),
        });
    }
}

type BreakdownNotesSlot = Option<(u32, Vec<String>)>;

#[inline]
fn layer_mod_may_grant_off_bf(modifier: &LayerMod) -> bool {
    match modifier {
        LayerMod::GrantAbility { ability } => matches!(
            ability.as_ref(),
            AbilityDef::Static(st)
                if matches!(
                    st.static_def.body,
                    StaticBody::OffBattlefieldCharacteristics { .. }
                )
        ),
        _ => false,
    }
}

#[inline]
fn layer_mod_may_make_off_bf_colorless(modifier: &LayerMod) -> bool {
    matches!(
        modifier,
        LayerMod::GrantAbility { ability }
            if matches!(
                ability.as_ref(),
                AbilityDef::Static(st)
                    if matches!(
                        st.static_def.body,
                        StaticBody::RuleModifier {
                            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            ..
                        }
                    )
            )
    )
}

#[inline]
fn static_body_may_make_off_bf_colorless(body: &StaticBody) -> bool {
    match body {
        StaticBody::RuleModifier {
            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
            ..
        } => true,
        StaticBody::Continuous { modifier, .. } => layer_mod_may_make_off_bf_colorless(modifier),
        _ => false,
    }
}

#[inline]
fn scan_ability_for_off_bf_flags(ab: &AbilityDef, id: ObjectId, board: &mut WorkBoard<'_>) {
    if let AbilityDef::Static(st) = ab {
        match &st.static_def.body {
            StaticBody::OffBattlefieldCharacteristics { .. } => {
                board.may_have_off_bf_char_grants = true;
                board.record_off_bf_grantor(id);
            }
            StaticBody::Continuous { modifier, .. } if layer_mod_may_grant_off_bf(modifier) => {
                board.may_have_off_bf_char_grants = true;
                board.off_bf_grantors_unbounded = true;
            }
            _ => {}
        }
        if static_body_may_make_off_bf_colorless(&st.static_def.body) {
            board.may_have_all_off_bf_colorless = true;
        }
    }
}

/// Core layer evaluation pipeline (`CR 613.1–613.11`, `CR 613.8a–c`, `CR 305.7`, `CR 604.3`).
pub(crate) fn compute_layer_cache_inner(
    state: &GameState,
    collect_notes: bool,
) -> (LayerCache, Vec<BreakdownNotesSlot>) {
    let subtype_table = SubtypeTable::from_db(state.db());
    let slot_cap = state.objects().slot_capacity();
    let perm_cap = state
        .players()
        .iter()
        .filter(|p| !p.lost)
        .map(|p| p.battlefield.len())
        .sum::<usize>()
        .max(16);
    let mut board = WorkBoard::with_capacity(slot_cap, perm_cap);

    struct InitPermInfo<'a> {
        id: ObjectId,
        card_def: CardDefId,
        controller: PlayerId,
        is_face_down: bool,
        opts: FaceResolveOpts<'a>,
        active_face_indices: SmallVec<[u8; 2]>,
    }
    let mut init_infos: Vec<InitPermInfo<'_>> = Vec::with_capacity(perm_cap);
    let mut active_layer_mask: u16 = 0;
    let mut has_reconfigure_cand = false;
    let mut has_l6_extra_keywords = false;

    // 1a. Populate base Layer 1a/1b characteristics for every active (non-phased-out) battlefield permanent.
    for player in state.players() {
        if player.lost {
            continue;
        }
        for &id in &player.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let rare = state.rare(id);
            let copiable = rare.and_then(|r| r.copiable.as_deref());
            let init_controller = rare
                .and_then(|r| r.default_controller)
                .unwrap_or(obj.controller);
            let is_face_down = obj.is_face_down();
            let opts = face_opts_for_game_object(obj, rare, None);

            if subtype_table.reconfigure_kw.is_some() && obj.attached_to.is_some() {
                has_reconfigure_cand = true;
                active_layer_mask |= LAYER_BIT_4B;
            }
            if obj.flags.intersects(
                ObjectFlags::SUSPECTED
                    | ObjectFlags::BLITZED
                    | ObjectFlags::DASHED
                    | ObjectFlags::CAST_VIA_SUSPEND,
            ) || obj.has_counters()
                || rare.is_some_and(|r| !r.granted_keywords.is_empty())
            {
                has_l6_extra_keywords = true;
                active_layer_mask |= LAYER_BIT_6;
            }

            let (
                base_name,
                mut eval,
                is_pt_cda,
                has_copy_pt_power_except,
                has_copy_pt_toughness_except,
                active_face_indices,
                resolved_ward_cost,
            ) = if !is_face_down
                && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
                && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
            {
                let (nm, mut ev) = evaluate_token_spec_base(spec, copiable);
                ev.controller = init_controller;
                (nm, ev, false, false, false, SmallVec::new(), None)
            } else if let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts) {
                let except_pt = copiable.and_then(|c| c.except.as_ref()).and_then(|e| e.pt);
                let ev = EvaluatedObject {
                    pt: resolved.pt,
                    base_pt: resolved.pt,
                    types: resolved.types,
                    subtypes: resolved.subtypes,
                    supertypes: resolved.supertypes,
                    colors: resolved.colors,
                    mana_value: resolved.mana_value,
                    keywords: resolved.extra_keywords.iter().copied().collect(),
                    has_all_creature_types: false,
                    lost_all_abilities: false,
                    loyalty: resolved.loyalty,
                    defense: resolved.defense,
                    controller: init_controller,
                    name_override: None,
                    mana_cost: resolved.mana_cost,
                };
                (
                    resolved.name,
                    ev,
                    resolved.is_pt_cda,
                    except_pt.is_some(),
                    except_pt.is_some(),
                    resolved.active_face_indices,
                    resolved.ward_cost,
                )
            } else {
                let ev = EvaluatedObject {
                    pt: None,
                    base_pt: None,
                    types: CardTypeMask::empty(),
                    subtypes: SmallVec::new(),
                    supertypes: SupertypeMask_empty(),
                    colors: ColorMask::empty(),
                    mana_value: 0,
                    keywords: SmallVec::new(),
                    has_all_creature_types: false,
                    lost_all_abilities: false,
                    loyalty: None,
                    defense: None,
                    controller: init_controller,
                    name_override: None,
                    mana_cost: ManaCost::default(),
                };
                (
                    String::new(),
                    ev,
                    false,
                    false,
                    false,
                    SmallVec::new(),
                    None,
                )
            };

            if !is_face_down {
                let base_id = copiable.map_or(obj.card_def, |c| c.card_def);
                if let Some(card) = state.db().get(base_id) {
                    for &fi in &active_face_indices {
                        if let Some(face) = card.faces.get(usize::from(fi)) {
                            for ab in &face.abilities {
                                scan_ability_for_off_bf_flags(ab, id, &mut board);
                            }
                        }
                    }
                }
                if let Some(r) = rare {
                    for comp in r.merged_components.iter().skip(1) {
                        if !comp.is_token
                            && let Some(mc_card) = state.db().get(comp.card_def)
                            && let Some(mc_face) = mc_card.faces.first()
                        {
                            for ab in &mc_face.abilities {
                                scan_ability_for_off_bf_flags(ab, id, &mut board);
                            }
                        }
                    }
                }
            }

            let mut items: SmallVec<[WorkAbilityItem; 8]> = SmallVec::new();
            let mut next_item_id = 0u32;
            for kw in &eval.keywords {
                items.push(WorkAbilityItem {
                    id: next_item_id,
                    is_innate: true,
                    is_intrinsic_basic_mana: false,
                    required_basic_subtype: None,
                    is_mana_ability: false,
                    keyword: Some(*kw),
                    keyword_param: None,
                    granted_ability: None,
                    timestamp: 0,
                    removed: false,
                });
                next_item_id += 1;
            }
            eval.keywords.clear();
            if is_face_down && let Some(wc) = resolved_ward_cost {
                let generic = wc.mana_value();
                if generic > 0 {
                    items.push(WorkAbilityItem {
                        id: next_item_id,
                        is_innate: true,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: false,
                        keyword: subtype_table.ward_kw,
                        keyword_param: None,
                        granted_ability: Some(predicates::GrantedAbilityEntry::DynamicWard {
                            generic,
                            grant_source: None,
                        }),
                        timestamp: 0,
                        removed: false,
                    });
                    next_item_id += 1;
                }
            }

            let perm_idx = board.perms.len();
            if let Some(slot) = board.index_by_obj.get_mut(id.index as usize) {
                *slot = perm_idx as u32;
            }
            board.perms.push(PermWorkState {
                id,
                owner: obj.owner,
                timestamp: obj.entered_timestamp,
                board_order: perm_idx,
                base_name,
                is_face_down,
                is_pt_cda,
                has_copy_pt_power_except,
                has_copy_pt_toughness_except,
                lost_innate_statics: false,
                eval,
                extras: EvaluatedObjectExtras::default(),
                items,
                statics: SmallVec::new(),
                notes: Vec::new(),
                next_item_id,
            });
            init_infos.push(InitPermInfo {
                id,
                card_def: obj.card_def,
                controller: init_controller,
                is_face_down,
                opts,
                active_face_indices,
            });
        }
    }

    for te in state.continuous_effects() {
        if layer_mod_may_grant_off_bf(&te.modifier) {
            board.may_have_off_bf_char_grants = true;
            board.off_bf_grantors_unbounded = true;
        }
        if layer_mod_may_make_off_bf_colorless(&te.modifier) {
            board.may_have_all_off_bf_colorless = true;
        }
    }
    if state
        .player_continuous_effects()
        .iter()
        .any(|pe| matches!(pe.rule, RuleMod::AllOffBattlefieldAndSpellsColorless))
    {
        board.may_have_all_off_bf_colorless = true;
    }

    // 1b. Collect innate abilities (including `grant_condition`-gated Leveler/Class/Station abilities)
    // using `BoardView::LayerWork(&board)` so `OnceLock` is never re-entered!
    let mut ab_buf: Vec<ActiveAbilityWithCardRef<'_>> = Vec::with_capacity(16);
    for (perm_idx, info) in init_infos.iter().enumerate() {
        if info.is_face_down {
            board.perms[perm_idx].sync_abilities();
            continue;
        }
        ab_buf.clear();
        let mut ctx = EvalContext::new(info.controller).with_board(BoardView::LayerWork(&board));
        ctx.source = Some(info.id);
        for_each_active_ability_with_card_on_faces(
            state.db(),
            info.card_def,
            &info.opts,
            &info.active_face_indices,
            |cond| {
                board.record_read(dirty_mask_for_condition(cond));
                evaluate_condition(state, cond, &ctx)
            },
            |ab_ref| ab_buf.push(ab_ref),
        );
        let mut saw_off_bf_grant = false;
        let mut saw_off_bf_unbounded = false;
        let mut saw_all_colorless = false;
        let perm = &mut board.perms[perm_idx];
        for ab_ref in &ab_buf {
            match ab_ref.ability {
                AbilityDef::Keyword(kw) => {
                    let kid = kw.keyword.keyword;
                    let item_id = perm.alloc_item_id();
                    perm.items.push(WorkAbilityItem {
                        id: item_id,
                        is_innate: true,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: false,
                        keyword: Some(kid),
                        keyword_param: kw.keyword.param.clone(),
                        granted_ability: None,
                        timestamp: 0,
                        removed: false,
                    });
                }
                AbilityDef::Activated(act) => {
                    let item_id = perm.alloc_item_id();
                    perm.items.push(WorkAbilityItem {
                        id: item_id,
                        is_innate: true,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: act.is_mana,
                        keyword: None,
                        keyword_param: None,
                        granted_ability: None,
                        timestamp: 0,
                        removed: false,
                    });
                }
                AbilityDef::Static(st) => {
                    match &st.static_def.body {
                        StaticBody::OffBattlefieldCharacteristics { .. } => {
                            saw_off_bf_grant = true;
                        }
                        StaticBody::Continuous { modifier, .. }
                            if layer_mod_may_grant_off_bf(modifier) =>
                        {
                            saw_off_bf_unbounded = true;
                        }
                        _ => {}
                    }
                    if static_body_may_make_off_bf_colorless(&st.static_def.body) {
                        saw_all_colorless = true;
                    }
                    record_static_dirty_reads_direct(
                        st.static_def.condition.as_deref(),
                        &st.static_def,
                        &board.reads,
                    );
                    active_layer_mask |= layer_mask_for_static_def(&st.static_def, true);
                    let sidx = perm.statics.len();
                    perm.statics.push(WorkStatic {
                        static_idx: sidx,
                        provenance: StaticProvenance {
                            origin: StaticGrantOrigin::Innate,
                            card_def: Some(ab_ref.card_def),
                            face_index: ab_ref.face_index,
                            component_index: ab_ref.component_index,
                            ability_index: ab_ref.ability_index as u32,
                            effect_id: 0,
                        },
                        static_def: &st.static_def,
                        span: st.span,
                        timestamp: perm.timestamp,
                        started_in_earlier_layer: false,
                        granted_item_id: None,
                        applied_targets: SmallVec::new(),
                    });
                }
                AbilityDef::Triggered(_)
                | AbilityDef::Replacement(_)
                | AbilityDef::Spell(_)
                | AbilityDef::Unsupported(_) => {
                    let item_id = perm.alloc_item_id();
                    perm.items.push(WorkAbilityItem {
                        id: item_id,
                        is_innate: true,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: false,
                        keyword: None,
                        keyword_param: None,
                        granted_ability: None,
                        timestamp: 0,
                        removed: false,
                    });
                }
            }
        }
        perm.sync_abilities();
        if saw_off_bf_grant {
            board.may_have_off_bf_char_grants = true;
            board.record_off_bf_grantor(info.id);
        }
        if saw_off_bf_unbounded {
            board.may_have_off_bf_char_grants = true;
            board.off_bf_grantors_unbounded = true;
        }
        if saw_all_colorless {
            board.may_have_all_off_bf_colorless = true;
        }
    }

    // 1c. Collect active Emblems and Graveyard continuous statics (`CR 113.6b`).
    let mut off_bf_statics: Vec<ActiveOffBfStatic<'_>> = Vec::new();
    for player in state.players() {
        if player.lost {
            continue;
        }
        for &id in &player.command {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            if let Some(card) = state.db().get(obj.card_def)
                && let Some(face) = card.faces.get(usize::from(obj.active_face_index()))
            {
                let face_idx = obj.active_face_index();
                for (sidx, ab) in face.abilities.iter().enumerate() {
                    if let AbilityDef::Static(st) = ab {
                        scan_ability_for_off_bf_flags(ab, id, &mut board);
                        record_static_dirty_reads(&board, &st.static_def);
                        active_layer_mask |= layer_mask_for_static_def(&st.static_def, false);
                        off_bf_statics.push(ActiveOffBfStatic {
                            origin: EffectOrigin::EmblemStatic {
                                source: id,
                                static_idx: sidx,
                            },
                            provenance: StaticProvenance {
                                origin: StaticGrantOrigin::Innate,
                                card_def: Some(obj.card_def),
                                face_index: face_idx,
                                component_index: 0,
                                ability_index: sidx as u32,
                                effect_id: 0,
                            },
                            source_id: id,
                            controller: obj.controller,
                            label: &face.name,
                            timestamp: obj.entered_timestamp,
                            static_def: &st.static_def,
                        });
                    }
                }
            }
        }
        for &id in &player.graveyard {
            let Some(obj) = state.object(id) else {
                continue;
            };
            let rare = state.rare(id);
            let base_def = rare
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            let has_gy_static = state.db().get(base_def).is_some_and(|card| {
                card.faces.iter().any(|f| {
                    f.abilities.iter().any(|ab| {
                        matches!(
                            ab,
                            AbilityDef::Static(st)
                                if matches!(
                                    &st.static_def.body,
                                    StaticBody::Continuous {
                                        active_in_zone: Some(Zone::Graveyard),
                                        ..
                                    }
                                )
                        )
                    })
                })
            }) || rare.is_some_and(|r| !r.merged_components.is_empty());
            if !has_gy_static {
                continue;
            }
            let opts = face_opts_for_game_object(obj, rare, None);
            let active_faces = resolve_object_faces(state.db(), obj.card_def, &opts)
                .map_or_else(SmallVec::new, |r| r.active_face_indices);
            let label = state
                .db()
                .get(obj.card_def)
                .map_or("graveyard card", |c| c.name.as_str());
            let mut sidx = 0usize;
            for_each_active_ability_with_card_on_faces(
                state.db(),
                obj.card_def,
                &opts,
                &active_faces,
                |_| true,
                |ab_ref| {
                    if let AbilityDef::Static(st) = ab_ref.ability
                        && let StaticBody::Continuous {
                            active_in_zone: Some(Zone::Graveyard),
                            ..
                        } = &st.static_def.body
                    {
                        scan_ability_for_off_bf_flags(ab_ref.ability, id, &mut board);
                        record_static_dirty_reads(&board, &st.static_def);
                        active_layer_mask |= layer_mask_for_static_def(&st.static_def, false);
                        off_bf_statics.push(ActiveOffBfStatic {
                            origin: EffectOrigin::GraveyardStatic {
                                source: id,
                                static_idx: sidx,
                            },
                            provenance: StaticProvenance {
                                origin: StaticGrantOrigin::Innate,
                                card_def: Some(ab_ref.card_def),
                                face_index: ab_ref.face_index,
                                component_index: ab_ref.component_index,
                                ability_index: ab_ref.ability_index as u32,
                                effect_id: 0,
                            },
                            source_id: id,
                            controller: obj.owner,
                            label,
                            timestamp: obj.entered_timestamp,
                            static_def: &st.static_def,
                        });
                    }
                    sidx += 1;
                },
            );
        }
    }

    // 1d. Collect active `TimedContinuousEffect`s.
    let mut active_timed: Vec<(usize, &TimedContinuousEffect)> = Vec::new();
    for (idx, te) in state.continuous_effects().iter().enumerate() {
        if is_timed_effect_active(te.source, te.controller, te.duration, &board, state) {
            if let ContinuousTarget::Filter(f) = &te.affected {
                board.record_read(dirty_mask_for_filter(f));
            }
            if layer_mod_may_grant_off_bf(&te.modifier) {
                board.may_have_off_bf_char_grants = true;
                board.off_bf_grantors_unbounded = true;
            }
            if layer_mod_may_make_off_bf_colorless(&te.modifier) {
                board.may_have_all_off_bf_colorless = true;
            }
            record_layer_mod_dirty_reads(&board, &te.modifier);
            active_layer_mask |= layer_mask_for_timed(te);
            active_timed.push((idx, te));
        }
    }

    let mut items_buf: Vec<LayerEffectItem<'_>> = Vec::new();

    // 2. Layer 1 timed effects (if any).
    if (active_layer_mask & LAYER_BIT_1) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer1,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer1,
            &mut items_buf,
            collect_notes,
        );
    }

    // 3. Layer 2: Control (CR 613.1b)
    if (active_layer_mask & LAYER_BIT_2) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer2,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer2,
            &mut items_buf,
            collect_notes,
        );
    }

    // 4. Layer 3: Text / SetName (CR 613.1c)
    if (active_layer_mask & LAYER_BIT_3) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer3,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer3,
            &mut items_buf,
            collect_notes,
        );
    }

    // 5. Layer 4: Types & Subtypes (CR 613.1d)
    // 5a. Subpass 4a: CDAs first (CR 613.3)
    if let Some(ckw) = subtype_table.changeling_kw {
        for perm in &mut board.perms {
            if !perm.is_face_down
                && perm
                    .eval
                    .types
                    .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                && perm.has_active_keyword(ckw)
            {
                perm.eval.has_all_creature_types = true;
                perm.push_note(collect_notes, || {
                    "Layer 4a: Changeling grants all creature types".into()
                });
            }
        }
    }
    if (active_layer_mask & LAYER_BIT_4A) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer4,
            Some(true),
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer4,
            &mut items_buf,
            collect_notes,
        );
    }

    // 5b. Subpass 4b: Non-CDA Layer 4 + Reconfigure (CR 702.151b)
    if (active_layer_mask & LAYER_BIT_4B) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer4,
            Some(false),
            &mut items_buf,
        );
        if has_reconfigure_cand && let Some(reconf_kw) = subtype_table.reconfigure_kw {
            let mut order = items_buf.len();
            for perm in &board.perms {
                if !perm.lost_innate_statics
                    && perm.has_active_keyword(reconf_kw)
                    && state
                        .object(perm.id)
                        .and_then(|o| o.attached_to)
                        .is_some_and(|t| board.perm(t).is_some())
                {
                    items_buf.push(LayerEffectItem {
                        origin: EffectOrigin::Reconfigure { host: perm.id },
                        provenance: StaticProvenance::default(),
                        source_id: Some(perm.id),
                        source_controller: perm.eval.controller,
                        source_label: Some("Reconfigure"),
                        timestamp: perm.timestamp,
                        order,
                        is_cda: false,
                        layer: Layer::Layer4,
                        payload: EffectPayload::ReconfigureStripCreature,
                        started_in_earlier_layer: false,
                        locked_targets: None,
                        timed_chosen_color: None,
                        timed_chosen_subtype: None,
                        timed_chosen_player: None,
                        timed_chosen_card_type: None,
                        timed_locked_pt: None,
                    });
                    order += 1;
                }
            }
        }
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer4,
            &mut items_buf,
            collect_notes,
        );
    }

    // End-of-Layer-4 cleanup (CR 205.1a)
    for perm in &mut board.perms {
        perm.strip_orphaned_subtypes(&subtype_table);
    }

    // 6. Layer 5: Colors (CR 613.1e)
    if (active_layer_mask & LAYER_BIT_5A) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer5,
            Some(true),
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer5,
            &mut items_buf,
            collect_notes,
        );
    }
    if (active_layer_mask & LAYER_BIT_5B) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer5,
            Some(false),
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer5,
            &mut items_buf,
            collect_notes,
        );
    }
    for perm in &mut board.perms {
        perm.eval.colors &= ColorMask::ALL_COLORS;
    }

    // 7. Layer 6: Abilities & Keywords (CR 613.1f)
    if (active_layer_mask & LAYER_BIT_6) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer6,
            None,
            &mut items_buf,
        );
        if has_l6_extra_keywords {
            let mut order = items_buf.len();
            // Flag-based keywords (Suspected -> Menace, Blitzed/Dashed/CastViaSuspend -> Haste)
            let menace_kw = subtype_table.menace_kw;
            let haste_kw = subtype_table.haste_kw;
            for perm in &board.perms {
                let Some(obj) = state.object(perm.id) else {
                    continue;
                };
                if obj.flags.contains(ObjectFlags::SUSPECTED)
                    && let Some(kw) = menace_kw
                {
                    items_buf.push(LayerEffectItem {
                        origin: EffectOrigin::FlagKeyword { host: perm.id },
                        provenance: StaticProvenance::default(),
                        source_id: Some(perm.id),
                        source_controller: perm.eval.controller,
                        source_label: Some("Suspected"),
                        timestamp: perm.timestamp,
                        order,
                        is_cda: false,
                        layer: Layer::Layer6,
                        payload: EffectPayload::GrantKeyword { keyword: kw },
                        started_in_earlier_layer: false,
                        locked_targets: None,
                        timed_chosen_color: None,
                        timed_chosen_subtype: None,
                        timed_chosen_player: None,
                        timed_chosen_card_type: None,
                        timed_locked_pt: None,
                    });
                    order += 1;
                }
                if obj.flags.intersects(
                    ObjectFlags::BLITZED | ObjectFlags::DASHED | ObjectFlags::CAST_VIA_SUSPEND,
                ) && let Some(kw) = haste_kw
                {
                    items_buf.push(LayerEffectItem {
                        origin: EffectOrigin::FlagKeyword { host: perm.id },
                        provenance: StaticProvenance::default(),
                        source_id: Some(perm.id),
                        source_controller: perm.eval.controller,
                        source_label: Some("Haste flag"),
                        timestamp: perm.timestamp,
                        order,
                        is_cda: false,
                        layer: Layer::Layer6,
                        payload: EffectPayload::GrantKeyword { keyword: kw },
                        started_in_earlier_layer: false,
                        locked_targets: None,
                        timed_chosen_color: None,
                        timed_chosen_subtype: None,
                        timed_chosen_player: None,
                        timed_chosen_card_type: None,
                        timed_locked_pt: None,
                    });
                    order += 1;
                }

                // Keyword counters (CR 122.1b, CR 613.7c)
                let rare = state.rare(perm.id);
                for &(cid, count) in state.counters(perm.id) {
                    if count == 0 {
                        continue;
                    }
                    if let Some(cname) = state.db().counter_name(cid)
                        && let Some(kw) = state.db().keyword_id(cname)
                    {
                        let ts = rare
                            .and_then(|r| {
                                r.counter_timestamps
                                    .iter()
                                    .find(|(c, _)| *c == cid)
                                    .map(|(_, t)| *t)
                            })
                            .unwrap_or(perm.timestamp);
                        items_buf.push(LayerEffectItem {
                            origin: EffectOrigin::KeywordCounter {
                                host: perm.id,
                                counter: cid,
                            },
                            provenance: StaticProvenance::default(),
                            source_id: Some(perm.id),
                            source_controller: perm.eval.controller,
                            source_label: Some(cname),
                            timestamp: ts,
                            order,
                            is_cda: false,
                            layer: Layer::Layer6,
                            payload: EffectPayload::GrantKeyword { keyword: kw },
                            started_in_earlier_layer: false,
                            locked_targets: None,
                            timed_chosen_color: None,
                            timed_chosen_subtype: None,
                            timed_chosen_player: None,
                            timed_chosen_card_type: None,
                            timed_locked_pt: None,
                        });
                        order += 1;
                    }
                }

                // Rare granted_keywords overlay (CR 613.7)
                if let Some(r) = rare {
                    for &kw in &r.granted_keywords {
                        items_buf.push(LayerEffectItem {
                            origin: EffectOrigin::FlagKeyword { host: perm.id },
                            provenance: StaticProvenance::default(),
                            source_id: Some(perm.id),
                            source_controller: perm.eval.controller,
                            source_label: Some("granted keyword"),
                            timestamp: perm.timestamp,
                            order,
                            is_cda: false,
                            layer: Layer::Layer6,
                            payload: EffectPayload::GrantKeyword { keyword: kw },
                            started_in_earlier_layer: false,
                            locked_targets: None,
                            timed_chosen_color: None,
                            timed_chosen_subtype: None,
                            timed_chosen_player: None,
                            timed_chosen_card_type: None,
                            timed_locked_pt: None,
                        });
                        order += 1;
                    }
                }
            }
        }
        const MAX_LAYER6_NESTED_PASSES: usize = 8;
        let mut prev_static_lens: SmallVec<[usize; 32]> = SmallVec::new();
        for pass in 0..=MAX_LAYER6_NESTED_PASSES {
            if items_buf.is_empty() {
                break;
            }
            let might_push_l6 = items_buf.iter().any(item_can_push_l6_static);
            if might_push_l6 {
                if prev_static_lens.is_empty() {
                    prev_static_lens.extend(board.perms.iter().map(|p| p.statics.len()));
                } else {
                    for (slot, perm) in prev_static_lens.iter_mut().zip(board.perms.iter()) {
                        *slot = perm.statics.len();
                    }
                }
            }
            let extra = apply_layer_with_deps(
                &mut board,
                state,
                &subtype_table,
                Layer::Layer6,
                &mut items_buf,
                collect_notes,
            );
            active_layer_mask |= extra;
            if pass == MAX_LAYER6_NESTED_PASSES || (extra & LAYER_BIT_6) == 0 || !might_push_l6 {
                break;
            }
            items_buf.clear();
            let mut order = 0usize;
            for (perm, &prev_len) in board.perms.iter().zip(prev_static_lens.iter()) {
                for st in perm.statics.get(prev_len..).into_iter().flatten() {
                    try_push_perm_static_item(
                        perm,
                        state,
                        st,
                        Layer::Layer6,
                        None,
                        &mut order,
                        &mut items_buf,
                    );
                }
            }
        }
    }
    if (active_layer_mask & (LAYER_BIT_4A | LAYER_BIT_4B | LAYER_BIT_6)) != 0 {
        for perm in &mut board.perms {
            perm.sync_abilities();
        }
    }

    // 8. Layer 7a: P/T CDAs (CR 613.4a) & CR 208.3 non-creature check
    for perm in &mut board.perms {
        if !perm.eval.types.contains(CardTypeMask::CREATURE) {
            perm.eval.base_pt = None;
            perm.eval.pt = None;
        } else if perm.eval.pt.is_none() {
            let zero = PowerToughness {
                power: 0,
                toughness: 0,
            };
            perm.eval.base_pt = Some(zero);
            perm.eval.pt = Some(zero);
        }
    }
    if (active_layer_mask & LAYER_BIT_7A) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer7a,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer7a,
            &mut items_buf,
            collect_notes,
        );
    }

    // 9. Layer 7b: SetBasePt / SetBasePower / SetBaseToughness (CR 613.4b)
    if (active_layer_mask & LAYER_BIT_7B) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer7b,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer7b,
            &mut items_buf,
            collect_notes,
        );
    }

    // 10. Layer 7c: Counters, temporary mods, and Layer 7c Pt statics/timed (CR 613.4c)
    for perm in &mut board.perms {
        if !perm.eval.types.contains(CardTypeMask::CREATURE) {
            continue;
        }
        let Some(pt) = &mut perm.eval.pt else {
            continue;
        };
        for &(cid, count) in state.counters(perm.id) {
            if count > 0
                && let Some((dp, dt)) = parse_pt_counter(state.db(), cid, count)
            {
                pt.power += dp;
                pt.toughness += dt;
                if collect_notes {
                    let cname = state.db().counter_name(cid).unwrap_or("counter");
                    perm.notes.push(format!(
                        "Layer 7c: {count}x {cname} counter ({dp:+}/{dt:+})"
                    ));
                }
            }
        }
        if let Some(rare) = state.rare(perm.id)
            && (rare.temp_power_mod != 0 || rare.temp_toughness_mod != 0)
        {
            pt.power += rare.temp_power_mod;
            pt.toughness += rare.temp_toughness_mod;
            if collect_notes {
                perm.notes.push(format!(
                    "Layer 7c: temp mod ({:+}/{:+})",
                    rare.temp_power_mod, rare.temp_toughness_mod
                ));
            }
        }
    }
    if (active_layer_mask & LAYER_BIT_7C) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer7c,
            None,
            &mut items_buf,
        );
        active_layer_mask |= apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer7c,
            &mut items_buf,
            collect_notes,
        );
    }

    // 11. Layer 7d: SwitchPt (CR 613.4d)
    for perm in &mut board.perms {
        if !perm.eval.types.contains(CardTypeMask::CREATURE) {
            continue;
        }
        if let Some(rare) = state.rare(perm.id)
            && !rare.switched_pt_count.is_multiple_of(2)
            && let Some(pt) = &mut perm.eval.pt
        {
            std::mem::swap(&mut pt.power, &mut pt.toughness);
            if collect_notes {
                perm.notes.push("Layer 7d: switched P/T".into());
            }
        }
    }
    if (active_layer_mask & LAYER_BIT_7D) != 0 {
        collect_layer_items(
            &board,
            state,
            &off_bf_statics,
            &active_timed,
            Layer::Layer7d,
            None,
            &mut items_buf,
        );
        let _ = apply_layer_with_deps(
            &mut board,
            state,
            &subtype_table,
            Layer::Layer7d,
            &mut items_buf,
            collect_notes,
        );
    }

    // 12. CR 613.10–613.11 Player Continuous Effects (RuleModifier)
    board.record_read(DirtyMask::PLAYER_STATE);
    let mut player_states: SmallVec<[EvaluatedPlayerState; 4]> =
        smallvec::smallvec![EvaluatedPlayerState::default(); state.players().len()];
    for (idx, p) in state.players().iter().enumerate() {
        if !p.lost && p.protection_from_everything.is_some() {
            player_states[idx].protection_from_everything = true;
        }
    }
    let mut all_off_battlefield_colorless = false;

    let mut rule_entries: Vec<RuleEffectEntry<'_>> = Vec::new();
    let mut r_order = 0usize;

    for perm in &board.perms {
        let rare = state.rare(perm.id);
        for st in &perm.statics {
            if let Some(item_id) = st.granted_item_id {
                if !perm.items.iter().any(|it| it.id == item_id && !it.removed) {
                    continue;
                }
            } else if (perm.lost_innate_statics || perm.extras.innate_abilities_removed)
                && !perm.statics.iter().any(|prev| {
                    matches!(&prev.static_def.body, StaticBody::Continuous { .. })
                        && !prev.applied_targets.is_empty()
                        && spans_or_affected_match(prev, st)
                })
            {
                continue;
            }
            if let StaticBody::RuleModifier { rule, who } = &st.static_def.body {
                if let Some(cond) = &st.static_def.condition {
                    let mut ctx = EvalContext::new(perm.eval.controller)
                        .with_board(BoardView::LayerWork(&board));
                    ctx.source = Some(perm.id);
                    if !evaluate_condition(state, cond, &ctx) {
                        continue;
                    }
                }
                let mut linked_names: SmallVec<[String; 2]> = SmallVec::new();
                if matches!(rule, RuleMod::CantCastSameNameAsLinkedExile) {
                    board.record_read(DirtyMask::EXILE);
                    for p in state.players() {
                        for &eid in &p.exile {
                            if state.rare(eid).and_then(|r| r.exiled_by) == Some(perm.id)
                                && let Some(eobj) = state.object(eid)
                                && let Some(opts) = face_opts_for_object(state, eid)
                                && let Some(res) =
                                    resolve_object_faces(state.db(), eobj.card_def, &opts)
                                && !res.name.is_empty()
                            {
                                linked_names.push(res.name);
                            }
                        }
                    }
                }
                rule_entries.push(RuleEffectEntry {
                    timestamp: st.timestamp,
                    order: r_order,
                    source: Some(perm.id),
                    controller: perm.eval.controller,
                    who: *who,
                    rule,
                    chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
                    chosen_card_type: rare.and_then(|r| r.chosen_card_type),
                    linked_exile_names: linked_names,
                });
                r_order += 1;
            }
        }
    }
    for off in &off_bf_statics {
        if let StaticBody::RuleModifier { rule, who } = &off.static_def.body {
            if let Some(cond) = &off.static_def.condition {
                let mut ctx =
                    EvalContext::new(off.controller).with_board(BoardView::LayerWork(&board));
                ctx.source = Some(off.source_id);
                if !evaluate_condition(state, cond, &ctx) {
                    continue;
                }
            }
            rule_entries.push(RuleEffectEntry {
                timestamp: off.timestamp,
                order: r_order,
                source: Some(off.source_id),
                controller: off.controller,
                who: *who,
                rule,
                chosen_card_name: None,
                chosen_card_type: None,
                linked_exile_names: SmallVec::new(),
            });
            r_order += 1;
        }
    }
    for pe in state.player_continuous_effects() {
        if is_timed_effect_active(pe.source, pe.controller, pe.duration, &board, state) {
            board.record_read(DirtyMask::PLAYER_STATE);
            record_rule_mod_dirty_reads(&board, &pe.rule);
            rule_entries.push(RuleEffectEntry {
                timestamp: pe.timestamp,
                order: r_order,
                source: pe.source,
                controller: pe.controller,
                who: pe.who,
                rule: &pe.rule,
                chosen_card_name: pe.chosen_card_name.clone(),
                chosen_card_type: pe.chosen_card_type,
                linked_exile_names: pe.linked_exile_names.clone(),
            });
            r_order += 1;
        }
    }

    rule_entries.sort_by_key(|e| (e.timestamp, e.order));
    for entry in rule_entries {
        let mut ctx = EvalContext::new(entry.controller).with_board(BoardView::LayerWork(&board));
        ctx.source = entry.source;
        for p in state.players() {
            if p.lost {
                continue;
            }
            if matches_player_filter(state, p.id, &entry.who, &ctx)
                && let Some(pstate) = player_states.get_mut(usize::from(p.id.0))
            {
                apply_rule_mod(pstate, &entry, &mut all_off_battlefield_colorless);
            }
        }
    }

    // 13. Build final `LayerCache` objects & extras tables; evaluate off-battlefield objects (`CR 604.3`).
    let mut objects: Vec<Option<(u32, EvaluatedObject)>> = vec![None; slot_cap];
    let mut extras: Vec<Option<EvaluatedObjectExtras>> = vec![None; slot_cap];
    let mut breakdown_notes: Vec<BreakdownNotesSlot> = if collect_notes {
        vec![None; slot_cap]
    } else {
        Vec::new()
    };

    let mut off_bf_char_grants: Vec<OffBfGrantSource<'_>> = Vec::new();
    for perm in &board.perms {
        for st in &perm.statics {
            if let Some(item_id) = st.granted_item_id {
                if !perm.items.iter().any(|it| it.id == item_id && !it.removed) {
                    continue;
                }
            } else if perm.lost_innate_statics || perm.eval.lost_all_abilities {
                continue;
            }
            if matches!(
                &st.static_def.body,
                StaticBody::OffBattlefieldCharacteristics { .. }
            ) {
                if let Some(cond) = &st.static_def.condition {
                    let mut cond_ctx = EvalContext::new(perm.eval.controller)
                        .with_board(BoardView::LayerWork(&board));
                    cond_ctx.source = Some(perm.id);
                    if !evaluate_condition(state, cond, &cond_ctx) {
                        continue;
                    }
                }
                off_bf_char_grants.push(OffBfGrantSource {
                    source_id: perm.id,
                    controller: perm.eval.controller,
                    static_def: st.static_def,
                });
            }
        }
    }
    for off in &off_bf_statics {
        if matches!(
            &off.static_def.body,
            StaticBody::OffBattlefieldCharacteristics { .. }
        ) {
            if let Some(cond) = &off.static_def.condition {
                let mut cond_ctx =
                    EvalContext::new(off.controller).with_board(BoardView::LayerWork(&board));
                cond_ctx.source = Some(off.source_id);
                if !evaluate_condition(state, cond, &cond_ctx) {
                    continue;
                }
            }
            off_bf_char_grants.push(OffBfGrantSource {
                source_id: off.source_id,
                controller: off.controller,
                static_def: off.static_def,
            });
        }
    }
    if !off_bf_char_grants.is_empty() {
        board.record_read(DirtyMask::TOPOLOGY);
    }
    let mut off_bf_grantor_ids: Vec<ObjectId> = Vec::with_capacity(off_bf_char_grants.len());
    for g in &off_bf_char_grants {
        if !off_bf_grantor_ids.contains(&g.source_id) {
            off_bf_grantor_ids.push(g.source_id);
        }
    }
    let off_bf_ids = OffBfLookupIds {
        changeling_kw: subtype_table.changeling_kw,
        devoid_kw: subtype_table.devoid_kw,
        spacecraft_sid: subtype_table.spacecraft_sid,
    };

    // Off-battlefield objects (phased-out permanents, lost-player permanents, Stack, Hand, Library, Graveyard, Exile, Command)
    for (id, obj) in state.objects().iter() {
        if board.perm(id).is_some() {
            continue;
        }
        let idx = id.index as usize;
        let eff_ctrl = match obj.zone {
            Zone::Stack => state
                .stack()
                .iter()
                .position(|&x| x == id)
                .and_then(|si| state.stack_entries().get(si))
                .map_or(obj.controller, |se| se.controller),
            Zone::Battlefield => obj.controller,
            _ => obj.owner,
        };
        let (ev, off_notes) = if obj.zone == Zone::Battlefield {
            let mut ctx = EvalContext::new(eff_ctrl).with_board(BoardView::BaseOnly);
            ctx.source = Some(id);
            evaluate_off_battlefield_object_with_grants(
                state,
                id,
                obj,
                &[],
                false,
                off_bf_ids,
                &ctx,
                collect_notes,
            )
        } else {
            let mut ctx = EvalContext::new(eff_ctrl).with_board(BoardView::LayerWork(&board));
            ctx.source = Some(id);
            evaluate_off_battlefield_object_with_grants(
                state,
                id,
                obj,
                &off_bf_char_grants,
                all_off_battlefield_colorless,
                off_bf_ids,
                &ctx,
                collect_notes,
            )
        };
        if idx < slot_cap {
            objects[idx] = Some((id.generation, ev));
            if collect_notes {
                breakdown_notes[idx] = Some((id.generation, off_notes));
            }
        }
    }

    let reads_mask = board.reads.get()
        | DirtyMask::TOPOLOGY
        | DirtyMask::COUNTERS
        | DirtyMask::DESIGNATIONS
        | DirtyMask::CONTINUOUS_EFFECTS
        | DirtyMask::PLAYER_STATE;

    let mut static_presence = StaticPresenceFlags::empty();
    for off in &off_bf_statics {
        static_presence |= static_presence_flag_for_body(&off.static_def.body);
    }
    for player in state.players() {
        for &id in &player.command {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                continue;
            }
            let rare = state.rare(id);
            let base_id = rare
                .and_then(|r| r.copiable.as_deref())
                .map_or(obj.card_def, |c| c.card_def);
            let might_have_flag = state.db().get(base_id).is_some_and(|card| {
                card.faces.iter().any(|f| {
                    f.abilities.iter().any(|ab| {
                        matches!(
                            ab,
                            AbilityDef::Static(st)
                                if !static_presence_flag_for_body(&st.static_def.body).is_empty()
                        )
                    })
                })
            }) || rare.is_some_and(|r| {
                !r.merged_components.is_empty()
                    || r.copiable
                        .as_deref()
                        .is_some_and(|c| c.merged_components.len() > 1)
            });
            if !might_have_flag {
                continue;
            }
            let opts = face_opts_for_game_object(obj, rare, None);
            if opts.is_face_down {
                continue;
            }
            if let Some(view) = resolve_object_faces(state.db(), obj.card_def, &opts) {
                for_each_active_ability_with_card_on_faces(
                    state.db(),
                    obj.card_def,
                    &opts,
                    &view.active_face_indices,
                    |_| false,
                    |ab_ref| {
                        if let AbilityDef::Static(st) = ab_ref.ability {
                            static_presence |= static_presence_flag_for_body(&st.static_def.body);
                        }
                    },
                );
            }
        }
    }

    for perm in board.perms {
        for st in &perm.statics {
            if let Some(item_id) = st.granted_item_id {
                if !perm.items.iter().any(|it| it.id == item_id && !it.removed) {
                    continue;
                }
            } else if perm.eval.lost_all_abilities {
                continue;
            }
            static_presence |= static_presence_flag_for_body(&st.static_def.body);
        }
        for granted in &perm.extras.granted_abilities {
            match granted {
                predicates::GrantedAbilityEntry::Inline { ability, .. } => {
                    if let AbilityDef::Static(st) = ability.as_ref() {
                        static_presence |= static_presence_flag_for_body(&st.static_def.body);
                    }
                }
                predicates::GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index,
                    ..
                } => {
                    if let Some(AbilityDef::Static(st)) = state
                        .db()
                        .get(*card_def)
                        .and_then(|c| c.faces.get(usize::from(*face_index)))
                        .and_then(|face| face.abilities.get(*ability_index))
                    {
                        static_presence |= static_presence_flag_for_body(&st.static_def.body);
                    }
                }
                predicates::GrantedAbilityEntry::IntrinsicBasicMana { .. }
                | predicates::GrantedAbilityEntry::DynamicWard { .. } => {}
            }
        }
        let idx = perm.id.index as usize;
        if idx < slot_cap {
            objects[idx] = Some((perm.id.generation, perm.eval));
            if !perm.extras.is_default() {
                extras[idx] = Some(perm.extras);
            }
            if collect_notes {
                breakdown_notes[idx] = Some((perm.id.generation, perm.notes));
            }
        }
    }

    let cache = LayerCache {
        rev: state.layer_rev(),
        reads_mask,
        objects,
        extras,
        players: player_states,
        all_off_battlefield_colorless,
        may_have_off_bf_char_grants: board.may_have_off_bf_char_grants,
        off_bf_grantor_ids,
        static_presence,
    };

    (cache, breakdown_notes)
}

#[inline]
fn record_static_dirty_reads_direct(
    cond: Option<&mtg_ir::Condition>,
    st: &StaticDef,
    reads: &std::cell::Cell<DirtyMask>,
) {
    let mut acc = reads.get();
    accumulate_static_dirty_reads(cond, st, &mut acc);
    reads.set(acc);
}

#[allow(non_snake_case)]
#[inline]
const fn SupertypeMask_empty() -> mtg_ir::SupertypeMask {
    mtg_ir::SupertypeMask::empty()
}

/// On-demand breakdown builder for `MatchSession::hover_breakdown`.
pub fn inspect_continuous_breakdown(
    state: &GameState,
    object: ObjectId,
    redact_face_down: bool,
) -> Option<ContinuousBreakdown> {
    let _obj = state.object(object)?;
    let (cache, notes_slots) = compute_layer_cache_inner(state, true);
    let ev = cache.get(object)?;
    let notes = if redact_face_down {
        notes_slots
            .get(object.index as usize)
            .and_then(|s| s.as_ref())
            .filter(|(slot_gen, _)| *slot_gen == object.generation)
            .map(|(_, ns)| {
                ns.iter()
                    .filter(|n| !n.starts_with("Layer 3:"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    } else {
        notes_slots
            .get(object.index as usize)
            .and_then(|s| s.as_ref())
            .filter(|(slot_gen, _)| *slot_gen == object.generation)
            .map(|(_, ns)| ns.clone())
            .unwrap_or_default()
    };

    Some(ContinuousBreakdown {
        object,
        base_pt: ev.base_pt,
        evaluated_pt: ev.pt,
        types: ev.types,
        colors: ev.colors,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mtg_ir::{
        AbilityDef, AbilityGrantSource, ActivatedAbility, Affected, Amount, CardDatabase, CardDef,
        CardDefId, CardLayout, CardTypeMask, Characteristics, ColorMask, Compare, CompareOp,
        Condition, CostDef, CounterTarget, Duration, FaceDef, FilterFlags, IR_V2_VERSION,
        InternTables, IrV2Bundle, KeywordAbility, KeywordDef, KeywordId, Layer, LayerMod,
        ManaColor, ManaCost, ObjectFilter, ObjectPredicate, PlayerFilter, PlayerRef,
        PlayerRelation, PrintedPt, RelativeControl, RuleMod, Span, StatKind, StaticAbility,
        StaticBody, StaticDef, SubtypeId, SupertypeMask, Timing, Zone, ZoneMask,
    };
    use smallvec::SmallVec;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::layers::predicates::GrantedAbilityEntry;
    use crate::zones::{MoveRawOptions, attach_raw, move_object, move_object_raw};

    fn make_card(
        name: &str,
        types: CardTypeMask,
        subtypes: SmallVec<[SubtypeId; 3]>,
        supertypes: SupertypeMask,
        colors: ColorMask,
        pt: Option<PrintedPt>,
        abilities: Vec<AbilityDef>,
    ) -> CardDef {
        CardDef {
            oracle_id: format!("oracle_{name}"),
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
                    supertypes,
                    colors,
                    color_indicator: None,
                    pt,
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

    fn static_ab(body: StaticBody, condition: Option<Condition>) -> AbilityDef {
        AbilityDef::Static(StaticAbility {
            static_def: StaticDef {
                body,
                condition: condition.map(Box::new),
            },
            span: Span::default(),
        })
    }

    fn kw_ab(kw: KeywordId) -> AbilityDef {
        AbilityDef::Keyword(KeywordAbility {
            keyword: KeywordDef {
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
            },
            id: None,
            span: Span::default(),
        })
    }

    fn mana_ab(id: &str) -> AbilityDef {
        AbilityDef::Activated(ActivatedAbility {
            id: id.into(),
            cost: vec![CostDef::TapSelf],
            targets: vec![],
            effect: vec![],
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
            span: Span::default(),
        })
    }

    fn setup_state(
        db: Arc<CardDatabase>,
        p0_cards: Vec<CardDefId>,
        p1_cards: Vec<CardDefId>,
    ) -> GameState {
        let mut main0 = p0_cards;
        while main0.len() < 15 {
            main0.push(CardDefId(0));
        }
        let mut main1 = p1_cards;
        while main1.len() < 15 {
            main1.push(CardDefId(0));
        }
        let d0 = DeckSpec {
            commanders: vec![],
            mainboard: main0,
            sideboard: vec![],
        };
        let d1 = DeckSpec {
            commanders: vec![],
            mainboard: main1,
            sideboard: vec![],
        };
        GameState::new(db, GameConfig::standard_1v1(), &[d0, d1], 77).unwrap()
    }

    fn move_card_to_bf(state: &mut GameState, player: PlayerId, def: CardDefId) -> ObjectId {
        let id = state.players()[player.0 as usize]
            .hand
            .iter()
            .chain(state.players()[player.0 as usize].library.iter())
            .copied()
            .find(|&oid| state.object(oid).is_some_and(|o| o.card_def == def))
            .expect("card must exist in hand/library");
        move_object_raw(
            state,
            id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap()
    }

    #[test]
    fn layer_ordering_2_to_7d_and_set_name_and_unclamped_pt() {
        let tables = InternTables {
            subtypes: vec!["Bear".into(), "Goblin".into()],
            counters: vec!["+1/+1".into(), "-1/-1".into()],
            keywords: vec!["flying".into(), "trample".into(), "menace".into()],
        };
        let cards = vec![
            // 0: Vanilla 1/3 green Bear
            make_card(
                "Synth Grizzly",
                CardTypeMask::CREATURE,
                smallvec::smallvec![SubtypeId(0)],
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 3,
                }),
                vec![kw_ab(KeywordId(0))],
            ),
            // 1: Layer 2 control stealer + Layer 3 name setter
            make_card(
                "Synth Mindtaker",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::U,
                None,
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer2,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::SetController {
                                controller: PlayerRef::You,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer3,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::SetName {
                                name: "Mindtaken".into(),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
            // 2: Layer 7c -3/-0 and +2/+0 + Layer 7d SwitchPt
            make_card(
                "Synth Twister",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7c,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::Pt {
                                power: Box::new(Amount::Const { n: -3 }),
                                toughness: Box::new(Amount::Const { n: 0 }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7c,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::Pt {
                                power: Box::new(Amount::Const { n: 2 }),
                                toughness: Box::new(Amount::Const { n: 0 }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(1), CardDefId(2)], vec![CardDefId(0)]);
        let bear_id = move_card_to_bf(&mut state, PlayerId(1), CardDefId(0));
        let _steal_id = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let _twist_id = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));

        let cache = state.evaluate_board();
        let ev = cache.get(bear_id).unwrap();
        assert_eq!(
            ev.controller,
            PlayerId(0),
            "Layer 2 must transfer control to P0"
        );
        assert_eq!(ev.name_override.as_deref(), Some("Mindtaken"));
        // 1 + (-3) + 2 = 0 (unclamped intermediate -2 + 2 = 0, not max(0, -2) + 2 = 2!)
        assert_eq!(
            ev.pt,
            Some(PowerToughness {
                power: 0,
                toughness: 3
            })
        );

        // Add timed SwitchPt in Layer 7d and negative toughness check
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer7c,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear_id]),
            modifier: LayerMod::Pt {
                power: Box::new(Amount::Const { n: -2 }),
                toughness: Box::new(Amount::Const { n: 0 }),
            },
            duration: Duration::EndOfTurn,
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
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer7d,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear_id]),
            modifier: LayerMod::SwitchPt,
            duration: Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        let ev2 = state.evaluate_board().get(bear_id).unwrap();
        // (0 - 2) / 3 = -2 / 3 -> switched in 7d -> 3 / -2
        assert_eq!(
            ev2.pt,
            Some(PowerToughness {
                power: 3,
                toughness: -2
            })
        );
    }

    #[test]
    fn cr_613_8a_urborg_and_blood_moon_both_timestamp_orders() {
        let tables = InternTables {
            subtypes: vec!["Mountain".into(), "Swamp".into()],
            counters: vec![],
            keywords: vec![],
        };
        let mtn_sub = SubtypeId(0);
        let swamp_sub = SubtypeId(1);
        let cards = vec![
            // 0: Basic Plains-like land (no basic subtype, has basic supertype)
            make_card(
                "Synth Basic",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::BASIC,
                ColorMask::empty(),
                None,
                vec![mana_ab("basic_mana")],
            ),
            // 1: Nonbasic Dual land with innate mana ability
            make_card(
                "Synth Dual",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![mana_ab("dual_g")],
            ),
            // 2: Urborg-like ("All lands are Swamps in addition to their other types")
            make_card(
                "Synth Swampmaker",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::AddTypes {
                            types: CardTypeMask::empty(),
                            subtypes: smallvec::smallvec![swamp_sub],
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 3: Blood Moon-like ("Nonbasic lands are Mountains")
            make_card(
                "Synth Moon",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                supertypes_none: SupertypeMask::BASIC,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetTypes {
                            types: CardTypeMask::LAND,
                            subtypes: smallvec::smallvec![mtn_sub],
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        // Case A: Urborg enters FIRST (earlier ts), Blood Moon enters SECOND (later ts).
        // CR 613.8a: Urborg depends on Blood Moon -> Blood Moon applies FIRST!
        let mut s1 = setup_state(
            Arc::clone(&db),
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![],
        );
        let basic1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(0));
        let dual1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(1));
        let urborg1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(2));
        let _moon1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(3));

        let c1 = s1.evaluate_board();
        assert!(!c1.get(basic1).unwrap().subtypes.contains(&swamp_sub));
        assert_eq!(c1.get(dual1).unwrap().subtypes.as_slice(), &[mtn_sub]);
        assert!(c1.get(dual1).unwrap().lost_all_abilities);
        assert!(c1.get(urborg1).unwrap().lost_all_abilities);
        let dual1_extra = c1.extra(dual1).unwrap();
        assert_eq!(
            dual1_extra.granted_abilities,
            vec![GrantedAbilityEntry::IntrinsicBasicMana {
                color: ManaColor::R,
                subtype: mtn_sub
            }]
        );

        // Case B: Blood Moon enters FIRST, Urborg enters SECOND -> identical result!
        let mut s2 = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![],
        );
        let basic2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(0));
        let dual2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(1));
        let _moon2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(3));
        let _urborg2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(2));

        let c2 = s2.evaluate_board();
        assert!(!c2.get(basic2).unwrap().subtypes.contains(&swamp_sub));
        assert_eq!(c2.get(dual2).unwrap().subtypes.as_slice(), &[mtn_sub]);
    }

    #[test]
    fn cr_613_8a_opalescence_and_humility_cr_613_6_persistence() {
        let tables = InternTables {
            subtypes: vec![],
            counters: vec![],
            keywords: vec![],
        };
        let cards = vec![
            // 0: Vanilla artifact
            make_card(
                "Synth Relic",
                CardTypeMask::ARTIFACT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![],
            ),
            // 1: Starfield ("Enchantments are Creatures + SetBasePt 4/4")
            make_card(
                "Synth Starfield",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer4,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::ENCHANTMENT,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::AddTypes {
                                types: CardTypeMask::CREATURE,
                                subtypes: SmallVec::new(),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7b,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::ENCHANTMENT,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::SetBasePt {
                                power: Box::new(Amount::Const { n: 4 }),
                                toughness: Box::new(Amount::Const { n: 4 }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
            // 2: Humility ("Creatures lose all abilities and have base P/T 1/1")
            make_card(
                "Synth Humility",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::RemoveAllAbilities {
                                keep_mana_abilities: false,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7b,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::SetBasePt {
                                power: Box::new(Amount::Const { n: 1 }),
                                toughness: Box::new(Amount::Const { n: 1 }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        // Starfield enters first (4/4), Humility enters second (1/1) -> both are 1/1 (Humility wins 7b timestamp).
        let mut s1 = setup_state(Arc::clone(&db), vec![CardDefId(1), CardDefId(2)], vec![]);
        let star1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(1));
        let hum1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(2));
        let c1 = s1.evaluate_board();
        assert!(c1.get(hum1).unwrap().types.contains(CardTypeMask::CREATURE));
        assert!(c1.get(star1).unwrap().lost_all_abilities);
        assert_eq!(
            c1.get(star1).unwrap().pt,
            Some(PowerToughness {
                power: 1,
                toughness: 1
            })
        );
        assert_eq!(
            c1.get(hum1).unwrap().pt,
            Some(PowerToughness {
                power: 1,
                toughness: 1
            })
        );

        // Humility enters first (1/1), Starfield enters second (4/4) -> CR 613.6 keeps Starfield's 7b effect active -> both are 4/4!
        let mut s2 = setup_state(db, vec![CardDefId(1), CardDefId(2)], vec![]);
        let hum2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(2));
        let star2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(1));
        let c2 = s2.evaluate_board();
        assert_eq!(
            c2.get(star2).unwrap().pt,
            Some(PowerToughness {
                power: 4,
                toughness: 4
            })
        );
        assert_eq!(
            c2.get(hum2).unwrap().pt,
            Some(PowerToughness {
                power: 4,
                toughness: 4
            })
        );
    }

    #[test]
    fn cr_613_8b_circular_dependency_loop_broken_by_timestamp() {
        let tables = InternTables::default();
        let cards = vec![
            // 0: Blue creature
            make_card(
                "Synth Drake",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::U,
                Some(PrintedPt::Fixed {
                    power: 2,
                    toughness: 2,
                }),
                vec![],
            ),
            // 1: Blue creatures are Red
            make_card(
                "Synth U2R",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer5,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                colors_any: ColorMask::U,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetColors {
                            colors: ColorMask::R,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 2: Red creatures are Blue
            make_card(
                "Synth R2U",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::U,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer5,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                colors_any: ColorMask::R,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetColors {
                            colors: ColorMask::U,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1), CardDefId(2)], vec![]);
        let drake = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _u2r = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1)); // ts earlier
        let _r2u = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2)); // ts later

        // Loop: U2R turns Drake Red (so R2U now matches it), R2U turns Red->Blue (changing U2R).
        // Cycle broken by timestamp: U2R applies first (Drake -> R), then R2U applies (Drake -> U).
        assert_eq!(
            state.evaluate_board().get(drake).unwrap().colors,
            ColorMask::U
        );
    }

    #[test]
    fn layer_6_grants_keep_mana_abilities_and_cant_have_or_gain() {
        let tables = InternTables {
            subtypes: vec![],
            counters: vec![],
            keywords: vec!["flying".into(), "hexproof".into()],
        };
        let flying = KeywordId(0);
        let hexproof = KeywordId(1);
        let cards = vec![
            // 0: Creature with innate flying, hexproof, and a mana ability
            make_card(
                "Synth Bird",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 0,
                    toughness: 1,
                }),
                vec![kw_ab(flying), kw_ab(hexproof), mana_ab("tap_mana")],
            ),
            // 1: Blood Sun-like ("RemoveAllAbilities { keep_mana_abilities: true }")
            make_card(
                "Synth Sun",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::RemoveAllAbilities {
                            keep_mana_abilities: true,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 2: Arcane Lighthouse-like ("CantHaveOrGain hexproof")
            make_card(
                "Synth Lighthouse",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::RemoveKeywords {
                            keywords: smallvec::smallvec![hexproof],
                            cant_have_or_gain: true,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1), CardDefId(2)], vec![]);
        let bird = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _sun = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let _light = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));

        // Grant flying AND hexproof AFTER Blood Sun & Lighthouse via timed effect:
        // - Flying survives because its grant timestamp > Blood Sun's removal timestamp.
        // - Hexproof does NOT survive because Lighthouse set `cant_have_or_gain: true` (CR 113.11)!
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bird]),
            modifier: LayerMod::AddKeywords {
                keywords: smallvec::smallvec![flying, hexproof],
                protection_attachment_exemption: None,
            },
            duration: Duration::EndOfTurn,
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

        let cache = state.evaluate_board();
        let ev = cache.get(bird).unwrap();
        assert!(ev.keywords.contains(&flying));
        assert!(
            !ev.keywords.contains(&hexproof),
            "CR 113.11 must forbid gaining hexproof"
        );
        assert!(ev.lost_all_abilities);
        assert!(
            !cache.extra(bird).unwrap().innate_mana_abilities_removed,
            "keep_mana_abilities must preserve innate mana abilities"
        );
    }

    #[test]
    fn dirty_mask_selective_skip_on_life_unless_condition_reads_life() {
        let tables = InternTables::default();
        let cards = vec![
            // 0: Vanilla creature
            make_card(
                "Synth Bear",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 2,
                    toughness: 2,
                }),
                vec![],
            ),
            // 1: Life-gated anthem ("If your life >= 25, creatures you control get +2/+2")
            make_card(
                "Synth Ascendant",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: 2 }),
                            toughness: Box::new(Amount::Const { n: 2 }),
                        },
                        active_in_zone: None,
                    },
                    Some(Condition::AnyPlayerLife {
                        op: CompareOp::Ge,
                        count: 25,
                    }),
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        // Board WITHOUT life condition: LIFE does not invalidate cache.
        let mut s1 = setup_state(Arc::clone(&db), vec![CardDefId(0)], vec![]);
        let _bear1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(0));
        let rev0 = s1.evaluate_board().rev;
        assert!(s1.has_cached_layer_eval());
        s1.mark_dirty(DirtyMask::LIFE);
        assert!(s1.has_cached_layer_eval());
        assert_eq!(s1.evaluate_board().rev, rev0 + 1);

        // Board WITH life condition: reads_mask includes LIFE -> LIFE change invalidates cache!
        let mut s2 = setup_state(db, vec![CardDefId(0), CardDefId(1)], vec![]);
        let bear2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(0));
        let _asc = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(1));
        assert_eq!(
            s2.evaluate_board().get(bear2).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
        assert!(s2.evaluate_board().reads_mask.contains(DirtyMask::LIFE));
        s2.player_mut(PlayerId(0)).unwrap().life = 26;
        s2.mark_dirty(DirtyMask::LIFE);
        assert!(!s2.has_cached_layer_eval());
        assert_eq!(
            s2.evaluate_board().get(bear2).unwrap().pt,
            Some(PowerToughness {
                power: 4,
                toughness: 4
            })
        );
    }

    #[test]
    fn cr_613_11_player_rule_mods_and_all_off_bf_colorless() {
        let tables = InternTables::default();
        let cards = vec![
            // 0: Colored spell card in hand
            make_card(
                "Synth Bolt",
                CardTypeMask::INSTANT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![],
            ),
            // 1: RuleModifier permanent (NoMaxHandSize, ExtraLandPlays(2), PlayerHexproof, AllOffBattlefieldAndSpellsColorless)
            make_card(
                "Synth Leyline",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![
                    static_ab(
                        StaticBody::RuleModifier {
                            rule: RuleMod::NoMaxHandSize,
                            who: PlayerFilter {
                                relation: PlayerRelation::You,
                            },
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::RuleModifier {
                            rule: RuleMod::ExtraLandPlays { count: 2 },
                            who: PlayerFilter {
                                relation: PlayerRelation::You,
                            },
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::RuleModifier {
                            rule: RuleMod::PlayerHexproof,
                            who: PlayerFilter {
                                relation: PlayerRelation::You,
                            },
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::RuleModifier {
                            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            who: PlayerFilter {
                                relation: PlayerRelation::EachPlayer,
                            },
                        },
                        None,
                    ),
                ],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(1), CardDefId(0)], vec![]);
        let _ley = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let hand_bolt = state.players()[0].hand[0];

        let cache = state.evaluate_board();
        let p0 = cache.player(PlayerId(0)).unwrap();
        assert!(p0.no_max_hand_size);
        assert_eq!(p0.effective_max_hand_size(7), None);
        assert_eq!(p0.extra_land_plays, 2);
        assert!(p0.hexproof);
        assert!(cache.all_off_battlefield_colorless);
        assert_eq!(cache.get(hand_bolt).unwrap().colors, ColorMask::empty());
    }

    #[test]
    fn cr_205_1a_stripping_creature_removes_creature_subtypes_and_preserves_supertypes() {
        let tables = InternTables {
            subtypes: vec!["Elf".into(), "Equipment".into(), "Forest".into()],
            counters: vec![],
            keywords: vec![],
        };
        let elf = SubtypeId(0);
        let equip = SubtypeId(1);
        let forest = SubtypeId(2);
        let cards = vec![
            // 0: Legendary Artifact Creature — Elf Equipment
            make_card(
                "Synth GearElf",
                CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                smallvec::smallvec![elf, equip],
                SupertypeMask::LEGENDARY,
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 2,
                    toughness: 2,
                }),
                vec![],
            ),
            // 1: Land — Forest
            make_card(
                "Synth Forest",
                CardTypeMask::LAND,
                smallvec::smallvec![forest],
                SupertypeMask::BASIC,
                ColorMask::empty(),
                None,
                vec![],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1)], vec![]);
        let gear_elf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![gear_elf]),
            modifier: LayerMod::RemoveTypes {
                types: CardTypeMask::CREATURE,
            },
            duration: Duration::EndOfTurn,
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
        let ev = state.evaluate_board().get(gear_elf).unwrap();
        assert_eq!(ev.types, CardTypeMask::ARTIFACT);
        assert_eq!(
            ev.supertypes,
            SupertypeMask::LEGENDARY,
            "CR 205.4b preserves supertypes"
        );
        assert_eq!(
            ev.subtypes.as_slice(),
            &[equip],
            "CR 205.1a strips Elf, keeps Equipment"
        );
        assert_eq!(ev.pt, None, "CR 208.3 non-creature has None P/T");
        assert_eq!(ev.base_pt, None);
    }

    #[test]
    fn rename_invariance_across_layer_pipeline() {
        let make_db = |n1: &str, n2: &str| {
            let tables = InternTables {
                subtypes: vec!["Mountain".into()],
                counters: vec!["+1/+1".into()],
                keywords: vec!["flying".into()],
            };
            let cards = vec![
                make_card(
                    n1,
                    CardTypeMask::CREATURE,
                    SmallVec::new(),
                    SupertypeMask::empty(),
                    ColorMask::W,
                    Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    vec![kw_ab(KeywordId(0))],
                ),
                make_card(
                    n2,
                    CardTypeMask::ENCHANTMENT,
                    SmallVec::new(),
                    SupertypeMask::empty(),
                    ColorMask::W,
                    None,
                    vec![static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7c,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::Pt {
                                power: Box::new(Amount::Const { n: 1 }),
                                toughness: Box::new(Amount::Const { n: 1 }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    )],
                ),
            ];
            Arc::new(
                CardDatabase::from_bundle(IrV2Bundle {
                    ir_version: IR_V2_VERSION,
                    source_compiler_version: 46,
                    tables,
                    cards,
                    decks: vec![],
                })
                .unwrap(),
            )
        };
        let mut s1 = setup_state(
            make_db("Alpha Bear", "Alpha Anthem"),
            vec![CardDefId(0), CardDefId(1)],
            vec![],
        );
        let b1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(0));
        let _a1 = move_card_to_bf(&mut s1, PlayerId(0), CardDefId(1));

        let mut s2 = setup_state(
            make_db("Zeta Beast", "Zeta Blessing"),
            vec![CardDefId(0), CardDefId(1)],
            vec![],
        );
        let b2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(0));
        let _a2 = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(1));

        assert_eq!(s1.evaluate_board().get(b1), s2.evaluate_board().get(b2));
    }

    #[test]
    fn duration_for_as_long_as_self_remains_tapped_tracks_tapped_dirty_mask() {
        let tables = InternTables::default();
        let cards = vec![make_card(
            "Synth Bear",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        )];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0)], vec![]);
        let bear = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        state
            .object_mut(bear)
            .unwrap()
            .flags
            .insert(ObjectFlags::TAPPED);
        state.mark_dirty(DirtyMask::TAPPED);
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: Some(bear),
            controller: PlayerId(0),
            layer: Layer::Layer7c,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear]),
            modifier: LayerMod::Pt {
                power: Box::new(Amount::Const { n: 3 }),
                toughness: Box::new(Amount::Const { n: 3 }),
            },
            duration: Duration::ForAsLongAsSelfRemainsTapped,
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

        let c1 = state.evaluate_board();
        assert!(c1.reads_mask.contains(DirtyMask::TAPPED));
        assert_eq!(
            c1.get(bear).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );

        state
            .object_mut(bear)
            .unwrap()
            .flags
            .remove(ObjectFlags::TAPPED);
        state.mark_dirty(DirtyMask::TAPPED);
        assert_eq!(
            state.evaluate_board().get(bear).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
    }

    #[test]
    fn hover_breakdown_includes_notes_and_redacts_face_down_name() {
        let tables = InternTables::default();
        let cards = vec![make_card(
            "Secret Creature",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::U,
            Some(PrintedPt::Fixed {
                power: 5,
                toughness: 5,
            }),
            vec![],
        )];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0)], vec![]);
        let obj = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        state
            .object_mut(obj)
            .unwrap()
            .flags
            .insert(ObjectFlags::FACE_DOWN);
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer7c,
            affected: ContinuousTarget::Objects(smallvec::smallvec![obj]),
            modifier: LayerMod::Pt {
                power: Box::new(Amount::Const { n: 1 }),
                toughness: Box::new(Amount::Const { n: 1 }),
            },
            duration: Duration::EndOfTurn,
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

        let bd = inspect_continuous_breakdown(&state, obj, true).unwrap();
        assert_eq!(
            bd.evaluated_pt,
            Some(PowerToughness {
                power: 3,
                toughness: 3
            })
        );
        assert!(bd.notes.iter().any(|n| n.contains("Layer 7c")));
        assert!(!bd.notes.iter().any(|n| n.contains("Secret Creature")));

        // Verify attach_raw test helper works cleanly with layers
        let obj2 = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        attach_raw(&mut state, obj2, obj).unwrap();
        let _ = state.evaluate_board();
    }

    #[test]
    fn granted_static_after_remove_all_abilities_layer7_and_cr_613_11() {
        let tables = InternTables {
            subtypes: vec!["Mountain".into()],
            counters: vec![],
            keywords: vec![],
        };
        let mtn = SubtypeId(0);
        let cards = vec![
            // 0: Vanilla 2/2 Bear
            make_card(
                "Synth Bear",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 2,
                    toughness: 2,
                }),
                vec![],
            ),
            // 1: Humility-like RemoveAllAbilities
            make_card(
                "Synth Humility",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::RemoveAllAbilities {
                            keep_mana_abilities: false,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 2: Nonbasic Land (target for Blood Moon)
            make_card(
                "Synth Nonbasic",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![],
            ),
            // 3: Blood Moon-like SetTypes { LAND, [Mountain] }
            make_card(
                "Synth Moon",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                supertypes_none: SupertypeMask::BASIC,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetTypes {
                            types: CardTypeMask::LAND,
                            subtypes: smallvec::smallvec![mtn],
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![],
        );
        let bear = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _humility = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1)); // ts earlier
        let nonbasic = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let _moon = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));

        // Grant Layer 7c (+2/+2 if AnyPlayerLife >= 25) and CR 613.11 ExtraLandPlays(1) after Humility & Moon
        let ts_grant = state.allocate_timestamp();
        let granted_l7c = static_ab(
            StaticBody::Continuous {
                layer: Layer::Layer7c,
                affected: Affected::SelfObj,
                modifier: LayerMod::Pt {
                    power: Box::new(Amount::Const { n: 2 }),
                    toughness: Box::new(Amount::Const { n: 2 }),
                },
                active_in_zone: None,
            },
            Some(Condition::AnyPlayerLife {
                op: CompareOp::Ge,
                count: 25,
            }),
        );
        let granted_rule = static_ab(
            StaticBody::RuleModifier {
                rule: RuleMod::ExtraLandPlays { count: 1 },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            None,
        );
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear]),
            modifier: LayerMod::GrantAbility {
                ability: Box::new(granted_l7c),
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_grant,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        state.add_continuous_effect(TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear, nonbasic]),
            modifier: LayerMod::GrantAbility {
                ability: Box::new(granted_rule),
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_grant,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        // Before life >= 25: bear is 2/2, reads_mask tracks LIFE, and both granted RuleModifiers apply (+2 land plays).
        {
            let c1 = state.evaluate_board();
            assert!(c1.reads_mask.contains(DirtyMask::LIFE));
            assert_eq!(
                c1.get(bear).unwrap().pt,
                Some(PowerToughness {
                    power: 2,
                    toughness: 2
                })
            );
            assert_eq!(c1.player(PlayerId(0)).unwrap().extra_land_plays, 2);
        }

        // Raise life to 26 -> granted Layer 7c static becomes active (4/4) despite earlier Humility!
        state.player_mut(PlayerId(0)).unwrap().life = 26;
        state.mark_dirty(DirtyMask::LIFE);
        {
            let c2 = state.evaluate_board();
            assert_eq!(
                c2.get(bear).unwrap().pt,
                Some(PowerToughness {
                    power: 4,
                    toughness: 4
                })
            );
            assert_eq!(c2.player(PlayerId(0)).unwrap().extra_land_plays, 2);
        }

        // Now add a LATER RemoveAllAbilities (ts_remove > ts_grant) on bear: removes bear's granted L7c & RuleModifier,
        // while nonbasic (under Blood Moon's lost_innate_statics) still keeps its granted RuleModifier!
        let ts_remove = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 3,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![bear]),
            modifier: LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_remove,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        let c3 = state.evaluate_board();
        assert_eq!(
            c3.get(bear).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
        assert_eq!(c3.player(PlayerId(0)).unwrap().extra_land_plays, 1);
    }

    #[test]
    fn commander_pre_game_color_choice_bf_and_designations_dirty() {
        let tables = InternTables::default();
        let cards = vec![
            // 0: Colorless creature with CommanderPreGameColorChoice
            make_card(
                "Synth Piper",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::LEGENDARY,
                ColorMask::empty(),
                Some(PrintedPt::Fixed {
                    power: 3,
                    toughness: 3,
                }),
                vec![static_ab(StaticBody::CommanderPreGameColorChoice, None)],
            ),
            // 1: Filler card
            make_card(
                "Synth Filler",
                CardTypeMask::ARTIFACT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut main0 = vec![CardDefId(0)];
        main0.resize(60, CardDefId(1));
        let d0 = DeckSpec {
            commanders: vec![CardDefId(0)],
            mainboard: main0.clone(),
            sideboard: vec![],
        };
        let d1 = DeckSpec {
            commanders: vec![CardDefId(1)],
            mainboard: main0,
            sideboard: vec![],
        };
        let mut state = GameState::new(db, GameConfig::commander(2), &[d0, d1], 11).unwrap();
        let cmd_id = state.players()[0].command[0];
        let non_cmd_id = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));

        // Set chosen_color = U on both commander (in Command) and non-commander (on BF)
        state.rare_or_default(cmd_id).unwrap().chosen_color = Some(ColorMask::U);
        state.rare_or_default(non_cmd_id).unwrap().chosen_color = Some(ColorMask::U);
        state.mark_dirty(DirtyMask::DESIGNATIONS);

        {
            let c1 = state.evaluate_board();
            assert_eq!(c1.get(cmd_id).unwrap().colors, ColorMask::U);
            assert_eq!(c1.get(non_cmd_id).unwrap().colors, ColorMask::empty());
        }

        // Move commander to battlefield
        let cmd_bf = move_object_raw(
            &mut state,
            cmd_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        {
            let c2 = state.evaluate_board();
            assert!(c2.reads_mask.contains(DirtyMask::DESIGNATIONS));
            assert_eq!(c2.get(cmd_bf).unwrap().colors, ColorMask::U);
        }

        // Mutate chosen_color -> R and mark DESIGNATIONS dirty
        state.rare_or_default(cmd_bf).unwrap().chosen_color = Some(ColorMask::R);
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        assert_eq!(
            state.evaluate_board().get(cmd_bf).unwrap().colors,
            ColorMask::R
        );

        // Layer 6 RemoveAllAbilities runs AFTER Layer 5 (CR 613.1e/f), so CommanderPreGameColorChoice
        // already applied in Layer 5. Turning the commander into a Land in Layer 4 (lost_innate_statics)
        // DOES suppress CommanderPreGameColorChoice!
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![cmd_bf]),
            modifier: LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            duration: Duration::EndOfTurn,
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
        assert_eq!(
            state.evaluate_board().get(cmd_bf).unwrap().colors,
            ColorMask::R
        );

        let tables2 = InternTables {
            subtypes: vec!["Mountain".into()],
            counters: vec![],
            keywords: vec![],
        };
        let mtn = SubtypeId(0);
        let cards2 = vec![
            make_card(
                "Synth Land Piper",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::LEGENDARY,
                ColorMask::empty(),
                None,
                vec![static_ab(StaticBody::CommanderPreGameColorChoice, None)],
            ),
            make_card(
                "Synth Moon",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                supertypes_none: SupertypeMask::BASIC,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetTypes {
                            types: CardTypeMask::LAND,
                            subtypes: smallvec::smallvec![mtn],
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db2 = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: tables2,
                cards: cards2,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut main_b = vec![CardDefId(1)];
        main_b.resize(60, CardDefId(1));
        let d_a = DeckSpec {
            commanders: vec![CardDefId(0)],
            mainboard: main_b.clone(),
            sideboard: vec![],
        };
        let d_b = DeckSpec {
            commanders: vec![CardDefId(1)],
            mainboard: main_b,
            sideboard: vec![],
        };
        let mut s2 = GameState::new(db2, GameConfig::commander(2), &[d_a, d_b], 12).unwrap();
        let cmd2 = s2.players()[0].command[0];
        let cmd2_bf = move_object_raw(
            &mut s2,
            cmd2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        s2.rare_or_default(cmd2_bf).unwrap().chosen_color = Some(ColorMask::U);
        let _moon = move_card_to_bf(&mut s2, PlayerId(0), CardDefId(1));
        assert_eq!(
            s2.evaluate_board().get(cmd2_bf).unwrap().colors,
            ColorMask::empty(),
            "Layer 4 Blood Moon sets lost_innate_statics and suppresses Layer 5 CommanderPreGameColorChoice"
        );
    }

    #[test]
    fn cr_613_6_without_remove_abilities_and_with_blood_moon_and_distinct_spans() {
        let tables = InternTables {
            subtypes: vec!["Mountain".into()],
            counters: vec![],
            keywords: vec![],
        };
        let mtn = SubtypeId(0);
        let shared_span = Span { start: 10, end: 50 };
        let other_span = Span { start: 60, end: 90 };
        let noncreature_artifact_filter = ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_all: CardTypeMask::ARTIFACT,
            types_none: CardTypeMask::CREATURE,
            ..Default::default()
        };
        let cards = vec![
            // 0: Non-creature Artifact
            make_card(
                "Synth Idol",
                CardTypeMask::ARTIFACT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![],
            ),
            // 1: Shared-span animator (L4 AddTypes(CREATURE) + L7b SetBasePt(5/5) filtering noncreature artifact)
            // PLUS an independent other_span L7b SetBasePt(9/9) with the SAME filter!
            make_card(
                "Synth Animator",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::U,
                None,
                vec![
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer4,
                                affected: Affected::Filter {
                                    filter: Box::new(noncreature_artifact_filter.clone()),
                                },
                                modifier: LayerMod::AddTypes {
                                    types: CardTypeMask::CREATURE,
                                    subtypes: SmallVec::new(),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: shared_span,
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7b,
                                affected: Affected::Filter {
                                    filter: Box::new(noncreature_artifact_filter.clone()),
                                },
                                modifier: LayerMod::SetBasePt {
                                    power: Box::new(Amount::Const { n: 5 }),
                                    toughness: Box::new(Amount::Const { n: 5 }),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: shared_span,
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7b,
                                affected: Affected::Filter {
                                    filter: Box::new(noncreature_artifact_filter),
                                },
                                modifier: LayerMod::SetBasePt {
                                    power: Box::new(Amount::Const { n: 9 }),
                                    toughness: Box::new(Amount::Const { n: 9 }),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: other_span,
                    }),
                ],
            ),
            // 2: Nonbasic Land with shared-span L2 SetController(P0) + L5 SetColors(G)
            make_card(
                "Synth SelfLand",
                CardTypeMask::LAND,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer2,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetController {
                                    controller: PlayerRef::You,
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: shared_span,
                    }),
                    AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer5,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetColors {
                                    colors: ColorMask::G,
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: shared_span,
                    }),
                ],
            ),
            // 3: Blood Moon
            make_card(
                "Synth Moon",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                supertypes_none: SupertypeMask::BASIC,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::SetTypes {
                            types: CardTypeMask::LAND,
                            subtypes: smallvec::smallvec![mtn],
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![],
        );
        let idol = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _anim = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let self_land = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let _moon = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));

        let cache = state.evaluate_board();
        let idol_ev = cache.get(idol).unwrap();
        assert!(idol_ev.types.contains(CardTypeMask::CREATURE));
        // shared_span L7b 5/5 applies via locked_targets even without innate_abilities_removed;
        // other_span L7b 9/9 does NOT falsely merge and thus skips idol (now a creature)!
        assert_eq!(
            idol_ev.pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );
        // self_land lost innate statics to Blood Moon in L4, but its shared_span L2->L5 effect persists in L5!
        let land_ev = cache.get(self_land).unwrap();
        assert_eq!(land_ev.subtypes.as_slice(), &[mtn]);
        assert_eq!(land_ev.colors, ColorMask::G);
    }

    #[test]
    fn filter_mana_value_deps_and_keep_mana_abilities_grant_and_pw_subtypes() {
        let tables = InternTables {
            subtypes: vec![
                "Forest".into(),
                "Dryad".into(),
                "Treefolk".into(),
                "Jace".into(),
            ],
            counters: vec![],
            keywords: vec![],
        };
        let forest = SubtypeId(0);
        let dryad = SubtypeId(1);
        let treefolk = SubtypeId(2);
        let jace = SubtypeId(3);
        let cards = vec![
            // 0: Creature with MV 4 and base 2/2
            CardDef {
                oracle_id: "oracle_mv4".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Four".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Four".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{2}{G}{G}"),
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
                    abilities: vec![mana_ab("tap_g"), kw_ab(KeywordId(0))],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            },
            // 1: Dynamic MV filter in L7c: creatures with mana_value == power get +1/+1
            // AND SetBasePt(4/4) in L7b? Wait: let's test two L7c effects:
            //   - ts=10: creatures with mana_value == self.power get +1/+1 (depends on ts=20!)
            //   - ts=20: creatures get +2/+2 (turns 2/2 into 4/4 so mana_value 4 == power 4!)
            make_card(
                "Synth MvBuffer",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::G,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                mana_value: Some(Compare {
                                    op: CompareOp::Eq,
                                    value: Box::new(Amount::GreatestStatAmong {
                                        filter: Box::new(ObjectFilter {
                                            zones: ZoneMask::BATTLEFIELD,
                                            types_any: CardTypeMask::CREATURE,
                                            ..Default::default()
                                        }),
                                        stat: StatKind::Power,
                                    }),
                                }),
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: 1 }),
                            toughness: Box::new(Amount::Const { n: 1 }),
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 2: +2/+2 anthem
            make_card(
                "Synth Anthem",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: 2 }),
                            toughness: Box::new(Amount::Const { n: 2 }),
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
            // 3: Land Creature — Forest Dryad
            make_card(
                "Synth Arbor",
                CardTypeMask::LAND | CardTypeMask::CREATURE,
                smallvec::smallvec![forest, dryad],
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                }),
                vec![],
            ),
            // 4: Planeswalker — Jace
            make_card(
                "Synth Walker",
                CardTypeMask::PLANESWALKER,
                smallvec::smallvec![jace],
                SupertypeMask::empty(),
                ColorMask::U,
                None,
                vec![],
            ),
            // 5: GrantAbilitiesFromObjects(Battlefield) + RemoveAllAbilities(keep_mana_abilities: true)
            make_card(
                "Synth Copier",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::U,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                }),
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    flags: FilterFlags::OTHER,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::RemoveAllAbilities {
                                keep_mana_abilities: true,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::GrantAbilitiesFromObjects {
                                from: AbilityGrantSource::Battlefield {
                                    filter: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::CREATURE,
                                        flags: FilterFlags::OTHER,
                                        ..Default::default()
                                    }),
                                },
                                mana_as_any_color: false,
                                once_each_turn: false,
                                loyalty_only: false,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
        ];
        // Fix keyword table for CardDef(0)
        let mut tables = tables;
        tables.keywords.push("flying".into());
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(4),
                CardDefId(5),
            ],
            vec![],
        );
        let four = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _mv_buf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1)); // ts earlier
        let _anthem = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2)); // ts later
        let arbor = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let walker = move_card_to_bf(&mut state, PlayerId(0), CardDefId(4));
        let copier = move_card_to_bf(&mut state, PlayerId(0), CardDefId(5));

        // Lignify-like SetSubtypes([Treefolk]) on arbor
        let ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![arbor]),
            modifier: LayerMod::SetSubtypes {
                subtypes: smallvec::smallvec![treefolk],
            },
            duration: Duration::EndOfTurn,
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

        let c1 = state.evaluate_board();
        // Dynamic mana_value dependency detected in L7c: +2/+2 runs before +1/+1 -> 2+2+1 = 5/5!
        assert_eq!(
            c1.get(four).unwrap().pt,
            Some(PowerToughness {
                power: 5,
                toughness: 5
            })
        );
        // Domain-aware SetSubtypes preserves Forest on Land Creature
        assert!(c1.get(arbor).unwrap().subtypes.contains(&forest));
        assert!(c1.get(arbor).unwrap().subtypes.contains(&treefolk));
        assert!(!c1.get(arbor).unwrap().subtypes.contains(&dryad));
        // Planeswalker retains Jace subtype on battlefield
        assert_eq!(c1.get(walker).unwrap().subtypes.as_slice(), &[jace]);
        // Copier copied four's innate mana ability (preserved by keep_mana_abilities: true), NOT flying!
        let copier_extra = c1.extra(copier).unwrap();
        assert!(
            copier_extra
                .granted_abilities
                .iter()
                .any(|g| matches!(g, GrantedAbilityEntry::CardAbility { is_mana: true, .. })),
            "copier must receive innate mana ability when keep_mana_abilities is true"
        );
        assert!(c1.get(copier).unwrap().keywords.is_empty());

        // Stripping PLANESWALKER type strips Jace subtype (CR 205.1a)
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![walker]),
            modifier: LayerMod::RemoveTypes {
                types: CardTypeMask::PLANESWALKER,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert!(
            state
                .evaluate_board()
                .get(walker)
                .unwrap()
                .subtypes
                .is_empty()
        );
    }

    #[test]
    fn phased_out_permanent_layer_cache_pt_mv_controller_and_colors_parity() {
        let tables = InternTables {
            subtypes: vec!["Goblin".into()],
            counters: vec![],
            keywords: vec![],
        };
        let cards = vec![
            // 0: {2}{R}{R} -1/3 Red Creature
            CardDef {
                oracle_id: "oracle_rumbler".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Rumbler".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Rumbler".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{2}{R}{R}"),
                        types: CardTypeMask::CREATURE,
                        subtypes: smallvec::smallvec![SubtypeId(0)],
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::R,
                        color_indicator: None,
                        pt: Some(PrintedPt::Fixed {
                            power: -1,
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
            },
            // 1: AllOffBattlefieldAndSpellsColorless + OffBattlefieldCharacteristics(add_colors: U)
            make_card(
                "Synth OffBf",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![
                    static_ab(
                        StaticBody::RuleModifier {
                            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            who: PlayerFilter {
                                relation: PlayerRelation::EachPlayer,
                            },
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::OffBattlefieldCharacteristics {
                            zones: ZoneMask::all(),
                            filter: None,
                            all_players: true,
                            add_all_creature_types: false,
                            add_chosen_subtype: false,
                            add_types: CardTypeMask::empty(),
                            add_subtypes: SmallVec::new(),
                            add_chosen_color: false,
                            add_colors: ColorMask::U,
                        },
                        None,
                    ),
                ],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1)], vec![]);
        let rumbler = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _off = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // Transfer controller to P1 and phase out
        crate::zones::transfer_control_raw(&mut state, rumbler, PlayerId(1)).unwrap();
        state
            .object_mut(rumbler)
            .unwrap()
            .flags
            .insert(ObjectFlags::PHASED_OUT);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let cache = state.evaluate_board();
        let ev = cache.get(rumbler).unwrap();
        assert_eq!(ev.controller, PlayerId(1));
        assert_eq!(
            ev.pt,
            Some(PowerToughness {
                power: -1,
                toughness: 3
            })
        );
        assert_eq!(
            ev.base_pt,
            Some(PowerToughness {
                power: -1,
                toughness: 3
            })
        );
        assert_eq!(ev.mana_value, 4);
        assert_eq!(ev.colors, ColorMask::R);

        let bd = inspect_continuous_breakdown(&state, rumbler, false).unwrap();
        assert_eq!(bd.base_pt, ev.base_pt);
        assert_eq!(bd.evaluated_pt, ev.pt);

        let ctx_cached = EvalContext::new(PlayerId(1)).with_board(BoardView::Cached(cache));
        let ctx_base = EvalContext::new(PlayerId(1)).with_board(BoardView::BaseOnly);
        assert_eq!(
            crate::eval::object_pt(&state, rumbler, &ctx_cached),
            crate::eval::object_pt(&state, rumbler, &ctx_base)
        );
        assert_eq!(
            crate::eval::object_mana_value(&state, rumbler, &ctx_cached),
            crate::eval::object_mana_value(&state, rumbler, &ctx_base)
        );

        let bf_filter = ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            ..Default::default()
        };
        assert!(!crate::filter::matches_object_filter(
            &state,
            rumbler,
            &bf_filter,
            &ctx_cached
        ));
    }

    #[test]
    fn layer6_fast_path_vs_slow_path_parity() {
        let tables = InternTables {
            subtypes: vec![],
            counters: vec![],
            keywords: vec![
                "flying".into(),
                "trample".into(),
                "vigilance".into(),
                "haste".into(),
                "cycling".into(),
            ],
        };
        let flying = KeywordId(0);
        let trample = KeywordId(1);
        let vigilance = KeywordId(2);
        let haste = KeywordId(3);
        let cycling = KeywordId(4);
        let cards = vec![
            // 0: Creature with cycling
            make_card(
                "Synth Cyclops",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                Some(PrintedPt::Fixed {
                    power: 3,
                    toughness: 3,
                }),
                vec![kw_ab(cycling)],
            ),
            // 1: 4 Layer 6 anthems (fast-path eligible: AddKeywords + NotSelf, DynamicWard Const, GrantKeyword)
            make_card(
                "Synth L6 Fast",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    extra: vec![ObjectPredicate::NotSelf],
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec::smallvec![flying],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec::smallvec![trample],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(kw_ab(vigilance)),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                    static_ab(
                        StaticBody::DynamicWardMana {
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    zones: ZoneMask::BATTLEFIELD,
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            generic: Box::new(Amount::Const { n: 2 }),
                        },
                        None,
                    ),
                ],
            ),
            // 2: Slow-path filter using WITH_CYCLING_ABILITY + HasKeywords
            make_card(
                "Synth L6 Slow",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                flags: FilterFlags::WITH_CYCLING_ABILITY,
                                extra: vec![ObjectPredicate::HasKeywords {
                                    keywords: smallvec::smallvec![flying],
                                }],
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::AddKeywords {
                            keywords: smallvec::smallvec![haste],
                            protection_attachment_exemption: None,
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut s_fast = setup_state(Arc::clone(&db), vec![CardDefId(0), CardDefId(1)], vec![]);
        let c_fast = move_card_to_bf(&mut s_fast, PlayerId(0), CardDefId(0));
        let _f_fast = move_card_to_bf(&mut s_fast, PlayerId(0), CardDefId(1));
        let ev_fast = s_fast.evaluate_board().get(c_fast).unwrap();
        assert!(ev_fast.keywords.contains(&flying));
        assert!(ev_fast.keywords.contains(&trample));
        assert!(ev_fast.keywords.contains(&vigilance));
        assert!(!ev_fast.keywords.contains(&haste));

        // Slow path: Synth L6 Slow depends on Synth L6 Fast granting flying!
        let mut s_slow = setup_state(db, vec![CardDefId(0), CardDefId(1), CardDefId(2)], vec![]);
        let c_slow = move_card_to_bf(&mut s_slow, PlayerId(0), CardDefId(0));
        let _s_slow_ent = move_card_to_bf(&mut s_slow, PlayerId(0), CardDefId(2)); // ts earlier
        let _f_slow = move_card_to_bf(&mut s_slow, PlayerId(0), CardDefId(1)); // ts later
        let ev_slow = s_slow.evaluate_board().get(c_slow).unwrap();
        assert!(ev_slow.keywords.contains(&flying));
        assert!(ev_slow.keywords.contains(&trample));
        assert!(ev_slow.keywords.contains(&vigilance));
        assert!(
            ev_slow.keywords.contains(&haste),
            "Layer 6 slow-path must resolve flying -> haste dependency"
        );
    }

    #[test]
    fn direct_card_def_off_bf_and_verify_layer_cache_edge_cases() {
        let tables = InternTables {
            subtypes: vec!["Spacecraft".into(), "Goblin".into()],
            counters: vec![],
            keywords: vec!["devoid".into(), "changeling".into()],
        };
        let spacecraft_sid = SubtypeId(0);
        let goblin_sid = SubtypeId(1);
        let devoid_kw = KeywordId(0);
        let changeling_kw = KeywordId(1);

        let cards = vec![
            // 0: Non-creature Spacecraft with printed P/T (must be None off-BF!)
            make_card(
                "Synth Ship",
                CardTypeMask::ARTIFACT,
                smallvec::smallvec![spacecraft_sid],
                SupertypeMask::empty(),
                ColorMask::empty(),
                Some(PrintedPt::Fixed {
                    power: 6,
                    toughness: 6,
                }),
                vec![],
            ),
            // 1: Devoid + Changeling + Layer 7a */* CDA (* = creatures on BF)
            make_card(
                "Synth Eldrazi",
                CardTypeMask::CREATURE,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::R,
                Some(PrintedPt::Cda {
                    power: None,
                    toughness: None,
                }),
                vec![
                    kw_ab(devoid_kw),
                    kw_ab(changeling_kw),
                    static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer7a,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::SetBasePt {
                                power: Box::new(Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                }),
                                toughness: Box::new(Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                }),
                            },
                            active_in_zone: None,
                        },
                        None,
                    ),
                ],
            ),
            // 2: Split card
            CardDef {
                oracle_id: "oracle_split".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Left // Synth Right".into(),
                layout: CardLayout::Split,
                faces: vec![
                    FaceDef {
                        name: "Synth Left".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::parse("{1}{R}"),
                            types: CardTypeMask::INSTANT,
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
                    },
                    FaceDef {
                        name: "Synth Right".into(),
                        characteristics: Characteristics {
                            mana_cost: ManaCost::parse("{2}{U}"),
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
                    },
                ],
                fully_supported: true,
            },
            // 3: OffBattlefieldCharacteristics with add_chosen_subtype
            make_card(
                "Synth Conspiracy",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::empty(),
                None,
                vec![static_ab(
                    StaticBody::OffBattlefieldCharacteristics {
                        zones: ZoneMask::HAND | ZoneMask::LIBRARY | ZoneMask::GRAVEYARD,
                        filter: None,
                        all_players: false,
                        add_all_creature_types: false,
                        add_chosen_subtype: true,
                        add_types: CardTypeMask::empty(),
                        add_subtypes: SmallVec::new(),
                        add_chosen_color: false,
                        add_colors: ColorMask::empty(),
                    },
                    None,
                )],
            ),
        ];
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![],
        );
        let ship_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let eldrazi_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(1))
            .unwrap();
        let split_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();

        // Put eldrazi in graveyard and split in exile to test multiple zones
        let eldrazi_gy = move_object_raw(
            &mut state,
            eldrazi_id,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let split_ex = move_object_raw(
            &mut state,
            split_id,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        {
            let c1 = state.evaluate_board();
            let ship_ev = c1.get(ship_id).unwrap();
            assert_eq!(
                ship_ev.pt, None,
                "non-creature Spacecraft off-BF must have pt=None"
            );
            let el_ev = c1.get(eldrazi_gy).unwrap();
            assert_eq!(el_ev.colors, ColorMask::empty(), "Devoid off-BF");
            assert!(el_ev.has_all_creature_types, "Changeling off-BF");
            assert_eq!(
                el_ev.pt,
                Some(PowerToughness {
                    power: 0,
                    toughness: 0
                })
            );
            let sp_ev = c1.get(split_ex).unwrap();
            assert_eq!(sp_ev.mana_value, 5);
            assert_eq!(sp_ev.colors, ColorMask::R | ColorMask::U);
        }

        // Put Synth Conspiracy on BF with chosen_subtype = Goblin and verify DESIGNATIONS dirty bit
        let consp = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));
        state.rare_or_default(consp).unwrap().chosen_subtype = Some(goblin_sid);
        state.mark_dirty(DirtyMask::DESIGNATIONS);
        let c2 = state.evaluate_board();
        assert!(c2.reads_mask.contains(DirtyMask::DESIGNATIONS));
        assert!(c2.get(ship_id).unwrap().subtypes.contains(&goblin_sid));
    }

    #[test]
    fn earlier_layer_extra_mask_and_layer6_nested_fixpoint() {
        let tables = InternTables {
            subtypes: vec![
                "Bear".into(),
                "Goblin".into(),
                "Elf".into(),
                "Zombie".into(),
            ],
            counters: vec![],
            keywords: vec!["flying".into()],
        };
        let bear_sid = SubtypeId(0);
        let goblin_sid = SubtypeId(1);
        let elf_sid = SubtypeId(2);
        let zombie_sid = SubtypeId(3);
        let flying_kw = KeywordId(0);

        let cards = vec![
            // 0: Vanilla Bear (1/3)
            make_card(
                "Synth Vanilla Bear",
                CardTypeMask::CREATURE,
                smallvec::smallvec![bear_sid],
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 3,
                }),
                vec![],
            ),
            // 1: Goblin creature (1/1)
            make_card(
                "Synth Goblin",
                CardTypeMask::CREATURE,
                smallvec::smallvec![goblin_sid],
                SupertypeMask::empty(),
                ColorMask::R,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                }),
                vec![],
            ),
            // 2: Elf creature (1/1)
            make_card(
                "Synth Elf",
                CardTypeMask::CREATURE,
                smallvec::smallvec![elf_sid],
                SupertypeMask::empty(),
                ColorMask::G,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                }),
                vec![],
            ),
            // 3: Zombie creature (1/1)
            make_card(
                "Synth Zombie",
                CardTypeMask::CREATURE,
                smallvec::smallvec![zombie_sid],
                SupertypeMask::empty(),
                ColorMask::B,
                Some(PrintedPt::Fixed {
                    power: 1,
                    toughness: 1,
                }),
                vec![],
            ),
            // 4: Enchantment A: Layer 6 grants S_B (grants S_C onto Elf) onto Goblin
            make_card(
                "Synth L6 Hop A",
                CardTypeMask::ENCHANTMENT,
                SmallVec::new(),
                SupertypeMask::empty(),
                ColorMask::W,
                None,
                vec![static_ab(
                    StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                subtypes_any: smallvec::smallvec![goblin_sid],
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(static_ab(
                                StaticBody::Continuous {
                                    layer: Layer::Layer6,
                                    affected: Affected::Filter {
                                        filter: Box::new(ObjectFilter {
                                            zones: ZoneMask::BATTLEFIELD,
                                            subtypes_any: smallvec::smallvec![elf_sid],
                                            ..Default::default()
                                        }),
                                    },
                                    modifier: LayerMod::GrantAbility {
                                        ability: Box::new(static_ab(
                                            StaticBody::Continuous {
                                                layer: Layer::Layer6,
                                                affected: Affected::Filter {
                                                    filter: Box::new(ObjectFilter {
                                                        zones: ZoneMask::BATTLEFIELD,
                                                        subtypes_any: smallvec::smallvec![
                                                            zombie_sid
                                                        ],
                                                        ..Default::default()
                                                    }),
                                                },
                                                modifier: LayerMod::GrantAbility {
                                                    ability: Box::new(kw_ab(flying_kw)),
                                                },
                                                active_in_zone: None,
                                            },
                                            None,
                                        )),
                                    },
                                    active_in_zone: None,
                                },
                                None,
                            )),
                        },
                        active_in_zone: None,
                    },
                    None,
                )],
            ),
        ];

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards,
                decks: vec![],
            })
            .unwrap(),
        );

        // Subtest A: L1 -> L5 -> L6 -> L7c with zero dummy statics in target layers.
        {
            let mut state = setup_state(Arc::clone(&db), vec![CardDefId(0)], vec![]);
            let bear = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
            state.add_continuous_effect(TimedContinuousEffect {
                id: 1,
                source: None,
                controller: PlayerId(0),
                layer: Layer::Layer1,
                affected: ContinuousTarget::Objects(smallvec::smallvec![bear]),
                modifier: LayerMod::GrantAbility {
                    ability: Box::new(static_ab(
                        StaticBody::Continuous {
                            layer: Layer::Layer5,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::GrantAbility {
                                ability: Box::new(static_ab(
                                    StaticBody::Continuous {
                                        layer: Layer::Layer6,
                                        affected: Affected::SelfObj,
                                        modifier: LayerMod::GrantAbility {
                                            ability: Box::new(static_ab(
                                                StaticBody::Continuous {
                                                    layer: Layer::Layer7c,
                                                    affected: Affected::SelfObj,
                                                    modifier: LayerMod::Pt {
                                                        power: Box::new(Amount::Const { n: 2 }),
                                                        toughness: Box::new(Amount::Const { n: 2 }),
                                                    },
                                                    active_in_zone: None,
                                                },
                                                None,
                                            )),
                                        },
                                        active_in_zone: None,
                                    },
                                    None,
                                )),
                            },
                            active_in_zone: None,
                        },
                        None,
                    )),
                },
                duration: Duration::EndOfTurn,
                timestamp: 100,
                expires_turn: None,
                expires_on_player: None,
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });

            let cache = state.evaluate_board();
            assert_eq!(
                cache.get(bear).unwrap().pt,
                Some(PowerToughness {
                    power: 3,
                    toughness: 5
                }),
                "L1 -> L5 -> L6 -> L7c chain must propagate active_layer_mask without dummy statics"
            );
        }

        // Subtest B: L6 -> L6 -> L6 multi-hop fixpoint & CR 613.7a RemoveAllAbilities timestamps.
        {
            let mut state = setup_state(
                db,
                vec![CardDefId(4), CardDefId(1), CardDefId(2), CardDefId(3)],
                vec![],
            );
            let perm_a = move_card_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let perm_b = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let perm_c = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let perm_d = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));

            state.object_mut(perm_a).unwrap().entered_timestamp = 100;
            state.object_mut(perm_b).unwrap().entered_timestamp = 50;
            state.object_mut(perm_c).unwrap().entered_timestamp = 60;
            state.object_mut(perm_d).unwrap().entered_timestamp = 70;
            state.mark_dirty(DirtyMask::ALL);

            assert!(
                state
                    .evaluate_board()
                    .get(perm_d)
                    .unwrap()
                    .keywords
                    .contains(&flying_kw),
                "L6 -> L6 -> L6 3-hop chain must grant flying onto D"
            );

            // RemoveAllAbilities on B at ts = 200 (> max(100, 50) = 100) removes S_B so D loses flying.
            state.add_continuous_effect(TimedContinuousEffect {
                id: 10,
                source: None,
                controller: PlayerId(1),
                layer: Layer::Layer6,
                affected: ContinuousTarget::Objects(smallvec::smallvec![perm_b]),
                modifier: LayerMod::RemoveAllAbilities {
                    keep_mana_abilities: false,
                },
                duration: Duration::EndOfTurn,
                timestamp: 200,
                expires_turn: None,
                expires_on_player: None,
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });
            assert!(
                !state
                    .evaluate_board()
                    .get(perm_d)
                    .unwrap()
                    .keywords
                    .contains(&flying_kw),
                "RemoveAllAbilities (ts=200 > 100) on B must suppress S_B via is_effect_active"
            );

            // Replace with RemoveAllAbilities at ts = 80 (< grant_ts = max(100, 50) = 100):
            // CR 613.7a keeps S_B active, so D regains flying.
            state.retain_continuous_effects(|e| e.id != 10);
            state.add_continuous_effect(TimedContinuousEffect {
                id: 11,
                source: None,
                controller: PlayerId(1),
                layer: Layer::Layer6,
                affected: ContinuousTarget::Objects(smallvec::smallvec![perm_b]),
                modifier: LayerMod::RemoveAllAbilities {
                    keep_mana_abilities: false,
                },
                duration: Duration::EndOfTurn,
                timestamp: 80,
                expires_turn: None,
                expires_on_player: None,
                chosen_color: None,
                chosen_subtype: None,
                chosen_player: None,
                chosen_card_type: None,
                locked_pt: None,
                is_cda: false,
            });
            assert!(
                state
                    .evaluate_board()
                    .get(perm_d)
                    .unwrap()
                    .keywords
                    .contains(&flying_kw),
                "RemoveAllAbilities (ts=80 < grant_ts=100) on B must NOT suppress S_B (CR 613.7a)"
            );
        }
    }

    #[test]
    fn may_have_off_bf_char_grants_tracks_presence_and_evaluates_gy_count_accurately() {
        use crate::zones::{MoveRawOptions, move_object};

        let mut tables = InternTables::default();
        tables.keywords.push("filler".into());
        tables.subtypes.push("Elf".into());
        let elf_sid = SubtypeId(0);

        // Card 0: Synth Land (Land)
        let synth_land = make_card(
            "Synth Land",
            CardTypeMask::LAND,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![],
        );
        // Card 1: Multani-like creature (Layer 7c Pt = Count(BF Land) + Count(GY Land))
        let land_gy_filter = ObjectFilter {
            zones: ZoneMask::GRAVEYARD,
            types_any: CardTypeMask::LAND,
            ..Default::default()
        };
        let land_bf_filter = ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_any: CardTypeMask::LAND,
            ..Default::default()
        };
        let multani_like = make_card(
            "Synth Avatar",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 0,
                toughness: 0,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Sum {
                            args: vec![
                                Amount::Count {
                                    of: Box::new(land_bf_filter.clone()),
                                },
                                Amount::Count {
                                    of: Box::new(land_gy_filter.clone()),
                                },
                            ],
                        }),
                        toughness: Box::new(Amount::Sum {
                            args: vec![
                                Amount::Count {
                                    of: Box::new(land_bf_filter),
                                },
                                Amount::Count {
                                    of: Box::new(land_gy_filter),
                                },
                            ],
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // Card 2: Nexus-like enchantment (OffBattlefieldCharacteristics: graveyard cards gain Elf)
        let nexus_like = make_card(
            "Synth Nexus",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::OffBattlefieldCharacteristics {
                    zones: ZoneMask::GRAVEYARD,
                    filter: None,
                    all_players: true,
                    add_all_creature_types: false,
                    add_chosen_subtype: false,
                    add_types: CardTypeMask::empty(),
                    add_subtypes: smallvec::smallvec![elf_sid],
                    add_chosen_color: false,
                    add_colors: ColorMask::empty(),
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![synth_land, multani_like, nexus_like],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut p0_cards = vec![CardDefId(1), CardDefId(2)];
        p0_cards.extend(std::iter::repeat_n(CardDefId(0), 20));
        let mut state = setup_state(db, p0_cards, vec![CardDefId(0); 10]);

        let avatar = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        for _ in 0..5 {
            let lid = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            let _ = move_object(
                &mut state,
                lid,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let bf_land = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _ = bf_land;

        let cache_no_grant = state.evaluate_board();
        assert!(!cache_no_grant.may_have_off_bf_char_grants);
        assert_eq!(
            cache_no_grant.get(avatar).unwrap().pt,
            Some(PowerToughness {
                power: 6,
                toughness: 6
            })
        );

        // Now move Synth Nexus onto BF -> may_have_off_bf_char_grants flips true and grants Elf in GY
        let nexus = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let _ = nexus;
        let cache_with_grant = state.evaluate_board();
        assert!(cache_with_grant.may_have_off_bf_char_grants);
        assert_eq!(
            cache_with_grant.get(avatar).unwrap().pt,
            Some(PowerToughness {
                power: 6,
                toughness: 6
            })
        );
        let gy_land = state.players()[0].graveyard[0];
        assert!(
            cache_with_grant
                .get(gy_land)
                .unwrap()
                .subtypes
                .contains(&elf_sid)
        );
    }

    #[test]
    fn layer7c_non_const_count_dependency_parity() {
        let tables = InternTables {
            subtypes: vec!["Bear".into()],
            counters: vec!["acorn".into()],
            keywords: vec![],
        };
        let bear_sid = SubtypeId(0);
        let acorn_cid = CounterId(0);

        let land_card = make_card(
            "Synth Forest",
            CardTypeMask::LAND,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![],
        );
        let ench_card = make_card(
            "Synth Enchantment",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            None,
            vec![],
        );
        // Multani-like: +1/+1 for each land on BF + each land in GY
        let multani_like = make_card(
            "Synth Avatar",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 0,
                toughness: 0,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Sum {
                            args: vec![
                                Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::LAND,
                                        controller: Some(RelativeControl::You),
                                        ..Default::default()
                                    }),
                                },
                                Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::GRAVEYARD,
                                        types_any: CardTypeMask::LAND,
                                        controller: Some(RelativeControl::You),
                                        ..Default::default()
                                    }),
                                },
                            ],
                        }),
                        toughness: Box::new(Amount::Sum {
                            args: vec![
                                Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::LAND,
                                        controller: Some(RelativeControl::You),
                                        ..Default::default()
                                    }),
                                },
                                Amount::Count {
                                    of: Box::new(ObjectFilter {
                                        zones: ZoneMask::GRAVEYARD,
                                        types_any: CardTypeMask::LAND,
                                        controller: Some(RelativeControl::You),
                                        ..Default::default()
                                    }),
                                },
                            ],
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // Enchantress-like: +1/+1 per enchantment on BF
        let enchantress_like = make_card(
            "Synth Enchantress",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 0,
                toughness: 2,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::ENCHANTMENT,
                                ..Default::default()
                            }),
                        }),
                        toughness: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::ENCHANTMENT,
                                ..Default::default()
                            }),
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // Chitterspitter-like: Bears you control get +1/+1 per acorn counter on self
        let spitter_like = make_card(
            "Synth Spitter",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: smallvec::smallvec![bear_sid],
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::CountersOn {
                            counter: CounterTarget::Specific(acorn_cid),
                            of: mtg_ir::Ref::SelfObj,
                        }),
                        toughness: Box::new(Amount::CountersOn {
                            counter: CounterTarget::Specific(acorn_cid),
                            of: mtg_ir::Ref::SelfObj,
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // Weaver-like: +1/+1 to all Bears you control
        let weaver_like = make_card(
            "Synth Weaver",
            CardTypeMask::CREATURE,
            smallvec::smallvec![bear_sid],
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            subtypes_any: smallvec::smallvec![bear_sid],
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 1 }),
                        toughness: Box::new(Amount::Const { n: 1 }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // Cultivator-like: creatures you control get +X/+X where X is self's power (forces 7c dependency loop)
        let cultivator_like = make_card(
            "Synth Cultivator",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Stat {
                            of: mtg_ir::Ref::SelfObj,
                            stat: StatKind::Power,
                            lki: false,
                        }),
                        toughness: Box::new(Amount::Stat {
                            of: mtg_ir::Ref::SelfObj,
                            stat: StatKind::Power,
                            lki: false,
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    land_card,
                    ench_card,
                    multani_like,
                    enchantress_like,
                    spitter_like,
                    weaver_like,
                    cultivator_like,
                ],
                decks: vec![],
            })
            .unwrap(),
        );
        let p0_cards = vec![
            CardDefId(0),
            CardDefId(0),
            CardDefId(0),
            CardDefId(1),
            CardDefId(2),
            CardDefId(3),
            CardDefId(4),
            CardDefId(5),
            CardDefId(6),
            CardDefId(0),
        ];
        let mut state = setup_state(db, p0_cards, vec![CardDefId(0); 10]);

        let bf_land = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _ = bf_land;
        let gy_land = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let _ = move_object(
            &mut state,
            gy_land,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let _ench = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let avatar = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let enchantress = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let spitter = move_card_to_bf(&mut state, PlayerId(0), CardDefId(4));
        state.add_counter(spitter, acorn_cid, 2);
        let weaver = move_card_to_bf(&mut state, PlayerId(0), CardDefId(5));
        let cultivator = move_card_to_bf(&mut state, PlayerId(0), CardDefId(6));

        let cache = state.evaluate_board();
        // Cultivator is 1/1 and buffs itself (+1/+1) + all your creatures (+1/+1) -> 2/2.
        assert_eq!(
            cache.get(cultivator).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 2
            })
        );
        // Avatar: 0/0 + (1 BF land + 1 GY land = +2/+2) + (+1/+1 from Cultivator) = 3/3.
        assert_eq!(
            cache.get(avatar).unwrap().pt,
            Some(PowerToughness {
                power: 3,
                toughness: 3
            })
        );
        // Enchantress: 0/2 + (1 ench = +1/+1) + (+1/+1 from Cultivator) = 2/4.
        assert_eq!(
            cache.get(enchantress).unwrap().pt,
            Some(PowerToughness {
                power: 2,
                toughness: 4
            })
        );
        // Weaver (Bear): 2/2 + 2 (Spitter 2 counters) + 1 (Weaver) + 1 (Cultivator) = 6/6.
        assert_eq!(
            cache.get(weaver).unwrap().pt,
            Some(PowerToughness {
                power: 6,
                toughness: 6
            })
        );
    }

    #[test]
    fn layer7c_pt_reader_dependency_orders_anthem_before_power_scaler() {
        let tables = InternTables::default();
        // 0: Vanilla 1/1
        let vanilla = make_card(
            "Synth Token",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![],
        );
        // 1: Power-scaler entered FIRST (earlier timestamp): other creatures get +X/+X where X = Self's power
        let scaler = make_card(
            "Synth Scaler",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            extra: vec![ObjectPredicate::NotSelf],
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Stat {
                            of: mtg_ir::Ref::SelfObj,
                            stat: StatKind::Power,
                            lki: false,
                        }),
                        toughness: Box::new(Amount::Stat {
                            of: mtg_ir::Ref::SelfObj,
                            stat: StatKind::Power,
                            lki: false,
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // 2: Anthem entered SECOND (later timestamp): all creatures you control get +2/+2
        let anthem = make_card(
            "Synth Anthem",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::W,
            None,
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Const { n: 2 }),
                        toughness: Box::new(Amount::Const { n: 2 }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![vanilla, scaler, anthem],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2)],
            vec![CardDefId(0); 10],
        );
        let token = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let scaler_id = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1)); // earlier ts
        let _anthem_id = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2)); // later ts

        let cache = state.evaluate_board();
        // Scaler depends on Anthem (Anthem raises Scaler's power 1 -> 3).
        // Therefore Anthem applies FIRST (Scaler 3/3, Token 3/3), then Scaler applies (+3/+3 to Token -> 6/6)!
        assert_eq!(
            cache.get(scaler_id).unwrap().pt,
            Some(PowerToughness {
                power: 3,
                toughness: 3
            })
        );
        assert_eq!(
            cache.get(token).unwrap().pt,
            Some(PowerToughness {
                power: 6,
                toughness: 6
            })
        );
    }

    #[test]
    fn off_bf_grantor_ids_tracks_multiple_grantors_and_layer6_removal() {
        let tables = InternTables {
            subtypes: vec!["Elf".into(), "Goblin".into()],
            counters: vec![],
            keywords: vec![],
        };
        let elf_sid = SubtypeId(0);
        let goblin_sid = SubtypeId(1);

        let off_bf_target = make_card(
            "Synth Grizzly",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let grantor1 = make_card(
            "Synth Grantor One",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::U,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::OffBattlefieldCharacteristics {
                    zones: ZoneMask::GRAVEYARD,
                    filter: None,
                    all_players: true,
                    add_all_creature_types: false,
                    add_chosen_subtype: false,
                    add_types: CardTypeMask::empty(),
                    add_subtypes: smallvec::smallvec![elf_sid],
                    add_chosen_color: false,
                    add_colors: ColorMask::empty(),
                },
                None,
            )],
        );
        let grantor2 = make_card(
            "Synth Grantor Two",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::R,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::OffBattlefieldCharacteristics {
                    zones: ZoneMask::GRAVEYARD,
                    filter: None,
                    all_players: true,
                    add_all_creature_types: false,
                    add_chosen_subtype: false,
                    add_types: CardTypeMask::empty(),
                    add_subtypes: smallvec::smallvec![goblin_sid],
                    add_chosen_color: false,
                    add_colors: ColorMask::empty(),
                },
                None,
            )],
        );
        let humility_like = make_card(
            "Synth Humility",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::W,
            None,
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![off_bf_target, grantor1, grantor2, humility_like],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![
                CardDefId(0),
                CardDefId(1),
                CardDefId(2),
                CardDefId(3),
                CardDefId(0),
            ],
            vec![CardDefId(0); 10],
        );

        let gy_card = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let gy_card = match move_object(
            &mut state,
            gy_card,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        {
            crate::zones::MoveObjectOutcome::Moved(raw) => raw.new_id.unwrap_or(raw.old_id),
            other => panic!("unexpected MoveObjectOutcome: {other:?}"),
        };

        let g1 = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let g2 = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));

        {
            let cache = state.evaluate_board();
            assert_eq!(cache.off_bf_grantor_ids, vec![g1, g2]);
            let gy_obj = state.object(gy_card).unwrap();
            let c_default = crate::eval::off_battlefield_characteristics(
                &state,
                gy_card,
                gy_obj,
                Zone::Graveyard,
                &EvalContext::new(PlayerId(0)),
            );
            let c_cached = crate::eval::off_battlefield_characteristics(
                &state,
                gy_card,
                gy_obj,
                Zone::Graveyard,
                &EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(cache)),
            );
            assert_eq!(c_default, c_cached);
            assert!(c_cached.subtypes.contains(&elf_sid));
            assert!(c_cached.subtypes.contains(&goblin_sid));
        }

        // Humility removes abilities from both creature grantors -> off_bf_grantor_ids becomes empty
        let _hum = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let cache2 = state.evaluate_board();
        assert!(cache2.off_bf_grantor_ids.is_empty());
        let gy_obj = state.object(gy_card).unwrap();
        let c_default2 = crate::eval::off_battlefield_characteristics(
            &state,
            gy_card,
            gy_obj,
            Zone::Graveyard,
            &EvalContext::new(PlayerId(0)),
        );
        let c_cached2 = crate::eval::off_battlefield_characteristics(
            &state,
            gy_card,
            gy_obj,
            Zone::Graveyard,
            &EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(cache2)),
        );
        assert_eq!(c_default2, c_cached2);
        assert!(!c_cached2.subtypes.contains(&elf_sid));
        assert!(!c_cached2.subtypes.contains(&goblin_sid));
    }

    #[test]
    fn all_off_bf_colorless_layer_work_parity() {
        let tables = InternTables::default();
        // 0: Green card in GY
        let green_card = make_card(
            "Synth Bear",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        // 1: Creature with Layer 7c CDA/continuous counting GREEN cards in your graveyard
        let green_gy_scaler = make_card(
            "Synth Green Counter",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer7c,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::Pt {
                        power: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                colors_any: ColorMask::G,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        }),
                        toughness: Box::new(Amount::Count {
                            of: Box::new(ObjectFilter {
                                zones: ZoneMask::GRAVEYARD,
                                colors_any: ColorMask::G,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        }),
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );
        // 2: Mycosynth Lattice-like (AllOffBattlefieldAndSpellsColorless)
        let lattice_like = make_card(
            "Synth Lattice",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::RuleModifier {
                    rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                    who: PlayerFilter {
                        relation: PlayerRelation::EachPlayer,
                    },
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![green_card, green_gy_scaler, lattice_like],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(0), CardDefId(1), CardDefId(2)],
            vec![CardDefId(0); 10],
        );
        for _ in 0..2 {
            let gid = state.players()[0]
                .hand
                .iter()
                .chain(state.players()[0].library.iter())
                .copied()
                .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
                .unwrap();
            let _ = move_object(
                &mut state,
                gid,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        let scaler = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // Without Lattice: 2 green cards in GY -> 1/1 + 2/2 = 3/3
        {
            let cache = state.evaluate_board();
            assert!(!cache.all_off_battlefield_colorless);
            assert_eq!(
                cache.get(scaler).unwrap().pt,
                Some(PowerToughness {
                    power: 3,
                    toughness: 3
                })
            );
        }

        // With Lattice: GY cards become colorless during LayerWork -> 0 green cards in GY -> 1/1
        let _lattice = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let cache2 = state.evaluate_board();
        assert!(cache2.all_off_battlefield_colorless);
        assert_eq!(
            cache2.get(scaler).unwrap().pt,
            Some(PowerToughness {
                power: 1,
                toughness: 1
            })
        );
    }

    #[test]
    fn layer4_object_state_equipped_dependency_orders_subtype_grant_first() {
        let tables = InternTables {
            subtypes: vec!["Equipment".into(), "Construct".into()],
            counters: vec![],
            keywords: vec![],
        };
        let equipment_sid = SubtypeId(0);
        let construct_sid = SubtypeId(1);

        let bearer_card = make_card(
            "Synth Bearer",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer4,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::SetTypes {
                        types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                        subtypes: smallvec::smallvec![construct_sid],
                    },
                    active_in_zone: None,
                },
                Some(Condition::ObjectState {
                    of: mtg_ir::AttachmentOwnerTarget::SelfObj,
                    tapped: None,
                    attacking: None,
                    blocking: None,
                    monstrous: None,
                    saddled: None,
                    equipped: Some(true),
                    enchanted: None,
                    filter: None,
                }),
            )],
        );
        let relic_card = make_card(
            "Synth Relic",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![bearer_card, relic_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1)], vec![CardDefId(0); 10]);

        let bearer = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let relic = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        attach_raw(&mut state, relic, bearer).unwrap();

        let ts_later = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![relic]),
            modifier: LayerMod::AddTypes {
                types: CardTypeMask::empty(),
                subtypes: smallvec::smallvec![equipment_sid],
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_later,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        let cache = state.evaluate_board();
        let ev = cache.get(bearer).unwrap();
        assert!(ev.types.contains(CardTypeMask::ARTIFACT));
        assert!(ev.subtypes.contains(&construct_sid));
    }

    #[test]
    fn layer5_most_common_color_dependency_orders_color_setter_first() {
        let tables = InternTables::default();
        let watcher_card = make_card(
            "Synth Watcher",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer5,
                    affected: Affected::SelfObj,
                    modifier: LayerMod::AddColors {
                        colors: ColorMask::R,
                    },
                    active_in_zone: None,
                },
                Some(Condition::MostCommonColorAmongPermanents {
                    color: ManaColor::U,
                    allow_tie: false,
                }),
            )],
        );
        let bluewash_card = make_card(
            "Synth Bluewash",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            None,
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer5,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::SetColors {
                        colors: ColorMask::U,
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![watcher_card, bluewash_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0), CardDefId(1)], vec![CardDefId(0); 10]);

        let watcher = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));
        let _bluewash = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));

        let cache = state.evaluate_board();
        assert_eq!(
            cache.get(watcher).unwrap().colors,
            ColorMask::U | ColorMask::R
        );
    }

    #[test]
    fn layer6_exiled_cards_sharing_type_count_dependency_orders_humility_first() {
        let tables = InternTables {
            subtypes: vec![],
            counters: vec![],
            keywords: vec!["ward".into()],
        };
        let exile_relic = make_card(
            "Synth Exile Relic",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![],
        );
        let off_bf_grantor = make_card(
            "Synth OffBf Grantor",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::U,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::OffBattlefieldCharacteristics {
                    zones: ZoneMask::EXILE,
                    filter: None,
                    all_players: true,
                    add_all_creature_types: false,
                    add_chosen_subtype: false,
                    add_types: CardTypeMask::ENCHANTMENT | CardTypeMask::LAND,
                    add_subtypes: SmallVec::new(),
                    add_chosen_color: false,
                    add_colors: ColorMask::empty(),
                },
                None,
            )],
        );
        let ward_emblem = make_card(
            "Synth Ward Emblem",
            CardTypeMask::EMBLEM,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::DynamicWardMana {
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    generic: Box::new(Amount::ExiledCardsSharingTypeCount { owner: None }),
                },
                None,
            )],
        );
        let humility = make_card(
            "Synth Humility",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::W,
            None,
            vec![static_ab(
                StaticBody::Continuous {
                    layer: Layer::Layer6,
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    },
                    modifier: LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                    active_in_zone: None,
                },
                None,
            )],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![exile_relic, off_bf_grantor, ward_emblem, humility],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(
            db,
            vec![CardDefId(0), CardDefId(1), CardDefId(2), CardDefId(3)],
            vec![CardDefId(0); 10],
        );

        let exile_id = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(0))
            .unwrap();
        let _exile_id = move_object(
            &mut state,
            exile_id,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let grantor = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));

        let emblem_src = state.players()[0]
            .hand
            .iter()
            .chain(state.players()[0].library.iter())
            .copied()
            .find(|&id| state.object(id).unwrap().card_def == CardDefId(2))
            .unwrap();
        let emblem = move_object_raw(
            &mut state,
            emblem_src,
            Zone::Command,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let ts_emblem = state.allocate_timestamp();
        {
            let obj = state.object_mut(emblem).unwrap();
            obj.flags.insert(ObjectFlags::IS_EMBLEM);
            obj.entered_timestamp = ts_emblem;
        }
        state.mark_dirty(DirtyMask::TOPOLOGY);

        // Later timestamp than emblem: Humility strips OffBf Grantor before Emblem reads Exile types
        let _hum = move_card_to_bf(&mut state, PlayerId(0), CardDefId(3));

        let cache = state.evaluate_board();
        let extras = cache.extra(grantor).unwrap();
        assert!(
            extras
                .granted_abilities
                .iter()
                .any(|g| matches!(g, GrantedAbilityEntry::DynamicWard { generic: 1, .. })),
            "expected DynamicWard generic: 1 after Humility strips OffBf Grantor; got {:?}",
            extras.granted_abilities
        );
    }

    #[test]
    fn layer4_timed_all_creature_types_depends_on_add_creature_type() {
        let tables = InternTables::default();
        let automaton_card = make_card(
            "Synth Automaton",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![automaton_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0)], vec![CardDefId(0); 10]);
        let automaton = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));

        let ts1 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![automaton]),
            modifier: LayerMod::AllCreatureTypes,
            duration: Duration::EndOfTurn,
            timestamp: ts1,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        let ts2 = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: ContinuousTarget::Objects(smallvec::smallvec![automaton]),
            modifier: LayerMod::AddTypes {
                types: CardTypeMask::CREATURE,
                subtypes: SmallVec::new(),
            },
            duration: Duration::EndOfTurn,
            timestamp: ts2,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        let cache = state.evaluate_board();
        assert!(cache.get(automaton).unwrap().has_all_creature_types);
    }

    #[test]
    fn synth_designations_clean_to_suspected_invalidates_layer_cache() {
        let tables = InternTables {
            subtypes: vec![],
            counters: vec![],
            keywords: vec!["menace".into()],
        };
        let menace_kw = KeywordId(0);
        let creature_card = make_card(
            "Synth Suspectee",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::B,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![creature_card],
                decks: vec![],
            })
            .unwrap(),
        );
        let mut state = setup_state(db, vec![CardDefId(0)], vec![CardDefId(0); 10]);
        let creature = move_card_to_bf(&mut state, PlayerId(0), CardDefId(0));

        // Clean board has zero designation flags; reads_mask must still include DESIGNATIONS.
        let c1 = state.evaluate_board();
        assert!(c1.reads_mask.contains(DirtyMask::DESIGNATIONS));
        assert!(!c1.get(creature).unwrap().keywords.contains(&menace_kw));

        // Flipping SUSPECTED from clean -> true must invalidate LayerCache and grant Menace.
        state
            .object_mut(creature)
            .unwrap()
            .flags
            .insert(ObjectFlags::SUSPECTED);
        state.mark_dirty(DirtyMask::DESIGNATIONS);

        let c2 = state.evaluate_board();
        assert!(c2.get(creature).unwrap().keywords.contains(&menace_kw));
    }

    #[test]
    fn synth_static_presence_flags_all_eight_and_sources() {
        let tables = InternTables::default();
        let vanilla = make_card(
            "Synth Bear",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::G,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 2,
            }),
            vec![],
        );
        let hand_vis = make_card(
            "Synth Spyglass",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::HandVisibility {
                    players: RelativeControl::Opponents,
                },
                None,
            )],
        );
        let top_vis = make_card(
            "Synth Oracle Lens",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::TopOfLibraryVisibility {
                    reveal_to_all: false,
                    all_players: false,
                },
                None,
            )],
        );
        let cmd_zone_perm = make_card(
            "Synth Command Sage",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::LEGENDARY,
            ColorMask::U,
            Some(PrintedPt::Fixed {
                power: 2,
                toughness: 3,
            }),
            vec![static_ab(
                StaticBody::ZonePlayPermission {
                    zone: Zone::Command,
                    allow_lands: false,
                    allow_spells: true,
                    spell_filter: None,
                    self_only: true,
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
                None,
            )],
        );
        let alt_cast = make_card(
            "Synth Alt Emblem",
            CardTypeMask::ENCHANTMENT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::B,
            None,
            vec![static_ab(
                StaticBody::AltCastPermission {
                    mana_cost: ManaCost::default(),
                    use_printed_mana_cost: false,
                    cost_defs: vec![],
                    affected: Affected::SelfObj,
                    allowed_zones: ZoneMask::GRAVEYARD,
                    once_each_turn: false,
                    during_your_turn_only: false,
                    grant_flash: false,
                    any_player: false,
                    dynamic_mana_cost_from_mv: false,
                    max_mv_by_self_counters: None,
                    max_mv_by_controlled_count: None,
                    enters_with_counters: vec![],
                    additional_only: false,
                    additional_per_metric: None,
                    require_zone_permission: false,
                    pay_life_equal_to_mv: false,
                    slot_mode: None,
                },
                None,
            )],
        );
        // CardDefId(5): DFC where front face has no flag and back face has GrantHandForetell.
        let dfc_foretell = CardDef {
            oracle_id: "oracle_Synth_DFC".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth DFC Front".into(),
            layout: CardLayout::Transform,
            faces: vec![
                FaceDef {
                    name: "Synth DFC Front".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
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
                    abilities: vec![],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
                FaceDef {
                    name: "Synth DFC Back".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
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
                    abilities: vec![static_ab(
                        StaticBody::GrantHandForetell {
                            filter: Box::new(ObjectFilter::default()),
                            reduce_generic: 2,
                        },
                        None,
                    )],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                },
            ],
            fully_supported: true,
        };
        let cant_act = make_card(
            "Synth Suppression Totem",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(
                StaticBody::CantActivateAbilities {
                    affected: Affected::Filter {
                        filter: Box::new(ObjectFilter::default()),
                    },
                    allow_mana_abilities: false,
                },
                None,
            )],
        );
        let haste_act = make_card(
            "Synth Chronomancer",
            CardTypeMask::CREATURE,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::R,
            Some(PrintedPt::Fixed {
                power: 1,
                toughness: 1,
            }),
            vec![static_ab(
                StaticBody::ActivateAsThoughHaste {
                    filter: Box::new(ObjectFilter::default()),
                },
                None,
            )],
        );
        let look_exile = make_card(
            "Synth Glass Vault",
            CardTypeMask::ARTIFACT,
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            None,
            vec![static_ab(StaticBody::MayLookAtExiledWithSelf, None)],
        );

        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![
                    vanilla,       // 0
                    hand_vis,      // 1
                    top_vis,       // 2
                    cmd_zone_perm, // 3
                    alt_cast,      // 4
                    dfc_foretell,  // 5
                    cant_act,      // 6
                    haste_act,     // 7
                    look_exile,    // 8
                ],
                decks: vec![],
            })
            .unwrap(),
        );

        let mut d0_cards = vec![
            CardDefId(1),
            CardDefId(2),
            CardDefId(5),
            CardDefId(6),
            CardDefId(7),
            CardDefId(8),
        ];
        while d0_cards.len() < 15 {
            d0_cards.push(CardDefId(0));
        }
        let d0 = DeckSpec {
            commanders: vec![CardDefId(3)],
            mainboard: d0_cards,
            sideboard: vec![],
        };
        let d1 = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 15],
            sideboard: vec![],
        };
        let mut state = GameState::new(db, GameConfig::commander(2), &[d0, d1], 99).unwrap();

        // Non-emblem commander in Command zone contributes ZONE_PLAY_PERMISSION.
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::ZONE_PLAY_PERMISSION
        );

        // Move commander out of command zone onto BF then remove all abilities -> flag clears.
        let cmd_id = state.players()[0].command[0];
        let cmd_bf = move_object_raw(
            &mut state,
            cmd_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::ZONE_PLAY_PERMISSION
        );

        let ts_rem = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![cmd_bf]),
            modifier: LayerMod::RemoveAllAbilities {
                keep_mana_abilities: false,
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_rem,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::empty()
        );

        // Grant ZonePlayPermission via LayerMod::GrantAbility AFTER RemoveAllAbilities -> flag is set!
        let ts_grant = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 2,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer6,
            affected: ContinuousTarget::Objects(smallvec::smallvec![cmd_bf]),
            modifier: LayerMod::GrantAbility {
                ability: Box::new(static_ab(
                    StaticBody::ZonePlayPermission {
                        zone: Zone::Exile,
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
                    None,
                )),
            },
            duration: Duration::EndOfTurn,
            timestamp: ts_grant,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::ZONE_PLAY_PERMISSION
        );

        // Add Emblem (off_bf_statics) with AltCastPermission -> ALT_CAST_PERMISSION set.
        let emblem_obj = crate::state::GameObject::new(
            CardDefId(4),
            PlayerId(0),
            Zone::Command,
            5,
            ObjectFlags::IS_EMBLEM,
        );
        let emblem_id = state.insert_object(emblem_obj, None);
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .command
            .push(emblem_id);
        state.mark_dirty(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
        assert!(
            state
                .evaluate_board()
                .static_presence
                .contains(StaticPresenceFlags::ALT_CAST_PERMISSION)
        );

        // Deploy remaining BF statics + DFC front (no flag) then flip DFC to back face (GRANT_HAND_FORETELL).
        let hand_vis_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let _top_vis_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let dfc_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(5));
        let _cant_act_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(6));
        let _haste_act_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(7));
        let _look_exile_bf = move_card_to_bf(&mut state, PlayerId(0), CardDefId(8));

        let before_transform = state.evaluate_board().static_presence;
        assert!(!before_transform.contains(StaticPresenceFlags::GRANT_HAND_FORETELL));
        assert!(before_transform.contains(StaticPresenceFlags::HAND_VISIBILITY));
        assert!(before_transform.contains(StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY));
        assert!(before_transform.contains(StaticPresenceFlags::CANT_ACTIVATE_ABILITIES));
        assert!(before_transform.contains(StaticPresenceFlags::ACTIVATE_AS_THOUGH_HASTE));
        assert!(before_transform.contains(StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF));

        crate::faces::transform_permanent(&mut state, dfc_bf).unwrap();
        assert_eq!(
            state.evaluate_board().static_presence,
            StaticPresenceFlags::HAND_VISIBILITY
                | StaticPresenceFlags::TOP_OF_LIBRARY_VISIBILITY
                | StaticPresenceFlags::ZONE_PLAY_PERMISSION
                | StaticPresenceFlags::ALT_CAST_PERMISSION
                | StaticPresenceFlags::GRANT_HAND_FORETELL
                | StaticPresenceFlags::CANT_ACTIVATE_ABILITIES
                | StaticPresenceFlags::ACTIVATE_AS_THOUGH_HASTE
                | StaticPresenceFlags::MAY_LOOK_AT_EXILED_WITH_SELF
        );

        // Phasing out hand_vis_bf removes HAND_VISIBILITY.
        assert!(crate::zones::phase_out_permanent(&mut state, hand_vis_bf));
        let after_phase = state.evaluate_board().static_presence;
        assert!(!after_phase.contains(StaticPresenceFlags::HAND_VISIBILITY));
    }
}
