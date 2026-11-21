use std::sync::Arc;

use mtg_ir::{
    AbilityDef, CardDefId, CardTypeMask, ColorMask, Condition, EffectOp, EffectStep, EventDetail,
    EventFilter, EventKind, EventPattern, GamePhase, KeywordId, ManaChoiceKind, ManaColor,
    ManaProduction, ModalDef, MultiplierTarget, ObjectFilter, ReflexiveTriggerSpec,
    RelativeControl, StaticBody, TargetSpec, TriggerDef, TriggerScope, TriggerSubject, Zone,
    ZoneMask,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::action::{AttackTarget, ChoiceAnswer, ChoiceRequest, DecisionState, TargetRef};
use crate::eval::{
    BoardView, EvalContext, effective_controller_for_object, evaluate_amount, evaluate_condition,
    resolve_player_ref,
};
use crate::faces::{
    ActiveAbilityRef, active_abilities_for_object, active_abilities_where,
    active_abilities_with_card_for_object, face_opts_for_object,
};
use crate::filter::{
    matches_lki_filter, matches_object_filter, matches_player_filter, matches_relative_control,
};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::layers::predicates::{GrantedAbilityEntry, StaticGrantOrigin};
use crate::state::{
    DirtyMask, EngineError, GameState, LkiSnapshot, NONE_OBJECT_ID, ObjectFlags,
    ReplacementGrantOrigin, ResolutionBinding, StackChoices, StackEntry, StackEntryKind, arc_serde,
    opt_arc_serde,
};

/// Maximum recursion depth for inline triggered mana abilities (`CR 605.1b / 605.4a`).
pub const MAX_INLINE_MANA_DEPTH: u8 = 8;

/// Execution mode for a matched trigger (`CR 603.3` vs `CR 605.1b`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerMode {
    Stack,
    Inline,
}

/// Identifies the origin of a triggered ability (`Innate`, Layer 6 grant, keyword, delayed, reflexive, saga).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerSourceKind {
    Innate {
        component_index: u8,
        face_index: u8,
        ability_idx: usize,
    },
    Layer6Granted {
        grant_source: Option<ObjectId>,
        grant_card_def: CardDefId,
        grant_face_index: u8,
        grant_ability_idx: usize,
        donor_object: ObjectId,
        donor_component_index: u8,
        granter_origin: StaticGrantOrigin,
        granter_card_def: Option<CardDefId>,
        granter_face_index: u8,
        granter_component_index: u8,
        granter_ability_index: u32,
        source_effect_id: u64,
    },
    Layer6Inline {
        grant_origin: ReplacementGrantOrigin,
        grant_source: Option<ObjectId>,
        grant_card_def: Option<CardDefId>,
        source_face_index: u8,
        source_component_index: u8,
        source_ability_index: u32,
        source_effect_id: u64,
    },
    DynamicWard {
        grant_source: Option<ObjectId>,
        generic: u32,
    },
    KeywordGranted {
        keyword: KeywordId,
    },
    Delayed {
        delayed_id: u32,
    },
    Reflexive {
        reflexive_id: u32,
    },
    SagaChapter {
        face_index: u8,
        ability_idx: usize,
        chapter: u8,
    },
}

/// Provenance discriminator for [`OncePerTurnKey`] and [`BatchTriggerKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TriggerAbilityOrigin {
    Innate {
        component_index: u8,
    },
    Layer6CardAbility {
        card_def: CardDefId,
        grant_source: Option<ObjectId>,
        donor_object: ObjectId,
        donor_component_index: u8,
        granter_origin: StaticGrantOrigin,
        granter_card_def: Option<CardDefId>,
        granter_face_index: u8,
        granter_component_index: u8,
        granter_ability_index: u32,
        source_effect_id: u64,
    },
    Layer6Inline {
        grant_origin: ReplacementGrantOrigin,
        grant_source: Option<ObjectId>,
        grant_card_def: Option<CardDefId>,
        source_component_index: u8,
        source_effect_id: u64,
    },
    DynamicWard {
        grant_source: Option<ObjectId>,
        generic: u32,
    },
    Delayed {
        delayed_id: u32,
    },
    KeywordGranted {
        keyword: KeywordId,
    },
    Reflexive {
        reflexive_id: u32,
    },
}

#[inline]
const fn trigger_ability_origin(kind: &TriggerSourceKind) -> TriggerAbilityOrigin {
    match *kind {
        TriggerSourceKind::Innate {
            component_index, ..
        } => TriggerAbilityOrigin::Innate { component_index },
        TriggerSourceKind::SagaChapter { .. } => {
            TriggerAbilityOrigin::Innate { component_index: 0 }
        }
        TriggerSourceKind::Layer6Granted {
            grant_source,
            grant_card_def,
            donor_object,
            donor_component_index,
            granter_origin,
            granter_card_def,
            granter_face_index,
            granter_component_index,
            granter_ability_index,
            source_effect_id,
            ..
        } => TriggerAbilityOrigin::Layer6CardAbility {
            card_def: grant_card_def,
            grant_source,
            donor_object,
            donor_component_index,
            granter_origin,
            granter_card_def,
            granter_face_index,
            granter_component_index,
            granter_ability_index,
            source_effect_id,
        },
        TriggerSourceKind::Layer6Inline {
            grant_origin,
            grant_source,
            grant_card_def,
            source_component_index,
            source_effect_id,
            ..
        } => TriggerAbilityOrigin::Layer6Inline {
            grant_origin,
            grant_source,
            grant_card_def,
            source_component_index,
            source_effect_id,
        },
        TriggerSourceKind::DynamicWard {
            grant_source,
            generic,
        } => TriggerAbilityOrigin::DynamicWard {
            grant_source,
            generic,
        },
        TriggerSourceKind::KeywordGranted { keyword } => {
            TriggerAbilityOrigin::KeywordGranted { keyword }
        }
        TriggerSourceKind::Delayed { delayed_id } => TriggerAbilityOrigin::Delayed { delayed_id },
        TriggerSourceKind::Reflexive { reflexive_id } => {
            TriggerAbilityOrigin::Reflexive { reflexive_id }
        }
    }
}

/// Key for `only_once_per_turn` (`CR 603.2i`) and `do_this_once_per_turn` (`CR 603.2h`).
/// Because `ObjectId` embeds `generation`, blinking an object (`CR 400.7`) automatically
/// invalidates prior keys and treats the blinked permanent as a fresh object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OncePerTurnKey {
    pub object: ObjectId,
    pub zone: Zone,
    pub face_index: u8,
    pub ability_idx: usize,
    pub origin: TriggerAbilityOrigin,
    pub is_do_this_once: bool,
}

/// Key for `one_or_more` batch deduplication (`CR 603.2c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BatchTriggerKey {
    pub object: ObjectId,
    pub zone: Zone,
    pub face_index: u8,
    pub ability_idx: usize,
    pub origin: TriggerAbilityOrigin,
    pub qualifier: u32,
}

/// A matched trigger queued prior to APNAP stack placement (`CR 603.2` / `CR 603.3`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingTrigger {
    pub order: u64,
    pub mode: TriggerMode,
    pub source: ObjectId,
    #[serde(with = "arc_serde")]
    pub source_lki: Arc<LkiSnapshot>,
    pub controller: PlayerId,
    pub kind: TriggerSourceKind,
    pub trigger_def: TriggerDef,
    pub condition: Option<Condition>,
    pub targets: Vec<TargetSpec>,
    pub effect: Vec<EffectStep>,
    pub modal: Option<ModalDef>,
    pub event_object: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub event_lki: Option<Arc<LkiSnapshot>>,
    pub event_player: Option<PlayerId>,
    pub event_amount: Option<i32>,
    pub defending_player: Option<PlayerId>,
    pub last_tapped_mana_colors: SmallVec<[ManaColor; 6]>,
    pub is_look_back: bool,
    pub x_value: u32,
}

/// Expiration rule for delayed triggers (`CR 603.7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelayedExpiry {
    OneShot,
    ThisTurn,
    UntilYourNextTurn {
        creator: PlayerId,
        created_turn: u32,
    },
    NextCombat {
        creator: PlayerId,
    },
    NextUpkeep {
        target_player: PlayerId,
        min_turn: u32,
    },
    Permanent,
}

/// Registered delayed trigger (`CR 603.7`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayedTriggerEntry {
    pub id: u32,
    pub controller: PlayerId,
    pub source_card: CardDefId,
    pub source_object: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub source_lki: Option<Arc<LkiSnapshot>>,
    pub trigger: TriggerDef,
    pub condition: Option<Condition>,
    pub targets: Vec<TargetSpec>,
    pub effect: Vec<EffectStep>,
    pub expiry: DelayedExpiry,
    pub watched_objects: SmallVec<[ObjectId; 4]>,
    pub x_value: u32,
}

/// Reflexive trigger queued during step resolution (`CR 603.12`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingReflexiveTrigger {
    pub id: u32,
    pub controller: PlayerId,
    pub source: ObjectId,
    #[serde(with = "arc_serde")]
    pub source_lki: Arc<LkiSnapshot>,
    pub spec: ReflexiveTriggerSpec,
    pub event_object: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub event_lki: Option<Arc<LkiSnapshot>>,
    pub event_player: Option<PlayerId>,
    pub event_amount: Option<i32>,
    pub bindings: Vec<ResolutionBinding>,
    pub x_value: u32,
}

/// Trigger metadata stored on [`GameState`] keyed by [`StackItemId`] (keeps [`StackEntry`] untouched).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackTriggerMeta {
    pub source: ObjectId,
    pub face_index: u8,
    pub ability_idx: usize,
    pub kind: TriggerSourceKind,
    pub condition: Option<Condition>,
    pub targets: Vec<TargetSpec>,
    pub effect: Vec<EffectStep>,
    pub modal: Option<ModalDef>,
    pub event_object: Option<ObjectId>,
    #[serde(with = "opt_arc_serde")]
    pub event_lki: Option<Arc<LkiSnapshot>>,
    pub event_player: Option<PlayerId>,
    pub event_amount: Option<i32>,
    pub defending_player: Option<PlayerId>,
    pub last_tapped_mana_colors: SmallVec<[ManaColor; 6]>,
    pub is_look_back: bool,
    pub x_value: u32,
    pub bindings: Vec<ResolutionBinding>,
}

/// Active `ChoiceRequest::OrderObjects` state when a player orders simultaneous triggers (`CR 603.3b`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTriggerOrder {
    pub prompt_id: u32,
    pub controller: PlayerId,
    pub stack_item_ids: Vec<StackItemId>,
    pub object_ids: Vec<ObjectId>,
    pub chosen_top_first: Vec<ObjectId>,
    pub resume_decision: DecisionState,
    pub resume_priority_player: PlayerId,
}

/// Candidate triggered ability discovered on a battlefield/zone/lookback object.
#[derive(Clone)]
struct TriggerCandidate {
    source_id: ObjectId,
    source_zone: Zone,
    source_ctrl: PlayerId,
    source_lki: Arc<LkiSnapshot>,
    face_index: u8,
    ability_idx: usize,
    kind: TriggerSourceKind,
    trigger: TriggerDef,
    condition: Option<Condition>,
    targets: Vec<TargetSpec>,
    effect: Vec<EffectStep>,
    modal: Option<ModalDef>,
    text: Option<String>,
    is_look_back: bool,
    watched_objects: SmallVec<[ObjectId; 4]>,
    delayed_id: Option<u32>,
    delayed_expiry: Option<DelayedExpiry>,
    x_value: u32,
}

#[derive(Clone)]
struct MatchedSubEvent {
    event_object: Option<ObjectId>,
    event_new_id: Option<ObjectId>,
    event_lki: Option<Arc<LkiSnapshot>>,
    event_player: Option<PlayerId>,
    event_amount: Option<i32>,
    defending_player: Option<PlayerId>,
    attack_target: Option<AttackTarget>,
    is_unblocked_attack: bool,
    last_tapped_mana_colors: SmallVec<[ManaColor; 6]>,
    batch_qualifier: u32,
    from_zone: Option<Zone>,
    to_zone: Option<Zone>,
    ceased_to_exist: bool,
    cast_targets: Vec<TargetRef>,
    kicked: bool,
    is_combat_damage: bool,
    damage_target: Option<TargetRef>,
    counter_id: Option<mtg_ir::CounterId>,
    prev_counter_count: u32,
    new_counter_count: u32,
    by_spell: bool,
    by_ability: bool,
    stack_item: Option<StackItemId>,
    attachment_host: Option<ObjectId>,
    is_loyalty_ability: bool,
    is_mana_ability: bool,
    discard_caused_by_opp: bool,
    in_own_draw_step_ordinal: Option<u16>,
    die_roll_result: i32,
    won_coin_flip: bool,
    amass_subtype: Option<mtg_ir::SubtypeId>,
    expend_new_total: u32,
    class_level: u16,
    door_face_index: u8,
    day_night_prev: Option<bool>,
    day_night_now: bool,
    saga_chapters: SmallVec<[u8; 3]>,
    saga_is_final: bool,
    clash_won: bool,
    blocker_attacker: Option<ObjectId>,
    attack_count: usize,
    event_phase: Option<GamePhase>,
}

impl Default for MatchedSubEvent {
    fn default() -> Self {
        Self {
            event_object: None,
            event_new_id: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: None,
            attack_target: None,
            is_unblocked_attack: false,
            last_tapped_mana_colors: SmallVec::new(),
            batch_qualifier: 0,
            from_zone: None,
            to_zone: None,
            ceased_to_exist: false,
            cast_targets: Vec::new(),
            kicked: false,
            is_combat_damage: false,
            damage_target: None,
            counter_id: None,
            prev_counter_count: 0,
            new_counter_count: 0,
            by_spell: false,
            by_ability: false,
            stack_item: None,
            attachment_host: None,
            is_loyalty_ability: false,
            is_mana_ability: false,
            discard_caused_by_opp: false,
            in_own_draw_step_ordinal: None,
            die_roll_result: 0,
            won_coin_flip: false,
            amass_subtype: None,
            expend_new_total: 0,
            class_level: 0,
            door_face_index: 0,
            day_night_prev: None,
            day_night_now: false,
            saga_chapters: SmallVec::new(),
            saga_is_final: false,
            clash_won: false,
            blocker_attacker: None,
            attack_count: 0,
            event_phase: None,
        }
    }
}

/// Returns active (non-lost) players in APNAP order starting at `state.active_player()` (`CR 101.4 / 800.4a`).
pub fn apnap_player_order(state: &GameState) -> SmallVec<[PlayerId; 4]> {
    let players = state.players();
    let n = players.len();
    let mut out = SmallVec::with_capacity(n);
    if n == 0 {
        return out;
    }
    let start = usize::from(state.active_player().0) % n;
    for k in 0..n {
        let p = &players[(start + k) % n];
        if !p.lost {
            out.push(p.id);
        }
    }
    out
}

/// Returns `true` if `ability` qualifies as an inline stackless triggered mana ability (`CR 605.1b / 605.4a`).
pub fn is_triggered_mana_ability(
    trigger: &TriggerDef,
    targets: &[TargetSpec],
    modal: Option<&ModalDef>,
    steps: &[EffectStep],
) -> bool {
    matches!(
        trigger.event,
        EventKind::TappedForMana | EventKind::ManaAddedByLandAbility
    ) && targets.is_empty()
        && modal.is_none()
        && !steps.is_empty()
        && steps
            .iter()
            .all(|s| matches!(s.op, EffectOp::AddMana { .. }))
}

/// Registers a delayed trigger (`CR 603.7`) onto `GameState`.
pub fn register_delayed_trigger(state: &mut GameState, mut entry: DelayedTriggerEntry) -> u32 {
    let id = state.allocate_delayed_id();
    entry.id = id;
    state.delayed_triggers_mut().push(entry);
    id
}

/// Queues a reflexive trigger (`CR 603.12`) for flushing after step resolution.
pub fn queue_reflexive_trigger(state: &mut GameState, mut entry: PendingReflexiveTrigger) -> u32 {
    let id = state.allocate_delayed_id();
    entry.id = id;
    state.pending_reflexive_triggers_mut().push(entry);
    id
}

/// Records `do_this_once_per_turn` completion (`CR 603.2h`).
pub fn mark_do_this_once_taken(
    state: &mut GameState,
    object: ObjectId,
    zone: Zone,
    face_index: u8,
    ability_idx: usize,
) {
    mark_do_this_once_taken_with_origin(
        state,
        object,
        zone,
        face_index,
        ability_idx,
        TriggerAbilityOrigin::Innate { component_index: 0 },
    );
}

/// Records `do_this_once_per_turn` completion (`CR 603.2h`) with explicit [`TriggerAbilityOrigin`].
pub fn mark_do_this_once_taken_with_origin(
    state: &mut GameState,
    object: ObjectId,
    zone: Zone,
    face_index: u8,
    ability_idx: usize,
    origin: TriggerAbilityOrigin,
) {
    let key = OncePerTurnKey {
        object,
        zone,
        face_index,
        ability_idx,
        origin,
        is_do_this_once: true,
    };
    let list = state.do_this_once_taken_this_turn_mut();
    if !list.contains(&key) {
        list.push(key);
    }
}

/// Synchronous trigger matching entry point invoked by [`crate::events::emit_event`].
pub fn on_event(state: &mut GameState, event: &crate::events::AtomicEvent) {
    on_event_with_granted(state, event, &[]);
}

pub(crate) fn on_event_with_granted(
    state: &mut GameState,
    event: &crate::events::AtomicEvent,
    single_leave_granted: &[crate::events::GrantedTriggerLki],
) {
    if matches!(state.decision(), DecisionState::Mulligan) {
        return;
    }

    let candidates = collect_trigger_candidates(state, &event.kind, single_leave_granted);
    if candidates.is_empty() {
        return;
    }

    for cand in candidates {
        let alts = std::iter::once(&cand.trigger).chain(cand.trigger.or.iter());
        for alt in alts {
            let eff_from_zones = alt.from_zones.or(cand.trigger.from_zones);
            if !zone_allows_trigger(&cand, alt.event, eff_from_zones, &event.kind) {
                continue;
            }
            let eff_scope = alt.scope.or(cand.trigger.scope);
            if !scope_allows_trigger(state, cand.source_ctrl, eff_scope) {
                continue;
            }

            let sub_events = extract_matching_subevents(state, alt, &event.kind);
            if sub_events.is_empty() {
                continue;
            }

            for sub in sub_events {
                if !matches_event_filter_and_detail(state, &cand, alt, &sub, &event.kind) {
                    continue;
                }
                if is_ward_suppressed(state, &cand, &sub, &event.kind) {
                    continue;
                }
                if record_pending_trigger(state, &cand, alt, &sub, &event.kind) {
                    // One alternative matched for this sub-event; do not double-match `or` branches
                    // for the same sub-event.
                    break;
                }
            }
        }
    }
}

fn zone_allows_trigger(
    cand: &TriggerCandidate,
    alt_event: EventKind,
    from_zones: Option<ZoneMask>,
    ev_kind: &crate::events::AtomicEventKind,
) -> bool {
    if cand.delayed_id.is_some() {
        return true;
    }
    if cand.is_look_back {
        // Look-back sources are objects that just left the battlefield (`CR 603.10a`).
        return from_zones.is_none_or(|m| m.contains(ZoneMask::BATTLEFIELD));
    }
    match cand.source_zone {
        Zone::Battlefield => from_zones.is_none_or(|m| m.contains(ZoneMask::BATTLEFIELD)),
        Zone::Command => {
            if cand.source_lki.types.contains(CardTypeMask::EMBLEM)
                || from_zones.is_some_and(|m| m.contains(ZoneMask::COMMAND))
            {
                true
            } else {
                // Check IS_EMBLEM flag via caller? Caller only adds Command emblems when IS_EMBLEM.
                from_zones.is_some_and(|m| m.contains(ZoneMask::COMMAND))
            }
        }
        Zone::Stack => {
            from_zones.is_some_and(|m| m.contains(ZoneMask::STACK))
                || (alt_event == EventKind::Cast
                    && matches!(
                        ev_kind,
                        crate::events::AtomicEventKind::Cast { object, .. } if *object == cand.source_id
                    ))
        }
        Zone::Exile => {
            if from_zones.is_some_and(|m| m.contains(ZoneMask::EXILE)) {
                true
            } else if alt_event == EventKind::HauntedCreatureDies {
                matches!(
                    ev_kind,
                    crate::events::AtomicEventKind::HauntedCreatureDies { haunter, .. }
                        if *haunter == cand.source_id
                )
            } else {
                false
            }
        }
        Zone::Graveyard | Zone::Hand | Zone::Library => {
            from_zones.is_some_and(|m| m.contains(ZoneMask::from_zone(cand.source_zone)))
        }
    }
}

fn scope_allows_trigger(
    state: &GameState,
    controller: PlayerId,
    scope: Option<TriggerScope>,
) -> bool {
    match scope {
        None | Some(TriggerScope::EachTurn) => true,
        Some(TriggerScope::ControllerTurn) => state.active_player() == controller,
        Some(TriggerScope::OpponentTurn) => state.active_player() != controller,
    }
}

fn collect_trigger_candidates(
    state: &GameState,
    ev_kind: &crate::events::AtomicEventKind,
    single_leave_granted: &[crate::events::GrantedTriggerLki],
) -> Vec<TriggerCandidate> {
    let mut out = Vec::new();
    let cache = state.evaluate_board();

    // 1. Live objects across non-lost players in APNAP/seat order (CR 800.4a).
    for pid in apnap_player_order(state) {
        let Some(p) = state.players().get(usize::from(pid.0)) else {
            continue;
        };

        // 1a. Battlefield permanents (non-phased-out, !lost_all_abilities).
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let Some(eval_obj) = cache.get(id) else {
                continue;
            };
            if eval_obj.lost_all_abilities {
                continue;
            }
            let lki = Arc::new(crate::events::build_batch_lki_snapshot(state, id, eval_obj));
            let ctx = EvalContext::for_source(eval_obj.controller, id);
            for ab in active_abilities_with_card_for_object(state, id, &ctx) {
                push_ability_candidate(
                    &mut out,
                    id,
                    Zone::Battlefield,
                    eval_obj.controller,
                    Arc::clone(&lki),
                    ActiveAbilityRef {
                        face_index: ab.face_index,
                        ability_index: ab.ability_index,
                        ability: ab.ability,
                    },
                    ab.component_index,
                    false,
                );
            }
            if let Some(extras) = cache.extra(id) {
                push_granted_ability_candidates(
                    state,
                    &mut out,
                    id,
                    Zone::Battlefield,
                    eval_obj.controller,
                    Arc::clone(&lki),
                    &extras.granted_abilities,
                );
            }
        }

        // 1b. Command zone emblems (CR 114.4) and command-zone `from_zones` abilities.
        for &id in &p.command {
            let Some(obj) = state.object(id) else {
                continue;
            };
            let is_emblem = obj.flags.contains(ObjectFlags::IS_EMBLEM);
            let Some(lki) = crate::events::snapshot_for_live_object(state, id).map(Arc::new) else {
                continue;
            };
            let Some(opts) = face_opts_for_object(state, id) else {
                continue;
            };
            for ab in active_abilities_where(state.db(), obj.card_def, &opts, |_| false) {
                if let AbilityDef::Triggered(t) = ab.ability
                    && (is_emblem || trigger_or_alt_allows_zone(&t.trigger, Zone::Command))
                {
                    push_ability_candidate(
                        &mut out,
                        id,
                        Zone::Command,
                        obj.controller,
                        Arc::clone(&lki),
                        ab,
                        0,
                        false,
                    );
                }
            }
        }

        // 1c. Graveyard / Exile / Hand triggers with `from_zones` (and Haunt in Exile).
        for (zone, ids) in [
            (Zone::Graveyard, &p.graveyard),
            (Zone::Exile, &p.exile),
            (Zone::Hand, &p.hand),
        ] {
            for &id in ids {
                let Some(obj) = state.object(id) else {
                    continue;
                };
                if obj.is_face_down() {
                    continue;
                }
                let Some(opts) = face_opts_for_object(state, id) else {
                    continue;
                };
                let mut zone_lki: Option<Arc<LkiSnapshot>> = None;
                for ab in active_abilities_where(state.db(), obj.card_def, &opts, |_| false) {
                    if let AbilityDef::Triggered(t) = ab.ability {
                        let allows = trigger_or_alt_allows_zone(&t.trigger, zone)
                            || (zone == Zone::Exile
                                && t.trigger.event == EventKind::HauntedCreatureDies);
                        if allows {
                            let lki = zone_lki.get_or_insert_with(|| {
                                Arc::new(
                                    crate::events::snapshot_for_live_object(state, id)
                                        .expect("live object checked"),
                                )
                            });
                            push_ability_candidate(
                                &mut out,
                                id,
                                zone,
                                obj.controller,
                                Arc::clone(lki),
                                ab,
                                0,
                                false,
                            );
                        }
                    }
                }
            }
        }
    }

    // 2. Stack objects (`from_zones: STACK` or Cast self-triggers).
    for &id in state.stack() {
        let Some(obj) = state.object(id) else {
            continue;
        };
        let Some(opts) = face_opts_for_object(state, id) else {
            continue;
        };
        let mut stack_lki: Option<Arc<LkiSnapshot>> = None;
        for ab in active_abilities_where(state.db(), obj.card_def, &opts, |_| false) {
            if let AbilityDef::Triggered(t) = ab.ability {
                let allows = trigger_or_alt_allows_zone(&t.trigger, Zone::Stack)
                    || t.trigger.event == EventKind::Cast
                    || t.trigger.or.iter().any(|o| o.event == EventKind::Cast);
                if allows {
                    let lki = stack_lki.get_or_insert_with(|| {
                        Arc::new(
                            crate::events::snapshot_for_live_object(state, id)
                                .expect("stack object checked"),
                        )
                    });
                    push_ability_candidate(
                        &mut out,
                        id,
                        Zone::Stack,
                        obj.controller,
                        Arc::clone(lki),
                        ab,
                        0,
                        false,
                    );
                }
            }
        }
    }

    // 3. CR 603.10a Pre-Leave Lookback for ZoneChange leaving Battlefield.
    if let crate::events::AtomicEventKind::ZoneChange {
        object,
        from: Some(Zone::Battlefield),
        lki,
        ..
    } = ev_kind
    {
        let mut covered_ids: SmallVec<[ObjectId; 8]> = SmallVec::new();
        // 3a. Simultaneous batch lookback entries (for all objects captured before leaving).
        for scope in state.batch_lki_scopes().iter().rev() {
            for entry in scope {
                if covered_ids.contains(&entry.id) {
                    continue;
                }
                covered_ids.push(entry.id);
                // Skip if still on battlefield (already collected in step 1a!).
                if state
                    .object(entry.id)
                    .is_some_and(|o| o.zone == Zone::Battlefield)
                {
                    continue;
                }
                if entry.lki.lost_all_abilities || entry.lki.phased_out {
                    continue;
                }
                if state
                    .players()
                    .get(usize::from(entry.lki.controller.0))
                    .is_some_and(|p| p.lost)
                {
                    continue;
                }
                collect_lookback_abilities_from_lki(
                    state,
                    &entry.lki,
                    &entry.granted_triggers,
                    &mut out,
                );
            }
        }
        // 3b. Single-object leave lookback if not covered by batch scope.
        if !covered_ids.contains(object)
            && !state
                .object(*object)
                .is_some_and(|o| o.zone == Zone::Battlefield)
            && let Some(snap) = lki
            && !snap.lost_all_abilities
            && !snap.phased_out
            && state
                .players()
                .get(usize::from(snap.controller.0))
                .is_none_or(|p| !p.lost)
        {
            collect_lookback_abilities_from_lki(state, snap, single_leave_granted, &mut out);
        }
    }

    // 4. Delayed Triggers (`state.delayed_triggers()`, CR 603.7).
    for dt in state.delayed_triggers() {
        if state
            .players()
            .get(usize::from(dt.controller.0))
            .is_some_and(|p| p.lost)
        {
            continue;
        }
        let src_id = dt.source_object.unwrap_or(NONE_OBJECT_ID);
        let src_lki = dt.source_lki.clone().unwrap_or_else(|| {
            Arc::new(LkiSnapshot {
                object_id: src_id,
                card_def: dt.source_card,
                name: state
                    .db()
                    .get(dt.source_card)
                    .map_or_else(String::new, |c| c.name.clone()),
                owner: dt.controller,
                controller: dt.controller,
                last_zone: Zone::Command,
                pt: None,
                types: CardTypeMask::empty(),
                subtypes: SmallVec::new(),
                supertypes: mtg_ir::SupertypeMask::empty(),
                colors: ColorMask::empty(),
                mana_value: 0,
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
            })
        });
        out.push(TriggerCandidate {
            source_id: src_id,
            source_zone: Zone::Command,
            source_ctrl: dt.controller,
            source_lki: src_lki,
            face_index: 0,
            ability_idx: dt.id as usize,
            kind: TriggerSourceKind::Delayed { delayed_id: dt.id },
            trigger: dt.trigger.clone(),
            condition: dt.condition.clone(),
            targets: dt.targets.clone(),
            effect: dt.effect.clone(),
            modal: None,
            text: None,
            is_look_back: false,
            watched_objects: dt.watched_objects.clone(),
            delayed_id: Some(dt.id),
            delayed_expiry: Some(dt.expiry),
            x_value: dt.x_value,
        });
    }

    out
}

fn collect_lookback_abilities_from_lki(
    state: &GameState,
    lki: &Arc<LkiSnapshot>,
    granted: &[crate::events::GrantedTriggerLki],
    out: &mut Vec<TriggerCandidate>,
) {
    if lki.face_down {
        return;
    }
    if let Some(card) = state.db().get(lki.card_def)
        && let Some(face) = card.faces.get(usize::from(lki.active_face_index))
    {
        let ctx = EvalContext {
            lki_override: Some(lki.as_ref()),
            ..EvalContext::for_source(lki.controller, lki.object_id)
        };
        for (ai, ab) in face.abilities.iter().enumerate() {
            if let AbilityDef::Triggered(t) = ab {
                if let Some(gc) = &t.grant_condition
                    && !evaluate_condition(state, gc, &ctx)
                {
                    continue;
                }
                push_ability_candidate(
                    out,
                    lki.object_id,
                    Zone::Battlefield,
                    lki.controller,
                    Arc::clone(lki),
                    ActiveAbilityRef {
                        face_index: lki.active_face_index,
                        ability_index: ai,
                        ability: ab,
                    },
                    0,
                    true,
                );
            }
        }
    }
    for g in granted {
        match g {
            crate::events::GrantedTriggerLki::CardAbility {
                card_def,
                face_index,
                ability_index,
                grant_source,
                donor_object,
                donor_component_index,
                granter_origin,
                granter_card_def,
                granter_face_index,
                granter_component_index,
                granter_ability_index,
                source_effect_id,
                ..
            } => {
                if let Some(card) = state.db().get(*card_def)
                    && let Some(face) = card.faces.get(usize::from(*face_index))
                    && let Some(AbilityDef::Triggered(t)) = face.abilities.get(*ability_index)
                {
                    out.push(TriggerCandidate {
                        source_id: lki.object_id,
                        source_zone: Zone::Battlefield,
                        source_ctrl: lki.controller,
                        source_lki: Arc::clone(lki),
                        face_index: *face_index,
                        ability_idx: *ability_index,
                        kind: TriggerSourceKind::Layer6Granted {
                            grant_source: *grant_source,
                            grant_card_def: *card_def,
                            grant_face_index: *face_index,
                            grant_ability_idx: *ability_index,
                            donor_object: *donor_object,
                            donor_component_index: *donor_component_index,
                            granter_origin: *granter_origin,
                            granter_card_def: *granter_card_def,
                            granter_face_index: *granter_face_index,
                            granter_component_index: *granter_component_index,
                            granter_ability_index: *granter_ability_index,
                            source_effect_id: *source_effect_id,
                        },
                        trigger: t.trigger.clone(),
                        condition: t.condition.as_deref().cloned(),
                        targets: t.targets.clone(),
                        effect: t.effect.clone(),
                        modal: t.modal.clone(),
                        text: t.text.clone(),
                        is_look_back: true,
                        watched_objects: SmallVec::new(),
                        delayed_id: None,
                        delayed_expiry: None,
                        x_value: 0,
                    });
                }
            }
            crate::events::GrantedTriggerLki::Inline {
                ability,
                grant_source,
                grant_origin,
                grant_card_def,
                source_face_index,
                source_component_index,
                source_ability_index,
                source_effect_id,
                ..
            } => {
                out.push(TriggerCandidate {
                    source_id: lki.object_id,
                    source_zone: Zone::Battlefield,
                    source_ctrl: lki.controller,
                    source_lki: Arc::clone(lki),
                    face_index: *source_face_index,
                    ability_idx: *source_ability_index as usize,
                    kind: TriggerSourceKind::Layer6Inline {
                        grant_origin: *grant_origin,
                        grant_source: *grant_source,
                        grant_card_def: *grant_card_def,
                        source_face_index: *source_face_index,
                        source_component_index: *source_component_index,
                        source_ability_index: *source_ability_index,
                        source_effect_id: *source_effect_id,
                    },
                    trigger: ability.trigger.clone(),
                    condition: ability.condition.as_deref().cloned(),
                    targets: ability.targets.clone(),
                    effect: ability.effect.clone(),
                    modal: ability.modal.clone(),
                    text: ability.text.clone(),
                    is_look_back: true,
                    watched_objects: SmallVec::new(),
                    delayed_id: None,
                    delayed_expiry: None,
                    x_value: 0,
                });
            }
        }
    }
}

fn trigger_or_alt_allows_zone(trigger: &TriggerDef, zone: Zone) -> bool {
    let bit = ZoneMask::from_zone(zone);
    if trigger.from_zones.is_some_and(|m| m.contains(bit)) {
        return true;
    }
    trigger
        .or
        .iter()
        .any(|alt| alt.from_zones.is_some_and(|m| m.contains(bit)))
}

#[allow(clippy::too_many_arguments)]
fn push_ability_candidate(
    out: &mut Vec<TriggerCandidate>,
    source_id: ObjectId,
    source_zone: Zone,
    source_ctrl: PlayerId,
    source_lki: Arc<LkiSnapshot>,
    ab: ActiveAbilityRef<'_>,
    component_index: u8,
    is_look_back: bool,
) {
    let AbilityDef::Triggered(t) = ab.ability else {
        return;
    };
    if t.trigger.is_state_trigger || t.trigger.spell_delayed.is_some() {
        return;
    }
    let kind = if t.trigger.is_chapter_ability {
        TriggerSourceKind::SagaChapter {
            face_index: ab.face_index,
            ability_idx: ab.ability_index,
            chapter: *t.trigger.chapter_numbers.first().unwrap_or(&1),
        }
    } else {
        TriggerSourceKind::Innate {
            component_index,
            face_index: ab.face_index,
            ability_idx: ab.ability_index,
        }
    };
    out.push(TriggerCandidate {
        source_id,
        source_zone,
        source_ctrl,
        source_lki,
        face_index: ab.face_index,
        ability_idx: ab.ability_index,
        kind,
        trigger: t.trigger.clone(),
        condition: t.condition.as_deref().cloned(),
        targets: t.targets.clone(),
        effect: t.effect.clone(),
        modal: t.modal.clone(),
        text: t.text.clone(),
        is_look_back,
        watched_objects: SmallVec::new(),
        delayed_id: None,
        delayed_expiry: None,
        x_value: 0,
    });
}

fn push_granted_ability_candidates(
    state: &GameState,
    out: &mut Vec<TriggerCandidate>,
    source_id: ObjectId,
    source_zone: Zone,
    source_ctrl: PlayerId,
    source_lki: Arc<LkiSnapshot>,
    granted: &[GrantedAbilityEntry],
) {
    for g in granted {
        match g {
            GrantedAbilityEntry::CardAbility {
                card_def,
                face_index,
                ability_index,
                grant_source,
                donor_object,
                donor_component_index,
                granter_origin,
                granter_card_def,
                granter_face_index,
                granter_component_index,
                granter_ability_index,
                source_effect_id,
                ..
            } => {
                if let Some(card) = state.db().get(*card_def)
                    && let Some(face) = card.faces.get(usize::from(*face_index))
                    && let Some(AbilityDef::Triggered(t)) = face.abilities.get(*ability_index)
                    && !t.trigger.is_state_trigger
                    && t.trigger.spell_delayed.is_none()
                {
                    out.push(TriggerCandidate {
                        source_id,
                        source_zone,
                        source_ctrl,
                        source_lki: Arc::clone(&source_lki),
                        face_index: *face_index,
                        ability_idx: *ability_index,
                        kind: TriggerSourceKind::Layer6Granted {
                            grant_source: *grant_source,
                            grant_card_def: *card_def,
                            grant_face_index: *face_index,
                            grant_ability_idx: *ability_index,
                            donor_object: *donor_object,
                            donor_component_index: *donor_component_index,
                            granter_origin: *granter_origin,
                            granter_card_def: *granter_card_def,
                            granter_face_index: *granter_face_index,
                            granter_component_index: *granter_component_index,
                            granter_ability_index: *granter_ability_index,
                            source_effect_id: *source_effect_id,
                        },
                        trigger: t.trigger.clone(),
                        condition: t.condition.as_deref().cloned(),
                        targets: t.targets.clone(),
                        effect: t.effect.clone(),
                        modal: t.modal.clone(),
                        text: t.text.clone(),
                        is_look_back: false,
                        watched_objects: SmallVec::new(),
                        delayed_id: None,
                        delayed_expiry: None,
                        x_value: 0,
                    });
                }
            }
            GrantedAbilityEntry::Inline {
                ability,
                grant_source,
                grant_origin,
                grant_card_def,
                source_face_index,
                source_component_index,
                source_ability_index,
                source_effect_id,
                ..
            } => {
                if let AbilityDef::Triggered(t) = ability.as_ref()
                    && !t.trigger.is_state_trigger
                    && t.trigger.spell_delayed.is_none()
                {
                    out.push(TriggerCandidate {
                        source_id,
                        source_zone,
                        source_ctrl,
                        source_lki: Arc::clone(&source_lki),
                        face_index: *source_face_index,
                        ability_idx: *source_ability_index as usize,
                        kind: TriggerSourceKind::Layer6Inline {
                            grant_origin: *grant_origin,
                            grant_source: *grant_source,
                            grant_card_def: *grant_card_def,
                            source_face_index: *source_face_index,
                            source_component_index: *source_component_index,
                            source_ability_index: *source_ability_index,
                            source_effect_id: *source_effect_id,
                        },
                        trigger: t.trigger.clone(),
                        condition: t.condition.as_deref().cloned(),
                        targets: t.targets.clone(),
                        effect: t.effect.clone(),
                        modal: t.modal.clone(),
                        text: t.text.clone(),
                        is_look_back: false,
                        watched_objects: SmallVec::new(),
                        delayed_id: None,
                        delayed_expiry: None,
                        x_value: 0,
                    });
                }
            }
            GrantedAbilityEntry::DynamicWard {
                generic,
                grant_source,
            } => {
                // Synthesize Ward {N} trigger for Layer-6 DynamicWard grant.
                out.push(TriggerCandidate {
                    source_id,
                    source_zone,
                    source_ctrl,
                    source_lki: Arc::clone(&source_lki),
                    face_index: 0,
                    ability_idx: 0,
                    kind: TriggerSourceKind::DynamicWard {
                        grant_source: *grant_source,
                        generic: *generic,
                    },
                    trigger: TriggerDef {
                        event: EventKind::BecomesTarget,
                        filter: EventFilter {
                            object: Some(TriggerSubject::SelfObj),
                            detail: Some(EventDetail::BecomesTarget {
                                targeted_by: Some(mtg_ir::TargetedByKind::SpellOrAbility),
                                targeted_by_activated_ability: false,
                                first_time_targeted_by_you_this_turn: false,
                                targeting_source_filter: None,
                                target_player: Some(RelativeControl::Opponent),
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
                    targets: vec![],
                    effect: vec![EffectStep {
                        op: EffectOp::CounterSpell {
                            replacement_zone: None,
                            unless_pays_generic: Some(Box::new(mtg_ir::Amount::Const {
                                n: *generic as i32,
                            })),
                            on_countered_steal_filter: None,
                        },
                        subject: mtg_ir::Selector::EventObject,
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
                    text: Some(format!("Ward {{{generic}}}")),
                    is_look_back: false,
                    watched_objects: SmallVec::new(),
                    delayed_id: None,
                    delayed_expiry: None,
                    x_value: 0,
                });
            }
            GrantedAbilityEntry::IntrinsicBasicMana { .. } => {}
        }
    }
}

/// Exhaustively maps all 91 [`EventKind`] variants against [`AtomicEventKind`].
fn extract_matching_subevents(
    state: &GameState,
    trig: &TriggerDef,
    ev: &crate::events::AtomicEventKind,
) -> SmallVec<[MatchedSubEvent; 4]> {
    use crate::events::AtomicEventKind as AE;

    let mut out = SmallVec::new();
    match trig.event {
        EventKind::ZoneChange => {
            if let AE::ZoneChange {
                object,
                new_id,
                from,
                to,
                from_player,
                to_player,
                ceased_to_exist,
                lki,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_new_id: *new_id,
                    event_lki: lki.clone(),
                    event_player: (*to_player).or(*from_player),
                    from_zone: *from,
                    to_zone: Some(*to),
                    ceased_to_exist: *ceased_to_exist,
                    ..Default::default()
                });
            }
        }
        EventKind::Cast => match ev {
            AE::Cast {
                player,
                object,
                stack_item,
                is_copy,
                targets,
                kicked,
                from_zone,
                ..
            } => {
                let or_copy = matches!(
                    trig.filter.detail.as_ref(),
                    Some(EventDetail::Cast { or_copy: true, .. })
                );
                if !*is_copy || or_copy {
                    out.push(MatchedSubEvent {
                        event_object: Some(*object),
                        event_player: Some(*player),
                        stack_item: Some(*stack_item),
                        cast_targets: targets.clone(),
                        kicked: *kicked,
                        from_zone: Some(*from_zone),
                        ..Default::default()
                    });
                }
            }
            AE::SpellCopied {
                player,
                object,
                stack_item,
            } => {
                if matches!(
                    trig.filter.detail.as_ref(),
                    Some(EventDetail::Cast { or_copy: true, .. })
                ) {
                    out.push(MatchedSubEvent {
                        event_object: Some(*object),
                        event_player: Some(*player),
                        stack_item: Some(*stack_item),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        },
        EventKind::SpellCopied => {
            if let AE::SpellCopied {
                player,
                object,
                stack_item,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    stack_item: Some(*stack_item),
                    ..Default::default()
                });
            }
        }
        EventKind::SpellCountered => {
            if let AE::SpellCountered {
                stack_item,
                countered_lki,
                by_controller,
                by_spell_or_ability,
                ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(countered_lki.object_id),
                    event_lki: Some(Arc::clone(countered_lki)),
                    event_player: Some(*by_controller),
                    stack_item: Some(*stack_item),
                    by_spell: *by_spell_or_ability,
                    ..Default::default()
                });
            }
        }
        EventKind::Attacks => {
            if let AE::AttackDeclared { player, attackers } = ev {
                let per_player = matches!(
                    trig.filter.detail.as_ref(),
                    Some(EventDetail::Combat {
                        per_attacked_player: true,
                        ..
                    })
                );
                let mut seen_defenders: SmallVec<[PlayerId; 4]> = SmallVec::new();
                for att in attackers {
                    let def_pid = match att.target {
                        AttackTarget::Player { id } => Some(id),
                        AttackTarget::Planeswalker { id } | AttackTarget::Battle { id } => {
                            state.object(id).map(|o| o.controller)
                        }
                    };
                    if per_player {
                        if !matches!(att.target, AttackTarget::Player { .. }) {
                            continue;
                        }
                        if let Some(dp) = def_pid {
                            if seen_defenders.contains(&dp) {
                                continue;
                            }
                            seen_defenders.push(dp);
                        }
                    }
                    out.push(MatchedSubEvent {
                        event_object: Some(att.attacker),
                        event_player: Some(*player),
                        defending_player: def_pid,
                        attack_target: Some(att.target),
                        attack_count: attackers.len(),
                        batch_qualifier: def_pid.map_or(0, |d| u32::from(d.0) + 1),
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::Blocks => {
            if let AE::BlockersDeclared { defender, blocks } = ev {
                for b in blocks {
                    out.push(MatchedSubEvent {
                        event_object: Some(b.blocker),
                        event_player: Some(*defender),
                        blocker_attacker: Some(b.attacker),
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::BecomesBlocked => {
            if let AE::BlockersDeclared { defender, blocks } = ev {
                let mut seen: SmallVec<[ObjectId; 8]> = SmallVec::new();
                for b in blocks {
                    if !seen.contains(&b.attacker) {
                        seen.push(b.attacker);
                        out.push(MatchedSubEvent {
                            event_object: Some(b.attacker),
                            event_player: Some(*defender),
                            blocker_attacker: Some(b.blocker),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        EventKind::BecomesBlockedBy => {
            if let AE::BlockersDeclared { defender, blocks } = ev {
                for b in blocks {
                    out.push(MatchedSubEvent {
                        event_object: Some(b.attacker),
                        event_player: Some(*defender),
                        blocker_attacker: Some(b.blocker),
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::BlocksOrBecomesBlocked => {
            if let AE::BlockersDeclared { defender, blocks } = ev {
                for b in blocks {
                    out.push(MatchedSubEvent {
                        event_object: Some(b.blocker),
                        event_player: Some(*defender),
                        blocker_attacker: Some(b.attacker),
                        ..Default::default()
                    });
                    out.push(MatchedSubEvent {
                        event_object: Some(b.attacker),
                        event_player: Some(*defender),
                        blocker_attacker: Some(b.blocker),
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::AttackedAndUnblocked => {
            if let AE::UnblockedAttackers {
                defender,
                attackers,
            } = ev
            {
                for &att in attackers {
                    let atk_target = state
                        .combat()
                        .attackers
                        .iter()
                        .find(|a| a.attacker == att)
                        .map_or(AttackTarget::Player { id: *defender }, |a| a.target);
                    out.push(MatchedSubEvent {
                        event_object: Some(att),
                        event_player: Some(*defender),
                        defending_player: Some(*defender),
                        attack_target: Some(atk_target),
                        is_unblocked_attack: true,
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::AttackTriggered => {
            if let AE::AttackTriggered {
                source,
                controller,
                stack_item,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*source),
                    event_player: Some(*controller),
                    stack_item: Some(*stack_item),
                    ..Default::default()
                });
            }
        }
        EventKind::DealsDamage => {
            if let AE::DamageDealt {
                source,
                source_controller: _,
                target,
                amount,
                is_combat,
            } = ev
                && *amount > 0
            {
                let is_pw_recipient = matches!(
                    trig.filter.detail.as_ref(),
                    Some(EventDetail::Damage {
                        damage_to: Some(mtg_ir::DamageRecipient::PlayerOrPlaneswalker),
                        ..
                    })
                );
                let damaged_player = match *target {
                    TargetRef::Player { id } => Some(id),
                    TargetRef::Object { id } if is_pw_recipient => state
                        .evaluate_board()
                        .get(id)
                        .filter(|e| e.types.contains(CardTypeMask::PLANESWALKER))
                        .map(|e| e.controller),
                    _ => None,
                };
                out.push(MatchedSubEvent {
                    event_object: *source,
                    event_player: damaged_player,
                    event_amount: Some(*amount as i32),
                    is_combat_damage: *is_combat,
                    damage_target: Some(*target),
                    ..Default::default()
                });
            }
        }
        EventKind::DealtDamage => {
            if let AE::DamageDealt {
                source,
                source_controller,
                target,
                amount,
                is_combat,
            } = ev
                && *amount > 0
            {
                let (ev_obj, ev_player) = match *target {
                    TargetRef::Object { id } => (Some(id), Some(*source_controller)),
                    TargetRef::Player { id } => (*source, Some(id)),
                    TargetRef::Stack { .. } => (*source, Some(*source_controller)),
                };
                out.push(MatchedSubEvent {
                    event_object: ev_obj,
                    event_player: ev_player,
                    event_amount: Some(*amount as i32),
                    is_combat_damage: *is_combat,
                    damage_target: Some(*target),
                    ..Default::default()
                });
            }
        }
        EventKind::CombatDamageBatchPlayer => {
            if let AE::CombatDamageBatchPlayer {
                damaged_player,
                attackers,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: attackers.first().copied(),
                    event_player: Some(*damaged_player),
                    event_amount: Some(attackers.len() as i32),
                    is_combat_damage: true,
                    ..Default::default()
                });
            }
        }
        EventKind::PhaseBegin => {
            if let AE::PhaseStepEntered {
                phase,
                active_player,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_player: Some(*active_player),
                    event_phase: Some(*phase),
                    ..Default::default()
                });
            }
        }
        EventKind::CounterPlaced => {
            if let AE::CounterChanged {
                object,
                counter,
                previous_count,
                new_count,
                actor,
            } = ev
                && *new_count > *previous_count
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: *actor,
                    event_amount: Some((*new_count - *previous_count) as i32),
                    counter_id: Some(*counter),
                    prev_counter_count: *previous_count,
                    new_counter_count: *new_count,
                    ..Default::default()
                });
            }
        }
        EventKind::CounterRemoved => {
            if let AE::CounterChanged {
                object,
                counter,
                previous_count,
                new_count,
                actor,
            } = ev
                && *new_count < *previous_count
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: *actor,
                    event_amount: Some((*previous_count - *new_count) as i32),
                    counter_id: Some(*counter),
                    prev_counter_count: *previous_count,
                    new_counter_count: *new_count,
                    ..Default::default()
                });
            }
        }
        EventKind::BecomesTarget => {
            if let AE::BecomesTarget {
                target,
                by_spell,
                controller,
                stack_item,
            } = ev
            {
                let (ev_obj, ev_player) = match *target {
                    TargetRef::Object { id } => (Some(id), Some(*controller)),
                    TargetRef::Player { id } => (None, Some(id)),
                    TargetRef::Stack { .. } => (None, Some(*controller)),
                };
                out.push(MatchedSubEvent {
                    event_object: ev_obj,
                    event_player: ev_player,
                    by_spell: *by_spell,
                    by_ability: !*by_spell,
                    stack_item: *stack_item,
                    damage_target: Some(*target),
                    ..Default::default()
                });
            }
        }
        EventKind::Attached => {
            if let AE::AttachmentChanged {
                attachment,
                host,
                attached: true,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*attachment),
                    attachment_host: Some(*host),
                    ..Default::default()
                });
            }
        }
        EventKind::Unattached => {
            if let AE::AttachmentChanged {
                attachment,
                host,
                attached: false,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*attachment),
                    attachment_host: Some(*host),
                    ..Default::default()
                });
            }
        }
        EventKind::BecomesTapped => {
            if let AE::TapStateChanged {
                object,
                now_tapped: true,
                actor,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: *actor,
                    ..Default::default()
                });
            }
        }
        EventKind::BecomesUntapped => {
            if let AE::TapStateChanged {
                object,
                now_tapped: false,
                actor,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: *actor,
                    ..Default::default()
                });
            }
        }
        EventKind::DrawCard => {
            if let AE::DrawCard {
                player,
                object,
                in_own_draw_step_ordinal,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    in_own_draw_step_ordinal: *in_own_draw_step_ordinal,
                    ..Default::default()
                });
            }
        }
        EventKind::GainLife => {
            if let AE::LifeChange { player, delta } = ev
                && *delta > 0
            {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(*delta),
                    ..Default::default()
                });
            }
        }
        EventKind::LoseLife => {
            if let AE::LifeChange { player, delta } = ev
                && *delta < 0
            {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(-*delta),
                    ..Default::default()
                });
            }
        }
        EventKind::TappedForMana => {
            if let AE::TappedForMana {
                player,
                source,
                produced_colors,
                amount,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*source),
                    event_player: Some(*player),
                    event_amount: Some(*amount as i32),
                    last_tapped_mana_colors: produced_colors.clone(),
                    ..Default::default()
                });
            }
        }
        EventKind::ManaAddedByLandAbility => {
            if let AE::ManaAddedByLandAbility {
                player,
                source,
                produced_colors,
                amount,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*source),
                    event_player: Some(*player),
                    event_amount: Some(*amount as i32),
                    last_tapped_mana_colors: produced_colors.clone(),
                    ..Default::default()
                });
            }
        }
        EventKind::Cycle => {
            if let AE::Discard {
                player,
                object,
                dest_zone,
                is_cycle: true,
                cause_controller,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    to_zone: Some(*dest_zone),
                    discard_caused_by_opp: cause_controller.is_some_and(|c| c != *player),
                    ..Default::default()
                });
            }
        }
        EventKind::Discard => {
            if let AE::Discard {
                player,
                object,
                dest_zone,
                cause_controller,
                ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    to_zone: Some(*dest_zone),
                    discard_caused_by_opp: cause_controller.is_some_and(|c| c != *player),
                    ..Default::default()
                });
            }
        }
        EventKind::Transforms => {
            if let AE::Transformed { object, .. } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    ..Default::default()
                });
            }
        }
        EventKind::TurnFaceUp => {
            if let AE::TurnedFaceUp {
                object,
                player,
                x_value,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    event_amount: x_value.map(|x| x as i32),
                    ..Default::default()
                });
            }
        }
        EventKind::PhasedIn => {
            if let AE::PhasedIn { object, controller } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*controller),
                    ..Default::default()
                });
            }
        }
        EventKind::Investigate => {
            if let AE::Investigate { player, count } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*count)),
                    ..Default::default()
                });
            }
        }
        EventKind::Surveil => {
            if let AE::Surveil { player, count, .. } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*count)),
                    ..Default::default()
                });
            }
        }
        EventKind::Explore => {
            if let AE::Explore { player, object, .. } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Connive => {
            if let AE::Connive {
                player,
                object,
                nonland_discarded,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*nonland_discarded)),
                    ..Default::default()
                });
            }
        }
        EventKind::Scry => {
            if let AE::Scry { player, amount } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*amount)),
                    ..Default::default()
                });
            }
        }
        EventKind::BecomesCrewed => {
            if let AE::BecomesCrewed {
                player,
                vehicle,
                crewed_by,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*vehicle),
                    event_player: Some(*player),
                    event_amount: Some(crewed_by.len() as i32),
                    ..Default::default()
                });
            }
        }
        EventKind::CrewedVehicle => {
            if let AE::CrewedVehicle {
                player,
                creature,
                vehicle: _,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*creature),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::BecomesSaddled => {
            if let AE::BecomesSaddled {
                player,
                mount,
                saddled_by,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*mount),
                    event_player: Some(*player),
                    event_amount: Some(saddled_by.len() as i32),
                    ..Default::default()
                });
            }
        }
        EventKind::SaddledMount => {
            if let AE::SaddledMount {
                player,
                creature,
                mount: _,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*creature),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::BecameMonarch => {
            if let AE::BecameMonarch { player, .. } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Foretell => {
            if let AE::Foretell { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Plot => {
            if let AE::Plot { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Exert => {
            if let AE::Exerted { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Amass => {
            if let AE::Amass {
                player,
                object,
                subtype,
                amount,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: *object,
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*amount)),
                    amass_subtype: Some(*subtype),
                    ..Default::default()
                });
            }
        }
        EventKind::ManifestDread => {
            if let AE::ManifestDread {
                player, manifested, ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: *manifested,
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::BecameMonstrous => {
            if let AE::BecameMonstrous {
                player,
                object,
                amount,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*amount)),
                    ..Default::default()
                });
            }
        }
        EventKind::CommittedCrime => {
            if let AE::CommittedCrime { player, stack_item } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    stack_item: *stack_item,
                    ..Default::default()
                });
            }
        }
        EventKind::Exploited => {
            if let AE::Exploited {
                player,
                exploiter,
                exploited_lki,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*exploiter),
                    event_lki: Some(Arc::clone(exploited_lki)),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Expended => {
            if let AE::ManaExpended {
                player,
                previous_total,
                new_total,
                amount_spent,
            } = ev
            {
                let threshold = match trig.filter.detail.as_ref() {
                    Some(EventDetail::Misc {
                        expend_threshold: Some(th),
                        ..
                    }) => u32::from(*th),
                    _ => 0,
                };
                if threshold == 0 || (*previous_total < threshold && *new_total >= threshold) {
                    out.push(MatchedSubEvent {
                        event_player: Some(*player),
                        event_amount: Some(*amount_spent as i32),
                        expend_new_total: *new_total,
                        ..Default::default()
                    });
                }
            }
        }
        EventKind::Sacrificed => {
            if let AE::Sacrificed {
                player,
                object,
                lki,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_lki: Some(Arc::clone(lki)),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::GraveyardLeaveBatch => {
            if let AE::GraveyardLeaveBatch { owner, objects } = ev {
                out.push(MatchedSubEvent {
                    event_object: objects.first().copied(),
                    event_player: Some(*owner),
                    event_amount: Some(objects.len() as i32),
                    ..Default::default()
                });
            }
        }
        EventKind::Discover => {
            if let AE::Discover {
                player,
                n,
                hit_object,
                ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: *hit_object,
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*n)),
                    ..Default::default()
                });
            }
        }
        EventKind::Forage => {
            if let AE::Forage { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::CollectEvidence => {
            if let AE::CollectEvidence { player, total_mv } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(*total_mv as i32),
                    ..Default::default()
                });
            }
        }
        EventKind::ClassLevelGained => {
            if let AE::ClassLevelGained {
                object,
                controller,
                level,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*controller),
                    event_amount: Some(i32::from(*level)),
                    class_level: *level,
                    ..Default::default()
                });
            }
        }
        EventKind::CaseSolved => {
            if let AE::CaseSolved { object, controller } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*controller),
                    ..Default::default()
                });
            }
        }
        EventKind::BecameRenowned => {
            if let AE::BecameRenowned {
                object,
                controller,
                amount,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*controller),
                    event_amount: Some(i32::from(*amount)),
                    ..Default::default()
                });
            }
        }
        EventKind::DoorUnlocked => {
            if let AE::DoorUnlocked {
                player,
                object,
                face_index,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    door_face_index: *face_index,
                    ..Default::default()
                });
            }
        }
        EventKind::RoomFullyUnlocked => {
            if let AE::RoomFullyUnlocked { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::RingTempted => {
            if let AE::RingTempted {
                player,
                ring_level,
                ring_bearer,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: *ring_bearer,
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*ring_level)),
                    ..Default::default()
                });
            }
        }
        EventKind::DungeonRoomEntered => {
            if let AE::DungeonRoomEntered { player, room_index } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*room_index)),
                    ..Default::default()
                });
            }
        }
        EventKind::DungeonCompleted => {
            if let AE::DungeonCompleted { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::TookInitiative => {
            if let AE::TookInitiative { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Mutates => {
            if let AE::Mutates {
                player,
                object,
                mutate_count,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*mutate_count)),
                    ..Default::default()
                });
            }
        }
        EventKind::SoulbondPaired => {
            if let AE::SoulbondPaired {
                player,
                primary,
                partner,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*primary),
                    attachment_host: Some(*partner),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::HauntedCreatureDies => {
            if let AE::HauntedCreatureDies {
                haunter,
                haunted_lki,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*haunter),
                    event_lki: Some(Arc::clone(haunted_lki)),
                    event_player: Some(haunted_lki.controller),
                    ..Default::default()
                });
            }
        }
        EventKind::StateTriggered => {
            // Checked via `check_state_triggers(state)` rather than event emission.
        }
        EventKind::DayNightChanged => {
            if let AE::DayNightChanged {
                previous_night,
                now_night,
            } = ev
            {
                out.push(MatchedSubEvent {
                    day_night_prev: *previous_night,
                    day_night_now: *now_night,
                    ..Default::default()
                });
            }
        }
        EventKind::FacedDilemma => {
            if let AE::FacedDilemma { player, stack_item } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    stack_item: *stack_item,
                    ..Default::default()
                });
            }
        }
        EventKind::ExhaustActivated => {
            if let AE::ExhaustActivated { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::LandPlayed => {
            if let AE::LandPlayed {
                player,
                object,
                from_zone,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    from_zone: Some(*from_zone),
                    ..Default::default()
                });
            }
        }
        EventKind::LibrarySearched => {
            if let AE::LibrarySearched { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::LibraryShuffled => {
            if let AE::LibraryShuffled { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::PlayerLostGame => {
            if let AE::PlayerLostGame { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::CoinFlipped => {
            if let AE::CoinFlipped { player, won, .. } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    won_coin_flip: *won,
                    ..Default::default()
                });
            }
        }
        EventKind::DieRolled => {
            if let AE::DieRolled { player, result, .. } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(*result),
                    die_roll_result: *result,
                    ..Default::default()
                });
            }
        }
        EventKind::EnergyGained => {
            if let AE::EnergyGained { player, amount } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    event_amount: Some(i32::from(*amount)),
                    ..Default::default()
                });
            }
        }
        EventKind::Proliferated => {
            if let AE::Proliferated { player } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::Evolved => {
            if let AE::Evolved { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        EventKind::AbilityActivated => {
            if let AE::AbilityActivated {
                player,
                source,
                stack_item,
                is_mana,
                is_loyalty,
                ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*source),
                    event_player: Some(*player),
                    stack_item: *stack_item,
                    is_mana_ability: *is_mana,
                    is_loyalty_ability: *is_loyalty,
                    ..Default::default()
                });
            }
        }
        EventKind::PlayersFinishedVoting => {
            if let AE::PlayersFinishedVoting { controller } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*controller),
                    ..Default::default()
                });
            }
        }
        EventKind::Clash => {
            if let AE::Clash { player, won } = ev {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    clash_won: *won,
                    ..Default::default()
                });
            }
        }
        EventKind::SagaChapterResolved => {
            if let AE::SagaChapterResolved {
                object,
                controller,
                chapters,
                is_final,
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*controller),
                    saga_chapters: chapters.clone(),
                    saga_is_final: *is_final,
                    ..Default::default()
                });
            }
        }
        EventKind::GiftGiven => {
            if let AE::GiftGiven {
                player, recipient, ..
            } = ev
            {
                out.push(MatchedSubEvent {
                    event_player: Some(*player),
                    defending_player: Some(*recipient),
                    ..Default::default()
                });
            }
        }
        EventKind::TokenCreated => {
            if let AE::TokenCreated { player, object } = ev {
                out.push(MatchedSubEvent {
                    event_object: Some(*object),
                    event_player: Some(*player),
                    ..Default::default()
                });
            }
        }
        // Replacement-only EventKinds never match emitted events.
        EventKind::DrawReplacement
        | EventKind::DamageReplacement
        | EventKind::LifeGainReplacement
        | EventKind::ProliferateReplacement => {}
    }
    out
}

fn build_trigger_eval_ctx<'a>(
    cand: &'a TriggerCandidate,
    sub: &'a MatchedSubEvent,
) -> EvalContext<'a> {
    EvalContext {
        controller: cand.source_ctrl,
        source: Some(cand.source_id),
        event_object: sub.event_new_id.or(sub.event_object),
        event_lki: sub.event_lki.as_deref(),
        event_player: sub.event_player,
        event_amount: sub.event_amount,
        defending_player: sub.defending_player,
        lki_override: cand.is_look_back.then_some(cand.source_lki.as_ref()),
        board: BoardView::Default,
        ..EvalContext::new(cand.source_ctrl)
    }
}

fn matches_event_filter_and_detail(
    state: &GameState,
    cand: &TriggerCandidate,
    alt: &TriggerDef,
    sub: &MatchedSubEvent,
    ev_kind: &crate::events::AtomicEventKind,
) -> bool {
    let f = &alt.filter;
    let ctx = build_trigger_eval_ctx(cand, sub);

    // 1. Saga chapter check
    if (alt.is_chapter_ability || !alt.chapter_numbers.is_empty())
        && (sub.event_object != Some(cand.source_id)
            || !alt
                .chapter_numbers
                .iter()
                .any(|ch| sub.saga_chapters.contains(ch)))
    {
        return false;
    }

    // 2. `watched_bound_object` (for delayed triggers & Soulbond / Haunt)
    if f.watched_bound_object {
        let Some(ev_id) = sub.event_object else {
            return false;
        };
        if !cand.watched_objects.is_empty() {
            if !cand.watched_objects.contains(&ev_id)
                && !sub
                    .event_new_id
                    .is_some_and(|nid| cand.watched_objects.contains(&nid))
            {
                return false;
            }
        } else if let Some(src_obj) = state.object(cand.source_id) {
            let exiled_match = state
                .rare(cand.source_id)
                .and_then(|r| r.exiled_by)
                .is_some_and(|eid| eid == ev_id);
            let chosen_match = state
                .rare(cand.source_id)
                .and_then(|r| r.chosen_object)
                .is_some_and(|cid| cid == ev_id);
            let attached_match = src_obj.attached_to == Some(ev_id);
            if !exiled_match && !chosen_match && !attached_match {
                return false;
            }
        } else {
            return false;
        }
    }

    // 3. `during_turn`, `during_step`, `during_combat`, `during_main_phase`
    if let Some(dt) = f.during_turn {
        let ok = match dt {
            mtg_ir::DuringTurnKind::Your => state.active_player() == cand.source_ctrl,
            mtg_ir::DuringTurnKind::Opponent => state.active_player() != cand.source_ctrl,
            mtg_ir::DuringTurnKind::EventPlayer => sub
                .event_player
                .is_some_and(|ep| ep == state.active_player()),
        };
        if !ok {
            return false;
        }
    }
    let eff_phase = sub.event_phase.unwrap_or_else(|| state.phase());
    if let Some(req_step) = f.during_step
        && eff_phase != req_step
    {
        return false;
    }
    if f.during_combat
        && !matches!(
            eff_phase,
            GamePhase::BeginCombat
                | GamePhase::DeclareAttackers
                | GamePhase::DeclareBlockers
                | GamePhase::FirstStrikeDamage
                | GamePhase::CombatDamage
                | GamePhase::EndCombat
        )
    {
        return false;
    }
    if f.during_main_phase && !matches!(eff_phase, GamePhase::Main1 | GamePhase::Main2) {
        return false;
    }

    // 4. `player: Option<PlayerFilter>`
    if let Some(pf) = &f.player {
        let Some(ep) = sub.event_player else {
            return false;
        };
        if !matches_player_filter(state, ep, pf, &ctx) {
            return false;
        }
    }

    // 5. `first_time_each_turn`, `nth_this_turn`, `nth_event_this_turn`
    if f.first_time_each_turn || f.nth_this_turn.is_some() || f.nth_event_this_turn.is_some() {
        let ordinal = turn_ordinal_for_event(state, ev_kind, sub, cand.source_ctrl);
        if f.first_time_each_turn && ordinal != 1 {
            return false;
        }
        if let Some(n) = f.nth_this_turn
            && ordinal != n
        {
            return false;
        }
        if let Some(n) = f.nth_event_this_turn
            && ordinal != n
        {
            return false;
        }
    }

    // 6. `object: Option<TriggerSubject>` + `or_self` + `or_attached_to_self` + `or_commander`
    let mut matched_as_self = false;
    if let Some(subj) = &f.object {
        let mut matched = matches_trigger_subject(state, cand, sub, subj, f.detail.as_ref(), &ctx);
        if matched && matches!(subj, TriggerSubject::SelfObj) {
            matched_as_self = true;
        }
        if !matched && f.or_self {
            matched = matches_trigger_subject(
                state,
                cand,
                sub,
                &TriggerSubject::SelfObj,
                f.detail.as_ref(),
                &ctx,
            );
            if matched {
                matched_as_self = true;
            }
        }
        if !matched && f.or_attached_to_self {
            if matches_trigger_subject(
                state,
                cand,
                sub,
                &TriggerSubject::SelfObj,
                f.detail.as_ref(),
                &ctx,
            ) {
                matched = true;
                matched_as_self = true;
            } else {
                matched = matches_trigger_subject(
                    state,
                    cand,
                    sub,
                    &TriggerSubject::AttachedTo,
                    f.detail.as_ref(),
                    &ctx,
                );
            }
        }
        if !matched
            && f.or_commander
            && let Some(ev_id) = sub.event_new_id.or(sub.event_object)
        {
            let is_cmd = state.object(ev_id).is_some_and(|o| o.is_commander())
                || sub.event_lki.as_ref().is_some_and(|l| l.is_commander);
            if is_cmd {
                matched = true;
            }
        }
        if !matched {
            return false;
        }
    }

    // 7. `attached_filter` (skipped when `or_attached_to_self` matched `SelfObj`)
    if let Some(att_f) = &f.attached_filter
        && !(f.or_attached_to_self && matched_as_self)
    {
        let host_ok = if let Some(hid) = sub
            .attachment_host
            .or_else(|| state.object(cand.source_id).and_then(|o| o.attached_to))
        {
            if sub.from_zone == Some(Zone::Battlefield)
                && (sub.event_object == Some(hid) || sub.event_new_id == Some(hid))
                && let Some(snap) = sub
                    .event_lki
                    .as_deref()
                    .or_else(|| crate::events::pre_batch_lki(state, hid).map(|(l, _)| l.as_ref()))
            {
                matches_lki_filter(state, snap, att_f, &ctx)
            } else {
                matches_object_filter(state, hid, att_f, &ctx)
            }
        } else if let Some(snap) = sub
            .event_lki
            .as_deref()
            .filter(|l| l.attached_ids.contains(&cand.source_id))
        {
            matches_lki_filter(state, snap, att_f, &ctx)
        } else if let Some(ev_id) = sub.event_object
            && let Some((snap, _)) = crate::events::pre_batch_lki(state, ev_id)
            && snap.attached_ids.contains(&cand.source_id)
        {
            matches_lki_filter(state, snap, att_f, &ctx)
        } else {
            false
        };
        if !host_ok {
            return false;
        }
    }

    // 8. `EventDetail` (all 18 variants exhaustively handled)
    if let Some(detail) = &f.detail
        && !matches_event_detail(state, cand, sub, detail, &ctx)
    {
        return false;
    }

    // 9. `one_or_more` batch deduplication (`CR 603.2c`)
    if f.one_or_more && state.defer_flush() > 0 {
        let key = BatchTriggerKey {
            object: cand.source_id,
            zone: cand.source_zone,
            face_index: cand.face_index,
            ability_idx: cand.ability_idx,
            origin: trigger_ability_origin(&cand.kind),
            qualifier: sub.batch_qualifier,
        };
        if state.batch_triggered_keys().contains(&key) {
            return false;
        }
    }

    true
}

fn turn_ordinal_for_event(
    state: &GameState,
    ev_kind: &crate::events::AtomicEventKind,
    _sub: &MatchedSubEvent,
    _fallback_player: PlayerId,
) -> u16 {
    let (tag, player) = crate::events::event_count_tag(ev_kind);
    state.turn_history().event_count(tag, player)
}

fn matches_trigger_subject(
    state: &GameState,
    cand: &TriggerCandidate,
    sub: &MatchedSubEvent,
    subj: &TriggerSubject,
    detail: Option<&EventDetail>,
    ctx: &EvalContext<'_>,
) -> bool {
    match subj {
        TriggerSubject::SelfObj => {
            sub.event_object == Some(cand.source_id) || sub.event_new_id == Some(cand.source_id)
        }
        TriggerSubject::AttachedTo => {
            if let Some(host_id) = state.object(cand.source_id).and_then(|o| o.attached_to) {
                return sub.event_object == Some(host_id) || sub.event_new_id == Some(host_id);
            }
            let lki_has_self = |lki: &LkiSnapshot| lki.attached_ids.contains(&cand.source_id);
            if sub.event_lki.as_deref().is_some_and(lki_has_self) {
                return true;
            }
            if let Some(ev_id) = sub.event_object
                && crate::events::pre_batch_lki(state, ev_id)
                    .is_some_and(|(lki, _)| lki_has_self(lki))
            {
                return true;
            }
            false
        }
        TriggerSubject::Filter { filter } => {
            matches_subject_object_filter(state, sub, detail, filter, ctx)
        }
    }
}

fn matches_subject_object_filter(
    state: &GameState,
    sub: &MatchedSubEvent,
    detail: Option<&EventDetail>,
    filter: &ObjectFilter,
    ctx: &EvalContext<'_>,
) -> bool {
    let from_anywhere = matches!(
        detail,
        Some(EventDetail::ZoneChange {
            from_anywhere: true,
            ..
        })
    );

    // ZoneChange special cases (`CR 603.6a` vs `CR 603.10a` vs `CR 603.6c`):
    if let Some(to_z) = sub.to_zone {
        if to_z == Zone::Battlefield {
            // CR 603.6a: Entering the battlefield evaluates post-layer characteristics on BF.
            let target_id = sub.event_new_id.or(sub.event_object);
            let Some(tid) = target_id else {
                return false;
            };
            let mut z_filter = filter.clone();
            if z_filter.zones.is_empty() {
                z_filter.zones = ZoneMask::BATTLEFIELD;
            }
            return matches_object_filter(state, tid, &z_filter, ctx);
        }

        if sub.from_zone == Some(Zone::Battlefield) {
            if from_anywhere && to_z == Zone::Graveyard {
                // CR 603.6c: "put into a graveyard from anywhere" checks post-move object in GY.
                if let Some(nid) = sub.event_new_id
                    && !sub.ceased_to_exist
                {
                    let gy_ctx = EvalContext {
                        zone_override: Some(Zone::Graveyard),
                        ..*ctx
                    };
                    return matches_object_filter(state, nid, filter, &gy_ctx);
                }
            }
            // CR 603.10a: Leaving battlefield uses pre-leave / pre-batch LkiSnapshot.
            let lki = sub.event_lki.as_deref().or_else(|| {
                sub.event_object.and_then(|oid| {
                    crate::events::pre_batch_lki(state, oid).map(|(l, _)| l.as_ref())
                })
            });
            if let Some(snap) = lki {
                return matches_lki_filter(state, snap, filter, ctx);
            }
            return false;
        }
    }

    // Non-BF zone transitions or non-zone-change events:
    if let Some(nid) = sub.event_new_id.or(sub.event_object)
        && state.object(nid).is_some()
        && !sub.ceased_to_exist
        && matches_object_filter(state, nid, filter, ctx)
    {
        return true;
    }
    if let Some(snap) = sub.event_lki.as_deref() {
        return matches_lki_filter(state, snap, filter, ctx);
    }
    false
}

fn matches_event_detail(
    state: &GameState,
    cand: &TriggerCandidate,
    sub: &MatchedSubEvent,
    detail: &EventDetail,
    ctx: &EvalContext<'_>,
) -> bool {
    match detail {
        EventDetail::ZoneChange {
            from,
            to,
            from_anywhere,
            not_from_zone,
            not_to_zone,
            enters_untapped,
            dealt_damage_by_self_this_turn,
        } => {
            if !*from_anywhere
                && !from.is_empty()
                && !sub
                    .from_zone
                    .is_some_and(|fz| from.contains(ZoneMask::from_zone(fz)))
            {
                return false;
            }
            if let Some(req_to) = to
                && sub.to_zone != Some(*req_to)
            {
                return false;
            }
            if let Some(nf) = not_from_zone
                && sub.from_zone == Some(*nf)
            {
                return false;
            }
            if let Some(nt) = not_to_zone
                && sub.to_zone == Some(*nt)
            {
                return false;
            }
            if *enters_untapped {
                let target_id = sub.event_new_id.or(sub.event_object);
                let untapped = target_id
                    .and_then(|id| state.object(id))
                    .is_some_and(|o| !o.flags.contains(ObjectFlags::TAPPED));
                if !untapped {
                    return false;
                }
            }
            if *dealt_damage_by_self_this_turn {
                let victim_id = sub.event_object.or(sub.event_new_id);
                let ok = victim_id.is_some_and(|vid| {
                    state
                        .turn_history()
                        .was_damaged_by_this_turn(cand.source_id, vid)
                });
                if !ok {
                    return false;
                }
            }
            true
        }
        EventDetail::Cast {
            or_copy: _,
            targets_self,
            targets_only_self,
            during_opponent_turn,
            cast_from_not_hand,
            cast_from_zone,
            was_kicked,
            spell_ordinal,
            spell_has_keyword,
            enchanted_player_casts,
        } => {
            if *targets_self
                && !sub
                    .cast_targets
                    .iter()
                    .any(|t| matches!(t, TargetRef::Object { id } if *id == cand.source_id))
            {
                return false;
            }
            if *targets_only_self
                && (sub.cast_targets.len() != 1
                    || sub.cast_targets[0] != TargetRef::Object { id: cand.source_id })
            {
                return false;
            }
            if *during_opponent_turn && state.active_player() == cand.source_ctrl {
                return false;
            }
            if *cast_from_not_hand && sub.from_zone == Some(Zone::Hand) {
                return false;
            }
            if let Some(fz) = cast_from_zone
                && sub.from_zone != Some(*fz)
            {
                return false;
            }
            if *was_kicked && !sub.kicked {
                return false;
            }
            if let Some(ord) = spell_ordinal
                && let Some(caster) = sub.event_player
            {
                let count = state
                    .players()
                    .get(usize::from(caster.0))
                    .map_or(0, |p| p.spells_cast_this_turn);
                if count as i32 != i32::from(*ord) {
                    return false;
                }
            }
            if let Some(kw) = spell_has_keyword
                && let Some(obj_id) = sub.event_object
            {
                let has_kw = state
                    .evaluate_board()
                    .get(obj_id)
                    .is_some_and(|e| e.keywords.contains(kw))
                    || state.object(obj_id).is_some_and(|o| {
                        face_opts_for_object(state, obj_id)
                            .map(|opts| {
                                active_abilities_where(state.db(), o.card_def, &opts, |_| false)
                                    .iter()
                                    .any(|a| {
                                        matches!(
                                            a.ability,
                                            AbilityDef::Keyword(k) if k.keyword.keyword == *kw
                                        )
                                    })
                            })
                            .unwrap_or(false)
                    });
                if !has_kw {
                    return false;
                }
            }
            if *enchanted_player_casts {
                let ench_p = state.rare(cand.source_id).and_then(|r| r.chosen_player);
                if sub.event_player != ench_p {
                    return false;
                }
            }
            true
        }
        EventDetail::Combat {
            alone,
            saddled,
            min_attackers,
            dethrone_most_life,
            training_greater_attacker,
            blocked_attacker_filter,
            host,
            without_flanking,
            attacked_target,
            co_attacker_filter,
            co_attacker_min,
            per_attacked_player: _,
            attacking_with_filter,
            min_attacking_with_count,
            attacking_total_power_min,
            attack_enchanted_or_attacked_by_enchanted,
            attacking_unblocked,
        } => {
            if *alone && sub.attack_count != 1 {
                return false;
            }
            if let Some(min_a) = min_attackers
                && sub.attack_count < usize::from(*min_a)
            {
                return false;
            }
            if *attacking_unblocked && !sub.is_unblocked_attack {
                return false;
            }
            if *saddled
                && !sub
                    .event_object
                    .and_then(|id| state.object(id))
                    .is_some_and(|o| o.flags.contains(ObjectFlags::SADDLED))
            {
                return false;
            }
            if *dethrone_most_life && let Some(def_p) = sub.defending_player {
                let max_life = state
                    .players()
                    .iter()
                    .filter(|p| !p.lost)
                    .map(|p| p.life)
                    .max()
                    .unwrap_or(i32::MIN);
                let def_life = state
                    .players()
                    .get(usize::from(def_p.0))
                    .map_or(i32::MIN, |p| p.life);
                if def_life < max_life {
                    return false;
                }
            }
            if *training_greater_attacker && let Some(self_id) = sub.event_object {
                let self_pow = state
                    .evaluate_board()
                    .get(self_id)
                    .and_then(|e| e.pt)
                    .map_or(0, |pt| pt.power);
                let has_greater = state.combat().attackers.iter().any(|att| {
                    att.attacker != self_id
                        && state
                            .evaluate_board()
                            .get(att.attacker)
                            .and_then(|e| e.pt)
                            .is_some_and(|pt| pt.power > self_pow)
                });
                if !has_greater {
                    return false;
                }
            }
            if let Some(baf) = blocked_attacker_filter {
                let Some(att_id) = sub.blocker_attacker else {
                    return false;
                };
                if !matches_object_filter(state, att_id, baf, ctx) {
                    return false;
                }
            }
            if let Some(hf) = host {
                let Some(host_id) = state.object(cand.source_id).and_then(|o| o.attached_to) else {
                    return false;
                };
                if !matches_object_filter(state, host_id, hf, ctx) {
                    return false;
                }
            }
            if *without_flanking
                && let Some(att_id) = sub.blocker_attacker
                && let Some(flank_id) = state.db().keyword_id("flanking")
                && state
                    .evaluate_board()
                    .get(att_id)
                    .is_some_and(|e| e.keywords.contains(&flank_id))
            {
                return false;
            }
            if let Some(atf) = attacked_target {
                let Some(def_p) = sub.defending_player else {
                    return false;
                };
                let atk_tgt = sub
                    .attack_target
                    .unwrap_or(AttackTarget::Player { id: def_p });
                let ok = match atf {
                    mtg_ir::AttackedTargetFilter::You => {
                        matches!(atk_tgt, AttackTarget::Player { id } if id == cand.source_ctrl)
                    }
                    mtg_ir::AttackedTargetFilter::YouOrPlaneswalkerYouControl => match atk_tgt {
                        AttackTarget::Player { id } => id == cand.source_ctrl,
                        AttackTarget::Planeswalker { id } => {
                            state.evaluate_board().get(id).is_some_and(|e| {
                                e.controller == cand.source_ctrl
                                    && e.types.contains(CardTypeMask::PLANESWALKER)
                            })
                        }
                        AttackTarget::Battle { .. } => false,
                    },
                    mtg_ir::AttackedTargetFilter::Opponent => {
                        matches!(atk_tgt, AttackTarget::Player { id } if id != cand.source_ctrl)
                    }
                    mtg_ir::AttackedTargetFilter::OpponentOrPlaneswalkerOpponentControls => {
                        match atk_tgt {
                            AttackTarget::Player { id } => id != cand.source_ctrl,
                            AttackTarget::Planeswalker { id } => {
                                state.evaluate_board().get(id).is_some_and(|e| {
                                    e.controller != cand.source_ctrl
                                        && e.types.contains(CardTypeMask::PLANESWALKER)
                                })
                            }
                            AttackTarget::Battle { .. } => false,
                        }
                    }
                    mtg_ir::AttackedTargetFilter::EnchantedPlayer => {
                        matches!(atk_tgt, AttackTarget::Player { id }
                            if state.rare(cand.source_id).and_then(|r| r.chosen_player) == Some(id))
                    }
                    mtg_ir::AttackedTargetFilter::PlayerWithMostLife => match atk_tgt {
                        AttackTarget::Player { id } => {
                            let max_life = state
                                .players()
                                .iter()
                                .filter(|p| !p.lost)
                                .map(|p| p.life)
                                .max()
                                .unwrap_or(i32::MIN);
                            state
                                .players()
                                .get(usize::from(id.0))
                                .is_some_and(|p| p.life >= max_life)
                        }
                        AttackTarget::Planeswalker { .. } | AttackTarget::Battle { .. } => false,
                    },
                    mtg_ir::AttackedTargetFilter::AnyPlayer => {
                        matches!(atk_tgt, AttackTarget::Player { .. })
                    }
                    mtg_ir::AttackedTargetFilter::Planeswalker => match atk_tgt {
                        AttackTarget::Planeswalker { id } => state
                            .evaluate_board()
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::PLANESWALKER)),
                        AttackTarget::Player { .. } | AttackTarget::Battle { .. } => false,
                    },
                    mtg_ir::AttackedTargetFilter::Battle => match atk_tgt {
                        AttackTarget::Battle { id } => state
                            .evaluate_board()
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::BATTLE)),
                        AttackTarget::Player { .. } | AttackTarget::Planeswalker { .. } => false,
                    },
                };
                if !ok {
                    return false;
                }
            }
            if let Some(caf) = co_attacker_filter {
                let co_count = state
                    .combat()
                    .attackers
                    .iter()
                    .filter(|att| {
                        Some(att.attacker) != sub.event_object
                            && matches_object_filter(state, att.attacker, caf, ctx)
                    })
                    .count();
                if co_count < usize::from(co_attacker_min.unwrap_or(1)) {
                    return false;
                }
            }
            if let Some(awf) = attacking_with_filter {
                let cnt = state
                    .combat()
                    .attackers
                    .iter()
                    .filter(|att| matches_object_filter(state, att.attacker, awf, ctx))
                    .count();
                if cnt < usize::from(min_attacking_with_count.unwrap_or(1)) {
                    return false;
                }
            }
            if let Some(min_pow) = attacking_total_power_min {
                let total_pow: i32 = state
                    .combat()
                    .attackers
                    .iter()
                    .filter_map(|att| {
                        state
                            .evaluate_board()
                            .get(att.attacker)
                            .and_then(|e| e.pt)
                            .map(|pt| pt.power.max(0))
                    })
                    .sum();
                if total_pow < *min_pow {
                    return false;
                }
            }
            if *attack_enchanted_or_attacked_by_enchanted {
                let ench_p = state.rare(cand.source_id).and_then(|r| r.chosen_player);
                if sub.event_player != ench_p && sub.defending_player != ench_p {
                    return false;
                }
            }
            true
        }
        EventDetail::LandPlayed {
            cast_from_not_hand,
            cast_from_zone,
        } => {
            if *cast_from_not_hand && sub.from_zone == Some(Zone::Hand) {
                return false;
            }
            if let Some(fz) = cast_from_zone
                && sub.from_zone != Some(*fz)
            {
                return false;
            }
            true
        }
        EventDetail::Damage {
            combat,
            noncombat_only,
            damage_to,
            damage_target_kind,
            damage_player_relation,
            damaged_player,
            damage_target_filter,
            damage_target_enchanted_player,
            single_source_damage_min,
            combat_damage_batch_player,
        } => {
            if let Some(req_c) = combat
                && sub.is_combat_damage != *req_c
            {
                return false;
            }
            if *noncombat_only && sub.is_combat_damage {
                return false;
            }
            if let Some(min_dmg) = single_source_damage_min
                && sub.event_amount.unwrap_or(0) < *min_dmg
            {
                return false;
            }
            if let Some(dr) = damage_to {
                let ok = match (dr, sub.damage_target) {
                    (mtg_ir::DamageRecipient::Player, Some(TargetRef::Player { .. })) => true,
                    (
                        mtg_ir::DamageRecipient::PlayerOrPlaneswalker,
                        Some(TargetRef::Player { .. }),
                    ) => true,
                    (
                        mtg_ir::DamageRecipient::PlayerOrPlaneswalker,
                        Some(TargetRef::Object { id }),
                    ) => state
                        .evaluate_board()
                        .get(id)
                        .is_some_and(|e| e.types.contains(CardTypeMask::PLANESWALKER)),
                    (mtg_ir::DamageRecipient::Creature, Some(TargetRef::Object { id })) => state
                        .evaluate_board()
                        .get(id)
                        .is_some_and(|e| e.types.contains(CardTypeMask::CREATURE)),
                    (mtg_ir::DamageRecipient::Any, Some(_)) => true,
                    _ => false,
                };
                if !ok {
                    return false;
                }
            }
            if let Some(tk) = damage_target_kind {
                let ok = match (tk, sub.damage_target) {
                    (mtg_ir::DamageTargetKind::Any, Some(_)) => true,
                    (mtg_ir::DamageTargetKind::PlayerOrBattle, Some(TargetRef::Player { .. })) => {
                        true
                    }
                    (mtg_ir::DamageTargetKind::PlayerOrBattle, Some(TargetRef::Object { id })) => {
                        state
                            .evaluate_board()
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::BATTLE))
                    }
                    (mtg_ir::DamageTargetKind::Battle, Some(TargetRef::Object { id })) => state
                        .evaluate_board()
                        .get(id)
                        .is_some_and(|e| e.types.contains(CardTypeMask::BATTLE)),
                    (mtg_ir::DamageTargetKind::Planeswalker, Some(TargetRef::Object { id })) => {
                        state
                            .evaluate_board()
                            .get(id)
                            .is_some_and(|e| e.types.contains(CardTypeMask::PLANESWALKER))
                    }
                    (mtg_ir::DamageTargetKind::Permanent, Some(TargetRef::Object { .. })) => true,
                    _ => false,
                };
                if !ok {
                    return false;
                }
            }
            if let Some(rel) = damage_player_relation
                .or(*damaged_player)
                .or(*combat_damage_batch_player)
            {
                let target_pid = match sub.damage_target {
                    Some(TargetRef::Player { id }) => Some(id),
                    Some(TargetRef::Object { id })
                        if *damage_to == Some(mtg_ir::DamageRecipient::PlayerOrPlaneswalker) =>
                    {
                        state
                            .evaluate_board()
                            .get(id)
                            .filter(|e| e.types.contains(CardTypeMask::PLANESWALKER))
                            .map(|e| e.controller)
                    }
                    Some(TargetRef::Object { .. }) | Some(TargetRef::Stack { .. }) => None,
                    None => sub.event_player,
                };
                if !matches!(rel, RelativeControl::All | RelativeControl::Any) {
                    let Some(dp) = target_pid else {
                        return false;
                    };
                    if !matches_relative_control(dp, rel, ctx) {
                        return false;
                    }
                }
            }
            if let Some(dtf) = damage_target_filter {
                let Some(TargetRef::Object { id }) = sub.damage_target else {
                    return false;
                };
                if !matches_object_filter(state, id, dtf, ctx) {
                    return false;
                }
            }
            if *damage_target_enchanted_player {
                let Some(ench_p) = state.rare(cand.source_id).and_then(|r| r.chosen_player) else {
                    return false;
                };
                let target_p = match sub.damage_target {
                    Some(TargetRef::Player { id }) => Some(id),
                    Some(TargetRef::Object { .. }) | Some(TargetRef::Stack { .. }) => None,
                    None => sub.event_player,
                };
                if target_p != Some(ench_p) {
                    return false;
                }
            }
            true
        }
        EventDetail::Phase {
            phase,
            any_main_phase,
            enchanted_controller_upkeep,
            enchanted_player_upkeep,
            enchanted_opponent_end_step,
            phase_chosen_player,
            turn_filter,
        } => {
            let eff_phase = sub.event_phase.unwrap_or_else(|| state.phase());
            if let Some(req_p) = phase
                && eff_phase != *req_p
            {
                return false;
            }
            if *any_main_phase && !matches!(eff_phase, GamePhase::Main1 | GamePhase::Main2) {
                return false;
            }
            if *enchanted_controller_upkeep {
                let host_ctrl = state
                    .object(cand.source_id)
                    .and_then(|o| o.attached_to)
                    .and_then(|hid| effective_controller_for_object(state, hid, ctx));
                if sub.event_player != host_ctrl {
                    return false;
                }
            }
            if *enchanted_player_upkeep {
                let ench_p = state.rare(cand.source_id).and_then(|r| r.chosen_player);
                if sub.event_player != ench_p {
                    return false;
                }
            }
            if *enchanted_opponent_end_step {
                let ench_p = state.rare(cand.source_id).and_then(|r| r.chosen_player);
                if sub.event_player != ench_p || sub.event_player == Some(cand.source_ctrl) {
                    return false;
                }
            }
            if *phase_chosen_player {
                let chosen = state.rare(cand.source_id).and_then(|r| r.chosen_player);
                if sub.event_player != chosen {
                    return false;
                }
            }
            if let Some(tf) = turn_filter {
                match tf {
                    mtg_ir::PhaseTurnFilter::FirstOfGame => {
                        if state.turn_history().first_turn_of_game_done {
                            return false;
                        }
                    }
                    mtg_ir::PhaseTurnFilter::YourFirstUpkeep => {
                        if eff_phase != GamePhase::Upkeep
                            || sub.event_player != Some(cand.source_ctrl)
                            || state
                                .turn_history()
                                .first_upkeep_seen
                                .contains(&cand.source_ctrl)
                        {
                            return false;
                        }
                    }
                    mtg_ir::PhaseTurnFilter::Monarch => {
                        if state.monarch() != Some(cand.source_ctrl) {
                            return false;
                        }
                    }
                }
            }
            true
        }
        EventDetail::Counter {
            counter,
            last_counter,
            put_by_player,
            nth_counter_on_self,
        } => {
            if let Some(req_c) = counter
                && sub.counter_id != Some(*req_c)
            {
                return false;
            }
            if *last_counter && !(sub.new_counter_count == 0 && sub.prev_counter_count > 0) {
                return false;
            }
            if let Some(rel) = put_by_player
                && !sub
                    .event_player
                    .is_some_and(|p| matches_relative_control(p, *rel, ctx))
            {
                return false;
            }
            if let Some(nth) = nth_counter_on_self
                && (sub.event_object != Some(cand.source_id)
                    || sub.new_counter_count < u32::from(*nth)
                    || sub.prev_counter_count >= u32::from(*nth))
            {
                return false;
            }
            true
        }
        EventDetail::BecomesTarget {
            targeted_by,
            targeted_by_activated_ability,
            first_time_targeted_by_you_this_turn,
            targeting_source_filter,
            target_player,
        } => {
            if let Some(tb) = targeted_by {
                let ok = match tb {
                    mtg_ir::TargetedByKind::Spell => sub.by_spell,
                    mtg_ir::TargetedByKind::Ability => sub.by_ability,
                    mtg_ir::TargetedByKind::SpellOrAbility => sub.by_spell || sub.by_ability,
                };
                if !ok {
                    return false;
                }
            }
            if *targeted_by_activated_ability && !sub.by_ability {
                return false;
            }
            if *first_time_targeted_by_you_this_turn {
                let Some(tid) = sub.event_object else {
                    return false;
                };
                // Checked BEFORE record_post_trigger_history writes `targeted_by_this_turn`.
                if sub.event_player != Some(cand.source_ctrl)
                    || state
                        .turn_history()
                        .was_targeted_by_this_turn(cand.source_ctrl, tid)
                {
                    return false;
                }
            }
            if let Some(tsf) = targeting_source_filter {
                let Some(sid) = sub.stack_item else {
                    return false;
                };
                let Some(se) = state.stack_entries().iter().find(|s| s.id == sid) else {
                    return false;
                };
                let src_obj = match se.kind {
                    StackEntryKind::Spell { object }
                    | StackEntryKind::Activated { source: object, .. }
                    | StackEntryKind::Triggered { source: object, .. } => object,
                };
                if !matches_object_filter(state, src_obj, tsf, ctx) {
                    return false;
                }
            }
            if let Some(rel) = target_player
                && !sub
                    .event_player
                    .is_some_and(|p| matches_relative_control(p, *rel, ctx))
            {
                return false;
            }
            true
        }
        EventDetail::Attach { host, host_self } => {
            if *host_self && sub.attachment_host != Some(cand.source_id) {
                return false;
            }
            if let Some(hf) = host {
                let Some(hid) = sub.attachment_host else {
                    return false;
                };
                if !matches_object_filter(state, hid, hf, ctx) {
                    return false;
                }
            }
            true
        }
        EventDetail::Mana {
            tapped_mana_color,
            tapped_mana_chosen_color,
        } => {
            if let Some(mc) = tapped_mana_color
                && !sub.last_tapped_mana_colors.contains(mc)
            {
                return false;
            }
            if *tapped_mana_chosen_color {
                let chosen = state
                    .rare(cand.source_id)
                    .and_then(|r| r.chosen_color)
                    .unwrap_or(ColorMask::empty());
                let any_match = sub
                    .last_tapped_mana_colors
                    .iter()
                    .any(|&mc| chosen.contains(ColorMask::from_mana_color(mc)));
                if !any_match {
                    return false;
                }
            }
            true
        }
        EventDetail::AbilityActivated {
            is_loyalty_ability,
            is_non_mana_ability,
            ability_source,
            ability_source_controlled_by_event_player,
        } => {
            if *is_loyalty_ability && !sub.is_loyalty_ability {
                return false;
            }
            if *is_non_mana_ability && sub.is_mana_ability {
                return false;
            }
            if let Some(asub) = ability_source
                && !matches_trigger_subject(state, cand, sub, asub, None, ctx)
            {
                return false;
            }
            if *ability_source_controlled_by_event_player
                && let (Some(src_id), Some(ep)) = (sub.event_object, sub.event_player)
                && effective_controller_for_object(state, src_id, ctx) != Some(ep)
            {
                return false;
            }
            true
        }
        EventDetail::Discard {
            madness_exiled,
            caused_by_opponent,
        } => {
            if *madness_exiled && sub.to_zone != Some(Zone::Exile) {
                return false;
            }
            if *caused_by_opponent && !sub.discard_caused_by_opp {
                return false;
            }
            true
        }
        EventDetail::Draw {
            except_first_in_draw_step,
        } => !(*except_first_in_draw_step && sub.in_own_draw_step_ordinal == Some(1)),
        EventDetail::Transform { to_face_filter } => {
            if let Some(tff) = to_face_filter {
                let Some(oid) = sub.event_object else {
                    return false;
                };
                if !matches_object_filter(state, oid, tff, ctx) {
                    return false;
                }
            }
            true
        }
        EventDetail::Tapped { tapped_by_player } => {
            if let Some(rel) = tapped_by_player
                && !sub
                    .event_player
                    .is_some_and(|p| matches_relative_control(p, *rel, ctx))
            {
                return false;
            }
            true
        }
        EventDetail::DieOrCoin {
            die_roll_min,
            die_roll_values,
            won_coin_flip,
        } => {
            if let Some(min_v) = die_roll_min
                && sub.die_roll_result < i32::from(*min_v)
            {
                return false;
            }
            if !die_roll_values.is_empty()
                && !die_roll_values
                    .iter()
                    .any(|&v| i32::from(v) == sub.die_roll_result)
            {
                return false;
            }
            if *won_coin_flip && !sub.won_coin_flip {
                return false;
            }
            true
        }
        EventDetail::State { condition } => evaluate_condition(state, condition, ctx),
        EventDetail::Misc {
            amass_subtype,
            expend_threshold,
            class_level,
            this_door_face_index,
            day_night_direction,
            final_chapter,
            won,
        } => {
            if let Some(req_st) = amass_subtype
                && sub.amass_subtype != Some(*req_st)
            {
                return false;
            }
            if let Some(et) = expend_threshold
                && sub.expend_new_total < u32::from(*et)
            {
                return false;
            }
            if let Some(cl) = class_level
                && sub.class_level != u16::from(*cl)
            {
                return false;
            }
            if let Some(dfi) = this_door_face_index
                && sub.door_face_index != *dfi
            {
                return false;
            }
            if let Some(dnd) = day_night_direction {
                let ok = match dnd {
                    mtg_ir::DayNightDirection::Any => true,
                    mtg_ir::DayNightDirection::DayNightFlip => sub.day_night_prev.is_some(),
                    mtg_ir::DayNightDirection::ToDay => !sub.day_night_now,
                    mtg_ir::DayNightDirection::ToNight => sub.day_night_now,
                };
                if !ok {
                    return false;
                }
            }
            if *final_chapter && !sub.saga_is_final {
                return false;
            }
            if let Some(req_won) = won
                && sub.clash_won != *req_won
            {
                return false;
            }
            true
        }
    }
}

fn is_ward_suppressed(
    state: &GameState,
    cand: &TriggerCandidate,
    sub: &MatchedSubEvent,
    ev_kind: &crate::events::AtomicEventKind,
) -> bool {
    let crate::events::AtomicEventKind::BecomesTarget { controller, .. } = ev_kind else {
        return false;
    };
    let is_ward = match &cand.kind {
        TriggerSourceKind::KeywordGranted { keyword } => state
            .db()
            .keyword_name(*keyword)
            .is_some_and(|n| n.eq_ignore_ascii_case("ward")),
        _ => {
            cand.text
                .as_deref()
                .is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("ward"))
                || (cand.trigger.event == EventKind::BecomesTarget
                    && cand.effect.iter().any(|s| {
                        matches!(
                            s.op,
                            EffectOp::CounterSpell {
                                unless_pays_generic: Some(_),
                                ..
                            }
                        )
                    }))
        }
    };
    if !is_ward {
        return false;
    }
    let _ = sub;
    state
        .evaluate_board()
        .player(*controller)
        .is_some_and(|ps| ps.suppress_ward_triggers)
}

/// Computes `CR 603.2d` trigger multiplier (`StaticBody::Multiplier { what: Triggers, .. }`).
///
/// Multipliers stack additively (`1 + sum(max(0, factor - 1) + delta)`). Any matching active
/// suppressor (`factor <= 0.0`, e.g., Torpor Orb / Elesh Norn, Mother of Machines) returns `0`.
pub fn trigger_multiplier(
    state: &GameState,
    triggered_source: ObjectId,
    triggered_lki: Option<&LkiSnapshot>,
    triggered_ctrl: PlayerId,
    event: &crate::events::AtomicEventKind,
) -> i32 {
    let mut times: i32 = 1;
    let cache = state.evaluate_board();

    for p in state.players() {
        if p.lost {
            continue;
        }
        let mult_sources = p
            .battlefield
            .iter()
            .copied()
            .map(|id| (id, false))
            .chain(p.command.iter().copied().map(|id| (id, true)));

        for (src_id, is_cmd) in mult_sources {
            let Some(obj) = state.object(src_id) else {
                continue;
            };
            if is_cmd {
                if !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                    continue;
                }
            } else if obj.is_phased_out() {
                continue;
            }
            if !is_cmd && cache.get(src_id).is_some_and(|ev| ev.lost_all_abilities) {
                continue;
            }
            let src_ctrl = if is_cmd {
                obj.controller
            } else {
                cache.get(src_id).map_or(obj.controller, |e| e.controller)
            };
            let ctx = EvalContext::for_source(src_ctrl, src_id);
            let abilities = if is_cmd {
                face_opts_for_object(state, src_id)
                    .map(|opts| active_abilities_where(state.db(), obj.card_def, &opts, |_| false))
                    .unwrap_or_default()
            } else {
                active_abilities_for_object(state, src_id, &ctx)
            };

            for ab in abilities {
                let AbilityDef::Static(st) = ab.ability else {
                    continue;
                };
                let StaticBody::Multiplier {
                    what: MultiplierTarget::Triggers,
                    factor,
                    delta,
                    filter,
                    cause,
                    cause_or,
                    applies_to_players,
                    trigger_source_attached_to,
                    ..
                } = &st.static_def.body
                else {
                    continue;
                };
                if let Some(cond) = st.static_def.condition.as_deref()
                    && !evaluate_condition(state, cond, &ctx)
                {
                    continue;
                }
                if *applies_to_players && src_ctrl != triggered_ctrl {
                    // Applies to players: check player filter / controller match.
                    continue;
                }
                if *trigger_source_attached_to && obj.attached_to != Some(triggered_source) {
                    continue;
                }
                if let Some(f) = filter.as_deref() {
                    let ok = if state
                        .object(triggered_source)
                        .is_some_and(|o| o.zone == Zone::Battlefield)
                    {
                        matches_object_filter(state, triggered_source, f, &ctx)
                    } else if let Some(lki) = triggered_lki {
                        matches_lki_filter(state, lki, f, &ctx)
                    } else {
                        false
                    };
                    if !ok {
                        continue;
                    }
                }
                if let Some(c) = cause {
                    if !event_matches_pattern(state, src_id, src_ctrl, c, event) {
                        continue;
                    }
                } else if !cause_or.is_empty()
                    && !cause_or
                        .iter()
                        .any(|c| event_matches_pattern(state, src_id, src_ctrl, c, event))
                {
                    continue;
                }

                if *factor <= 0.0 {
                    return 0;
                }
                times += ((*factor as i32) - 1).max(0) + i32::from(*delta);
            }
        }
    }

    times.max(0)
}

fn event_matches_pattern(
    state: &GameState,
    mult_source: ObjectId,
    mult_ctrl: PlayerId,
    pattern: &EventPattern,
    event: &crate::events::AtomicEventKind,
) -> bool {
    let dummy_cand = TriggerCandidate {
        source_id: mult_source,
        source_zone: Zone::Battlefield,
        source_ctrl: mult_ctrl,
        source_lki: Arc::new(
            crate::events::snapshot_for_live_object(state, mult_source).unwrap_or_else(|| {
                LkiSnapshot {
                    object_id: mult_source,
                    card_def: CardDefId(0),
                    name: String::new(),
                    owner: mult_ctrl,
                    controller: mult_ctrl,
                    last_zone: Zone::Battlefield,
                    pt: None,
                    types: CardTypeMask::empty(),
                    subtypes: SmallVec::new(),
                    supertypes: mtg_ir::SupertypeMask::empty(),
                    colors: ColorMask::empty(),
                    mana_value: 0,
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
                }
            }),
        ),
        face_index: 0,
        ability_idx: 0,
        kind: TriggerSourceKind::Innate {
            component_index: 0,
            face_index: 0,
            ability_idx: 0,
        },
        trigger: TriggerDef {
            event: pattern.event,
            filter: pattern.filter.clone(),
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
        targets: vec![],
        effect: vec![],
        modal: None,
        text: None,
        is_look_back: false,
        watched_objects: SmallVec::new(),
        delayed_id: None,
        delayed_expiry: None,
        x_value: 0,
    };

    let subs = extract_matching_subevents(state, &dummy_cand.trigger, event);
    subs.iter().any(|sub| {
        matches_event_filter_and_detail(state, &dummy_cand, &dummy_cand.trigger, sub, event)
    })
}

fn record_pending_trigger(
    state: &mut GameState,
    cand: &TriggerCandidate,
    alt: &TriggerDef,
    sub: &MatchedSubEvent,
    ev_kind: &crate::events::AtomicEventKind,
) -> bool {
    // 1. CR 603.4 intervening-if at trigger moment: if false, never queue AND do NOT consume
    //    `only_once_per_turn` / `do_this_once_per_turn`!
    if let Some(cond) = &cand.condition {
        let ctx = build_trigger_eval_ctx(cand, sub);
        if !evaluate_condition(state, cond, &ctx) {
            return false;
        }
    }

    let origin = trigger_ability_origin(&cand.kind);

    // 2. CR 603.2h `do_this_once_per_turn`
    if alt.do_this_once_per_turn || cand.trigger.do_this_once_per_turn {
        let key = OncePerTurnKey {
            object: cand.source_id,
            zone: cand.source_zone,
            face_index: cand.face_index,
            ability_idx: cand.ability_idx,
            origin,
            is_do_this_once: true,
        };
        if state.do_this_once_taken_this_turn().contains(&key) {
            return false;
        }
    }

    // 3. CR 603.2i `only_once_per_turn` (check before mutating)
    let is_only_once = alt.only_once_per_turn || cand.trigger.only_once_per_turn;
    let opt_key = OncePerTurnKey {
        object: cand.source_id,
        zone: cand.source_zone,
        face_index: cand.face_index,
        ability_idx: cand.ability_idx,
        origin,
        is_do_this_once: false,
    };
    if is_only_once && state.triggered_once_this_turn().contains(&opt_key) {
        return false;
    }

    // 4. CR 603.2d trigger multiplier (`only_once_per_turn` clamps to 1; factor <= 0 suppresses).
    let mut count = if is_only_once {
        1
    } else {
        trigger_multiplier(
            state,
            cand.source_id,
            Some(cand.source_lki.as_ref()),
            cand.source_ctrl,
            ev_kind,
        )
    };
    if is_only_once && count > 1 {
        count = 1;
    }
    if count <= 0 {
        return false;
    }

    // Now commit state mutations (once-per-turn, one_or_more batch key, one-shot delayed expiry).
    if is_only_once {
        state.triggered_once_this_turn_mut().push(opt_key);
    }
    if alt.filter.one_or_more && state.defer_flush() > 0 {
        let bkey = BatchTriggerKey {
            object: cand.source_id,
            zone: cand.source_zone,
            face_index: cand.face_index,
            ability_idx: cand.ability_idx,
            origin,
            qualifier: sub.batch_qualifier,
        };
        if !state.batch_triggered_keys().contains(&bkey) {
            state.batch_triggered_keys_mut().push(bkey);
        }
    }
    if let (Some(did), Some(DelayedExpiry::OneShot)) = (cand.delayed_id, cand.delayed_expiry)
        && let Some(pos) = state.delayed_triggers().iter().position(|d| d.id == did)
    {
        state.delayed_triggers_mut().remove(pos);
    }

    let mode = if is_triggered_mana_ability(alt, &cand.targets, cand.modal.as_ref(), &cand.effect)
        && state.inline_mana_depth() < MAX_INLINE_MANA_DEPTH
    {
        TriggerMode::Inline
    } else {
        TriggerMode::Stack
    };

    for _ in 0..count {
        let order = state.allocate_trigger_order();
        state.pending_triggers_mut().push(PendingTrigger {
            order,
            mode,
            source: cand.source_id,
            source_lki: Arc::clone(&cand.source_lki),
            controller: cand.source_ctrl,
            kind: cand.kind,
            trigger_def: alt.clone(),
            condition: cand.condition.clone(),
            targets: cand.targets.clone(),
            effect: cand.effect.clone(),
            modal: cand.modal.clone(),
            event_object: sub.event_new_id.or(sub.event_object),
            event_lki: sub.event_lki.clone(),
            event_player: sub.event_player,
            event_amount: sub.event_amount,
            defending_player: sub.defending_player,
            last_tapped_mana_colors: sub.last_tapped_mana_colors.clone(),
            is_look_back: cand.is_look_back,
            x_value: cand.x_value,
        });
    }
    true
}

/// Evaluates `StaticBody::TapForManaMultiplier` (`CR 106.12`, e.g., Mana Reflection / Nyxbloom Ancient).
pub fn tap_for_mana_multiplier(state: &GameState, tapper: PlayerId, tapped_perm: ObjectId) -> u32 {
    let mut mult: u32 = 1;
    let cache = state.evaluate_board();

    for p in state.players() {
        if p.lost {
            continue;
        }
        let sources = p
            .battlefield
            .iter()
            .copied()
            .map(|id| (id, false))
            .chain(p.command.iter().copied().map(|id| (id, true)));
        for (src_id, is_cmd) in sources {
            let Some(obj) = state.object(src_id) else {
                continue;
            };
            if is_cmd {
                if !obj.flags.contains(ObjectFlags::IS_EMBLEM) {
                    continue;
                }
            } else if obj.is_phased_out() {
                continue;
            }
            if !is_cmd && cache.get(src_id).is_some_and(|ev| ev.lost_all_abilities) {
                continue;
            }
            let src_ctrl = if is_cmd {
                obj.controller
            } else {
                cache.get(src_id).map_or(obj.controller, |e| e.controller)
            };
            let ctx =
                EvalContext::for_source(src_ctrl, src_id).with_board(BoardView::Cached(cache));
            let abilities = if is_cmd {
                face_opts_for_object(state, src_id)
                    .map(|opts| active_abilities_where(state.db(), obj.card_def, &opts, |_| false))
                    .unwrap_or_default()
            } else {
                active_abilities_for_object(state, src_id, &ctx)
            };

            for ab in abilities {
                let AbilityDef::Static(st) = ab.ability else {
                    continue;
                };
                let StaticBody::TapForManaMultiplier {
                    factor,
                    filter,
                    all_players,
                } = &st.static_def.body
                else {
                    continue;
                };
                if !*all_players && src_ctrl != tapper {
                    continue;
                }
                if let Some(cond) = st.static_def.condition.as_deref()
                    && !evaluate_condition(state, cond, &ctx)
                {
                    continue;
                }
                if matches_object_filter(state, tapped_perm, filter, &ctx) {
                    mult = mult.saturating_mul(u32::from((*factor).max(1)));
                }
            }
        }
    }
    mult
}

/// Immediately executes pending `TriggerMode::Inline` triggered mana abilities without using the
/// stack (`CR 605.1b / 605.4a`), leaving `TriggerMode::Stack` triggers intact.
pub fn flush_inline_triggers(state: &mut GameState) {
    if state.defer_flush() > 0 || state.inline_mana_depth() >= MAX_INLINE_MANA_DEPTH {
        return;
    }

    for _guard in 0..1000 {
        if !state
            .pending_triggers()
            .iter()
            .any(|t| t.mode == TriggerMode::Inline)
        {
            break;
        }
        if matches!(state.decision(), DecisionState::GameOver { .. }) {
            return;
        }
        if state.inline_mana_depth() >= MAX_INLINE_MANA_DEPTH {
            // Convert any remaining inline triggers beyond depth limit to avoid infinite loop.
            state
                .pending_triggers_mut()
                .retain(|t| t.mode != TriggerMode::Inline);
            break;
        }

        let all = std::mem::take(state.pending_triggers_mut());
        let mut inline_batch = Vec::new();
        let mut remaining = Vec::new();
        for t in all {
            if t.mode == TriggerMode::Inline {
                inline_batch.push(t);
            } else {
                remaining.push(t);
            }
        }
        *state.pending_triggers_mut() = remaining;

        let order = apnap_player_order(state);
        // Reverse APNAP for inline mana resolution (matches TS triggers.ts:5213).
        let mut ordered = Vec::with_capacity(inline_batch.len());
        for &pid in order.iter().rev() {
            let mut per_player: Vec<PendingTrigger> = inline_batch
                .iter()
                .filter(|t| t.controller == pid)
                .cloned()
                .collect();
            per_player.sort_by_key(|t| t.order);
            ordered.extend(per_player);
        }

        let prev_depth = state.inline_mana_depth();
        state.set_inline_mana_depth(prev_depth.saturating_add(1));

        for t in ordered {
            if matches!(state.decision(), DecisionState::GameOver { .. }) {
                state.set_inline_mana_depth(prev_depth);
                return;
            }
            if let Some(cond) = &t.condition {
                let ctx = EvalContext {
                    controller: t.controller,
                    source: Some(t.source),
                    event_object: t.event_object,
                    event_lki: t.event_lki.as_deref(),
                    event_player: t.event_player,
                    event_amount: t.event_amount,
                    defending_player: t.defending_player,
                    lki_override: t.is_look_back.then_some(t.source_lki.as_ref()),
                    ..EvalContext::new(t.controller)
                };
                if !evaluate_condition(state, cond, &ctx) {
                    continue;
                }
            }
            execute_inline_mana_trigger(state, &t);
        }

        state.set_inline_mana_depth(prev_depth);
    }
}

fn mana_pool_index(c: ManaColor) -> usize {
    match c {
        ManaColor::W => 0,
        ManaColor::U => 1,
        ManaColor::B => 2,
        ManaColor::R => 3,
        ManaColor::G => 4,
        ManaColor::C => 5,
    }
}

fn add_mana_color(state: &mut GameState, player: PlayerId, color: ManaColor, count: u32) {
    if count == 0 {
        return;
    }
    if let Some(p) = state.player_mut(player) {
        let idx = mana_pool_index(color);
        p.mana_pool[idx] = p.mana_pool[idx].saturating_add(count);
    }
    state.mark_dirty(DirtyMask::PLAYER_STATE);
}

fn execute_inline_mana_trigger(state: &mut GameState, t: &PendingTrigger) {
    for step in &t.effect {
        let EffectOp::AddMana {
            production,
            restriction,
            restriction_spec,
            retain_until_eot,
            delayed_rider,
        } = &step.op
        else {
            continue;
        };
        let ctx = EvalContext {
            controller: t.controller,
            source: Some(t.source),
            event_object: t.event_object,
            event_lki: t.event_lki.as_deref(),
            event_player: t.event_player,
            event_amount: t.event_amount,
            defending_player: t.defending_player,
            lki_override: t.is_look_back.then_some(t.source_lki.as_ref()),
            ..EvalContext::new(t.controller)
        };
        if let Some(mtg_ir::StepGate::Condition { condition }) = &step.gate
            && !evaluate_condition(state, condition, &ctx)
        {
            continue;
        }
        let recipient = step
            .actor
            .as_ref()
            .and_then(|pr| resolve_player_ref(state, pr, &ctx))
            .unwrap_or(t.controller);

        let mult = if crate::mana::add_mana_ignores_step_amount(production) {
            1
        } else {
            step.amount
                .as_deref()
                .map_or(1u32, |amt| evaluate_amount(state, amt, &ctx).max(0) as u32)
        };
        if mult == 0 {
            continue;
        }

        match production {
            ManaProduction::Fixed { symbols } => {
                for &sym in symbols {
                    add_mana_color(state, recipient, sym, mult);
                }
            }
            ManaProduction::FromProducedEvent => {
                if let Some(&first_color) = t.last_tapped_mana_colors.first() {
                    add_mana_color(state, recipient, first_color, mult);
                }
            }
            ManaProduction::Choice { kind, colors } => {
                let chosen = match kind {
                    ManaChoiceKind::ChosenColor => state
                        .rare(t.source)
                        .and_then(|r| r.chosen_color)
                        .or_else(|| {
                            t.is_look_back
                                .then_some(t.source_lki.chosen_color)
                                .flatten()
                        })
                        .and_then(first_mana_color_from_mask)
                        .unwrap_or(ManaColor::C),
                    ManaChoiceKind::EventProducedColors | ManaChoiceKind::EventProducedTypes => t
                        .last_tapped_mana_colors
                        .first()
                        .copied()
                        .unwrap_or(ManaColor::C),
                    ManaChoiceKind::Any => ManaColor::W,
                    _ => first_mana_color_from_mask(*colors).unwrap_or(ManaColor::C),
                };
                add_mana_color(state, recipient, chosen, mult);
            }
            ManaProduction::ChosenColorAmount { amount } => {
                let chosen_opt = state
                    .rare(t.source)
                    .and_then(|r| r.chosen_color)
                    .or_else(|| {
                        t.is_look_back
                            .then_some(t.source_lki.chosen_color)
                            .flatten()
                    })
                    .and_then(first_mana_color_from_mask);
                if let Some(chosen) = chosen_opt {
                    let n = crate::mana::evaluate_chosen_color_amount(
                        state,
                        amount,
                        ColorMask::from_mana_color(chosen),
                        &ctx,
                    )
                    .max(0) as u32;
                    add_mana_color(state, recipient, chosen, n.saturating_mul(mult));
                }
            }
            ManaProduction::OrChosenColor { colors } => {
                let chosen = state
                    .rare(t.source)
                    .and_then(|r| r.chosen_color)
                    .or_else(|| {
                        t.is_look_back
                            .then_some(t.source_lki.chosen_color)
                            .flatten()
                    })
                    .and_then(first_mana_color_from_mask)
                    .or_else(|| first_mana_color_from_mask(*colors))
                    .unwrap_or(ManaColor::C);
                add_mana_color(state, recipient, chosen, mult);
            }
            ManaProduction::WithChosenColor { colors } => {
                for c in crate::mana::colors_from_mask(*colors) {
                    add_mana_color(state, recipient, c, mult);
                }
                if let Some(cm) = state
                    .rare(t.source)
                    .and_then(|r| r.chosen_color)
                    .or_else(|| {
                        t.is_look_back
                            .then_some(t.source_lki.chosen_color)
                            .flatten()
                    })
                {
                    for c in crate::mana::colors_from_mask(cm) {
                        add_mana_color(state, recipient, c, mult);
                    }
                }
            }
            ManaProduction::Bundles { options } => {
                if let Some(first_bundle) = options.first() {
                    for &c in first_bundle {
                        add_mana_color(state, recipient, c, mult);
                    }
                }
            }
            ManaProduction::Combination { any_color, colors } => {
                let c = if *any_color {
                    ManaColor::W
                } else {
                    first_mana_color_from_mask(*colors).unwrap_or(ManaColor::C)
                };
                add_mana_color(state, recipient, c, mult);
            }
            ManaProduction::LandProducedTypes
            | ManaProduction::FromExiledColors
            | ManaProduction::PerColorControlled => {
                let _ = crate::mana::add_mana_from_production(
                    state,
                    recipient,
                    Some(t.source),
                    production,
                    *restriction,
                    restriction_spec.as_deref(),
                    *retain_until_eot,
                    delayed_rider.as_deref(),
                    t.last_tapped_mana_colors.first().copied(),
                    mult,
                );
            }
        }
    }
}

fn first_mana_color_from_mask(mask: ColorMask) -> Option<ManaColor> {
    if mask.contains(ColorMask::W) {
        Some(ManaColor::W)
    } else if mask.contains(ColorMask::U) {
        Some(ManaColor::U)
    } else if mask.contains(ColorMask::B) {
        Some(ManaColor::B)
    } else if mask.contains(ColorMask::R) {
        Some(ManaColor::R)
    } else if mask.contains(ColorMask::G) {
        Some(ManaColor::G)
    } else if mask.contains(ColorMask::C) {
        Some(ManaColor::C)
    } else {
        None
    }
}

/// Checks state-triggered abilities (`CR 603.8`) in APNAP order and enqueues any whose condition
/// holds and which do not already have an instance from the same `(source, face_index, ability_idx)`
/// on the stack or in `pending_triggers`. Returns the number of newly queued state triggers.
pub fn check_state_triggers(state: &mut GameState) -> usize {
    if matches!(
        state.decision(),
        DecisionState::Mulligan | DecisionState::GameOver { .. }
    ) {
        return 0;
    }

    let cache = state.evaluate_board();
    let mut to_queue: Vec<TriggerCandidate> = Vec::new();

    for pid in apnap_player_order(state) {
        let Some(p) = state.players().get(usize::from(pid.0)) else {
            continue;
        };
        for &id in &p.battlefield {
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.is_phased_out() {
                continue;
            }
            let Some(eval_obj) = cache.get(id) else {
                continue;
            };
            if eval_obj.lost_all_abilities {
                continue;
            }
            let ctx = EvalContext::for_source(eval_obj.controller, id);
            let mut lki_opt: Option<Arc<LkiSnapshot>> = None;

            for ab in active_abilities_with_card_for_object(state, id, &ctx) {
                let AbilityDef::Triggered(t) = ab.ability else {
                    continue;
                };
                if !t.trigger.is_state_trigger {
                    continue;
                }
                let Some(EventDetail::State {
                    condition: state_cond,
                }) = t.trigger.filter.detail.as_ref()
                else {
                    continue;
                };

                // CR 603.8 anti-loop guard: check stack_trigger_meta & pending_triggers for exact
                // `(source_id [with generation], component_index, face_index, ability_idx)`.
                let already_on_stack = state.stack_entries().iter().any(|se| {
                    state.stack_trigger_meta(se.id).is_some_and(|m| {
                        m.source == id
                            && matches!(
                                m.kind,
                                TriggerSourceKind::Innate {
                                    component_index,
                                    face_index,
                                    ability_idx,
                                } if component_index == ab.component_index
                                    && face_index == ab.face_index
                                    && ability_idx == ab.ability_index
                            )
                    })
                });
                let already_pending = state.pending_triggers().iter().any(|pt| {
                    pt.source == id
                        && matches!(
                            pt.kind,
                            TriggerSourceKind::Innate {
                                component_index,
                                face_index,
                                ability_idx,
                            } if component_index == ab.component_index
                                && face_index == ab.face_index
                                && ability_idx == ab.ability_index
                        )
                });
                if already_on_stack || already_pending {
                    continue;
                }

                let origin = TriggerAbilityOrigin::Innate {
                    component_index: ab.component_index,
                };
                if t.trigger.do_this_once_per_turn {
                    let key = OncePerTurnKey {
                        object: id,
                        zone: Zone::Battlefield,
                        face_index: ab.face_index,
                        ability_idx: ab.ability_index,
                        origin,
                        is_do_this_once: true,
                    };
                    if state.do_this_once_taken_this_turn().contains(&key) {
                        continue;
                    }
                }
                if t.trigger.only_once_per_turn {
                    let key = OncePerTurnKey {
                        object: id,
                        zone: Zone::Battlefield,
                        face_index: ab.face_index,
                        ability_idx: ab.ability_index,
                        origin,
                        is_do_this_once: false,
                    };
                    if state.triggered_once_this_turn().contains(&key) {
                        continue;
                    }
                }

                if !evaluate_condition(state, state_cond, &ctx) {
                    continue;
                }
                if let Some(cond) = &t.condition
                    && !evaluate_condition(state, cond, &ctx)
                {
                    continue;
                }

                let lki = lki_opt.get_or_insert_with(|| {
                    Arc::new(crate::events::build_batch_lki_snapshot(state, id, eval_obj))
                });
                to_queue.push(TriggerCandidate {
                    source_id: id,
                    source_zone: Zone::Battlefield,
                    source_ctrl: eval_obj.controller,
                    source_lki: Arc::clone(lki),
                    face_index: ab.face_index,
                    ability_idx: ab.ability_index,
                    kind: TriggerSourceKind::Innate {
                        component_index: ab.component_index,
                        face_index: ab.face_index,
                        ability_idx: ab.ability_index,
                    },
                    trigger: t.trigger.clone(),
                    condition: t.condition.as_deref().cloned(),
                    targets: t.targets.clone(),
                    effect: t.effect.clone(),
                    modal: t.modal.clone(),
                    text: t.text.clone(),
                    is_look_back: false,
                    watched_objects: SmallVec::new(),
                    delayed_id: None,
                    delayed_expiry: None,
                    x_value: 0,
                });
            }
        }
    }

    let added = to_queue.len();
    for cand in to_queue {
        if cand.trigger.only_once_per_turn {
            let key = OncePerTurnKey {
                object: cand.source_id,
                zone: Zone::Battlefield,
                face_index: cand.face_index,
                ability_idx: cand.ability_idx,
                origin: trigger_ability_origin(&cand.kind),
                is_do_this_once: false,
            };
            state.triggered_once_this_turn_mut().push(key);
        }
        let order = state.allocate_trigger_order();
        state.pending_triggers_mut().push(PendingTrigger {
            order,
            mode: TriggerMode::Stack,
            source: cand.source_id,
            source_lki: cand.source_lki,
            controller: cand.source_ctrl,
            kind: cand.kind,
            trigger_def: cand.trigger,
            condition: cand.condition,
            targets: cand.targets,
            effect: cand.effect,
            modal: cand.modal,
            event_object: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: None,
            last_tapped_mana_colors: SmallVec::new(),
            is_look_back: false,
            x_value: 0,
        });
    }
    added
}

/// Evaluates whether a triggered ability on the stack still satisfies its `CR 603.4` intervening-if
/// condition upon resolution. Returns `true` if the trigger has no intervening-if condition.
pub fn trigger_intervening_if_holds_on_resolution(
    state: &GameState,
    stack_id: StackItemId,
) -> bool {
    let Some(meta) = state.stack_trigger_meta(stack_id) else {
        return true;
    };
    let Some(cond) = &meta.condition else {
        return true;
    };
    let Some(se) = state.stack_entries().iter().find(|s| s.id == stack_id) else {
        return false;
    };
    let source_lki = match &se.kind {
        StackEntryKind::Triggered { lki, .. } | StackEntryKind::Activated { lki, .. } => {
            Some(lki.as_ref())
        }
        StackEntryKind::Spell { .. } => None,
    };
    let ctx = EvalContext {
        controller: se.controller,
        source: Some(meta.source),
        stack_item: Some(se),
        x_value: Some(meta.x_value),
        targets: &se.targets,
        event_object: meta.event_object,
        event_lki: meta.event_lki.as_deref(),
        event_player: meta.event_player,
        event_amount: meta.event_amount,
        defending_player: meta.defending_player,
        bindings: &meta.bindings,
        lki_override: source_lki,
        board: BoardView::Default,
        ..EvalContext::new(se.controller)
    };
    evaluate_condition(state, cond, &ctx)
}

/// Checks via joint bounded backtracking whether `targets` admits at least one legal assignment (`CR 603.3d`).
#[allow(clippy::too_many_arguments)]
pub fn has_legal_targets_for_specs(
    state: &GameState,
    controller: PlayerId,
    source: ObjectId,
    source_lki: &LkiSnapshot,
    targets: &[TargetSpec],
    event_object: Option<ObjectId>,
    event_lki: Option<&LkiSnapshot>,
    event_player: Option<PlayerId>,
) -> bool {
    if targets.is_empty() {
        return true;
    }
    let cache = state.evaluate_board();
    let mut info = crate::targets::build_ability_target_source_info_cached(
        state,
        controller,
        source,
        Some(source_lki),
        None,
        &[],
        &[],
        cache,
    );
    info.event_object = event_object;
    info.event_lki = event_lki;
    info.event_player = event_player;
    crate::targets::has_legal_target_assignment_cached(state, targets, &info, cache)
}

#[allow(clippy::too_many_arguments)]
fn has_legal_trigger_targets_and_modes(
    state: &GameState,
    controller: PlayerId,
    source: ObjectId,
    source_lki: &LkiSnapshot,
    targets: &[TargetSpec],
    modal: Option<&ModalDef>,
    x_value: u32,
    bindings: &[ResolutionBinding],
    event_object: Option<ObjectId>,
    event_lki: Option<&LkiSnapshot>,
    event_player: Option<PlayerId>,
    defending_player: Option<PlayerId>,
) -> bool {
    if targets.is_empty() && modal.is_none() {
        return true;
    }
    let cache = state.evaluate_board();
    let mut info = crate::targets::build_ability_target_source_info_cached(
        state,
        controller,
        source,
        Some(source_lki),
        Some(x_value),
        &[],
        bindings,
        cache,
    );
    info.event_object = event_object;
    info.event_lki = event_lki;
    info.event_player = event_player;
    info.defending_player = defending_player;
    let eval_ctx = EvalContext {
        controller,
        source: Some(source),
        x_value: Some(x_value),
        bindings,
        event_object,
        event_lki,
        event_player,
        defending_player,
        lki_override: Some(source_lki),
        board: BoardView::Cached(cache),
        ..EvalContext::new(controller)
    };
    crate::targets::has_legal_modal_target_assignment_cached(
        state,
        targets,
        modal,
        &info,
        &eval_ctx,
        |_| true,
        cache,
    )
}

fn triggers_need_player_ordering(triggers: &[PendingTrigger]) -> bool {
    if triggers.len() < 2 {
        return false;
    }
    // ChoiceRequest::OrderObjects carries Vec<ObjectId>, so if any two simultaneous triggers share
    // the same source ObjectId, ObjectId ordering would be ambiguous. Also if all triggers have
    // functionally identical card/ability/effect with no targets or modal choices (e.g. t2_f10_03's
    // 10 identical GainLife(1) triggers from 2 copies of the same enchantment), auto-stack in
    // deterministic order without prompting.
    let mut seen_sources: SmallVec<[ObjectId; 8]> = SmallVec::new();
    for t in triggers {
        if seen_sources.contains(&t.source) {
            return false;
        }
        seen_sources.push(t.source);
    }
    let first = &triggers[0];
    let all_identical_simple = triggers.iter().all(|t| {
        t.targets.is_empty()
            && t.modal.is_none()
            && t.condition.is_none()
            && t.source_lki.card_def == first.source_lki.card_def
            && t.effect == first.effect
    });
    !all_identical_simple
}

/// Flushes all pending `Stack` triggers and `PendingReflexiveTrigger`s onto the stack in APNAP
/// order (`CR 603.3b`, `CR 603.3d`, `CR 603.12`), executing `Inline` mana triggers first.
///
/// Respects `state.defer_flush() > 0`, `state.pending_replacement().is_some()`, and paused prompt /
/// game-over decision states. If a player has `>= 2` distinct simultaneous triggers with unique
/// source objects, pauses with `ChoiceRequest::OrderObjects` (`CR 603.3b`).
pub fn flush_triggers(state: &mut GameState) {
    if state.defer_flush() > 0
        || state.pending_replacement().is_some()
        || state.pending_trigger_order().is_some()
        || matches!(
            state.decision(),
            DecisionState::GameOver { .. } | DecisionState::Prompt { .. }
        )
    {
        return;
    }

    for _guard in 0..1000 {
        if state.pending_triggers().is_empty() && state.pending_reflexive_triggers().is_empty() {
            break;
        }
        if state.pending_replacement().is_some()
            || state.pending_trigger_order().is_some()
            || matches!(
                state.decision(),
                DecisionState::GameOver { .. } | DecisionState::Prompt { .. }
            )
        {
            return;
        }

        // 1. Run inline triggered mana abilities before stack triggers (`CR 605.4a`).
        flush_inline_triggers(state);

        // 2. Drain `Stack` triggers and push in APNAP order (`CR 603.3b`).
        let all = std::mem::take(state.pending_triggers_mut());
        let mut stacked = Vec::new();
        let mut remaining = Vec::new();
        for t in all {
            if t.mode == TriggerMode::Stack {
                stacked.push(t);
            } else {
                remaining.push(t);
            }
        }
        *state.pending_triggers_mut() = remaining;

        if !stacked.is_empty() {
            let apnap = apnap_player_order(state);
            for (ap_idx, &pid) in apnap.iter().enumerate() {
                let mut player_trigs: Vec<PendingTrigger> = stacked
                    .iter()
                    .filter(|t| t.controller == pid)
                    .cloned()
                    .collect();
                player_trigs.sort_by_key(|t| t.order);

                let mut pushed_stack_ids: Vec<StackItemId> = Vec::new();
                let mut pushed_source_ids: Vec<ObjectId> = Vec::new();
                let mut kept_trigs: Vec<PendingTrigger> = Vec::new();

                for t in player_trigs {
                    // CR 603.3c-d: trigger with impossible modal choices or joint targets is
                    // removed cleanly before entering the stack.
                    if !has_legal_trigger_targets_and_modes(
                        state,
                        pid,
                        t.source,
                        &t.source_lki,
                        &t.targets,
                        t.modal.as_ref(),
                        t.x_value,
                        &[],
                        t.event_object,
                        t.event_lki.as_deref(),
                        t.event_player,
                        t.defending_player,
                    ) {
                        continue;
                    }
                    let sid = push_pending_trigger_to_stack(state, &t);
                    pushed_stack_ids.push(sid);
                    pushed_source_ids.push(t.source);
                    kept_trigs.push(t);
                }

                if triggers_need_player_ordering(&kept_trigs) {
                    let tail_players = &apnap[ap_idx + 1..];
                    for rem_t in stacked {
                        if tail_players.contains(&rem_t.controller) {
                            state.pending_triggers_mut().push(rem_t);
                        }
                    }
                    let prompt_id = state.allocate_prompt_id();
                    let resume_decision = state.decision().clone();
                    let resume_priority_player = state.priority_player();
                    let req = ChoiceRequest::OrderObjects {
                        prompt_id,
                        objects: pushed_source_ids.clone(),
                    };
                    state.set_pending_trigger_order(Some(PendingTriggerOrder {
                        prompt_id,
                        controller: pid,
                        stack_item_ids: pushed_stack_ids,
                        object_ids: pushed_source_ids,
                        chosen_top_first: Vec::new(),
                        resume_decision,
                        resume_priority_player,
                    }));
                    state.set_priority_player(pid);
                    state.set_decision(DecisionState::Prompt { request: req });
                    return;
                }
            }
        }

        // 3. Flush `PendingReflexiveTrigger`s in APNAP order WITHOUT trigger multipliers (`CR 603.12`).
        if !state.pending_reflexive_triggers().is_empty() {
            let reflexives = std::mem::take(state.pending_reflexive_triggers_mut());
            for pid in apnap_player_order(state) {
                for rt in reflexives.iter().filter(|r| r.controller == pid) {
                    if !has_legal_trigger_targets_and_modes(
                        state,
                        pid,
                        rt.source,
                        &rt.source_lki,
                        &rt.spec.targets,
                        rt.spec.modal.as_deref(),
                        rt.x_value,
                        &rt.bindings,
                        rt.event_object,
                        rt.event_lki.as_deref(),
                        rt.event_player,
                        None,
                    ) {
                        continue;
                    }
                    push_reflexive_trigger_to_stack(state, rt);
                }
            }
        }
    }
}

fn push_pending_trigger_to_stack(state: &mut GameState, t: &PendingTrigger) -> StackItemId {
    let sid = state.allocate_stack_id();
    let ability_idx = match &t.kind {
        TriggerSourceKind::Innate { ability_idx, .. }
        | TriggerSourceKind::Layer6Granted {
            grant_ability_idx: ability_idx,
            ..
        }
        | TriggerSourceKind::SagaChapter { ability_idx, .. } => *ability_idx,
        TriggerSourceKind::Layer6Inline {
            source_ability_index,
            ..
        } => *source_ability_index as usize,
        _ => 0,
    };
    let face_index = match &t.kind {
        TriggerSourceKind::Innate { face_index, .. }
        | TriggerSourceKind::Layer6Granted {
            grant_face_index: face_index,
            ..
        }
        | TriggerSourceKind::SagaChapter { face_index, .. } => *face_index,
        TriggerSourceKind::Layer6Inline {
            source_face_index, ..
        } => *source_face_index,
        _ => t.source_lki.active_face_index,
    };

    state.stack_mut().push(t.source);
    state.stack_entries_mut().push(StackEntry {
        id: sid,
        kind: StackEntryKind::Triggered {
            source: t.source,
            ability_idx,
            lki: Arc::clone(&t.source_lki),
        },
        controller: t.controller,
        choices: StackChoices::default(),
        targets: Vec::new(),
        x_value: t.x_value,
        paid_mana_colors: ColorMask::empty(),
    });
    state.push_stack_trigger_meta(
        sid,
        StackTriggerMeta {
            source: t.source,
            face_index,
            ability_idx,
            kind: t.kind,
            condition: t.condition.clone(),
            targets: t.targets.clone(),
            effect: t.effect.clone(),
            modal: t.modal.clone(),
            event_object: t.event_object,
            event_lki: t.event_lki.clone(),
            event_player: t.event_player,
            event_amount: t.event_amount,
            defending_player: t.defending_player,
            last_tapped_mana_colors: t.last_tapped_mana_colors.clone(),
            is_look_back: t.is_look_back,
            x_value: t.x_value,
            bindings: Vec::new(),
        },
    );
    state.mark_dirty(DirtyMask::TOPOLOGY);
    sid
}

fn push_reflexive_trigger_to_stack(
    state: &mut GameState,
    rt: &PendingReflexiveTrigger,
) -> StackItemId {
    let sid = state.allocate_stack_id();
    state.stack_mut().push(rt.source);
    state.stack_entries_mut().push(StackEntry {
        id: sid,
        kind: StackEntryKind::Triggered {
            source: rt.source,
            ability_idx: 0,
            lki: Arc::clone(&rt.source_lki),
        },
        controller: rt.controller,
        choices: StackChoices::default(),
        targets: Vec::new(),
        x_value: rt.x_value,
        paid_mana_colors: ColorMask::empty(),
    });
    state.push_stack_trigger_meta(
        sid,
        StackTriggerMeta {
            source: rt.source,
            face_index: rt.source_lki.active_face_index,
            ability_idx: 0,
            kind: TriggerSourceKind::Reflexive {
                reflexive_id: rt.id,
            },
            condition: None,
            targets: rt.spec.targets.clone(),
            effect: rt.spec.effect.clone(),
            modal: rt.spec.modal.as_deref().cloned(),
            event_object: rt.event_object,
            event_lki: rt.event_lki.clone(),
            event_player: rt.event_player,
            event_amount: rt.event_amount,
            defending_player: None,
            last_tapped_mana_colors: SmallVec::new(),
            is_look_back: false,
            x_value: rt.x_value,
            bindings: rt.bindings.clone(),
        },
    );
    state.mark_dirty(DirtyMask::TOPOLOGY);
    sid
}

/// Processes a `ChoiceAnswer::OrderNext` for an active `PendingTriggerOrder` (`CR 603.3b`),
/// reordering the player's simultaneous triggers on the stack once all are ordered.
pub fn resume_trigger_order(
    state: &mut GameState,
    actor: PlayerId,
    prompt_id: u32,
    answer: ChoiceAnswer,
) -> Result<(), EngineError> {
    let Some(mut order) = state.pending_trigger_order().cloned() else {
        return Err(EngineError::IllegalAction(
            "no pending trigger order prompt".into(),
        ));
    };
    if actor != order.controller || prompt_id != order.prompt_id {
        return Err(EngineError::IllegalAction(
            "prompt actor or prompt_id mismatch for trigger ordering".into(),
        ));
    }
    let ChoiceAnswer::OrderNext { id } = answer else {
        return Err(EngineError::IllegalAction(
            "expected OrderNext answer for trigger ordering".into(),
        ));
    };
    let Some(pos) = order.object_ids.iter().position(|&oid| oid == id) else {
        return Err(EngineError::IllegalAction(format!(
            "object {id:?} is not a remaining trigger to order"
        )));
    };
    order.object_ids.remove(pos);
    order.chosen_top_first.push(id);

    if order.object_ids.len() > 1 {
        let next_prompt_id = state.allocate_prompt_id();
        order.prompt_id = next_prompt_id;
        let req = ChoiceRequest::OrderObjects {
            prompt_id: next_prompt_id,
            objects: order.object_ids.clone(),
        };
        state.set_pending_trigger_order(Some(order));
        state.set_decision(DecisionState::Prompt { request: req });
        return Ok(());
    }

    if let Some(last_id) = order.object_ids.pop() {
        order.chosen_top_first.push(last_id);
    }

    // Reorder the matching stack positions: `chosen_top_first[0]` resolves FIRST (top of stack),
    // so bottom-to-top in `state.stack` / `state.stack_entries` is `chosen_top_first.iter().rev()`.
    let mut positions: Vec<usize> = order
        .stack_item_ids
        .iter()
        .filter_map(|sid| state.stack_entries().iter().position(|e| e.id == *sid))
        .collect();
    positions.sort_unstable();

    if positions.len() == order.chosen_top_first.len() {
        let mut paired: Vec<(ObjectId, StackEntry)> = Vec::with_capacity(positions.len());
        for &p in &positions {
            paired.push((state.stack()[p], state.stack_entries()[p].clone()));
        }
        let mut reordered = Vec::with_capacity(positions.len());
        for &obj_id in order.chosen_top_first.iter().rev() {
            if let Some(idx) = paired.iter().position(|(oid, _)| *oid == obj_id) {
                reordered.push(paired.remove(idx));
            }
        }
        if reordered.len() == positions.len() {
            for (slot, (oid, entry)) in positions.into_iter().zip(reordered) {
                state.stack_mut()[slot] = oid;
                state.stack_entries_mut()[slot] = entry;
            }
        }
    }

    state.set_pending_trigger_order(None);
    state.set_priority_player(order.resume_priority_player);
    state.set_decision(order.resume_decision);
    state.mark_dirty(DirtyMask::TOPOLOGY);
    flush_triggers(state);
    Ok(())
}

/// Clears turn-scoped trigger tracking (`only_once_per_turn`, `do_this_once_taken_this_turn`,
/// expiring `DelayedExpiry::ThisTurn` and past-turn `UntilYourNextTurn` delayed triggers)
/// without rolling over `TurnEventHistory` or `PlayerState` turn counters.
pub fn clear_turn_trigger_flags_only(state: &mut GameState) {
    let ending_active = state.active_player();
    let ending_turn = state.turn_number();

    state.triggered_once_this_turn_mut().clear();
    state.do_this_once_taken_this_turn_mut().clear();
    state.batch_triggered_keys_mut().clear();

    state.delayed_triggers_mut().retain(|dt| match dt.expiry {
        DelayedExpiry::ThisTurn => false,
        DelayedExpiry::UntilYourNextTurn {
            creator,
            created_turn,
        } => !(creator == ending_active && ending_turn > created_turn),
        DelayedExpiry::OneShot
        | DelayedExpiry::NextCombat { .. }
        | DelayedExpiry::NextUpkeep { .. }
        | DelayedExpiry::Permanent => true,
    });

    state.mark_dirty(DirtyMask::TURN_HISTORY);
}

/// Clears turn-scoped trigger tracking (`only_once_per_turn`, `do_this_once_taken_this_turn`,
/// expiring `DelayedExpiry::ThisTurn` and past-turn `UntilYourNextTurn` delayed triggers, and
/// turn-history counters). Called at turn cleanup / turn advance by M5b `turn.rs`.
pub fn clear_turn_trigger_state(state: &mut GameState) {
    clear_turn_trigger_flags_only(state);
    state.turn_history_mut().end_turn_rollover();
    let player_count = state.players().len();
    for i in 0..player_count {
        if let Some(p) = state.player_mut(PlayerId(i as u8)) {
            p.clear_turn_counters();
        }
    }
    state.mark_dirty(DirtyMask::TURN_HISTORY);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::events::{AtomicEventKind, emit_event, emit_zone_change, with_simultaneous_batch};
    use crate::layers::predicates::TimedContinuousEffect;
    use crate::state::GameObject;
    use crate::zones::{MoveRawOptions, move_object_raw};
    use mtg_ir::{
        Affected, Amount, CardDatabase, CardDef, CardLayout, Characteristics, Compare, CompareOp,
        Duration, FaceDef, FilterFlags, IR_V2_VERSION, InternTables, IrV2Bundle, Layer, LayerMod,
        ManaCost, ObjectFilter, PlayerRef, PrintedPt, Selector, StaticAbility, StaticDef,
        SupertypeMask, TargetKind, TriggeredAbility,
    };

    fn base_face(name: &str, types: CardTypeMask, colors: ColorMask) -> FaceDef {
        FaceDef {
            name: name.into(),
            characteristics: Characteristics {
                mana_cost: ManaCost::default(),
                types,
                subtypes: SmallVec::new(),
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
            abilities: vec![],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }
    }

    fn make_card(id: &str, name: &str, types: CardTypeMask, abilities: Vec<AbilityDef>) -> CardDef {
        let mut face = base_face(name, types, ColorMask::G);
        face.abilities = abilities;
        CardDef {
            oracle_id: id.into(),
            compiler_version: 46,
            text_hash: "0000".into(),
            name: name.into(),
            layout: CardLayout::Normal,
            faces: vec![face],
            fully_supported: true,
        }
    }

    fn gain_life_step(n: i32) -> EffectStep {
        EffectStep {
            op: EffectOp::GainLife {
                set_life_total: false,
            },
            subject: Selector::PlayerRef {
                player: PlayerRef::You,
            },
            actor: None,
            amount: Some(Box::new(Amount::Const { n })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        }
    }

    fn add_to_bf(state: &mut GameState, owner: PlayerId, def: CardDefId) -> ObjectId {
        let ts = state.allocate_timestamp();
        let oid = state.insert_object(
            GameObject::new(def, owner, Zone::Hand, ts, ObjectFlags::empty()),
            None,
        );
        state.player_mut(owner).unwrap().hand.push(oid);
        move_object_raw(
            state,
            oid,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap()
    }

    fn build_trig_db(extra: Vec<CardDef>) -> Arc<CardDatabase> {
        let mut cards = vec![make_card(
            "synth_land_trig",
            "Synth Wastes",
            CardTypeMask::LAND,
            vec![],
        )];
        cards.extend(extra);
        let mut tables = InternTables::default();
        tables.keywords.push("ward".into());
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
    }

    fn setup_2p(db: Arc<CardDatabase>) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 17).unwrap();
        state.set_decision(DecisionState::Priority);
        state
    }

    fn setup_4p(db: Arc<CardDatabase>) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut cfg = GameConfig::commander(4);
        cfg.commander = None;
        let mut state = GameState::new(
            db,
            cfg,
            &[deck.clone(), deck.clone(), deck.clone(), deck],
            23,
        )
        .unwrap();
        state.set_decision(DecisionState::Priority);
        state
    }

    fn death_trigger_def(filter: ObjectFilter) -> TriggerDef {
        TriggerDef {
            event: EventKind::ZoneChange,
            filter: EventFilter {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(filter),
                }),
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
            scope: None,
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: vec![],
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        }
    }

    #[test]
    fn suite1_pre_batch_lki_dying_creatures_see_each_other_and_lord_buff() {
        // Card 1: 2/2 Creature that triggers when ANOTHER creature dies.
        // Card 2: 1/1 BuffLord (+2/+2 to other creatures).
        // Card 3: Enchantment watcher: creature with power >= 4 dies.
        let c1 = make_card(
            "synth_peer_1",
            "Synth WitnessBear",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: death_trigger_def(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    flags: FilterFlags::OTHER,
                    ..Default::default()
                }),
                condition: None,
                grant_condition: None,
                targets: vec![],
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let c2 = make_card(
            "synth_lord_1",
            "Synth Lord",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer7c,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                flags: FilterFlags::OTHER,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::Pt {
                            power: Box::new(Amount::Const { n: 2 }),
                            toughness: Box::new(Amount::Const { n: 2 }),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let c3 = make_card(
            "synth_watch_1",
            "Synth PowerWatch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: death_trigger_def(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    power: Some(Compare {
                        op: CompareOp::Ge,
                        value: Box::new(Amount::Const { n: 4 }),
                    }),
                    ..Default::default()
                }),
                condition: None,
                grant_condition: None,
                targets: vec![],
                effect: vec![gain_life_step(4)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![c1, c2, c3]);
        let mut state = setup_2p(db);

        let peer = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let lord = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let _watch = add_to_bf(&mut state, PlayerId(0), CardDefId(3));

        // Destroy Lord first, then Peer inside simultaneous batch.
        with_simultaneous_batch(&mut state, &[lord, peer], |st| {
            for id in [lord, peer] {
                let (lki, ev) = crate::events::pre_batch_lki(st, id).unwrap();
                let lki = Arc::clone(lki);
                let ev = ev.clone();
                let raw = move_object_raw(
                    st,
                    id,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    Some(&ev),
                )
                .unwrap();
                emit_zone_change(
                    st,
                    raw.old_id,
                    raw.new_id,
                    Zone::Battlefield,
                    Zone::Graveyard,
                    Some(PlayerId(0)),
                    Some(PlayerId(0)),
                    Some(&lki),
                    false,
                );
            }
        });

        // 1) Peer (lookback CR 603.10a) saw Lord die (1 trigger), AND
        // 2) PowerWatch saw Peer die with pre-batch power 4 (1 trigger).
        assert_eq!(state.stack_entries().len(), 2);
    }

    #[test]
    fn suite2_apnap_stacking_2p_and_4p_skips_lost_players_cr603_3b_800_4a() {
        let watcher = make_card(
            "synth_apnap_watch",
            "Synth ApnapWatch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter::default(),
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        // 2-player APNAP check (active = P1 -> pushes P1 bottom, P0 top).
        {
            let db = build_trig_db(vec![watcher.clone()]);
            let mut state = setup_2p(db);
            let _w0 = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let _w1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));
            state.set_active_player(PlayerId(1));

            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::Upkeep,
                    active_player: PlayerId(1),
                },
            );
            flush_triggers(&mut state);
            let ctrls: Vec<PlayerId> = state.stack_entries().iter().map(|s| s.controller).collect();
            assert_eq!(ctrls, vec![PlayerId(1), PlayerId(0)]);
        }

        // 4-player APNAP check with P2 eliminated (`p.lost = true`, CR 800.4a).
        {
            let db = build_trig_db(vec![watcher]);
            let mut state = setup_4p(db);
            for seat in 0..4 {
                let _ = add_to_bf(&mut state, PlayerId(seat), CardDefId(1));
            }
            state.player_mut(PlayerId(2)).unwrap().lost = true;
            state.set_active_player(PlayerId(1));

            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::Upkeep,
                    active_player: PlayerId(1),
                },
            );
            flush_triggers(&mut state);
            // Active P1 -> P3 (P2 lost skipped!) -> P0
            let ctrls: Vec<PlayerId> = state.stack_entries().iter().map(|s| s.controller).collect();
            assert_eq!(ctrls, vec![PlayerId(1), PlayerId(3), PlayerId(0)]);
        }
    }

    #[test]
    fn suite3_intervening_if_dual_check_cr603_4_never_consumes_once_per_turn() {
        // Trigger on Upkeep ONLY IF you control an artifact (`only_once_per_turn: true`).
        let watcher = make_card(
            "synth_if_watch",
            "Synth IfWatch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter::default(),
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: true,
                    do_this_once_per_turn: false,
                    is_state_trigger: false,
                    or: vec![],
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                },
                condition: Some(Box::new(Condition::ControlsFilter {
                    filter: Box::new(ObjectFilter {
                        zones: ZoneMask::BATTLEFIELD,
                        types_any: CardTypeMask::ARTIFACT,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    }),
                })),
                grant_condition: None,
                targets: vec![],
                effect: vec![gain_life_step(5)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let relic = make_card(
            "synth_relic_3",
            "Synth Relic",
            CardTypeMask::ARTIFACT,
            vec![],
        );
        let db = build_trig_db(vec![watcher, relic]);
        let mut state = setup_2p(db);
        let _w = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // 1) No artifact -> intervening-if false -> does NOT queue and does NOT consume once_per_turn!
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        flush_triggers(&mut state);
        assert!(state.stack().is_empty());
        assert!(state.triggered_once_this_turn().is_empty());

        // 2) Artifact enters -> triggers and queues; if artifact is removed before resolution,
        //    `trigger_intervening_if_holds_on_resolution` returns `false` (CR 603.4).
        let art = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        let sid = state.stack_entries()[0].id;
        assert!(trigger_intervening_if_holds_on_resolution(&state, sid));

        move_object_raw(
            &mut state,
            art,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        assert!(!trigger_intervening_if_holds_on_resolution(&state, sid));
    }

    #[test]
    fn suite4_zero_legal_targets_removed_before_stack_cr603_3d() {
        let etb_target = make_card(
            "synth_etb_tgt",
            "Synth TargetETB",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::ALL,
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
                targets: vec![TargetSpec {
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
                }],
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![etb_target]);
        let mut state = setup_2p(db);

        let ts = state.allocate_timestamp();
        let hand_id = state.insert_object(
            GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(hand_id);
        let raw = move_object_raw(
            &mut state,
            hand_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        emit_zone_change(
            &mut state,
            raw.old_id,
            raw.new_id,
            Zone::Hand,
            Zone::Battlefield,
            Some(PlayerId(0)),
            Some(PlayerId(0)),
            raw.lki.as_ref(),
            false,
        );
        flush_triggers(&mut state);
        // Opponent has 0 creatures -> CR 603.3d removes trigger before it sits on stack!
        assert!(state.stack().is_empty());
    }

    #[test]
    fn suite5_etb_post_layer_types_cr603_6a_sees_layer4_artifact() {
        let art_watch = make_card(
            "synth_art_etb",
            "Synth ArtWatch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::ARTIFACT,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        }),
                        detail: Some(EventDetail::ZoneChange {
                            from: ZoneMask::ALL,
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let vanilla_crea = make_card("synth_bear_5", "Synth Bear", CardTypeMask::CREATURE, vec![]);
        let db = build_trig_db(vec![art_watch, vanilla_crea]);
        let mut state = setup_2p(db);
        let _w = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // Layer 4 continuous effect turns all permanents into Artifacts.
        let ce_ts = state.allocate_timestamp();
        state.add_continuous_effect(TimedContinuousEffect {
            id: 1,
            source: None,
            controller: PlayerId(0),
            layer: Layer::Layer4,
            affected: mtg_engine_continuous_all(),
            modifier: LayerMod::AddTypes {
                types: CardTypeMask::ARTIFACT,
                subtypes: SmallVec::new(),
            },
            duration: Duration::Permanent,
            timestamp: ce_ts,
            expires_turn: None,
            expires_on_player: None,
            chosen_color: None,
            chosen_subtype: None,
            chosen_player: None,
            chosen_card_type: None,
            locked_pt: None,
            is_cda: false,
        });

        let ts = state.allocate_timestamp();
        let bear_h = state.insert_object(
            GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Hand,
                ts,
                ObjectFlags::empty(),
            ),
            None,
        );
        state.player_mut(PlayerId(0)).unwrap().hand.push(bear_h);
        let raw = move_object_raw(
            &mut state,
            bear_h,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        emit_zone_change(
            &mut state,
            raw.old_id,
            raw.new_id,
            Zone::Hand,
            Zone::Battlefield,
            Some(PlayerId(0)),
            Some(PlayerId(0)),
            raw.lki.as_ref(),
            false,
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack().len(), 1);
    }

    fn mtg_engine_continuous_all() -> crate::layers::predicates::ContinuousTarget {
        crate::layers::predicates::ContinuousTarget::Filter(ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            ..Default::default()
        })
    }

    #[test]
    fn suite6_state_triggers_fire_once_with_anti_loop_guard_cr603_8() {
        let state_ench = make_card(
            "synth_state_6",
            "Synth StateEnch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::StateTriggered,
                    filter: EventFilter {
                        detail: Some(EventDetail::State {
                            condition: Box::new(Condition::Not {
                                of: Box::new(Condition::ControlsFilter {
                                    filter: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::CREATURE,
                                        controller: Some(RelativeControl::You),
                                        ..Default::default()
                                    }),
                                }),
                            }),
                        }),
                        ..Default::default()
                    },
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: false,
                    do_this_once_per_turn: false,
                    is_state_trigger: true,
                    or: vec![],
                    chapter_numbers: SmallVec::new(),
                    is_chapter_ability: false,
                    spell_delayed: None,
                },
                condition: None,
                grant_condition: None,
                targets: vec![],
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![state_ench]);
        let mut state = setup_2p(db);
        let _s = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

        assert_eq!(check_state_triggers(&mut state), 1);
        // Second call while still pending must queue 0!
        assert_eq!(check_state_triggers(&mut state), 0);
        flush_triggers(&mut state);
        assert_eq!(state.stack().len(), 1);
        // Calling while already on stack must queue 0!
        assert_eq!(check_state_triggers(&mut state), 0);

        // Remove from stack (simulating resolution) -> condition still holds -> re-triggers!
        let popped = state.stack_entries_mut().pop().unwrap();
        state.stack_mut().pop();
        state.remove_stack_trigger_meta(popped.id);
        assert_eq!(check_state_triggers(&mut state), 1);
    }

    #[test]
    fn suite7_once_per_turn_multiplier_suppression_and_blink_reset_cr603_2i_400_7() {
        let once_card = make_card(
            "synth_once_7",
            "Synth OncePerTurn",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::DrawCard,
                    filter: EventFilter::default(),
                    scope: None,
                    from_zones: None,
                    only_once_per_turn: true,
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let mult_card = make_card(
            "synth_pan_7",
            "Synth Harmonicon",
            CardTypeMask::ARTIFACT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: MultiplierTarget::Triggers,
                        factor: 2.0,
                        delta: 0,
                        filter: None,
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![once_card, mult_card]);
        let mut state = setup_2p(db);
        let once_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let _mult_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(2));

        let card_obj = state.players()[0].library[0];
        // Draw twice: despite Panharmonicon multiplier, `only_once_per_turn` fires strictly ONCE!
        emit_event(
            &mut state,
            AtomicEventKind::DrawCard {
                player: PlayerId(0),
                object: card_obj,
                in_own_draw_step_ordinal: None,
            },
        );
        emit_event(
            &mut state,
            AtomicEventKind::DrawCard {
                player: PlayerId(0),
                object: card_obj,
                in_own_draw_step_ordinal: None,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack().len(), 1);

        // Blink `once_bf` (Exile -> Battlefield bumps generation CR 400.7) -> can trigger again!
        let ex_id = move_object_raw(
            &mut state,
            once_bf,
            Zone::Exile,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let blinked_bf = move_object_raw(
            &mut state,
            ex_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        assert_ne!(blinked_bf.generation, once_bf.generation);

        emit_event(
            &mut state,
            AtomicEventKind::DrawCard {
                player: PlayerId(0),
                object: card_obj,
                in_own_draw_step_ordinal: None,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack().len(), 2);
    }

    #[test]
    fn suite8_delayed_one_shot_and_reflexive_no_multiplier_cr603_7_12() {
        let mult_card = make_card(
            "synth_pan_8",
            "Synth Harmonicon",
            CardTypeMask::ARTIFACT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: MultiplierTarget::Triggers,
                        factor: 2.0,
                        delta: 0,
                        filter: None,
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![mult_card]);
        let mut state = setup_2p(db);
        let mult_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // 1) OneShot delayed trigger on EndStep expires after firing once (t2_f10_05).
        register_delayed_trigger(
            &mut state,
            DelayedTriggerEntry {
                id: 0,
                controller: PlayerId(0),
                source_card: CardDefId(0),
                source_object: None,
                source_lki: None,
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter {
                        detail: Some(EventDetail::Phase {
                            phase: Some(GamePhase::EndStep),
                            any_main_phase: false,
                            enchanted_controller_upkeep: false,
                            enchanted_player_upkeep: false,
                            enchanted_opponent_end_step: false,
                            phase_chosen_player: false,
                            turn_filter: None,
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
                targets: vec![],
                effect: vec![gain_life_step(3)],
                expiry: DelayedExpiry::OneShot,
                watched_objects: SmallVec::new(),
                x_value: 0,
            },
        );
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::EndStep,
                active_player: PlayerId(0),
            },
        );
        assert!(state.delayed_triggers().is_empty());
        // Second EndStep emits 0 delayed triggers.
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::EndStep,
                active_player: PlayerId(1),
            },
        );
        flush_triggers(&mut state);
        // Note: mult_bf doubled the first EndStep delayed trigger, or 2 stack entries from first fire only!
        let count_after_delayed = state.stack().len();
        assert!(count_after_delayed >= 1);
        state.stack_mut().clear();
        state.stack_entries_mut().clear();

        // 2) Reflexive trigger flushes in APNAP order WITHOUT trigger multiplication (CR 603.2d / 603.12).
        let lki = Arc::new(crate::events::snapshot_for_live_object(&state, mult_bf).unwrap());
        queue_reflexive_trigger(
            &mut state,
            PendingReflexiveTrigger {
                id: 0,
                controller: PlayerId(0),
                source: mult_bf,
                source_lki: Arc::clone(&lki),
                spec: ReflexiveTriggerSpec {
                    targets: vec![],
                    effect: vec![gain_life_step(3)],
                    modal: None,
                    text: None,
                    event_filter: None,
                    object_filter: None,
                },
                event_object: None,
                event_lki: None,
                event_player: None,
                event_amount: None,
                bindings: vec![],
                x_value: 0,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack().len(), 1);
    }

    #[test]
    fn suite9_triggered_mana_abilities_inline_and_tap_multiplier_cr605_1b_106_12() {
        let mana_ench = make_card(
            "synth_mana_9",
            "Synth ManaEnch",
            CardTypeMask::ENCHANTMENT,
            vec![
                AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::TappedForMana,
                        filter: EventFilter::default(),
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
                    effect: vec![EffectStep {
                        op: EffectOp::AddMana {
                            production: ManaProduction::FromProducedEvent,
                            restriction: None,
                            restriction_spec: None,
                            retain_until_eot: false,
                            delayed_rider: None,
                        },
                        subject: Selector::PlayerRef {
                            player: PlayerRef::You,
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
                    text: None,
                    span: Default::default(),
                }),
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::TapForManaMultiplier {
                            factor: 3,
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::LAND,
                                ..Default::default()
                            }),
                            all_players: false,
                        },
                        condition: None,
                    },
                    span: Default::default(),
                }),
            ],
        );
        let db = build_trig_db(vec![mana_ench]);
        let mut state = setup_2p(db);
        let _me = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let land_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(0));

        assert_eq!(tap_for_mana_multiplier(&state, PlayerId(0), land_bf), 3);

        emit_event(
            &mut state,
            AtomicEventKind::TappedForMana {
                player: PlayerId(0),
                source: land_bf,
                produced_colors: smallvec::smallvec![ManaColor::G],
                amount: 1,
            },
        );
        flush_inline_triggers(&mut state);
        // Stack stays empty (CR 605.4a) and 1 Green mana was added directly to pool!
        assert!(state.stack().is_empty());
        assert_eq!(state.players()[0].mana_pool[4], 1);

        // Depth limit guard (`MAX_INLINE_MANA_DEPTH`):
        state.set_inline_mana_depth(MAX_INLINE_MANA_DEPTH);
        emit_event(
            &mut state,
            AtomicEventKind::ManaAddedByLandAbility {
                player: PlayerId(0),
                source: land_bf,
                produced_colors: smallvec::smallvec![ManaColor::G],
                amount: 1,
            },
        );
        flush_inline_triggers(&mut state);
        // Not executed inline at depth >= MAX_INLINE_MANA_DEPTH!
        assert_eq!(state.players()[0].mana_pool[4], 1);
    }

    #[test]
    fn suite10_snapshot_round_trip_and_trigger_order_resume() {
        let trig_a = make_card(
            "synth_ord_a",
            "Synth OrderA",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter::default(),
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let trig_b = make_card(
            "synth_ord_b",
            "Synth OrderB",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter::default(),
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![trig_a, trig_b]);
        let mut state = setup_2p(Arc::clone(&db));
        let obj_a = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let obj_b = add_to_bf(&mut state, PlayerId(0), CardDefId(2));

        register_delayed_trigger(
            &mut state,
            DelayedTriggerEntry {
                id: 0,
                controller: PlayerId(0),
                source_card: CardDefId(1),
                source_object: Some(obj_a),
                source_lki: None,
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter::default(),
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
                targets: vec![],
                effect: vec![gain_life_step(9)],
                expiry: DelayedExpiry::ThisTurn,
                watched_objects: SmallVec::new(),
                x_value: 0,
            },
        );

        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        // Serialize and restore before flushing (PendingTrigger + DelayedTrigger survive!).
        let snap = state.to_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let decoded = serde_json::from_str(&json).unwrap();
        let mut restored = GameState::from_snapshot(db, decoded);
        assert_eq!(restored.pending_triggers().len(), 3);
        assert_eq!(restored.delayed_triggers().len(), 1);

        // Mark once-per-turn and clear_turn_trigger_state expires `ThisTurn` delayed trigger.
        restored
            .triggered_once_this_turn_mut()
            .push(OncePerTurnKey {
                object: obj_a,
                zone: Zone::Battlefield,
                face_index: 0,
                ability_idx: 0,
                origin: TriggerAbilityOrigin::Innate { component_index: 0 },
                is_do_this_once: false,
            });
        clear_turn_trigger_state(&mut restored);
        assert!(restored.triggered_once_this_turn().is_empty());
        assert!(restored.delayed_triggers().is_empty());

        // Distinct simultaneous triggers (obj_a + obj_b) prompt OrderObjects, and resume_trigger_order
        // places `obj_b` on top (last in stack Vec) when picked first!
        restored.pending_triggers_mut().clear();
        emit_event(
            &mut restored,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        flush_triggers(&mut restored);
        let DecisionState::Prompt {
            request: ChoiceRequest::OrderObjects { prompt_id, objects },
        } = restored.decision().clone()
        else {
            panic!("expected OrderObjects prompt for distinct simultaneous triggers");
        };
        assert_eq!(objects.len(), 2);
        resume_trigger_order(
            &mut restored,
            PlayerId(0),
            prompt_id,
            ChoiceAnswer::OrderNext { id: obj_b },
        )
        .unwrap();
        assert_eq!(restored.decision(), &DecisionState::Priority);
        assert_eq!(restored.stack().last().copied(), Some(obj_b));
    }

    #[test]
    fn trigger_multiplier_additive_and_suppression_cr603_2d() {
        let pan = make_card(
            "synth_pan_mult",
            "Synth Pan",
            CardTypeMask::ARTIFACT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: MultiplierTarget::Triggers,
                        factor: 2.0,
                        delta: 0,
                        filter: None,
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let torpor = make_card(
            "synth_torpor_mult",
            "Synth Suppress",
            CardTypeMask::ARTIFACT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Multiplier {
                        what: MultiplierTarget::Triggers,
                        factor: 0.0,
                        delta: 0,
                        filter: None,
                        counter: None,
                        cause: None,
                        cause_or: vec![],
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![pan, torpor]);
        let mut state = setup_2p(db);
        let p1 = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let _p2 = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let ev = AtomicEventKind::PhaseStepEntered {
            phase: GamePhase::Upkeep,
            active_player: PlayerId(0),
        };
        // Additive stacking: 1 + (2-1) + (2-1) = 3 (NOT 4!)
        assert_eq!(trigger_multiplier(&state, p1, None, PlayerId(0), &ev), 3);
        // Adding factor <= 0 suppressor yields 0 triggers!
        let _t = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        assert_eq!(trigger_multiplier(&state, p1, None, PlayerId(0), &ev), 0);
    }

    fn stack_source_id(entry: &StackEntry) -> ObjectId {
        match entry.kind {
            StackEntryKind::Spell { object } => object,
            StackEntryKind::Activated { source, .. } | StackEntryKind::Triggered { source, .. } => {
                source
            }
        }
    }

    #[test]
    fn attached_to_ltb_single_and_simultaneous_batch_aura_plus_host() {
        let host = make_card(
            "synth_host_att",
            "Synth Host",
            CardTypeMask::CREATURE,
            vec![],
        );
        let death_aura = make_card(
            "synth_death_aura",
            "Synth DeathAura",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::AttachedTo),
                        attached_filter: Some(Box::new(ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        })),
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let self_or_host_aura = make_card(
            "synth_self_or_host_aura",
            "Synth SelfOrHostAura",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::AttachedTo),
                        or_attached_to_self: true,
                        attached_filter: Some(Box::new(ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        })),
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
                effect: vec![gain_life_step(3)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        // Case A: single host death while both Auras remain on BF (move_object_raw detaches them before emit_zone_change).
        {
            let db = build_trig_db(vec![
                host.clone(),
                death_aura.clone(),
                self_or_host_aura.clone(),
            ]);
            let mut state = setup_2p(db);
            let host_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let aura_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let self_host_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            state.object_mut(aura_bf).unwrap().attached_to = Some(host_bf);
            state.object_mut(self_host_bf).unwrap().attached_to = Some(host_bf);

            crate::zones::move_object(
                &mut state,
                host_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
        }

        // Case B: self death of SelfOrHostAura while host lives -> SelfOrHostAura triggers (attached_filter skipped for SelfObj),
        // while DeathAura does not trigger.
        {
            let db = build_trig_db(vec![
                host.clone(),
                death_aura.clone(),
                self_or_host_aura.clone(),
            ]);
            let mut state = setup_2p(db);
            let host_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let aura_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let self_host_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            state.object_mut(aura_bf).unwrap().attached_to = Some(host_bf);
            state.object_mut(self_host_bf).unwrap().attached_to = Some(host_bf);

            crate::zones::move_object(
                &mut state,
                self_host_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(stack_source_id(&state.stack_entries()[0]), self_host_bf);

            state.stack_entries_mut().clear();
            state.stack_mut().clear();
            crate::zones::move_object(
                &mut state,
                aura_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 0);
        }

        // Case C: simultaneous batch where Aura leaves BEFORE host (cand.is_look_back == true and pre_batch_lki fallback).
        {
            let db = build_trig_db(vec![host, death_aura]);
            let mut state = setup_2p(db);
            let host_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let aura_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            state.object_mut(aura_bf).unwrap().attached_to = Some(host_bf);

            with_simultaneous_batch(&mut state, &[aura_bf, host_bf], |st| {
                crate::zones::move_object(
                    st,
                    aura_bf,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap();
                crate::zones::move_object(
                    st,
                    host_bf,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap();
            });
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(stack_source_id(&state.stack_entries()[0]), aura_bf);
        }
    }

    #[test]
    fn your_first_upkeep_fires_turn1_and_suppresses_turn3() {
        let first_upkeep_card = make_card(
            "synth_first_upkeep",
            "Synth FirstUpkeep",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::PhaseBegin,
                    filter: EventFilter {
                        detail: Some(EventDetail::Phase {
                            phase: Some(GamePhase::Upkeep),
                            any_main_phase: false,
                            enchanted_controller_upkeep: false,
                            enchanted_player_upkeep: false,
                            enchanted_opponent_end_step: false,
                            phase_chosen_player: false,
                            turn_filter: Some(mtg_ir::PhaseTurnFilter::YourFirstUpkeep),
                        }),
                        ..Default::default()
                    },
                    scope: Some(TriggerScope::ControllerTurn),
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
                effect: vec![gain_life_step(5)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let db = build_trig_db(vec![first_upkeep_card]);
        let mut state = setup_2p(db);
        let _src = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

        // Turn 1 (P0 upkeep): fires!
        state.set_active_player(PlayerId(0));
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);

        // Turn 2 (P1 upkeep): controller turn filter rejects P1; P0's seen list unchanged.
        clear_turn_trigger_state(&mut state);
        state.set_active_player(PlayerId(1));
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(1),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);

        // Turn 3 (P0 second upkeep): suppressed by first_upkeep_seen!
        clear_turn_trigger_state(&mut state);
        state.set_active_player(PlayerId(0));
        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
    }

    #[test]
    fn damage_detail_runs_target_filter_and_pw_controller_without_early_return() {
        let pw_watcher = make_card(
            "synth_pw_watcher",
            "Synth PwWatcher",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::DealsDamage,
                    filter: EventFilter {
                        detail: Some(EventDetail::Damage {
                            combat: None,
                            noncombat_only: false,
                            damage_to: Some(mtg_ir::DamageRecipient::PlayerOrPlaneswalker),
                            damage_target_kind: None,
                            damage_player_relation: Some(RelativeControl::Opponent),
                            damaged_player: None,
                            damage_target_filter: Some(Box::new(ObjectFilter {
                                types_any: CardTypeMask::PLANESWALKER,
                                mana_value: Some(Compare {
                                    op: CompareOp::Ge,
                                    value: Box::new(Amount::Const { n: 4 }),
                                }),
                                ..Default::default()
                            })),
                            damage_target_enchanted_player: false,
                            single_source_damage_min: None,
                            combat_damage_batch_player: None,
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        let mut low_pw_face = base_face("Synth LowPw", CardTypeMask::PLANESWALKER, ColorMask::U);
        low_pw_face.characteristics.mana_cost = ManaCost::parse("{2}");
        let low_pw = CardDef {
            oracle_id: "synth_low_pw".into(),
            compiler_version: 46,
            text_hash: "0000".into(),
            name: "Synth LowPw".into(),
            layout: CardLayout::Normal,
            faces: vec![low_pw_face],
            fully_supported: true,
        };

        let mut high_pw_face = base_face("Synth HighPw", CardTypeMask::PLANESWALKER, ColorMask::U);
        high_pw_face.characteristics.mana_cost = ManaCost::parse("{5}");
        let high_pw = CardDef {
            oracle_id: "synth_high_pw".into(),
            compiler_version: 46,
            text_hash: "0000".into(),
            name: "Synth HighPw".into(),
            layout: CardLayout::Normal,
            faces: vec![high_pw_face],
            fully_supported: true,
        };

        let ench_player_watcher = make_card(
            "synth_ench_player_watcher",
            "Synth EnchPlayerWatcher",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::DealsDamage,
                    filter: EventFilter {
                        detail: Some(EventDetail::Damage {
                            combat: None,
                            noncombat_only: false,
                            damage_to: None,
                            damage_target_kind: None,
                            damage_player_relation: Some(RelativeControl::Opponent),
                            damaged_player: None,
                            damage_target_filter: None,
                            damage_target_enchanted_player: true,
                            single_source_damage_min: None,
                            combat_damage_batch_player: None,
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );

        let pinger = make_card(
            "synth_pinger",
            "Synth Pinger",
            CardTypeMask::CREATURE,
            vec![],
        );
        let db = build_trig_db(vec![
            pw_watcher,
            low_pw,
            high_pw,
            ench_player_watcher,
            pinger,
        ]);
        let mut state = setup_2p(db);

        let pw_watch = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let p1_low_pw = add_to_bf(&mut state, PlayerId(1), CardDefId(2));
        let p0_high_pw = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let p1_high_pw = add_to_bf(&mut state, PlayerId(1), CardDefId(3));
        let ench_watch = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
        state.rare_or_default(ench_watch).unwrap().chosen_player = Some(PlayerId(1));
        let src = add_to_bf(&mut state, PlayerId(0), CardDefId(5));

        // 1) Damage to P1's LowPw (MV 2): PwWatcher must NOT trigger because damage_target_filter requires MV >= 4.
        emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: Some(src),
                source_controller: PlayerId(0),
                target: TargetRef::Object { id: p1_low_pw },
                amount: 3,
                is_combat: false,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);

        // 2) Damage to P0's HighPw (MV 5, controller P0): PwWatcher must NOT trigger because relation is Opponent.
        emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: Some(src),
                source_controller: PlayerId(0),
                target: TargetRef::Object { id: p0_high_pw },
                amount: 3,
                is_combat: false,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);

        // 3) Damage to P1's HighPw (MV 5, controller P1): PwWatcher DOES trigger!
        emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: Some(src),
                source_controller: PlayerId(0),
                target: TargetRef::Object { id: p1_high_pw },
                amount: 3,
                is_combat: false,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), pw_watch);

        // 4) Damage to non-player creature (src): EnchPlayerWatcher must NOT trigger; damage to PlayerId(1): DOES trigger!
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: Some(src),
                source_controller: PlayerId(0),
                target: TargetRef::Object { id: src },
                amount: 1,
                is_combat: false,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);

        emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: Some(src),
                source_controller: PlayerId(0),
                target: TargetRef::Player { id: PlayerId(1) },
                amount: 1,
                is_combat: false,
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), ench_watch);
    }

    #[test]
    fn combat_attacked_target_kinds_unblocked_and_turn_ordinal_all_kinds() {
        let make_atk_watcher = |id: &str, atf: mtg_ir::AttackedTargetFilter| {
            make_card(
                id,
                id,
                CardTypeMask::ENCHANTMENT,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::Attacks,
                        filter: EventFilter {
                            detail: Some(EventDetail::Combat {
                                alone: false,
                                saddled: false,
                                min_attackers: None,
                                dethrone_most_life: false,
                                training_greater_attacker: false,
                                blocked_attacker_filter: None,
                                host: None,
                                without_flanking: false,
                                attacked_target: Some(atf),
                                co_attacker_filter: None,
                                co_attacker_min: None,
                                per_attacked_player: false,
                                attacking_with_filter: None,
                                min_attacking_with_count: None,
                                attacking_total_power_min: None,
                                attack_enchanted_or_attacked_by_enchanted: false,
                                attacking_unblocked: false,
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
                    effect: vec![gain_life_step(1)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
            )
        };

        let w_pw = make_atk_watcher("synth_atk_pw", mtg_ir::AttackedTargetFilter::Planeswalker);
        let w_battle = make_atk_watcher("synth_atk_battle", mtg_ir::AttackedTargetFilter::Battle);
        let w_player =
            make_atk_watcher("synth_atk_player", mtg_ir::AttackedTargetFilter::AnyPlayer);
        let w_unblocked = make_card(
            "synth_atk_unblocked",
            "Synth Unblocked",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::AttackedAndUnblocked,
                    filter: EventFilter {
                        object: Some(TriggerSubject::SelfObj),
                        detail: Some(EventDetail::Combat {
                            alone: false,
                            saddled: false,
                            min_attackers: None,
                            dethrone_most_life: false,
                            training_greater_attacker: false,
                            blocked_attacker_filter: None,
                            host: None,
                            without_flanking: false,
                            attacked_target: None,
                            co_attacker_filter: None,
                            co_attacker_min: None,
                            per_attacked_player: false,
                            attacking_with_filter: None,
                            min_attacking_with_count: None,
                            attacking_total_power_min: None,
                            attack_enchanted_or_attacked_by_enchanted: false,
                            attacking_unblocked: true,
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let w_tap_first = make_card(
            "synth_tap_first",
            "Synth TapFirst",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::BecomesTapped,
                    filter: EventFilter {
                        first_time_each_turn: true,
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let w_tap_second = make_card(
            "synth_tap_second",
            "Synth TapSecond",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::BecomesTapped,
                    filter: EventFilter {
                        nth_this_turn: Some(2),
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
                effect: vec![gain_life_step(2)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let w_die_first = make_card(
            "synth_die_first",
            "Synth DieFirst",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        first_time_each_turn: true,
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let target_pw = make_card(
            "synth_tgt_pw",
            "Synth TgtPw",
            CardTypeMask::PLANESWALKER,
            vec![],
        );
        let target_battle = make_card(
            "synth_tgt_battle",
            "Synth TgtBattle",
            CardTypeMask::BATTLE,
            vec![],
        );

        let db = build_trig_db(vec![
            w_pw,
            w_battle,
            w_player,
            w_unblocked,
            w_tap_first,
            w_tap_second,
            w_die_first,
            target_pw,
            target_battle,
        ]);
        let mut state = setup_2p(db);

        let watch_pw = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let watch_battle = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let watch_player = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let att_cre = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
        let tap_1_id = add_to_bf(&mut state, PlayerId(0), CardDefId(5));
        let tap_2_id = add_to_bf(&mut state, PlayerId(0), CardDefId(6));
        let die_1_id = add_to_bf(&mut state, PlayerId(0), CardDefId(7));
        let pw_bf = add_to_bf(&mut state, PlayerId(1), CardDefId(8));
        let battle_bf = add_to_bf(&mut state, PlayerId(1), CardDefId(9));

        // 1) Attack PlayerId(1) -> only watch_player matches (NOT watch_pw or watch_battle).
        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(0),
                attackers: vec![crate::state::AttackerEntry {
                    attacker: att_cre,
                    target: AttackTarget::Player { id: PlayerId(1) },
                }],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), watch_player);

        // 2) Attack Planeswalker -> only watch_pw matches.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(0),
                attackers: vec![crate::state::AttackerEntry {
                    attacker: att_cre,
                    target: AttackTarget::Planeswalker { id: pw_bf },
                }],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), watch_pw);

        // 3) Attack Battle -> only watch_battle matches.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(0),
                attackers: vec![crate::state::AttackerEntry {
                    attacker: att_cre,
                    target: AttackTarget::Battle { id: battle_bf },
                }],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), watch_battle);

        // 4) UnblockedAttackers triggers att_cre with attacking_unblocked: true.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::UnblockedAttackers {
                defender: PlayerId(1),
                attackers: vec![att_cre],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), att_cre);

        // 5) BecomesTapped ordinal: 1st tap triggers only tap_1_id; 2nd tap triggers only tap_2_id.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::TapStateChanged {
                object: att_cre,
                now_tapped: true,
                actor: Some(PlayerId(0)),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), tap_1_id);

        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::TapStateChanged {
                object: att_cre,
                now_tapped: true,
                actor: Some(PlayerId(0)),
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), tap_2_id);

        // 6) ZoneChange ordinal: 1st death triggers die_1_id; 2nd death does not.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        crate::zones::move_object(
            &mut state,
            pw_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), die_1_id);

        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        crate::zones::move_object(
            &mut state,
            battle_bf,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);
    }

    #[test]
    fn unbatched_single_object_bf_leave_preserves_layer6_granted_ltb_trigger() {
        use mtg_ir::{
            AbilityGrantSource, EventPattern, ReplacementAbility, ReplacementDef, ReplacementWith,
            ZoneChangeMod,
        };

        let self_dies_trigger = TriggeredAbility {
            trigger: TriggerDef {
                event: EventKind::ZoneChange,
                filter: EventFilter {
                    object: Some(TriggerSubject::SelfObj),
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
            effect: vec![gain_life_step(2)],
            modal: None,
            text: None,
            span: Default::default(),
        };

        let self_ltb_trigger = TriggeredAbility {
            trigger: TriggerDef {
                event: EventKind::ZoneChange,
                filter: EventFilter {
                    object: Some(TriggerSubject::SelfObj),
                    detail: Some(EventDetail::ZoneChange {
                        from: ZoneMask::BATTLEFIELD,
                        to: None,
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
            effect: vec![gain_life_step(3)],
            modal: None,
            text: None,
            span: Default::default(),
        };

        // CardDefId(1): Vanilla creature with NO printed dies trigger.
        let vanilla = make_card(
            "synth_vanilla",
            "Synth Vanilla",
            CardTypeMask::CREATURE,
            vec![],
        );

        // CardDefId(2): Enchantment granting Inline self-dies trigger to all creatures.
        let inline_granter = make_card(
            "synth_inline_granter",
            "Synth InlineGranter",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(AbilityDef::Triggered(self_dies_trigger.clone())),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        // CardDefId(3): Creature with innate self-dies trigger (donor for CardAbility grant).
        let donor_creature = make_card(
            "synth_donor_dies",
            "Synth DonorDies",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(self_dies_trigger)],
        );

        // CardDefId(4): Creature that copies abilities from other BF creatures (CardAbility grant).
        let card_ability_copier = make_card(
            "synth_copier_creature",
            "Synth CopierCreature",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
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
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        // CardDefId(5): Layer-6 RemoveAllAbilities humility-style enchantment.
        let ability_remover = make_card(
            "synth_remove_all",
            "Synth RemoveAll",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
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
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        // CardDefId(6): Enchantment granting Inline generic LTB trigger ("leaves the battlefield").
        let ltb_granter = make_card(
            "synth_ltb_granter",
            "Synth LtbGranter",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(AbilityDef::Triggered(self_ltb_trigger)),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );

        // CardDefId(7) & CardDefId(8): Two competing BF->GY replacement effects to force ChooseReplacement pause.
        let make_exile_rep = |id: &str, name: &str| {
            make_card(
                id,
                name,
                CardTypeMask::ENCHANTMENT,
                vec![AbilityDef::Replacement(ReplacementAbility {
                    replacement: ReplacementDef {
                        replaces: EventPattern {
                            event: EventKind::ZoneChange,
                            filter: EventFilter {
                                object: Some(TriggerSubject::Filter {
                                    filter: Box::new(ObjectFilter {
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                }),
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
            )
        };
        let rep_a = make_exile_rep("synth_exile_rep_a", "Synth ExileRepA");
        let rep_b = make_exile_rep("synth_exile_rep_b", "Synth ExileRepB");

        let db = build_trig_db(vec![
            vanilla,
            inline_granter,
            donor_creature,
            card_ability_copier,
            ability_remover,
            ltb_granter,
            rep_a,
            rep_b,
        ]);

        // 1) Unbatched single-object BF -> Graveyard preserves Inline Layer-6 granted dies trigger.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _granter = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let creature_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

            crate::zones::move_object(
                &mut state,
                creature_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(stack_source_id(&state.stack_entries()[0]), creature_bf);
        }

        // 2) Unbatched single-object BF -> Graveyard preserves CardAbility Layer-6 granted dies trigger.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _donor = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let copier_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(4));

            crate::zones::move_object(
                &mut state,
                copier_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(stack_source_id(&state.stack_entries()[0]), copier_bf);
        }

        // 3) Suppressed when lost_all_abilities is true (newer timestamp RemoveAllAbilities).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _granter = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let creature_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let _remover = add_to_bf(&mut state, PlayerId(0), CardDefId(5));

            crate::zones::move_object(
                &mut state,
                creature_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 0);
        }

        // 4) Suppressed when object is phased out.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _granter = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let creature_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            state
                .object_mut(creature_bf)
                .unwrap()
                .flags
                .insert(ObjectFlags::PHASED_OUT);
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            crate::zones::move_object(
                &mut state,
                creature_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 0);
        }

        // 5) Deduplication: inside with_simultaneous_batch, granted trigger fires exactly ONCE (not twice).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _granter = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let creature_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

            with_simultaneous_batch(&mut state, &[creature_bf], |st| {
                crate::zones::move_object(
                    st,
                    creature_bf,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap();
            });
            assert_eq!(state.stack_entries().len(), 1);
            assert_eq!(stack_source_id(&state.stack_entries()[0]), creature_bf);
        }

        // 6) ChooseReplacement pause -> GameStateSnapshot round-trip -> resume_and_finalize_replacement
        //    still preserves Layer-6 granted LTB trigger onto the stack.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _ltb_granter = add_to_bf(&mut state, PlayerId(0), CardDefId(6));
            let _rep_a = add_to_bf(&mut state, PlayerId(0), CardDefId(7));
            let _rep_b = add_to_bf(&mut state, PlayerId(0), CardDefId(8));
            let creature_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

            let crate::zones::MoveObjectOutcome::Paused { prompt_id, chooser } =
                crate::zones::move_object(
                    &mut state,
                    creature_bf,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap()
            else {
                panic!("expected ChooseReplacement pause");
            };
            assert!(state.stack_entries().is_empty());

            let json = serde_json::to_string(&state.to_snapshot()).unwrap();
            let snap: crate::state::GameStateSnapshot = serde_json::from_str(&json).unwrap();
            let mut resumed_state = GameState::from_snapshot(Arc::clone(&db), snap);

            let crate::replace::ReplacementResumeOutcome::ZoneMove(
                crate::zones::MoveObjectOutcome::Moved(raw),
            ) = crate::replace::resume_and_finalize_replacement(
                &mut resumed_state,
                chooser,
                prompt_id,
                ChoiceAnswer::PickReplacement { index: 0 },
            )
            .unwrap()
            else {
                panic!("expected resumed ZoneMove Moved");
            };
            assert_eq!(raw.to_zone, Zone::Exile);
            flush_triggers(&mut resumed_state);
            assert_eq!(resumed_state.stack_entries().len(), 1);
            assert_eq!(
                stack_source_id(&resumed_state.stack_entries()[0]),
                creature_bf
            );
        }
    }

    #[test]
    fn attacked_target_filter_enchanted_player_matches_chosen_player() {
        let watcher_card = make_card(
            "synth_atk_enchanted_player",
            "Synth AtkEnchantedPlayer",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::Attacks,
                    filter: EventFilter {
                        detail: Some(EventDetail::Combat {
                            alone: false,
                            saddled: false,
                            min_attackers: None,
                            dethrone_most_life: false,
                            training_greater_attacker: false,
                            blocked_attacker_filter: None,
                            host: None,
                            without_flanking: false,
                            attacked_target: Some(mtg_ir::AttackedTargetFilter::EnchantedPlayer),
                            co_attacker_filter: None,
                            co_attacker_min: None,
                            per_attacked_player: false,
                            attacking_with_filter: None,
                            min_attacking_with_count: None,
                            attacking_total_power_min: None,
                            attack_enchanted_or_attacked_by_enchanted: false,
                            attacking_unblocked: false,
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
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let attacker_card = make_card(
            "synth_atk_cre",
            "Synth Attacker",
            CardTypeMask::CREATURE,
            vec![],
        );
        let pw_card = make_card(
            "synth_def_pw",
            "Synth DefPw",
            CardTypeMask::PLANESWALKER,
            vec![],
        );

        let db = build_trig_db(vec![watcher_card, attacker_card, pw_card]);
        let mut state = setup_2p(db);

        let curse_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let att_bf = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        let pw_bf = add_to_bf(&mut state, PlayerId(1), CardDefId(3));

        // 1) Without chosen_player set -> negative assertion (does not match PlayerId(1)).
        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(0),
                attackers: vec![crate::state::AttackerEntry {
                    attacker: att_bf,
                    target: AttackTarget::Player { id: PlayerId(1) },
                }],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);

        // 2) Set chosen_player = Some(PlayerId(1)) via rare_or_default -> positive assertion.
        state.rare_or_default(curse_bf).unwrap().chosen_player = Some(PlayerId(1));

        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(0),
                attackers: vec![crate::state::AttackerEntry {
                    attacker: att_bf,
                    target: AttackTarget::Player { id: PlayerId(1) },
                }],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 1);
        assert_eq!(stack_source_id(&state.stack_entries()[0]), curse_bf);

        // 3) Negative assertions: attacking PlayerId(0) or Planeswalker does NOT match.
        state.stack_entries_mut().clear();
        state.stack_mut().clear();
        emit_event(
            &mut state,
            AtomicEventKind::AttackDeclared {
                player: PlayerId(1),
                attackers: vec![
                    crate::state::AttackerEntry {
                        attacker: att_bf,
                        target: AttackTarget::Player { id: PlayerId(0) },
                    },
                    crate::state::AttackerEntry {
                        attacker: att_bf,
                        target: AttackTarget::Planeswalker { id: pw_bf },
                    },
                ],
            },
        );
        flush_triggers(&mut state);
        assert_eq!(state.stack_entries().len(), 0);
    }

    #[test]
    fn two_distinct_inline_granted_ltb_triggers_do_not_collide_on_once_or_batch_keys() {
        let make_inline_dies_granter =
            |id: &str, name: &str, one_or_more: bool, only_once_per_turn: bool, life: i32| {
                let trig = TriggeredAbility {
                    trigger: TriggerDef {
                        event: EventKind::ZoneChange,
                        filter: EventFilter {
                            object: Some(TriggerSubject::SelfObj),
                            one_or_more,
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
                        scope: None,
                        from_zones: None,
                        only_once_per_turn,
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
                    effect: vec![gain_life_step(life)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                };
                make_card(
                    id,
                    name,
                    CardTypeMask::ENCHANTMENT,
                    vec![AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer6,
                                affected: Affected::Filter {
                                    filter: Box::new(ObjectFilter {
                                        zones: ZoneMask::BATTLEFIELD,
                                        types_any: CardTypeMask::CREATURE,
                                        ..Default::default()
                                    }),
                                },
                                modifier: LayerMod::GrantAbility {
                                    ability: Box::new(AbilityDef::Triggered(trig)),
                                },
                                active_in_zone: None,
                            },
                            condition: None,
                        },
                        span: Default::default(),
                    })],
                )
            };

        let vanilla = make_card(
            "synth_vanilla_two_inl",
            "Synth Vanilla",
            CardTypeMask::CREATURE,
            vec![],
        );
        let once_a = make_inline_dies_granter("synth_once_a", "Synth OnceA", false, true, 1);
        let once_b = make_inline_dies_granter("synth_once_b", "Synth OnceB", false, true, 2);
        let batch_a = make_inline_dies_granter("synth_batch_a", "Synth BatchA", true, false, 3);
        let batch_b = make_inline_dies_granter("synth_batch_b", "Synth BatchB", true, false, 4);

        let db = build_trig_db(vec![vanilla, once_a, once_b, batch_a, batch_b]);

        // 1) Unbatched BF -> GY with two distinct Inline `only_once_per_turn` triggers -> both fire.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _ga = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let _gb = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let cre = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

            crate::zones::move_object(
                &mut state,
                cre,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
            assert_eq!(state.triggered_once_this_turn().len(), 2);
        }

        // 2) Batched BF -> GY with two distinct Inline `one_or_more` triggers -> both fire.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _ga = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let _gb = add_to_bf(&mut state, PlayerId(0), CardDefId(5));
            let cre = add_to_bf(&mut state, PlayerId(0), CardDefId(1));

            with_simultaneous_batch(&mut state, &[cre], |st| {
                crate::zones::move_object(
                    st,
                    cre,
                    Zone::Graveyard,
                    MoveRawOptions::default(),
                    None,
                )
                .unwrap();
                assert_eq!(st.batch_triggered_keys().len(), 2);
                assert_ne!(st.batch_triggered_keys()[0], st.batch_triggered_keys()[1]);
            });
            assert_eq!(state.stack_entries().len(), 2);
            assert!(state.batch_triggered_keys().is_empty());
        }
    }

    #[test]
    fn hyp1_inline_once_per_turn_survives_preceding_inline_activated_and_back_face_lookback() {
        use mtg_ir::{ActivatedAbility, CostDef, Timing};

        let make_dies_trig = |only_once_per_turn: bool, life: i32| TriggeredAbility {
            trigger: TriggerDef {
                event: EventKind::ZoneChange,
                filter: EventFilter {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(ObjectFilter {
                            zones: ZoneMask::BATTLEFIELD,
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    }),
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
                scope: None,
                from_zones: None,
                only_once_per_turn,
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
            effect: vec![gain_life_step(life)],
            modal: None,
            text: None,
            span: Default::default(),
        };

        let host_filter = ObjectFilter {
            zones: ZoneMask::BATTLEFIELD,
            types_any: CardTypeMask::CREATURE,
            controller: Some(RelativeControl::You),
            ..Default::default()
        };

        let act_ability = AbilityDef::Activated(ActivatedAbility {
            id: "synth_granted_act".into(),
            cost: vec![CostDef::TapSelf],
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
        });

        // CardDefId(1): Vanilla host creature.
        let host_card = make_card(
            "synth_hyp1_host",
            "Synth Host",
            CardTypeMask::CREATURE,
            vec![],
        );
        // CardDefId(2): Vanilla fodder creature (controlled by PlayerId(1)).
        let fodder_card = make_card(
            "synth_hyp1_fodder",
            "Synth Fodder",
            CardTypeMask::CREATURE,
            vec![],
        );
        // CardDefId(3): Enchantment granting Inline(Activated) at [0].
        let granter_act = make_card(
            "synth_hyp1_granter_act",
            "Synth GranterAct",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(host_filter.clone()),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(act_ability),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // CardDefId(4): Enchantment granting Inline(Triggered(only_once_per_turn: true)) at [1].
        let granter_trig = make_card(
            "synth_hyp1_granter_trig",
            "Synth GranterTrig",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::Filter {
                            filter: Box::new(host_filter),
                        },
                        modifier: LayerMod::GrantAbility {
                            ability: Box::new(AbilityDef::Triggered(make_dies_trig(true, 1))),
                        },
                        active_in_zone: None,
                    },
                    condition: None,
                },
                span: Default::default(),
            })],
        );
        // CardDefId(5): Transform DFC host (back face active_face_index = 1).
        let mut dfc_host = make_card(
            "synth_hyp1_dfc",
            "Synth DfcFront",
            CardTypeMask::CREATURE,
            vec![],
        );
        dfc_host.layout = CardLayout::Transform;
        dfc_host.faces.push(base_face(
            "Synth DfcBack",
            CardTypeMask::CREATURE,
            ColorMask::G,
        ));

        let db = build_trig_db(vec![
            host_card,
            fodder_card,
            granter_act,
            granter_trig,
            dfc_host,
        ]);

        // Case A: Normal host with preceding Inline(Activated) and Inline(Triggered).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _ga = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let _gt = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let fodder = add_to_bf(&mut state, PlayerId(1), CardDefId(2));

            crate::zones::move_object(
                &mut state,
                fodder,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);

            // Same turn: host itself dies -> lookback must match OncePerTurnKey and NOT double-fire.
            crate::zones::move_object(
                &mut state,
                host,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
        }

        // Case B: Transformed DFC host (active_face_index = 1) -> live face_index must match lookback.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _gt = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let dfc = add_to_bf(&mut state, PlayerId(0), CardDefId(5));
            state
                .object_mut(dfc)
                .unwrap()
                .flags
                .insert(ObjectFlags::TRANSFORMED);
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            let fodder = add_to_bf(&mut state, PlayerId(1), CardDefId(2));

            crate::zones::move_object(
                &mut state,
                fodder,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);

            crate::zones::move_object(
                &mut state,
                dfc,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
        }
    }

    #[test]
    fn hyp2_hyp3_hyp4_card_ability_and_innate_trigger_keys_do_not_collide() {
        use mtg_ir::AbilityGrantSource;

        let make_any_dies_trig =
            |only_once_per_turn: bool, one_or_more: bool, life: i32| TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        one_or_more,
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
                    scope: None,
                    from_zones: None,
                    only_once_per_turn,
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
                effect: vec![gain_life_step(life)],
                modal: None,
                text: None,
                span: Default::default(),
            };

        // CardDefId(1): Donor with `only_once_per_turn` at ab[0].
        let donor_once = make_card(
            "synth_hyp_donor_once",
            "Synth DonorOnce",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(true, false, 1))],
        );
        // CardDefId(2): Donor with `one_or_more` at ab[0].
        let donor_batch = make_card(
            "synth_hyp_donor_batch",
            "Synth DonorBatch",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(false, true, 2))],
        );
        // CardDefId(3): ExiledWithSelf copier (host).
        let exile_copier = make_card(
            "synth_hyp_exile_copier",
            "Synth ExileCopier",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer6,
                        affected: Affected::SelfObj,
                        modifier: LayerMod::GrantAbilitiesFromObjects {
                            from: AbilityGrantSource::ExiledWithSelf {
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
        // CardDefId(4): Host with innate `only_once_per_turn` at ab[0] AND ExiledWithSelf at ab[1].
        let innate_plus_copier = make_card(
            "synth_hyp4_host",
            "Synth InnateCopier",
            CardTypeMask::CREATURE,
            vec![
                AbilityDef::Triggered(make_any_dies_trig(true, false, 3)),
                AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::GrantAbilitiesFromObjects {
                                from: AbilityGrantSource::ExiledWithSelf {
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
                }),
            ],
        );
        // CardDefId(5): Vanilla fodder creature.
        let fodder_card = make_card(
            "synth_hyp_fodder",
            "Synth HypFodder",
            CardTypeMask::CREATURE,
            vec![],
        );

        let db = build_trig_db(vec![
            donor_once,
            donor_batch,
            exile_copier,
            innate_plus_copier,
            fodder_card,
        ]);

        let add_exiled_by = |state: &mut GameState, def: CardDefId, granter: ObjectId| {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                GameObject::new(def, PlayerId(0), Zone::Hand, ts, ObjectFlags::empty()),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(oid);
            let eid = move_object_raw(state, oid, Zone::Exile, MoveRawOptions::default(), None)
                .unwrap()
                .new_id
                .unwrap();
            state.rare_or_default(eid).unwrap().exiled_by = Some(granter);
            state.mark_dirty(DirtyMask::EXILE);
            eid
        };

        // Hyp2: 2 exiled `only_once_per_turn` donors at ab[0] -> both fire (2), and 2nd death fires 0 more.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let _d1 = add_exiled_by(&mut state, CardDefId(1), host);
            let _d2 = add_exiled_by(&mut state, CardDefId(1), host);
            let fodder1 = add_to_bf(&mut state, PlayerId(1), CardDefId(5));
            let fodder2 = add_to_bf(&mut state, PlayerId(1), CardDefId(5));

            crate::zones::move_object(
                &mut state,
                fodder1,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
            assert_eq!(state.triggered_once_this_turn().len(), 2);

            crate::zones::move_object(
                &mut state,
                fodder2,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
        }

        // Hyp3: 2 exiled `one_or_more` donors at ab[0] in a simultaneous batch -> both fire (2).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let _d1 = add_exiled_by(&mut state, CardDefId(2), host);
            let _d2 = add_exiled_by(&mut state, CardDefId(2), host);
            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(5));
            let f2 = add_to_bf(&mut state, PlayerId(1), CardDefId(5));

            with_simultaneous_batch(&mut state, &[f1, f2], |st| {
                crate::zones::move_object(st, f1, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                crate::zones::move_object(st, f2, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                assert_eq!(st.batch_triggered_keys().len(), 2);
                assert_ne!(st.batch_triggered_keys()[0], st.batch_triggered_keys()[1]);
            });
            assert_eq!(state.stack_entries().len(), 2);
        }

        // Hyp4: Innate ab[0] + CardAbility donor ab[0] `only_once_per_turn` -> both fire (2),
        // and CardAbility lookback on host's own death does not double-fire.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let _d1 = add_exiled_by(&mut state, CardDefId(1), host);
            let fodder = add_to_bf(&mut state, PlayerId(1), CardDefId(5));

            crate::zones::move_object(
                &mut state,
                fodder,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);

            // Snapshot round-trip preserves triggered_once_this_turn keys; host dying same turn
            // must NOT re-trigger either innate ab[0] or CardAbility donor ab[0].
            let json = serde_json::to_string(&state.to_snapshot()).unwrap();
            let snap: crate::state::GameStateSnapshot = serde_json::from_str(&json).unwrap();
            let mut restored = GameState::from_snapshot(Arc::clone(&db), snap);
            crate::zones::move_object(
                &mut restored,
                host,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut restored);
            assert_eq!(restored.stack_entries().len(), 2);
        }
    }

    #[test]
    fn mutate_innate_duplicate_components_once_per_turn_and_batch_do_not_collide() {
        let make_any_dies_trig =
            |only_once_per_turn: bool, one_or_more: bool, life: i32| TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        one_or_more,
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
                    scope: None,
                    from_zones: None,
                    only_once_per_turn,
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
                effect: vec![gain_life_step(life)],
                modal: None,
                text: None,
                span: Default::default(),
            };

        // CardDefId(1): Vanilla top component / fodder.
        let vanilla = make_card(
            "synth_mut_innate_top",
            "Synth MutTop",
            CardTypeMask::CREATURE,
            vec![],
        );
        // CardDefId(2): Mutate component with innate `only_once_per_turn` at ab[0].
        let mut_once = make_card(
            "synth_mut_innate_once",
            "Synth MutOnce",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(true, false, 1))],
        );
        // CardDefId(3): Mutate component with innate `one_or_more` at ab[0].
        let mut_batch = make_card(
            "synth_mut_innate_batch",
            "Synth MutBatch",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(false, true, 2))],
        );

        let db = build_trig_db(vec![vanilla, mut_once, mut_batch]);

        // (a) [Top, MutOnce, MutOnce] -> both components (component_index 1 & 2) fire once, 2nd death fires 0 more.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            state.rare_or_default(host).unwrap().merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(2),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(2),
                    is_token: false,
                },
            ];
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));
            let f2 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));

            crate::zones::move_object(
                &mut state,
                f1,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
            assert_eq!(state.triggered_once_this_turn().len(), 2);
            assert_ne!(
                state.triggered_once_this_turn()[0],
                state.triggered_once_this_turn()[1]
            );

            crate::zones::move_object(
                &mut state,
                f2,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
        }

        // (b) [Top, MutBatch, MutBatch] -> both components fire in simultaneous batch (2).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            state.rare_or_default(host).unwrap().merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(3),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(3),
                    is_token: false,
                },
            ];
            state.mark_dirty(DirtyMask::DESIGNATIONS);

            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));
            let f2 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));

            with_simultaneous_batch(&mut state, &[f1, f2], |st| {
                crate::zones::move_object(st, f1, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                crate::zones::move_object(st, f2, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                assert_eq!(st.batch_triggered_keys().len(), 2);
                assert_ne!(st.batch_triggered_keys()[0], st.batch_triggered_keys()[1]);
            });
            assert_eq!(state.stack_entries().len(), 2);
        }
    }

    #[test]
    fn single_granter_two_grant_abilities_from_objects_statics_same_donor_triggers_do_not_collide()
    {
        use mtg_ir::AbilityGrantSource;

        let make_any_dies_trig =
            |only_once_per_turn: bool, one_or_more: bool, life: i32| TriggeredAbility {
                trigger: TriggerDef {
                    event: EventKind::ZoneChange,
                    filter: EventFilter {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                zones: ZoneMask::BATTLEFIELD,
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        }),
                        one_or_more,
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
                    scope: None,
                    from_zones: None,
                    only_once_per_turn,
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
                effect: vec![gain_life_step(life)],
                modal: None,
                text: None,
                span: Default::default(),
            };

        let grant_exiled_with_self = || {
            AbilityDef::Static(StaticAbility {
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
                        modifier: LayerMod::GrantAbilitiesFromObjects {
                            from: AbilityGrantSource::ExiledWithSelf {
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
            })
        };

        // CardDefId(1): Vanilla host / fodder creature.
        let vanilla = make_card(
            "synth_mg_vanilla",
            "Synth MgVanilla",
            CardTypeMask::CREATURE,
            vec![],
        );
        // CardDefId(2): Donor with `only_once_per_turn` at ab[0].
        let donor_once = make_card(
            "synth_mg_donor_once",
            "Synth MgDonorOnce",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(true, false, 1))],
        );
        // CardDefId(3): Donor with `one_or_more` at ab[0].
        let donor_batch = make_card(
            "synth_mg_donor_batch",
            "Synth MgDonorBatch",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(make_any_dies_trig(false, true, 2))],
        );
        // CardDefId(4): Single granter enchantment with TWO GrantAbilitiesFromObjects statics (abilities[0] and [1]).
        let double_granter = make_card(
            "synth_mg_double_granter",
            "Synth MgDoubleGranter",
            CardTypeMask::ENCHANTMENT,
            vec![grant_exiled_with_self(), grant_exiled_with_self()],
        );
        // CardDefId(5): Mutate granter component with 1 GrantAbilitiesFromObjects static.
        let mut_granter = make_card(
            "synth_mg_mut_granter",
            "Synth MgMutGranter",
            CardTypeMask::ENCHANTMENT,
            vec![grant_exiled_with_self()],
        );

        let db = build_trig_db(vec![
            vanilla,
            donor_once,
            donor_batch,
            double_granter,
            mut_granter,
        ]);

        let add_exiled_by = |state: &mut GameState, def: CardDefId, granter: ObjectId| {
            let ts = state.allocate_timestamp();
            let oid = state.insert_object(
                GameObject::new(def, PlayerId(0), Zone::Hand, ts, ObjectFlags::empty()),
                None,
            );
            state.player_mut(PlayerId(0)).unwrap().hand.push(oid);
            let eid = move_object_raw(state, oid, Zone::Exile, MoveRawOptions::default(), None)
                .unwrap()
                .new_id
                .unwrap();
            state.rare_or_default(eid).unwrap().exiled_by = Some(granter);
            state.mark_dirty(DirtyMask::EXILE);
            eid
        };

        // (a) 1 DoubleGranter (abilities[0] & abilities[1]) + 1 exiled Donor (`only_once_per_turn`):
        //     first death fires both (2); second death and host's own death (lookback) fire 0 more.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let granter = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let _donor = add_exiled_by(&mut state, CardDefId(2), granter);
            let host = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));
            let f2 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));

            crate::zones::move_object(
                &mut state,
                f1,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
            assert_eq!(state.triggered_once_this_turn().len(), 2);
            assert_ne!(
                state.triggered_once_this_turn()[0],
                state.triggered_once_this_turn()[1]
            );

            crate::zones::move_object(
                &mut state,
                f2,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);

            crate::zones::move_object(
                &mut state,
                host,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
        }

        // (b) 1 DoubleGranter (abilities[0] & abilities[1]) + 1 exiled Donor (`one_or_more`):
        //     simultaneous batch produces 2 distinct BatchTriggerKeys and 2 stack entries.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let granter = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let _donor = add_exiled_by(&mut state, CardDefId(3), granter);
            let _host = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));
            let f2 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));

            with_simultaneous_batch(&mut state, &[f1, f2], |st| {
                crate::zones::move_object(st, f1, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                crate::zones::move_object(st, f2, Zone::Graveyard, MoveRawOptions::default(), None)
                    .unwrap();
                assert_eq!(st.batch_triggered_keys().len(), 2);
                assert_ne!(st.batch_triggered_keys()[0], st.batch_triggered_keys()[1]);
            });
            assert_eq!(state.stack_entries().len(), 2);
        }

        // (c) Mutate granter [Vanilla, MutGranter, MutGranter] (granter_component_index 1 vs 2)
        //     + 1 exiled Donor (`only_once_per_turn`) -> both fire (2).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let mut_g = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            state.rare_or_default(mut_g).unwrap().merged_components = vec![
                crate::state::MergedComponent {
                    card_def: CardDefId(1),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(5),
                    is_token: false,
                },
                crate::state::MergedComponent {
                    card_def: CardDefId(5),
                    is_token: false,
                },
            ];
            state.mark_dirty(DirtyMask::DESIGNATIONS);
            let _donor = add_exiled_by(&mut state, CardDefId(2), mut_g);
            let f1 = add_to_bf(&mut state, PlayerId(1), CardDefId(1));

            crate::zones::move_object(
                &mut state,
                f1,
                Zone::Graveyard,
                MoveRawOptions::default(),
                None,
            )
            .unwrap();
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 2);
            assert_eq!(state.triggered_once_this_turn().len(), 2);
            assert_ne!(
                state.triggered_once_this_turn()[0],
                state.triggered_once_this_turn()[1]
            );
        }
    }

    #[test]
    fn dynamic_ward_preserves_grant_source_and_distinct_origins() {
        let host = make_card(
            "synth_dw_host",
            "Synth DwHost",
            CardTypeMask::CREATURE,
            vec![],
        );
        let db = build_trig_db(vec![host]);
        let mut state = setup_2p(db);
        let host_id = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let granter_a = ObjectId {
            index: 100,
            generation: 1,
        };
        let granter_b = ObjectId {
            index: 101,
            generation: 1,
        };

        let mut cands = Vec::new();
        let lki = Arc::new(crate::events::snapshot_for_live_object(&state, host_id).unwrap());
        push_granted_ability_candidates(
            &state,
            &mut cands,
            host_id,
            Zone::Battlefield,
            PlayerId(0),
            lki,
            &[
                GrantedAbilityEntry::DynamicWard {
                    generic: 2,
                    grant_source: Some(granter_a),
                },
                GrantedAbilityEntry::DynamicWard {
                    generic: 2,
                    grant_source: Some(granter_b),
                },
            ],
        );

        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].ability_idx, 0);
        assert_eq!(cands[1].ability_idx, 0);
        assert_eq!(
            cands[0].kind,
            TriggerSourceKind::DynamicWard {
                grant_source: Some(granter_a),
                generic: 2,
            }
        );
        assert_eq!(
            cands[1].kind,
            TriggerSourceKind::DynamicWard {
                grant_source: Some(granter_b),
                generic: 2,
            }
        );
        assert_ne!(
            trigger_ability_origin(&cands[0].kind),
            trigger_ability_origin(&cands[1].kind)
        );
    }

    #[test]
    fn synth_tidus_joint_other_target_trigger_cr_603_3d() {
        use crate::action::Action;
        use crate::casting::{
            apply_casting_action, legal_casting_actions, prepare_next_untargeted_stack_trigger,
        };
        use mtg_ir::{ModalMode, ReflexiveTriggerSpec};

        let make_spec = |ctrl: Option<RelativeControl>,
                         min: u16,
                         max: u16,
                         other_target: bool,
                         different_controllers: bool,
                         types_any: CardTypeMask| TargetSpec {
            kind: TargetKind::Object,
            filter: ObjectFilter {
                types_any,
                controller: ctrl,
                ..Default::default()
            },
            player: None,
            min,
            max,
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
            different_controllers,
            chosen_by_event_player: false,
            shares_card_type_with_target: None,
            zones: ZoneMask::BATTLEFIELD,
        };

        let begin_combat_trig = || TriggerDef {
            event: EventKind::PhaseBegin,
            filter: EventFilter {
                during_step: Some(GamePhase::BeginCombat),
                during_turn: Some(mtg_ir::DuringTurnKind::Your),
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
        };

        let synth_tidus = make_card(
            "synth_tidus",
            "Synth Guardian",
            CardTypeMask::CREATURE,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: begin_combat_trig(),
                condition: None,
                grant_condition: None,
                targets: vec![
                    make_spec(
                        Some(RelativeControl::You),
                        1,
                        1,
                        false,
                        false,
                        CardTypeMask::CREATURE,
                    ),
                    make_spec(
                        Some(RelativeControl::You),
                        1,
                        1,
                        true,
                        false,
                        CardTypeMask::CREATURE,
                    ),
                ],
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let synth_bear = make_card("synth_bear_t", "Synth Bear", CardTypeMask::CREATURE, vec![]);
        let synth_diff_ctrl = make_card(
            "synth_diff_ctrl",
            "Synth SplitWatch",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: begin_combat_trig(),
                condition: None,
                grant_condition: None,
                targets: vec![make_spec(None, 2, 2, false, true, CardTypeMask::CREATURE)],
                effect: vec![gain_life_step(1)],
                modal: None,
                text: None,
                span: Default::default(),
            })],
        );
        let synth_modal_trig = make_card(
            "synth_modal_trig",
            "Synth DualCall",
            CardTypeMask::ENCHANTMENT,
            vec![AbilityDef::Triggered(TriggeredAbility {
                trigger: begin_combat_trig(),
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
                    modes: vec![
                        ModalMode {
                            targets: vec![make_spec(
                                None,
                                1,
                                1,
                                false,
                                false,
                                CardTypeMask::ARTIFACT,
                            )],
                            effect: vec![gain_life_step(1)],
                            span: Default::default(),
                        },
                        ModalMode {
                            targets: vec![],
                            effect: vec![gain_life_step(2)],
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
                text: None,
                span: Default::default(),
            })],
        );

        let db = build_trig_db(vec![
            synth_tidus,
            synth_bear.clone(),
            synth_diff_ctrl,
            synth_modal_trig,
        ]);

        // (a) With only 1 creature controlled by P0: flush_triggers removes Synth Guardian's trigger (CR 603.3d).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _tidus = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert!(state.stack().is_empty());
            assert_eq!(state.decision(), &DecisionState::Priority);
        }

        // (b) With 2 creatures (A = tidus [source], B = bear): trigger enters stack; because Spec 1
        //     has `other_target: true` WITHOUT `FilterFlags::OTHER`, ChooseTargets offers both A
        //     and B for Spec 0, and choosing B leaves A (the source itself) legal for Spec 1,
        //     committing [B, A] as well as [A, B]. Conversely, when Spec 1 DOES have `FilterFlags::OTHER`,
        //     picking B for Spec 0 would leave Spec 1 empty, so Spec 0 only offers A.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let a = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let b = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
            assert!(prepare_next_untargeted_stack_trigger(&mut state).unwrap());
            assert_eq!(state.decision(), &DecisionState::CastingSequence);

            let legal0 = legal_casting_actions(&state, PlayerId(0));
            assert!(legal0.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: a },
            }));
            assert!(legal0.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: b },
            }));

            // Branch 1: pick A first -> Spec 1 offers only B -> commits [A, B].
            let mut branch_ab = state.clone();
            apply_casting_action(
                &mut branch_ab,
                PlayerId(0),
                &Action::SelectTarget {
                    target: TargetRef::Object { id: a },
                },
            )
            .unwrap();
            let legal1_ab = legal_casting_actions(&branch_ab, PlayerId(0));
            assert!(!legal1_ab.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: a },
            }));
            assert!(legal1_ab.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: b },
            }));
            apply_casting_action(
                &mut branch_ab,
                PlayerId(0),
                &Action::SelectTarget {
                    target: TargetRef::Object { id: b },
                },
            )
            .unwrap();
            assert!(branch_ab.casting().is_none());
            assert_eq!(
                branch_ab.stack_entries()[0].targets,
                vec![TargetRef::Object { id: a }, TargetRef::Object { id: b }]
            );

            // Branch 2: pick B first -> Spec 1 offers source A -> commits [B, A].
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::SelectTarget {
                    target: TargetRef::Object { id: b },
                },
            )
            .unwrap();
            let legal1_ba = legal_casting_actions(&state, PlayerId(0));
            assert!(legal1_ba.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: a },
            }));
            assert!(!legal1_ba.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: b },
            }));
            apply_casting_action(
                &mut state,
                PlayerId(0),
                &Action::SelectTarget {
                    target: TargetRef::Object { id: a },
                },
            )
            .unwrap();
            assert!(state.casting().is_none());
            assert_eq!(
                state.stack_entries()[0].targets,
                vec![TargetRef::Object { id: b }, TargetRef::Object { id: a }]
            );
        }

        // (c) different_controllers: true with 2 creatures under P0 (removed) vs 1 P0 + 1 P1 (succeeds).
        {
            let mut state = setup_2p(Arc::clone(&db));
            let _watch = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
            let _p0_a = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            let _p0_b = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert!(state.stack().is_empty());

            let _p1_c = add_to_bf(&mut state, PlayerId(1), CardDefId(2));
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
        }

        // (d) Modal trigger with min: 2, max: 2 and only 1 legal mode (0 artifacts) is removed (CR 603.3c/d),
        //     and a PendingReflexiveTrigger with impossible joint other_target is also removed.
        {
            let mut state = setup_2p(Arc::clone(&db));
            let modal_src = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
            let bear = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert!(state.stack().is_empty());

            let lki = Arc::new(crate::events::snapshot_for_live_object(&state, bear).unwrap());
            let mut rt_spec_targets = vec![
                make_spec(
                    Some(RelativeControl::You),
                    1,
                    1,
                    false,
                    false,
                    CardTypeMask::CREATURE,
                ),
                make_spec(
                    Some(RelativeControl::You),
                    1,
                    1,
                    true,
                    false,
                    CardTypeMask::CREATURE,
                ),
            ];
            let _ = modal_src;
            state
                .pending_reflexive_triggers_mut()
                .push(PendingReflexiveTrigger {
                    id: 1,
                    controller: PlayerId(0),
                    source: bear,
                    source_lki: lki,
                    spec: ReflexiveTriggerSpec {
                        targets: std::mem::take(&mut rt_spec_targets),
                        effect: vec![gain_life_step(1)],
                        modal: None,
                        text: None,
                        event_filter: None,
                        object_filter: None,
                    },
                    event_object: None,
                    event_lki: None,
                    event_player: None,
                    event_amount: None,
                    bindings: vec![],
                    x_value: 0,
                });
            flush_triggers(&mut state);
            assert!(state.stack().is_empty());
        }

        // (e) Contrast: when Spec 1 has `FilterFlags::OTHER` (alongside `other_target: true`),
        //     `source` A is excluded from Spec 1, so picking B for Spec 0 would leave Spec 1
        //     empty; therefore Spec 0 offers ONLY A, then Spec 1 offers ONLY B.
        {
            let mut spec_other_flag = make_spec(
                Some(RelativeControl::You),
                1,
                1,
                true,
                false,
                CardTypeMask::CREATURE,
            );
            spec_other_flag.filter.flags = FilterFlags::OTHER;
            let synth_other_flag_card = make_card(
                "synth_other_flag_trig",
                "Synth OtherFilter",
                CardTypeMask::CREATURE,
                vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger: begin_combat_trig(),
                    condition: None,
                    grant_condition: None,
                    targets: vec![
                        make_spec(
                            Some(RelativeControl::You),
                            1,
                            1,
                            false,
                            false,
                            CardTypeMask::CREATURE,
                        ),
                        spec_other_flag,
                    ],
                    effect: vec![gain_life_step(1)],
                    modal: None,
                    text: None,
                    span: Default::default(),
                })],
            );
            let db_other = build_trig_db(vec![synth_other_flag_card, synth_bear]);
            let mut state = setup_2p(Arc::clone(&db_other));
            let a = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
            let b = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
            state.set_active_player(PlayerId(0));
            state.set_phase(GamePhase::BeginCombat);
            state.set_decision(DecisionState::Priority);
            emit_event(
                &mut state,
                AtomicEventKind::PhaseStepEntered {
                    phase: GamePhase::BeginCombat,
                    active_player: PlayerId(0),
                },
            );
            flush_triggers(&mut state);
            assert_eq!(state.stack_entries().len(), 1);
            assert!(prepare_next_untargeted_stack_trigger(&mut state).unwrap());
            let legal0 = legal_casting_actions(&state, PlayerId(0));
            assert!(legal0.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: a },
            }));
            assert!(!legal0.contains(&Action::SelectTarget {
                target: TargetRef::Object { id: b },
            }));
        }
    }

    #[test]
    fn synth_inline_mana_trigger_ignores_redundant_step_amount() {
        use mtg_ir::ManaProduction;
        use smallvec::smallvec;

        let trig_land = make_card(
            "synth_trig_land",
            "Synth ManaLand",
            CardTypeMask::LAND,
            vec![],
        );
        let db = build_trig_db(vec![trig_land]);
        let mut state = setup_2p(db);
        let src = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let lki = Arc::new(crate::events::snapshot_for_live_object(&state, src).unwrap());

        let make_pt = |order: u64, step: EffectStep| PendingTrigger {
            order,
            mode: TriggerMode::Inline,
            source: src,
            source_lki: Arc::clone(&lki),
            controller: PlayerId(0),
            kind: TriggerSourceKind::Innate {
                component_index: 0,
                face_index: 0,
                ability_idx: 0,
            },
            trigger_def: TriggerDef {
                event: EventKind::BecomesTapped,
                filter: EventFilter::default(),
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
            targets: vec![],
            effect: vec![step],
            modal: None,
            event_object: None,
            event_lki: None,
            event_player: None,
            event_amount: None,
            defending_player: None,
            last_tapped_mana_colors: SmallVec::new(),
            is_look_back: false,
            x_value: 0,
        };

        // 1. Multi-symbol Fixed {R}{R} with redundant step.amount = 2 adds 2 {R} (NOT 4!).
        let rr_step = EffectStep {
            op: EffectOp::AddMana {
                production: ManaProduction::Fixed {
                    symbols: smallvec![ManaColor::R, ManaColor::R],
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
            subject: Selector::None,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 2 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        execute_inline_mana_trigger(&mut state, &make_pt(1, rr_step));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 0, 2, 0, 0]
        );

        // 2. Single-symbol Fixed {G} with step.amount = 2 STILL adds 2 {G}.
        let gg_step = EffectStep {
            op: EffectOp::AddMana {
                production: ManaProduction::Fixed {
                    symbols: smallvec![ManaColor::G],
                },
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
            subject: Selector::None,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: 2 })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };
        execute_inline_mana_trigger(&mut state, &make_pt(2, gg_step));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 0, 2, 2, 0]
        );
    }

    #[test]
    fn synth_inline_mana_trigger_bug_q_and_u_variants() {
        use mtg_ir::{ManaChoiceKind, ManaProduction};
        use smallvec::smallvec;

        let mut trig_land = make_card(
            "synth_trig_q_u",
            "Synth ManaTrigger",
            CardTypeMask::LAND,
            vec![],
        );
        trig_land.faces[0].characteristics.colors = ColorMask::empty();

        let mut gg_cre = make_card(
            "synth_gg_q_u",
            "Synth Double Green",
            CardTypeMask::CREATURE,
            vec![],
        );
        gg_cre.faces[0].characteristics.mana_cost = ManaCost::parse("{G}{G}");
        gg_cre.faces[0].characteristics.colors = ColorMask::G;

        let mut wu_cre = make_card(
            "synth_wu_q_u",
            "Synth Azorius Bear",
            CardTypeMask::CREATURE,
            vec![],
        );
        wu_cre.faces[0].characteristics.mana_cost = ManaCost::parse("{W}{U}");
        wu_cre.faces[0].characteristics.colors = ColorMask::W | ColorMask::U;

        let mut r_cre = make_card(
            "synth_r_q_u",
            "Synth Red Scout",
            CardTypeMask::CREATURE,
            vec![],
        );
        r_cre.faces[0].characteristics.mana_cost = ManaCost::parse("{R}");
        r_cre.faces[0].characteristics.colors = ColorMask::R;

        let db = build_trig_db(vec![trig_land, gg_cre, wu_cre, r_cre]);
        let mut state = setup_2p(db);
        let src = add_to_bf(&mut state, PlayerId(0), CardDefId(1));
        let base_lki = Arc::new(crate::events::snapshot_for_live_object(&state, src).unwrap());

        let make_step = |production: ManaProduction, step_amt: i32| EffectStep {
            op: EffectOp::AddMana {
                production,
                restriction: None,
                restriction_spec: None,
                retain_until_eot: false,
                delayed_rider: None,
            },
            subject: Selector::None,
            actor: None,
            amount: Some(Box::new(Amount::Const { n: step_amt })),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Default::default(),
        };

        let make_pt = |order: u64, step: EffectStep, lki: Arc<LkiSnapshot>, is_look_back: bool| {
            PendingTrigger {
                order,
                mode: TriggerMode::Inline,
                source: src,
                source_lki: lki,
                controller: PlayerId(0),
                kind: TriggerSourceKind::Innate {
                    component_index: 0,
                    face_index: 0,
                    ability_idx: 0,
                },
                trigger_def: TriggerDef {
                    event: EventKind::BecomesTapped,
                    filter: EventFilter::default(),
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
                targets: vec![],
                effect: vec![step],
                modal: None,
                event_object: None,
                event_lki: None,
                event_player: None,
                event_amount: None,
                defending_player: None,
                last_tapped_mana_colors: SmallVec::new(),
                is_look_back,
                x_value: 0,
            }
        };

        // 1. WithChosenColor { W } with chosen_color = None -> [1, 0, 0, 0, 0, 0]
        let with_step = make_step(
            ManaProduction::WithChosenColor {
                colors: ColorMask::W,
            },
            3,
        );
        execute_inline_mana_trigger(
            &mut state,
            &make_pt(1, with_step.clone(), base_lki.clone(), false),
        );
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 0, 0, 0, 0, 0]
        );

        // 2. WithChosenColor { W } with chosen_color = Some(U) -> [1, 1, 0, 0, 0, 0]
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        state.rare_or_default(src).unwrap().chosen_color = Some(ColorMask::U);
        execute_inline_mana_trigger(
            &mut state,
            &make_pt(2, with_step.clone(), base_lki.clone(), false),
        );
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 1, 0, 0, 0, 0]
        );

        // 3. OrChosenColor { W } with chosen_color = Some(U) -> [0, 1, 0, 0, 0, 0]
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let or_step = make_step(
            ManaProduction::OrChosenColor {
                colors: ColorMask::W,
            },
            3,
        );
        execute_inline_mana_trigger(&mut state, &make_pt(3, or_step, base_lki.clone(), false));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 1, 0, 0, 0, 0]
        );

        // 4. Bundles { options: [[W, U], [B, R]] } with step.amount = 3 -> [1, 1, 0, 0, 0, 0]
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let bundle_step = make_step(
            ManaProduction::Bundles {
                options: vec![
                    smallvec![ManaColor::W, ManaColor::U],
                    smallvec![ManaColor::B, ManaColor::R],
                ],
            },
            3,
        );
        execute_inline_mana_trigger(
            &mut state,
            &make_pt(4, bundle_step, base_lki.clone(), false),
        );
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 1, 0, 0, 0, 0]
        );

        // 5. Choice { kind: Any, colors: empty } with step.amount = 2 -> [2, 0, 0, 0, 0, 0] (W, not C)
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let any_step = make_step(
            ManaProduction::Choice {
                kind: ManaChoiceKind::Any,
                colors: ColorMask::empty(),
            },
            2,
        );
        execute_inline_mana_trigger(&mut state, &make_pt(5, any_step, base_lki.clone(), false));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [2, 0, 0, 0, 0, 0]
        );

        // 6. ChosenColorAmount(Devotion { W }) with chosen_color = G and {G}{G} creature -> [0, 0, 0, 0, 2, 0]
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let _gg = add_to_bf(&mut state, PlayerId(0), CardDefId(2));
        state.rare_or_default(src).unwrap().chosen_color = Some(ColorMask::G);
        let devotion_step = make_step(
            ManaProduction::ChosenColorAmount {
                amount: Box::new(Amount::Devotion {
                    colors: ColorMask::W,
                    player: None,
                }),
            },
            1,
        );
        execute_inline_mana_trigger(
            &mut state,
            &make_pt(6, devotion_step.clone(), base_lki.clone(), false),
        );
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [0, 0, 0, 0, 2, 0]
        );

        // 7. ChosenColorAmount with chosen_color = None adds 0 mana
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        state.rare_or_default(src).unwrap().chosen_color = None;
        execute_inline_mana_trigger(&mut state, &make_pt(7, devotion_step, base_lki, false));
        assert_eq!(state.player(PlayerId(0)).unwrap().mana_pool, [0; 6]);

        // 8. PerColorControlled delegation with W|U and R creatures (plus G creature) -> [1, 1, 0, 1, 1, 0]
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        let _wu = add_to_bf(&mut state, PlayerId(0), CardDefId(3));
        let _r = add_to_bf(&mut state, PlayerId(0), CardDefId(4));
        let per_color_step = make_step(ManaProduction::PerColorControlled, 1);
        let lki_now = Arc::new(crate::events::snapshot_for_live_object(&state, src).unwrap());
        execute_inline_mana_trigger(&mut state, &make_pt(8, per_color_step, lki_now, false));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 1, 0, 1, 1, 0]
        );

        // 9. WithChosenColor LKI look-back fallback after source leaves battlefield
        state.player_mut(PlayerId(0)).unwrap().mana_pool = [0; 6];
        state.rare_or_default(src).unwrap().chosen_color = Some(ColorMask::B);
        let lki_b = Arc::new(crate::events::snapshot_for_live_object(&state, src).unwrap());
        let _ = move_object_raw(
            &mut state,
            src,
            Zone::Graveyard,
            MoveRawOptions::default(),
            None,
        )
        .unwrap();
        execute_inline_mana_trigger(&mut state, &make_pt(9, with_step, lki_b, true));
        assert_eq!(
            state.player(PlayerId(0)).unwrap().mana_pool,
            [1, 0, 1, 0, 0, 0]
        );
    }
}
