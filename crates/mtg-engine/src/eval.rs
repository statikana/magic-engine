use std::fmt;
use std::sync::Arc;

use mtg_ir::{
    AbilityDef, Affected, Amount, AttachmentOwnerTarget, CardDatabase, CardDefId, CardLayout,
    CardTypeMask, ColorMask, CompareOp, Condition, CountedThisWayKind, CounterId, CounterTarget,
    Duration, GamePhase, KeywordId, Layer, LayerMod, LifeCompareWho, ManaColor, ManaCost, ManaPip,
    NamedAltCostKind, ObjectColorTarget, ObjectFilter, OpponentComparisonMetric,
    OpponentMetricKind, PlayerCounterKind, PlayerCounterScope, PlayerRef, PrintedPt, Ref,
    RelativeControl, RoundMode, RuleMod, SacrificedStatKind, SpellsCastWho, SpellsLastTurnMode,
    StatKind, StaticBody, SubtypeId, SupertypeMask, Zone, ZoneMask, ZoneScopeWhose,
};
use smallvec::SmallVec;

use crate::action::TargetRef;
use crate::events::TurnEventHistory;
use crate::faces::{
    face_opts_for_game_object, for_each_active_ability_on_faces, is_non_creature_subtype,
    resolve_object_faces,
};
use crate::filter::{
    find_objects, matches_lki_filter, matches_object_filter, matches_player_filter,
    matches_relative_control,
};
use crate::id::{ObjectId, PlayerId};
use crate::state::{
    AltCastKind, DirtyMask, EvaluatedObject, GameObject, GameState, LayerCache, LkiSnapshot,
    ObjectFlags, PlayerState, ResolutionBinding, StackEntry, StackEntryKind, StepWayStats,
    TOKEN_CARD_DEF_ID, evaluate_token_spec_base,
};
use crate::view::PowerToughness;

/// Hard recursion ceiling for nested filter / amount / CDA evaluation.
pub const MAX_EVAL_DEPTH: u8 = 16;

/// Non-reentrant board view implemented by `layers/`'s in-progress `WorkBoard` so `filter`
/// and `eval` queries never re-enter `OnceLock<LayerCache>` during `compute_layer_cache`.
pub trait LayerBoardView {
    fn evaluated(&self, id: ObjectId) -> Option<&EvaluatedObject>;
    fn effective_controller(&self, id: ObjectId) -> Option<PlayerId>;
    fn effective_name(&self, id: ObjectId) -> Option<&str>;
    fn record_read(&self, mask: DirtyMask);
    fn may_have_off_bf_char_grants(&self) -> bool {
        true
    }
    fn off_bf_grantor_ids(&self) -> Option<&[ObjectId]> {
        None
    }
    fn may_have_all_off_bf_colorless(&self) -> bool {
        true
    }
}

/// Board view passed inside [`EvalContext`] to avoid `OnceLock` re-entrancy inside layer work.
#[derive(Clone, Copy, Default)]
pub enum BoardView<'a> {
    #[default]
    Default,
    Cached(&'a LayerCache),
    LayerWork(&'a dyn LayerBoardView),
    BaseOnly,
}

impl fmt::Debug for BoardView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("BoardView::Default"),
            Self::Cached(_) => f.write_str("BoardView::Cached(..)"),
            Self::LayerWork(_) => f.write_str("BoardView::LayerWork(..)"),
            Self::BaseOnly => f.write_str("BoardView::BaseOnly"),
        }
    }
}

impl<'a> BoardView<'a> {
    #[inline]
    pub fn resolve_eval(&self, state: &'a GameState, id: ObjectId) -> Option<&'a EvaluatedObject> {
        match self {
            Self::Default => state.evaluate_board().get(id),
            Self::Cached(cache) => cache.get(id),
            Self::LayerWork(work) => work.evaluated(id),
            Self::BaseOnly => None,
        }
    }

    #[inline]
    pub fn effective_controller(&self, state: &GameState, id: ObjectId) -> Option<PlayerId> {
        match self {
            Self::Default => state.evaluate_board().get(id).map(|e| e.controller),
            Self::Cached(cache) => cache.get(id).map(|e| e.controller),
            Self::LayerWork(work) => work.effective_controller(id),
            Self::BaseOnly => state.object(id).map(|o| o.controller),
        }
    }

    #[inline]
    pub fn effective_name_override<'b>(
        &'b self,
        state: &'b GameState,
        id: ObjectId,
    ) -> Option<&'b str> {
        match self {
            Self::Default => state
                .evaluate_board()
                .get(id)
                .and_then(|e| e.name_override.as_deref()),
            Self::Cached(cache) => cache.get(id).and_then(|e| e.name_override.as_deref()),
            Self::LayerWork(work) => work.effective_name(id),
            Self::BaseOnly => None,
        }
    }

    #[inline]
    pub fn record_read(&self, mask: DirtyMask) {
        if let Self::LayerWork(work) = self {
            work.record_read(mask);
        }
    }
}

/// Evaluation context shared across `filter.rs`, `eval.rs`, `faces.rs`, and `layers/`.
#[derive(Clone, Copy, Debug)]
pub struct EvalContext<'a> {
    pub controller: PlayerId,
    pub source: Option<ObjectId>,
    pub affected: Option<ObjectId>,
    pub stack_item: Option<&'a StackEntry>,
    pub x_value: Option<u32>,
    pub targets: &'a [TargetRef],
    pub targets_by_spec: &'a [&'a [TargetRef]],
    pub event_object: Option<ObjectId>,
    pub event_lki: Option<&'a LkiSnapshot>,
    pub event_player: Option<PlayerId>,
    pub event_amount: Option<i32>,
    pub defending_player: Option<PlayerId>,
    pub chosen_opponent: Option<PlayerId>,
    pub each_loop_player: Option<PlayerId>,
    pub bindings: &'a [ResolutionBinding],
    pub lki_override: Option<&'a LkiSnapshot>,
    pub zone_override: Option<Zone>,
    pub step_acted_ids: &'a [ObjectId],
    pub already_selected_controllers: &'a [PlayerId],
    pub revealed_cost_card: Option<CardDefId>,
    pub sacrificed_cost_lki: &'a [Arc<LkiSnapshot>],
    pub last_effect_amount: i32,
    pub last_excess_damage: i32,
    pub last_counters_removed: i32,
    pub last_roll_result: i32,
    pub coin_heads_count: i32,
    pub counters_removed_this_way: i32,
    pub step_way_stats: Option<&'a StepWayStats>,
    pub board: BoardView<'a>,
    pub depth: u8,
    pub in_off_bf_grants: bool,
}

impl<'a> EvalContext<'a> {
    #[inline]
    pub fn new(controller: PlayerId) -> Self {
        Self {
            controller,
            source: None,
            affected: None,
            stack_item: None,
            x_value: None,
            targets: &[],
            targets_by_spec: &[],
            event_object: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: None,
            chosen_opponent: None,
            each_loop_player: None,
            bindings: &[],
            lki_override: None,
            zone_override: None,
            step_acted_ids: &[],
            already_selected_controllers: &[],
            revealed_cost_card: None,
            sacrificed_cost_lki: &[],
            last_effect_amount: 0,
            last_excess_damage: 0,
            last_counters_removed: 0,
            last_roll_result: 0,
            coin_heads_count: 0,
            counters_removed_this_way: 0,
            step_way_stats: None,
            board: BoardView::Default,
            depth: 0,
            in_off_bf_grants: false,
        }
    }

    #[inline]
    pub fn for_source(controller: PlayerId, source: ObjectId) -> Self {
        let mut ctx = Self::new(controller);
        ctx.source = Some(source);
        ctx
    }

    #[inline]
    pub fn with_board(mut self, board: BoardView<'a>) -> Self {
        self.board = board;
        self
    }

    #[inline]
    pub fn with_stack_item(mut self, stack_item: &'a StackEntry) -> Self {
        self.stack_item = Some(stack_item);
        self
    }

    #[inline]
    pub fn with_zone_override(mut self, zone: Option<Zone>) -> Self {
        self.zone_override = zone;
        self
    }

    #[inline]
    pub fn nested(&self) -> Option<Self> {
        if self.depth >= MAX_EVAL_DEPTH {
            return None;
        }
        Some(Self {
            depth: self.depth + 1,
            ..*self
        })
    }

    #[inline]
    pub fn effective_x(&self) -> u32 {
        self.x_value
            .or_else(|| self.stack_item.map(|s| s.x_value))
            .unwrap_or(0)
    }

    #[inline]
    pub fn effective_targets(&self) -> &'a [TargetRef] {
        if !self.targets.is_empty() {
            self.targets
        } else if let Some(s) = self.stack_item {
            &s.targets
        } else {
            &[]
        }
    }

    #[inline]
    pub fn target_at(&self, index: u16) -> Option<TargetRef> {
        let idx = usize::from(index);
        if !self.targets_by_spec.is_empty() {
            return self
                .targets_by_spec
                .get(idx)
                .and_then(|spec| spec.first().copied());
        }
        if self.targets.is_empty()
            && let Some(s) = self.stack_item
            && !s.choices.targets_by_spec.is_empty()
        {
            return s
                .choices
                .targets_by_spec
                .get(idx)
                .and_then(|spec| spec.first().copied());
        }
        self.effective_targets().get(idx).copied()
    }

    #[inline]
    pub fn source_lki(&self) -> Option<&'a LkiSnapshot> {
        self.lki_override.or_else(|| {
            self.stack_item.and_then(|s| match &s.kind {
                StackEntryKind::Activated { lki, .. } | StackEntryKind::Triggered { lki, .. } => {
                    Some(lki.as_ref())
                }
                StackEntryKind::Spell { .. } => None,
            })
        })
    }
}

/// Off-battlefield effective characteristics including `CR 604.3` CDAs,
/// `StaticBody::OffBattlefieldCharacteristics`, and `RuleMod::AllOffBattlefieldAndSpellsColorless`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffBattlefieldChars {
    pub name: String,
    pub names: SmallVec<[String; 2]>,
    pub supertypes: SupertypeMask,
    pub types: CardTypeMask,
    pub subtypes: SmallVec<[SubtypeId; 4]>,
    pub colors: ColorMask,
    pub mana_cost: ManaCost,
    pub mana_value: u32,
    pub has_all_creature_types: bool,
    pub keywords: SmallVec<[KeywordId; 4]>,
    pub pt: Option<PowerToughness>,
    pub loyalty: Option<i32>,
    pub defense: Option<i32>,
    pub is_pt_cda: bool,
    pub is_face_down: bool,
}

impl OffBattlefieldChars {
    pub fn empty(is_face_down: bool) -> Self {
        Self {
            name: String::new(),
            names: SmallVec::new(),
            supertypes: SupertypeMask::empty(),
            types: CardTypeMask::empty(),
            subtypes: SmallVec::new(),
            colors: ColorMask::empty(),
            mana_cost: ManaCost::default(),
            mana_value: 0,
            has_all_creature_types: false,
            keywords: SmallVec::new(),
            pt: None,
            loyalty: None,
            defense: None,
            is_pt_cda: false,
            is_face_down,
        }
    }

    pub fn has_subtype(&self, db: &CardDatabase, sid: SubtypeId) -> bool {
        if self.subtypes.contains(&sid) {
            return true;
        }
        if self.has_all_creature_types
            && self
                .types
                .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
            && !db.subtype_name(sid).is_some_and(is_non_creature_subtype)
        {
            return true;
        }
        false
    }
}

/// Pre-collected `StaticBody::OffBattlefieldCharacteristics` source passed by `layers/`.
#[derive(Clone, Copy)]
pub(crate) struct OffBfGrantSource<'a> {
    pub source_id: ObjectId,
    pub controller: PlayerId,
    pub static_def: &'a mtg_ir::StaticDef,
}

/// Pre-resolved `CardDatabase` IDs used by `evaluate_off_battlefield_object_with_grants`.
#[derive(Clone, Copy)]
pub(crate) struct OffBfLookupIds {
    pub changeling_kw: Option<KeywordId>,
    pub devoid_kw: Option<KeywordId>,
    pub spacecraft_sid: Option<SubtypeId>,
}

/// Parses non-standard `±M/±N` counter names (excluding `+1/+1` and `-1/-1`).
pub fn parse_non_standard_pt_counter(name: &str) -> Option<PowerToughness> {
    if name == "+1/+1" || name == "-1/-1" {
        return None;
    }
    let (left, right) = name.split_once('/')?;
    if !(left.starts_with('+') || left.starts_with('-'))
        || !(right.starts_with('+') || right.starts_with('-'))
    {
        return None;
    }
    let power = left.parse::<i32>().ok()?;
    let toughness = right.parse::<i32>().ok()?;
    Some(PowerToughness { power, toughness })
}

#[inline]
pub fn compare_i32(lhs: i32, op: CompareOp, rhs: i32) -> bool {
    match op {
        CompareOp::Ge => lhs >= rhs,
        CompareOp::Le => lhs <= rhs,
        CompareOp::Eq => lhs == rhs,
        CompareOp::Gt => lhs > rhs,
        CompareOp::Lt => lhs < rhs,
    }
}

#[inline]
pub fn compare_u32(lhs: u32, op: CompareOp, rhs: i32) -> bool {
    let lhs_i64 = i64::from(lhs);
    let rhs_i64 = i64::from(rhs);
    match op {
        CompareOp::Ge => lhs_i64 >= rhs_i64,
        CompareOp::Le => lhs_i64 <= rhs_i64,
        CompareOp::Eq => lhs_i64 == rhs_i64,
        CompareOp::Gt => lhs_i64 > rhs_i64,
        CompareOp::Lt => lhs_i64 < rhs_i64,
    }
}

#[inline]
pub fn stack_entry_for_object(state: &GameState, id: ObjectId) -> Option<&StackEntry> {
    state
        .stack_entries()
        .iter()
        .find(|e| matches!(e.kind, StackEntryKind::Spell { object } if object == id))
}

#[inline]
pub fn effective_controller_for_object(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> Option<PlayerId> {
    let obj = state.object(id)?;
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    match eff_zone {
        Zone::Battlefield => {
            if matches!(ctx.board, BoardView::BaseOnly) {
                Some(obj.controller)
            } else {
                Some(
                    ctx.board
                        .effective_controller(state, id)
                        .unwrap_or(obj.controller),
                )
            }
        }
        Zone::Stack => {
            ctx.board.record_read(DirtyMask::TOPOLOGY);
            Some(stack_entry_for_object(state, id).map_or(obj.controller, |s| s.controller))
        }
        _ => Some(obj.owner),
    }
}

/// Resolves an object [`Ref`] to a live [`ObjectId`].
pub fn resolve_ref(state: &GameState, r: &Ref, ctx: &EvalContext<'_>) -> Option<ObjectId> {
    match r {
        Ref::SelfObj => ctx.source,
        Ref::Target { index } => match ctx.target_at(*index) {
            Some(TargetRef::Object { id }) => Some(id),
            Some(TargetRef::Stack { id }) => {
                ctx.board.record_read(DirtyMask::TOPOLOGY);
                state
                    .stack_entries()
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| match s.kind {
                        StackEntryKind::Spell { object } => Some(object),
                        _ => None,
                    })
            }
            _ => None,
        },
        Ref::Binding { name } => ctx
            .bindings
            .iter()
            .rev()
            .find(|b| b.name == *name)
            .and_then(|b| b.objects.first().copied()),
        Ref::EventObject => ctx
            .event_object
            .or_else(|| ctx.event_lki.map(|l| l.object_id)),
        Ref::Affected => ctx.affected,
        Ref::AttachedTo => ctx.source.and_then(|s| state.object(s)?.attached_to),
        Ref::LinkedExile => {
            let src = ctx.source?;
            for p in state.players() {
                for &ex in &p.exile {
                    if state.rare(ex).is_some_and(|r| r.exiled_by == Some(src)) {
                        return Some(ex);
                    }
                }
            }
            None
        }
    }
}

/// Resolves an object [`Ref`] to an [`LkiSnapshot`] (CR 608.2h).
pub fn resolve_ref_lki(
    state: &GameState,
    r: &Ref,
    prefer_lki: bool,
    ctx: &EvalContext<'_>,
) -> Option<LkiSnapshot> {
    match r {
        Ref::SelfObj => {
            if prefer_lki {
                if let Some(lki) = ctx.source_lki() {
                    return Some(lki.clone());
                }
                if let Some(id) = ctx.source
                    && let Some(lki) = state.lki(id)
                {
                    return Some(lki.clone());
                }
            }
            if let Some(id) = ctx.source
                && state.object(id).is_some()
            {
                return snapshot_from_live_object(state, id, ctx);
            }
            ctx.source_lki()
                .cloned()
                .or_else(|| ctx.source.and_then(|id| state.lki(id).cloned()))
        }
        Ref::EventObject => {
            if prefer_lki {
                if let Some(lki) = ctx.event_lki {
                    return Some(lki.clone());
                }
                if let Some(id) = ctx.event_object
                    && let Some(lki) = state.lki(id)
                {
                    return Some(lki.clone());
                }
            }
            if let Some(id) = ctx.event_object
                && state.object(id).is_some()
            {
                return snapshot_from_live_object(state, id, ctx);
            }
            ctx.event_lki
                .cloned()
                .or_else(|| ctx.event_object.and_then(|id| state.lki(id).cloned()))
        }
        Ref::Binding { name } => {
            let b = ctx.bindings.iter().rev().find(|b| b.name == *name)?;
            if prefer_lki && let Some(snap) = b.lki.first() {
                return Some((**snap).clone());
            }
            if let Some(&id) = b.objects.first() {
                if state.object(id).is_some() {
                    return snapshot_from_live_object(state, id, ctx);
                }
                if let Some(snap) = state.lki(id) {
                    return Some(snap.clone());
                }
            }
            b.lki.first().map(|s| (**s).clone())
        }
        Ref::Target { index } => match ctx.target_at(*index) {
            Some(TargetRef::Object { id }) => {
                if prefer_lki && let Some(snap) = state.lki(id) {
                    return Some(snap.clone());
                }
                if state.object(id).is_some() {
                    snapshot_from_live_object(state, id, ctx)
                } else {
                    state.lki(id).cloned()
                }
            }
            _ => None,
        },
        Ref::Affected | Ref::AttachedTo | Ref::LinkedExile => {
            let id = resolve_ref(state, r, ctx)?;
            if prefer_lki && let Some(snap) = state.lki(id) {
                return Some(snap.clone());
            }
            if state.object(id).is_some() {
                snapshot_from_live_object(state, id, ctx)
            } else {
                state.lki(id).cloned()
            }
        }
    }
}

fn snapshot_from_live_object(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> Option<LkiSnapshot> {
    let obj = state.object(id)?;
    let rare = state.rare(id);
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    let eff_ctrl = effective_controller_for_object(state, id, ctx).unwrap_or(obj.controller);
    let pt = object_pt(state, id, ctx);
    let mv = object_mana_value(state, id, ctx);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        let name = object_name_in_ctx(state, id, ctx);
        let attached_ids = state
            .players()
            .iter()
            .filter(|p| !p.lost)
            .flat_map(|p| p.battlefield.iter().copied())
            .filter(|&cand| {
                state
                    .object(cand)
                    .is_some_and(|co| co.attached_to == Some(id) && !co.is_phased_out())
            })
            .collect();
        return Some(LkiSnapshot {
            object_id: id,
            card_def: obj.card_def,
            name,
            owner: obj.owner,
            controller: eff_ctrl,
            last_zone: eff_zone,
            pt,
            types: ev.types,
            subtypes: ev.subtypes.clone(),
            supertypes: ev.supertypes,
            colors: ev.colors,
            mana_value: mv,
            keywords: ev.keywords.clone(),
            counters: state.counters(id).iter().copied().collect(),
            damage_marked: obj.damage_marked,
            is_token: obj.is_token(),
            is_commander: obj.is_commander(),
            tapped: obj.flags.contains(ObjectFlags::TAPPED),
            phased_out: obj.is_phased_out(),
            face_down: obj.is_face_down(),
            lost_all_abilities: ev.lost_all_abilities,
            has_all_creature_types: ev.has_all_creature_types,
            attached_ids,
            chosen_color: rare.and_then(|r| r.chosen_color),
            chosen_subtype: rare.and_then(|r| r.chosen_subtype),
            chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
            active_face_index: obj.active_face_index(),
        });
    }

    let chars = off_battlefield_characteristics(state, id, obj, eff_zone, ctx);
    Some(LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name: chars.name,
        owner: obj.owner,
        controller: eff_ctrl,
        last_zone: eff_zone,
        pt,
        types: chars.types,
        subtypes: chars.subtypes,
        supertypes: chars.supertypes,
        colors: chars.colors,
        mana_value: mv,
        keywords: chars.keywords,
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.damage_marked,
        is_token: obj.is_token(),
        is_commander: obj.is_commander(),
        tapped: obj.flags.contains(ObjectFlags::TAPPED),
        phased_out: obj.is_phased_out(),
        face_down: obj.is_face_down(),
        lost_all_abilities: false,
        has_all_creature_types: chars.has_all_creature_types,
        attached_ids: SmallVec::new(),
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.active_face_index(),
    })
}

/// Resolves a [`PlayerRef`] to a [`PlayerId`].
pub fn resolve_player_ref(
    state: &GameState,
    p: &PlayerRef,
    ctx: &EvalContext<'_>,
) -> Option<PlayerId> {
    match p {
        PlayerRef::You => Some(ctx.controller),
        PlayerRef::ControllerOf { of } => {
            if let Ref::Target { index } = &**of
                && let Some(TargetRef::Stack { id }) = ctx.target_at(*index)
            {
                ctx.board.record_read(DirtyMask::TOPOLOGY);
                return state
                    .stack_entries()
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.controller);
            }
            if let Some(id) = resolve_ref(state, of, ctx)
                && let Some(ctrl) = effective_controller_for_object(state, id, ctx)
            {
                return Some(ctrl);
            }
            resolve_ref_lki(state, of, true, ctx).map(|l| l.controller)
        }
        PlayerRef::OwnerOf { of } => {
            if let Ref::Target { index } = &**of
                && let Some(TargetRef::Stack { id }) = ctx.target_at(*index)
            {
                ctx.board.record_read(DirtyMask::TOPOLOGY);
                return state
                    .stack_entries()
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| match &s.kind {
                        StackEntryKind::Spell { object } => state.object(*object).map(|o| o.owner),
                        StackEntryKind::Activated { source, lki, .. }
                        | StackEntryKind::Triggered { source, lki, .. } => {
                            state.object(*source).map(|o| o.owner).or(Some(lki.owner))
                        }
                    });
            }
            if let Some(id) = resolve_ref(state, of, ctx)
                && let Some(obj) = state.object(id)
            {
                return Some(obj.owner);
            }
            resolve_ref_lki(state, of, true, ctx).map(|l| l.owner)
        }
        PlayerRef::Target { index } => match ctx.target_at(*index) {
            Some(TargetRef::Player { id }) => Some(id),
            Some(TargetRef::Object { id }) => effective_controller_for_object(state, id, ctx),
            Some(TargetRef::Stack { id }) => state
                .stack_entries()
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.controller),
            None => None,
        },
        PlayerRef::ActivePlayer => {
            ctx.board.record_read(DirtyMask::TURN_HISTORY);
            Some(state.active_player())
        }
        PlayerRef::EventPlayer => ctx.event_player,
        PlayerRef::DefendingPlayer => {
            ctx.board.record_read(DirtyMask::COMBAT);
            if let Some(dp) = ctx.defending_player {
                return Some(dp);
            }
            let src = ctx.source?;
            for att in &state.combat().attackers {
                if att.attacker == src {
                    return match att.target {
                        crate::action::AttackTarget::Player { id } => Some(id),
                        crate::action::AttackTarget::Planeswalker { id }
                        | crate::action::AttackTarget::Battle { id } => {
                            effective_controller_for_object(state, id, ctx)
                        }
                    };
                }
            }
            None
        }
        PlayerRef::ChosenOpponent => ctx
            .chosen_opponent
            .or_else(|| ctx.source.and_then(|s| state.rare(s)?.chosen_player)),
        PlayerRef::EachLoopPlayer => ctx.each_loop_player,
        PlayerRef::PlayerToYourRight => {
            let players = state.players();
            if players.is_empty() {
                return None;
            }
            let start = usize::from(ctx.controller.0);
            for offset in (1..players.len()).rev() {
                let idx = (start + offset) % players.len();
                if !players[idx].lost {
                    return Some(players[idx].id);
                }
            }
            Some(ctx.controller)
        }
        PlayerRef::ControllerOrControllerOfPlaneswalker { target_index } => {
            match ctx.target_at(*target_index) {
                Some(TargetRef::Player { id }) => Some(id),
                Some(TargetRef::Object { id }) => effective_controller_for_object(state, id, ctx),
                Some(TargetRef::Stack { id }) => state
                    .stack_entries()
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.controller),
                None => None,
            }
        }
    }
}

#[inline]
fn is_player_effect_duration_active(
    state: &GameState,
    source: Option<ObjectId>,
    controller: PlayerId,
    duration: Duration,
    ctx: &EvalContext<'_>,
) -> bool {
    match duration {
        Duration::UntilSourceLeaves => source.is_some_and(|s| {
            state
                .object(s)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
        }),
        Duration::ForAsLongAsYouControlSelf => source.is_some_and(|s| {
            state
                .object(s)
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
                && effective_controller_for_object(state, s, ctx) == Some(controller)
        }),
        Duration::ForAsLongAsSelfRemainsTapped => {
            ctx.board.record_read(DirtyMask::TAPPED);
            source.is_some_and(|s| {
                state.object(s).is_some_and(|o| {
                    o.zone == Zone::Battlefield
                        && !o.is_phased_out()
                        && o.flags.contains(ObjectFlags::TAPPED)
                })
            })
        }
        Duration::EndOfTurn
        | Duration::UntilEndOfCombat
        | Duration::UntilYourNextTurn
        | Duration::UntilEndOfYourNextTurn => {
            ctx.board.record_read(DirtyMask::TURN_HISTORY);
            true
        }
        Duration::Permanent => true,
    }
}

/// Checks if any active non-phased-out battlefield permanent, command emblem, or active
/// player continuous effect grants `RuleMod::AllOffBattlefieldAndSpellsColorless`.
pub fn has_all_off_battlefield_and_spells_colorless(
    state: &GameState,
    ctx: &EvalContext<'_>,
) -> bool {
    match ctx.board {
        BoardView::Default => return state.evaluate_board().all_off_battlefield_colorless,
        BoardView::Cached(cache) => return cache.all_off_battlefield_colorless,
        BoardView::BaseOnly => return false,
        BoardView::LayerWork(work) => {
            if !work.may_have_all_off_bf_colorless() {
                return false;
            }
        }
    }
    let Some(child_ctx) = ctx.nested() else {
        return false;
    };
    ctx.board
        .record_read(DirtyMask::TOPOLOGY | DirtyMask::PLAYER_STATE);
    let db = state.db();

    let rule_matches_any_living =
        |who: &mtg_ir::PlayerFilter, rule_ctx: &EvalContext<'_>| -> bool {
            state
                .players()
                .iter()
                .any(|pl| !pl.lost && matches_player_filter(state, pl.id, who, rule_ctx))
        };

    for p in state.players() {
        if p.lost {
            continue;
        }
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() || obj.is_face_down() {
                continue;
            }
            if ctx
                .board
                .resolve_eval(state, id)
                .is_some_and(|e| e.lost_all_abilities)
            {
                continue;
            }
            let opts = face_opts_for_game_object(obj, state.rare(id), None);
            let Some(resolved) = resolve_object_faces(db, obj.card_def, &opts) else {
                continue;
            };
            let ctrl =
                effective_controller_for_object(state, id, &child_ctx).unwrap_or(obj.controller);
            let perm_ctx = EvalContext {
                controller: ctrl,
                source: Some(id),
                board: ctx.board,
                in_off_bf_grants: true,
                ..child_ctx
            };
            let mut found = false;
            for_each_active_ability_on_faces(
                db,
                obj.card_def,
                &opts,
                &resolved.active_face_indices,
                |cond| evaluate_condition(state, cond, &perm_ctx),
                |ab| {
                    if !found
                        && let AbilityDef::Static(st) = ab.ability
                        && let StaticBody::RuleModifier {
                            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            who,
                        } = &st.static_def.body
                        && st
                            .static_def
                            .condition
                            .as_deref()
                            .is_none_or(|c| evaluate_condition(state, c, &perm_ctx))
                        && rule_matches_any_living(who, &perm_ctx)
                    {
                        found = true;
                    }
                },
            );
            if found {
                return true;
            }
        }
        for &id in &p.command {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if !obj.flags.contains(ObjectFlags::IS_EMBLEM) || obj.is_face_down() {
                continue;
            }
            let opts = face_opts_for_game_object(obj, state.rare(id), None);
            let Some(resolved) = resolve_object_faces(db, obj.card_def, &opts) else {
                continue;
            };
            let perm_ctx = EvalContext {
                controller: obj.owner,
                source: Some(id),
                board: ctx.board,
                in_off_bf_grants: true,
                ..child_ctx
            };
            let mut found = false;
            for_each_active_ability_on_faces(
                db,
                obj.card_def,
                &opts,
                &resolved.active_face_indices,
                |_| false,
                |ab| {
                    if !found
                        && let AbilityDef::Static(st) = ab.ability
                        && let StaticBody::RuleModifier {
                            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            who,
                        } = &st.static_def.body
                        && st
                            .static_def
                            .condition
                            .as_deref()
                            .is_none_or(|c| evaluate_condition(state, c, &perm_ctx))
                        && rule_matches_any_living(who, &perm_ctx)
                    {
                        found = true;
                    }
                },
            );
            if found {
                return true;
            }
        }
    }

    for pe in state.player_continuous_effects() {
        if matches!(pe.rule, RuleMod::AllOffBattlefieldAndSpellsColorless)
            && is_player_effect_duration_active(
                state,
                pe.source,
                pe.controller,
                pe.duration,
                &child_ctx,
            )
        {
            let pe_ctx = EvalContext {
                controller: pe.controller,
                source: pe.source,
                board: ctx.board,
                in_off_bf_grants: true,
                ..child_ctx
            };
            if rule_matches_any_living(&pe.who, &pe_ctx) {
                return true;
            }
        }
    }

    false
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn apply_layer4_self_cda(
    modifier: &LayerMod,
    types: &mut CardTypeMask,
    subtypes: &mut SmallVec<[SubtypeId; 4]>,
    has_all_creature_types: &mut bool,
) {
    match modifier {
        LayerMod::AllCreatureTypes => {
            *has_all_creature_types = true;
        }
        LayerMod::AddTypes {
            types: add_t,
            subtypes: add_s,
        } => {
            *types |= *add_t;
            for &st_id in add_s {
                if !subtypes.contains(&st_id) {
                    subtypes.push(st_id);
                }
            }
        }
        LayerMod::SetTypes {
            types: set_t,
            subtypes: set_s,
        } => {
            *types = *set_t;
            *subtypes = set_s.iter().copied().collect();
        }
        LayerMod::SetSubtypes { subtypes: set_s } => {
            *subtypes = set_s.iter().copied().collect();
        }
        _ => {}
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn apply_single_off_bf_grant(
    state: &GameState,
    id: ObjectId,
    eff_zone: Zone,
    obj_ctrl_or_owner: PlayerId,
    grant: &OffBfGrantSource<'_>,
    types: &mut CardTypeMask,
    subtypes: &mut SmallVec<[SubtypeId; 4]>,
    colors: &mut ColorMask,
    has_all_creature_types: &mut bool,
    ctx: &EvalContext<'_>,
) {
    let StaticBody::OffBattlefieldCharacteristics {
        zones,
        filter,
        all_players,
        add_all_creature_types,
        add_chosen_subtype,
        add_types,
        add_subtypes,
        add_chosen_color,
        add_colors,
    } = &grant.static_def.body
    else {
        return;
    };
    let zone_bit = ZoneMask::from_zone(eff_zone);
    if !zones.is_empty() && !zones.contains(zone_bit) {
        return;
    }
    if !*all_players && grant.controller != obj_ctrl_or_owner {
        return;
    }
    if let Some(f) = filter.as_deref() {
        let filter_ctx = EvalContext {
            controller: grant.controller,
            source: Some(grant.source_id),
            board: BoardView::BaseOnly,
            zone_override: Some(eff_zone),
            in_off_bf_grants: true,
            ..*ctx
        };
        if !matches_object_filter(state, id, f, &filter_ctx) {
            return;
        }
    }
    *types |= *add_types;
    for &sid in add_subtypes {
        if !subtypes.contains(&sid) {
            subtypes.push(sid);
        }
    }
    if *add_all_creature_types {
        *has_all_creature_types = true;
    }
    if *add_chosen_subtype {
        ctx.board.record_read(DirtyMask::DESIGNATIONS);
        if let Some(chosen_st) = state.rare(grant.source_id).and_then(|r| r.chosen_subtype)
            && !subtypes.contains(&chosen_st)
        {
            subtypes.push(chosen_st);
        }
    }
    *colors |= *add_colors & ColorMask::ALL_COLORS;
    if *add_chosen_color {
        ctx.board.record_read(DirtyMask::DESIGNATIONS);
        if let Some(chosen_col) = state.rare(grant.source_id).and_then(|r| r.chosen_color) {
            *colors |= chosen_col & ColorMask::ALL_COLORS;
        }
    }
}

#[inline]
fn apply_layer7a_self_cda(
    state: &GameState,
    st: &mtg_ir::StaticDef,
    base: &mut PowerToughness,
    cda_ctx: &EvalContext<'_>,
) {
    if let StaticBody::Continuous {
        layer: Layer::Layer7a,
        affected: Affected::SelfObj,
        modifier,
        active_in_zone: None,
    } = &st.body
        && st
            .condition
            .as_deref()
            .is_none_or(|c| evaluate_condition(state, c, cda_ctx))
    {
        match modifier {
            LayerMod::SetBasePt { power, toughness } => {
                base.power = evaluate_amount(state, power, cda_ctx);
                base.toughness = evaluate_amount(state, toughness, cda_ctx);
            }
            LayerMod::SetBasePower { power } => {
                base.power = evaluate_amount(state, power, cda_ctx);
            }
            LayerMod::SetBaseToughness { toughness } => {
                base.toughness = evaluate_amount(state, toughness, cda_ctx);
            }
            _ => {}
        }
    }
}

#[inline]
fn apply_pt_counters_and_temp(
    state: &GameState,
    id: ObjectId,
    rare: Option<&crate::state::RareObjectState>,
    base: &mut PowerToughness,
) {
    let db = state.db();
    for &(cid, count) in state.counters(id) {
        if count == 0 {
            continue;
        }
        let n = count as i32;
        if let Some(cname) = db.counter_name(cid) {
            if cname == "+1/+1" {
                base.power += n;
                base.toughness += n;
            } else if cname == "-1/-1" {
                base.power -= n;
                base.toughness -= n;
            } else if let Some(delta) = parse_non_standard_pt_counter(cname) {
                base.power += delta.power * n;
                base.toughness += delta.toughness * n;
            }
        }
    }
    if let Some(r) = rare {
        base.power += r.temp_power_mod;
        base.toughness += r.temp_toughness_mod;
    }
}

/// Evaluates an off-battlefield (or phased-out/lost-player battlefield) object for `LayerCache`
/// using pre-collected `OffBfGrantSource` entries and pre-resolved keyword/subtype IDs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_off_battlefield_object_with_grants(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    grants: &[OffBfGrantSource<'_>],
    all_off_bf_colorless: bool,
    ids: OffBfLookupIds,
    ctx: &EvalContext<'_>,
    collect_notes: bool,
) -> (EvaluatedObject, Vec<String>) {
    let db = state.db();

    // Branch 1: Zero-alloc direct CardDef fast-path for unmodified off-battlefield cards.
    if grants.is_empty()
        && !collect_notes
        && !obj.has_rare()
        && !obj.has_counters()
        && !obj.is_face_down()
        && obj.zone != Zone::Battlefield
        && obj.zone != Zone::Stack
        && obj.card_def != TOKEN_CARD_DEF_ID
        && let Some(card) = db.get(obj.card_def)
        && !card.faces.is_empty()
        && card.layout != CardLayout::Split
    {
        let face = &card.faces[0];
        let ch = &face.characteristics;
        let non_creature_spacecraft = !ch.types.contains(CardTypeMask::CREATURE)
            && ids
                .spacecraft_sid
                .is_some_and(|sid| ch.subtypes.contains(&sid));
        let printed_pt = if non_creature_spacecraft {
            None
        } else {
            ch.pt.map(|p| match p {
                PrintedPt::Fixed { power, toughness } => PowerToughness { power, toughness },
                PrintedPt::Cda { power, toughness } => PowerToughness {
                    power: power.unwrap_or(0),
                    toughness: toughness.unwrap_or(0),
                },
            })
        };

        let mut types = ch.types;
        let mut subtypes: SmallVec<[SubtypeId; 4]> = ch.subtypes.iter().copied().collect();
        let mut colors =
            (ch.colors | ch.color_indicator.unwrap_or(ColorMask::empty())) & ColorMask::ALL_COLORS;
        let mut keywords: SmallVec<[KeywordId; 4]> = SmallVec::new();
        let mut has_all_creature_types = false;
        let mut has_l7a_cda = false;

        for ab in &face.abilities {
            match ab {
                AbilityDef::Keyword(kw) => {
                    let kid = kw.keyword.keyword;
                    if !keywords.contains(&kid) {
                        keywords.push(kid);
                    }
                    if ids.devoid_kw == Some(kid)
                        || (ids.devoid_kw.is_none()
                            && db
                                .keyword_name(kid)
                                .is_some_and(|n| n.len() == 6 && n.eq_ignore_ascii_case("devoid")))
                    {
                        colors = ColorMask::empty();
                    }
                    if ids.changeling_kw == Some(kid)
                        || (ids.changeling_kw.is_none()
                            && db.keyword_name(kid).is_some_and(|n| {
                                n.len() == 10 && n.eq_ignore_ascii_case("changeling")
                            }))
                    {
                        has_all_creature_types = true;
                    }
                }
                AbilityDef::Static(st) => {
                    if let StaticBody::Continuous {
                        layer,
                        affected: Affected::SelfObj,
                        modifier,
                        active_in_zone: None,
                    } = &st.static_def.body
                    {
                        match layer {
                            Layer::Layer5 if st.static_def.condition.is_none() => {
                                if let LayerMod::SetColors { colors: c } = modifier {
                                    colors = *c & ColorMask::ALL_COLORS;
                                }
                            }
                            Layer::Layer4 if st.static_def.condition.is_none() => {
                                apply_layer4_self_cda(
                                    modifier,
                                    &mut types,
                                    &mut subtypes,
                                    &mut has_all_creature_types,
                                );
                            }
                            Layer::Layer7a => {
                                has_l7a_cda = true;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        let pt = if !types.contains(CardTypeMask::CREATURE) && printed_pt.is_none() {
            None
        } else {
            let mut base = printed_pt.unwrap_or(PowerToughness {
                power: 0,
                toughness: 0,
            });
            if has_l7a_cda && let Some(child_ctx) = ctx.nested() {
                let cda_ctx = EvalContext {
                    controller: obj.owner,
                    source: Some(id),
                    ..child_ctx
                };
                for ab in &face.abilities {
                    if let AbilityDef::Static(st) = ab {
                        apply_layer7a_self_cda(state, &st.static_def, &mut base, &cda_ctx);
                    }
                }
            }
            Some(base)
        };

        if all_off_bf_colorless {
            colors = ColorMask::empty();
        }

        return (
            EvaluatedObject {
                pt,
                base_pt: pt,
                types,
                subtypes,
                supertypes: ch.supertypes,
                colors,
                mana_value: ch.mana_cost.mana_value(),
                keywords,
                has_all_creature_types,
                lost_all_abilities: false,
                loyalty: ch.loyalty,
                defense: ch.defense,
                controller: obj.owner,
                name_override: None,
                mana_cost: ch.mana_cost.clone(),
            },
            Vec::new(),
        );
    }

    // Branch 2: Single-pass general fallback (Stack, Split, rare/counters/face-down, grants, phased-out BF).
    let eff_zone = obj.zone;
    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let eff_ctrl = match eff_zone {
        Zone::Battlefield => obj.controller,
        Zone::Stack => stack_entry.map_or(obj.controller, |s| s.controller),
        _ => obj.owner,
    };

    if !obj.is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        let (_nm, mut ev) = evaluate_token_spec_base(spec, copiable);
        ev.controller = eff_ctrl;
        if eff_zone != Zone::Battlefield && !grants.is_empty() {
            for g in grants {
                apply_single_off_bf_grant(
                    state,
                    id,
                    eff_zone,
                    eff_ctrl,
                    g,
                    &mut ev.types,
                    &mut ev.subtypes,
                    &mut ev.colors,
                    &mut ev.has_all_creature_types,
                    ctx,
                );
            }
            if ev.pt.is_none() && ev.types.contains(CardTypeMask::CREATURE) {
                ev.pt = Some(PowerToughness {
                    power: spec.power,
                    toughness: spec.toughness,
                });
                ev.base_pt = ev.pt;
            }
        }
        if let Some(mut pt) = ev.pt {
            apply_pt_counters_and_temp(state, id, rare, &mut pt);
            ev.pt = Some(pt);
        }
        let mut notes = Vec::new();
        if eff_zone != Zone::Battlefield && all_off_bf_colorless {
            ev.colors = ColorMask::empty();
            if collect_notes {
                notes.push("CR 613.11: forced colorless off-battlefield".into());
            }
        }
        return (ev, notes);
    }

    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = eff_zone;

    let Some(resolved) = resolve_object_faces(db, obj.card_def, &opts) else {
        return (
            EvaluatedObject {
                pt: None,
                base_pt: None,
                types: CardTypeMask::empty(),
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors: ColorMask::empty(),
                mana_value: 0,
                keywords: SmallVec::new(),
                has_all_creature_types: false,
                lost_all_abilities: false,
                loyalty: None,
                defense: None,
                controller: eff_ctrl,
                name_override: None,
                mana_cost: ManaCost::default(),
            },
            Vec::new(),
        );
    };

    let mut types = resolved.types;
    let mut subtypes = resolved.subtypes;
    let supertypes = resolved.supertypes;
    let mut colors = resolved.colors;
    let mana_cost = resolved.mana_cost;
    let mut mv = resolved.mana_value;
    if eff_zone == Zone::Stack {
        let x_val = stack_entry
            .map(|s| s.x_value)
            .or_else(|| rare.and_then(|r| r.entered_with_x))
            .unwrap_or(0);
        if x_val > 0 && mana_cost.x_count > 0 {
            mv = mv.saturating_add(x_val.saturating_mul(u32::from(mana_cost.x_count)));
        }
    }
    let mut keywords: SmallVec<[KeywordId; 4]> = resolved.extra_keywords.into_iter().collect();
    let mut has_all_creature_types = false;
    let mut base_pt = resolved.pt;
    let mut l7a_statics: SmallVec<[&mtg_ir::StaticDef; 2]> = SmallVec::new();

    if !resolved.is_face_down {
        let mut has_pregame_color_choice = false;
        for_each_active_ability_on_faces(
            db,
            obj.card_def,
            &opts,
            &resolved.active_face_indices,
            |_| eff_zone == Zone::Battlefield,
            |ab| match ab.ability {
                AbilityDef::Keyword(kw) => {
                    let kid = kw.keyword.keyword;
                    if !keywords.contains(&kid) {
                        keywords.push(kid);
                    }
                    if ids.changeling_kw == Some(kid)
                        || (ids.changeling_kw.is_none()
                            && db.keyword_name(kid).is_some_and(|n| {
                                n.len() == 10 && n.eq_ignore_ascii_case("changeling")
                            }))
                    {
                        has_all_creature_types = true;
                    }
                }
                AbilityDef::Static(st) => {
                    if matches!(st.static_def.body, StaticBody::CommanderPreGameColorChoice) {
                        has_pregame_color_choice = true;
                    }
                    if let StaticBody::Continuous {
                        layer,
                        affected: Affected::SelfObj,
                        modifier,
                        active_in_zone: None,
                    } = &st.static_def.body
                    {
                        match layer {
                            Layer::Layer4 if st.static_def.condition.is_none() => {
                                apply_layer4_self_cda(
                                    modifier,
                                    &mut types,
                                    &mut subtypes,
                                    &mut has_all_creature_types,
                                );
                            }
                            Layer::Layer7a => {
                                l7a_statics.push(&st.static_def);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
        );

        if let Some(r) = rare {
            for &kw in &r.granted_keywords {
                if !keywords.contains(&kw) {
                    keywords.push(kw);
                }
            }
            if obj.is_commander()
                && has_pregame_color_choice
                && let Some(chosen_c) = r.chosen_color
            {
                colors = chosen_c & ColorMask::ALL_COLORS;
            }
        }

        if eff_zone != Zone::Battlefield && !grants.is_empty() {
            for g in grants {
                apply_single_off_bf_grant(
                    state,
                    id,
                    eff_zone,
                    eff_ctrl,
                    g,
                    &mut types,
                    &mut subtypes,
                    &mut colors,
                    &mut has_all_creature_types,
                    ctx,
                );
            }
        }
    }

    let has_pt_except = copiable
        .and_then(|c| c.except.as_ref())
        .is_some_and(|e| e.pt.is_some());

    let (final_base_pt, final_pt) = if !types.contains(CardTypeMask::CREATURE) && base_pt.is_none()
    {
        (None, None)
    } else {
        let mut base = base_pt.unwrap_or(PowerToughness {
            power: 0,
            toughness: 0,
        });
        if !resolved.is_face_down
            && !has_pt_except
            && !l7a_statics.is_empty()
            && let Some(child_ctx) = ctx.nested()
        {
            let cda_ctx = EvalContext {
                controller: eff_ctrl,
                source: Some(id),
                ..child_ctx
            };
            for st in l7a_statics {
                apply_layer7a_self_cda(state, st, &mut base, &cda_ctx);
            }
        }
        base_pt = Some(base);
        let mut cur = base;
        apply_pt_counters_and_temp(state, id, rare, &mut cur);
        (base_pt, Some(cur))
    };

    let mut notes = Vec::new();
    if collect_notes && has_all_creature_types {
        notes.push("Off-battlefield CR 604.3: all creature types".into());
    }
    if eff_zone != Zone::Battlefield && all_off_bf_colorless {
        colors = ColorMask::empty();
        if collect_notes {
            notes.push("CR 613.11: forced colorless off-battlefield".into());
        }
    }

    (
        EvaluatedObject {
            pt: final_pt,
            base_pt: final_base_pt,
            types,
            subtypes,
            supertypes,
            colors,
            mana_value: mv,
            keywords,
            has_all_creature_types,
            lost_all_abilities: false,
            loyalty: resolved.loyalty,
            defense: resolved.defense,
            controller: eff_ctrl,
            name_override: None,
            mana_cost,
        },
        notes,
    )
}

/// Resolves off-battlefield characteristics for `id` in `eff_zone` (`CR 604.3` CDAs,
/// `OffBattlefieldCharacteristics`, and `RuleMod::AllOffBattlefieldAndSpellsColorless`).
pub fn off_battlefield_characteristics(
    state: &GameState,
    id: ObjectId,
    obj: &GameObject,
    eff_zone: Zone,
    ctx: &EvalContext<'_>,
) -> OffBattlefieldChars {
    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let db = state.db();

    // TokenSpec tokens not copied from a real card def.
    if !obj.is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        let (name, ev) = evaluate_token_spec_base(spec, copiable);
        let mut names = SmallVec::new();
        if !name.is_empty() {
            names.push(name.clone());
        }
        return OffBattlefieldChars {
            name,
            names,
            supertypes: ev.supertypes,
            types: ev.types,
            subtypes: ev.subtypes,
            colors: ev.colors,
            mana_cost: ManaCost::default(),
            mana_value: ev.mana_value,
            has_all_creature_types: false,
            keywords: ev.keywords,
            pt: ev.pt,
            loyalty: None,
            defense: None,
            is_pt_cda: false,
            is_face_down: false,
        };
    }

    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = eff_zone;

    let Some(resolved) = resolve_object_faces(db, obj.card_def, &opts) else {
        return OffBattlefieldChars::empty(obj.is_face_down());
    };

    let mut out = OffBattlefieldChars {
        name: resolved.name,
        names: resolved.names,
        supertypes: resolved.supertypes,
        types: resolved.types,
        subtypes: resolved.subtypes,
        colors: resolved.colors,
        mana_cost: resolved.mana_cost,
        mana_value: resolved.mana_value,
        has_all_creature_types: false,
        keywords: resolved.extra_keywords.into_iter().collect(),
        pt: resolved.pt,
        loyalty: resolved.loyalty,
        defense: resolved.defense,
        is_pt_cda: resolved.is_pt_cda,
        is_face_down: resolved.is_face_down,
    };

    if out.is_face_down {
        return out;
    }

    let mut has_pregame_color_choice = false;
    let changeling_kw = db.keyword_id("changeling");
    // Off-battlefield abilities: grant_condition abilities are stripped off-battlefield (CR 113.6).
    for_each_active_ability_on_faces(
        db,
        obj.card_def,
        &opts,
        &resolved.active_face_indices,
        |_| eff_zone == Zone::Battlefield,
        |ab| match ab.ability {
            AbilityDef::Keyword(kw) => {
                let kid = kw.keyword.keyword;
                if !out.keywords.contains(&kid) {
                    out.keywords.push(kid);
                }
                if changeling_kw == Some(kid)
                    || (changeling_kw.is_none()
                        && db
                            .keyword_name(kid)
                            .is_some_and(|n| n.eq_ignore_ascii_case("changeling")))
                {
                    out.has_all_creature_types = true;
                }
            }
            AbilityDef::Static(st) => {
                if matches!(st.static_def.body, StaticBody::CommanderPreGameColorChoice) {
                    has_pregame_color_choice = true;
                }
                // CR 604.3: Unconditional Layer 4 self CDAs apply in all zones.
                if st.static_def.condition.is_none()
                    && let StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::SelfObj,
                        modifier,
                        active_in_zone: None,
                    } = &st.static_def.body
                {
                    apply_layer4_self_cda(
                        modifier,
                        &mut out.types,
                        &mut out.subtypes,
                        &mut out.has_all_creature_types,
                    );
                }
            }
            _ => {}
        },
    );

    if let Some(r) = rare {
        for &kw in &r.granted_keywords {
            if !out.keywords.contains(&kw) {
                out.keywords.push(kw);
            }
        }
        // CR 903.4 pre-game chosen color for commanders with CommanderPreGameColorChoice
        if obj.is_commander()
            && has_pregame_color_choice
            && let Some(chosen_c) = r.chosen_color
        {
            out.colors = chosen_c & ColorMask::ALL_COLORS;
        }
    }

    // StaticBody::OffBattlefieldCharacteristics grants from battlefield permanents and command emblems
    let default_cache = if eff_zone != Zone::Battlefield
        && !ctx.in_off_bf_grants
        && matches!(ctx.board, BoardView::Default)
    {
        Some(state.evaluate_board())
    } else {
        None
    };
    let eff_board = match default_cache {
        Some(cache) => BoardView::Cached(cache),
        None => ctx.board,
    };
    let (skip_off_bf_scan, grantor_ids): (bool, Option<&[ObjectId]>) = match eff_board {
        BoardView::BaseOnly => (true, None),
        BoardView::LayerWork(work) => (
            !work.may_have_off_bf_char_grants(),
            work.off_bf_grantor_ids(),
        ),
        BoardView::Cached(cache) => (
            !cache.may_have_off_bf_char_grants || cache.off_bf_grantor_ids.is_empty(),
            Some(cache.off_bf_grantor_ids.as_slice()),
        ),
        BoardView::Default => unreachable!("BoardView::Default resolved to BoardView::Cached"),
    };
    if eff_zone != Zone::Battlefield
        && !ctx.in_off_bf_grants
        && !skip_off_bf_scan
        && let Some(child_ctx) = ctx.nested()
    {
        let child_ctx = EvalContext {
            board: eff_board,
            ..child_ctx
        };
        let obj_ctrl_or_owner = if eff_zone == Zone::Stack {
            stack_entry.map_or(obj.controller, |s| s.controller)
        } else {
            obj.owner
        };
        eff_board.record_read(DirtyMask::TOPOLOGY);

        #[allow(clippy::too_many_arguments)]
        fn apply_off_bf_grants_from_perm(
            state: &GameState,
            db: &CardDatabase,
            id: ObjectId,
            eff_zone: Zone,
            obj_ctrl_or_owner: PlayerId,
            perm_id: ObjectId,
            child_ctx: &EvalContext<'_>,
            out_types: &mut CardTypeMask,
            out_subtypes: &mut SmallVec<[SubtypeId; 4]>,
            out_colors: &mut ColorMask,
            out_all_ct: &mut bool,
        ) {
            let Some(perm_obj) = state.object(perm_id) else {
                return;
            };
            if perm_obj.is_phased_out() || perm_obj.is_face_down() {
                return;
            }
            if child_ctx
                .board
                .resolve_eval(state, perm_id)
                .is_some_and(|e| e.lost_all_abilities)
            {
                return;
            }
            let perm_ctrl = child_ctx
                .board
                .effective_controller(state, perm_id)
                .unwrap_or(perm_obj.controller);
            let perm_opts = face_opts_for_game_object(perm_obj, state.rare(perm_id), None);
            let Some(perm_resolved) = resolve_object_faces(db, perm_obj.card_def, &perm_opts)
            else {
                return;
            };
            let perm_ctx = EvalContext {
                controller: perm_ctrl,
                source: Some(perm_id),
                board: BoardView::BaseOnly,
                in_off_bf_grants: true,
                ..*child_ctx
            };
            for_each_active_ability_on_faces(
                db,
                perm_obj.card_def,
                &perm_opts,
                &perm_resolved.active_face_indices,
                |cond| evaluate_condition(state, cond, &perm_ctx),
                |ab| {
                    let AbilityDef::Static(st) = ab.ability else {
                        return;
                    };
                    if !matches!(
                        st.static_def.body,
                        StaticBody::OffBattlefieldCharacteristics { .. }
                    ) {
                        return;
                    }
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &perm_ctx)
                    {
                        return;
                    }
                    let grant = OffBfGrantSource {
                        source_id: perm_id,
                        controller: perm_ctrl,
                        static_def: &st.static_def,
                    };
                    apply_single_off_bf_grant(
                        state,
                        id,
                        eff_zone,
                        obj_ctrl_or_owner,
                        &grant,
                        out_types,
                        out_subtypes,
                        out_colors,
                        out_all_ct,
                        child_ctx,
                    );
                },
            );
        }

        #[allow(clippy::too_many_arguments)]
        fn apply_off_bf_grants_from_emblem(
            state: &GameState,
            db: &CardDatabase,
            id: ObjectId,
            eff_zone: Zone,
            obj_ctrl_or_owner: PlayerId,
            emblem_id: ObjectId,
            child_ctx: &EvalContext<'_>,
            out_types: &mut CardTypeMask,
            out_subtypes: &mut SmallVec<[SubtypeId; 4]>,
            out_colors: &mut ColorMask,
            out_all_ct: &mut bool,
        ) {
            let Some(emblem_obj) = state.object(emblem_id) else {
                return;
            };
            if !emblem_obj.flags.contains(ObjectFlags::IS_EMBLEM) || emblem_obj.is_face_down() {
                return;
            }
            let emb_opts = face_opts_for_game_object(emblem_obj, state.rare(emblem_id), None);
            let Some(emb_resolved) = resolve_object_faces(db, emblem_obj.card_def, &emb_opts)
            else {
                return;
            };
            let emb_ctrl = emblem_obj.owner;
            let emb_ctx = EvalContext {
                controller: emb_ctrl,
                source: Some(emblem_id),
                board: BoardView::BaseOnly,
                in_off_bf_grants: true,
                ..*child_ctx
            };
            for_each_active_ability_on_faces(
                db,
                emblem_obj.card_def,
                &emb_opts,
                &emb_resolved.active_face_indices,
                |_| false,
                |ab| {
                    let AbilityDef::Static(st) = ab.ability else {
                        return;
                    };
                    if !matches!(
                        st.static_def.body,
                        StaticBody::OffBattlefieldCharacteristics { .. }
                    ) {
                        return;
                    }
                    if let Some(cond) = st.static_def.condition.as_deref()
                        && !evaluate_condition(state, cond, &emb_ctx)
                    {
                        return;
                    }
                    let grant = OffBfGrantSource {
                        source_id: emblem_id,
                        controller: emb_ctrl,
                        static_def: &st.static_def,
                    };
                    apply_single_off_bf_grant(
                        state,
                        id,
                        eff_zone,
                        obj_ctrl_or_owner,
                        &grant,
                        out_types,
                        out_subtypes,
                        out_colors,
                        out_all_ct,
                        child_ctx,
                    );
                },
            );
        }

        if let Some(ids) = grantor_ids {
            for &gid in ids {
                let Some(g_obj) = state.object(gid) else {
                    continue;
                };
                match g_obj.zone {
                    Zone::Battlefield => {
                        if state.player(g_obj.controller).is_some_and(|p| p.lost) {
                            continue;
                        }
                        apply_off_bf_grants_from_perm(
                            state,
                            db,
                            id,
                            eff_zone,
                            obj_ctrl_or_owner,
                            gid,
                            &child_ctx,
                            &mut out.types,
                            &mut out.subtypes,
                            &mut out.colors,
                            &mut out.has_all_creature_types,
                        );
                    }
                    Zone::Command => {
                        if state.player(g_obj.owner).is_some_and(|p| p.lost) {
                            continue;
                        }
                        apply_off_bf_grants_from_emblem(
                            state,
                            db,
                            id,
                            eff_zone,
                            obj_ctrl_or_owner,
                            gid,
                            &child_ctx,
                            &mut out.types,
                            &mut out.subtypes,
                            &mut out.colors,
                            &mut out.has_all_creature_types,
                        );
                    }
                    _ => {}
                }
            }
        } else {
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &perm_id in &p.battlefield {
                    apply_off_bf_grants_from_perm(
                        state,
                        db,
                        id,
                        eff_zone,
                        obj_ctrl_or_owner,
                        perm_id,
                        &child_ctx,
                        &mut out.types,
                        &mut out.subtypes,
                        &mut out.colors,
                        &mut out.has_all_creature_types,
                    );
                }
                for &emblem_id in &p.command {
                    apply_off_bf_grants_from_emblem(
                        state,
                        db,
                        id,
                        eff_zone,
                        obj_ctrl_or_owner,
                        emblem_id,
                        &child_ctx,
                        &mut out.types,
                        &mut out.subtypes,
                        &mut out.colors,
                        &mut out.has_all_creature_types,
                    );
                }
            }
        }
    }

    if eff_zone != Zone::Battlefield
        && !matches!(eff_board, BoardView::BaseOnly)
        && has_all_off_battlefield_and_spells_colorless(
            state,
            &EvalContext {
                board: eff_board,
                ..*ctx
            },
        )
    {
        out.colors = ColorMask::empty();
    }

    out
}

/// Evaluates the signed unclamped P/T of `id` in any zone (`CR 107.1b`, `CR 208.2a`, `CR 208.3`, `CR 604.3`).
pub fn object_pt(state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> Option<PowerToughness> {
    object_pt_inner(state, id, ctx, true)
}

/// Evaluates the signed base P/T of `id` (after Layer 7a CDAs, before Layer 7c counters and temporary mods).
pub fn object_base_pt(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> Option<PowerToughness> {
    object_pt_inner(state, id, ctx, false)
}

fn object_pt_inner(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
    apply_7c: bool,
) -> Option<PowerToughness> {
    let obj = state.object(id)?;
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        return if apply_7c { ev.pt } else { ev.base_pt };
    }

    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let db = state.db();

    if !obj.is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        let (_name, ev) = evaluate_token_spec_base(spec, copiable);
        let mut pt = ev.pt?;
        if apply_7c {
            apply_pt_counters_and_temp(state, id, rare, &mut pt);
        }
        return Some(pt);
    }

    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = eff_zone;

    let resolved = resolve_object_faces(db, obj.card_def, &opts)?;
    let mut types = resolved.types;
    let mut subtypes = resolved.subtypes;
    let mut has_all_creature_types = false;
    let mut l7a_statics: SmallVec<[&mtg_ir::StaticDef; 2]> = SmallVec::new();

    if !resolved.is_face_down {
        for_each_active_ability_on_faces(
            db,
            obj.card_def,
            &opts,
            &resolved.active_face_indices,
            |_| eff_zone == Zone::Battlefield,
            |ab| {
                if let AbilityDef::Static(st) = ab.ability
                    && let StaticBody::Continuous {
                        layer,
                        affected: Affected::SelfObj,
                        modifier,
                        active_in_zone: None,
                    } = &st.static_def.body
                {
                    match layer {
                        Layer::Layer4 if st.static_def.condition.is_none() => {
                            apply_layer4_self_cda(
                                modifier,
                                &mut types,
                                &mut subtypes,
                                &mut has_all_creature_types,
                            );
                        }
                        Layer::Layer7a => {
                            l7a_statics.push(&st.static_def);
                        }
                        _ => {}
                    }
                }
            },
        );
    }

    // Apply OffBattlefieldCharacteristics grants only if non-battlefield and non-creature might gain CREATURE.
    if eff_zone != Zone::Battlefield
        && !resolved.is_face_down
        && !types.contains(CardTypeMask::CREATURE)
        && resolved.pt.is_none()
        && !ctx.in_off_bf_grants
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(child_ctx) = ctx.nested()
    {
        let chars = off_battlefield_characteristics(state, id, obj, eff_zone, &child_ctx);
        types = chars.types;
    }

    if !types.contains(CardTypeMask::CREATURE) && resolved.pt.is_none() {
        return None;
    }

    let has_pt_except = copiable
        .and_then(|c| c.except.as_ref())
        .is_some_and(|e| e.pt.is_some());

    let mut base = resolved.pt.unwrap_or(PowerToughness {
        power: 0,
        toughness: 0,
    });

    // CR 604.3 / 208.2a / 707.9d: Layer 7a */* CDAs in all zones unless overridden by copy except.
    if !resolved.is_face_down
        && !has_pt_except
        && !l7a_statics.is_empty()
        && let Some(child_ctx) = ctx.nested()
    {
        let cda_ctrl = effective_controller_for_object(state, id, &child_ctx).unwrap_or(obj.owner);
        let cda_ctx = EvalContext {
            controller: cda_ctrl,
            source: Some(id),
            ..child_ctx
        };
        for st in l7a_statics {
            apply_layer7a_self_cda(state, st, &mut base, &cda_ctx);
        }
    }

    if apply_7c {
        apply_pt_counters_and_temp(state, id, rare, &mut base);
    }

    Some(base)
}

/// Evaluates the mana value of `id` in any zone (`CR 202.3a`, `CR 202.3e`, `CR 708.2a`).
pub fn object_mana_value(state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> u32 {
    let Some(obj) = state.object(id) else {
        return 0;
    };
    if obj.is_face_down() {
        return 0;
    }
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        return ev.mana_value;
    }

    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    if copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && rare.is_some_and(|r| r.token_spec.is_some())
    {
        return 0;
    }

    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = eff_zone;
    let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts) else {
        return 0;
    };
    let base = resolved.mana_value;
    if eff_zone == Zone::Stack {
        let x_val = stack_entry
            .map(|s| s.x_value)
            .or_else(|| rare.and_then(|r| r.entered_with_x))
            .unwrap_or(0);
        if x_val > 0 && resolved.mana_cost.x_count > 0 {
            return base
                .saturating_add(x_val.saturating_mul(u32::from(resolved.mana_cost.x_count)));
        }
    }
    base
}

/// Computes `player`'s devotion to `colors` (`CR 700.5`) across non-phased-out battlefield permanents.
pub fn compute_devotion(
    state: &GameState,
    player: PlayerId,
    colors: ColorMask,
    ctx: &EvalContext<'_>,
) -> i32 {
    let target_colors = colors & ColorMask::ALL_COLORS;
    if target_colors.is_empty() {
        return 0;
    }
    ctx.board.record_read(DirtyMask::TOPOLOGY);
    let mut total: i32 = 0;
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() || obj.is_face_down() {
                continue;
            }
            let ctrl = effective_controller_for_object(state, id, ctx).unwrap_or(obj.controller);
            if ctrl != player {
                continue;
            }
            if !matches!(ctx.board, BoardView::BaseOnly)
                && let Some(ev) = ctx.board.resolve_eval(state, id)
            {
                total += count_mana_cost_devotion(&ev.mana_cost, target_colors);
            } else {
                let opts = face_opts_for_game_object(obj, state.rare(id), None);
                if let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts) {
                    total += count_mana_cost_devotion(&resolved.mana_cost, target_colors);
                }
            }
        }
    }
    total
}

#[inline]
pub fn count_mana_cost_devotion(cost: &ManaCost, target_colors: ColorMask) -> i32 {
    let mut total = 0i32;
    for pip in &cost.pips {
        let pip_colors = match *pip {
            ManaPip::Colored { color }
            | ManaPip::TwoBrid { color }
            | ManaPip::Phyrexian { color }
            | ManaPip::ColorlessHybrid { color } => ColorMask::from_mana_color(color),
            ManaPip::Hybrid { a, b } | ManaPip::HybridPhyrexian { a, b } => {
                ColorMask::from_mana_color(a) | ColorMask::from_mana_color(b)
            }
            ManaPip::Snow => ColorMask::empty(),
        };
        if !(pip_colors & target_colors & ColorMask::ALL_COLORS).is_empty() {
            total += 1;
        }
    }
    total
}

/// Computes `player`'s Domain (`CR 305.6`, `0..=5`) across lands they control.
pub fn compute_domain(state: &GameState, player: PlayerId, ctx: &EvalContext<'_>) -> i32 {
    const BASICS: [&str; 5] = ["Plains", "Island", "Swamp", "Mountain", "Forest"];
    ctx.board.record_read(DirtyMask::TOPOLOGY);
    let db = state.db();
    let mut basic_ids = [None; 5];
    for (idx, name) in BASICS.iter().enumerate() {
        basic_ids[idx] = db.subtype_id(name);
    }
    let mut seen_mask: u8 = 0;
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
            let ctrl = effective_controller_for_object(state, id, ctx).unwrap_or(obj.controller);
            if ctrl != player {
                continue;
            }
            if !matches!(ctx.board, BoardView::BaseOnly)
                && let Some(ev) = ctx.board.resolve_eval(state, id)
            {
                for (i, sid_opt) in basic_ids.iter().enumerate() {
                    if let Some(sid) = sid_opt
                        && ev.subtypes.contains(sid)
                    {
                        seen_mask |= 1 << i;
                    }
                }
            } else {
                let chars = off_battlefield_characteristics(state, id, obj, Zone::Battlefield, ctx);
                for (i, sid_opt) in basic_ids.iter().enumerate() {
                    if let Some(sid) = sid_opt
                        && chars.has_subtype(db, *sid)
                    {
                        seen_mask |= 1 << i;
                    }
                }
            }
            if seen_mask == 0b1_1111 {
                return 5;
            }
        }
    }
    seen_mask.count_ones() as i32
}

/// Computes `player`'s Party count (`CR 700.8a–b`, `0..=4`) via exact bipartite matching over Cleric/Rogue/Warrior/Wizard.
pub fn compute_party_count(state: &GameState, player: PlayerId, ctx: &EvalContext<'_>) -> i32 {
    const ROLES: [&str; 4] = ["Cleric", "Rogue", "Warrior", "Wizard"];
    ctx.board.record_read(DirtyMask::TOPOLOGY);
    let db = state.db();
    let mut role_ids = [None; 4];
    for (i, name) in ROLES.iter().enumerate() {
        role_ids[i] = db.subtype_id(name);
    }

    let mut reachable: u16 = 1; // bitmask over 2^4 = 16 subsets of matched roles
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
            let ctrl = effective_controller_for_object(state, id, ctx).unwrap_or(obj.controller);
            if ctrl != player {
                continue;
            }
            let mut role_mask: u8 = 0;
            if !matches!(ctx.board, BoardView::BaseOnly)
                && let Some(ev) = ctx.board.resolve_eval(state, id)
            {
                if !ev.types.contains(CardTypeMask::CREATURE) {
                    continue;
                }
                if ev.has_all_creature_types {
                    role_mask = 0x0F;
                } else {
                    for (r, sid_opt) in role_ids.iter().enumerate() {
                        if let Some(sid) = sid_opt
                            && ev.subtypes.contains(sid)
                        {
                            role_mask |= 1 << r;
                        }
                    }
                }
            } else {
                let chars = off_battlefield_characteristics(state, id, obj, Zone::Battlefield, ctx);
                if !chars.types.contains(CardTypeMask::CREATURE) {
                    continue;
                }
                for (r, sid_opt) in role_ids.iter().enumerate() {
                    if let Some(sid) = sid_opt
                        && chars.has_subtype(db, *sid)
                    {
                        role_mask |= 1 << r;
                    }
                }
            }
            if role_mask == 0 {
                continue;
            }
            let mut next = reachable;
            for s in 0..16u16 {
                if (reachable & (1 << s)) == 0 {
                    continue;
                }
                for r in 0..4u16 {
                    if (role_mask & (1 << r)) != 0 && (s & (1 << r)) == 0 {
                        next |= 1 << (s | (1 << r));
                    }
                }
            }
            reachable = next;
            if (reachable & (1 << 15)) != 0 {
                return 4;
            }
        }
    }

    let mut best = 0i32;
    for s in 1..16u16 {
        if (reachable & (1 << s)) != 0 {
            best = best.max(s.count_ones() as i32);
        }
    }
    best
}

fn zone_dirty_mask(zone: Zone) -> DirtyMask {
    match zone {
        Zone::Battlefield | Zone::Stack => DirtyMask::TOPOLOGY,
        Zone::Graveyard => DirtyMask::GRAVEYARDS,
        Zone::Exile => DirtyMask::EXILE,
        Zone::Hand | Zone::Library | Zone::Command => DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY,
    }
}

fn player_zone_slice(player: &PlayerState, zone: Zone) -> &[ObjectId] {
    match zone {
        Zone::Library => &player.library,
        Zone::Hand => &player.hand,
        Zone::Battlefield => &player.battlefield,
        Zone::Graveyard => &player.graveyard,
        Zone::Exile => &player.exile,
        Zone::Command => &player.command,
        Zone::Stack => &[],
    }
}

/// Evaluates an [`Amount`] expression, returning signed unclamped `i32` (`CR 107.1b`).
pub fn evaluate_amount(state: &GameState, amount: &Amount, ctx: &EvalContext<'_>) -> i32 {
    let Some(child_ctx) = ctx.nested() else {
        return 0;
    };
    match amount {
        Amount::Const { n } => *n,
        Amount::X => child_ctx.effective_x() as i32,
        Amount::AnyNumber => child_ctx.effective_x() as i32,
        Amount::Count { of } => find_objects(state, of, &child_ctx).len() as i32,
        Amount::Stat { of, stat, lki } => eval_stat_ref(state, of, *stat, *lki, &child_ctx),
        Amount::Devotion { colors, player } => {
            let pid = player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx))
                .unwrap_or(child_ctx.controller);
            compute_devotion(state, pid, *colors, &child_ctx)
        }
        Amount::ResultCount { of } => {
            if let Some(name) = of {
                child_ctx
                    .bindings
                    .iter()
                    .rev()
                    .find(|b| b.name == *name)
                    .map_or(0, |b| b.objects.len().max(b.lki.len()) as i32)
            } else {
                child_ctx.last_effect_amount
            }
        }
        Amount::EventAmount => child_ctx.event_amount.unwrap_or(0),
        Amount::CardsIn { zone, of } => {
            child_ctx.board.record_read(zone_dirty_mask(*zone));
            if *zone == Zone::Stack {
                return state.stack().len() as i32;
            }
            let Some(pid) = resolve_player_ref(state, of, &child_ctx) else {
                return 0;
            };
            state
                .players()
                .get(usize::from(pid.0))
                .map_or(0, |p| player_zone_slice(p, *zone).len() as i32)
        }
        Amount::Life { of } => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            let Some(pid) = resolve_player_ref(state, of, &child_ctx) else {
                return 0;
            };
            state
                .players()
                .get(usize::from(pid.0))
                .map_or(0, |p| p.life)
        }
        Amount::CommanderCastCount => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(0, |p| {
                    p.commander_cast_counts
                        .iter()
                        .map(|&(_, c)| i32::from(c))
                        .sum()
                })
        }
        Amount::KickedCount { tag, multiplier } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            let count: u16 = if let Some(tag_str) = tag.as_deref() {
                let stack_cnt = child_ctx.stack_item.map_or(0, |s| {
                    s.choices
                        .kicked_tags
                        .iter()
                        .filter(|t| t.eq_ignore_ascii_case(tag_str))
                        .count() as u16
                });
                if stack_cnt > 0 {
                    stack_cnt
                } else if let Some(src) = child_ctx.source {
                    state.rare(src).map_or(0, |r| {
                        r.kicked_tags
                            .iter()
                            .filter(|t| t.eq_ignore_ascii_case(tag_str))
                            .count() as u16
                    })
                } else {
                    0
                }
            } else {
                child_ctx
                    .stack_item
                    .map(|s| s.choices.kicked_count)
                    .filter(|&c| c > 0)
                    .or_else(|| {
                        child_ctx
                            .source
                            .and_then(|id| state.rare(id).map(|r| r.kicked_count))
                    })
                    .unwrap_or(0)
            };
            i32::from(count).saturating_mul(multiplier.unwrap_or(1))
        }
        Amount::StormCount => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            state
                .turn_history()
                .spells_cast_this_turn
                .iter()
                .map(|&(_, c)| i32::from(c))
                .sum()
        }
        Amount::GravestormCount => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            i32::from(state.turn_history().permanents_put_into_gy_from_bf_this_turn)
        }
        Amount::Negate { of } => evaluate_amount(state, of, &child_ctx).saturating_neg(),
        Amount::Sum { args } => args.iter().fold(0i32, |acc, a| {
            acc.saturating_add(evaluate_amount(state, a, &child_ctx))
        }),
        Amount::Min { args } => args
            .iter()
            .map(|a| evaluate_amount(state, a, &child_ctx))
            .min()
            .unwrap_or(0),
        Amount::Max { args } => args
            .iter()
            .map(|a| evaluate_amount(state, a, &child_ctx))
            .max()
            .unwrap_or(0),
        Amount::Mul { factor, of } => {
            factor.saturating_mul(evaluate_amount(state, of, &child_ctx))
        }
        Amount::Half { of, round } => {
            let v = evaluate_amount(state, of, &child_ctx);
            match round {
                RoundMode::Up => v.saturating_add(1).div_euclid(2),
                RoundMode::Down => v.div_euclid(2),
            }
        }
        Amount::Difference { left, right } => {
            evaluate_amount(state, left, &child_ctx)
                .saturating_sub(evaluate_amount(state, right, &child_ctx))
        }
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            if evaluate_condition(state, condition, &child_ctx) {
                evaluate_amount(state, then_amt, &child_ctx)
            } else {
                evaluate_amount(state, else_amt, &child_ctx)
            }
        }
        Amount::EnergyCount { player } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            state
                .players()
                .get(usize::from(pid.0))
                .map_or(0, |p| i32::from(p.energy))
        }
        Amount::PlayerCounters {
            counter,
            player,
            players,
        } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            match players {
                Some(PlayerCounterScope::Opponents) => state
                    .players()
                    .iter()
                    .filter(|p| !p.lost && p.id != child_ctx.controller)
                    .map(|p| player_counter_value(p, *counter))
                    .sum(),
                Some(PlayerCounterScope::All) => state
                    .players()
                    .iter()
                    .filter(|p| !p.lost)
                    .map(|p| player_counter_value(p, *counter))
                    .sum(),
                Some(PlayerCounterScope::DefendingPlayer) => {
                    let Some(pid) =
                        resolve_player_ref(state, &PlayerRef::DefendingPlayer, &child_ctx)
                    else {
                        return 0;
                    };
                    state
                        .players()
                        .get(usize::from(pid.0))
                        .map_or(0, |p| player_counter_value(p, *counter))
                }
                Some(PlayerCounterScope::You) | None => {
                    let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                        return 0;
                    };
                    state
                        .players()
                        .get(usize::from(pid.0))
                        .map_or(0, |p| player_counter_value(p, *counter))
                }
            }
        }
        Amount::PlayerCounterDifference {
            player,
            counter,
            target,
        } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            let cur = state
                .players()
                .get(usize::from(pid.0))
                .map_or(0, |p| player_counter_value(p, *counter));
            target.saturating_sub(cur).max(0)
        }
        Amount::CountersOn { counter, of } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            if let Some(id) = resolve_ref(state, of, &child_ctx)
                && state.object(id).is_some()
            {
                return count_counters_slice(state.counters(id), *counter);
            }
            if let Some(lki) = resolve_ref_lki(state, of, true, &child_ctx) {
                return count_counters_slice(&lki.counters, *counter);
            }
            0
        }
        Amount::LkiCounterCount { counter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let snap = child_ctx
                .event_lki
                .or_else(|| child_ctx.source_lki());
            snap.map_or(0, |l| count_counters_slice(&l.counters, *counter))
        }
        Amount::LkiTotalCounters { of } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            resolve_ref_lki(state, of, true, &child_ctx)
                .map_or(0, |l| count_counters_slice(&l.counters, CounterTarget::Any))
        }
        Amount::CountersRemovedThisWay => {
            child_ctx.counters_removed_this_way.max(child_ctx.last_counters_removed)
        }
        Amount::DistinctCounterKindsOn { of } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            if let Some(id) = resolve_ref(state, of, &child_ctx)
                && state.object(id).is_some()
            {
                return state
                    .counters(id)
                    .iter()
                    .filter(|(_, c)| *c > 0)
                    .count() as i32;
            }
            resolve_ref_lki(state, of, true, &child_ctx)
                .map_or(0, |l| l.counters.iter().filter(|(_, c)| *c > 0).count() as i32)
        }
        Amount::TotalCountersAmong { filter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            find_objects(state, filter, &child_ctx)
                .into_iter()
                .map(|id| count_counters_slice(state.counters(id), CounterTarget::Any))
                .sum()
        }
        Amount::CountersAmong { filter, counter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            find_objects(state, filter, &child_ctx)
                .into_iter()
                .map(|id| state.counter_count(id, *counter) as i32)
                .sum()
        }
        Amount::CountersAmongControlled { filter, player } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            let player_ctx = EvalContext {
                controller: pid,
                ..child_ctx
            };
            find_objects(state, filter, &player_ctx)
                .into_iter()
                .filter(|&id| {
                    effective_controller_for_object(state, id, &player_ctx) == Some(pid)
                })
                .map(|id| count_counters_slice(state.counters(id), CounterTarget::Any))
                .sum()
        }
        Amount::CountersAmongPlayersAndPermanents { counter } => {
            child_ctx
                .board
                .record_read(DirtyMask::COUNTERS | DirtyMask::PLAYER_STATE);
            let mut total = 0i32;
            let target = counter.unwrap_or(CounterTarget::Any);
            let db = state.db();
            let specific_name = match target {
                CounterTarget::Any | CounterTarget::Each => None,
                CounterTarget::Specific(cid) => db.counter_name(cid),
            };
            for p in state.players() {
                if p.lost {
                    continue;
                }
                match target {
                    CounterTarget::Any | CounterTarget::Each => {
                        total += i32::from(p.poison)
                            + i32::from(p.energy)
                            + i32::from(p.experience)
                            + i32::from(p.rad);
                    }
                    CounterTarget::Specific(_) => {
                        if let Some(cname) = specific_name {
                            if cname.eq_ignore_ascii_case("poison") {
                                total += i32::from(p.poison);
                            } else if cname.eq_ignore_ascii_case("energy") {
                                total += i32::from(p.energy);
                            } else if cname.eq_ignore_ascii_case("experience") {
                                total += i32::from(p.experience);
                            } else if cname.eq_ignore_ascii_case("rad") {
                                total += i32::from(p.rad);
                            }
                        }
                    }
                }
                for &id in &p.battlefield {
                    if let Some(obj) = state.object(id)
                        && !obj.is_phased_out()
                    {
                        total += count_counters_slice(state.counters(id), target);
                    }
                }
            }
            total
        }
        Amount::TappedCostPower => child_ctx
            .sacrificed_cost_lki
            .iter()
            .filter_map(|l| l.pt.map(|pt| pt.power))
            .sum(),
        Amount::ColorsOfManaSpentToCast => {
            let mask = paid_mana_colors_for_ctx(state, &child_ctx);
            mask.color_count() as i32
        }
        Amount::ManaSpentToCast => {
            child_ctx
                .source
                .and_then(|id| state.rare(id).map(|r| r.mana_spent_count as i32))
                .unwrap_or(0)
        }
        Amount::DistinctCardTypesInZone { zone, whose } => {
            count_distinct_card_types_in_zone(state, *zone, *whose, &child_ctx)
        }
        Amount::LastRollResult => child_ctx.last_roll_result,
        Amount::CoinHeadsCount => child_ctx.coin_heads_count,
        Amount::DevouredCount => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            0
        }
        Amount::OpponentCount => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .count() as i32
        }
        Amount::Domain { player } => {
            let pid = resolve_player_ref(state, player, &child_ctx)
                .unwrap_or(child_ctx.controller);
            compute_domain(state, pid, &child_ctx)
        }
        Amount::PartyCount { player } => {
            let pid = resolve_player_ref(state, player, &child_ctx)
                .unwrap_or(child_ctx.controller);
            compute_party_count(state, pid, &child_ctx)
        }
        Amount::AttachmentsOn { target, filter } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let host_id = match target {
                AttachmentOwnerTarget::SelfObj => child_ctx.source,
                AttachmentOwnerTarget::AttachedTo => {
                    child_ctx.source.and_then(|s| state.object(s)?.attached_to)
                }
            };
            let Some(host_id) = host_id else {
                return 0;
            };
            let mut count = 0i32;
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out() || obj.attached_to != Some(host_id) {
                        continue;
                    }
                    if let Some(f) = filter.as_deref()
                        && !matches_object_filter(state, id, f, &child_ctx)
                    {
                        continue;
                    }
                    count += 1;
                }
            }
            count
        }
        Amount::UnlockedDoorsCount { player } => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
            let pid = resolve_player_ref(state, player, &child_ctx)
                .unwrap_or(child_ctx.controller);
            let mut count = 0i32;
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out() || obj.is_face_down() {
                        continue;
                    }
                    if effective_controller_for_object(state, id, &child_ctx) != Some(pid) {
                        continue;
                    }
                    if let Some(rare) = state.rare(id) {
                        count += rare.unlocked_faces.len() as i32;
                    }
                }
            }
            count
        }
        Amount::ExcessDamage => child_ctx.last_excess_damage,
        Amount::LastEffectAmount => child_ctx.last_effect_amount,
        Amount::ColorsAmongPermanents { filter, player } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let pid = resolve_player_ref(state, player, &child_ctx)
                .unwrap_or(child_ctx.controller);
            let mut mask = ColorMask::empty();
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
                    if let Some(f) = filter.as_deref() {
                        let f_ctx = EvalContext {
                            controller: pid,
                            ..child_ctx
                        };
                        if !matches_object_filter(state, id, f, &f_ctx) {
                            continue;
                        }
                    } else if effective_controller_for_object(state, id, &child_ctx) != Some(pid) {
                        continue;
                    }
                    mask |= object_colors_in_ctx(state, id, &child_ctx) & ColorMask::ALL_COLORS;
                }
            }
            mask.color_count() as i32
        }
        Amount::DungeonsCompleted { .. } | Amount::PlayerSpeed { .. } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            0
        }
        Amount::GreatestStatAmong { filter, stat } => {
            let ids = find_objects(state, filter, &child_ctx);
            ids.into_iter()
                .filter_map(|id| read_object_stat(state, id, *stat, &child_ctx))
                .max()
                .unwrap_or(0)
        }
        Amount::LeastStatAmong { filter, stat } => {
            let ids = find_objects(state, filter, &child_ctx);
            ids.into_iter()
                .filter_map(|id| read_object_stat(state, id, *stat, &child_ctx))
                .min()
                .unwrap_or(0)
        }
        Amount::TotalStatAmong { filter, stat } => {
            let ids = find_objects(state, filter, &child_ctx);
            ids.into_iter()
                .filter_map(|id| read_object_stat(state, id, *stat, &child_ctx))
                .fold(0i32, |acc, v| acc.saturating_add(v))
        }
        Amount::GreatestStatAmongControlledAsCast { stat, filter } => {
            let ids = find_objects(state, filter, &child_ctx);
            ids.into_iter()
                .filter(|&id| {
                    effective_controller_for_object(state, id, &child_ctx)
                        == Some(child_ctx.controller)
                })
                .filter_map(|id| read_object_stat(state, id, *stat, &child_ctx))
                .max()
                .unwrap_or(0)
        }
        Amount::GreatestBaseToughnessControlled { filter } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut best: Option<i32> = None;
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
                    if effective_controller_for_object(state, id, &child_ctx)
                        != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    if let Some(f) = filter.as_deref()
                        && !matches_object_filter(state, id, f, &child_ctx)
                    {
                        continue;
                    }
                    let base_t = if !matches!(child_ctx.board, BoardView::BaseOnly)
                        && let Some(ev) = child_ctx.board.resolve_eval(state, id)
                    {
                        ev.base_pt.map(|pt| pt.toughness)
                    } else {
                        object_base_pt(state, id, &child_ctx).map(|pt| pt.toughness)
                    };
                    if let Some(t) = base_t {
                        best = Some(best.map_or(t, |cur| cur.max(t)));
                    }
                }
            }
            best.unwrap_or(0)
        }
        Amount::GreatestControlledCount { filter } => {
            let mut best = 0i32;
            for p in state.players() {
                if p.lost {
                    continue;
                }
                let p_ctx = EvalContext {
                    controller: p.id,
                    ..child_ctx
                };
                let cnt = find_objects(state, filter, &p_ctx)
                    .into_iter()
                    .filter(|&id| effective_controller_for_object(state, id, &p_ctx) == Some(p.id))
                    .count() as i32;
                if cnt > best {
                    best = cnt;
                }
            }
            best
        }
        Amount::TargetsBeyondFirst => child_ctx.effective_targets().len().saturating_sub(1) as i32,
        Amount::ColorsOfObject { of } => {
            let target_ref = match of {
                ObjectColorTarget::SelfObj => Ref::SelfObj,
                ObjectColorTarget::AttachedTo => Ref::AttachedTo,
                ObjectColorTarget::Affected => Ref::Affected,
            };
            let Some(id) = resolve_ref(state, &target_ref, &child_ctx) else {
                return 0;
            };
            (object_colors_in_ctx(state, id, &child_ctx) & ColorMask::ALL_COLORS).color_count()
                as i32
        }
        Amount::DistinctManaValuesInGraveyard { filter } => {
            child_ctx.board.record_read(DirtyMask::GRAVEYARDS);
            let mut mvs: Vec<u32> = Vec::new();
            let Some(owner) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return 0;
            };
            for &id in &owner.graveyard {
                if let Some(f) = filter.as_deref()
                    && !matches_object_filter(state, id, f, &child_ctx)
                {
                    continue;
                }
                let mv = object_mana_value(state, id, &child_ctx);
                if !mvs.contains(&mv) {
                    mvs.push(mv);
                }
            }
            mvs.len() as i32
        }
        Amount::DistinctCreatureTypesControlled => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let db = state.db();
            let mut seen: Vec<SubtypeId> = Vec::new();
            let mut has_changeling = false;
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    let (types, subtypes, all_ct) = object_types_subtypes(state, id, &child_ctx);
                    if !types.contains(CardTypeMask::CREATURE) {
                        continue;
                    }
                    if all_ct {
                        has_changeling = true;
                    }
                    for sid in subtypes {
                        if !db.subtype_name(sid).is_some_and(is_non_creature_subtype)
                            && !seen.contains(&sid)
                        {
                            seen.push(sid);
                        }
                    }
                }
            }
            if has_changeling {
                let total_creature_subs = db
                    .tables
                    .subtypes
                    .iter()
                    .filter(|s| !is_non_creature_subtype(s))
                    .count();
                total_creature_subs.max(seen.len()) as i32
            } else {
                seen.len() as i32
            }
        }
        Amount::DistinctLandNamesControlled => {
            let filter = ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                types_any: CardTypeMask::LAND,
                controller: Some(RelativeControl::You),
                ..Default::default()
            };
            count_distinct_names_for_filter(state, &filter, &child_ctx)
        }
        Amount::DistinctNamesControlled { filter } => {
            let default_filter = ObjectFilter {
                zones: ZoneMask::BATTLEFIELD,
                controller: Some(RelativeControl::You),
                ..Default::default()
            };
            let f = filter.as_deref().unwrap_or(&default_filter);
            count_distinct_names_for_filter(state, f, &child_ctx)
        }
        Amount::DistinctNamesCount { of } => {
            count_distinct_names_for_filter(state, of, &child_ctx)
        }
        Amount::DistinctColorsAmongControlled => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut mask = ColorMask::empty();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    mask |= object_colors_in_ctx(state, id, &child_ctx) & ColorMask::ALL_COLORS;
                }
            }
            mask.color_count() as i32
        }
        Amount::DistinctPowersControlled => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut seen: Vec<i32> = Vec::new();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                    if !types.contains(CardTypeMask::CREATURE) {
                        continue;
                    }
                    if let Some(pt) = object_pt(state, id, &child_ctx)
                        && !seen.contains(&pt.power)
                    {
                        seen.push(pt.power);
                    }
                }
            }
            seen.len() as i32
        }
        Amount::StartingLifeDifference => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(0, |p| (state.config().starting_life - p.life).max(0))
        }
        Amount::CommanderColorIdentityCount => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(0, |p| {
                    (p.commander_color_identity & ColorMask::ALL_COLORS).color_count() as i32
                })
        }
        Amount::LandsOpponentControlsProducingColorless => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut count = 0i32;
            for p in state.players() {
                if p.lost || p.id == child_ctx.controller {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out() {
                        continue;
                    }
                    let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                    if !types.contains(CardTypeMask::LAND) {
                        continue;
                    }
                    if land_can_produce_colorless(state, id, &child_ctx) {
                        count += 1;
                    }
                }
            }
            count
        }
        Amount::SameNameInGraveyardAsSpell => {
            child_ctx.board.record_read(DirtyMask::GRAVEYARDS);
            let Some(src_id) = child_ctx.source else {
                return 0;
            };
            let src_name = object_name_in_ctx(state, src_id, &child_ctx);
            if src_name.is_empty() {
                return 0;
            }
            let mut count = 0i32;
            for p in state.players() {
                for &gid in &p.graveyard {
                    let n = object_name_in_ctx(state, gid, &child_ctx);
                    if !n.is_empty() && n.eq_ignore_ascii_case(&src_name) {
                        count += 1;
                    }
                }
            }
            count
        }
        Amount::TargetsCount => child_ctx.effective_targets().len() as i32,
        Amount::TargetsMatchingCount { filter } => child_ctx
            .effective_targets()
            .iter()
            .filter(|t| {
                matches!(t, TargetRef::Object { id } if matches_object_filter(state, *id, filter, &child_ctx))
            })
            .count() as i32,
        Amount::TargetColorsCount => {
            let mut mask = ColorMask::empty();
            for t in child_ctx.effective_targets() {
                if let TargetRef::Object { id } = t {
                    mask |= object_colors_in_ctx(state, *id, &child_ctx) & ColorMask::ALL_COLORS;
                }
            }
            mask.color_count() as i32
        }
        Amount::TargetCounterCount { counter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let target = counter.unwrap_or(CounterTarget::Any);
            child_ctx
                .effective_targets()
                .iter()
                .filter_map(|t| match t {
                    TargetRef::Object { id } => Some(count_counters_slice(state.counters(*id), target)),
                    _ => None,
                })
                .sum()
        }
        Amount::TargetPower => {
            let Some(TargetRef::Object { id }) = child_ctx.effective_targets().first() else {
                return 0;
            };
            object_pt(state, *id, &child_ctx).map_or(0, |pt| pt.power)
        }
        Amount::TargetsTotalManaValue => child_ctx
            .effective_targets()
            .iter()
            .filter_map(|t| match t {
                TargetRef::Object { id } => Some(object_mana_value(state, *id, &child_ctx) as i32),
                _ => None,
            })
            .sum(),
        Amount::TotalManaValueOfTargetSpec { index } => {
            let idx = usize::from(*index);
            let spec_targets: Option<&[TargetRef]> = if !child_ctx.targets_by_spec.is_empty() {
                child_ctx.targets_by_spec.get(idx).copied()
            } else {
                child_ctx
                    .stack_item
                    .and_then(|s| s.choices.targets_by_spec.get(idx).map(Vec::as_slice))
            };
            spec_targets.map_or(0, |spec_targets| {
                spec_targets
                    .iter()
                    .filter_map(|t| match t {
                        TargetRef::Object { id } => {
                            Some(object_mana_value(state, *id, &child_ctx) as i32)
                        }
                        _ => None,
                    })
                    .sum()
            })
        }
        Amount::CostPaidCount { kind } => match kind {
            mtg_ir::CostPaidCountKind::Sacrificed => child_ctx.sacrificed_cost_lki.len() as i32,
            mtg_ir::CostPaidCountKind::RemovedCounters => child_ctx.counters_removed_this_way,
            mtg_ir::CostPaidCountKind::Exiled => {
                if let Some(src) = child_ctx.source {
                    state
                        .players()
                        .iter()
                        .flat_map(|p| p.exile.iter().copied())
                        .filter(|&ex| state.rare(ex).is_some_and(|r| r.exiled_by == Some(src)))
                        .count() as i32
                } else {
                    0
                }
            }
        },
        Amount::SacrificedCostStat { stat } => child_ctx
            .sacrificed_cost_lki
            .iter()
            .map(|lki| match stat {
                SacrificedStatKind::Power => lki.pt.map_or(0, |p| p.power),
                SacrificedStatKind::Toughness => lki.pt.map_or(0, |p| p.toughness),
                SacrificedStatKind::ManaValue => lki.mana_value as i32,
                SacrificedStatKind::ColorsCount => {
                    (lki.colors & ColorMask::ALL_COLORS).color_count() as i32
                }
            })
            .sum(),
        Amount::RevealedCostStat { stat } => {
            let Some(cid) = child_ctx.revealed_cost_card else {
                return 0;
            };
            let Some(card) = state.db().get(cid) else {
                return 0;
            };
            let Some(front) = card.faces.first() else {
                return 0;
            };
            match stat {
                StatKind::ManaValue => front.characteristics.mana_cost.mana_value() as i32,
                StatKind::Power => match front.characteristics.pt {
                    Some(mtg_ir::PrintedPt::Fixed { power, .. }) => power,
                    Some(mtg_ir::PrintedPt::Cda { power, .. }) => power.unwrap_or(0),
                    None => 0,
                },
                StatKind::Toughness => match front.characteristics.pt {
                    Some(mtg_ir::PrintedPt::Fixed { toughness, .. }) => toughness,
                    Some(mtg_ir::PrintedPt::Cda { toughness, .. }) => toughness.unwrap_or(0),
                    None => 0,
                },
            }
        }
        Amount::ChosenPlayerZoneCount { zone, filter } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | zone_dirty_mask(*zone));
            let Some(chosen) = child_ctx
                .chosen_opponent
                .or_else(|| child_ctx.source.and_then(|s| state.rare(s)?.chosen_player))
            else {
                return 0;
            };
            if *zone == Zone::Stack {
                return state.stack().len() as i32;
            }
            let Some(p) = state.players().get(usize::from(chosen.0)) else {
                return 0;
            };
            let slice = player_zone_slice(p, *zone);
            if let Some(f) = filter.as_deref() {
                slice
                    .iter()
                    .filter(|&&id| matches_object_filter(state, id, f, &child_ctx))
                    .count() as i32
            } else {
                slice.len() as i32
            }
        }
        Amount::OpponentsWithLifeLessThanHalfStarting => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            let half = state.config().starting_life.div_euclid(2);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller && p.life < half)
                .count() as i32
        }
        Amount::OpponentsControllingMoreCount { filter } => {
            let you_count = find_objects(state, filter, &child_ctx)
                .into_iter()
                .filter(|&id| {
                    effective_controller_for_object(state, id, &child_ctx)
                        == Some(child_ctx.controller)
                })
                .count();
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .filter(|p| {
                    let p_ctx = EvalContext {
                        controller: p.id,
                        ..child_ctx
                    };
                    let opp_count = find_objects(state, filter, &p_ctx)
                        .into_iter()
                        .filter(|&id| {
                            effective_controller_for_object(state, id, &p_ctx) == Some(p.id)
                        })
                        .count();
                    opp_count > you_count
                })
                .count() as i32
        }
        Amount::OpponentsControllingFilterCount { filter } => state
            .players()
            .iter()
            .filter(|p| !p.lost && p.id != child_ctx.controller)
            .filter(|p| {
                let p_ctx = EvalContext {
                    controller: p.id,
                    ..child_ctx
                };
                find_objects(state, filter, &p_ctx)
                    .into_iter()
                    .any(|id| effective_controller_for_object(state, id, &p_ctx) == Some(p.id))
            })
            .count() as i32,
        Amount::OpponentsWithHandMaxCount { max } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller && p.hand.len() <= usize::from(*max))
                .count() as i32
        }
        Amount::AttackingCountToPlayer { player } => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            state
                .combat()
                .attackers
                .iter()
                .filter(|a| match a.target {
                    crate::action::AttackTarget::Player { id } => id == pid,
                    crate::action::AttackTarget::Planeswalker { id }
                    | crate::action::AttackTarget::Battle { id } => {
                        effective_controller_for_object(state, id, &child_ctx) == Some(pid)
                    }
                })
                .count() as i32
        }
        Amount::HandSizeDifference { target_player, cap } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let you_hand = state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(0, |p| p.hand.len() as i32);
            if let Some(tp) = target_player {
                let other_hand = resolve_player_ref(state, tp, &child_ctx)
                    .and_then(|pid| state.players().get(usize::from(pid.0)))
                    .map_or(0, |p| p.hand.len() as i32);
                (other_hand - you_hand).max(0)
            } else {
                (cap.unwrap_or(7) - you_hand).max(0)
            }
        }
        Amount::ExiledCardsSharingTypeCount { owner } => {
            child_ctx.board.record_read(DirtyMask::EXILE);
            let mut mask = CardTypeMask::empty();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.exile {
                    if let Some(rel) = owner
                        && !matches_relative_control(p.id, *rel, &child_ctx)
                    {
                        continue;
                    }
                    if let Some(obj) = state.object(id)
                        && !obj.is_face_down()
                    {
                        let chars =
                            off_battlefield_characteristics(state, id, obj, Zone::Exile, &child_ctx);
                        mask |= chars.types;
                    }
                }
            }
            mask.bits().count_ones() as i32
        }
        Amount::UnspentManaTotal { player } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let pid = player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx))
                .unwrap_or(child_ctx.controller);
            state
                .players()
                .get(usize::from(pid.0))
                .map_or(0, |p| p.mana_pool.iter().map(|&u| u as i32).sum())
        }
        Amount::FractionalLifeLoss {
            player,
            denominator,
            round,
        } => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            let Some(p) = state.players().get(usize::from(pid.0)) else {
                return 0;
            };
            if p.life <= 0 {
                return 0;
            }
            let life = i64::from(p.life);
            let denom = i64::from((*denominator).max(1));
            match round {
                RoundMode::Up => ((life + denom - 1) / denom) as i32,
                RoundMode::Down => (life / denom) as i32,
            }
        }
        Amount::LifeGainedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            TurnEventHistory::get_u32(
                &state.turn_history().life_gained_this_turn,
                child_ctx.controller,
            ) as i32
        }
        Amount::OpponentsLifeLostThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .life_lost_this_turn
                .iter()
                .filter(|&&(pid, _)| pid != child_ctx.controller)
                .map(|&(_, amt)| amt as i32)
                .sum()
        }
        Amount::PlayersLostLifeThisTurnCount => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .life_lost_this_turn
                .iter()
                .filter(|&&(_, amt)| amt > 0)
                .count() as i32
        }
        Amount::CreaturesAttackedWithThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let mut seen: SmallVec<[ObjectId; 8]> = SmallVec::new();
            for &(pid, id) in &state.turn_history().creatures_attacked_this_turn {
                if pid == child_ctx.controller && !seen.contains(&id) {
                    seen.push(id);
                }
            }
            seen.len() as i32
        }
        Amount::CreaturesAttackedTotalThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let mut seen: SmallVec<[ObjectId; 16]> = SmallVec::new();
            for &(_, id) in &state.turn_history().creatures_attacked_this_turn {
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            seen.len() as i32
        }
        Amount::TimesAttackedThisTurn { of } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let target_id = of
                .as_ref()
                .and_then(|r| resolve_ref(state, r, &child_ctx))
                .or(child_ctx.source);
            target_id.map_or(0, |tid| {
                state
                    .turn_history()
                    .creatures_attacked_this_turn
                    .iter()
                    .filter(|&&(_, id)| id == tid)
                    .count() as i32
            })
        }
        Amount::OpponentsDealtDamageThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let mut seen: SmallVec<[PlayerId; 4]> = SmallVec::new();
            for &(_, victim, amt, _) in &state.turn_history().damage_dealt_to_players_this_turn {
                if amt > 0 && victim != child_ctx.controller && !seen.contains(&victim) {
                    seen.push(victim);
                }
            }
            seen.len() as i32
        }
        Amount::DamageDealtToOpponentsThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .damage_dealt_to_players_this_turn
                .iter()
                .filter(|&&(_, victim, _, _)| victim != child_ctx.controller)
                .map(|&(_, _, amt, _)| amt as i32)
                .sum()
        }
        Amount::DamageDealtToSelfByPlayerThisTurn { player } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return 0;
            };
            state
                .turn_history()
                .damage_dealt_to_players_this_turn
                .iter()
                .filter(|&&(src, victim, _, _)| {
                    victim == child_ctx.controller
                        && src.is_some_and(|sid| {
                            effective_controller_for_object(state, sid, &child_ctx) == Some(pid)
                        })
                })
                .map(|&(_, _, amt, _)| amt as i32)
                .sum()
        }
        Amount::OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn { subtype } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let mut seen: SmallVec<[PlayerId; 4]> = SmallVec::new();
            for &(src, victim, amt, is_combat) in
                &state.turn_history().damage_dealt_to_players_this_turn
            {
                if amt == 0 || !is_combat || victim == child_ctx.controller {
                    continue;
                }
                let matches_src = src.is_some_and(|sid| {
                    Some(sid) == child_ctx.source
                        || object_types_subtypes(state, sid, &child_ctx)
                            .1
                            .contains(subtype)
                });
                if matches_src && !seen.contains(&victim) {
                    seen.push(victim);
                }
            }
            seen.len() as i32
        }
        Amount::CountedThisWay { kind } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            match kind {
                CountedThisWayKind::ExiledThisWay | CountedThisWayKind::ExiledCardsThisWay => {
                    child_ctx
                        .step_way_stats
                        .map_or(child_ctx.last_effect_amount, |s| s.exiled_count_this_way as i32)
                }
                _ => {
                    if child_ctx.last_effect_amount != 0 {
                        child_ctx.last_effect_amount
                    } else {
                        child_ctx.step_acted_ids.len() as i32
                    }
                }
            }
        }
        Amount::CreaturesDiedThisTurn { controller } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            if let Some(rel) = controller {
                state
                    .players()
                    .iter()
                    .filter(|p| matches_relative_control(p.id, *rel, &child_ctx))
                    .map(|p| p.creatures_died_this_turn as i32)
                    .sum()
            } else {
                i32::from(state.turn_history().creatures_died_this_turn)
            }
        }
        Amount::NontokenCreaturesDiedThisTurn { controller } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            if let Some(rel) = controller {
                state
                    .players()
                    .iter()
                    .filter(|p| matches_relative_control(p.id, *rel, &child_ctx))
                    .map(|p| p.nontoken_creatures_died_this_turn as i32)
                    .sum()
            } else {
                i32::from(state.turn_history().nontoken_creatures_died_this_turn)
            }
        }
        Amount::SpellsCastThisTurnCount { filter: _, who } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            let target_pid = match who {
                None | Some(SpellsCastWho::You) => child_ctx.controller,
                Some(SpellsCastWho::ActivePlayer) => state.active_player(),
            };
            i32::from(TurnEventHistory::get_u16(
                &state.turn_history().spells_cast_this_turn,
                target_pid,
            ))
        }
        Amount::CardsDiscardedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            state
                .turn_history()
                .cards_discarded_this_turn
                .iter()
                .map(|&(_, c)| i32::from(c))
                .sum()
        }
        Amount::TokensCreatedThisTurn { player } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            let pid = player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx))
                .unwrap_or(child_ctx.controller);
            i32::from(TurnEventHistory::get_u16(
                &state.turn_history().tokens_created_this_turn,
                pid,
            ))
        }
        Amount::CardsPutIntoGraveyardFromHandOrLibraryThisTurn { player } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            let pid = player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx));
            state
                .turn_history()
                .put_into_graveyard_from_non_battlefield_this_turn
                .iter()
                .filter(|&&oid| pid.is_none_or(|p| state.object(oid).is_some_and(|o| o.owner == p)))
                .count() as i32
        }
        Amount::SacrificedPermsThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            i32::from(TurnEventHistory::get_u16(
                &state.turn_history().sacrificed_this_turn,
                child_ctx.controller,
            ))
        }
        Amount::GreatestOpponentCardsDrawnThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::GRAVEYARDS);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .map(|p| {
                    i32::from(TurnEventHistory::get_u16(
                        &state.turn_history().cards_drawn_this_turn,
                        p.id,
                    ))
                })
                .max()
                .unwrap_or(0)
        }
        Amount::RingBearerPower { .. } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            0
        }
    }
}

/// Evaluates a [`Condition`] boolean expression.
pub fn evaluate_condition(state: &GameState, cond: &Condition, ctx: &EvalContext<'_>) -> bool {
    let Some(child_ctx) = ctx.nested() else {
        return false;
    };
    match cond {
        Condition::Compare { left, op, right } => {
            let lhs = evaluate_amount(state, left, &child_ctx);
            let rhs = evaluate_amount(state, right, &child_ctx);
            compare_i32(lhs, *op, rhs)
        }
        Condition::And { of } => of.iter().all(|c| evaluate_condition(state, c, &child_ctx)),
        Condition::Or { of } => of.iter().any(|c| evaluate_condition(state, c, &child_ctx)),
        Condition::Not { of } => !evaluate_condition(state, of, &child_ctx),

        Condition::GraveyardAbove { filter, count } => {
            child_ctx.board.record_read(DirtyMask::GRAVEYARDS);
            find_objects(state, filter, &child_ctx).len() >= usize::from(*count)
        }
        Condition::IsTurnOf { player } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            resolve_player_ref(state, player, &child_ctx) == Some(state.active_player())
        }
        Condition::IsYourTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            state.active_player() == child_ctx.controller
        }
        Condition::NotYourTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            state.active_player() != child_ctx.controller
        }
        Condition::NotStartingPlayer => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            child_ctx.controller != PlayerId(0)
        }
        Condition::NotSpellControllerTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            let spell_ctrl = child_ctx
                .stack_item
                .map_or(child_ctx.controller, |s| s.controller);
            state.active_player() != spell_ctrl
        }
        Condition::NotEventPlayerTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            child_ctx
                .event_player
                .is_some_and(|ep| state.active_player() != ep)
        }
        Condition::IsYourMainPhase => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            state.active_player() == child_ctx.controller
                && matches!(state.phase(), GamePhase::Main1 | GamePhase::Main2)
        }
        Condition::ControlCommander => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .is_some_and(|p| {
                    p.battlefield.iter().any(|&id| {
                        state
                            .object(id)
                            .is_some_and(|o| !o.is_phased_out() && o.is_commander())
                    })
                })
        }
        Condition::Kicked { tag, min_count } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            let count: u16 = if let Some(tag_str) = tag.as_deref() {
                let stack_cnt = child_ctx.stack_item.map_or(0, |s| {
                    s.choices
                        .kicked_tags
                        .iter()
                        .filter(|t| t.eq_ignore_ascii_case(tag_str))
                        .count() as u16
                });
                if stack_cnt > 0 {
                    stack_cnt
                } else if let Some(src) = child_ctx.source {
                    state.rare(src).map_or(0, |r| {
                        r.kicked_tags
                            .iter()
                            .filter(|t| t.eq_ignore_ascii_case(tag_str))
                            .count() as u16
                    })
                } else {
                    0
                }
            } else {
                child_ctx
                    .stack_item
                    .map(|s| s.choices.kicked_count)
                    .filter(|&c| c > 0)
                    .or_else(|| {
                        child_ctx
                            .source
                            .and_then(|id| state.rare(id).map(|r| r.kicked_count))
                    })
                    .unwrap_or(0)
            };
            count >= min_count.unwrap_or(1)
        }
        Condition::Evoked => source_has_flag(state, &child_ctx, ObjectFlags::EVOKED),
        Condition::Escaped => source_has_flag(state, &child_ctx, ObjectFlags::ESCAPED),
        Condition::Foretold => source_has_flag(state, &child_ctx, ObjectFlags::FORETOLD),
        Condition::Blitzed => source_has_flag(state, &child_ctx, ObjectFlags::BLITZED),
        Condition::Saddled => source_has_flag(state, &child_ctx, ObjectFlags::SADDLED),
        Condition::ExertedThisTurn => source_has_flag(state, &child_ctx, ObjectFlags::EXERTED),
        Condition::Monstrous => source_has_flag(state, &child_ctx, ObjectFlags::MONSTROUS),
        Condition::GiftPromised => child_ctx
            .stack_item
            .is_some_and(|s| s.choices.gift_promised),
        Condition::IsRenowned => source_has_flag(state, &child_ctx, ObjectFlags::RENOWNED),
        Condition::SelfPrepared => source_has_flag(state, &child_ctx, ObjectFlags::PREPARED),
        Condition::IsSuspended => source_has_flag(state, &child_ctx, ObjectFlags::IS_SUSPENDED),
        Condition::IsHarnessed => source_has_flag(state, &child_ctx, ObjectFlags::HARNESSED),
        Condition::IsImpending => child_ctx.source.is_some_and(|id| {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            state
                .db()
                .counter_id("time")
                .is_some_and(|cid| state.counter_count(id, cid) > 0)
        }),

        Condition::MatchesFilter {
            target,
            filter,
            any,
        } => {
            if *any {
                !find_objects(state, filter, &child_ctx).is_empty()
            } else {
                let id = target
                    .as_ref()
                    .and_then(|r| resolve_ref(state, r, &child_ctx))
                    .or(child_ctx.source);
                id.is_some_and(|oid| matches_object_filter(state, oid, filter, &child_ctx))
            }
        }
        Condition::ControlsFilter { filter } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let bf_ctx = EvalContext {
                zone_override: Some(Zone::Battlefield),
                ..child_ctx
            };
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return false;
            };
            p.battlefield
                .iter()
                .any(|&id| matches_object_filter(state, id, filter, &bf_ctx))
        }
        Condition::PlayerControlsFilter { player, filter } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let Some(pid) = resolve_player_ref(state, player, &child_ctx) else {
                return false;
            };
            let Some(p) = state.players().get(usize::from(pid.0)) else {
                return false;
            };
            let bf_ctx = EvalContext {
                controller: pid,
                zone_override: Some(Zone::Battlefield),
                ..child_ctx
            };
            p.battlefield
                .iter()
                .any(|&id| matches_object_filter(state, id, filter, &bf_ctx))
        }
        Condition::ObjectState {
            of,
            tapped,
            attacking,
            blocking,
            monstrous,
            saddled,
            equipped,
            enchanted,
            filter,
        } => {
            let target_id = match of {
                AttachmentOwnerTarget::SelfObj => child_ctx.source,
                AttachmentOwnerTarget::AttachedTo => {
                    child_ctx.source.and_then(|s| state.object(s)?.attached_to)
                }
            };
            let Some(id) = target_id else {
                return false;
            };
            let Some(obj) = state.object(id) else {
                return false;
            };
            if let Some(want) = tapped {
                child_ctx.board.record_read(DirtyMask::TAPPED);
                if obj.flags.contains(ObjectFlags::TAPPED) != *want {
                    return false;
                }
            }
            if let Some(want) = attacking {
                child_ctx.board.record_read(DirtyMask::COMBAT);
                let is_atk = obj.zone == Zone::Battlefield
                    && state.combat().attackers.iter().any(|a| a.attacker == id);
                if is_atk != *want {
                    return false;
                }
            }
            if let Some(want) = blocking {
                child_ctx.board.record_read(DirtyMask::COMBAT);
                let is_blk = obj.zone == Zone::Battlefield
                    && state.combat().blockers.iter().any(|b| b.blocker == id);
                if is_blk != *want {
                    return false;
                }
            }
            if let Some(want) = monstrous {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                if obj.flags.contains(ObjectFlags::MONSTROUS) != *want {
                    return false;
                }
            }
            if let Some(want) = saddled {
                child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
                if obj.flags.contains(ObjectFlags::SADDLED) != *want {
                    return false;
                }
            }
            if let Some(want) = equipped {
                child_ctx.board.record_read(DirtyMask::TOPOLOGY);
                if has_attachment_subtype(state, id, "Equipment", &child_ctx) != *want {
                    return false;
                }
            }
            if let Some(want) = enchanted {
                child_ctx.board.record_read(DirtyMask::TOPOLOGY);
                if has_attachment_subtype(state, id, "Aura", &child_ctx) != *want {
                    return false;
                }
            }
            if let Some(f) = filter.as_deref()
                && !matches_object_filter(state, id, f, &child_ctx)
            {
                return false;
            }
            true
        }
        Condition::AttachedPermanentMatches { colors, untapped } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let Some(host_id) = child_ctx.source.and_then(|s| state.object(s)?.attached_to) else {
                return false;
            };
            let Some(host) = state.object(host_id) else {
                return false;
            };
            if host.zone != Zone::Battlefield || host.is_phased_out() {
                return false;
            }
            if *untapped == Some(true) {
                child_ctx.board.record_read(DirtyMask::TAPPED);
                if host.flags.contains(ObjectFlags::TAPPED) {
                    return false;
                }
            } else if *untapped == Some(false) {
                child_ctx.board.record_read(DirtyMask::TAPPED);
                if !host.flags.contains(ObjectFlags::TAPPED) {
                    return false;
                }
            }
            if !colors.is_empty() {
                let host_colors = object_colors_in_ctx(state, host_id, &child_ctx);
                if (host_colors & *colors & ColorMask::ALL_COLORS).is_empty() {
                    return false;
                }
            }
            true
        }

        Condition::WasCast { from_hand } => {
            let Some(src_id) = child_ctx.source else {
                return false;
            };
            let Some(obj) = state.object(src_id) else {
                return false;
            };
            if !obj.flags.contains(ObjectFlags::WAS_CAST) {
                return false;
            }
            if *from_hand {
                state
                    .rare(src_id)
                    .is_some_and(|r| r.cast_from_zone == Some(Zone::Hand))
            } else {
                true
            }
        }
        Condition::OpponentCount { op, count } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let n = state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .count() as i32;
            compare_i32(n, *op, i32::from(*count))
        }
        Condition::AnyPlayerLife { op, count } => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            state
                .players()
                .iter()
                .any(|p| !p.lost && compare_i32(p.life, *op, *count))
        }
        Condition::AnyOpponentMetric { metric, op, count } => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::PLAYER_STATE | DirtyMask::GRAVEYARDS);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .any(|p| {
                    let v = match metric {
                        OpponentMetricKind::Life => p.life,
                        OpponentMetricKind::Hand => p.hand.len() as i32,
                        OpponentMetricKind::Graveyard => p.graveyard.len() as i32,
                    };
                    compare_i32(v, *op, *count)
                })
        }
        Condition::DistinctControlledCreaturePowers { count } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut seen: Vec<i32> = Vec::new();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                    if !types.contains(CardTypeMask::CREATURE) {
                        continue;
                    }
                    if let Some(pt) = object_pt(state, id, &child_ctx)
                        && !seen.contains(&pt.power)
                    {
                        seen.push(pt.power);
                        if seen.len() >= usize::from(*count) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        Condition::LkiHasNoCounter { counter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let count = child_ctx
                .event_lki
                .or_else(|| child_ctx.source_lki())
                .map_or_else(
                    || {
                        child_ctx
                            .source
                            .map_or(0, |id| state.counter_count(id, *counter))
                    },
                    |l| {
                        l.counters
                            .iter()
                            .find(|(cid, _)| cid == counter)
                            .map_or(0, |(_, c)| *c)
                    },
                );
            count == 0
        }
        Condition::HadCounterOnIt { counter } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let snap = child_ctx.event_lki.or_else(|| child_ctx.source_lki());
            if let Some(lki) = snap {
                count_counters_slice(&lki.counters, *counter) > 0
            } else {
                child_ctx
                    .source
                    .is_some_and(|id| count_counters_slice(state.counters(id), *counter) > 0)
            }
        }
        Condition::LkiToughnessLessThan { max } => {
            let snap = child_ctx.event_lki.or_else(|| child_ctx.source_lki());
            snap.and_then(|l| l.pt)
                .is_some_and(|pt| pt.toughness < *max)
        }
        Condition::ManaColorSpentToCast { color, min_total } => {
            let mask = paid_mana_colors_for_ctx(state, &child_ctx);
            if let Some(c) = color {
                let bit = ColorMask::from_mana_color(*c);
                if !mask.contains(bit) {
                    return false;
                }
            }
            if let Some(min) = min_total {
                let spent = child_ctx
                    .source
                    .and_then(|id| state.rare(id).map(|r| r.mana_spent_count))
                    .unwrap_or(0);
                if spent < u32::from(*min) {
                    return false;
                }
            }
            true
        }
        Condition::SameColorManaSpentAtLeast { min, color } => {
            let mask = paid_mana_colors_for_ctx(state, &child_ctx);
            if let Some(c) = color {
                mask.contains(ColorMask::from_mana_color(*c))
                    && child_ctx
                        .source
                        .and_then(|id| state.rare(id).map(|r| r.mana_spent_count))
                        .unwrap_or(0)
                        >= u32::from(*min)
            } else {
                !mask.is_empty() && *min <= 1
            }
        }
        Condition::ManaSpentGreaterThanSelfPower => {
            let Some(src_id) = child_ctx.source else {
                return false;
            };
            let spent = state
                .rare(src_id)
                .map_or(0, |r| r.mana_spent_count as i32);
            let pow = object_pt(state, src_id, &child_ctx).map_or(0, |pt| pt.power);
            spent > pow
        }
        Condition::OpponentComparison { metric } => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::PLAYER_STATE | DirtyMask::TOPOLOGY);
            let you_val = player_comparison_metric(state, child_ctx.controller, *metric, &child_ctx);
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .any(|p| player_comparison_metric(state, p.id, *metric, &child_ctx) > you_val)
        }
        Condition::OpponentControlsAtLeastNMore { filter, n } => {
            let you_count = find_objects(state, filter, &child_ctx)
                .into_iter()
                .filter(|&id| {
                    effective_controller_for_object(state, id, &child_ctx)
                        == Some(child_ctx.controller)
                })
                .count();
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .any(|p| {
                    let p_ctx = EvalContext {
                        controller: p.id,
                        ..child_ctx
                    };
                    let opp_count = find_objects(state, filter, &p_ctx)
                        .into_iter()
                        .filter(|&id| {
                            effective_controller_for_object(state, id, &p_ctx) == Some(p.id)
                        })
                        .count();
                    opp_count >= you_count + usize::from(*n)
                })
        }
        Condition::OpponentHasMoreCardsInHandThan { who } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(target_pid) = resolve_player_ref(state, who, &child_ctx) else {
                return false;
            };
            let target_hand = state
                .players()
                .get(usize::from(target_pid.0))
                .map_or(0, |p| p.hand.len());
            state
                .players()
                .iter()
                .any(|p| !p.lost && p.id != child_ctx.controller && p.hand.len() > target_hand)
        }
        Condition::MoreCardsInHandThanEachOpponent => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let you_hand = state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(0, |p| p.hand.len());
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != child_ctx.controller)
                .all(|p| you_hand > p.hand.len())
        }
        Condition::YouHaveMostLife => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            let you_life = state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .map_or(i32::MIN, |p| p.life);
            let max_life = state
                .players()
                .iter()
                .filter(|p| !p.lost)
                .map(|p| p.life)
                .max()
                .unwrap_or(i32::MIN);
            you_life == max_life
        }
        Condition::YourLifeLessThanStarting => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .is_some_and(|p| p.life < state.config().starting_life)
        }
        Condition::LifeAtLeastAboveStarting { delta } => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .is_some_and(|p| p.life >= state.config().starting_life + *delta)
        }
        Condition::LifeCompareHalfStarting { who, op } => {
            child_ctx.board.record_read(DirtyMask::LIFE);
            let half = state.config().starting_life.div_euclid(2);
            match who {
                LifeCompareWho::You => state
                    .players()
                    .get(usize::from(child_ctx.controller.0))
                    .is_some_and(|p| compare_i32(p.life, *op, half)),
                LifeCompareWho::AnyOpponent => state
                    .players()
                    .iter()
                    .any(|p| !p.lost && p.id != child_ctx.controller && compare_i32(p.life, *op, half)),
            }
        }
        Condition::ClassLevelIs { level } => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(obj) = state.object(src) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || obj.is_face_down() {
                return false;
            }
            state
                .rare(src)
                .and_then(|r| r.class_level)
                .unwrap_or(1)
                == u16::from(*level)
        }
        Condition::ClassLevelAtLeast { level } => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::DESIGNATIONS);
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(obj) = state.object(src) else {
                return false;
            };
            if obj.zone != Zone::Battlefield || obj.is_face_down() {
                return false;
            }
            state
                .rare(src)
                .and_then(|r| r.class_level)
                .unwrap_or(1)
                >= u16::from(*level)
        }
        Condition::CaseSolved => {
            child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
            child_ctx
                .source
                .and_then(|id| state.object(id))
                .is_some_and(|o| o.zone == Zone::Battlefield && o.flags.contains(ObjectFlags::SOLVED))
        }
        Condition::CaseUnsolved => {
            child_ctx.board.record_read(DirtyMask::DESIGNATIONS);
            child_ctx
                .source
                .and_then(|id| state.object(id))
                .is_some_and(|o| o.zone == Zone::Battlefield && !o.flags.contains(ObjectFlags::SOLVED))
        }
        Condition::TotalPowerControlled { min } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut total: i32 = 0;
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                    if types.contains(CardTypeMask::CREATURE)
                        && let Some(pt) = object_pt(state, id, &child_ctx)
                    {
                        total = total.saturating_add(pt.power);
                    }
                }
            }
            total >= *min
        }
        Condition::ColorsAmongControlledPermanents { min } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut mask = ColorMask::empty();
            for p in state.players() {
                if p.lost {
                    continue;
                }
                for &id in &p.battlefield {
                    let Some(obj) = state.object(id) else {
                        continue;
                    };
                    if obj.is_phased_out()
                        || effective_controller_for_object(state, id, &child_ctx)
                            != Some(child_ctx.controller)
                    {
                        continue;
                    }
                    mask |= object_colors_in_ctx(state, id, &child_ctx) & ColorMask::ALL_COLORS;
                }
            }
            mask.color_count() >= u32::from(*min)
        }
        Condition::SelfInZone { zone } => {
            if let Some(z_ov) = child_ctx.zone_override {
                return z_ov == *zone;
            }
            child_ctx
                .source
                .and_then(|id| state.object(id))
                .is_some_and(|o| o.zone == *zone)
        }
        Condition::OpponentPoisonAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .iter()
                .any(|p| !p.lost && p.id != child_ctx.controller && p.poison >= *min)
        }
        Condition::EnteredWithXAtLeast { n } => {
            let x = child_ctx
                .stack_item
                .map(|s| s.x_value as i32)
                .or_else(|| {
                    child_ctx
                        .source
                        .and_then(|id| state.rare(id)?.entered_with_x.map(|x| x as i32))
                })
                .unwrap_or(0);
            x >= *n
        }
        Condition::AttackingTotalPowerAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::COMBAT | DirtyMask::TOPOLOGY);
            let total: i32 = state
                .combat()
                .attackers
                .iter()
                .filter_map(|a| object_pt(state, a.attacker, &child_ctx).map(|pt| pt.power))
                .sum();
            total >= *min
        }
        Condition::AttackingCount { op, n, filter } => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            let cnt = state
                .combat()
                .attackers
                .iter()
                .filter(|a| {
                    filter.as_deref().is_none_or(|f| {
                        matches_object_filter(state, a.attacker, f, &child_ctx)
                    })
                })
                .count() as i32;
            compare_i32(cnt, *op, i32::from(*n))
        }
        Condition::AttackingYouCount { op, n } => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            let cnt = state
                .combat()
                .attackers
                .iter()
                .filter(|a| match a.target {
                    crate::action::AttackTarget::Player { id } => id == child_ctx.controller,
                    crate::action::AttackTarget::Planeswalker { id }
                    | crate::action::AttackTarget::Battle { id } => {
                        effective_controller_for_object(state, id, &child_ctx)
                            == Some(child_ctx.controller)
                    }
                })
                .count() as i32;
            compare_i32(cnt, *op, i32::from(*n))
        }
        Condition::YouWereAttackedThisCombat => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            state.combat().attackers.iter().any(|a| {
                matches!(a.target, crate::action::AttackTarget::Player { id } if id == child_ctx.controller)
            })
        }
        Condition::EventPlayerHandCount { op, count } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(ep) = child_ctx.event_player else {
                return false;
            };
            let hand_len = state
                .players()
                .get(usize::from(ep.0))
                .map_or(0, |p| p.hand.len() as i32);
            compare_i32(hand_len, *op, i32::from(*count))
        }
        Condition::ControlledByOtherThanOwner => {
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(obj) = state.object(src) else {
                return false;
            };
            effective_controller_for_object(state, src, &child_ctx) != Some(obj.owner)
        }
        Condition::EventObjectMatchedLki { filter } => {
            let Some(lki) = child_ctx.event_lki else {
                if let Some(ev_id) = child_ctx.event_object {
                    return matches_object_filter(state, ev_id, filter, &child_ctx);
                }
                return false;
            };
            crate::filter::matches_lki_filter(state, lki, filter, &child_ctx)
        }
        Condition::MostCommonColorAmongPermanents { color, allow_tie } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut counts = [0u32; 5]; // W, U, B, R, G
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
                    let c = object_colors_in_ctx(state, id, &child_ctx);
                    if c.contains(ColorMask::W) {
                        counts[0] += 1;
                    }
                    if c.contains(ColorMask::U) {
                        counts[1] += 1;
                    }
                    if c.contains(ColorMask::B) {
                        counts[2] += 1;
                    }
                    if c.contains(ColorMask::R) {
                        counts[3] += 1;
                    }
                    if c.contains(ColorMask::G) {
                        counts[4] += 1;
                    }
                }
            }
            let max = counts.iter().copied().max().unwrap_or(0);
            if max == 0 {
                return false;
            }
            let target_idx = match color {
                ManaColor::W => 0,
                ManaColor::U => 1,
                ManaColor::B => 2,
                ManaColor::R => 3,
                ManaColor::G => 4,
                ManaColor::C => return false,
            };
            if counts[target_idx] != max {
                return false;
            }
            if *allow_tie {
                true
            } else {
                counts.iter().filter(|&&c| c == max).count() == 1
            }
        }
        Condition::EnteredUnderYourControlThisTurn { filter, min } => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY);
            let needed = usize::from(min.unwrap_or(1));
            let bf_ctx = EvalContext {
                zone_override: Some(Zone::Battlefield),
                ..child_ctx
            };
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return false;
            };
            let count = p
                .battlefield
                .iter()
                .copied()
                .filter(|&id| {
                    state.object(id).is_some_and(|o| {
                        !o.is_phased_out() && o.flags.contains(ObjectFlags::ENTERED_THIS_TURN)
                    }) && matches_object_filter(state, id, filter, &bf_ctx)
                })
                .count();
            count >= needed
        }
        Condition::SelfEnteredThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY);
            child_ctx
                .source
                .and_then(|id| state.object(id))
                .is_some_and(|o| {
                    o.zone == Zone::Battlefield && o.flags.contains(ObjectFlags::ENTERED_THIS_TURN)
                })
        }
        Condition::EventObjectEnteredThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY);
            child_ctx
                .event_object
                .and_then(|id| state.object(id))
                .is_some_and(|o| {
                    o.zone == Zone::Battlefield && o.flags.contains(ObjectFlags::ENTERED_THIS_TURN)
                })
        }
        Condition::DistinctManaValuesInGraveyardAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::GRAVEYARDS);
            let mut mvs: Vec<u32> = Vec::new();
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return false;
            };
            for &id in &p.graveyard {
                let mv = object_mana_value(state, id, &child_ctx);
                if !mvs.contains(&mv) {
                    mvs.push(mv);
                    if mvs.len() >= usize::from(*min) {
                        return true;
                    }
                }
            }
            false
        }
        Condition::AnyGraveyardAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::GRAVEYARDS);
            state
                .players()
                .iter()
                .any(|p| p.graveyard.len() >= usize::from(*min))
        }
        Condition::ControlledTargetAtResolution { target_index } => {
            match child_ctx.target_at(*target_index) {
                Some(TargetRef::Object { id }) => {
                    effective_controller_for_object(state, id, &child_ctx)
                        == Some(child_ctx.controller)
                }
                _ => false,
            }
        }
        Condition::IsAttackingAlone { of } => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            let id = match of {
                AttachmentOwnerTarget::SelfObj => child_ctx.source,
                AttachmentOwnerTarget::AttachedTo => {
                    child_ctx.source.and_then(|s| state.object(s)?.attached_to)
                }
            };
            let Some(id) = id else {
                return false;
            };
            let attackers = &state.combat().attackers;
            attackers.len() == 1 && attackers[0].attacker == id
        }
        Condition::NoMatchingCardsInHand { filter } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return true;
            };
            let hand_ctx = EvalContext {
                zone_override: Some(Zone::Hand),
                ..child_ctx
            };
            !p.hand
                .iter()
                .any(|&id| matches_object_filter(state, id, filter, &hand_ctx))
        }
        Condition::OnlyNamedOrNoMatchingInHand { filter } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return true;
            };
            let src_name = child_ctx
                .source
                .map(|s| object_name_in_ctx(state, s, &child_ctx))
                .unwrap_or_default();
            let hand_ctx = EvalContext {
                zone_override: Some(Zone::Hand),
                ..child_ctx
            };
            !p.hand.iter().any(|&id| {
                if !matches_object_filter(state, id, filter, &hand_ctx) {
                    return false;
                }
                let nm = object_name_in_ctx(state, id, &hand_ctx);
                src_name.is_empty() || !nm.eq_ignore_ascii_case(&src_name)
            })
        }
        Condition::AbilityTargetsFilter { filter } => {
            child_ctx.effective_targets().iter().any(|t| {
                matches!(t, TargetRef::Object { id } if matches_object_filter(state, *id, filter, &child_ctx))
            })
        }
        Condition::TargetsStackSpell { filter } => {
            child_ctx.effective_targets().iter().any(|t| match t {
                TargetRef::Stack { id } => {
                    let Some(entry) = state.stack_entries().iter().find(|e| e.id == *id) else {
                        return false;
                    };
                    let StackEntryKind::Spell { object } = entry.kind else {
                        return false;
                    };
                    filter.as_deref().is_none_or(|f| {
                        matches_object_filter(state, object, f, &child_ctx)
                    })
                }
                TargetRef::Object { id } => {
                    state.object(*id).is_some_and(|o| o.zone == Zone::Stack)
                        && filter
                            .as_deref()
                            .is_none_or(|f| matches_object_filter(state, *id, f, &child_ctx))
                }
                TargetRef::Player { .. } => false,
            })
        }
        Condition::EnchantedCreaturePowerAtLeast { min } => {
            let Some(host) = child_ctx.source.and_then(|s| state.object(s)?.attached_to) else {
                return false;
            };
            object_pt(state, host, &child_ctx).is_some_and(|pt| pt.power >= *min)
        }
        Condition::AllLandsAreSubtype { subtype } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let mut any_land = false;
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
                    let (types, subtypes, _) = object_types_subtypes(state, id, &child_ctx);
                    if types.contains(CardTypeMask::LAND) {
                        any_land = true;
                        if !subtypes.contains(subtype) {
                            return false;
                        }
                    }
                }
            }
            any_land
        }
        Condition::YouControlGreatestManaValueFilter { filter } => {
            let mut max_mv: Option<u32> = None;
            let mut you_has_max = false;
            for id in find_objects(state, filter, &child_ctx) {
                let mv = object_mana_value(state, id, &child_ctx);
                let ctrl = effective_controller_for_object(state, id, &child_ctx);
                match max_mv {
                    None => {
                        max_mv = Some(mv);
                        you_has_max = ctrl == Some(child_ctx.controller);
                    }
                    Some(cur) if mv > cur => {
                        max_mv = Some(mv);
                        you_has_max = ctrl == Some(child_ctx.controller);
                    }
                    Some(cur) if mv == cur && ctrl == Some(child_ctx.controller) => {
                        you_has_max = true;
                    }
                    _ => {}
                }
            }
            you_has_max
        }
        Condition::YourTeamControlsOtherFilter { filter } => {
            let bf_ctx = EvalContext {
                zone_override: Some(Zone::Battlefield),
                ..child_ctx
            };
            find_objects(state, filter, &bf_ctx).into_iter().any(|id| {
                Some(id) != child_ctx.source
                    && effective_controller_for_object(state, id, &bf_ctx)
                        == Some(child_ctx.controller)
            })
        }
        Condition::PlayersLostGameAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state.players().iter().filter(|p| p.lost).count() >= usize::from(*min)
        }
        Condition::SelfAttackedThisCombat => {
            child_ctx.board.record_read(DirtyMask::COMBAT);
            child_ctx.source.is_some_and(|src| {
                state
                    .combat()
                    .attacked_this_combat
                    .contains(&src)
                    || state.combat().attackers.iter().any(|a| a.attacker == src)
            })
        }
        Condition::HasCardsExiledWithSelf => {
            child_ctx.board.record_read(DirtyMask::EXILE);
            let Some(src) = child_ctx.source else {
                return false;
            };
            state.players().iter().any(|p| {
                p.exile
                    .iter()
                    .any(|&ex| state.rare(ex).is_some_and(|r| r.exiled_by == Some(src)))
            })
        }
        Condition::NoSameNameControlledOrInGraveyard => {
            child_ctx
                .board
                .record_read(DirtyMask::TOPOLOGY | DirtyMask::GRAVEYARDS);
            let Some(src) = child_ctx.source else {
                return false;
            };
            let src_name = object_name_in_ctx(state, src, &child_ctx);
            if src_name.is_empty() {
                return true;
            }
            for p in state.players() {
                if !p.lost {
                    for &id in &p.battlefield {
                        if id == src {
                            continue;
                        }
                        if state.object(id).is_some_and(|o| !o.is_phased_out())
                            && effective_controller_for_object(state, id, &child_ctx)
                                == Some(child_ctx.controller)
                            && object_name_in_ctx(state, id, &child_ctx)
                                .eq_ignore_ascii_case(&src_name)
                        {
                            return false;
                        }
                    }
                }
                if p.id == child_ctx.controller {
                    for &gid in &p.graveyard {
                        if gid == src {
                            continue;
                        }
                        if object_name_in_ctx(state, gid, &child_ctx).eq_ignore_ascii_case(&src_name)
                        {
                            return false;
                        }
                    }
                }
            }
            true
        }
        Condition::CountersAmongFilterAtLeast { filter, min } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            let total: i32 = find_objects(state, filter, &child_ctx)
                .into_iter()
                .map(|id| count_counters_slice(state.counters(id), CounterTarget::Any))
                .sum();
            total >= i32::from(*min)
        }
        Condition::DidntActivateLoyaltyThisTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            child_ctx.source.is_some_and(|src| {
                !state
                    .rare(src)
                    .is_some_and(|r| r.used_loyalty_this_turn)
            })
        }
        Condition::SameNameLandsControlledAtLeast { min } => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let Some(p) = state.players().get(usize::from(child_ctx.controller.0)) else {
                return false;
            };
            let mut names: Vec<(String, u16)> = Vec::new();
            for &id in &p.battlefield {
                let Some(obj) = state.object(id) else {
                    continue;
                };
                if obj.is_phased_out() || obj.is_face_down() {
                    continue;
                }
                let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                if !types.contains(CardTypeMask::LAND) {
                    continue;
                }
                let n = object_name_in_ctx(state, id, &child_ctx);
                if n.is_empty() {
                    continue;
                }
                if let Some((_, count)) = names.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&n))
                {
                    *count += 1;
                    if *count >= *min {
                        return true;
                    }
                } else {
                    if *min <= 1 {
                        return true;
                    }
                    names.push((n, 1));
                }
            }
            false
        }
        Condition::PlayedLandThisTurn => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .is_some_and(|p| p.lands_played_this_turn > 0)
        }
        Condition::SourceHasCounter { counter, min } => {
            child_ctx.board.record_read(DirtyMask::COUNTERS);
            child_ctx.source.is_some_and(|src| {
                state.counter_count(src, *counter) >= u32::from(min.unwrap_or(1))
            })
        }
        Condition::PlayerHasFewerHandThan { target_player, max } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let pid = target_player
                .as_ref()
                .and_then(|pr| resolve_player_ref(state, pr, &child_ctx))
                .unwrap_or(child_ctx.controller);
            state
                .players()
                .get(usize::from(pid.0))
                .is_some_and(|p| p.hand.len() < usize::from(max.unwrap_or(7)))
        }
        Condition::PlayerHasFewerCountersThan { counter, max } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state
                .players()
                .get(usize::from(child_ctx.controller.0))
                .is_some_and(|p| player_counter_value(p, *counter) < i32::from(*max))
        }
        Condition::TargetHasFewerCountersThan { counter, max } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(TargetRef::Player { id }) = child_ctx.effective_targets().first() else {
                return false;
            };
            state
                .players()
                .get(usize::from(id.0))
                .is_some_and(|p| player_counter_value(p, *counter) < i32::from(*max))
        }
        Condition::SpellCastFromZone { zone } => child_ctx.source.is_some_and(|src| {
            state
                .rare(src)
                .is_some_and(|r| r.cast_from_zone == Some(*zone))
        }),
        Condition::EventEnteredOrCastFromZone { zone } => child_ctx
            .event_object
            .is_some_and(|id| {
                state
                    .rare(id)
                    .is_some_and(|r| r.cast_from_zone == Some(*zone))
            }),
        Condition::EventEnteredOrCastFromGraveyardAny => child_ctx
            .event_object
            .is_some_and(|id| {
                state
                    .rare(id)
                    .is_some_and(|r| r.cast_from_zone == Some(Zone::Graveyard))
            }),
        Condition::EventObjectPowerOrToughnessGreaterThanSelf => {
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(self_pt) = object_pt(state, src, &child_ctx) else {
                return false;
            };
            let ev_pt = child_ctx
                .event_object
                .and_then(|id| object_pt(state, id, &child_ctx))
                .or_else(|| child_ctx.event_lki.and_then(|l| l.pt));
            ev_pt.is_some_and(|pt| pt.power > self_pt.power || pt.toughness > self_pt.toughness)
        }
        Condition::EventObjectWasNotSubtype { subtype } => {
            if let Some(ev_id) = child_ctx.event_object
                && let Some(obj) = state.object(ev_id)
            {
                let eff_zone = child_ctx.zone_override.unwrap_or(obj.zone);
                if eff_zone == Zone::Battlefield
                    && !matches!(child_ctx.board, BoardView::BaseOnly)
                    && let Some(ev) = child_ctx.board.resolve_eval(state, ev_id)
                {
                    let has_sub = ev.subtypes.contains(subtype)
                        || (ev.has_all_creature_types
                            && ev
                                .types
                                .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                            && !state
                                .db()
                                .subtype_name(*subtype)
                                .is_some_and(is_non_creature_subtype));
                    return !has_sub;
                }
                let chars = off_battlefield_characteristics(state, ev_id, obj, eff_zone, &child_ctx);
                return !chars.has_subtype(state.db(), *subtype);
            }
            if let Some(lki) = child_ctx.event_lki {
                let has_sub = lki.subtypes.contains(subtype)
                    || (lki.has_all_creature_types
                        && lki
                            .types
                            .intersects(CardTypeMask::CREATURE | CardTypeMask::KINDRED)
                        && !state
                            .db()
                            .subtype_name(*subtype)
                            .is_some_and(is_non_creature_subtype));
                return !has_sub;
            }
            false
        }
        Condition::EventSpellMatchesFilter { filter } => child_ctx
            .event_object
            .is_some_and(|id| matches_object_filter(state, id, filter, &child_ctx)),
        Condition::EventPlayerControlsMoreThanYou { filter } => {
            let Some(ep) = child_ctx.event_player else {
                return false;
            };
            let you_cnt = find_objects(state, filter, &child_ctx)
                .into_iter()
                .filter(|&id| {
                    effective_controller_for_object(state, id, &child_ctx)
                        == Some(child_ctx.controller)
                })
                .count();
            let ep_cnt = find_objects(state, filter, &child_ctx)
                .into_iter()
                .filter(|&id| effective_controller_for_object(state, id, &child_ctx) == Some(ep))
                .count();
            ep_cnt > you_cnt
        }
        Condition::EventPlayerControlsMoreThanEachOther { filter } => {
            let Some(ep) = child_ctx.event_player else {
                return false;
            };
            let all_matches = find_objects(state, filter, &child_ctx);
            let ep_cnt = all_matches
                .iter()
                .filter(|&&id| effective_controller_for_object(state, id, &child_ctx) == Some(ep))
                .count();
            state
                .players()
                .iter()
                .filter(|p| !p.lost && p.id != ep)
                .all(|p| {
                    let other_cnt = all_matches
                        .iter()
                        .filter(|&&id| {
                            effective_controller_for_object(state, id, &child_ctx) == Some(p.id)
                        })
                        .count();
                    ep_cnt > other_cnt
                })
        }
        Condition::EventObjectGreatestPowerAmongAll => {
            let ev_pow = child_ctx
                .event_object
                .and_then(|id| object_pt(state, id, &child_ctx))
                .or_else(|| child_ctx.event_lki.and_then(|l| l.pt))
                .map(|pt| pt.power);
            let Some(ev_pow) = ev_pow else {
                return false;
            };
            let max_pow = state
                .players()
                .iter()
                .filter(|p| !p.lost)
                .flat_map(|p| p.battlefield.iter().copied())
                .filter(|&id| state.object(id).is_some_and(|o| !o.is_phased_out()))
                .filter_map(|id| {
                    let (types, _, _) = object_types_subtypes(state, id, &child_ctx);
                    types
                        .contains(CardTypeMask::CREATURE)
                        .then(|| object_pt(state, id, &child_ctx).map(|pt| pt.power))
                        .flatten()
                })
                .max();
            max_pow.is_none_or(|m| ev_pow >= m)
        }
        Condition::EventSharesCardTypeWithLinkedExile => {
            let Some(src) = child_ctx.source else {
                return false;
            };
            let ev_types = child_ctx
                .event_object
                .map(|id| object_types_subtypes(state, id, &child_ctx).0)
                .or_else(|| child_ctx.event_lki.map(|l| l.types))
                .unwrap_or(CardTypeMask::empty());
            if ev_types.is_empty() {
                return false;
            }
            state.players().iter().any(|p| {
                p.exile.iter().any(|&ex| {
                    state.rare(ex).is_some_and(|r| r.exiled_by == Some(src))
                        && !(object_types_subtypes(state, ex, &child_ctx).0 & ev_types).is_empty()
                })
            })
        }
        Condition::EventSpellWasKicked => child_ctx
            .event_object
            .is_some_and(|id| state.rare(id).is_some_and(|r| r.kicked_count > 0)),
        Condition::EventSpellNoColoredManaSpent => child_ctx.event_object.is_some_and(|id| {
            state
                .rare(id)
                .is_none_or(|r| (r.paid_mana_colors & ColorMask::ALL_COLORS).is_empty())
        }),
        Condition::EventSpellManaSpentLessThanMv => {
            let Some(ev_id) = child_ctx.event_object else {
                return false;
            };
            let spent = state.rare(ev_id).map_or(0, |r| r.mana_spent_count);
            let mv = object_mana_value(state, ev_id, &child_ctx);
            spent < mv
        }
        Condition::EventSpellHasKeyword { keyword } => {
            let Some(ev_id) = child_ctx.event_object else {
                return false;
            };
            let Some(kw_id) = state.db().keyword_id(keyword) else {
                return false;
            };
            let Some(obj) = state.object(ev_id) else {
                return false;
            };
            let eff_zone = child_ctx.zone_override.unwrap_or(obj.zone);
            if eff_zone == Zone::Battlefield
                && !matches!(child_ctx.board, BoardView::BaseOnly)
                && let Some(ev) = child_ctx.board.resolve_eval(state, ev_id)
            {
                return ev.keywords.contains(&kw_id);
            }
            let chars = off_battlefield_characteristics(state, ev_id, obj, eff_zone, &child_ctx);
            chars.keywords.contains(&kw_id)
        }
        Condition::ControlAttackingLegendary => {
            child_ctx.board.record_read(DirtyMask::COMBAT | DirtyMask::TOPOLOGY);
            state.combat().attackers.iter().any(|a| {
                effective_controller_for_object(state, a.attacker, &child_ctx)
                    == Some(child_ctx.controller)
                    && supertypes_in_ctx(state, a.attacker, &child_ctx)
                        .contains(SupertypeMask::LEGENDARY)
            })
        }

        Condition::EvolveCompare => {
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(src_obj) = state.object(src) else {
                return false;
            };
            if src_obj.zone != Zone::Battlefield || src_obj.is_phased_out() {
                return false;
            }
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let (src_types, _, _) = object_types_subtypes(state, src, &child_ctx);
            if !src_types.contains(CardTypeMask::CREATURE) {
                return false;
            }
            let Some(self_pt) = object_pt(state, src, &child_ctx) else {
                return false;
            };
            let ev_pt = if let Some(ev_id) = child_ctx.event_object
                && state
                    .object(ev_id)
                    .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            {
                object_pt(state, ev_id, &child_ctx)
            } else if let Some(lki) = child_ctx.event_lki {
                lki.pt
            } else {
                child_ctx
                    .event_object
                    .and_then(|ev_id| object_pt(state, ev_id, &child_ctx))
            };
            ev_pt.is_some_and(|pt| pt.power > self_pt.power || pt.toughness > self_pt.toughness)
        }
        Condition::TrainingGreaterAttacker => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TOPOLOGY);
            let Some(src) = child_ctx.source else {
                return false;
            };
            let Some(src_obj) = state.object(src) else {
                return false;
            };
            if src_obj.zone != Zone::Battlefield || src_obj.is_phased_out() {
                return false;
            }
            let Some(self_pt) = object_pt(state, src, &child_ctx) else {
                return false;
            };
            state.combat().attackers.iter().any(|a| {
                a.attacker != src
                    && state
                        .object(a.attacker)
                        .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
                    && effective_controller_for_object(state, a.attacker, &child_ctx)
                        == Some(child_ctx.controller)
                    && object_pt(state, a.attacker, &child_ctx)
                        .is_some_and(|pt| pt.power > self_pt.power)
            })
        }
        Condition::DethroneMostLife => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::COMBAT);
            let def_pid = child_ctx.defending_player.or_else(|| {
                let src = child_ctx.source?;
                state
                    .combat()
                    .attackers
                    .iter()
                    .find(|a| a.attacker == src)
                    .and_then(|a| match a.target {
                        crate::action::AttackTarget::Player { id } => Some(id),
                        crate::action::AttackTarget::Planeswalker { .. }
                        | crate::action::AttackTarget::Battle { .. } => None,
                    })
            });
            def_pid.is_some_and(|pid| is_player_tied_for_most_life(state, pid))
        }
        Condition::EventWasAttachedEnchantedOrEquipped => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            let db = state.db();
            let aura_sid = db.subtype_id("Aura");
            let eq_sid = db.subtype_id("Equipment");
            if let Some(lki) = child_ctx.event_lki {
                for &att_id in &lki.attached_ids {
                    if state.object(att_id).is_some() {
                        let (_, subs, _) = object_types_subtypes(state, att_id, &child_ctx);
                        if aura_sid.is_some_and(|s| subs.contains(&s))
                            || eq_sid.is_some_and(|s| subs.contains(&s))
                        {
                            return true;
                        }
                    } else if let Some(att_lki) = state.lki(att_id)
                        && (aura_sid.is_some_and(|s| att_lki.subtypes.contains(&s))
                            || eq_sid.is_some_and(|s| att_lki.subtypes.contains(&s)))
                    {
                        return true;
                    }
                }
            }
            if let Some(ev_id) = child_ctx.event_object
                && state
                    .object(ev_id)
                    .is_some_and(|o| o.zone == Zone::Battlefield && !o.is_phased_out())
            {
                return has_attachment_subtype(state, ev_id, "Aura", &child_ctx)
                    || has_attachment_subtype(state, ev_id, "Equipment", &child_ctx);
            }
            false
        }
        Condition::EventAttackingPlayerWithMostLife => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::COMBAT);
            let def_pid = child_ctx
                .defending_player
                .or(child_ctx.event_player)
                .or_else(|| {
                    let eid = child_ctx.event_object?;
                    state
                        .combat()
                        .attackers
                        .iter()
                        .find(|a| a.attacker == eid)
                        .and_then(|a| match a.target {
                            crate::action::AttackTarget::Player { id } => Some(id),
                            crate::action::AttackTarget::Planeswalker { .. }
                            | crate::action::AttackTarget::Battle { .. } => None,
                        })
                });
            def_pid.is_some_and(|pid| is_player_tied_for_most_life(state, pid))
        }
        Condition::EventAttackerHasAnotherUnattackedOpponent => {
            child_ctx.board.record_read(
                DirtyMask::COMBAT | DirtyMask::PLAYER_STATE | DirtyMask::TURN_HISTORY,
            );
            let active_id = state.active_player();
            let mut attacked_opponents: SmallVec<[PlayerId; 4]> = SmallVec::new();
            for a in &state.combat().attackers {
                let def_pid = match a.target {
                    crate::action::AttackTarget::Player { id } => Some(id),
                    crate::action::AttackTarget::Planeswalker { id }
                    | crate::action::AttackTarget::Battle { id } => {
                        effective_controller_for_object(state, id, &child_ctx)
                    }
                };
                if let Some(dp) = def_pid
                    && dp != active_id
                    && !attacked_opponents.contains(&dp)
                {
                    attacked_opponents.push(dp);
                }
            }
            state
                .players()
                .iter()
                .any(|p| !p.lost && p.id != active_id && !attacked_opponents.contains(&p.id))
        }
        Condition::EventMinAttackersToYouOrPlaneswalkers { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TOPOLOGY);
            let count = state
                .combat()
                .attackers
                .iter()
                .filter(|a| match a.target {
                    crate::action::AttackTarget::Player { id } => id == child_ctx.controller,
                    crate::action::AttackTarget::Planeswalker { id } => {
                        effective_controller_for_object(state, id, &child_ctx)
                            == Some(child_ctx.controller)
                    }
                    crate::action::AttackTarget::Battle { .. } => false,
                })
                .count();
            count >= usize::from(*min)
        }
        Condition::EventObjectMatchesOrSubtype { filter } => {
            if let Some(ev_id) = child_ctx.event_object
                && state.object(ev_id).is_some()
            {
                let ev_ctx = EvalContext {
                    zone_override: None,
                    ..child_ctx
                };
                return matches_object_filter(state, ev_id, filter, &ev_ctx);
            }
            if let Some(lki) = child_ctx.event_lki {
                return matches_lki_filter(state, lki, filter, &child_ctx);
            }
            false
        }
        Condition::AnyAttackedPlayerWithMostLife => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::COMBAT);
            let Some(max_life) = state
                .players()
                .iter()
                .filter(|p| !p.lost)
                .map(|p| p.life)
                .max()
            else {
                return false;
            };
            state.combat().attackers.iter().any(|a| {
                matches!(a.target, crate::action::AttackTarget::Player { id } if state.players().get(usize::from(id.0)).is_some_and(|p| !p.lost && p.life >= max_life))
            })
        }
        Condition::SelfRemovedFromCombatOrAttached => {
            child_ctx.board.record_read(DirtyMask::TOPOLOGY);
            child_ctx
                .source
                .and_then(|id| state.object(id))
                .is_some_and(|o| {
                    o.zone == Zone::Battlefield && !o.is_phased_out() && o.attached_to.is_some()
                })
        }

        // M5a / M4 / M5b backed designations, turn history, combat, and step-way stats
        Condition::SpellWasBargained => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            child_ctx.stack_item.is_some_and(|s| s.choices.bargained)
                || child_ctx
                    .source
                    .and_then(|id| state.rare(id))
                    .is_some_and(|r| r.bargained)
        }
        Condition::AdditionalCostPaid { mana_cost: _ } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            child_ctx
                .stack_item
                .is_some_and(|s| s.choices.additional_cost_paid)
                || child_ctx
                    .source
                    .and_then(|id| state.rare(id))
                    .is_some_and(|r| r.additional_cost_paid)
        }
        Condition::AltCostPaid { mana_cost: _ } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            child_ctx
                .stack_item
                .is_some_and(|s| s.choices.alt_cast_kind != AltCastKind::Normal)
                || child_ctx
                    .source
                    .and_then(|id| state.rare(id))
                    .is_some_and(|r| r.alt_cast_kind != AltCastKind::Normal)
        }
        Condition::PaidNamedAltCost { kind } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            let alt = child_ctx
                .stack_item
                .map(|s| s.choices.alt_cast_kind)
                .or_else(|| {
                    child_ctx
                        .source
                        .and_then(|id| state.rare(id).map(|r| r.alt_cast_kind))
                })
                .unwrap_or(AltCastKind::Normal);
            let _ = kind;
            match kind {
                NamedAltCostKind::Surge
                | NamedAltCostKind::Prowl
                | NamedAltCostKind::Spectacle => matches!(alt, AltCastKind::ExternalPermission(_)),
            }
        }
        Condition::AnchorWordChosen { word } => {
            child_ctx
                .board
                .record_read(DirtyMask::DESIGNATIONS | DirtyMask::TOPOLOGY);
            child_ctx
                .source
                .and_then(|id| state.rare(id))
                .and_then(|r| r.chosen_anchor_word.as_deref())
                .is_some_and(|w| w.eq_ignore_ascii_case(word))
        }
        Condition::IsDay => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state.day_night() == Some(false)
        }
        Condition::IsNight => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state.day_night() == Some(true)
        }
        Condition::HasInitiative => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            state.initiative() == Some(child_ctx.controller)
        }
        Condition::IsMonarch { player, relation } => {
            child_ctx.board.record_read(DirtyMask::PLAYER_STATE);
            let Some(monarch) = state.monarch() else {
                return false;
            };
            if let Some(pr) = player {
                resolve_player_ref(state, pr, &child_ctx) == Some(monarch)
            } else if let Some(rel) = relation {
                matches_relative_control(monarch, *rel, &child_ctx)
            } else {
                monarch == child_ctx.controller
            }
        }
        Condition::CreatureDiedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            state.turn_history().creatures_died_this_turn > 0
        }
        Condition::CreatureDiedUnderYourControlThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .creatures_died_under_control_this_turn
                .contains(&child_ctx.controller)
        }
        Condition::PermanentLeftBattlefieldThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            state.turn_history().permanents_left_bf_this_turn > 0
        }
        Condition::CardLeftYourGraveyardThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .cards_left_graveyard_this_turn
                .contains(&child_ctx.controller)
        }
        Condition::GainedLifeThisTurn { min } => {
            let extra = min
                .as_deref()
                .map_or(0, |m| evaluate_amount(state, m, &child_ctx));
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            let gained = TurnEventHistory::get_u32(
                &state.turn_history().life_gained_this_turn,
                child_ctx.controller,
            );
            if min.is_some() {
                (gained as i32) >= extra.max(1)
            } else {
                gained > 0
            }
        }
        Condition::OpponentGainedLifeThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .life_gained_this_turn
                .iter()
                .any(|&(pid, amt)| pid != child_ctx.controller && amt > 0)
        }
        Condition::YourTeamGainedLifeThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            TurnEventHistory::get_u32(
                &state.turn_history().life_gained_this_turn,
                child_ctx.controller,
            ) > 0
        }
        Condition::OpponentLostLifeThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .life_lost_this_turn
                .iter()
                .any(|&(pid, amt)| pid != child_ctx.controller && amt > 0)
        }
        Condition::YouLostLifeThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            TurnEventHistory::get_u32(
                &state.turn_history().life_lost_this_turn,
                child_ctx.controller,
            ) > 0
        }
        Condition::PlayerLostLifeThisTurn { player } => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            resolve_player_ref(state, player, &child_ctx).is_some_and(|pid| {
                TurnEventHistory::get_u32(&state.turn_history().life_lost_this_turn, pid) > 0
            })
        }
        Condition::PlayerLostLifeAtLeastThisTurn { min, opponent_only } => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .life_lost_this_turn
                .iter()
                .any(|&(pid, amt)| (!*opponent_only || pid != child_ctx.controller) && (amt as i32) >= *min)
        }
        Condition::PlayerLostLifeLastTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::LIFE | DirtyMask::TURN_HISTORY);
            !state.turn_history().player_lost_life_last_turn.is_empty()
        }
        Condition::SpellsCastThisTurnAtLeast { min, filter: _ } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            let total: u16 = state
                .turn_history()
                .spells_cast_this_turn
                .iter()
                .map(|&(_, c)| c)
                .sum();
            total >= *min
        }
        Condition::SpellsCastLastTurn { mode, count } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            let th = state.turn_history();
            match mode {
                SpellsLastTurnMode::None => {
                    th.first_turn_of_game_done
                        && th.spells_cast_last_turn.iter().all(|&(_, c)| c == 0)
                }
                SpellsLastTurnMode::SinglePlayerAtLeast => {
                    th.spells_cast_last_turn.iter().any(|&(_, c)| c >= *count)
                }
            }
        }
        Condition::CardsDrawnThisTurnAtLeast { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            TurnEventHistory::get_u16(
                &state.turn_history().cards_drawn_this_turn,
                child_ctx.controller,
            ) >= *min
        }
        Condition::OpponentDrewCardsAtLeastThisTurn { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            state
                .turn_history()
                .cards_drawn_this_turn
                .iter()
                .any(|&(pid, c)| pid != child_ctx.controller && c >= *min)
        }
        Condition::OpponentCastSpellsAtLeast { n, filter: _ } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            let opp_total: u16 = state
                .turn_history()
                .spells_cast_this_turn
                .iter()
                .filter(|&&(pid, _)| pid != child_ctx.controller)
                .map(|&(_, c)| c)
                .sum();
            opp_total >= *n
        }
        Condition::FirstCombatPhaseOfTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            state.combat_phases_this_turn() <= 1
                && state.turn_history().combat_phases_this_turn <= 1
        }
        Condition::CommittedCrimeThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            state
                .turn_history()
                .committed_crime_this_turn
                .contains(&child_ctx.controller)
        }
        Condition::NoSpellCastFromHandThisTurn | Condition::NoSpellCastThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            state
                .turn_history()
                .spells_cast_this_turn
                .iter()
                .all(|&(_, c)| c == 0)
        }
        Condition::YouDiscardedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            TurnEventHistory::get_u16(
                &state.turn_history().cards_discarded_this_turn,
                child_ctx.controller,
            ) > 0
        }
        Condition::DiscardedCardThisTurn { who } => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            state
                .turn_history()
                .cards_discarded_this_turn
                .iter()
                .any(|&(pid, c)| c > 0 && matches_relative_control(pid, *who, &child_ctx))
        }
        Condition::CreatedTokenThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            TurnEventHistory::get_u16(
                &state.turn_history().tokens_created_this_turn,
                child_ctx.controller,
            ) > 0
        }
        Condition::DescendedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY);
            state
                .player(child_ctx.controller)
                .is_some_and(|p| p.descended_this_turn > 0)
        }
        Condition::DealtDamageByCreaturesAtLeastThisTurn { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let total: u32 = state
                .turn_history()
                .damage_dealt_to_players_this_turn
                .iter()
                .filter(|&&(src, _, _, _)| {
                    src.is_some_and(|sid| {
                        state
                            .object(sid)
                            .is_some_and(|o| o.zone == Zone::Battlefield)
                            || state.lki(sid).is_some()
                    })
                })
                .map(|&(_, _, amt, _)| amt)
                .sum();
            (total as i32) >= *min
        }
        Condition::AttackedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .creatures_attacked_this_turn
                .iter()
                .any(|&(pid, _)| pid == child_ctx.controller)
        }
        Condition::AttackedWithCommanderThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            state
                .turn_history()
                .creatures_attacked_this_turn
                .iter()
                .any(|&(pid, oid)| {
                    pid == child_ctx.controller
                        && state.object(oid).is_some_and(GameObject::is_commander)
                })
        }
        Condition::CreaturesAttackedCountThisTurn { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let mut seen: SmallVec<[ObjectId; 8]> = SmallVec::new();
            for &(pid, oid) in &state.turn_history().creatures_attacked_this_turn {
                if pid == child_ctx.controller && !seen.contains(&oid) {
                    seen.push(oid);
                }
            }
            seen.len() >= usize::from(*min)
        }
        Condition::NoCreaturesAttackedThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            state.turn_history().creatures_attacked_this_turn.is_empty()
        }
        Condition::SelfDidntAttackThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            child_ctx.source.is_some_and(|src| {
                !state
                    .turn_history()
                    .creatures_attacked_this_turn
                    .iter()
                    .any(|&(_, oid)| oid == src)
            })
        }
        Condition::NotBeingDeclaredAsAttacker => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            child_ctx
                .source
                .is_some_and(|src| !state.combat().attackers.iter().any(|a| a.attacker == src))
        }
        Condition::SelfDealtDamageToOpponentThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            child_ctx.source.is_some_and(|src| {
                state
                    .turn_history()
                    .damage_dealt_to_players_this_turn
                    .iter()
                    .any(|&(s, victim, amt, _)| {
                        s == Some(src) && amt > 0 && victim != child_ctx.controller
                    })
            })
        }
        Condition::DamageDealtToSelfThisTurnAtLeast { min } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let total: u32 = state
                .turn_history()
                .damage_dealt_to_players_this_turn
                .iter()
                .filter(|&&(_, victim, _, _)| victim == child_ctx.controller)
                .map(|&(_, _, amt, _)| amt)
                .sum();
            (total as i32) >= *min
        }
        Condition::HasntDealtDamageYet { of } => {
            child_ctx
                .board
                .record_read(DirtyMask::COMBAT | DirtyMask::TURN_HISTORY);
            let target_id = match of {
                AttachmentOwnerTarget::SelfObj => child_ctx.source,
                AttachmentOwnerTarget::AttachedTo => child_ctx
                    .source
                    .and_then(|s| state.object(s))
                    .and_then(|o| o.attached_to),
            };
            target_id.is_some_and(|id| {
                state
                    .object(id)
                    .is_some_and(|o| !o.flags.contains(ObjectFlags::DEALT_DAMAGE_THIS_TURN))
            })
        }
        Condition::YourTurnsTakenAtLeast {
            min,
            only_during_your_turn,
        } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            let pcount = (state.players().len() as u32).max(1);
            let approx_turns = state.turn_number().div_ceil(pcount);
            let ok_turn = !*only_during_your_turn || state.active_player() == child_ctx.controller;
            ok_turn && approx_turns >= u32::from(*min)
        }
        Condition::CastCreatureAndNoncreatureThisTurn => {
            child_ctx
                .board
                .record_read(DirtyMask::TURN_HISTORY | DirtyMask::PLAYER_STATE);
            state.player(child_ctx.controller).is_some_and(|p| {
                p.noncreature_spells_cast_this_turn > 0
                    && p.spells_cast_this_turn > p.noncreature_spells_cast_this_turn
            })
        }
        Condition::DiedThisWay { .. } => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.died_this_way > 0)
        }
        Condition::NotPutOntoBattlefieldByThisAbility => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.not_put_onto_bf_by_this_ability)
        }
        Condition::PlayerDidntDiscardNonlandThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.player_didnt_discard_nonland_this_way)
        }
        Condition::PlayerDiscardedHandThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.player_discarded_hand_this_way)
        }
        Condition::DistinctCardTypesAmongSacrificedAtLeast { min } => {
            child_ctx.step_way_stats.is_some_and(|s| {
                s.distinct_sacrificed_types_mask.count_ones() >= u32::from(*min)
            })
        }
        Condition::CreatureCardMilledThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.creature_card_milled_this_way)
        }
        Condition::LandCardMilledThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.land_card_milled_this_way)
        }
        Condition::ReturnedFilterToHandThisWay { .. } => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.returned_filter_to_hand_this_way)
        }
        Condition::DiscardedNonlandThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.discarded_nonland_this_way)
        }
        Condition::DiscardedCreatureThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.discarded_creature_this_way)
        }
        Condition::DiscardedNoncreatureThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.discarded_noncreature_this_way)
        }
        Condition::NotDrawnCardsThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.not_drawn_cards_this_way)
        }
        Condition::ExiledCountAtLeast { min } => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.exiled_count_this_way >= u32::from(*min))
        }
        Condition::DidRevealThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.did_reveal_this_way)
        }
        Condition::SacrificedNonlandThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.sacrificed_nonland_this_way)
        }
        Condition::DidntPutOntoBattlefieldThisWay => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.didnt_put_onto_bf_this_way)
        }
        Condition::DidLastOptionalStep => {
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.did_last_optional_step)
        }
        Condition::SourceExiledThisWay => {
            child_ctx.board.record_read(DirtyMask::EXILE);
            child_ctx
                .step_way_stats
                .is_some_and(|s| s.source_exiled_this_way)
        }

        // Remaining Phase 2 / specialized conditions
        Condition::TributePaid
        | Condition::IsPaired
        | Condition::HasEnduringStory
        | Condition::HasCompletedDungeon
        | Condition::HasCitysBlessing
        | Condition::HasMaxSpeed
        | Condition::CameUnderControlSinceLastUpkeep
        | Condition::NinjutsuWindow
        | Condition::TreasureManaSpentToCast
        | Condition::BoastAttackedThisTurn
        | Condition::ExhaustUnused { .. }
        | Condition::SourcesDealtDamageThisTurn { .. }
        | Condition::CardsPutIntoGraveyardFromAnywhereThisTurn { .. }
        | Condition::NonlandEnteredThisTurn { .. }
        | Condition::VoidThisTurn
        | Condition::NthMatchingSpellThisTurn { .. }
        | Condition::ChoseOtherRingBearer
        | Condition::AbilityActivatedCountThisTurn { .. }
        | Condition::OpponentHadPermEnteredThisTurn { .. }
        | Condition::OpponentCardsPutIntoGraveyardAtLeastThisTurn { .. }
        | Condition::OpponentSearchedLibraryThisTurn
        | Condition::YourCreatureSpellCounteredByOpponentThisTurn
        | Condition::YourNoncreatureDestroyedByOpponentThisTurn
        | Condition::EventAbilityIsNotMana
        | Condition::EventAbilityLifePaid
        | Condition::EventCrewCountEquals { .. }
        | Condition::EventDamageDealtBySubtype { .. }
        | Condition::EventDamagedTargetDealtExcessDamageThisTurn
        | Condition::PutCounterOnCreatureThisTurn { .. }
        | Condition::SacrificedFilterThisTurn { .. }
        | Condition::SelfHaventAddedManaThisTurn
        | Condition::ZoneChangeFromToThisTurn { .. }
        | Condition::CounterPutOnSelfThisTurn
        | Condition::PlayerHadAnotherLandEnterThisTurn
        | Condition::WasDealtCombatDamageBySelfNameThisGame { .. }
        | Condition::RingBearerAndTemptedAtLeast { .. }
        | Condition::WonClashThisWay
        | Condition::CastTriggerCreatureSpell
        | Condition::AllPlayersCantCastCheck
        | Condition::FirstSpellThisTurnWithManaFrom { .. } => {
            child_ctx.board.record_read(DirtyMask::TURN_HISTORY);
            false
        }
    }
}

// ---- Internal Helpers -------------------------------------------------------------------------

#[inline]
fn is_player_tied_for_most_life(state: &GameState, pid: PlayerId) -> bool {
    let Some(target) = state.players().get(usize::from(pid.0)) else {
        return false;
    };
    if target.lost {
        return false;
    }
    let Some(max_life) = state
        .players()
        .iter()
        .filter(|p| !p.lost)
        .map(|p| p.life)
        .max()
    else {
        return false;
    };
    target.life >= max_life
}

#[inline]
fn source_has_flag(state: &GameState, ctx: &EvalContext<'_>, flag: ObjectFlags) -> bool {
    ctx.board.record_read(DirtyMask::DESIGNATIONS);
    ctx.source
        .and_then(|id| state.object(id))
        .is_some_and(|o| o.flags.contains(flag))
}

#[inline]
fn player_counter_value(p: &PlayerState, counter: PlayerCounterKind) -> i32 {
    match counter {
        PlayerCounterKind::Poison => i32::from(p.poison),
        PlayerCounterKind::Experience => i32::from(p.experience),
        PlayerCounterKind::Energy => i32::from(p.energy),
        PlayerCounterKind::Rad => i32::from(p.rad),
        PlayerCounterKind::Ticket => 0,
    }
}

#[inline]
fn count_counters_slice(counters: &[(CounterId, u32)], target: CounterTarget) -> i32 {
    match target {
        CounterTarget::Any | CounterTarget::Each => counters.iter().map(|&(_, c)| c as i32).sum(),
        CounterTarget::Specific(cid) => counters
            .iter()
            .find(|(k, _)| *k == cid)
            .map_or(0, |&(_, c)| c as i32),
    }
}

#[inline]
fn paid_mana_colors_for_ctx(state: &GameState, ctx: &EvalContext<'_>) -> ColorMask {
    if let Some(s) = ctx.stack_item
        && !s.paid_mana_colors.is_empty()
    {
        return s.paid_mana_colors;
    }
    if let Some(src) = ctx.source {
        if let Some(se) = stack_entry_for_object(state, src)
            && !se.paid_mana_colors.is_empty()
        {
            return se.paid_mana_colors;
        }
        if let Some(r) = state.rare(src) {
            return r.paid_mana_colors;
        }
    }
    ColorMask::empty()
}

fn eval_stat_ref(
    state: &GameState,
    r: &Ref,
    stat: StatKind,
    prefer_lki: bool,
    ctx: &EvalContext<'_>,
) -> i32 {
    if !prefer_lki
        && let Some(id) = resolve_ref(state, r, ctx)
        && state.object(id).is_some()
    {
        return read_object_stat(state, id, stat, ctx).unwrap_or(0);
    }
    if let Some(snap) = resolve_ref_lki(state, r, prefer_lki, ctx) {
        return match stat {
            StatKind::Power => snap.pt.map_or(0, |p| p.power),
            StatKind::Toughness => snap.pt.map_or(0, |p| p.toughness),
            StatKind::ManaValue => snap.mana_value as i32,
        };
    }
    0
}

pub(crate) fn read_object_stat(
    state: &GameState,
    id: ObjectId,
    stat: StatKind,
    ctx: &EvalContext<'_>,
) -> Option<i32> {
    match stat {
        StatKind::Power => object_pt(state, id, ctx).map(|pt| pt.power),
        StatKind::Toughness => object_pt(state, id, ctx).map(|pt| pt.toughness),
        StatKind::ManaValue => Some(object_mana_value(state, id, ctx) as i32),
    }
}

pub(crate) fn object_colors_in_ctx(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> ColorMask {
    let Some(obj) = state.object(id) else {
        return ColorMask::empty();
    };
    if obj.is_face_down() && obj.zone != Zone::Battlefield {
        return ColorMask::empty();
    }
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        return ev.colors & ColorMask::ALL_COLORS;
    }
    off_battlefield_characteristics(state, id, obj, eff_zone, ctx).colors
}

pub(crate) fn object_types_subtypes(
    state: &GameState,
    id: ObjectId,
    ctx: &EvalContext<'_>,
) -> (CardTypeMask, SmallVec<[SubtypeId; 4]>, bool) {
    let Some(obj) = state.object(id) else {
        return (CardTypeMask::empty(), SmallVec::new(), false);
    };
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        return (ev.types, ev.subtypes.clone(), ev.has_all_creature_types);
    }
    let chars = off_battlefield_characteristics(state, id, obj, eff_zone, ctx);
    (chars.types, chars.subtypes, chars.has_all_creature_types)
}

fn supertypes_in_ctx(state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> SupertypeMask {
    let Some(obj) = state.object(id) else {
        return SupertypeMask::empty();
    };
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ev) = ctx.board.resolve_eval(state, id)
    {
        return ev.supertypes;
    }
    off_battlefield_characteristics(state, id, obj, eff_zone, ctx).supertypes
}

pub(crate) fn object_name_in_ctx(state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> String {
    let Some(obj) = state.object(id) else {
        return String::new();
    };
    if obj.is_face_down() {
        return String::new();
    }
    let eff_zone = ctx.zone_override.unwrap_or(obj.zone);
    if eff_zone == Zone::Battlefield
        && !matches!(ctx.board, BoardView::BaseOnly)
        && let Some(ov) = ctx.board.effective_name_override(state, id)
        && !ov.is_empty()
    {
        return ov.to_string();
    }
    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    if copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        return evaluate_token_spec_base(spec, copiable).0;
    }
    let stack_entry = if eff_zone == Zone::Stack {
        stack_entry_for_object(state, id)
    } else {
        None
    };
    let mut opts = face_opts_for_game_object(obj, rare, stack_entry);
    opts.zone = eff_zone;
    resolve_object_faces(state.db(), obj.card_def, &opts)
        .map(|r| r.name)
        .unwrap_or_default()
}

fn has_attachment_subtype(
    state: &GameState,
    host_id: ObjectId,
    subtype_name: &str,
    ctx: &EvalContext<'_>,
) -> bool {
    let Some(sid) = state.db().subtype_id(subtype_name) else {
        return false;
    };
    for p in state.players() {
        if p.lost {
            continue;
        }
        for &cand in &p.battlefield {
            let Some(obj) = state.object(cand) else {
                continue;
            };
            if obj.is_phased_out() || obj.attached_to != Some(host_id) {
                continue;
            }
            let (_, subtypes, _) = object_types_subtypes(state, cand, ctx);
            if subtypes.contains(&sid) {
                return true;
            }
        }
    }
    false
}

fn count_distinct_names_for_filter(
    state: &GameState,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> i32 {
    let mut names: Vec<String> = Vec::new();
    for id in find_objects(state, filter, ctx) {
        let nm = object_name_in_ctx(state, id, ctx);
        if nm.is_empty() {
            continue;
        }
        if !names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&nm))
        {
            names.push(nm);
        }
    }
    names.len() as i32
}

fn count_distinct_card_types_in_zone(
    state: &GameState,
    zone: Zone,
    whose: ZoneScopeWhose,
    ctx: &EvalContext<'_>,
) -> i32 {
    const CANONICAL: CardTypeMask = CardTypeMask::from_bits_truncate(
        CardTypeMask::LAND.bits()
            | CardTypeMask::CREATURE.bits()
            | CardTypeMask::ARTIFACT.bits()
            | CardTypeMask::ENCHANTMENT.bits()
            | CardTypeMask::PLANESWALKER.bits()
            | CardTypeMask::INSTANT.bits()
            | CardTypeMask::SORCERY.bits()
            | CardTypeMask::BATTLE.bits()
            | CardTypeMask::KINDRED.bits(),
    );
    ctx.board.record_read(zone_dirty_mask(zone));
    let mut seen = CardTypeMask::empty();
    if zone == Zone::Stack {
        for entry in state.stack_entries() {
            let matches_who = match whose {
                ZoneScopeWhose::You => entry.controller == ctx.controller,
                ZoneScopeWhose::Opponents => entry.controller != ctx.controller,
                ZoneScopeWhose::All => true,
            };
            if !matches_who {
                continue;
            }
            if let StackEntryKind::Spell { object } = entry.kind {
                if state.object(object).is_some_and(GameObject::is_face_down) {
                    seen |= CardTypeMask::CREATURE;
                    continue;
                }
                let stack_ctx = EvalContext {
                    zone_override: Some(Zone::Stack),
                    ..*ctx
                };
                seen |= object_types_subtypes(state, object, &stack_ctx).0 & CANONICAL;
            }
        }
        return seen.bits().count_ones() as i32;
    }

    for p in state.players() {
        if p.lost {
            continue;
        }
        let matches_who = match whose {
            ZoneScopeWhose::You => p.id == ctx.controller,
            ZoneScopeWhose::Opponents => p.id != ctx.controller,
            ZoneScopeWhose::All => true,
        };
        if !matches_who {
            continue;
        }
        for &id in player_zone_slice(p, zone) {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if zone == Zone::Battlefield && obj.is_phased_out() {
                continue;
            }
            if obj.is_face_down() && zone != Zone::Battlefield {
                continue;
            }
            let z_ctx = EvalContext {
                zone_override: Some(zone),
                ..*ctx
            };
            let mut types = object_types_subtypes(state, id, &z_ctx).0 & CANONICAL;
            if zone == Zone::Battlefield {
                let raw_types = object_types_subtypes(state, id, &z_ctx).0;
                if raw_types.contains(CardTypeMask::DUNGEON) {
                    types |= CardTypeMask::DUNGEON;
                }
            }
            seen |= types;
        }
    }
    seen.bits().count_ones() as i32
}

fn player_comparison_metric(
    state: &GameState,
    pid: PlayerId,
    metric: OpponentComparisonMetric,
    ctx: &EvalContext<'_>,
) -> i32 {
    let Some(p) = state.players().get(usize::from(pid.0)) else {
        return 0;
    };
    match metric {
        OpponentComparisonMetric::Life => p.life,
        OpponentComparisonMetric::Hand => p.hand.len() as i32,
        OpponentComparisonMetric::Lands => p
            .battlefield
            .iter()
            .filter(|&&id| {
                state.object(id).is_some_and(|o| !o.is_phased_out())
                    && object_types_subtypes(state, id, ctx)
                        .0
                        .contains(CardTypeMask::LAND)
            })
            .count() as i32,
        OpponentComparisonMetric::Creatures => p
            .battlefield
            .iter()
            .filter(|&&id| {
                state.object(id).is_some_and(|o| !o.is_phased_out())
                    && object_types_subtypes(state, id, ctx)
                        .0
                        .contains(CardTypeMask::CREATURE)
            })
            .count() as i32,
    }
}

fn land_can_produce_colorless(state: &GameState, id: ObjectId, ctx: &EvalContext<'_>) -> bool {
    let Some(obj) = state.object(id) else {
        return false;
    };
    if obj.is_face_down() {
        return false;
    }
    let opts = face_opts_for_game_object(obj, state.rare(id), None);
    let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts) else {
        return false;
    };
    let mut found = false;
    for_each_active_ability_on_faces(
        state.db(),
        obj.card_def,
        &opts,
        &resolved.active_face_indices,
        |c| evaluate_condition(state, c, ctx),
        |ab| {
            if !found
                && let AbilityDef::Activated(act) = ab.ability
                && act.is_mana
            {
                for step in &act.effect {
                    if let mtg_ir::EffectOp::AddMana { production, .. } = &step.op {
                        match production {
                            mtg_ir::ManaProduction::Fixed { symbols }
                                if symbols.contains(&ManaColor::C) =>
                            {
                                found = true;
                                return;
                            }
                            mtg_ir::ManaProduction::Bundles { options }
                                if options.iter().any(|b| b.contains(&ManaColor::C)) =>
                            {
                                found = true;
                                return;
                            }
                            _ => {}
                        }
                    }
                }
            }
        },
    );
    found
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;

    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::state::{CopiableState, CopyExceptions, StackChoices, StackEntry};
    use crate::zones::{MoveRawOptions, move_object_raw};
    use mtg_ir::{
        CardDef, CardLayout, Characteristics, CounterId, FaceDef, IR_V2_VERSION, InternTables,
        IrV2Bundle, KeywordAbility, KeywordDef, PrintedPt, Span, StaticAbility, StaticDef,
    };

    struct TestWorkView {
        reads: Cell<DirtyMask>,
    }

    impl LayerBoardView for TestWorkView {
        fn evaluated(&self, _id: ObjectId) -> Option<&EvaluatedObject> {
            None
        }
        fn effective_controller(&self, _id: ObjectId) -> Option<PlayerId> {
            None
        }
        fn effective_name(&self, _id: ObjectId) -> Option<&str> {
            None
        }
        fn record_read(&self, mask: DirtyMask) {
            self.reads.set(self.reads.get() | mask);
        }
    }

    fn make_eval_db() -> Arc<CardDatabase> {
        // Subtypes: 0=Plains, 1=Island, 2=Cleric, 3=Rogue, 4=Warrior, 5=Wizard, 6=Elf
        // Counters: 0=+1/+1, 1=-1/-1, 2=+2/+0
        // Keywords: 0=devoid, 1=changeling
        let mk_face = |name: &str, cost: &str, types: CardTypeMask, colors: ColorMask| FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse(cost),
                types,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors,
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
        };
        let mk_kw = |kw_id: u16| {
            AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(kw_id),
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
        };

        let cards = vec![
            // 0: */1+* CDA Creature (distinct card types in all graveyards)
            CardDef {
                oracle_id: "cda_goyf".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Goyf".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        mk_face("Synth Goyf", "{1}{G}", CardTypeMask::CREATURE, ColorMask::G);
                    f.characteristics.pt = Some(PrintedPt::Cda {
                        power: Some(0),
                        toughness: Some(1),
                    });
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7a,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetBasePt {
                                    power: Box::new(Amount::DistinctCardTypesInZone {
                                        zone: Zone::Graveyard,
                                        whose: ZoneScopeWhose::All,
                                    }),
                                    toughness: Box::new(Amount::Sum {
                                        args: vec![
                                            Amount::DistinctCardTypesInZone {
                                                zone: Zone::Graveyard,
                                                whose: ZoneScopeWhose::All,
                                            },
                                            Amount::Const { n: 1 },
                                        ],
                                    }),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 1: Artifact Land
            CardDef {
                oracle_id: "art_land".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Synth Seat".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Seat",
                        "",
                        CardTypeMask::ARTIFACT | CardTypeMask::LAND,
                        ColorMask::empty(),
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0), SubtypeId(1)]; // Plains, Island
                    f
                }],
                fully_supported: true,
            },
            // 2: X-spell with Devoid ({X}{R}{R})
            CardDef {
                oracle_id: "x_devoid".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "Synth Blaze".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Blaze",
                        "{X}{R}{R}",
                        CardTypeMask::SORCERY,
                        ColorMask::R,
                    );
                    f.abilities.push(mk_kw(0)); // devoid
                    f
                }],
                fully_supported: true,
            },
            // 3: OffBattlefieldCharacteristics grantor (all creatures in graveyards/hand gain Elf)
            CardDef {
                oracle_id: "off_bf_grant".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Synth Nexus".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Nexus",
                        "{3}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::OffBattlefieldCharacteristics {
                                zones: ZoneMask::HAND | ZoneMask::GRAVEYARD,
                                filter: Some(Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                })),
                                all_players: true,
                                add_all_creature_types: false,
                                add_chosen_subtype: false,
                                add_types: CardTypeMask::empty(),
                                add_subtypes: smallvec::smallvec![SubtypeId(6)], // Elf
                                add_chosen_color: false,
                                add_colors: ColorMask::G,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 4: Party creature with Cleric + Rogue, cost {W/U}{U}
            CardDef {
                oracle_id: "party_cr".into(),
                compiler_version: 46,
                text_hash: "4".into(),
                name: "Synth Duelist".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Duelist",
                        "{W/U}{U}",
                        CardTypeMask::CREATURE,
                        ColorMask::W | ColorMask::U,
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(2), SubtypeId(3)]; // Cleric, Rogue
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: -2,
                        toughness: 2,
                    });
                    f
                }],
                fully_supported: true,
            },
            // 5: Changeling Party creature + AllOffBattlefieldAndSpellsColorless
            CardDef {
                oracle_id: "changeling_colorless".into(),
                compiler_version: 46,
                text_hash: "5".into(),
                name: "Synth Blank".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face("Synth Blank", "{2}", CardTypeMask::CREATURE, ColorMask::U);
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 1,
                        toughness: 1,
                    });
                    f.abilities.push(mk_kw(1)); // changeling
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::RuleModifier {
                                rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                                who: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::EachPlayer,
                                },
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 6: Self-referential recursive CDA (tests MAX_EVAL_DEPTH termination)
            CardDef {
                oracle_id: "recursive_cda".into(),
                compiler_version: 46,
                text_hash: "6".into(),
                name: "Synth Loop".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Loop",
                        "{1}",
                        CardTypeMask::CREATURE,
                        ColorMask::empty(),
                    );
                    f.characteristics.pt = Some(PrintedPt::Cda {
                        power: None,
                        toughness: None,
                    });
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7a,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetBasePt {
                                    power: Box::new(Amount::Count {
                                        of: Box::new(ObjectFilter {
                                            zones: ZoneMask::GRAVEYARD,
                                            power: Some(mtg_ir::Compare {
                                                op: CompareOp::Ge,
                                                value: Box::new(Amount::Const { n: 0 }),
                                            }),
                                            ..Default::default()
                                        }),
                                    }),
                                    toughness: Box::new(Amount::Const { n: 2 }),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
        ];

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![
                        "Plains".into(),
                        "Island".into(),
                        "Cleric".into(),
                        "Rogue".into(),
                        "Warrior".into(),
                        "Wizard".into(),
                        "Elf".into(),
                    ],
                    counters: vec!["+1/+1".into(), "-1/-1".into(), "+2/+0".into()],
                    keywords: vec!["devoid".into(), "changeling".into()],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn off_battlefield_cdas_counters_copy_except_and_recursion_guard() {
        let db = make_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            3,
        )
        .unwrap();

        // Put Artifact Land + Sorcery into graveyards (3 distinct card types: Artifact, Land, Sorcery)
        let h_land = state.players()[0].hand[0];
        state.object_mut(h_land).unwrap().card_def = CardDefId(1);
        move_object_raw(
            &mut state,
            h_land,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        let h_sorc = state.players()[0].hand[0];
        state.object_mut(h_sorc).unwrap().card_def = CardDefId(2);
        move_object_raw(
            &mut state,
            h_sorc,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        // Synth Goyf in Hand with +1/+1, -1/-1, and +2/+0 counters -> 3/4 + 2/0 = 5/4
        let goyf_hand = state.players()[0].hand[0];
        state.add_counter(goyf_hand, CounterId(0), 1); // +1/+1
        state.add_counter(goyf_hand, CounterId(1), 1); // -1/-1
        state.add_counter(goyf_hand, CounterId(2), 1); // +2/+0

        let ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::BaseOnly);
        assert_eq!(
            object_pt(&state, goyf_hand, &ctx),
            Some(PowerToughness {
                power: 5,
                toughness: 4
            })
        );

        // CR 707.9d: CopiableState.except.pt overrides Layer 7a CDA
        state.rare_or_default(goyf_hand).unwrap().copiable = Some(Arc::new(CopiableState {
            card_def: CardDefId(0),
            face_index: None,
            fused: false,
            is_single_face_copy: false,
            prototyped: false,
            merged_components: SmallVec::new(),
            except: Some(CopyExceptions {
                pt: Some(PowerToughness {
                    power: 7,
                    toughness: 7,
                }),
                ..Default::default()
            }),
            modifications: None,
        }));
        assert_eq!(
            object_pt(&state, goyf_hand, &ctx),
            Some(PowerToughness {
                power: 9,
                toughness: 7
            })
        );

        // Stack {X} mana value (CR 202.3e) and Devoid in Hand (pick hand[1] so we don't reuse goyf_hand with copiable)
        let x_hand = state.players()[0].hand[1];
        state.object_mut(x_hand).unwrap().card_def = CardDefId(2);
        assert_eq!(object_mana_value(&state, x_hand, &ctx), 2);
        let x_chars = off_battlefield_characteristics(
            &state,
            x_hand,
            state.object(x_hand).unwrap(),
            Zone::Hand,
            &ctx,
        );
        assert_eq!(x_chars.colors, ColorMask::empty()); // Devoid!

        let x_stack = move_object_raw(
            &mut state,
            x_hand,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.stack_entries_mut()[0].x_value = 5;
        assert_eq!(object_mana_value(&state, x_stack, &ctx), 7);

        // Self-referential recursive CDA in graveyard terminates cleanly via MAX_EVAL_DEPTH
        let rec_h = state.players()[0].hand[1];
        state.object_mut(rec_h).unwrap().card_def = CardDefId(6);
        let rec_gy = move_object_raw(
            &mut state,
            rec_h,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let pt_rec = object_pt(&state, rec_gy, &ctx);
        assert!(pt_rec.is_some());
    }

    #[test]
    fn off_bf_grants_colorless_rule_signed_pt_devotion_domain_party_and_layer_work() {
        let db = make_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(4); 12],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            5,
        )
        .unwrap();

        // Off-battlefield creature (Synth Duelist, -2/2, W|U)
        let duelist_h = state.players()[0].hand[0];
        let base_ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::BaseOnly);
        assert_eq!(
            object_pt(&state, duelist_h, &base_ctx),
            Some(PowerToughness {
                power: -2,
                toughness: 2
            })
        );
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::Stat {
                    of: Ref::Binding { name: "x".into() },
                    stat: StatKind::Power,
                    lki: false,
                },
                &EvalContext {
                    bindings: &[ResolutionBinding {
                        name: "x".into(),
                        objects: vec![duelist_h],
                        lki: vec![],
                    }],
                    ..base_ctx
                }
            ),
            -2
        );

        // Put Synth Nexus (OffBattlefieldCharacteristics: +Elf, +G for creatures in Hand/GY) on BF
        let nexus_h = state.players()[0].hand[1];
        state.object_mut(nexus_h).unwrap().card_def = CardDefId(3);
        move_object_raw(
            &mut state,
            nexus_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        // Put 1 Synth Blank (Changeling + AllOffBattlefieldAndSpellsColorless) on BF
        let blank_h = state.players()[0].hand[1];
        state.object_mut(blank_h).unwrap().card_def = CardDefId(5);
        move_object_raw(
            &mut state,
            blank_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        // Put 1 Artifact Land (Plains + Island) and 2 Synth Duelists (Cleric+Rogue, {W/U}{U}) on BF
        let land_h = state.players()[0].hand[1];
        state.object_mut(land_h).unwrap().card_def = CardDefId(1);
        move_object_raw(
            &mut state,
            land_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();

        for _ in 0..2 {
            let dh = state.players()[0].hand[1];
            move_object_raw(
                &mut state,
                dh,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }

        // LayerWork test: verify zero OnceLock re-entrancy and accurate DirtyMask read tracking
        let work_view = TestWorkView {
            reads: Cell::new(DirtyMask::empty()),
        };
        let work_ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::LayerWork(&work_view));

        // Verify OffBattlefieldCharacteristics grants Elf subtype, while AllOffBattlefieldAndSpellsColorless clears G!
        let chars = off_battlefield_characteristics(
            &state,
            duelist_h,
            state.object(duelist_h).unwrap(),
            Zone::Hand,
            &work_ctx,
        );
        assert!(chars.has_subtype(&db, SubtypeId(6))); // Elf granted!
        assert_eq!(chars.colors, ColorMask::empty()); // Forced colorless!

        // Devotion to U: 2 Duelists * 2 ({W/U}{U} both contain U) = 4
        assert_eq!(
            compute_devotion(&state, PlayerId(0), ColorMask::U, &work_ctx),
            4
        );
        // Devotion to W: 2 Duelists * 1 ({W/U}) = 2
        assert_eq!(
            compute_devotion(&state, PlayerId(0), ColorMask::W, &work_ctx),
            2
        );
        // Domain: Plains + Island = 2
        assert_eq!(compute_domain(&state, PlayerId(0), &work_ctx), 2);
        // Party count: 2x (Cleric|Rogue) + 1x Changeling (fills Warrior) = 3!
        assert_eq!(compute_party_count(&state, PlayerId(0), &work_ctx), 3);

        // Life condition & DirtyMask read tracking
        assert!(evaluate_condition(
            &state,
            &Condition::AnyPlayerLife {
                op: CompareOp::Eq,
                count: 20,
            },
            &work_ctx
        ));
        assert!(work_view.reads.get().contains(DirtyMask::LIFE));
        assert!(work_view.reads.get().contains(DirtyMask::TOPOLOGY));
        // Prove zero OnceLock re-entrancy during LayerWork!
        assert!(!state.has_cached_layer_eval());

        // ColorsOfManaSpentToCast
        let cast_ctx = EvalContext {
            stack_item: Some(&StackEntry {
                id: crate::id::StackItemId(99),
                kind: StackEntryKind::Spell { object: duelist_h },
                controller: PlayerId(0),
                choices: StackChoices::default(),
                targets: vec![],
                x_value: 0,
                paid_mana_colors: ColorMask::W | ColorMask::U | ColorMask::G,
            }),
            ..base_ctx
        };
        assert_eq!(
            evaluate_amount(&state, &Amount::ColorsOfManaSpentToCast, &cast_ctx),
            3
        );
    }

    fn make_i2_eval_db() -> Arc<CardDatabase> {
        // Subtypes: 0=Elf, 1=Knight, 2=Aura, 3=Equipment, 4=Spacecraft
        // Counters: 0=+1/+1, 1=poison, 2=energy
        // Keywords: 0=devoid, 1=changeling, 2=flying
        let mk_face = |name: &str, cost: &str, types: CardTypeMask, colors: ColorMask| FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost: ManaCost::parse(cost),
                types,
                subtypes: SmallVec::new(),
                supertypes: SupertypeMask::empty(),
                colors,
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
        };
        let mk_kw = |kw_id: u16| {
            AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: KeywordId(kw_id),
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
        };

        let cards = vec![
            // 0: 2/3 green Elf creature {1}{G}
            CardDef {
                oracle_id: "i2_elf".into(),
                compiler_version: 46,
                text_hash: "0".into(),
                name: "Synth Squire".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Squire",
                        "{1}{G}",
                        CardTypeMask::CREATURE,
                        ColorMask::G,
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(0)];
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 3,
                    });
                    f
                }],
                fully_supported: true,
            },
            // 1: Artifact {2}
            CardDef {
                oracle_id: "i2_art".into(),
                compiler_version: 46,
                text_hash: "1".into(),
                name: "Synth Relic".into(),
                layout: CardLayout::Normal,
                faces: vec![mk_face(
                    "Synth Relic",
                    "{2}",
                    CardTypeMask::ARTIFACT,
                    ColorMask::empty(),
                )],
                fully_supported: true,
            },
            // 2: Equipment {1}
            CardDef {
                oracle_id: "i2_eq".into(),
                compiler_version: 46,
                text_hash: "2".into(),
                name: "Synth Blade".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Blade",
                        "{1}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(3)];
                    f
                }],
                fully_supported: true,
            },
            // 3: Aura {W}
            CardDef {
                oracle_id: "i2_aura".into(),
                compiler_version: 46,
                text_hash: "3".into(),
                name: "Synth Ward".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f =
                        mk_face("Synth Ward", "{W}", CardTypeMask::ENCHANTMENT, ColorMask::W);
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(2)];
                    f
                }],
                fully_supported: true,
            },
            // 4: Planeswalker loyalty=4
            CardDef {
                oracle_id: "i2_pw".into(),
                compiler_version: 46,
                text_hash: "4".into(),
                name: "Synth Walker".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Walker",
                        "{2}{U}",
                        CardTypeMask::PLANESWALKER,
                        ColorMask::U,
                    );
                    f.characteristics.loyalty = Some(4);
                    f
                }],
                fully_supported: true,
            },
            // 5: Emblem with AllOffBattlefieldAndSpellsColorless
            CardDef {
                oracle_id: "i2_emblem".into(),
                compiler_version: 46,
                text_hash: "5".into(),
                name: "Synth Void Emblem".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Void Emblem",
                        "",
                        CardTypeMask::empty(),
                        ColorMask::empty(),
                    );
                    f.abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::RuleModifier {
                                who: mtg_ir::PlayerFilter {
                                    relation: mtg_ir::PlayerRelation::Any,
                                },
                                rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                            },
                            condition: None,
                        },
                        span: Span::default(),
                    }));
                    f
                }],
                fully_supported: true,
            },
            // 6: Transform card (front 1/1 R, back 4/4 B Flying)
            CardDef {
                oracle_id: "i2_dfc".into(),
                compiler_version: 46,
                text_hash: "6".into(),
                name: "Synth Dawn // Synth Dusk".into(),
                layout: CardLayout::Transform,
                faces: vec![
                    {
                        let mut f =
                            mk_face("Synth Dawn", "{R}", CardTypeMask::CREATURE, ColorMask::R);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 1,
                            toughness: 1,
                        });
                        f
                    },
                    {
                        let mut f = mk_face("Synth Dusk", "", CardTypeMask::CREATURE, ColorMask::B);
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 4,
                            toughness: 4,
                        });
                        f.abilities.push(mk_kw(2));
                        f
                    },
                ],
                fully_supported: true,
            },
            // 7: Adventure card
            CardDef {
                oracle_id: "i2_adv".into(),
                compiler_version: 46,
                text_hash: "7".into(),
                name: "Synth Knight // Synth Quest".into(),
                layout: CardLayout::Adventure,
                faces: vec![
                    {
                        let mut f = mk_face(
                            "Synth Knight",
                            "{1}{W}",
                            CardTypeMask::CREATURE,
                            ColorMask::W,
                        );
                        f.characteristics.subtypes = smallvec::smallvec![SubtypeId(1)];
                        f.characteristics.pt = Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        });
                        f
                    },
                    mk_face("Synth Quest", "{U}", CardTypeMask::INSTANT, ColorMask::U),
                ],
                fully_supported: true,
            },
            // 8: Split card
            CardDef {
                oracle_id: "i2_split".into(),
                compiler_version: 46,
                text_hash: "8".into(),
                name: "Synth Spark // Synth Ash".into(),
                layout: CardLayout::Split,
                faces: vec![
                    mk_face("Synth Spark", "{R}", CardTypeMask::INSTANT, ColorMask::R),
                    mk_face("Synth Ash", "{2}{B}", CardTypeMask::SORCERY, ColorMask::B),
                ],
                fully_supported: true,
            },
            // 9: Non-creature Spacecraft (5/5 printed but not a creature yet)
            CardDef {
                oracle_id: "i2_ship".into(),
                compiler_version: 46,
                text_hash: "9".into(),
                name: "Synth Vessel".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Vessel",
                        "{3}",
                        CardTypeMask::ARTIFACT,
                        ColorMask::empty(),
                    );
                    f.characteristics.subtypes = smallvec::smallvec![SubtypeId(4)];
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 5,
                        toughness: 5,
                    });
                    f
                }],
                fully_supported: true,
            },
            // 10: Devoid + Changeling + Battle defense=6
            CardDef {
                oracle_id: "i2_devoid_changeling".into(),
                compiler_version: 46,
                text_hash: "10".into(),
                name: "Synth Chimera".into(),
                layout: CardLayout::Normal,
                faces: vec![{
                    let mut f = mk_face(
                        "Synth Chimera",
                        "{2}{U}{R}",
                        CardTypeMask::CREATURE,
                        ColorMask::U | ColorMask::R,
                    );
                    f.characteristics.defense = Some(6);
                    f.characteristics.pt = Some(PrintedPt::Fixed {
                        power: 3,
                        toughness: 3,
                    });
                    f.abilities.push(mk_kw(0));
                    f.abilities.push(mk_kw(1));
                    f
                }],
                fully_supported: true,
            },
        ];

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables {
                    subtypes: vec![
                        "Elf".into(),
                        "Knight".into(),
                        "Aura".into(),
                        "Equipment".into(),
                        "Spacecraft".into(),
                    ],
                    counters: vec!["+1/+1".into(), "poison".into(), "energy".into()],
                    keywords: vec!["devoid".into(), "changeling".into(), "flying".into()],
                },
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    #[test]
    fn fractional_life_loss_counters_among_players_and_not_starting_player() {
        let db = make_i2_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 3;
        let mut state =
            GameState::new(Arc::clone(&db), cfg, &[deck.clone(), deck.clone(), deck], 1).unwrap();

        state.player_mut(PlayerId(0)).unwrap().life = 19;
        state.player_mut(PlayerId(0)).unwrap().poison = 3;
        state.player_mut(PlayerId(1)).unwrap().energy = 2;
        state.player_mut(PlayerId(2)).unwrap().poison = 7;
        state.player_mut(PlayerId(2)).unwrap().lost = true;

        let h0 = state.players()[0].hand[0];
        let c_live = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_counter(c_live, CounterId(0), 4);

        let h1 = state.players()[0].hand[0];
        let c_phased = move_object_raw(
            &mut state,
            h1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_counter(c_phased, CounterId(0), 5);
        state
            .object_mut(c_phased)
            .unwrap()
            .flags
            .insert(ObjectFlags::PHASED_OUT);

        let work_view = TestWorkView {
            reads: Cell::new(DirtyMask::empty()),
        };
        let ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::LayerWork(&work_view));

        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::FractionalLifeLoss {
                    player: PlayerRef::You,
                    denominator: 2,
                    round: RoundMode::Up,
                },
                &ctx
            ),
            10
        );
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::FractionalLifeLoss {
                    player: PlayerRef::You,
                    denominator: 2,
                    round: RoundMode::Down,
                },
                &ctx
            ),
            9
        );
        assert!(work_view.reads.get().contains(DirtyMask::LIFE));

        state.player_mut(PlayerId(0)).unwrap().life = 0;
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::FractionalLifeLoss {
                    player: PlayerRef::You,
                    denominator: 2,
                    round: RoundMode::Up,
                },
                &ctx
            ),
            0
        );

        // CountersAmongPlayersAndPermanents: Any vs Specific
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::CountersAmongPlayersAndPermanents { counter: None },
                &ctx
            ),
            9
        );
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::CountersAmongPlayersAndPermanents {
                    counter: Some(CounterTarget::Specific(CounterId(1))), // poison
                },
                &ctx
            ),
            3
        );
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::CountersAmongPlayersAndPermanents {
                    counter: Some(CounterTarget::Specific(CounterId(0))), // +1/+1
                },
                &ctx
            ),
            4
        );

        // NotStartingPlayer
        assert!(!evaluate_condition(
            &state,
            &Condition::NotStartingPlayer,
            &EvalContext::new(PlayerId(0))
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::NotStartingPlayer,
            &EvalContext::new(PlayerId(1))
        ));
    }

    #[test]
    fn evolve_training_dethrone_and_combat_event_conditions() {
        let db = make_i2_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 14],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 3;
        let mut state =
            GameState::new(Arc::clone(&db), cfg, &[deck.clone(), deck.clone(), deck], 2).unwrap();
        state.player_mut(PlayerId(0)).unwrap().life = 20;
        state.player_mut(PlayerId(1)).unwrap().life = 25;
        state.player_mut(PlayerId(2)).unwrap().life = 25;

        // Attacker 1 (2/3) attacking P1
        let h0 = state.players()[0].hand[0];
        let a1 = move_object_raw(
            &mut state,
            h0,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        // Attacker 2 (4/4 back-face DFC, let's give 2x +1/+1 to 2/3 -> 4/5) attacking P2
        let h1 = state.players()[0].hand[0];
        let a2 = move_object_raw(
            &mut state,
            h1,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_counter(a2, CounterId(0), 2);
        // Planeswalker on P1's BF
        let hp = state.players()[1].hand[0];
        state.object_mut(hp).unwrap().card_def = CardDefId(4);
        let pw = move_object_raw(
            &mut state,
            hp,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        // Attacker 3 (2/3) attacking pw
        let h2 = state.players()[0].hand[0];
        let a3 = move_object_raw(
            &mut state,
            h2,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        // Equipment + Aura attached to a1
        let heq = state.players()[0].hand[0];
        state.object_mut(heq).unwrap().card_def = CardDefId(2);
        let eq = move_object_raw(
            &mut state,
            heq,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.object_mut(eq).unwrap().attached_to = Some(a1);
        let haura = state.players()[0].hand[0];
        state.object_mut(haura).unwrap().card_def = CardDefId(3);
        let aura = move_object_raw(
            &mut state,
            haura,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        state
            .combat_mut()
            .attackers
            .push(crate::state::AttackerEntry {
                attacker: a1,
                target: crate::action::AttackTarget::Player { id: PlayerId(1) },
            });
        state
            .combat_mut()
            .attackers
            .push(crate::state::AttackerEntry {
                attacker: a3,
                target: crate::action::AttackTarget::Planeswalker { id: pw },
            });

        let a1_ctx = EvalContext::for_source(PlayerId(0), a1);
        let a3_ctx = EvalContext::for_source(PlayerId(0), a3);

        // Before a2 attacks: P2 is unattacked -> EventAttackerHasAnotherUnattackedOpponent is true
        assert!(evaluate_condition(
            &state,
            &Condition::EventAttackerHasAnotherUnattackedOpponent,
            &a1_ctx
        ));
        // Now a2 joins attacking P2 -> TrainingGreaterAttacker for a1 becomes true; unattacked opponent becomes false
        state
            .combat_mut()
            .attackers
            .push(crate::state::AttackerEntry {
                attacker: a2,
                target: crate::action::AttackTarget::Player { id: PlayerId(2) },
            });
        assert!(evaluate_condition(
            &state,
            &Condition::TrainingGreaterAttacker,
            &a1_ctx
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::TrainingGreaterAttacker,
            &EvalContext::for_source(PlayerId(0), a2)
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::EventAttackerHasAnotherUnattackedOpponent,
            &a1_ctx
        ));

        // DethroneMostLife & AnyAttackedPlayerWithMostLife & EventAttackingPlayerWithMostLife
        assert!(evaluate_condition(
            &state,
            &Condition::DethroneMostLife,
            &a1_ctx
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::DethroneMostLife,
            &a3_ctx
        )); // attacking PW, not player
        assert!(evaluate_condition(
            &state,
            &Condition::AnyAttackedPlayerWithMostLife,
            &a1_ctx
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::EventAttackingPlayerWithMostLife,
            &EvalContext {
                event_object: Some(a1),
                ..a1_ctx
            }
        ));
        // P0 surges ahead in life -> neither P1 nor P2 has most life
        state.player_mut(PlayerId(0)).unwrap().life = 30;
        assert!(!evaluate_condition(
            &state,
            &Condition::DethroneMostLife,
            &a1_ctx
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::AnyAttackedPlayerWithMostLife,
            &a1_ctx
        ));

        // EventMinAttackersToYouOrPlaneswalkers (from P1's perspective: a1->P1 and a3->pw = 2 attackers)
        let p1_ctx = EvalContext::new(PlayerId(1));
        assert!(evaluate_condition(
            &state,
            &Condition::EventMinAttackersToYouOrPlaneswalkers { min: 2 },
            &p1_ctx
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::EventMinAttackersToYouOrPlaneswalkers { min: 3 },
            &p1_ctx
        ));

        // EvolveCompare: a1 is 2/3, a2 is 4/5 -> true; identical 2/3 (a3) -> false
        assert!(evaluate_condition(
            &state,
            &Condition::EvolveCompare,
            &EvalContext {
                event_object: Some(a2),
                ..a1_ctx
            }
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::EvolveCompare,
            &EvalContext {
                event_object: Some(a3),
                ..a1_ctx
            }
        ));
        // LKI fallback for EvolveCompare (0/4 beats 2/3 via toughness)
        let ev_lki = LkiSnapshot {
            object_id: ObjectId {
                index: 999,
                generation: 1,
            },
            card_def: CardDefId(0),
            last_zone: Zone::Battlefield,
            owner: PlayerId(0),
            controller: PlayerId(0),
            name: "Synth Tank".into(),
            pt: Some(PowerToughness {
                power: 0,
                toughness: 4,
            }),
            types: CardTypeMask::CREATURE,
            subtypes: smallvec::smallvec![SubtypeId(0)],
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
            attached_ids: SmallVec::from_slice(&[eq]),
            chosen_color: None,
            chosen_subtype: None,
            chosen_card_name: None,
            active_face_index: 0,
        };
        assert!(evaluate_condition(
            &state,
            &Condition::EvolveCompare,
            &EvalContext {
                event_lki: Some(&ev_lki),
                ..a1_ctx
            }
        ));

        // EventObjectMatchesOrSubtype (live + LKI)
        let elf_filter = ObjectFilter {
            subtypes_any: smallvec::smallvec![SubtypeId(0)],
            ..Default::default()
        };
        assert!(evaluate_condition(
            &state,
            &Condition::EventObjectMatchesOrSubtype {
                filter: Box::new(elf_filter.clone()),
            },
            &EvalContext {
                event_object: Some(a2),
                ..a1_ctx
            }
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::EventObjectMatchesOrSubtype {
                filter: Box::new(elf_filter),
            },
            &EvalContext {
                event_lki: Some(&ev_lki),
                ..a1_ctx
            }
        ));

        // SelfRemovedFromCombatOrAttached
        assert!(evaluate_condition(
            &state,
            &Condition::SelfRemovedFromCombatOrAttached,
            &EvalContext::for_source(PlayerId(0), eq)
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::SelfRemovedFromCombatOrAttached,
            &EvalContext::for_source(PlayerId(0), aura)
        ));

        // EventWasAttachedEnchantedOrEquipped: live a1 (attached eq) and LKI attached_ids
        assert!(evaluate_condition(
            &state,
            &Condition::EventWasAttachedEnchantedOrEquipped,
            &EvalContext {
                event_object: Some(a1),
                ..a1_ctx
            }
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::EventWasAttachedEnchantedOrEquipped,
            &EvalContext {
                event_lki: Some(&ev_lki),
                ..a1_ctx
            }
        ));
    }

    #[test]
    fn opponents_controlling_more_greatest_base_toughness_and_token_snapshot() {
        let db = make_i2_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(1); 12],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::standard_1v1();
        cfg.player_count = 3;
        let mut state =
            GameState::new(Arc::clone(&db), cfg, &[deck.clone(), deck.clone(), deck], 4).unwrap();

        // P0: 1 artifact, P1: 1 artifact, P2: 3 artifacts
        for _ in 0..1 {
            let h = state.players()[0].hand[0];
            move_object_raw(
                &mut state,
                h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        for _ in 0..1 {
            let h = state.players()[1].hand[0];
            move_object_raw(
                &mut state,
                h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }
        for _ in 0..3 {
            let h = state.players()[2].hand[0];
            move_object_raw(
                &mut state,
                h,
                Zone::Battlefield,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
        }

        let art_filter = ObjectFilter {
            types_any: CardTypeMask::ARTIFACT,
            ..Default::default()
        };
        let ctx0 = EvalContext::new(PlayerId(0));
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::OpponentsControllingMoreCount {
                    filter: Box::new(art_filter.clone()),
                },
                &ctx0
            ),
            1
        );
        assert!(evaluate_condition(
            &state,
            &Condition::OpponentControlsAtLeastNMore {
                filter: Box::new(art_filter.clone()),
                n: 2,
            },
            &ctx0
        ));
        assert!(!evaluate_condition(
            &state,
            &Condition::OpponentControlsAtLeastNMore {
                filter: Box::new(art_filter),
                n: 3,
            },
            &ctx0
        ));

        // BaseOnly GreatestBaseToughnessControlled ignores counters + reads TokenSpec
        let h_elf = state.players()[0].hand[0];
        state.object_mut(h_elf).unwrap().card_def = CardDefId(0);
        let elf = move_object_raw(
            &mut state,
            h_elf,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.add_counter(elf, CounterId(0), 5); // 2/3 printed -> 7/8 with counters, base toughness still 3

        let h_tok = state.players()[0].hand[0];
        let tok = move_object_raw(
            &mut state,
            h_tok,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state.object_mut(tok).unwrap().card_def = crate::state::TOKEN_CARD_DEF_ID;
        state
            .object_mut(tok)
            .unwrap()
            .flags
            .insert(ObjectFlags::IS_TOKEN);
        state.rare_or_default(tok).unwrap().token_spec = Some(Arc::new(mtg_ir::TokenSpec {
            name: "Synth Golem".into(),
            power: 4,
            toughness: 5,
            colors: ColorMask::empty(),
            types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
            supertypes: SupertypeMask::empty(),
            subtypes: SmallVec::new(),
            type_line: String::new(),
            keywords: SmallVec::new(),
            tapped: false,
            quoted_abilities: vec![],
            legendary: false,
        }));

        let base_ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::BaseOnly);
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::GreatestBaseToughnessControlled { filter: None },
                &base_ctx
            ),
            5
        );
        // snapshot_from_live_object uses object_name_in_ctx -> preserves TokenSpec name!
        let tok_snap = snapshot_from_live_object(&state, tok, &base_ctx).unwrap();
        assert_eq!(tok_snap.name, "Synth Golem");
    }

    #[test]
    fn all_off_bf_colorless_phased_out_and_branch1_parity() {
        let db = make_i2_eval_db();
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 30],
            sideboard: vec![],
        };
        let mut state = GameState::new(
            Arc::clone(&db),
            GameConfig::standard_1v1(),
            &[deck.clone(), deck],
            9,
        )
        .unwrap();

        // 1. Emblem with RuleMod::AllOffBattlefieldAndSpellsColorless
        let h_emb = state.players()[0].hand[0];
        state.object_mut(h_emb).unwrap().card_def = CardDefId(5);
        let emb = move_object_raw(
            &mut state,
            h_emb,
            Zone::Command,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state
            .object_mut(emb)
            .unwrap()
            .flags
            .insert(ObjectFlags::IS_EMBLEM);
        state.mark_dirty(DirtyMask::TOPOLOGY);

        let work_view = TestWorkView {
            reads: Cell::new(DirtyMask::empty()),
        };
        let work_ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::LayerWork(&work_view));
        assert!(has_all_off_battlefield_and_spells_colorless(
            &state, &work_ctx
        ));

        let h_elf = state.players()[0].hand[0];
        let elf_chars = off_battlefield_characteristics(
            &state,
            h_elf,
            state.object(h_elf).unwrap(),
            Zone::Hand,
            &work_ctx,
        );
        assert_eq!(elf_chars.colors, ColorMask::empty());

        // Move emblem away and use PlayerContinuousEffect instead
        move_object_raw(
            &mut state,
            emb,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(!has_all_off_battlefield_and_spells_colorless(
            &state, &work_ctx
        ));
        state.add_player_continuous_effect(crate::layers::predicates::PlayerContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            who: mtg_ir::PlayerFilter {
                relation: mtg_ir::PlayerRelation::Any,
            },
            rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
            duration: Duration::EndOfTurn,
            timestamp: 1,
            expires_turn: None,
            expires_on_player: None,
            chosen_card_name: None,
            chosen_card_type: None,
            linked_exile_names: SmallVec::new(),
        });
        assert!(has_all_off_battlefield_and_spells_colorless(
            &state, &work_ctx
        ));
        assert!(state.evaluate_board().all_off_battlefield_colorless);
        state.retain_player_continuous_effects(|_| false);
        assert!(!has_all_off_battlefield_and_spells_colorless(
            &state, &work_ctx
        ));

        // 2. Phased-out BF permanent has consistent object_pt and object_mana_value across all BoardViews
        let h_bf = state.players()[0].hand[0];
        let bf_id = move_object_raw(
            &mut state,
            h_bf,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        state
            .object_mut(bf_id)
            .unwrap()
            .flags
            .insert(ObjectFlags::PHASED_OUT);
        let cache = state.evaluate_board().clone();
        let expected_pt = object_pt(
            &state,
            bf_id,
            &EvalContext::new(PlayerId(0)).with_board(BoardView::Default),
        );
        let expected_mv = object_mana_value(
            &state,
            bf_id,
            &EvalContext::new(PlayerId(0)).with_board(BoardView::Default),
        );
        assert_eq!(
            expected_pt,
            Some(PowerToughness {
                power: 2,
                toughness: 3
            })
        );
        assert_eq!(expected_mv, 2);
        for board in [
            BoardView::Default,
            BoardView::Cached(&cache),
            BoardView::LayerWork(&work_view),
            BoardView::BaseOnly,
        ] {
            let ctx = EvalContext::new(PlayerId(0)).with_board(board);
            assert_eq!(object_pt(&state, bf_id, &ctx), expected_pt);
            assert_eq!(object_mana_value(&state, bf_id, &ctx), expected_mv);
        }

        // 3. Direct CardDef fast-path (Branch 1) vs single-pass slow-path (Branch 2) parity
        let grant_sd = StaticDef {
            body: StaticBody::OffBattlefieldCharacteristics {
                zones: ZoneMask::GRAVEYARD,
                filter: None,
                all_players: true,
                add_all_creature_types: false,
                add_chosen_subtype: false,
                add_types: CardTypeMask::CREATURE,
                add_subtypes: smallvec::smallvec![SubtypeId(1)],
                add_chosen_color: false,
                add_colors: ColorMask::G,
            },
            condition: None,
        };
        let grants = [OffBfGrantSource {
            source_id: emb,
            controller: PlayerId(0),
            static_def: &grant_sd,
        }];
        let ids = OffBfLookupIds {
            changeling_kw: db.keyword_id("changeling"),
            devoid_kw: db.keyword_id("devoid"),
            spacecraft_sid: db.subtype_id("Spacecraft"),
        };

        for card_id in [
            CardDefId(0),
            CardDefId(4),
            CardDefId(6),
            CardDefId(7),
            CardDefId(8),
            CardDefId(9),
            CardDefId(10),
        ] {
            let h_fast = state.players()[0].library[0];
            state.object_mut(h_fast).unwrap().card_def = card_id;
            let fast_id = move_object_raw(
                &mut state,
                h_fast,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();

            let h_slow = state.players()[0].library[0];
            state.object_mut(h_slow).unwrap().card_def = card_id;
            let slow_id = move_object_raw(
                &mut state,
                h_slow,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap()
            .new_id
            .unwrap();
            // Giving slow_id an empty RareObjectState routes it through Branch 2 instead of Branch 1.
            state.rare_or_default(slow_id);

            let obj_fast = state.object(fast_id).unwrap();
            let obj_slow = state.object(slow_id).unwrap();
            let ctx_eval = EvalContext::new(PlayerId(0)).with_board(BoardView::BaseOnly);

            for colorless_override in [false, true] {
                let (ev_fast, notes_fast) = evaluate_off_battlefield_object_with_grants(
                    &state,
                    fast_id,
                    obj_fast,
                    &grants,
                    colorless_override,
                    ids,
                    &ctx_eval,
                    true,
                );
                let (ev_slow, notes_slow) = evaluate_off_battlefield_object_with_grants(
                    &state,
                    slow_id,
                    obj_slow,
                    &grants,
                    colorless_override,
                    ids,
                    &ctx_eval,
                    true,
                );
                assert_eq!(
                    ev_fast, ev_slow,
                    "mismatch for {card_id:?} colorless={colorless_override}"
                );
                assert_eq!(
                    notes_fast, notes_slow,
                    "notes mismatch for {card_id:?} colorless={colorless_override}"
                );
            }
        }
    }

    #[test]
    fn synth_post_m5a_hooks_eval_filter_and_history() {
        use crate::state::{StackChoices, StackEntry, StackEntryKind, StepWayStats};
        use mtg_ir::{Amount, Condition, FilterFlags, ObjectFilter};

        let mut tables = InternTables::default();
        let plains = CardDef {
            oracle_id: "synth_eval_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Eval Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Eval Plains".into(),
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
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: true,
        };
        tables.keywords.push("flying".into());
        let db = Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables,
                cards: vec![plains],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 5).unwrap();

        let obj = state.player(PlayerId(0)).unwrap().hand[0];
        let mv = move_object_raw(
            &mut state,
            obj,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let stack_oid = mv.new_id.unwrap();
        state.rare_or_default(stack_oid).unwrap().bargained = true;
        state.rare_or_default(stack_oid).unwrap().kicked_count = 2;
        state
            .rare_or_default(stack_oid)
            .unwrap()
            .kicked_tags
            .push("gift".into());
        let entry = StackEntry {
            id: state.allocate_stack_id(),
            kind: StackEntryKind::Spell { object: stack_oid },
            controller: PlayerId(0),
            choices: StackChoices {
                bargained: true,
                ..Default::default()
            },
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        };

        let cache = state.evaluate_board();
        let stats = StepWayStats {
            exiled_count_this_way: 3,
            discarded_creature_this_way: true,
            ..Default::default()
        };
        let mut ctx = EvalContext::for_source(PlayerId(0), stack_oid)
            .with_board(BoardView::Cached(cache))
            .with_stack_item(&entry);
        ctx.step_way_stats = Some(&stats);

        let mut bargained_filter = ObjectFilter::default();
        bargained_filter.flags |= FilterFlags::BARGAINED_SPELL;
        assert!(matches_object_filter(
            &state,
            stack_oid,
            &bargained_filter,
            &ctx
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::SpellWasBargained,
            &ctx
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::Kicked {
                tag: Some("gift".into()),
                min_count: None,
            },
            &ctx,
        ));
        assert_eq!(
            evaluate_amount(
                &state,
                &Amount::KickedCount {
                    tag: Some("gift".into()),
                    multiplier: None,
                },
                &ctx,
            ),
            1
        );
        assert!(evaluate_condition(
            &state,
            &Condition::ExiledCountAtLeast { min: 3 },
            &ctx,
        ));
        assert!(evaluate_condition(
            &state,
            &Condition::DiscardedCreatureThisWay,
            &ctx,
        ));

        // Designations: IsDay / IsNight / IsMonarch / HasInitiative
        state.set_day_night(Some(false));
        state.set_monarch(Some(PlayerId(0)));
        state.set_initiative(Some(PlayerId(0)));
        let cache2 = state.evaluate_board();
        let ctx2 = EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(cache2));
        assert!(evaluate_condition(&state, &Condition::IsDay, &ctx2));
        assert!(!evaluate_condition(&state, &Condition::IsNight, &ctx2));
        assert!(evaluate_condition(
            &state,
            &Condition::IsMonarch {
                player: Some(mtg_ir::PlayerRef::You),
                relation: None,
            },
            &ctx2,
        ));
        assert!(evaluate_condition(&state, &Condition::HasInitiative, &ctx2));
    }

    #[test]
    fn synth_target_ref_stack_controller_of_and_owner_of() {
        use crate::state::{StackChoices, StackEntry, StackEntryKind};

        let tables = InternTables::default();
        let plains = CardDef {
            oracle_id: "synth_eval_stack_plains".into(),
            compiler_version: 46,
            text_hash: "0".into(),
            name: "Synth Eval Stack Plains".into(),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: "Synth Eval Stack Plains".into(),
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
                cards: vec![plains],
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
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 81).unwrap();

        let p1_obj = state.player(PlayerId(1)).unwrap().hand[0];
        let mv = move_object_raw(
            &mut state,
            p1_obj,
            Zone::Stack,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        let stack_oid = mv.new_id.unwrap();
        let sid = state.stack_entries().last().unwrap().id;
        let _ = StackEntry {
            id: sid,
            kind: StackEntryKind::Spell { object: stack_oid },
            controller: PlayerId(1),
            choices: StackChoices::default(),
            targets: Vec::new(),
            x_value: 0,
            paid_mana_colors: ColorMask::empty(),
        };

        let tgt = [TargetRef::Stack { id: sid }];
        let cache = state.evaluate_board();
        let mut ctx = EvalContext::new(PlayerId(0)).with_board(BoardView::Cached(cache));
        ctx.targets = &tgt;

        assert_eq!(
            resolve_ref(&state, &Ref::Target { index: 0 }, &ctx),
            Some(stack_oid)
        );
        assert_eq!(
            resolve_player_ref(
                &state,
                &PlayerRef::ControllerOf {
                    of: Box::new(Ref::Target { index: 0 }),
                },
                &ctx
            ),
            Some(PlayerId(1))
        );
        assert_eq!(
            resolve_player_ref(
                &state,
                &PlayerRef::OwnerOf {
                    of: Box::new(Ref::Target { index: 0 }),
                },
                &ctx
            ),
            Some(PlayerId(1))
        );
    }
}
