use mtg_ir::{
    AbilityDef, AbilityGrantSource, Affected, Amount, CardTypeMask, ColorMask, Condition,
    CounterId, FilterFlags, KeywordId, Layer, LayerMod, ObjectFilter, ObjectPredicate,
    OpponentComparisonMetric, OpponentMetricKind, PlayerRef, PlayerRelation, RuleMod, StatKind,
    StaticBody, StaticDef, Zone, ZoneMask,
};
use smallvec::SmallVec;

use crate::eval::{
    BoardView, EvalContext, LayerBoardView, evaluate_amount, evaluate_condition, resolve_player_ref,
};
use crate::faces::{
    active_abilities_for_object, active_abilities_with_card_for_object, face_opts_for_object,
    resolve_object_faces,
};
use crate::filter::matches_object_filter;
use crate::id::{ObjectId, PlayerId};
use crate::layers::predicates::{
    ContinuousTarget, GrantedAbilityEntry, StaticGrantOrigin, StaticProvenance, SubtypeTable,
    TimedContinuousEffect, WorkAbilityItem, WorkBoard, WorkStatic,
};
use crate::state::{DirtyMask, GameState, ReplacementGrantOrigin};
use crate::view::PowerToughness;

pub(crate) const LAYER_BIT_1: u16 = 1 << 0;
pub(crate) const LAYER_BIT_2: u16 = 1 << 1;
pub(crate) const LAYER_BIT_3: u16 = 1 << 2;
pub(crate) const LAYER_BIT_4A: u16 = 1 << 3;
pub(crate) const LAYER_BIT_4B: u16 = 1 << 4;
pub(crate) const LAYER_BIT_5A: u16 = 1 << 5;
pub(crate) const LAYER_BIT_5B: u16 = 1 << 6;
pub(crate) const LAYER_BIT_6: u16 = 1 << 7;
pub(crate) const LAYER_BIT_7A: u16 = 1 << 8;
pub(crate) const LAYER_BIT_7B: u16 = 1 << 9;
pub(crate) const LAYER_BIT_7C: u16 = 1 << 10;
pub(crate) const LAYER_BIT_7D: u16 = 1 << 11;

#[inline]
pub(crate) fn is_cda_static_def(st: &StaticDef) -> bool {
    if st.condition.is_some() {
        return false;
    }
    match &st.body {
        StaticBody::Continuous {
            layer,
            affected: Affected::SelfObj,
            modifier,
            active_in_zone: None,
        } => match layer {
            Layer::Layer4 => matches!(modifier, LayerMod::AllCreatureTypes),
            Layer::Layer5 => matches!(modifier, LayerMod::SetColors { .. }),
            Layer::Layer7a => matches!(
                modifier,
                LayerMod::SetBasePt { .. }
                    | LayerMod::SetBasePower { .. }
                    | LayerMod::SetBaseToughness { .. }
            ),
            _ => false,
        },
        _ => false,
    }
}

#[inline]
pub(crate) fn layer_mask_for_static_def(st: &StaticDef, on_battlefield: bool) -> u16 {
    match &st.body {
        StaticBody::Continuous {
            layer,
            modifier,
            active_in_zone,
            ..
        } => {
            if on_battlefield && active_in_zone.is_some_and(|z| z != Zone::Battlefield) {
                return 0;
            }
            if matches!(
                modifier,
                LayerMod::CopyStatsAndSubtypesFromLastExiled { .. }
            ) {
                return LAYER_BIT_3 | LAYER_BIT_4B | LAYER_BIT_7B;
            }
            let is_cda = is_cda_static_def(st);
            match layer {
                Layer::Layer1 => LAYER_BIT_1,
                Layer::Layer2 => LAYER_BIT_2,
                Layer::Layer3 => LAYER_BIT_3,
                Layer::Layer4 => {
                    if is_cda {
                        LAYER_BIT_4A
                    } else {
                        LAYER_BIT_4B
                    }
                }
                Layer::Layer5 => {
                    if is_cda {
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
        StaticBody::DynamicWardMana { .. } => LAYER_BIT_6,
        StaticBody::CommanderPreGameColorChoice => LAYER_BIT_5B,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectOrigin {
    PermanentStatic { source: ObjectId, static_idx: usize },
    EmblemStatic { source: ObjectId, static_idx: usize },
    GraveyardStatic { source: ObjectId, static_idx: usize },
    GrantedStatic { host: ObjectId, item_id: u32 },
    TimedEffect { index: usize },
    KeywordCounter { host: ObjectId, counter: CounterId },
    FlagKeyword { host: ObjectId },
    Reconfigure { host: ObjectId },
}

#[derive(Debug, Clone)]
pub enum LayerAffected<'a> {
    Static(&'a Affected),
    Timed(&'a ContinuousTarget),
    SelfOnly,
}

#[derive(Debug, Clone)]
pub enum LayerOpKind<'a> {
    Mod(&'a LayerMod),
    DynamicWard { generic: &'a Amount },
    GrantKeyword { keyword: KeywordId },
    ReconfigureStripCreature,
}

#[derive(Debug, Clone)]
pub enum EffectPayload<'a> {
    StaticRef(&'a StaticDef),
    Timed(&'a TimedContinuousEffect),
    GrantKeyword { keyword: KeywordId },
    ReconfigureStripCreature,
}

#[derive(Debug, Clone)]
pub struct LayerEffectItem<'a> {
    pub origin: EffectOrigin,
    pub provenance: StaticProvenance,
    pub source_id: Option<ObjectId>,
    pub source_controller: PlayerId,
    pub source_label: Option<&'a str>,
    pub timestamp: u64,
    pub order: usize,
    pub is_cda: bool,
    pub layer: Layer,
    pub payload: EffectPayload<'a>,
    pub started_in_earlier_layer: bool,
    pub locked_targets: Option<SmallVec<[ObjectId; 4]>>,
    pub timed_chosen_color: Option<ColorMask>,
    pub timed_chosen_subtype: Option<mtg_ir::SubtypeId>,
    pub timed_chosen_player: Option<PlayerId>,
    pub timed_chosen_card_type: Option<CardTypeMask>,
    pub timed_locked_pt: Option<PowerToughness>,
}

impl<'a> LayerEffectItem<'a> {
    #[inline]
    pub fn condition(&self) -> Option<&'a Condition> {
        match &self.payload {
            EffectPayload::StaticRef(st) => st.condition.as_deref(),
            EffectPayload::Timed(_)
            | EffectPayload::GrantKeyword { .. }
            | EffectPayload::ReconfigureStripCreature => None,
        }
    }

    #[inline]
    pub fn affected(&self) -> LayerAffected<'a> {
        match &self.payload {
            EffectPayload::StaticRef(st) => static_body_affected(&st.body),
            EffectPayload::Timed(te) => LayerAffected::Timed(&te.affected),
            EffectPayload::GrantKeyword { .. } | EffectPayload::ReconfigureStripCreature => {
                LayerAffected::SelfOnly
            }
        }
    }

    #[inline]
    pub fn op(&self) -> Option<LayerOpKind<'a>> {
        match &self.payload {
            EffectPayload::StaticRef(st) => static_body_op(&st.body),
            EffectPayload::Timed(te) => Some(LayerOpKind::Mod(&te.modifier)),
            EffectPayload::GrantKeyword { keyword } => {
                Some(LayerOpKind::GrantKeyword { keyword: *keyword })
            }
            EffectPayload::ReconfigureStripCreature => Some(LayerOpKind::ReconfigureStripCreature),
        }
    }
}

#[inline]
fn static_body_affected<'a>(body: &'a StaticBody) -> LayerAffected<'a> {
    match body {
        StaticBody::Continuous { affected, .. } | StaticBody::DynamicWardMana { affected, .. } => {
            LayerAffected::Static(affected)
        }
        _ => LayerAffected::SelfOnly,
    }
}

static SET_CHOSEN_COLOR_MOD: LayerMod = LayerMod::SetChosenColor;

#[inline]
fn static_body_op<'a>(body: &'a StaticBody) -> Option<LayerOpKind<'a>> {
    match body {
        StaticBody::Continuous { modifier, .. } => Some(LayerOpKind::Mod(modifier)),
        StaticBody::DynamicWardMana { generic, .. } => Some(LayerOpKind::DynamicWard { generic }),
        StaticBody::CommanderPreGameColorChoice => Some(LayerOpKind::Mod(&SET_CHOSEN_COLOR_MOD)),
        _ => None,
    }
}

type TypeSigEntry = (ObjectId, u16, SmallVec<[mtg_ir::SubtypeId; 2]>);
type PtSigEntry = (ObjectId, Option<i32>, Option<i32>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum SignaturePayload {
    Unit,
    Controller(SmallVec<[(ObjectId, Option<PlayerId>); 4]>),
    Name(SmallVec<[(ObjectId, Option<String>); 4]>),
    Types(SmallVec<[TypeSigEntry; 4]>),
    Colors(SmallVec<[(ObjectId, u8); 4]>),
    Keywords(SmallVec<[(ObjectId, u64); 4]>),
    Pt(SmallVec<[PtSigEntry; 4]>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectSignature {
    active: bool,
    targets: SmallVec<[ObjectId; 8]>,
    payload: SignaturePayload,
}

#[inline]
pub fn current_source_controller(board: &WorkBoard<'_>, item: &LayerEffectItem<'_>) -> PlayerId {
    if let Some(src) = item.source_id
        && let Some(ctrl) = board.effective_controller(src)
    {
        return ctrl;
    }
    item.source_controller
}

pub fn is_effect_active(
    board: &WorkBoard<'_>,
    state: &GameState,
    item: &LayerEffectItem<'_>,
) -> bool {
    match item.origin {
        EffectOrigin::PermanentStatic { source, .. } => {
            let Some(perm) = board.perm(source) else {
                return false;
            };
            !((perm.lost_innate_statics || perm.extras.innate_abilities_removed)
                && !item.started_in_earlier_layer)
        }
        EffectOrigin::GrantedStatic { host, item_id } => {
            let Some(perm) = board.perm(host) else {
                return false;
            };
            perm.items.iter().any(|it| it.id == item_id && !it.removed)
        }
        EffectOrigin::Reconfigure { host } => {
            let Some(perm) = board.perm(host) else {
                return false;
            };
            if perm.lost_innate_statics || perm.extras.innate_abilities_removed {
                return false;
            }
            let Some(obj) = state.object(host) else {
                return false;
            };
            obj.attached_to
                .is_some_and(|target| board.perm(target).is_some())
        }
        EffectOrigin::KeywordCounter { host, counter } => {
            let Some(perm) = board.perm(host) else {
                return false;
            };
            let _ = perm;
            state.counter_count(host, counter) > 0
        }
        EffectOrigin::FlagKeyword { host } => board.perm(host).is_some(),
        EffectOrigin::EmblemStatic { .. }
        | EffectOrigin::GraveyardStatic { .. }
        | EffectOrigin::TimedEffect { .. } => true,
    }
}

fn check_condition(
    board: &WorkBoard<'_>,
    state: &GameState,
    item: &LayerEffectItem<'_>,
    target_id: Option<ObjectId>,
) -> bool {
    let Some(cond) = item.condition() else {
        return true;
    };
    let ctrl = current_source_controller(board, item);
    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
    ctx.source = item.source_id;
    ctx.affected = target_id;
    evaluate_condition(state, cond, &ctx)
}

pub fn resolve_effect_targets(
    board: &WorkBoard<'_>,
    state: &GameState,
    item: &LayerEffectItem<'_>,
) -> SmallVec<[ObjectId; 8]> {
    let mut out: SmallVec<[ObjectId; 8]> = SmallVec::new();
    if !is_effect_active(board, state, item) {
        return out;
    }

    // CR 613.6: If an effect started applying in an earlier layer, lock its affected set to
    // the objects it began applying to in that earlier layer.
    if item.started_in_earlier_layer
        && let Some(locked) = &item.locked_targets
    {
        for &tid in locked {
            if let Some(perm) = board.perm(tid) {
                if matches!(
                    item.layer,
                    Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                ) && !perm.eval.types.contains(CardTypeMask::CREATURE)
                {
                    continue;
                }
                if check_condition(board, state, item, Some(tid)) {
                    out.push(tid);
                }
            }
        }
        return out;
    }

    let ctrl = current_source_controller(board, item);
    let mut base_ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
    base_ctx.source = item.source_id;

    match item.affected() {
        LayerAffected::SelfOnly | LayerAffected::Static(Affected::SelfObj) => {
            if let Some(src) = item.source_id
                && let Some(perm) = board.perm(src)
                && (!matches!(
                    item.layer,
                    Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                ) || perm.eval.types.contains(CardTypeMask::CREATURE))
                && check_condition(board, state, item, Some(src))
            {
                out.push(src);
            }
        }
        LayerAffected::Static(Affected::AttachedTo) => {
            if let Some(src) = item.source_id
                && let Some(obj) = state.object(src)
                && let Some(host_id) = obj.attached_to
                && let Some(host_perm) = board.perm(host_id)
                && (!matches!(
                    item.layer,
                    Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                ) || host_perm.eval.types.contains(CardTypeMask::CREATURE))
                && check_condition(board, state, item, Some(host_id))
            {
                out.push(host_id);
            }
        }
        LayerAffected::Static(Affected::Filter { filter }) => {
            for perm in &board.perms {
                if matches!(
                    item.layer,
                    Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                ) && !perm.eval.types.contains(CardTypeMask::CREATURE)
                {
                    continue;
                }
                if matches_object_filter(state, perm.id, filter, &base_ctx)
                    && check_condition(board, state, item, Some(perm.id))
                {
                    out.push(perm.id);
                }
            }
        }
        LayerAffected::Timed(ContinuousTarget::Objects(ids)) => {
            for &tid in ids {
                if let Some(perm) = board.perm(tid) {
                    if matches!(
                        item.layer,
                        Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                    ) && !perm.eval.types.contains(CardTypeMask::CREATURE)
                    {
                        continue;
                    }
                    if check_condition(board, state, item, Some(tid)) {
                        out.push(tid);
                    }
                }
            }
        }
        LayerAffected::Timed(ContinuousTarget::Filter(filter)) => {
            for perm in &board.perms {
                if matches!(
                    item.layer,
                    Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
                ) && !perm.eval.types.contains(CardTypeMask::CREATURE)
                {
                    continue;
                }
                if matches_object_filter(state, perm.id, filter, &base_ctx)
                    && check_condition(board, state, item, Some(perm.id))
                {
                    out.push(perm.id);
                }
            }
        }
    }
    out
}

fn filter_is_simple_for_layer(layer: Layer, filter: &ObjectFilter) -> bool {
    if !filter
        .mana_value
        .as_ref()
        .is_none_or(|c| matches!(*c.value, Amount::Const { .. }))
    {
        return false;
    }
    let extra_ok = if layer == Layer::Layer6 {
        filter
            .extra
            .iter()
            .all(|p| matches!(p, ObjectPredicate::NotSelf))
    } else {
        filter.extra.is_empty()
    };
    if !extra_ok {
        return false;
    }
    match layer {
        Layer::Layer1 | Layer::Layer2 | Layer::Layer3 | Layer::Layer4 => false,
        Layer::Layer5 => {
            filter.colors_any.is_empty()
                && filter.colors_all.is_empty()
                && filter.colors_none.is_empty()
                && !filter.flags.intersects(
                    mtg_ir::FilterFlags::MULTICOLORED
                        | mtg_ir::FilterFlags::NOT_MULTICOLORED
                        | mtg_ir::FilterFlags::MONOCOLORED
                        | mtg_ir::FilterFlags::NOT_MONOCOLORED
                        | mtg_ir::FilterFlags::IS_COLORED
                        | mtg_ir::FilterFlags::IS_COLORLESS,
                )
        }
        Layer::Layer6 => {
            !filter
                .flags
                .contains(mtg_ir::FilterFlags::WITH_CYCLING_ABILITY)
                && filter
                    .power
                    .as_ref()
                    .is_none_or(|c| matches!(*c.value, Amount::Const { .. }))
                && filter
                    .toughness
                    .as_ref()
                    .is_none_or(|c| matches!(*c.value, Amount::Const { .. }))
        }
        Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d => {
            filter.power.is_none() && filter.toughness.is_none()
        }
    }
}

fn op_is_fast_path_safe(layer: Layer, op: &LayerOpKind<'_>) -> bool {
    match op {
        LayerOpKind::Mod(m) => match m {
            LayerMod::Pt { power, toughness } => {
                matches!(**power, Amount::Const { .. })
                    && matches!(**toughness, Amount::Const { .. })
            }
            LayerMod::SetBasePt { power, toughness } => {
                matches!(**power, Amount::Const { .. })
                    && matches!(**toughness, Amount::Const { .. })
            }
            LayerMod::SetBasePower { power } => matches!(**power, Amount::Const { .. }),
            LayerMod::SetBaseToughness { toughness } => {
                matches!(**toughness, Amount::Const { .. })
            }
            LayerMod::SetColors { .. }
            | LayerMod::AddColors { .. }
            | LayerMod::SetChosenColor
            | LayerMod::AddChosenColor => true,
            LayerMod::AddKeywords { .. }
            | LayerMod::ProtectionFromChosenColor { .. }
            | LayerMod::ProtectionFromChosenPlayer
            | LayerMod::ProtectionFromChosenCardType
            | LayerMod::CopyKeywordsFromExiled { .. } => layer == Layer::Layer6,
            LayerMod::GrantAbility { ability } => {
                layer == Layer::Layer6 && matches!(ability.as_ref(), AbilityDef::Keyword(_))
            }
            LayerMod::SetController { .. }
            | LayerMod::SetName { .. }
            | LayerMod::SwitchPt
            | LayerMod::RemoveKeywords { .. }
            | LayerMod::RemoveAllAbilities { .. }
            | LayerMod::AddTypes { .. }
            | LayerMod::SetTypes { .. }
            | LayerMod::RemoveTypes { .. }
            | LayerMod::SetSubtypes { .. }
            | LayerMod::AllCreatureTypes
            | LayerMod::AddChosenSubtype
            | LayerMod::SetChosenSubtype
            | LayerMod::GrantAbilitiesFromObjects { .. }
            | LayerMod::CopyStatsAndSubtypesFromLastExiled { .. } => false,
        },
        LayerOpKind::GrantKeyword { .. } => true,
        LayerOpKind::DynamicWard { generic } => {
            layer == Layer::Layer6 && matches!(**generic, Amount::Const { .. })
        }
        LayerOpKind::ReconfigureStripCreature => false,
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    struct BoardFieldsMask: u8 {
        const CONTROLLER      = 1 << 0; // Layer 2: eval.controller
        const NAME            = 1 << 1; // Layer 3: eval.name_override
        const TYPES_SUBTYPES  = 1 << 2; // Layer 4: eval.types, subtypes, has_all_creature_types
        const COLORS          = 1 << 3; // Layer 5: eval.colors
        const ABILITIES_GRANT = 1 << 4; // Layer 6: added items/keywords/extras
        const ABILITIES_STATE = 1 << 5; // Layer 4 (CR 305.7) / Layer 6: lost_innate_statics, innate_abilities_removed, removed items, granted statics
        const PT              = 1 << 6; // Layer 7a-7d: eval.base_pt, eval.pt
    }
}

fn writes_for_effect(item: &LayerEffectItem<'_>) -> BoardFieldsMask {
    if item.layer == Layer::Layer1 {
        return BoardFieldsMask::all();
    }
    match item.op() {
        Some(LayerOpKind::Mod(m)) => match m {
            LayerMod::SetController { .. } => BoardFieldsMask::CONTROLLER,
            LayerMod::SetName { .. } => BoardFieldsMask::NAME,
            LayerMod::AllCreatureTypes => BoardFieldsMask::TYPES_SUBTYPES,
            LayerMod::AddTypes { .. }
            | LayerMod::SetTypes { .. }
            | LayerMod::RemoveTypes { .. }
            | LayerMod::SetSubtypes { .. }
            | LayerMod::AddChosenSubtype
            | LayerMod::SetChosenSubtype => {
                BoardFieldsMask::TYPES_SUBTYPES
                    | BoardFieldsMask::ABILITIES_STATE
                    | BoardFieldsMask::ABILITIES_GRANT
            }
            LayerMod::SetColors { .. }
            | LayerMod::AddColors { .. }
            | LayerMod::SetChosenColor
            | LayerMod::AddChosenColor => BoardFieldsMask::COLORS,
            LayerMod::AddKeywords { .. }
            | LayerMod::ProtectionFromChosenColor { .. }
            | LayerMod::ProtectionFromChosenPlayer
            | LayerMod::ProtectionFromChosenCardType => BoardFieldsMask::ABILITIES_GRANT,
            LayerMod::RemoveKeywords { .. }
            | LayerMod::RemoveAllAbilities { .. }
            | LayerMod::GrantAbilitiesFromObjects { .. }
            | LayerMod::CopyKeywordsFromExiled { .. } => {
                BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE
            }
            LayerMod::GrantAbility { ability } => {
                if matches!(ability.as_ref(), AbilityDef::Keyword(_)) {
                    BoardFieldsMask::ABILITIES_GRANT
                } else {
                    BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE
                }
            }
            LayerMod::CopyStatsAndSubtypesFromLastExiled { .. } => match item.layer {
                Layer::Layer3 => BoardFieldsMask::NAME,
                Layer::Layer4 => {
                    BoardFieldsMask::TYPES_SUBTYPES
                        | BoardFieldsMask::ABILITIES_STATE
                        | BoardFieldsMask::ABILITIES_GRANT
                }
                Layer::Layer7b => BoardFieldsMask::PT,
                _ => BoardFieldsMask::empty(),
            },
            LayerMod::SetBasePt { .. }
            | LayerMod::SetBasePower { .. }
            | LayerMod::SetBaseToughness { .. }
            | LayerMod::Pt { .. }
            | LayerMod::SwitchPt => BoardFieldsMask::PT,
        },
        Some(LayerOpKind::GrantKeyword { .. } | LayerOpKind::DynamicWard { .. }) => {
            BoardFieldsMask::ABILITIES_GRANT
        }
        Some(LayerOpKind::ReconfigureStripCreature) => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::ABILITIES_GRANT
        }
        None => BoardFieldsMask::empty(),
    }
}

fn reads_for_player_ref(pr: &PlayerRef) -> BoardFieldsMask {
    match pr {
        PlayerRef::ControllerOf { .. }
        | PlayerRef::Target { .. }
        | PlayerRef::DefendingPlayer
        | PlayerRef::ControllerOrControllerOfPlaneswalker { .. } => BoardFieldsMask::CONTROLLER,
        PlayerRef::You
        | PlayerRef::OwnerOf { .. }
        | PlayerRef::ActivePlayer
        | PlayerRef::EventPlayer
        | PlayerRef::ChosenOpponent
        | PlayerRef::EachLoopPlayer
        | PlayerRef::PlayerToYourRight => BoardFieldsMask::empty(),
    }
}

fn reads_for_predicate(pred: &ObjectPredicate) -> BoardFieldsMask {
    match pred {
        ObjectPredicate::NotSelf
        | ObjectPredicate::PlayerOnly
        | ObjectPredicate::SameOwnerAsSource
        | ObjectPredicate::ControllerDealtCombatDamageBySelfThisTurn
        | ObjectPredicate::HasCounter { .. }
        | ObjectPredicate::LacksCounter { .. }
        | ObjectPredicate::ExiledWithSelf
        | ObjectPredicate::ExiledByAnyPlayer
        | ObjectPredicate::AssimilatedWithSelf
        | ObjectPredicate::CreatedWithSelf
        | ObjectPredicate::PairedWithSource
        | ObjectPredicate::ChosenObject
        | ObjectPredicate::AttachedToRef { .. }
        | ObjectPredicate::TargetsSelf
        | ObjectPredicate::TargetsAttached
        | ObjectPredicate::WithSingleTarget
        | ObjectPredicate::CastFromZones { .. }
        | ObjectPredicate::ManaValueParity { .. }
        | ObjectPredicate::ManaValueEqualsChosenNumber
        | ObjectPredicate::ManaValueLteEventObject
        | ObjectPredicate::ManaValueLtRevealedCostCard
        | ObjectPredicate::ManaValueLteEventAmount
        | ObjectPredicate::ManaValueLteTotalCounters
        | ObjectPredicate::UntappedNonattackingSummoningReady
        | ObjectPredicate::DealtDamageByAttachedThisTurn
        | ObjectPredicate::EnteredGraveyardFromBattlefieldThisTurn
        | ObjectPredicate::PutIntoGraveyardFromNonBattlefieldThisTurn
        | ObjectPredicate::NotYetChosenInStep => BoardFieldsMask::empty(),

        ObjectPredicate::OwnerRef { player } => reads_for_player_ref(player),
        ObjectPredicate::ControllerRef { player } => {
            BoardFieldsMask::CONTROLLER | reads_for_player_ref(player)
        }
        ObjectPredicate::ControllerAndNotOwner { .. }
        | ObjectPredicate::ControlledByEnchantedPlayer
        | ObjectPredicate::ControlledByChosenPlayer
        | ObjectPredicate::ControllerOfTarget { .. }
        | ObjectPredicate::ControllerOfEventObject
        | ObjectPredicate::DifferentControllers => BoardFieldsMask::CONTROLLER,

        ObjectPredicate::TargetsPlayer { player }
        | ObjectPredicate::SingleTargetPlayer { player } => {
            if matches!(
                player.relation,
                PlayerRelation::You | PlayerRelation::Opponent | PlayerRelation::EachOpponent
            ) {
                BoardFieldsMask::CONTROLLER
            } else {
                BoardFieldsMask::empty()
            }
        }

        ObjectPredicate::Named { .. }
        | ObjectPredicate::OfChosenName
        | ObjectPredicate::SameNameAsSource
        | ObjectPredicate::NotSameNameAsSource
        | ObjectPredicate::SameNameAsAffected
        | ObjectPredicate::SameNameAsLinkedExile
        | ObjectPredicate::SameNameAsNontokenPermanent
        | ObjectPredicate::SameNameAsTarget { .. } => BoardFieldsMask::NAME,

        ObjectPredicate::HasKeywords { .. } | ObjectPredicate::LacksKeywords { .. } => {
            BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE
        }

        ObjectPredicate::HasAnotherAuraAttached
        | ObjectPredicate::OfChosenType
        | ObjectPredicate::NotOfChosenType
        | ObjectPredicate::SharesCreatureTypeWithSelf
        | ObjectPredicate::SharesSubtypeWithSacrificed => BoardFieldsMask::TYPES_SUBTYPES,

        ObjectPredicate::SharesTypeWithCommander
        | ObjectPredicate::SharesCardTypeWithExiled
        | ObjectPredicate::SharesCardTypeWithBinding { .. } => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::CONTROLLER
        }

        ObjectPredicate::OfChosenColor
        | ObjectPredicate::SharesColorWithSelf
        | ObjectPredicate::SharesColorWithSpell => BoardFieldsMask::COLORS,

        ObjectPredicate::TargetsFilter { filter }
        | ObjectPredicate::SingleTargetFilter { filter }
        | ObjectPredicate::AbilitySourceFilter { filter } => reads_for_off_bf_filter(filter),

        ObjectPredicate::TargetsYouOrFilter { filter } => {
            BoardFieldsMask::CONTROLLER | reads_for_off_bf_filter(filter)
        }

        ObjectPredicate::ManaValueEqualsSourcePower
        | ObjectPredicate::PowerGreaterThanBase
        | ObjectPredicate::PowerEqualsToughness
        | ObjectPredicate::ToughnessGreaterThanPower
        | ObjectPredicate::PowerLessThanSource
        | ObjectPredicate::PowerLtSelf
        | ObjectPredicate::PowerLteRevealedCostCard => BoardFieldsMask::PT,

        ObjectPredicate::GreatestPowerAmongAll => {
            BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::PT
        }

        ObjectPredicate::PowerOrToughness { cmp }
        | ObjectPredicate::BasePowerOrToughness { cmp } => {
            BoardFieldsMask::PT | reads_for_amount(&cmp.value)
        }

        ObjectPredicate::GreatestStatAmongControlled { stat, .. }
        | ObjectPredicate::LeastStatAmongControlled { stat } => {
            let mut m = BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::CONTROLLER;
            if matches!(stat, StatKind::Power | StatKind::Toughness) {
                m |= BoardFieldsMask::PT;
            }
            m
        }

        ObjectPredicate::AnyOf { filters } => {
            let mut m = BoardFieldsMask::empty();
            for f in filters {
                m |= reads_for_filter(f);
            }
            m
        }
    }
}

fn reads_for_filter(filter: &ObjectFilter) -> BoardFieldsMask {
    let mut m = BoardFieldsMask::empty();

    if !filter.types_any.is_empty()
        || !filter.types_all.is_empty()
        || !filter.types_none.is_empty()
        || !filter.subtypes_any.is_empty()
        || !filter.subtypes_all.is_empty()
        || !filter.subtypes_none.is_empty()
        || filter.flags.intersects(
            FilterFlags::HISTORIC
                | FilterFlags::OUTLAW
                | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN
                | FilterFlags::EQUIPPED
                | FilterFlags::NOT_EQUIPPED
                | FilterFlags::ENCHANTED
                | FilterFlags::NOT_ENCHANTED
                | FilterFlags::ENCHANTED_OR_EQUIPPED
                | FilterFlags::MODIFIED
                | FilterFlags::NOT_MODIFIED,
        )
    {
        m |= BoardFieldsMask::TYPES_SUBTYPES;
    }

    if filter.flags.intersects(
        FilterFlags::EQUIPPED
            | FilterFlags::NOT_EQUIPPED
            | FilterFlags::ENCHANTED
            | FilterFlags::NOT_ENCHANTED
            | FilterFlags::ENCHANTED_OR_EQUIPPED
            | FilterFlags::MODIFIED
            | FilterFlags::NOT_MODIFIED,
    ) {
        m |= BoardFieldsMask::CONTROLLER;
    }

    if !filter.colors_any.is_empty()
        || !filter.colors_all.is_empty()
        || !filter.colors_none.is_empty()
        || filter.flags.intersects(
            FilterFlags::MULTICOLORED
                | FilterFlags::NOT_MULTICOLORED
                | FilterFlags::MONOCOLORED
                | FilterFlags::NOT_MONOCOLORED
                | FilterFlags::IS_COLORED
                | FilterFlags::IS_COLORLESS,
        )
    {
        m |= BoardFieldsMask::COLORS;
    }

    if filter.controller.is_some() {
        m |= BoardFieldsMask::CONTROLLER;
    }

    if filter.flags.contains(FilterFlags::WITH_CYCLING_ABILITY) {
        m |= BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE;
    }

    if let Some(cmp) = &filter.power {
        m |= BoardFieldsMask::PT | reads_for_amount(&cmp.value);
    }
    if let Some(cmp) = &filter.toughness {
        m |= BoardFieldsMask::PT | reads_for_amount(&cmp.value);
    }
    if let Some(cmp) = &filter.mana_value {
        m |= reads_for_amount(&cmp.value);
    }

    for pred in &filter.extra {
        m |= reads_for_predicate(pred);
    }

    if !filter.zones.is_empty() && filter.zones != ZoneMask::BATTLEFIELD {
        m |= BoardFieldsMask::ABILITIES_STATE | BoardFieldsMask::CONTROLLER;
    }

    m
}

#[inline]
fn reads_for_off_bf_filter(filter: &ObjectFilter) -> BoardFieldsMask {
    reads_for_filter(filter) | BoardFieldsMask::ABILITIES_STATE | BoardFieldsMask::CONTROLLER
}

fn reads_for_amount(amt: &Amount) -> BoardFieldsMask {
    match amt {
        Amount::Count { of } => reads_for_filter(of),
        Amount::DistinctNamesCount { of } => BoardFieldsMask::NAME | reads_for_filter(of),
        Amount::TargetsMatchingCount { filter: of } => reads_for_off_bf_filter(of),
        Amount::Stat { stat, lki, .. } => {
            if *lki {
                BoardFieldsMask::empty()
            } else if matches!(stat, StatKind::Power | StatKind::Toughness) {
                BoardFieldsMask::PT
            } else {
                BoardFieldsMask::empty()
            }
        }
        Amount::Devotion { player, .. } => {
            let mut m = BoardFieldsMask::CONTROLLER;
            if let Some(p) = player {
                m |= reads_for_player_ref(p);
            }
            m
        }
        Amount::CardsIn { of, .. }
        | Amount::Life { of }
        | Amount::EnergyCount { player: of }
        | Amount::PlayerCounters { player: of, .. }
        | Amount::PlayerCounterDifference { player: of, .. }
        | Amount::DungeonsCompleted { player: of }
        | Amount::PlayerSpeed { player: of }
        | Amount::FractionalLifeLoss { player: of, .. }
        | Amount::DamageDealtToSelfByPlayerThisTurn { player: of } => reads_for_player_ref(of),

        Amount::UnlockedDoorsCount { player: of }
        | Amount::AttackingCountToPlayer { player: of } => {
            BoardFieldsMask::CONTROLLER | reads_for_player_ref(of)
        }

        Amount::HandSizeDifference { target_player, .. }
        | Amount::TokensCreatedThisTurn {
            player: target_player,
        }
        | Amount::CardsPutIntoGraveyardFromHandOrLibraryThisTurn {
            player: target_player,
        }
        | Amount::UnspentManaTotal {
            player: target_player,
        } => target_player
            .as_ref()
            .map_or(BoardFieldsMask::empty(), reads_for_player_ref),

        Amount::Negate { of } | Amount::Mul { of, .. } | Amount::Half { of, .. } => {
            reads_for_amount(of)
        }
        Amount::Sum { args } | Amount::Min { args } | Amount::Max { args } => {
            let mut m = BoardFieldsMask::empty();
            for a in args {
                m |= reads_for_amount(a);
            }
            m
        }
        Amount::Difference { left, right } => reads_for_amount(left) | reads_for_amount(right),
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            reads_for_condition(condition) | reads_for_amount(then_amt) | reads_for_amount(else_amt)
        }

        Amount::TotalCountersAmong { filter } | Amount::CountersAmong { filter, .. } => {
            reads_for_filter(filter)
        }
        Amount::CountersAmongControlled { filter, player } => {
            BoardFieldsMask::CONTROLLER | reads_for_filter(filter) | reads_for_player_ref(player)
        }

        Amount::DistinctCardTypesInZone { .. } | Amount::ExiledCardsSharingTypeCount { .. } => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::CONTROLLER
        }
        Amount::Domain { player } | Amount::PartyCount { player } => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::CONTROLLER
                | reads_for_player_ref(player)
        }
        Amount::AttachmentsOn { filter, .. } => {
            BoardFieldsMask::TYPES_SUBTYPES
                | filter
                    .as_deref()
                    .map_or(BoardFieldsMask::empty(), reads_for_filter)
        }
        Amount::ColorsAmongPermanents { filter, player } => {
            BoardFieldsMask::COLORS
                | BoardFieldsMask::CONTROLLER
                | reads_for_player_ref(player)
                | filter
                    .as_deref()
                    .map_or(BoardFieldsMask::empty(), reads_for_filter)
        }
        Amount::GreatestStatAmong { filter, stat }
        | Amount::LeastStatAmong { filter, stat }
        | Amount::TotalStatAmong { filter, stat }
        | Amount::GreatestStatAmongControlledAsCast { stat, filter } => {
            let mut m = reads_for_filter(filter) | BoardFieldsMask::CONTROLLER;
            if matches!(stat, StatKind::Power | StatKind::Toughness) {
                m |= BoardFieldsMask::PT;
            }
            m
        }
        Amount::GreatestBaseToughnessControlled { filter } => {
            BoardFieldsMask::PT
                | BoardFieldsMask::CONTROLLER
                | filter
                    .as_deref()
                    .map_or(BoardFieldsMask::empty(), reads_for_filter)
        }
        Amount::GreatestControlledCount { filter }
        | Amount::OpponentsControllingMoreCount { filter }
        | Amount::OpponentsControllingFilterCount { filter } => {
            reads_for_filter(filter) | BoardFieldsMask::CONTROLLER
        }
        Amount::ColorsOfObject { .. } | Amount::TargetColorsCount => BoardFieldsMask::COLORS,
        Amount::DistinctColorsAmongControlled => {
            BoardFieldsMask::COLORS | BoardFieldsMask::CONTROLLER
        }
        Amount::DistinctManaValuesInGraveyard { filter } => filter
            .as_deref()
            .map_or(BoardFieldsMask::empty(), reads_for_off_bf_filter),
        Amount::ChosenPlayerZoneCount { zone, filter } => filter.as_deref().map_or(
            BoardFieldsMask::empty(),
            if *zone == Zone::Battlefield {
                reads_for_filter
            } else {
                reads_for_off_bf_filter
            },
        ),
        Amount::DistinctCreatureTypesControlled => {
            BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::CONTROLLER
        }
        Amount::DistinctLandNamesControlled => {
            BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::NAME | BoardFieldsMask::CONTROLLER
        }
        Amount::DistinctNamesControlled { filter } => {
            BoardFieldsMask::NAME
                | BoardFieldsMask::CONTROLLER
                | filter
                    .as_deref()
                    .map_or(BoardFieldsMask::empty(), reads_for_filter)
        }
        Amount::DistinctPowersControlled => {
            BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::PT | BoardFieldsMask::CONTROLLER
        }
        Amount::LandsOpponentControlsProducingColorless => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::CONTROLLER
                | BoardFieldsMask::ABILITIES_GRANT
                | BoardFieldsMask::ABILITIES_STATE
        }
        Amount::SameNameInGraveyardAsSpell => BoardFieldsMask::NAME,
        Amount::TargetPower => BoardFieldsMask::PT,
        Amount::RingBearerPower { player } => {
            BoardFieldsMask::PT
                | player
                    .as_ref()
                    .map_or(BoardFieldsMask::empty(), reads_for_player_ref)
        }
        Amount::SpellsCastThisTurnCount { filter, .. } => filter
            .as_deref()
            .map_or(BoardFieldsMask::empty(), reads_for_filter),

        Amount::Const { .. }
        | Amount::X
        | Amount::AnyNumber
        | Amount::ResultCount { .. }
        | Amount::EventAmount
        | Amount::CommanderCastCount
        | Amount::KickedCount { .. }
        | Amount::StormCount
        | Amount::GravestormCount
        | Amount::LkiCounterCount { .. }
        | Amount::LkiTotalCounters { .. }
        | Amount::CountersRemovedThisWay
        | Amount::DistinctCounterKindsOn { .. }
        | Amount::CountersAmongPlayersAndPermanents { .. }
        | Amount::CountersOn { .. }
        | Amount::TappedCostPower
        | Amount::ColorsOfManaSpentToCast
        | Amount::ManaSpentToCast
        | Amount::LastRollResult
        | Amount::CoinHeadsCount
        | Amount::DevouredCount
        | Amount::OpponentCount
        | Amount::ExcessDamage
        | Amount::LastEffectAmount
        | Amount::TargetsBeyondFirst
        | Amount::LifeGainedThisTurn
        | Amount::StartingLifeDifference
        | Amount::CreaturesAttackedWithThisTurn
        | Amount::CreaturesAttackedTotalThisTurn
        | Amount::OpponentsDealtDamageThisTurn
        | Amount::CommanderColorIdentityCount
        | Amount::TargetsCount
        | Amount::TargetCounterCount { .. }
        | Amount::TargetsTotalManaValue
        | Amount::TotalManaValueOfTargetSpec { .. }
        | Amount::CostPaidCount { .. }
        | Amount::SacrificedCostStat { .. }
        | Amount::RevealedCostStat { .. }
        | Amount::OpponentsWithLifeLessThanHalfStarting
        | Amount::CountedThisWay { .. }
        | Amount::CreaturesDiedThisTurn { .. }
        | Amount::NontokenCreaturesDiedThisTurn { .. }
        | Amount::OpponentsLifeLostThisTurn
        | Amount::CardsDiscardedThisTurn
        | Amount::OpponentsWithHandMaxCount { .. }
        | Amount::OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn { .. }
        | Amount::PlayersLostLifeThisTurnCount
        | Amount::SacrificedPermsThisTurn
        | Amount::GreatestOpponentCardsDrawnThisTurn
        | Amount::TimesAttackedThisTurn { .. }
        | Amount::DamageDealtToOpponentsThisTurn => BoardFieldsMask::empty(),
    }
}

fn reads_for_condition(cond: &Condition) -> BoardFieldsMask {
    match cond {
        Condition::Compare { left, right, .. } => reads_for_amount(left) | reads_for_amount(right),
        Condition::And { of } | Condition::Or { of } => {
            let mut m = BoardFieldsMask::empty();
            for c in of {
                m |= reads_for_condition(c);
            }
            m
        }
        Condition::Not { of } => reads_for_condition(of),

        Condition::GraveyardAbove { filter, .. }
        | Condition::MatchesFilter { filter, .. }
        | Condition::ControlsFilter { filter } => reads_for_filter(filter),

        Condition::EventObjectMatchedLki { filter }
        | Condition::NoMatchingCardsInHand { filter }
        | Condition::AbilityTargetsFilter { filter }
        | Condition::EventSpellMatchesFilter { filter }
        | Condition::EventObjectMatchesOrSubtype { filter }
        | Condition::ReturnedFilterToHandThisWay { filter } => reads_for_off_bf_filter(filter),

        Condition::OnlyNamedOrNoMatchingInHand { filter } => {
            reads_for_off_bf_filter(filter) | BoardFieldsMask::NAME
        }

        Condition::PlayerControlsFilter { player, filter } => {
            reads_for_player_ref(player) | reads_for_filter(filter)
        }
        Condition::ObjectState {
            equipped,
            enchanted,
            filter,
            ..
        } => {
            let mut m = filter
                .as_deref()
                .map_or(BoardFieldsMask::empty(), reads_for_filter);
            if equipped.is_some() || enchanted.is_some() {
                m |= BoardFieldsMask::TYPES_SUBTYPES;
            }
            m
        }
        Condition::AttachedPermanentMatches { colors, .. } => {
            if colors.is_empty() {
                BoardFieldsMask::empty()
            } else {
                BoardFieldsMask::COLORS
            }
        }
        Condition::GainedLifeThisTurn { min } => min
            .as_deref()
            .map_or(BoardFieldsMask::empty(), reads_for_amount),

        Condition::IsTurnOf { player }
        | Condition::PlayerLostLifeThisTurn { player }
        | Condition::OpponentHasMoreCardsInHandThan { who: player }
        | Condition::WasDealtCombatDamageBySelfNameThisGame { player } => {
            reads_for_player_ref(player)
        }
        Condition::IsMonarch { player, .. } => player
            .as_ref()
            .map_or(BoardFieldsMask::empty(), reads_for_player_ref),
        Condition::PlayerHasFewerHandThan { target_player, .. } => target_player
            .as_ref()
            .map_or(BoardFieldsMask::empty(), reads_for_player_ref),

        Condition::DistinctControlledCreaturePowers { .. }
        | Condition::ManaSpentGreaterThanSelfPower
        | Condition::TrainingGreaterAttacker
        | Condition::TotalPowerControlled { .. }
        | Condition::AttackingTotalPowerAtLeast { .. }
        | Condition::EnchantedCreaturePowerAtLeast { .. }
        | Condition::EvolveCompare
        | Condition::EventObjectPowerOrToughnessGreaterThanSelf
        | Condition::EventObjectGreatestPowerAmongAll => {
            BoardFieldsMask::PT | BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::CONTROLLER
        }

        Condition::ColorsAmongControlledPermanents { .. } => {
            BoardFieldsMask::COLORS | BoardFieldsMask::CONTROLLER
        }
        Condition::MostCommonColorAmongPermanents { .. } => BoardFieldsMask::COLORS,
        Condition::OpponentComparison { metric } => match metric {
            OpponentComparisonMetric::Lands | OpponentComparisonMetric::Creatures => {
                BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::CONTROLLER
            }
            OpponentComparisonMetric::Life | OpponentComparisonMetric::Hand => {
                BoardFieldsMask::empty()
            }
        },
        Condition::OpponentControlsAtLeastNMore { filter, .. }
        | Condition::EventPlayerControlsMoreThanYou { filter }
        | Condition::EventPlayerControlsMoreThanEachOther { filter }
        | Condition::YouControlGreatestManaValueFilter { filter }
        | Condition::YourTeamControlsOtherFilter { filter } => {
            reads_for_filter(filter) | BoardFieldsMask::CONTROLLER
        }
        Condition::CardsPutIntoGraveyardFromAnywhereThisTurn { filter, .. }
        | Condition::SpellsCastThisTurnAtLeast { filter, .. }
        | Condition::NthMatchingSpellThisTurn { filter, .. }
        | Condition::AttackingCount { filter, .. }
        | Condition::OpponentCastSpellsAtLeast { filter, .. }
        | Condition::OpponentHadPermEnteredThisTurn { filter, .. }
        | Condition::OpponentCardsPutIntoGraveyardAtLeastThisTurn { filter, .. } => filter
            .as_deref()
            .map_or(BoardFieldsMask::empty(), reads_for_filter),
        Condition::TargetsStackSpell { filter } => filter
            .as_deref()
            .map_or(BoardFieldsMask::empty(), reads_for_off_bf_filter),
        Condition::SacrificedFilterThisTurn { filter, .. }
        | Condition::ZoneChangeFromToThisTurn { filter, .. }
        | Condition::CountersAmongFilterAtLeast { filter, .. }
        | Condition::EnteredUnderYourControlThisTurn { filter, .. } => reads_for_filter(filter),

        Condition::AllLandsAreSubtype { .. } | Condition::EventWasAttachedEnchantedOrEquipped => {
            BoardFieldsMask::TYPES_SUBTYPES
        }
        Condition::EventObjectWasNotSubtype { .. }
        | Condition::EventSharesCardTypeWithLinkedExile => {
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::CONTROLLER
        }

        Condition::ControlledTargetAtResolution { .. }
        | Condition::ControlAttackingLegendary
        | Condition::ControlledByOtherThanOwner
        | Condition::AttackingYouCount { .. }
        | Condition::EventMinAttackersToYouOrPlaneswalkers { .. }
        | Condition::EventAttackerHasAnotherUnattackedOpponent => BoardFieldsMask::CONTROLLER,

        Condition::NoSameNameControlledOrInGraveyard
        | Condition::SameNameLandsControlledAtLeast { .. } => {
            BoardFieldsMask::NAME | BoardFieldsMask::TYPES_SUBTYPES | BoardFieldsMask::CONTROLLER
        }
        Condition::EventSpellHasKeyword { .. } => {
            BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE
        }
        Condition::AnyOpponentMetric { metric, .. } => {
            let _ = OpponentMetricKind::Life;
            let _ = metric;
            BoardFieldsMask::empty()
        }

        Condition::IsYourTurn
        | Condition::NotYourTurn
        | Condition::NotStartingPlayer
        | Condition::NotSpellControllerTurn
        | Condition::NotEventPlayerTurn
        | Condition::IsYourMainPhase
        | Condition::ControlCommander
        | Condition::Kicked { .. }
        | Condition::Evoked
        | Condition::Escaped
        | Condition::Foretold
        | Condition::Blitzed
        | Condition::Saddled
        | Condition::ExertedThisTurn
        | Condition::Monstrous
        | Condition::GiftPromised
        | Condition::TributePaid
        | Condition::IsPaired
        | Condition::HasEnduringStory
        | Condition::IsHarnessed
        | Condition::IsSuspended
        | Condition::IsRenowned
        | Condition::SelfPrepared
        | Condition::IsImpending
        | Condition::IsDay
        | Condition::IsNight
        | Condition::HasCompletedDungeon
        | Condition::HasInitiative
        | Condition::HasCitysBlessing
        | Condition::HasMaxSpeed
        | Condition::CreatureDiedThisTurn
        | Condition::CreatureDiedUnderYourControlThisTurn
        | Condition::AttackedThisTurn
        | Condition::AttackedWithCommanderThisTurn
        | Condition::PermanentLeftBattlefieldThisTurn
        | Condition::OpponentGainedLifeThisTurn
        | Condition::YourTeamGainedLifeThisTurn
        | Condition::WasCast { .. }
        | Condition::OpponentCount { .. }
        | Condition::AnyPlayerLife { .. }
        | Condition::CameUnderControlSinceLastUpkeep
        | Condition::NinjutsuWindow
        | Condition::LkiHasNoCounter { .. }
        | Condition::HadCounterOnIt { .. }
        | Condition::LkiToughnessLessThan { .. }
        | Condition::OpponentLostLifeThisTurn
        | Condition::YouLostLifeThisTurn
        | Condition::PlayerLostLifeAtLeastThisTurn { .. }
        | Condition::PlayerLostLifeLastTurn
        | Condition::DescendedThisTurn
        | Condition::ManaColorSpentToCast { .. }
        | Condition::SameColorManaSpentAtLeast { .. }
        | Condition::TreasureManaSpentToCast
        | Condition::MoreCardsInHandThanEachOpponent
        | Condition::YouHaveMostLife
        | Condition::YourLifeLessThanStarting
        | Condition::LifeAtLeastAboveStarting { .. }
        | Condition::LifeCompareHalfStarting { .. }
        | Condition::ClassLevelIs { .. }
        | Condition::ClassLevelAtLeast { .. }
        | Condition::CaseSolved
        | Condition::CaseUnsolved
        | Condition::BoastAttackedThisTurn
        | Condition::ExhaustUnused { .. }
        | Condition::DethroneMostLife
        | Condition::CreaturesAttackedCountThisTurn { .. }
        | Condition::SourcesDealtDamageThisTurn { .. }
        | Condition::SelfInZone { .. }
        | Condition::SpellsCastLastTurn { .. }
        | Condition::OpponentPoisonAtLeast { .. }
        | Condition::NonlandEnteredThisTurn { .. }
        | Condition::VoidThisTurn
        | Condition::CardsDrawnThisTurnAtLeast { .. }
        | Condition::EnteredWithXAtLeast { .. }
        | Condition::FirstCombatPhaseOfTurn
        | Condition::EventPlayerHandCount { .. }
        | Condition::CardLeftYourGraveyardThisTurn
        | Condition::CommittedCrimeThisTurn
        | Condition::AnchorWordChosen { .. }
        | Condition::YouWereAttackedThisCombat
        | Condition::DiedThisWay { .. }
        | Condition::NoSpellCastFromHandThisTurn
        | Condition::NoSpellCastThisTurn
        | Condition::ChoseOtherRingBearer
        | Condition::PaidNamedAltCost { .. }
        | Condition::AltCostPaid { .. }
        | Condition::AdditionalCostPaid { .. }
        | Condition::NotBeingDeclaredAsAttacker
        | Condition::SelfDealtDamageToOpponentThisTurn
        | Condition::DamageDealtToSelfThisTurnAtLeast { .. }
        | Condition::EventObjectEnteredThisTurn
        | Condition::SelfEnteredThisTurn
        | Condition::DistinctManaValuesInGraveyardAtLeast { .. }
        | Condition::AnyGraveyardAtLeast { .. }
        | Condition::AbilityActivatedCountThisTurn { .. }
        | Condition::IsAttackingAlone { .. }
        | Condition::HasntDealtDamageYet { .. }
        | Condition::YourTurnsTakenAtLeast { .. }
        | Condition::YouDiscardedThisTurn
        | Condition::DiscardedCardThisTurn { .. }
        | Condition::SpellWasBargained
        | Condition::OpponentDrewCardsAtLeastThisTurn { .. }
        | Condition::OpponentSearchedLibraryThisTurn
        | Condition::YourCreatureSpellCounteredByOpponentThisTurn
        | Condition::YourNoncreatureDestroyedByOpponentThisTurn
        | Condition::DealtDamageByCreaturesAtLeastThisTurn { .. }
        | Condition::EventAbilityIsNotMana
        | Condition::EventAbilityLifePaid
        | Condition::EventSpellWasKicked
        | Condition::EventSpellNoColoredManaSpent
        | Condition::EventSpellManaSpentLessThanMv
        | Condition::EventEnteredOrCastFromZone { .. }
        | Condition::EventEnteredOrCastFromGraveyardAny
        | Condition::EventCrewCountEquals { .. }
        | Condition::EventDamageDealtBySubtype { .. }
        | Condition::EventDamagedTargetDealtExcessDamageThisTurn
        | Condition::EventAttackingPlayerWithMostLife
        | Condition::PutCounterOnCreatureThisTurn { .. }
        | Condition::NoCreaturesAttackedThisTurn
        | Condition::CastCreatureAndNoncreatureThisTurn
        | Condition::PlayersLostGameAtLeast { .. }
        | Condition::SelfAttackedThisCombat
        | Condition::SelfDidntAttackThisTurn
        | Condition::SelfHaventAddedManaThisTurn
        | Condition::CounterPutOnSelfThisTurn
        | Condition::PlayerHadAnotherLandEnterThisTurn
        | Condition::HasCardsExiledWithSelf
        | Condition::DidntActivateLoyaltyThisTurn
        | Condition::NotPutOntoBattlefieldByThisAbility
        | Condition::AnyAttackedPlayerWithMostLife
        | Condition::PlayerDidntDiscardNonlandThisWay
        | Condition::PlayerDiscardedHandThisWay
        | Condition::CreatedTokenThisTurn
        | Condition::PlayedLandThisTurn
        | Condition::SourceHasCounter { .. }
        | Condition::DistinctCardTypesAmongSacrificedAtLeast { .. }
        | Condition::CreatureCardMilledThisWay
        | Condition::LandCardMilledThisWay
        | Condition::DiscardedNonlandThisWay
        | Condition::DiscardedCreatureThisWay
        | Condition::DiscardedNoncreatureThisWay
        | Condition::PlayerHasFewerCountersThan { .. }
        | Condition::TargetHasFewerCountersThan { .. }
        | Condition::SpellCastFromZone { .. }
        | Condition::NotDrawnCardsThisWay
        | Condition::ExiledCountAtLeast { .. }
        | Condition::DidRevealThisWay
        | Condition::RingBearerAndTemptedAtLeast { .. }
        | Condition::WonClashThisWay
        | Condition::SacrificedNonlandThisWay
        | Condition::DidntPutOntoBattlefieldThisWay
        | Condition::CastTriggerCreatureSpell
        | Condition::AllPlayersCantCastCheck
        | Condition::SelfRemovedFromCombatOrAttached
        | Condition::DidLastOptionalStep
        | Condition::SourceExiledThisWay
        | Condition::FirstSpellThisTurnWithManaFrom { .. } => BoardFieldsMask::empty(),
    }
}

fn reads_for_effect(item: &LayerEffectItem<'_>) -> BoardFieldsMask {
    let mut m = BoardFieldsMask::empty();

    if matches!(
        item.origin,
        EffectOrigin::PermanentStatic { .. }
            | EffectOrigin::GrantedStatic { .. }
            | EffectOrigin::Reconfigure { .. }
    ) {
        m |= BoardFieldsMask::ABILITIES_STATE;
    }

    if item.source_id.is_some() {
        m |= BoardFieldsMask::CONTROLLER;
    }

    if matches!(
        item.layer,
        Layer::Layer7a | Layer::Layer7b | Layer::Layer7c | Layer::Layer7d
    ) {
        m |= BoardFieldsMask::TYPES_SUBTYPES;
    }

    match item.affected() {
        LayerAffected::Static(Affected::Filter { filter }) => {
            m |= reads_for_filter(filter);
        }
        LayerAffected::Timed(ContinuousTarget::Filter(filter)) => {
            m |= reads_for_filter(filter);
        }
        LayerAffected::SelfOnly
        | LayerAffected::Static(Affected::SelfObj | Affected::AttachedTo)
        | LayerAffected::Timed(ContinuousTarget::Objects(_)) => {}
    }

    if let Some(cond) = item.condition() {
        m |= reads_for_condition(cond);
    }

    if let Some(op) = item.op() {
        match op {
            LayerOpKind::Mod(lm) => match lm {
                LayerMod::SetController { controller } => {
                    m |= BoardFieldsMask::CONTROLLER | reads_for_player_ref(controller);
                }
                LayerMod::Pt { power, toughness } => {
                    if item.timed_locked_pt.is_none() {
                        m |= reads_for_amount(power) | reads_for_amount(toughness);
                    }
                }
                LayerMod::SetBasePt { power, toughness } => {
                    m |= reads_for_amount(power) | reads_for_amount(toughness);
                }
                LayerMod::SetBasePower { power } => {
                    m |= reads_for_amount(power);
                }
                LayerMod::SetBaseToughness { toughness } => {
                    m |= reads_for_amount(toughness);
                }
                LayerMod::GrantAbilitiesFromObjects { from, .. } => {
                    m |= BoardFieldsMask::ABILITIES_GRANT | BoardFieldsMask::ABILITIES_STATE;
                    match from {
                        AbilityGrantSource::ExiledWithSelf { filter } => {
                            m |= reads_for_off_bf_filter(filter);
                        }
                        AbilityGrantSource::ExiledCardsWithCounter { filter, .. } => {
                            if let Some(f) = filter.as_deref() {
                                m |= reads_for_off_bf_filter(f);
                            }
                        }
                        AbilityGrantSource::Battlefield { filter } => {
                            m |= reads_for_filter(filter);
                        }
                    }
                }
                LayerMod::AllCreatureTypes
                | LayerMod::AddTypes { .. }
                | LayerMod::SetSubtypes { .. }
                | LayerMod::AddChosenSubtype
                | LayerMod::SetChosenSubtype => {
                    m |= BoardFieldsMask::TYPES_SUBTYPES;
                }
                LayerMod::CopyStatsAndSubtypesFromLastExiled {
                    include_subtypes, ..
                } => {
                    if item.layer == Layer::Layer4 && *include_subtypes {
                        m |= BoardFieldsMask::TYPES_SUBTYPES;
                    }
                }
                LayerMod::SetName { .. }
                | LayerMod::SetTypes { .. }
                | LayerMod::RemoveTypes { .. }
                | LayerMod::SetColors { .. }
                | LayerMod::AddColors { .. }
                | LayerMod::SetChosenColor
                | LayerMod::AddChosenColor
                | LayerMod::AddKeywords { .. }
                | LayerMod::RemoveKeywords { .. }
                | LayerMod::RemoveAllAbilities { .. }
                | LayerMod::GrantAbility { .. }
                | LayerMod::ProtectionFromChosenColor { .. }
                | LayerMod::ProtectionFromChosenPlayer
                | LayerMod::ProtectionFromChosenCardType
                | LayerMod::CopyKeywordsFromExiled { .. }
                | LayerMod::SwitchPt => {}
            },
            LayerOpKind::DynamicWard { generic } => {
                m |= reads_for_amount(generic);
            }
            LayerOpKind::GrantKeyword { .. } | LayerOpKind::ReconfigureStripCreature => {}
        }
    }

    m
}

/// Fast path (`CR 613.8` optimization): skips O(N²) probing when `<= 1` effect or effects
/// in this sublayer are provably non-interacting.
pub fn can_use_timestamp_fast_path(layer: Layer, effects: &[LayerEffectItem<'_>]) -> bool {
    if effects.len() <= 1 {
        return true;
    }
    if layer != Layer::Layer1 {
        let reads: SmallVec<[BoardFieldsMask; 16]> = effects.iter().map(reads_for_effect).collect();
        let writes: SmallVec<[BoardFieldsMask; 16]> =
            effects.iter().map(writes_for_effect).collect();
        let mut non_interacting = true;
        'pair_check: for i in 0..effects.len() {
            let wi = writes[i];
            if wi.is_empty() {
                continue;
            }
            for j in 0..effects.len() {
                if i != j && effects[i].is_cda == effects[j].is_cda && reads[j].intersects(wi) {
                    non_interacting = false;
                    break 'pair_check;
                }
            }
        }
        if non_interacting {
            return true;
        }
    }
    if matches!(
        layer,
        Layer::Layer1 | Layer::Layer2 | Layer::Layer3 | Layer::Layer4 | Layer::Layer7d
    ) {
        return false;
    }
    for e in effects {
        let Some(op) = e.op() else {
            return false;
        };
        if e.condition().is_some() || !op_is_fast_path_safe(layer, &op) {
            return false;
        }
        match e.affected() {
            LayerAffected::SelfOnly
            | LayerAffected::Static(Affected::SelfObj | Affected::AttachedTo)
            | LayerAffected::Timed(ContinuousTarget::Objects(_)) => {}
            LayerAffected::Static(Affected::Filter { filter }) => {
                if !filter_is_simple_for_layer(layer, filter) {
                    return false;
                }
            }
            LayerAffected::Timed(ContinuousTarget::Filter(filter)) => {
                if !filter_is_simple_for_layer(layer, filter) {
                    return false;
                }
            }
        }
    }
    true
}

fn effect_signature(
    board: &WorkBoard<'_>,
    state: &GameState,
    item: &LayerEffectItem<'_>,
) -> EffectSignature {
    let active = is_effect_active(board, state, item);
    let Some(op) = item.op() else {
        return EffectSignature {
            active: false,
            targets: SmallVec::new(),
            payload: SignaturePayload::Unit,
        };
    };
    if !active {
        return EffectSignature {
            active: false,
            targets: SmallVec::new(),
            payload: SignaturePayload::Unit,
        };
    }
    let targets = resolve_effect_targets(board, state, item);
    let ctrl = current_source_controller(board, item);
    let payload = match &op {
        LayerOpKind::Mod(m) => match m {
            LayerMod::SetController { controller } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    v.push((tid, resolve_player_ref(state, controller, &ctx)));
                }
                SignaturePayload::Controller(v)
            }
            LayerMod::SetName { name } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, Some(name.clone())));
                }
                SignaturePayload::Name(v)
            }
            LayerMod::AddTypes { types, subtypes } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let is_land_bit = if board
                        .perm(tid)
                        .is_some_and(|p| p.eval.types.contains(CardTypeMask::LAND))
                    {
                        0x8000
                    } else {
                        0
                    };
                    v.push((tid, types.bits() | is_land_bit, subtypes.clone()));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::SetTypes { types, subtypes } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, types.bits(), subtypes.clone()));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::RemoveTypes { types } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, types.bits(), SmallVec::new()));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::SetSubtypes { subtypes } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let is_land_bit = if board
                        .perm(tid)
                        .is_some_and(|p| p.eval.types.contains(CardTypeMask::LAND))
                    {
                        0x8000
                    } else {
                        0
                    };
                    v.push((tid, is_land_bit, subtypes.clone()));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::AllCreatureTypes => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let eligible = board.perm(tid).is_some_and(|p| {
                        p.eval
                            .types
                            .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                    });
                    v.push((tid, u16::from(eligible), SmallVec::new()));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::AddChosenSubtype | LayerMod::SetChosenSubtype => {
                let chosen = item
                    .timed_chosen_subtype
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_subtype));
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let is_land_bit = if board
                        .perm(tid)
                        .is_some_and(|p| p.eval.types.contains(CardTypeMask::LAND))
                    {
                        0x8000
                    } else {
                        0
                    };
                    let mut sts = SmallVec::new();
                    if let Some(c) = chosen {
                        sts.push(c);
                    }
                    v.push((tid, is_land_bit, sts));
                }
                SignaturePayload::Types(v)
            }
            LayerMod::SetColors { colors } | LayerMod::AddColors { colors } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, colors.bits()));
                }
                SignaturePayload::Colors(v)
            }
            LayerMod::SetChosenColor | LayerMod::AddChosenColor => {
                let c = item
                    .timed_chosen_color
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_color))
                    .unwrap_or(ColorMask::empty());
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, c.bits()));
                }
                SignaturePayload::Colors(v)
            }
            LayerMod::Pt { power, toughness } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    if let Some(locked) = item.timed_locked_pt {
                        v.push((tid, Some(locked.power), Some(locked.toughness)));
                    } else {
                        let mut ctx =
                            EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                        ctx.source = item.source_id;
                        ctx.affected = Some(tid);
                        let p = evaluate_amount(state, power, &ctx);
                        let t = evaluate_amount(state, toughness, &ctx);
                        v.push((tid, Some(p), Some(t)));
                    }
                }
                SignaturePayload::Pt(v)
            }
            LayerMod::SetBasePt { power, toughness } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let p = evaluate_amount(state, power, &ctx);
                    let t = evaluate_amount(state, toughness, &ctx);
                    v.push((tid, Some(p), Some(t)));
                }
                SignaturePayload::Pt(v)
            }
            LayerMod::SetBasePower { power } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let p = evaluate_amount(state, power, &ctx);
                    v.push((tid, Some(p), None));
                }
                SignaturePayload::Pt(v)
            }
            LayerMod::SetBaseToughness { toughness } => {
                let mut v = SmallVec::new();
                for &tid in &targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let t = evaluate_amount(state, toughness, &ctx);
                    v.push((tid, None, Some(t)));
                }
                SignaturePayload::Pt(v)
            }
            LayerMod::CopyStatsAndSubtypesFromLastExiled {
                include_subtypes, ..
            } => {
                let exiled = item.source_id.and_then(|s| last_exiled_by(state, s));
                if item.layer == Layer::Layer7b {
                    let exiled_pt = exiled.and_then(|(_, resolved)| resolved.pt);
                    let mut v = SmallVec::new();
                    for &tid in &targets {
                        v.push((
                            tid,
                            exiled_pt.map(|p| p.power),
                            exiled_pt.map(|p| p.toughness),
                        ));
                    }
                    SignaturePayload::Pt(v)
                } else if item.layer == Layer::Layer4 && *include_subtypes {
                    let sts: SmallVec<[mtg_ir::SubtypeId; 2]> = exiled
                        .map(|(_, resolved)| resolved.subtypes.iter().copied().collect())
                        .unwrap_or_default();
                    let mut v = SmallVec::new();
                    for &tid in &targets {
                        let is_land_bit = if board
                            .perm(tid)
                            .is_some_and(|p| p.eval.types.contains(CardTypeMask::LAND))
                        {
                            0x8000
                        } else {
                            0
                        };
                        v.push((tid, is_land_bit, sts.clone()));
                    }
                    SignaturePayload::Types(v)
                } else {
                    SignaturePayload::Unit
                }
            }
            LayerMod::GrantAbilitiesFromObjects {
                from, loyalty_only, ..
            } => {
                let count = count_grantable_abilities(board, state, item, from, *loyalty_only);
                let mut v = SmallVec::new();
                for &tid in &targets {
                    v.push((tid, count as u64));
                }
                SignaturePayload::Keywords(v)
            }
            LayerMod::SwitchPt
            | LayerMod::AddKeywords { .. }
            | LayerMod::RemoveKeywords { .. }
            | LayerMod::RemoveAllAbilities { .. }
            | LayerMod::GrantAbility { .. }
            | LayerMod::ProtectionFromChosenColor { .. }
            | LayerMod::ProtectionFromChosenPlayer
            | LayerMod::ProtectionFromChosenCardType
            | LayerMod::CopyKeywordsFromExiled { .. } => SignaturePayload::Unit,
        },
        LayerOpKind::DynamicWard { generic } => {
            let mut v = SmallVec::new();
            for &tid in &targets {
                let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                ctx.source = item.source_id;
                ctx.affected = Some(tid);
                let g = evaluate_amount(state, generic, &ctx).max(0) as u64;
                v.push((tid, g));
            }
            SignaturePayload::Keywords(v)
        }
        LayerOpKind::GrantKeyword { .. } | LayerOpKind::ReconfigureStripCreature => {
            SignaturePayload::Unit
        }
    };
    EffectSignature {
        active,
        targets,
        payload,
    }
}

fn count_grantable_abilities(
    board: &WorkBoard<'_>,
    state: &GameState,
    item: &LayerEffectItem<'_>,
    from: &AbilityGrantSource,
    loyalty_only: bool,
) -> usize {
    let ctrl = current_source_controller(board, item);
    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
    ctx.source = item.source_id;
    match from {
        AbilityGrantSource::ExiledWithSelf { filter } => {
            let Some(src) = item.source_id else {
                return 0;
            };
            let mut total = 0;
            for p in state.players() {
                for &eid in &p.exile {
                    if state.rare(eid).and_then(|r| r.exiled_by) == Some(src)
                        && matches_object_filter(state, eid, filter, &ctx)
                    {
                        total += active_abilities_for_object(state, eid, &ctx)
                            .into_iter()
                            .filter(|ab| !loyalty_only || is_loyalty_ability(ab.ability))
                            .count();
                    }
                }
            }
            total
        }
        AbilityGrantSource::ExiledCardsWithCounter {
            counter,
            owner,
            filter,
        } => {
            let mut total = 0;
            for p in state.players() {
                if let Some(rel) = owner
                    && !crate::filter::matches_relative_control(p.id, *rel, &ctx)
                {
                    continue;
                }
                for &eid in &p.exile {
                    if state.counter_count(eid, *counter) == 0 {
                        continue;
                    }
                    if let Some(f) = filter
                        && !matches_object_filter(state, eid, f, &ctx)
                    {
                        continue;
                    }
                    total += active_abilities_for_object(state, eid, &ctx)
                        .into_iter()
                        .filter(|ab| !loyalty_only || is_loyalty_ability(ab.ability))
                        .count();
                }
            }
            total
        }
        AbilityGrantSource::Battlefield { filter } => {
            let mut total = 0;
            for perm in &board.perms {
                if perm.extras.innate_mana_abilities_removed {
                    continue;
                }
                if matches_object_filter(state, perm.id, filter, &ctx) {
                    total += active_abilities_for_object(state, perm.id, &ctx)
                        .into_iter()
                        .filter(|ab| {
                            (!loyalty_only || is_loyalty_ability(ab.ability))
                                && (!perm.extras.innate_abilities_removed
                                    || matches!(ab.ability, AbilityDef::Activated(a) if a.is_mana))
                        })
                        .count();
                }
            }
            total
        }
    }
}

#[inline]
fn is_loyalty_ability(ab: &AbilityDef) -> bool {
    match ab {
        AbilityDef::Activated(a) => a
            .cost
            .iter()
            .any(|c| matches!(c, mtg_ir::CostDef::Loyalty { .. })),
        _ => false,
    }
}

pub fn last_exiled_by(
    state: &GameState,
    source: ObjectId,
) -> Option<(ObjectId, crate::faces::ResolvedObjectFaces)> {
    let mut best: Option<(u64, ObjectId)> = None;
    for p in state.players() {
        for &eid in &p.exile {
            if state.rare(eid).and_then(|r| r.exiled_by) == Some(source)
                && let Some(obj) = state.object(eid)
            {
                let ts = obj.entered_timestamp;
                if best.is_none_or(|(bts, _)| ts > bts) {
                    best = Some((ts, eid));
                }
            }
        }
    }
    let (_, eid) = best?;
    let obj = state.object(eid)?;
    let opts = face_opts_for_object(state, eid)?;
    let resolved = resolve_object_faces(state.db(), obj.card_def, &opts)?;
    Some((eid, resolved))
}

pub fn apply_layer_effect<'a>(
    board: &mut WorkBoard<'a>,
    state: &GameState,
    subtype_table: &SubtypeTable,
    item: &LayerEffectItem<'a>,
    collect_notes: bool,
) -> u16 {
    let mut extra_layer_mask: u16 = 0;
    let targets = resolve_effect_targets(board, state, item);
    if targets.is_empty() {
        return extra_layer_mask;
    }

    match item.origin {
        EffectOrigin::PermanentStatic { source, static_idx } => {
            if let Some(src_perm) = board.perm_mut(source)
                && let Some(st) = src_perm.statics.get_mut(static_idx)
            {
                st.applied_targets = targets.iter().copied().collect();
            }
        }
        EffectOrigin::GrantedStatic { host, item_id } => {
            if let Some(src_perm) = board.perm_mut(host)
                && let Some(st) = src_perm
                    .statics
                    .iter_mut()
                    .find(|s| s.granted_item_id == Some(item_id))
            {
                st.applied_targets = targets.iter().copied().collect();
            }
        }
        _ => {}
    }

    let ctrl = current_source_controller(board, item);
    let owned_label: String;
    let label: &str = if collect_notes {
        if let Some(lbl) = item.source_label {
            lbl
        } else if let Some(src) = item.source_id
            && let Some(nm) = board.effective_name(src)
        {
            owned_label = if nm.is_empty() {
                "face-down permanent".into()
            } else {
                nm.to_string()
            };
            &owned_label
        } else {
            "continuous effect"
        }
    } else {
        ""
    };

    let Some(op) = item.op() else {
        return extra_layer_mask;
    };
    match &op {
        LayerOpKind::ReconfigureStripCreature => {
            for tid in targets {
                if let Some(perm) = board.perm_mut(tid) {
                    perm.apply_remove_types(
                        subtype_table,
                        CardTypeMask::CREATURE,
                        Some("Reconfigure"),
                        collect_notes,
                    );
                }
            }
        }
        LayerOpKind::GrantKeyword { keyword } => {
            for tid in targets {
                if let Some(perm) = board.perm_mut(tid) {
                    let id = perm.alloc_item_id();
                    perm.items.push(WorkAbilityItem {
                        id,
                        is_innate: false,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: false,
                        keyword: Some(*keyword),
                        keyword_param: None,
                        granted_ability: None,
                        timestamp: item.timestamp,
                        removed: false,
                    });
                    perm.sync_abilities();
                    perm.push_note(collect_notes, || {
                        let kw_name = state.db().keyword_name(*keyword).unwrap_or("keyword");
                        format!("Layer 6: {label} granted {kw_name}")
                    });
                }
            }
        }
        LayerOpKind::DynamicWard { generic } => {
            board.record_read(DirtyMask::TOPOLOGY);
            for tid in targets {
                let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                ctx.source = item.source_id;
                ctx.affected = Some(tid);
                let g = evaluate_amount(state, generic, &ctx).max(0) as u32;
                if let Some(perm) = board.perm_mut(tid) {
                    let id = perm.alloc_item_id();
                    perm.items.push(WorkAbilityItem {
                        id,
                        is_innate: false,
                        is_intrinsic_basic_mana: false,
                        required_basic_subtype: None,
                        is_mana_ability: false,
                        keyword: subtype_table.ward_kw,
                        keyword_param: None,
                        granted_ability: Some(GrantedAbilityEntry::DynamicWard {
                            generic: g,
                            grant_source: item.source_id,
                        }),
                        timestamp: item.timestamp,
                        removed: false,
                    });
                    perm.sync_abilities();
                    perm.push_note(collect_notes, || {
                        format!("Layer 6: {label} granted Ward {{{g}}}")
                    });
                }
            }
        }
        LayerOpKind::Mod(modifier) => match modifier {
            LayerMod::SetController { controller } => {
                for tid in targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    if let Some(new_ctrl) = resolve_player_ref(state, controller, &ctx)
                        && let Some(perm) = board.perm_mut(tid)
                    {
                        perm.eval.controller = new_ctrl;
                        perm.push_note(collect_notes, || {
                            format!("Layer 2: {label} set controller to P{}", new_ctrl.0)
                        });
                    }
                }
            }
            LayerMod::SetName { name } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.eval.name_override = Some(name.clone());
                        perm.push_note(collect_notes, || {
                            format!("Layer 3: {label} set name to {name}")
                        });
                    }
                }
            }
            LayerMod::AddTypes { types, subtypes } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.apply_add_types(
                            subtype_table,
                            *types,
                            subtypes,
                            item.timestamp,
                            Some(label),
                            collect_notes,
                        );
                    }
                }
            }
            LayerMod::SetTypes { types, subtypes } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.apply_set_types_or_subtypes(
                            subtype_table,
                            Some(*types),
                            Some(subtypes),
                            item.timestamp,
                            Some(label),
                            collect_notes,
                        );
                    }
                }
            }
            LayerMod::RemoveTypes { types } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.apply_remove_types(subtype_table, *types, Some(label), collect_notes);
                    }
                }
            }
            LayerMod::SetSubtypes { subtypes } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.apply_set_types_or_subtypes(
                            subtype_table,
                            None,
                            Some(subtypes),
                            item.timestamp,
                            Some(label),
                            collect_notes,
                        );
                    }
                }
            }
            LayerMod::AllCreatureTypes => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid)
                        && perm
                            .eval
                            .types
                            .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                    {
                        perm.eval.has_all_creature_types = true;
                        perm.push_note(collect_notes, || {
                            format!("Layer 4: {label} granted all creature types")
                        });
                    }
                }
            }
            LayerMod::AddChosenSubtype => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let chosen = item
                    .timed_chosen_subtype
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_subtype));
                if let Some(sid) = chosen {
                    for tid in targets {
                        if let Some(perm) = board.perm_mut(tid) {
                            perm.apply_add_types(
                                subtype_table,
                                CardTypeMask::empty(),
                                &[sid],
                                item.timestamp,
                                Some(label),
                                collect_notes,
                            );
                        }
                    }
                }
            }
            LayerMod::SetChosenSubtype => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let chosen = item
                    .timed_chosen_subtype
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_subtype));
                if let Some(sid) = chosen {
                    for tid in targets {
                        if let Some(perm) = board.perm_mut(tid) {
                            perm.apply_set_types_or_subtypes(
                                subtype_table,
                                None,
                                Some(&[sid]),
                                item.timestamp,
                                Some(label),
                                collect_notes,
                            );
                        }
                    }
                }
            }
            LayerMod::SetColors { colors } => {
                let c = *colors & ColorMask::ALL_COLORS;
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.eval.colors = c;
                        perm.push_note(collect_notes, || format!("Layer 5: {label} set colors"));
                    }
                }
            }
            LayerMod::AddColors { colors } => {
                let c = *colors & ColorMask::ALL_COLORS;
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.eval.colors |= c;
                        perm.push_note(collect_notes, || format!("Layer 5: {label} added colors"));
                    }
                }
            }
            LayerMod::SetChosenColor => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let c = item
                    .timed_chosen_color
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_color))
                    .unwrap_or(ColorMask::empty())
                    & ColorMask::ALL_COLORS;
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.eval.colors = c;
                        perm.push_note(collect_notes, || {
                            format!("Layer 5: {label} set chosen color")
                        });
                    }
                }
            }
            LayerMod::AddChosenColor => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let c = item
                    .timed_chosen_color
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_color))
                    .unwrap_or(ColorMask::empty())
                    & ColorMask::ALL_COLORS;
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.eval.colors |= c;
                        perm.push_note(collect_notes, || {
                            format!("Layer 5: {label} added chosen color")
                        });
                    }
                }
            }
            LayerMod::AddKeywords {
                keywords,
                protection_attachment_exemption,
            } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        for &kw in keywords {
                            let id = perm.alloc_item_id();
                            perm.items.push(WorkAbilityItem {
                                id,
                                is_innate: false,
                                is_intrinsic_basic_mana: false,
                                required_basic_subtype: None,
                                is_mana_ability: false,
                                keyword: Some(kw),
                                keyword_param: None,
                                granted_ability: None,
                                timestamp: item.timestamp,
                                removed: false,
                            });
                            if let Some(ex) = protection_attachment_exemption
                                && !perm.extras.protection_exemptions.contains(&(kw, *ex))
                            {
                                perm.extras.protection_exemptions.push((kw, *ex));
                            }
                        }
                        perm.sync_abilities();
                        perm.push_note(collect_notes, || {
                            format!("Layer 6: {label} added keywords")
                        });
                    }
                }
            }
            LayerMod::RemoveKeywords {
                keywords,
                cant_have_or_gain,
            } => {
                let removes_hexproof = keywords.iter().any(|&kw| {
                    state
                        .db()
                        .keyword_name(kw)
                        .is_some_and(|n| n.eq_ignore_ascii_case("hexproof"))
                });
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        for it in &mut perm.items {
                            if !it.removed
                                && let Some(kw) = it.keyword
                                && (keywords.contains(&kw)
                                    || (removes_hexproof
                                        && state.db().keyword_name(kw).is_some_and(|n| {
                                            n.to_ascii_lowercase().starts_with("hexproof from ")
                                        })))
                            {
                                it.removed = true;
                            }
                        }
                        if *cant_have_or_gain {
                            for &kw in keywords {
                                if !perm.extras.cant_have_keywords.contains(&kw) {
                                    perm.extras.cant_have_keywords.push(kw);
                                }
                            }
                        }
                        perm.sync_abilities();
                        perm.push_note(collect_notes, || {
                            format!("Layer 6: {label} removed keywords")
                        });
                    }
                }
            }
            LayerMod::RemoveAllAbilities {
                keep_mana_abilities,
            } => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        perm.apply_remove_all_abilities(
                            *keep_mana_abilities,
                            item.timestamp,
                            Some(label),
                            collect_notes,
                        );
                    }
                }
            }
            LayerMod::GrantAbility { ability } => {
                let inline_origin = match item.provenance.origin {
                    StaticGrantOrigin::Innate => ReplacementGrantOrigin::StaticAbility,
                    StaticGrantOrigin::TimedEffect => ReplacementGrantOrigin::TimedEffect,
                    StaticGrantOrigin::Nested => ReplacementGrantOrigin::NestedStatic,
                };
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        let item_id = perm.alloc_item_id();
                        let grant_ts = item.timestamp.max(perm.timestamp);
                        match ability.as_ref() {
                            AbilityDef::Keyword(kw) => {
                                perm.items.push(WorkAbilityItem {
                                    id: item_id,
                                    is_innate: false,
                                    is_intrinsic_basic_mana: false,
                                    required_basic_subtype: None,
                                    is_mana_ability: false,
                                    keyword: Some(kw.keyword.keyword),
                                    keyword_param: kw.keyword.param.clone(),
                                    granted_ability: None,
                                    timestamp: grant_ts,
                                    removed: false,
                                });
                            }
                            AbilityDef::Static(st) => {
                                let entry = GrantedAbilityEntry::Inline {
                                    ability: Box::new((**ability).clone()),
                                    is_mana: false,
                                    grant_source: item.source_id,
                                    grant_controller: ctrl,
                                    grant_origin: inline_origin,
                                    grant_card_def: item.provenance.card_def,
                                    source_face_index: item.provenance.face_index,
                                    source_component_index: item.provenance.component_index,
                                    source_ability_index: item.provenance.ability_index,
                                    source_effect_id: item.provenance.effect_id,
                                };
                                perm.items.push(WorkAbilityItem {
                                    id: item_id,
                                    is_innate: false,
                                    is_intrinsic_basic_mana: false,
                                    required_basic_subtype: None,
                                    is_mana_ability: false,
                                    keyword: None,
                                    keyword_param: None,
                                    granted_ability: Some(entry),
                                    timestamp: grant_ts,
                                    removed: false,
                                });
                                // CR 613.7a: Granted static ability receives max(grant_ts, host_ts).
                                let host_tag = item.source_id.map_or(0, |o| {
                                    (u64::from(o.index) << 32) | u64::from(o.generation)
                                });
                                let nested_effect_id = match item.provenance.origin {
                                    StaticGrantOrigin::Innate => host_tag,
                                    StaticGrantOrigin::TimedEffect => item.provenance.effect_id,
                                    StaticGrantOrigin::Nested => {
                                        item.provenance.effect_id ^ host_tag.rotate_left(17)
                                    }
                                };
                                let nested_prov = StaticProvenance {
                                    origin: StaticGrantOrigin::Nested,
                                    card_def: item.provenance.card_def,
                                    face_index: item.provenance.face_index,
                                    component_index: item.provenance.component_index,
                                    ability_index: item.provenance.ability_index,
                                    effect_id: nested_effect_id,
                                };
                                let sidx = perm.statics.len();
                                perm.statics.push(WorkStatic {
                                    static_idx: sidx,
                                    provenance: nested_prov,
                                    static_def: &st.static_def,
                                    span: st.span,
                                    timestamp: grant_ts,
                                    started_in_earlier_layer: false,
                                    granted_item_id: Some(item_id),
                                    applied_targets: SmallVec::new(),
                                });
                                extra_layer_mask |= layer_mask_for_static_def(&st.static_def, true);
                            }
                            AbilityDef::Activated(act) => {
                                perm.items.push(WorkAbilityItem {
                                    id: item_id,
                                    is_innate: false,
                                    is_intrinsic_basic_mana: false,
                                    required_basic_subtype: None,
                                    is_mana_ability: act.is_mana,
                                    keyword: None,
                                    keyword_param: None,
                                    granted_ability: Some(GrantedAbilityEntry::Inline {
                                        ability: Box::new((**ability).clone()),
                                        is_mana: act.is_mana,
                                        grant_source: item.source_id,
                                        grant_controller: ctrl,
                                        grant_origin: inline_origin,
                                        grant_card_def: item.provenance.card_def,
                                        source_face_index: item.provenance.face_index,
                                        source_component_index: item.provenance.component_index,
                                        source_ability_index: item.provenance.ability_index,
                                        source_effect_id: item.provenance.effect_id,
                                    }),
                                    timestamp: grant_ts,
                                    removed: false,
                                });
                            }
                            AbilityDef::Triggered(_)
                            | AbilityDef::Replacement(_)
                            | AbilityDef::Spell(_)
                            | AbilityDef::Unsupported(_) => {
                                perm.items.push(WorkAbilityItem {
                                    id: item_id,
                                    is_innate: false,
                                    is_intrinsic_basic_mana: false,
                                    required_basic_subtype: None,
                                    is_mana_ability: false,
                                    keyword: None,
                                    keyword_param: None,
                                    granted_ability: Some(GrantedAbilityEntry::Inline {
                                        ability: Box::new((**ability).clone()),
                                        is_mana: false,
                                        grant_source: item.source_id,
                                        grant_controller: ctrl,
                                        grant_origin: inline_origin,
                                        grant_card_def: item.provenance.card_def,
                                        source_face_index: item.provenance.face_index,
                                        source_component_index: item.provenance.component_index,
                                        source_ability_index: item.provenance.ability_index,
                                        source_effect_id: item.provenance.effect_id,
                                    }),
                                    timestamp: grant_ts,
                                    removed: false,
                                });
                            }
                        }
                        perm.sync_abilities();
                        perm.push_note(collect_notes, || {
                            format!("Layer 6: {label} granted ability")
                        });
                    }
                    if let AbilityDef::Static(st) = ability.as_ref() {
                        if matches!(
                            st.static_def.body,
                            StaticBody::OffBattlefieldCharacteristics { .. }
                        ) {
                            board.may_have_off_bf_char_grants = true;
                            board.record_off_bf_grantor(tid);
                        }
                        if matches!(
                            st.static_def.body,
                            StaticBody::RuleModifier {
                                rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                                ..
                            }
                        ) {
                            board.may_have_all_off_bf_colorless = true;
                        }
                    }
                }
            }
            LayerMod::ProtectionFromChosenColor {
                protection_attachment_exemption,
            } => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let c = item
                    .timed_chosen_color
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_color))
                    .unwrap_or(ColorMask::empty())
                    & ColorMask::ALL_COLORS;
                if !c.is_empty() {
                    for tid in targets {
                        if let Some(perm) = board.perm_mut(tid) {
                            perm.extras.protection_from_chosen_color =
                                Some((c, *protection_attachment_exemption));
                            perm.push_note(collect_notes, || {
                                format!("Layer 6: {label} granted protection from chosen color")
                            });
                        }
                    }
                }
            }
            LayerMod::ProtectionFromChosenPlayer => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let cp = item
                    .timed_chosen_player
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_player));
                if let Some(pid) = cp {
                    for tid in targets {
                        if let Some(perm) = board.perm_mut(tid) {
                            if !perm.extras.protection_from_players.contains(&pid) {
                                perm.extras.protection_from_players.push(pid);
                            }
                            perm.push_note(collect_notes, || {
                                format!("Layer 6: {label} granted protection from P{}", pid.0)
                            });
                        }
                    }
                }
            }
            LayerMod::ProtectionFromChosenCardType => {
                board.record_read(DirtyMask::DESIGNATIONS);
                let ct = item
                    .timed_chosen_card_type
                    .or_else(|| item.source_id.and_then(|s| state.rare(s)?.chosen_card_type))
                    .unwrap_or(CardTypeMask::empty());
                if !ct.is_empty() {
                    for tid in targets {
                        if let Some(perm) = board.perm_mut(tid) {
                            perm.extras.protection_from_card_types |= ct;
                            perm.push_note(collect_notes, || {
                                format!("Layer 6: {label} granted protection from chosen card type")
                            });
                        }
                    }
                }
            }
            LayerMod::GrantAbilitiesFromObjects {
                from,
                mana_as_any_color,
                once_each_turn,
                loyalty_only,
            } => {
                #[derive(Clone)]
                struct CollectedGrant {
                    card_def: mtg_ir::CardDefId,
                    face_index: u8,
                    ability_index: usize,
                    donor_object: ObjectId,
                    donor_component_index: u8,
                    is_mana: bool,
                    kw: Option<KeywordId>,
                    keyword_param: Option<String>,
                }
                let mut collected: SmallVec<[CollectedGrant; 8]> = SmallVec::new();
                let ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                let mut ctx_with_src = ctx;
                ctx_with_src.source = item.source_id;
                match from {
                    AbilityGrantSource::ExiledWithSelf { filter } => {
                        board.record_read(DirtyMask::EXILE);
                        if let Some(src) = item.source_id {
                            for p in state.players() {
                                for &eid in &p.exile {
                                    if state.rare(eid).and_then(|r| r.exiled_by) == Some(src)
                                        && matches_object_filter(state, eid, filter, &ctx_with_src)
                                    {
                                        for ab in active_abilities_with_card_for_object(
                                            state,
                                            eid,
                                            &ctx_with_src,
                                        ) {
                                            if *loyalty_only && !is_loyalty_ability(ab.ability) {
                                                continue;
                                            }
                                            let is_mana = matches!(ab.ability, AbilityDef::Activated(a) if a.is_mana);
                                            let (kw, keyword_param) = match ab.ability {
                                                AbilityDef::Keyword(k) => (
                                                    Some(k.keyword.keyword),
                                                    k.keyword.param.clone(),
                                                ),
                                                _ => (None, None),
                                            };
                                            collected.push(CollectedGrant {
                                                card_def: ab.card_def,
                                                face_index: ab.face_index,
                                                ability_index: ab.ability_index,
                                                donor_object: eid,
                                                donor_component_index: ab.component_index,
                                                is_mana,
                                                kw,
                                                keyword_param,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    AbilityGrantSource::ExiledCardsWithCounter {
                        counter,
                        owner,
                        filter,
                    } => {
                        board.record_read(DirtyMask::EXILE | DirtyMask::COUNTERS);
                        for p in state.players() {
                            if let Some(rel) = owner
                                && !crate::filter::matches_relative_control(
                                    p.id,
                                    *rel,
                                    &ctx_with_src,
                                )
                            {
                                continue;
                            }
                            for &eid in &p.exile {
                                if state.counter_count(eid, *counter) == 0 {
                                    continue;
                                }
                                if let Some(f) = filter
                                    && !matches_object_filter(state, eid, f, &ctx_with_src)
                                {
                                    continue;
                                }
                                for ab in
                                    active_abilities_with_card_for_object(state, eid, &ctx_with_src)
                                {
                                    if *loyalty_only && !is_loyalty_ability(ab.ability) {
                                        continue;
                                    }
                                    let is_mana =
                                        matches!(ab.ability, AbilityDef::Activated(a) if a.is_mana);
                                    let (kw, keyword_param) = match ab.ability {
                                        AbilityDef::Keyword(k) => {
                                            (Some(k.keyword.keyword), k.keyword.param.clone())
                                        }
                                        _ => (None, None),
                                    };
                                    collected.push(CollectedGrant {
                                        card_def: ab.card_def,
                                        face_index: ab.face_index,
                                        ability_index: ab.ability_index,
                                        donor_object: eid,
                                        donor_component_index: ab.component_index,
                                        is_mana,
                                        kw,
                                        keyword_param,
                                    });
                                }
                            }
                        }
                    }
                    AbilityGrantSource::Battlefield { filter } => {
                        board.record_read(DirtyMask::TOPOLOGY);
                        for perm in &board.perms {
                            if perm.extras.innate_mana_abilities_removed {
                                continue;
                            }
                            if matches_object_filter(state, perm.id, filter, &ctx_with_src) {
                                for ab in active_abilities_with_card_for_object(
                                    state,
                                    perm.id,
                                    &ctx_with_src,
                                ) {
                                    if *loyalty_only && !is_loyalty_ability(ab.ability) {
                                        continue;
                                    }
                                    let is_mana =
                                        matches!(ab.ability, AbilityDef::Activated(a) if a.is_mana);
                                    if perm.extras.innate_abilities_removed && !is_mana {
                                        continue;
                                    }
                                    let (kw, keyword_param) = match ab.ability {
                                        AbilityDef::Keyword(k) => {
                                            (Some(k.keyword.keyword), k.keyword.param.clone())
                                        }
                                        _ => (None, None),
                                    };
                                    collected.push(CollectedGrant {
                                        card_def: ab.card_def,
                                        face_index: ab.face_index,
                                        ability_index: ab.ability_index,
                                        donor_object: perm.id,
                                        donor_component_index: ab.component_index,
                                        is_mana,
                                        kw,
                                        keyword_param,
                                    });
                                }
                            }
                        }
                    }
                }
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        for g in &collected {
                            let id = perm.alloc_item_id();
                            let entry = if g.kw.is_some() {
                                None
                            } else {
                                Some(GrantedAbilityEntry::CardAbility {
                                    card_def: g.card_def,
                                    face_index: g.face_index,
                                    ability_index: g.ability_index,
                                    is_mana: g.is_mana,
                                    mana_as_any_color: *mana_as_any_color,
                                    once_each_turn: *once_each_turn,
                                    grant_source: item.source_id,
                                    grant_controller: ctrl,
                                    donor_object: g.donor_object,
                                    donor_component_index: g.donor_component_index,
                                    granter_origin: item.provenance.origin,
                                    granter_card_def: item.provenance.card_def,
                                    granter_face_index: item.provenance.face_index,
                                    granter_component_index: item.provenance.component_index,
                                    granter_ability_index: item.provenance.ability_index,
                                    source_effect_id: item.provenance.effect_id,
                                })
                            };
                            perm.items.push(WorkAbilityItem {
                                id,
                                is_innate: false,
                                is_intrinsic_basic_mana: false,
                                required_basic_subtype: None,
                                is_mana_ability: g.is_mana,
                                keyword: g.kw,
                                keyword_param: g.keyword_param.clone(),
                                granted_ability: entry,
                                timestamp: item.timestamp,
                                removed: false,
                            });
                        }
                        perm.sync_abilities();
                        perm.push_note(collect_notes, || {
                            format!("Layer 6: {label} granted abilities from objects")
                        });
                    }
                }
            }
            LayerMod::CopyKeywordsFromExiled { keywords } => {
                board.record_read(DirtyMask::EXILE);
                let mut found_kws: SmallVec<[(KeywordId, Option<String>); 4]> = SmallVec::new();
                if let Some(src) = item.source_id {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = Some(src);
                    for p in state.players() {
                        for &eid in &p.exile {
                            if state.rare(eid).and_then(|r| r.exiled_by) == Some(src) {
                                for ab in active_abilities_for_object(state, eid, &ctx) {
                                    if let AbilityDef::Keyword(k) = ab.ability {
                                        let kw = k.keyword.keyword;
                                        let param = k.keyword.param.clone();
                                        if (keywords.is_empty() || keywords.contains(&kw))
                                            && !found_kws
                                                .iter()
                                                .any(|(ek, ep)| *ek == kw && *ep == param)
                                        {
                                            found_kws.push((kw, param));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid) {
                        for (kw, param) in &found_kws {
                            let id = perm.alloc_item_id();
                            perm.items.push(WorkAbilityItem {
                                id,
                                is_innate: false,
                                is_intrinsic_basic_mana: false,
                                required_basic_subtype: None,
                                is_mana_ability: false,
                                keyword: Some(*kw),
                                keyword_param: param.clone(),
                                granted_ability: None,
                                timestamp: item.timestamp,
                                removed: false,
                            });
                            if !perm.extras.copied_keywords_from_exiled.contains(kw) {
                                perm.extras.copied_keywords_from_exiled.push(*kw);
                            }
                        }
                        perm.sync_abilities();
                        perm.push_note(collect_notes, || {
                            format!("Layer 6: {label} copied keywords from exiled cards")
                        });
                    }
                }
            }
            LayerMod::CopyStatsAndSubtypesFromLastExiled {
                include_subtypes,
                keep_self_name,
            } => {
                board.record_read(DirtyMask::EXILE);
                if let Some(src) = item.source_id
                    && let Some((_, resolved)) = last_exiled_by(state, src)
                {
                    match item.layer {
                        Layer::Layer3 => {
                            if !*keep_self_name {
                                for tid in targets {
                                    if let Some(perm) = board.perm_mut(tid) {
                                        perm.eval.name_override = Some(resolved.name.clone());
                                        perm.push_note(collect_notes, || {
                                            format!(
                                                "Layer 3: {label} copied name {}",
                                                resolved.name
                                            )
                                        });
                                    }
                                }
                            }
                        }
                        Layer::Layer4 => {
                            if *include_subtypes {
                                for tid in targets {
                                    if let Some(perm) = board.perm_mut(tid) {
                                        perm.apply_add_types(
                                            subtype_table,
                                            CardTypeMask::empty(),
                                            &resolved.subtypes,
                                            item.timestamp,
                                            Some(label),
                                            collect_notes,
                                        );
                                    }
                                }
                            }
                        }
                        Layer::Layer7b => {
                            if let Some(pt) = resolved.pt {
                                for tid in targets {
                                    if let Some(perm) = board.perm_mut(tid) {
                                        perm.eval.base_pt = Some(pt);
                                        perm.eval.pt = Some(pt);
                                        perm.push_note(collect_notes, || {
                                            format!(
                                                "Layer 7b: {label} set base P/T to {}/{}",
                                                pt.power, pt.toughness
                                            )
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            LayerMod::SetBasePt { power, toughness } => {
                for tid in targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let p = evaluate_amount(state, power, &ctx);
                    let t = evaluate_amount(state, toughness, &ctx);
                    if let Some(perm) = board.perm_mut(tid) {
                        if item.layer == Layer::Layer7a {
                            if perm.has_copy_pt_power_except && perm.has_copy_pt_toughness_except {
                                continue;
                            }
                            let cur = perm.eval.base_pt.unwrap_or(PowerToughness {
                                power: 0,
                                toughness: 0,
                            });
                            let next = PowerToughness {
                                power: if perm.has_copy_pt_power_except {
                                    cur.power
                                } else {
                                    p
                                },
                                toughness: if perm.has_copy_pt_toughness_except {
                                    cur.toughness
                                } else {
                                    t
                                },
                            };
                            perm.eval.base_pt = Some(next);
                            perm.eval.pt = Some(next);
                            perm.push_note(collect_notes, || {
                                format!(
                                    "Layer 7a: {label} CDA P/T {}/{}",
                                    next.power, next.toughness
                                )
                            });
                        } else {
                            let next = PowerToughness {
                                power: p,
                                toughness: t,
                            };
                            perm.eval.base_pt = Some(next);
                            perm.eval.pt = Some(next);
                            perm.push_note(collect_notes, || {
                                format!("Layer 7b: {label} set base P/T to {p}/{t}")
                            });
                        }
                    }
                }
            }
            LayerMod::SetBasePower { power } => {
                for tid in targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let p = evaluate_amount(state, power, &ctx);
                    if let Some(perm) = board.perm_mut(tid) {
                        if item.layer == Layer::Layer7a && perm.has_copy_pt_power_except {
                            continue;
                        }
                        let cur = perm.eval.pt.unwrap_or(PowerToughness {
                            power: 0,
                            toughness: 0,
                        });
                        let next = PowerToughness {
                            power: p,
                            toughness: cur.toughness,
                        };
                        perm.eval.base_pt = Some(next);
                        perm.eval.pt = Some(next);
                        let lstr = if item.layer == Layer::Layer7a {
                            "7a"
                        } else {
                            "7b"
                        };
                        perm.push_note(collect_notes, || {
                            format!("Layer {lstr}: {label} set base power to {p}")
                        });
                    }
                }
            }
            LayerMod::SetBaseToughness { toughness } => {
                for tid in targets {
                    let mut ctx = EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                    ctx.source = item.source_id;
                    ctx.affected = Some(tid);
                    let t = evaluate_amount(state, toughness, &ctx);
                    if let Some(perm) = board.perm_mut(tid) {
                        if item.layer == Layer::Layer7a && perm.has_copy_pt_toughness_except {
                            continue;
                        }
                        let cur = perm.eval.pt.unwrap_or(PowerToughness {
                            power: 0,
                            toughness: 0,
                        });
                        let next = PowerToughness {
                            power: cur.power,
                            toughness: t,
                        };
                        perm.eval.base_pt = Some(next);
                        perm.eval.pt = Some(next);
                        let lstr = if item.layer == Layer::Layer7a {
                            "7a"
                        } else {
                            "7b"
                        };
                        perm.push_note(collect_notes, || {
                            format!("Layer {lstr}: {label} set base toughness to {t}")
                        });
                    }
                }
            }
            LayerMod::Pt { power, toughness } => {
                for tid in targets {
                    let (dp, dt) = if let Some(locked) = item.timed_locked_pt {
                        (locked.power, locked.toughness)
                    } else {
                        let mut ctx =
                            EvalContext::new(ctrl).with_board(BoardView::LayerWork(board));
                        ctx.source = item.source_id;
                        ctx.affected = Some(tid);
                        (
                            evaluate_amount(state, power, &ctx),
                            evaluate_amount(state, toughness, &ctx),
                        )
                    };
                    if let Some(perm) = board.perm_mut(tid)
                        && let Some(pt) = &mut perm.eval.pt
                    {
                        pt.power += dp;
                        pt.toughness += dt;
                        perm.push_note(collect_notes, || {
                            format!("Layer 7c: {label} ({dp:+}/{dt:+})")
                        });
                    }
                }
            }
            LayerMod::SwitchPt => {
                for tid in targets {
                    if let Some(perm) = board.perm_mut(tid)
                        && let Some(pt) = &mut perm.eval.pt
                    {
                        std::mem::swap(&mut pt.power, &mut pt.toughness);
                        perm.push_note(collect_notes, || format!("Layer 7d: {label} switched P/T"));
                    }
                }
            }
        },
    }
    extra_layer_mask
}

/// Applies a sublayer's effects using the fast path when safe, or full `CR 613.8a–c` Tarjan-SCC
/// dynamic dependency ordering with timestamp cycle breaking (`CR 613.8b`).
#[must_use]
pub fn apply_layer_with_deps<'a>(
    board: &mut WorkBoard<'a>,
    state: &GameState,
    subtype_table: &SubtypeTable,
    layer: Layer,
    effects: &mut [LayerEffectItem<'a>],
    collect_notes: bool,
) -> u16 {
    if effects.is_empty() {
        return 0;
    }

    let mut extra_layer_mask: u16 = 0;
    if can_use_timestamp_fast_path(layer, effects) {
        effects.sort_by_key(|e| (e.timestamp, e.order));
        for item in effects.iter() {
            extra_layer_mask |=
                apply_layer_effect(board, state, subtype_table, item, collect_notes);
        }
        return extra_layer_mask;
    }

    let n = effects.len();
    let reads: SmallVec<[BoardFieldsMask; 16]> = effects.iter().map(reads_for_effect).collect();
    let writes: SmallVec<[BoardFieldsMask; 16]> = effects.iter().map(writes_for_effect).collect();
    let mut applied = vec![false; n];
    let mut sig_base: Vec<Option<EffectSignature>> = Vec::with_capacity(n);
    for e in effects.iter() {
        sig_base.push(Some(effect_signature(board, state, e)));
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut last_writes = BoardFieldsMask::empty();

    for step in 0..n {
        if step > 0 {
            for j in 0..n {
                if applied[j] {
                    sig_base[j] = None;
                } else if reads[j].intersects(last_writes) {
                    sig_base[j] = Some(effect_signature(board, state, &effects[j]));
                }
                adj[j].clear();
            }
        }

        for i in 0..n {
            if applied[i] {
                continue;
            }
            let Some(sig_i) = &sig_base[i] else {
                continue;
            };
            if !sig_i.active || sig_i.targets.is_empty() {
                continue;
            }
            let wi = writes[i];
            if wi.is_empty() {
                continue;
            }
            let any_dep_cand = (0..n).any(|j| {
                i != j
                    && !applied[j]
                    && effects[i].is_cda == effects[j].is_cda
                    && reads[j].intersects(wi)
            });
            if !any_dep_cand {
                continue;
            }
            let mut probe = board.clone();
            apply_layer_effect(&mut probe, state, subtype_table, &effects[i], false);
            for j in 0..n {
                if i == j || applied[j] {
                    continue;
                }
                // CR 613.8a(c): CDAs and non-CDAs do not depend on each other in the same sublayer.
                if effects[i].is_cda != effects[j].is_cda {
                    continue;
                }
                if !reads[j].intersects(wi) {
                    continue;
                }
                let sig_after = effect_signature(&probe, state, &effects[j]);
                if Some(&sig_after) != sig_base[j].as_ref() {
                    // `j` depends on `i` -> `i` must precede `j`.
                    adj[i].push(j);
                }
            }
        }

        let best = pick_next_source_scc_node(&applied, &adj, effects);
        extra_layer_mask |=
            apply_layer_effect(board, state, subtype_table, &effects[best], collect_notes);
        applied[best] = true;
        last_writes = writes[best];
    }
    extra_layer_mask
}

/// Tarjan's Strongly Connected Components over unapplied nodes (`0..n`), selecting the single
/// node with minimum `(timestamp, order)` across all source SCCs (zero incoming inter-SCC edges).
fn pick_next_source_scc_node(
    applied: &[bool],
    adj: &[Vec<usize>],
    effects: &[LayerEffectItem<'_>],
) -> usize {
    let n = applied.len();
    let mut index_counter: usize = 0;
    let mut indices: Vec<Option<usize>> = vec![None; n];
    let mut lowlink: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::with_capacity(n);
    let mut comp_of: Vec<usize> = vec![usize::MAX; n];
    let mut comps: Vec<Vec<usize>> = Vec::new();

    struct TarjanEnv<'a> {
        applied: &'a [bool],
        adj: &'a [Vec<usize>],
        index_counter: &'a mut usize,
        indices: &'a mut [Option<usize>],
        lowlink: &'a mut [usize],
        on_stack: &'a mut [bool],
        stack: &'a mut Vec<usize>,
        comp_of: &'a mut [usize],
        comps: &'a mut Vec<Vec<usize>>,
    }

    fn strongconnect(v: usize, env: &mut TarjanEnv<'_>) {
        let idx = *env.index_counter;
        *env.index_counter += 1;
        env.indices[v] = Some(idx);
        env.lowlink[v] = idx;
        env.stack.push(v);
        env.on_stack[v] = true;

        for &w in &env.adj[v] {
            if env.applied[w] {
                continue;
            }
            if env.indices[w].is_none() {
                strongconnect(w, env);
                env.lowlink[v] = env.lowlink[v].min(env.lowlink[w]);
            } else if env.on_stack[w] {
                let w_idx = env.indices[w].unwrap_or(usize::MAX);
                env.lowlink[v] = env.lowlink[v].min(w_idx);
            }
        }

        if env.lowlink[v] == idx {
            let cid = env.comps.len();
            let mut members = Vec::new();
            while let Some(w) = env.stack.pop() {
                env.on_stack[w] = false;
                env.comp_of[w] = cid;
                members.push(w);
                if w == v {
                    break;
                }
            }
            env.comps.push(members);
        }
    }

    {
        let mut env = TarjanEnv {
            applied,
            adj,
            index_counter: &mut index_counter,
            indices: &mut indices,
            lowlink: &mut lowlink,
            on_stack: &mut on_stack,
            stack: &mut stack,
            comp_of: &mut comp_of,
            comps: &mut comps,
        };
        for v in 0..n {
            if !applied[v] && env.indices[v].is_none() {
                strongconnect(v, &mut env);
            }
        }
    }

    let mut has_incoming = vec![false; comps.len()];
    for u in 0..n {
        if applied[u] {
            continue;
        }
        let cu = comp_of[u];
        for &v in &adj[u] {
            if applied[v] {
                continue;
            }
            let cv = comp_of[v];
            if cu != cv {
                has_incoming[cv] = true;
            }
        }
    }

    let mut best: Option<usize> = None;
    for (cid, members) in comps.iter().enumerate() {
        if has_incoming[cid] {
            continue;
        }
        for &m in members {
            if best.is_none_or(|b| {
                (effects[m].timestamp, effects[m].order) < (effects[b].timestamp, effects[b].order)
            }) {
                best = Some(m);
            }
        }
    }

    best.unwrap_or_else(|| (0..n).find(|&i| !applied[i]).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtg_ir::{AttachmentOwnerTarget, CompareOp, ManaColor};

    #[test]
    fn reads_for_condition_covers_equipped_enchanted_color_and_off_bf_fields() {
        let eq_cond = Condition::ObjectState {
            of: AttachmentOwnerTarget::SelfObj,
            tapped: None,
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: Some(true),
            enchanted: None,
            filter: None,
        };
        assert_eq!(
            reads_for_condition(&eq_cond),
            BoardFieldsMask::TYPES_SUBTYPES
        );

        let ench_cond = Condition::ObjectState {
            of: AttachmentOwnerTarget::SelfObj,
            tapped: None,
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: Some(false),
            filter: None,
        };
        assert_eq!(
            reads_for_condition(&ench_cond),
            BoardFieldsMask::TYPES_SUBTYPES
        );

        let mcc_cond = Condition::MostCommonColorAmongPermanents {
            color: ManaColor::U,
            allow_tie: false,
        };
        assert_eq!(reads_for_condition(&mcc_cond), BoardFieldsMask::COLORS);

        let ev_or_sub = Condition::EventObjectMatchesOrSubtype {
            filter: Box::new(ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            }),
        };
        assert_eq!(
            reads_for_condition(&ev_or_sub),
            BoardFieldsMask::TYPES_SUBTYPES
                | BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::CONTROLLER
        );

        let only_named = Condition::OnlyNamedOrNoMatchingInHand {
            filter: Box::new(ObjectFilter::default()),
        };
        assert_eq!(
            reads_for_condition(&only_named),
            BoardFieldsMask::NAME | BoardFieldsMask::ABILITIES_STATE | BoardFieldsMask::CONTROLLER
        );

        let atk_you = Condition::AttackingYouCount {
            op: CompareOp::Ge,
            n: 1,
        };
        assert_eq!(reads_for_condition(&atk_you), BoardFieldsMask::CONTROLLER);
    }

    #[test]
    fn reads_for_amount_and_effect_cover_exile_names_colors_and_all_creature_types() {
        let exiled_share = Amount::ExiledCardsSharingTypeCount { owner: None };
        assert_eq!(
            reads_for_amount(&exiled_share),
            BoardFieldsMask::ABILITIES_STATE
                | BoardFieldsMask::CONTROLLER
                | BoardFieldsMask::TYPES_SUBTYPES
        );

        let distinct_names = Amount::DistinctNamesCount {
            of: Box::new(ObjectFilter::default()),
        };
        assert_eq!(reads_for_amount(&distinct_names), BoardFieldsMask::NAME);

        let distinct_colors = Amount::DistinctColorsAmongControlled;
        assert_eq!(
            reads_for_amount(&distinct_colors),
            BoardFieldsMask::COLORS | BoardFieldsMask::CONTROLLER
        );

        let st = StaticDef {
            body: StaticBody::Continuous {
                layer: Layer::Layer4,
                affected: Affected::SelfObj,
                modifier: LayerMod::AllCreatureTypes,
                active_in_zone: None,
            },
            condition: None,
        };
        let item = LayerEffectItem {
            origin: EffectOrigin::TimedEffect { index: 0 },
            provenance: StaticProvenance::default(),
            source_id: None,
            source_controller: PlayerId(0),
            source_label: None,
            timestamp: 1,
            order: 0,
            is_cda: false,
            layer: Layer::Layer4,
            payload: EffectPayload::StaticRef(&st),
            started_in_earlier_layer: false,
            locked_targets: None,
            timed_chosen_color: None,
            timed_chosen_subtype: None,
            timed_chosen_player: None,
            timed_chosen_card_type: None,
            timed_locked_pt: None,
        };
        assert!(reads_for_effect(&item).contains(BoardFieldsMask::TYPES_SUBTYPES));
    }
}
