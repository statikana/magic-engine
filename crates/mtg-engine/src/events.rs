use std::sync::Arc;

use mtg_ir::{
    AbilityDef, CardDefId, CardTypeMask, ColorMask, CounterId, GamePhase, GiftKind, KeywordId,
    ManaColor, SubtypeId, SupertypeMask, TriggeredAbility, Zone,
};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::action::TargetRef;
use crate::faces::{active_abilities_for_object, face_opts_for_object, resolve_object_faces};
use crate::id::{ObjectId, PlayerId, StackItemId};
use crate::layers::predicates::{GrantedAbilityEntry, StaticGrantOrigin};
use crate::state::{
    AttackerEntry, BlockerEntry, DirtyMask, EvaluatedObject, GameState, LkiSnapshot, ObjectFlags,
    ReplacementGrantOrigin, TOKEN_CARD_DEF_ID, arc_serde, evaluate_token_spec_base, opt_arc_serde,
};

/// A deterministic, sequence-numbered game event emitted during action resolution (CR 603.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomicEvent {
    pub seq: u64,
    pub kind: AtomicEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AtomicEventKind {
    ZoneChange {
        object: ObjectId,
        new_id: Option<ObjectId>,
        from: Option<Zone>,
        to: Zone,
        from_player: Option<PlayerId>,
        to_player: Option<PlayerId>,
        ceased_to_exist: bool,
        #[serde(with = "opt_arc_serde")]
        lki: Option<Arc<LkiSnapshot>>,
    },
    Cast {
        player: PlayerId,
        object: ObjectId,
        stack_item: StackItemId,
        is_copy: bool,
        targets: Vec<TargetRef>,
        mana_spent: u32,
        colored_mana_spent: u32,
        kicked: bool,
        from_zone: Zone,
    },
    SpellCopied {
        player: PlayerId,
        object: ObjectId,
        stack_item: StackItemId,
    },
    SpellCountered {
        stack_item: StackItemId,
        #[serde(with = "arc_serde")]
        countered_lki: Arc<LkiSnapshot>,
        by_controller: PlayerId,
        by_source: Option<ObjectId>,
        by_spell_or_ability: bool,
    },
    AbilityActivated {
        player: PlayerId,
        source: ObjectId,
        stack_item: Option<StackItemId>,
        is_mana: bool,
        is_loyalty: bool,
        life_paid: u32,
    },
    AttackDeclared {
        player: PlayerId,
        attackers: Vec<AttackerEntry>,
    },
    BlockersDeclared {
        defender: PlayerId,
        blocks: Vec<BlockerEntry>,
    },
    UnblockedAttackers {
        defender: PlayerId,
        attackers: Vec<ObjectId>,
    },
    AttackTriggered {
        source: ObjectId,
        controller: PlayerId,
        stack_item: StackItemId,
    },
    DamageDealt {
        source: Option<ObjectId>,
        source_controller: PlayerId,
        target: TargetRef,
        amount: u32,
        is_combat: bool,
    },
    CombatDamageBatchPlayer {
        damaged_player: PlayerId,
        attackers: Vec<ObjectId>,
    },
    PhaseStepEntered {
        phase: GamePhase,
        active_player: PlayerId,
    },
    CounterChanged {
        object: ObjectId,
        counter: CounterId,
        previous_count: u32,
        new_count: u32,
        actor: Option<PlayerId>,
    },
    LifeChange {
        player: PlayerId,
        /// `> 0` = GainLife, `< 0` = LoseLife; `0` emits nothing (CR 119.9).
        delta: i32,
    },
    DrawCard {
        player: PlayerId,
        object: ObjectId,
        in_own_draw_step_ordinal: Option<u16>,
    },
    Discard {
        player: PlayerId,
        object: ObjectId,
        dest_zone: Zone,
        is_cycle: bool,
        cause_controller: Option<PlayerId>,
    },
    TapStateChanged {
        object: ObjectId,
        now_tapped: bool,
        actor: Option<PlayerId>,
    },
    BecomesTarget {
        target: TargetRef,
        by_spell: bool,
        controller: PlayerId,
        stack_item: Option<StackItemId>,
    },
    AttachmentChanged {
        attachment: ObjectId,
        host: ObjectId,
        attached: bool,
    },
    TappedForMana {
        player: PlayerId,
        source: ObjectId,
        produced_colors: SmallVec<[ManaColor; 6]>,
        amount: u32,
    },
    ManaAddedByLandAbility {
        player: PlayerId,
        source: ObjectId,
        produced_colors: SmallVec<[ManaColor; 6]>,
        amount: u32,
    },
    LandPlayed {
        player: PlayerId,
        object: ObjectId,
        from_zone: Zone,
    },
    Transformed {
        object: ObjectId,
        to_face_index: u8,
    },
    TurnedFaceUp {
        object: ObjectId,
        player: PlayerId,
        x_value: Option<u32>,
    },
    PhasedIn {
        object: ObjectId,
        controller: PlayerId,
    },
    TokenCreated {
        player: PlayerId,
        object: ObjectId,
    },
    Sacrificed {
        player: PlayerId,
        object: ObjectId,
        #[serde(with = "arc_serde")]
        lki: Arc<LkiSnapshot>,
    },
    BecameMonarch {
        player: PlayerId,
        previous: Option<PlayerId>,
    },
    DayNightChanged {
        previous_night: Option<bool>,
        now_night: bool,
    },
    PlayerLostGame {
        player: PlayerId,
    },
    LibrarySearched {
        player: PlayerId,
    },
    LibraryShuffled {
        player: PlayerId,
    },
    EnergyGained {
        player: PlayerId,
        amount: u16,
    },
    Proliferated {
        player: PlayerId,
    },
    Investigate {
        player: PlayerId,
        count: u16,
    },
    Surveil {
        player: PlayerId,
        count: u16,
        graveyard_count: u16,
    },
    Scry {
        player: PlayerId,
        amount: u16,
    },
    Explore {
        player: PlayerId,
        object: ObjectId,
        revealed_land: bool,
    },
    Connive {
        player: PlayerId,
        object: ObjectId,
        nonland_discarded: u16,
    },
    BecomesCrewed {
        player: PlayerId,
        vehicle: ObjectId,
        crewed_by: Vec<ObjectId>,
    },
    CrewedVehicle {
        player: PlayerId,
        creature: ObjectId,
        vehicle: ObjectId,
    },
    BecomesSaddled {
        player: PlayerId,
        mount: ObjectId,
        saddled_by: Vec<ObjectId>,
    },
    SaddledMount {
        player: PlayerId,
        creature: ObjectId,
        mount: ObjectId,
    },
    Exerted {
        player: PlayerId,
        object: ObjectId,
    },
    Amass {
        player: PlayerId,
        object: Option<ObjectId>,
        subtype: SubtypeId,
        amount: u16,
    },
    ManifestDread {
        player: PlayerId,
        manifested: Option<ObjectId>,
        graveyard_ids: Vec<ObjectId>,
    },
    BecameMonstrous {
        player: PlayerId,
        object: ObjectId,
        amount: u16,
    },
    CommittedCrime {
        player: PlayerId,
        stack_item: Option<StackItemId>,
    },
    Exploited {
        player: PlayerId,
        exploiter: ObjectId,
        #[serde(with = "arc_serde")]
        exploited_lki: Arc<LkiSnapshot>,
    },
    ManaExpended {
        player: PlayerId,
        previous_total: u32,
        new_total: u32,
        amount_spent: u32,
    },
    GraveyardLeaveBatch {
        owner: PlayerId,
        objects: Vec<ObjectId>,
    },
    Discover {
        player: PlayerId,
        n: u16,
        hit_object: Option<ObjectId>,
        was_cast: bool,
    },
    Forage {
        player: PlayerId,
    },
    CollectEvidence {
        player: PlayerId,
        total_mv: u32,
    },
    ClassLevelGained {
        object: ObjectId,
        controller: PlayerId,
        level: u16,
    },
    CaseSolved {
        object: ObjectId,
        controller: PlayerId,
    },
    BecameRenowned {
        object: ObjectId,
        controller: PlayerId,
        amount: u16,
    },
    DoorUnlocked {
        player: PlayerId,
        object: ObjectId,
        face_index: u8,
    },
    RoomFullyUnlocked {
        player: PlayerId,
        object: ObjectId,
    },
    RingTempted {
        player: PlayerId,
        ring_level: u8,
        ring_bearer: Option<ObjectId>,
    },
    DungeonRoomEntered {
        player: PlayerId,
        room_index: u8,
    },
    DungeonCompleted {
        player: PlayerId,
    },
    TookInitiative {
        player: PlayerId,
    },
    Mutates {
        player: PlayerId,
        object: ObjectId,
        mutate_count: u16,
    },
    SoulbondPaired {
        player: PlayerId,
        primary: ObjectId,
        partner: ObjectId,
    },
    HauntedCreatureDies {
        haunter: ObjectId,
        #[serde(with = "arc_serde")]
        haunted_lki: Arc<LkiSnapshot>,
    },
    FacedDilemma {
        player: PlayerId,
        stack_item: Option<StackItemId>,
    },
    ExhaustActivated {
        player: PlayerId,
        object: ObjectId,
    },
    CoinFlipped {
        player: PlayerId,
        won: bool,
        wins: u16,
        flips: u16,
    },
    DieRolled {
        player: PlayerId,
        result: i32,
        natural_result: i32,
        sides: u16,
        count: u16,
    },
    Evolved {
        player: PlayerId,
        object: ObjectId,
    },
    PlayersFinishedVoting {
        controller: PlayerId,
    },
    Clash {
        player: PlayerId,
        won: bool,
    },
    SagaChapterResolved {
        object: ObjectId,
        controller: PlayerId,
        chapters: SmallVec<[u8; 3]>,
        is_final: bool,
    },
    GiftGiven {
        player: PlayerId,
        recipient: PlayerId,
        gift_kind: GiftKind,
    },
    Foretell {
        player: PlayerId,
        object: ObjectId,
    },
    Plot {
        player: PlayerId,
        object: ObjectId,
    },
}

/// Compact serializable turn-scoped event history stored on [`GameState`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEventHistory {
    pub creatures_died_this_turn: u16,
    pub nontoken_creatures_died_this_turn: u16,
    pub permanents_left_bf_this_turn: u16,
    pub life_gained_this_turn: SmallVec<[(PlayerId, u32); 4]>,
    pub life_lost_this_turn: SmallVec<[(PlayerId, u32); 4]>,
    pub life_gain_events_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub life_loss_events_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub cards_drawn_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub draw_step_draws_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub spells_cast_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub spells_cast_last_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub cards_discarded_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub tokens_created_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub sacrificed_this_turn: SmallVec<[(PlayerId, u16); 4]>,
    pub mana_expended_this_turn: SmallVec<[(PlayerId, u32); 4]>,
    pub committed_crime_this_turn: SmallVec<[PlayerId; 4]>,
    pub targeted_by_this_turn: Vec<(PlayerId, ObjectId)>,
    pub damaged_by_this_turn: Vec<(ObjectId, ObjectId)>,
    pub damage_dealt_to_players_this_turn: Vec<(Option<ObjectId>, PlayerId, u32, bool)>,
    pub event_counts_this_turn: Vec<(u16, PlayerId, u16)>,
    pub creatures_died_under_control_this_turn: SmallVec<[PlayerId; 4]>,
    pub cards_left_graveyard_this_turn: SmallVec<[PlayerId; 4]>,
    pub player_lost_life_last_turn: SmallVec<[PlayerId; 4]>,
    pub first_upkeep_seen: SmallVec<[PlayerId; 4]>,
    pub first_turn_of_game_done: bool,
    pub combat_phases_this_turn: u8,
    pub permanents_put_into_gy_from_bf_this_turn: u16,
    pub put_into_graveyard_from_non_battlefield_this_turn: Vec<ObjectId>,
    pub creatures_attacked_this_turn: Vec<(PlayerId, ObjectId)>,
    pub previous_turn_active_player: Option<PlayerId>,
}

impl TurnEventHistory {
    #[inline]
    pub fn add_u32(list: &mut SmallVec<[(PlayerId, u32); 4]>, player: PlayerId, delta: u32) {
        if delta == 0 {
            return;
        }
        if let Some((_, val)) = list.iter_mut().find(|(p, _)| *p == player) {
            *val = val.saturating_add(delta);
        } else {
            list.push((player, delta));
        }
    }

    #[inline]
    pub fn add_u16(list: &mut SmallVec<[(PlayerId, u16); 4]>, player: PlayerId, delta: u16) {
        if delta == 0 {
            return;
        }
        if let Some((_, val)) = list.iter_mut().find(|(p, _)| *p == player) {
            *val = val.saturating_add(delta);
        } else {
            list.push((player, delta));
        }
    }

    #[inline]
    pub fn get_u32(list: &[(PlayerId, u32)], player: PlayerId) -> u32 {
        list.iter()
            .find(|(p, _)| *p == player)
            .map_or(0, |(_, v)| *v)
    }

    #[inline]
    pub fn get_u16(list: &[(PlayerId, u16)], player: PlayerId) -> u16 {
        list.iter()
            .find(|(p, _)| *p == player)
            .map_or(0, |(_, v)| *v)
    }

    #[inline]
    pub fn increment_event_count(&mut self, tag: u16, player: PlayerId) -> u16 {
        if let Some((_, _, count)) = self
            .event_counts_this_turn
            .iter_mut()
            .find(|(t, p, _)| *t == tag && *p == player)
        {
            *count = count.saturating_add(1);
            *count
        } else {
            self.event_counts_this_turn.push((tag, player, 1));
            1
        }
    }

    #[inline]
    pub fn event_count(&self, tag: u16, player: PlayerId) -> u16 {
        self.event_counts_this_turn
            .iter()
            .find(|(t, p, _)| *t == tag && *p == player)
            .map_or(0, |(_, _, c)| *c)
    }

    #[inline]
    pub fn was_targeted_by_this_turn(&self, player: PlayerId, target: ObjectId) -> bool {
        self.targeted_by_this_turn
            .iter()
            .any(|&(p, o)| p == player && o == target)
    }

    #[inline]
    pub fn was_damaged_by_this_turn(&self, source: ObjectId, victim: ObjectId) -> bool {
        self.damaged_by_this_turn
            .iter()
            .any(|&(s, v)| s == source && v == victim)
    }

    /// Resets per-turn counters at turn cleanup / turn advance while rolling over
    /// `spells_cast_last_turn` and `player_lost_life_last_turn`.
    pub fn end_turn_rollover(&mut self) {
        self.spells_cast_last_turn = self.spells_cast_this_turn.clone();
        self.player_lost_life_last_turn.clear();
        for &(pid, lost) in &self.life_lost_this_turn {
            if lost > 0 && !self.player_lost_life_last_turn.contains(&pid) {
                self.player_lost_life_last_turn.push(pid);
            }
        }
        self.creatures_died_this_turn = 0;
        self.nontoken_creatures_died_this_turn = 0;
        self.permanents_left_bf_this_turn = 0;
        self.life_gained_this_turn.clear();
        self.life_lost_this_turn.clear();
        self.life_gain_events_this_turn.clear();
        self.life_loss_events_this_turn.clear();
        self.cards_drawn_this_turn.clear();
        self.draw_step_draws_this_turn.clear();
        self.spells_cast_this_turn.clear();
        self.cards_discarded_this_turn.clear();
        self.tokens_created_this_turn.clear();
        self.sacrificed_this_turn.clear();
        self.mana_expended_this_turn.clear();
        self.committed_crime_this_turn.clear();
        self.targeted_by_this_turn.clear();
        self.damaged_by_this_turn.clear();
        self.damage_dealt_to_players_this_turn.clear();
        self.event_counts_this_turn.clear();
        self.creatures_died_under_control_this_turn.clear();
        self.cards_left_graveyard_this_turn.clear();
        self.first_turn_of_game_done = true;
        self.combat_phases_this_turn = 0;
        self.permanents_put_into_gy_from_bf_this_turn = 0;
        self.put_into_graveyard_from_non_battlefield_this_turn
            .clear();
        self.creatures_attacked_this_turn.clear();
    }

    /// Performs [`Self::end_turn_rollover`] and records `finished_player` as
    /// [`Self::previous_turn_active_player`] (for Day/Night `CR 730.2` checks).
    pub fn end_turn_rollover_with_player(&mut self, finished_player: PlayerId) {
        self.end_turn_rollover();
        self.previous_turn_active_player = Some(finished_player);
    }
}

/// Serializable record of a Layer-6 granted triggered ability captured pre-batch (CR 603.10a).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GrantedTriggerLki {
    CardAbility {
        card_def: CardDefId,
        face_index: u8,
        ability_index: usize,
        grant_source: Option<ObjectId>,
        grant_controller: PlayerId,
        donor_object: ObjectId,
        donor_component_index: u8,
        granter_origin: StaticGrantOrigin,
        granter_card_def: Option<CardDefId>,
        granter_face_index: u8,
        granter_component_index: u8,
        granter_ability_index: u32,
        source_effect_id: u64,
    },
    Inline {
        ability: Box<TriggeredAbility>,
        grant_source: Option<ObjectId>,
        grant_controller: PlayerId,
        grant_origin: ReplacementGrantOrigin,
        grant_card_def: Option<CardDefId>,
        source_face_index: u8,
        source_component_index: u8,
        source_ability_index: u32,
        source_effect_id: u64,
    },
}

/// Pre-leave Layer 1–7d snapshot captured before any object leaves the battlefield in a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchLkiEntry {
    pub id: ObjectId,
    #[serde(with = "arc_serde")]
    pub lki: Arc<LkiSnapshot>,
    pub eval: EvaluatedObject,
    pub granted_triggers: Vec<GrantedTriggerLki>,
}

/// Guard token returned by [`begin_simultaneous_batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLkiGuardToken {
    pub scope_depth: usize,
}

/// Returns the selective [`DirtyMask`] associated with recording `kind`'s turn-history side effects.
///
/// Purely observational events (e.g. `PhaseStepEntered`, `LibrarySearched`) return `DirtyMask::empty()`
/// unless they mutate tracked turn-history state, avoiding needless `OnceLock<LayerCache>` churn.
pub fn dirty_mask_for_event(kind: &AtomicEventKind) -> DirtyMask {
    match kind {
        AtomicEventKind::ZoneChange { from, to, .. } => {
            let mut m = DirtyMask::TOPOLOGY | DirtyMask::TURN_HISTORY;
            if *from == Some(Zone::Graveyard) || *to == Zone::Graveyard {
                m |= DirtyMask::GRAVEYARDS;
            }
            if *from == Some(Zone::Exile) || *to == Zone::Exile {
                m |= DirtyMask::EXILE;
            }
            m
        }
        AtomicEventKind::CounterChanged { .. } => DirtyMask::COUNTERS,
        AtomicEventKind::LifeChange { .. } => DirtyMask::LIFE | DirtyMask::TURN_HISTORY,
        AtomicEventKind::TapStateChanged { .. } => DirtyMask::TAPPED,
        AtomicEventKind::DamageDealt { .. }
        | AtomicEventKind::AttackDeclared { .. }
        | AtomicEventKind::BlockersDeclared { .. }
        | AtomicEventKind::CombatDamageBatchPlayer { .. }
        | AtomicEventKind::UnblockedAttackers { .. } => DirtyMask::COMBAT | DirtyMask::TURN_HISTORY,
        AtomicEventKind::BecameMonarch { .. }
        | AtomicEventKind::DayNightChanged { .. }
        | AtomicEventKind::DoorUnlocked { .. }
        | AtomicEventKind::RoomFullyUnlocked { .. }
        | AtomicEventKind::ClassLevelGained { .. }
        | AtomicEventKind::CaseSolved { .. }
        | AtomicEventKind::BecameRenowned { .. }
        | AtomicEventKind::Exerted { .. }
        | AtomicEventKind::BecameMonstrous { .. }
        | AtomicEventKind::RingTempted { .. }
        | AtomicEventKind::TookInitiative { .. } => DirtyMask::DESIGNATIONS,
        AtomicEventKind::Cast { .. }
        | AtomicEventKind::Discard { .. }
        | AtomicEventKind::DrawCard { .. }
        | AtomicEventKind::LandPlayed { .. }
        | AtomicEventKind::Sacrificed { .. }
        | AtomicEventKind::CommittedCrime { .. }
        | AtomicEventKind::ManaExpended { .. }
        | AtomicEventKind::EnergyGained { .. }
        | AtomicEventKind::PlayerLostGame { .. } => {
            DirtyMask::PLAYER_STATE | DirtyMask::TURN_HISTORY
        }
        AtomicEventKind::Transformed { .. }
        | AtomicEventKind::TurnedFaceUp { .. }
        | AtomicEventKind::PhasedIn { .. }
        | AtomicEventKind::TokenCreated { .. }
        | AtomicEventKind::AttachmentChanged { .. }
        | AtomicEventKind::Mutates { .. }
        | AtomicEventKind::SoulbondPaired { .. } => DirtyMask::TOPOLOGY,
        AtomicEventKind::GraveyardLeaveBatch { .. } => {
            DirtyMask::GRAVEYARDS | DirtyMask::TURN_HISTORY
        }
        AtomicEventKind::PhaseStepEntered { phase, .. } => match phase {
            GamePhase::BeginCombat => DirtyMask::TURN_HISTORY | DirtyMask::COMBAT,
            GamePhase::Upkeep => DirtyMask::TURN_HISTORY,
            _ => DirtyMask::empty(),
        },
        AtomicEventKind::SpellCopied { .. }
        | AtomicEventKind::SpellCountered { .. }
        | AtomicEventKind::AbilityActivated { .. }
        | AtomicEventKind::AttackTriggered { .. }
        | AtomicEventKind::BecomesTarget { .. }
        | AtomicEventKind::TappedForMana { .. }
        | AtomicEventKind::ManaAddedByLandAbility { .. }
        | AtomicEventKind::LibrarySearched { .. }
        | AtomicEventKind::LibraryShuffled { .. }
        | AtomicEventKind::Proliferated { .. }
        | AtomicEventKind::Investigate { .. }
        | AtomicEventKind::Surveil { .. }
        | AtomicEventKind::Scry { .. }
        | AtomicEventKind::Explore { .. }
        | AtomicEventKind::Connive { .. }
        | AtomicEventKind::BecomesCrewed { .. }
        | AtomicEventKind::CrewedVehicle { .. }
        | AtomicEventKind::BecomesSaddled { .. }
        | AtomicEventKind::SaddledMount { .. }
        | AtomicEventKind::Amass { .. }
        | AtomicEventKind::ManifestDread { .. }
        | AtomicEventKind::Exploited { .. }
        | AtomicEventKind::Discover { .. }
        | AtomicEventKind::Forage { .. }
        | AtomicEventKind::CollectEvidence { .. }
        | AtomicEventKind::DungeonRoomEntered { .. }
        | AtomicEventKind::DungeonCompleted { .. }
        | AtomicEventKind::HauntedCreatureDies { .. }
        | AtomicEventKind::FacedDilemma { .. }
        | AtomicEventKind::ExhaustActivated { .. }
        | AtomicEventKind::CoinFlipped { .. }
        | AtomicEventKind::DieRolled { .. }
        | AtomicEventKind::Evolved { .. }
        | AtomicEventKind::PlayersFinishedVoting { .. }
        | AtomicEventKind::Clash { .. }
        | AtomicEventKind::SagaChapterResolved { .. }
        | AtomicEventKind::GiftGiven { .. }
        | AtomicEventKind::Foretell { .. }
        | AtomicEventKind::Plot { .. } => DirtyMask::empty(),
    }
}

/// Emits an atomic game event, updates turn-scoped history, invokes [`crate::triggers::on_event`],
/// and records `targeted_by_this_turn` and `first_upkeep_seen` **after** trigger matching
/// (matching TS `events.ts:546` and `triggerStacking.ts:370–378`).
pub fn emit_event(state: &mut GameState, kind: AtomicEventKind) -> u64 {
    emit_event_with_granted(state, kind, &[])
}

pub(crate) fn emit_event_with_granted(
    state: &mut GameState,
    kind: AtomicEventKind,
    single_leave_granted: &[GrantedTriggerLki],
) -> u64 {
    // CR 119.9 & CR 120.8: zero life change or zero damage is not an event.
    match &kind {
        AtomicEventKind::LifeChange { delta: 0, .. } => return state.event_seq(),
        AtomicEventKind::DamageDealt { amount: 0, .. } => return state.event_seq(),
        _ => {}
    }

    let seq = state.bump_event_seq();
    let mask = dirty_mask_for_event(&kind);

    // Pre-trigger turn-history updates (counters & flags read during trigger matching / intervening-if).
    record_pre_trigger_history(state, &kind);

    if !mask.is_empty() {
        state.mark_dirty(mask);
    }

    let event = AtomicEvent { seq, kind };
    state.event_log_mut().push(event.clone());

    // Synchronously match triggers against the emitted event.
    crate::triggers::on_event_with_granted(state, &event, single_leave_granted);

    // Post-trigger turn-history updates: `targeted_by_this_turn` and `first_upkeep_seen` must
    // be recorded AFTER `triggers::on_event` so `first_time_targeted_by_you_this_turn` (Valiant)
    // and `YourFirstUpkeep` see their pre-event state during matching.
    record_post_trigger_history(state, &event.kind);

    seq
}

/// Maps every [`AtomicEventKind`] variant exhaustively to a `(tag, player)` counter key for
/// `first_time_each_turn` / `nth_this_turn` / `nth_event_this_turn`.
pub(crate) fn event_count_tag(kind: &AtomicEventKind) -> (u16, PlayerId) {
    match kind {
        AtomicEventKind::LifeChange { player, delta } if *delta > 0 => (1, *player),
        AtomicEventKind::LifeChange { player, delta } if *delta < 0 => (2, *player),
        AtomicEventKind::LifeChange { player, .. } => (83, *player),
        AtomicEventKind::DrawCard { player, .. } => (3, *player),
        AtomicEventKind::Cast {
            player,
            is_copy: false,
            ..
        } => (4, *player),
        AtomicEventKind::Cast {
            player,
            is_copy: true,
            ..
        }
        | AtomicEventKind::SpellCopied { player, .. } => (28, *player),
        AtomicEventKind::Discard { player, .. } => (5, *player),
        AtomicEventKind::CommittedCrime { player, .. } => (6, *player),
        AtomicEventKind::Sacrificed { player, .. } => (7, *player),
        AtomicEventKind::TokenCreated { player, .. } => (8, *player),
        AtomicEventKind::AttackDeclared { player, .. } => (9, *player),
        AtomicEventKind::BlockersDeclared { defender, .. } => (10, *defender),
        AtomicEventKind::LandPlayed { player, .. } => (11, *player),
        AtomicEventKind::BecomesCrewed { player, .. } => (12, *player),
        AtomicEventKind::BecomesSaddled { player, .. } => (13, *player),
        AtomicEventKind::BecomesTarget { controller, .. } => (14, *controller),
        AtomicEventKind::DamageDealt {
            source_controller, ..
        } => (15, *source_controller),
        AtomicEventKind::AbilityActivated { player, .. } => (16, *player),
        AtomicEventKind::Proliferated { player } => (17, *player),
        AtomicEventKind::Surveil { player, .. } => (18, *player),
        AtomicEventKind::Scry { player, .. } => (19, *player),
        AtomicEventKind::Explore { player, .. } => (20, *player),
        AtomicEventKind::Connive { player, .. } => (21, *player),
        AtomicEventKind::Investigate { player, .. } => (22, *player),
        AtomicEventKind::Forage { player } => (23, *player),
        AtomicEventKind::CollectEvidence { player, .. } => (24, *player),
        AtomicEventKind::Discover { player, .. } => (25, *player),
        AtomicEventKind::CoinFlipped { player, .. } => (26, *player),
        AtomicEventKind::DieRolled { player, .. } => (27, *player),
        AtomicEventKind::SpellCountered { by_controller, .. } => (29, *by_controller),
        AtomicEventKind::ZoneChange {
            from_player,
            to_player,
            ..
        } => (30, (*to_player).or(*from_player).unwrap_or(PlayerId(0))),
        AtomicEventKind::UnblockedAttackers { defender, .. } => (31, *defender),
        AtomicEventKind::AttackTriggered { controller, .. } => (32, *controller),
        AtomicEventKind::CombatDamageBatchPlayer { damaged_player, .. } => (33, *damaged_player),
        AtomicEventKind::PhaseStepEntered { active_player, .. } => (34, *active_player),
        AtomicEventKind::CounterChanged {
            previous_count,
            new_count,
            actor,
            ..
        } => {
            let pid = actor.unwrap_or(PlayerId(0));
            if *new_count > *previous_count {
                (35, pid)
            } else if *new_count < *previous_count {
                (36, pid)
            } else {
                (37, pid)
            }
        }
        AtomicEventKind::TapStateChanged {
            now_tapped, actor, ..
        } => (
            if *now_tapped { 38 } else { 39 },
            actor.unwrap_or(PlayerId(0)),
        ),
        AtomicEventKind::AttachmentChanged { attached, .. } => {
            (if *attached { 40 } else { 41 }, PlayerId(0))
        }
        AtomicEventKind::TappedForMana { player, .. } => (42, *player),
        AtomicEventKind::ManaAddedByLandAbility { player, .. } => (43, *player),
        AtomicEventKind::Transformed { .. } => (44, PlayerId(0)),
        AtomicEventKind::TurnedFaceUp { player, .. } => (45, *player),
        AtomicEventKind::PhasedIn { controller, .. } => (46, *controller),
        AtomicEventKind::BecameMonarch { player, .. } => (47, *player),
        AtomicEventKind::DayNightChanged { .. } => (48, PlayerId(0)),
        AtomicEventKind::PlayerLostGame { player } => (49, *player),
        AtomicEventKind::LibrarySearched { player } => (50, *player),
        AtomicEventKind::LibraryShuffled { player } => (51, *player),
        AtomicEventKind::EnergyGained { player, .. } => (52, *player),
        AtomicEventKind::CrewedVehicle { player, .. } => (53, *player),
        AtomicEventKind::SaddledMount { player, .. } => (54, *player),
        AtomicEventKind::Exerted { player, .. } => (55, *player),
        AtomicEventKind::Amass { player, .. } => (56, *player),
        AtomicEventKind::ManifestDread { player, .. } => (57, *player),
        AtomicEventKind::BecameMonstrous { player, .. } => (58, *player),
        AtomicEventKind::Exploited { player, .. } => (59, *player),
        AtomicEventKind::ManaExpended { player, .. } => (60, *player),
        AtomicEventKind::GraveyardLeaveBatch { owner, .. } => (61, *owner),
        AtomicEventKind::ClassLevelGained { controller, .. } => (62, *controller),
        AtomicEventKind::CaseSolved { controller, .. } => (63, *controller),
        AtomicEventKind::BecameRenowned { controller, .. } => (64, *controller),
        AtomicEventKind::DoorUnlocked { player, .. } => (65, *player),
        AtomicEventKind::RoomFullyUnlocked { player, .. } => (66, *player),
        AtomicEventKind::RingTempted { player, .. } => (67, *player),
        AtomicEventKind::DungeonRoomEntered { player, .. } => (68, *player),
        AtomicEventKind::DungeonCompleted { player } => (69, *player),
        AtomicEventKind::TookInitiative { player } => (70, *player),
        AtomicEventKind::Mutates { player, .. } => (71, *player),
        AtomicEventKind::SoulbondPaired { player, .. } => (72, *player),
        AtomicEventKind::HauntedCreatureDies { haunted_lki, .. } => (73, haunted_lki.controller),
        AtomicEventKind::FacedDilemma { player, .. } => (74, *player),
        AtomicEventKind::ExhaustActivated { player, .. } => (75, *player),
        AtomicEventKind::Evolved { player, .. } => (76, *player),
        AtomicEventKind::PlayersFinishedVoting { controller } => (77, *controller),
        AtomicEventKind::Clash { player, .. } => (78, *player),
        AtomicEventKind::SagaChapterResolved { controller, .. } => (79, *controller),
        AtomicEventKind::GiftGiven { player, .. } => (80, *player),
        AtomicEventKind::Foretell { player, .. } => (81, *player),
        AtomicEventKind::Plot { player, .. } => (82, *player),
    }
}

fn record_pre_trigger_history(state: &mut GameState, kind: &AtomicEventKind) {
    // Generic per-kind per-player event ordinals (used for `first_time_each_turn` / `nth_this_turn`).
    let (tag, player) = event_count_tag(kind);
    state.turn_history_mut().increment_event_count(tag, player);

    match kind {
        AtomicEventKind::ZoneChange {
            new_id,
            from,
            to,
            from_player,
            lki,
            ..
        } => {
            if *to == Zone::Graveyard
                && let Some(src_zone) = *from
                && src_zone != Zone::Battlefield
                && let Some(nid) = *new_id
            {
                let th = state.turn_history_mut();
                if !th
                    .put_into_graveyard_from_non_battlefield_this_turn
                    .contains(&nid)
                {
                    th.put_into_graveyard_from_non_battlefield_this_turn
                        .push(nid);
                }
            }
            if *from == Some(Zone::Battlefield) {
                let th = state.turn_history_mut();
                th.permanents_left_bf_this_turn = th.permanents_left_bf_this_turn.saturating_add(1);
                if *to == Zone::Graveyard {
                    th.permanents_put_into_gy_from_bf_this_turn = th
                        .permanents_put_into_gy_from_bf_this_turn
                        .saturating_add(1);
                }
                let mut is_creature_death = false;
                let mut is_nontoken = false;
                let mut death_ctrl = None;
                if *to == Zone::Graveyard
                    && let Some(snap) = lki
                    && snap.types.contains(CardTypeMask::CREATURE)
                {
                    is_creature_death = true;
                    is_nontoken = !snap.is_token;
                    death_ctrl = Some(snap.controller);
                    th.creatures_died_this_turn = th.creatures_died_this_turn.saturating_add(1);
                    if is_nontoken {
                        th.nontoken_creatures_died_this_turn =
                            th.nontoken_creatures_died_this_turn.saturating_add(1);
                    }
                    if !th
                        .creatures_died_under_control_this_turn
                        .contains(&snap.controller)
                    {
                        th.creatures_died_under_control_this_turn
                            .push(snap.controller);
                    }
                }
                if let Some(ctrl) = lki.as_ref().map(|s| s.controller).or(*from_player)
                    && let Some(p) = state.player_mut(ctrl)
                {
                    p.permanents_left_bf_this_turn =
                        p.permanents_left_bf_this_turn.saturating_add(1);
                }
                if is_creature_death
                    && let Some(ctrl) = death_ctrl
                    && let Some(p) = state.player_mut(ctrl)
                {
                    p.creatures_died_this_turn = p.creatures_died_this_turn.saturating_add(1);
                    if is_nontoken {
                        p.nontoken_creatures_died_this_turn =
                            p.nontoken_creatures_died_this_turn.saturating_add(1);
                    }
                }
            }
            if *from == Some(Zone::Graveyard)
                && *to != Zone::Graveyard
                && let Some(owner) = from_player
            {
                let th = state.turn_history_mut();
                if !th.cards_left_graveyard_this_turn.contains(owner) {
                    th.cards_left_graveyard_this_turn.push(*owner);
                }
            }
        }
        AtomicEventKind::AttackDeclared { player, attackers } => {
            let th = state.turn_history_mut();
            for a in attackers {
                th.creatures_attacked_this_turn.push((*player, a.attacker));
            }
        }
        AtomicEventKind::LifeChange { player, delta } => {
            if *delta > 0 {
                let d = *delta as u32;
                let th = state.turn_history_mut();
                TurnEventHistory::add_u32(&mut th.life_gained_this_turn, *player, d);
                TurnEventHistory::add_u16(&mut th.life_gain_events_this_turn, *player, 1);
                if let Some(p) = state.player_mut(*player) {
                    p.life_gained_this_turn = p.life_gained_this_turn.saturating_add(d);
                    p.life_gain_events_this_turn = p.life_gain_events_this_turn.saturating_add(1);
                }
            } else if *delta < 0 {
                let d = delta.unsigned_abs();
                let th = state.turn_history_mut();
                TurnEventHistory::add_u32(&mut th.life_lost_this_turn, *player, d);
                TurnEventHistory::add_u16(&mut th.life_loss_events_this_turn, *player, 1);
                if let Some(p) = state.player_mut(*player) {
                    p.life_lost_this_turn = p.life_lost_this_turn.saturating_add(d);
                    p.life_loss_events_this_turn = p.life_loss_events_this_turn.saturating_add(1);
                }
            }
        }
        AtomicEventKind::DrawCard {
            player,
            in_own_draw_step_ordinal,
            ..
        } => {
            let th = state.turn_history_mut();
            TurnEventHistory::add_u16(&mut th.cards_drawn_this_turn, *player, 1);
            if in_own_draw_step_ordinal.is_some() {
                TurnEventHistory::add_u16(&mut th.draw_step_draws_this_turn, *player, 1);
            }
            if let Some(p) = state.player_mut(*player) {
                p.cards_drawn_this_turn = p.cards_drawn_this_turn.saturating_add(1);
                if in_own_draw_step_ordinal.is_some() {
                    p.draw_step_draws_this_turn = p.draw_step_draws_this_turn.saturating_add(1);
                }
            }
        }
        AtomicEventKind::Cast {
            player, is_copy, ..
        } => {
            if !*is_copy {
                let th = state.turn_history_mut();
                TurnEventHistory::add_u16(&mut th.spells_cast_this_turn, *player, 1);
                if let Some(p) = state.player_mut(*player) {
                    p.spells_cast_this_turn = p.spells_cast_this_turn.saturating_add(1);
                }
            }
        }
        AtomicEventKind::Discard { player, .. } => {
            let th = state.turn_history_mut();
            TurnEventHistory::add_u16(&mut th.cards_discarded_this_turn, *player, 1);
            if let Some(p) = state.player_mut(*player) {
                p.cards_discarded_this_turn = p.cards_discarded_this_turn.saturating_add(1);
            }
        }
        AtomicEventKind::TokenCreated { player, .. } => {
            let th = state.turn_history_mut();
            TurnEventHistory::add_u16(&mut th.tokens_created_this_turn, *player, 1);
        }
        AtomicEventKind::Sacrificed { player, .. } => {
            let th = state.turn_history_mut();
            TurnEventHistory::add_u16(&mut th.sacrificed_this_turn, *player, 1);
        }
        AtomicEventKind::ManaExpended {
            player,
            amount_spent,
            ..
        } => {
            let th = state.turn_history_mut();
            TurnEventHistory::add_u32(&mut th.mana_expended_this_turn, *player, *amount_spent);
            if let Some(p) = state.player_mut(*player) {
                p.mana_expended_this_turn = p.mana_expended_this_turn.saturating_add(*amount_spent);
            }
        }
        AtomicEventKind::CommittedCrime { player, .. } => {
            let th = state.turn_history_mut();
            if !th.committed_crime_this_turn.contains(player) {
                th.committed_crime_this_turn.push(*player);
            }
            if let Some(p) = state.player_mut(*player) {
                p.committed_crime_this_turn = true;
            }
        }
        AtomicEventKind::DamageDealt {
            source,
            target,
            amount,
            is_combat,
            ..
        } => {
            let th = state.turn_history_mut();
            match *target {
                TargetRef::Object { id: victim } => {
                    if let Some(src) = *source
                        && !th.damaged_by_this_turn.contains(&(src, victim))
                    {
                        th.damaged_by_this_turn.push((src, victim));
                    }
                }
                TargetRef::Player { id: damaged_player } => {
                    th.damage_dealt_to_players_this_turn.push((
                        *source,
                        damaged_player,
                        *amount,
                        *is_combat,
                    ));
                    if let Some(p) = state.player_mut(damaged_player) {
                        p.damage_dealt_to_this_turn =
                            p.damage_dealt_to_this_turn.saturating_add(*amount);
                    }
                }
                TargetRef::Stack { .. } => {}
            }
        }
        AtomicEventKind::PhaseStepEntered { phase, .. } => {
            if *phase == GamePhase::BeginCombat {
                let th = state.turn_history_mut();
                th.combat_phases_this_turn = th.combat_phases_this_turn.saturating_add(1);
            }
        }
        AtomicEventKind::BecameMonarch { player, .. } => {
            state.set_monarch(Some(*player));
        }
        AtomicEventKind::TookInitiative { player } => {
            state.set_initiative(Some(*player));
        }
        AtomicEventKind::DayNightChanged { now_night, .. } => {
            state.set_day_night(Some(*now_night));
        }
        _ => {}
    }
}

fn record_post_trigger_history(state: &mut GameState, kind: &AtomicEventKind) {
    match kind {
        AtomicEventKind::BecomesTarget {
            target, controller, ..
        } => match target {
            TargetRef::Object { id } => {
                let th = state.turn_history_mut();
                if !th.targeted_by_this_turn.contains(&(*controller, *id)) {
                    th.targeted_by_this_turn.push((*controller, *id));
                }
                if let Some(obj_ctrl) = state.object(*id).map(|o| o.controller)
                    && let Some(p) = state.player_mut(obj_ctrl)
                    && !p.targeted_by_this_turn.contains(controller)
                {
                    p.targeted_by_this_turn.push(*controller);
                }
            }
            TargetRef::Player { id: target_pid } => {
                if let Some(p) = state.player_mut(*target_pid)
                    && !p.targeted_by_this_turn.contains(controller)
                {
                    p.targeted_by_this_turn.push(*controller);
                }
            }
            TargetRef::Stack { .. } => {}
        },
        AtomicEventKind::PhaseStepEntered {
            phase: GamePhase::Upkeep,
            active_player,
        } => {
            let th = state.turn_history_mut();
            if !th.first_upkeep_seen.contains(active_player) {
                th.first_upkeep_seen.push(*active_player);
            }
        }
        _ => {}
    }
}

/// Locked entry point called by `zones::finalize_zone_move` after every zone transition.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_zone_change(
    state: &mut GameState,
    old_id: ObjectId,
    new_id: Option<ObjectId>,
    from: Zone,
    to: Zone,
    from_player: Option<PlayerId>,
    to_player: Option<PlayerId>,
    lki: Option<&Arc<LkiSnapshot>>,
    ceased_to_exist: bool,
) -> u64 {
    emit_zone_change_with_granted(
        state,
        old_id,
        new_id,
        from,
        to,
        from_player,
        to_player,
        lki,
        ceased_to_exist,
        &[],
    )
}

/// Emits a `ZoneChange` event with pre-captured Layer-6 granted triggers for unbatched
/// single-object battlefield leaves (`CR 603.10a`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_zone_change_with_granted(
    state: &mut GameState,
    old_id: ObjectId,
    new_id: Option<ObjectId>,
    from: Zone,
    to: Zone,
    from_player: Option<PlayerId>,
    to_player: Option<PlayerId>,
    lki: Option<&Arc<LkiSnapshot>>,
    ceased_to_exist: bool,
    single_leave_granted: &[GrantedTriggerLki],
) -> u64 {
    emit_event_with_granted(
        state,
        AtomicEventKind::ZoneChange {
            object: old_id,
            new_id,
            from: Some(from),
            to,
            from_player,
            to_player,
            ceased_to_exist,
            lki: lki.cloned(),
        },
        single_leave_granted,
    )
}

/// Opens a top-level action event bracket. Clears `event_log` and transient batch keys when
/// nested depth drops back to `0` in [`end_action`].
pub fn begin_action(state: &mut GameState) {
    let d = state.event_action_depth();
    state.set_event_action_depth(d.saturating_add(1));
}

/// Closes a top-level action event bracket.
pub fn end_action(state: &mut GameState) {
    let d = state.event_action_depth();
    if d <= 1 {
        state.set_event_action_depth(0);
        state.event_log_mut().clear();
        if state.defer_flush() == 0 {
            state.batch_triggered_keys_mut().clear();
        }
    } else {
        state.set_event_action_depth(d - 1);
    }
}

/// Returns all atomic events recorded in the current action bracket.
#[inline]
pub fn peek_events(state: &GameState) -> &[AtomicEvent] {
    state.event_log()
}

/// Drains and returns all atomic events recorded in the current action bracket.
#[inline]
pub fn drain_events(state: &mut GameState) -> Vec<AtomicEvent> {
    std::mem::take(state.event_log_mut())
}

/// Extracts Layer-6 granted triggered abilities for `id` from an already-evaluated [`LayerCache`].
pub(crate) fn capture_granted_triggers_from_cache(
    db: &mtg_ir::CardDatabase,
    cache: &crate::state::LayerCache,
    id: ObjectId,
    lost_all_abilities: bool,
) -> Vec<GrantedTriggerLki> {
    let mut granted_triggers = Vec::new();
    if !lost_all_abilities && let Some(extras) = cache.extra(id) {
        for g in &extras.granted_abilities {
            match g {
                GrantedAbilityEntry::CardAbility {
                    card_def,
                    face_index,
                    ability_index,
                    grant_source,
                    grant_controller,
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
                    if let Some(card) = db.get(*card_def)
                        && let Some(face) = card.faces.get(*face_index as usize)
                        && let Some(AbilityDef::Triggered(_)) = face.abilities.get(*ability_index)
                    {
                        granted_triggers.push(GrantedTriggerLki::CardAbility {
                            card_def: *card_def,
                            face_index: *face_index,
                            ability_index: *ability_index,
                            grant_source: *grant_source,
                            grant_controller: *grant_controller,
                            donor_object: *donor_object,
                            donor_component_index: *donor_component_index,
                            granter_origin: *granter_origin,
                            granter_card_def: *granter_card_def,
                            granter_face_index: *granter_face_index,
                            granter_component_index: *granter_component_index,
                            granter_ability_index: *granter_ability_index,
                            source_effect_id: *source_effect_id,
                        });
                    }
                }
                GrantedAbilityEntry::Inline {
                    ability,
                    grant_source,
                    grant_controller,
                    grant_origin,
                    grant_card_def,
                    source_face_index,
                    source_component_index,
                    source_ability_index,
                    source_effect_id,
                    ..
                } => {
                    if let AbilityDef::Triggered(trig) = ability.as_ref() {
                        granted_triggers.push(GrantedTriggerLki::Inline {
                            ability: Box::new(trig.clone()),
                            grant_source: *grant_source,
                            grant_controller: *grant_controller,
                            grant_origin: *grant_origin,
                            grant_card_def: *grant_card_def,
                            source_face_index: *source_face_index,
                            source_component_index: *source_component_index,
                            source_ability_index: *source_ability_index,
                            source_effect_id: *source_effect_id,
                        });
                    }
                }
                GrantedAbilityEntry::IntrinsicBasicMana { .. }
                | GrantedAbilityEntry::DynamicWard { .. } => {}
            }
        }
    }
    granted_triggers
}

/// Captures Layer-6 granted triggered abilities for a single live battlefield object
/// right before it leaves the battlefield (`CR 603.10a`).
pub(crate) fn capture_granted_triggers_for_object(
    state: &GameState,
    id: ObjectId,
) -> Vec<GrantedTriggerLki> {
    let Some(obj) = state.object(id) else {
        return Vec::new();
    };
    if obj.zone != Zone::Battlefield || obj.is_phased_out() {
        return Vec::new();
    }
    let cache = state.evaluate_board();
    let Some(eval_obj) = cache.get(id) else {
        return Vec::new();
    };
    capture_granted_triggers_from_cache(state.db(), cache, id, eval_obj.lost_all_abilities)
}

/// Captures pre-batch Layer 1–7d [`LkiSnapshot`] + [`EvaluatedObject`] + Layer-6 granted triggers
/// for all `leaving_ids` currently on `Zone::Battlefield` **before** any object leaves (CR 603.10a).
/// Defers trigger flushing until [`end_simultaneous_batch`].
pub fn begin_simultaneous_batch(
    state: &mut GameState,
    leaving_ids: &[ObjectId],
) -> BatchLkiGuardToken {
    let mut entries = Vec::with_capacity(leaving_ids.len());
    {
        // Evaluate board once while all leaving objects & anthems/lords are still on battlefield.
        let cache = state.evaluate_board();
        for &id in leaving_ids {
            if entries.iter().any(|e: &BatchLkiEntry| e.id == id) {
                continue;
            }
            let Some(obj) = state.object(id) else {
                continue;
            };
            if obj.zone != Zone::Battlefield || obj.is_phased_out() {
                continue;
            }
            let Some(eval_obj) = cache.get(id).cloned() else {
                continue;
            };
            let lki = build_batch_lki_snapshot(state, id, &eval_obj);
            let granted_triggers = capture_granted_triggers_from_cache(
                state.db(),
                cache,
                id,
                eval_obj.lost_all_abilities,
            );
            entries.push(BatchLkiEntry {
                id,
                lki: Arc::new(lki),
                eval: eval_obj,
                granted_triggers,
            });
        }
    }

    state.batch_lki_scopes_mut().push(entries);
    let defer = state.defer_flush();
    state.set_defer_flush(defer.saturating_add(1));
    BatchLkiGuardToken {
        scope_depth: state.batch_lki_scopes().len(),
    }
}

/// Pops the current simultaneous batch LKI scope, clears batch deduplication keys,
/// and flushes pending triggers in APNAP order if no outer batch remains open.
pub fn end_simultaneous_batch(state: &mut GameState) {
    let _ = state.batch_lki_scopes_mut().pop();
    let defer = state.defer_flush();
    if defer <= 1 {
        state.set_defer_flush(0);
        state.batch_triggered_keys_mut().clear();
        crate::triggers::flush_triggers(state);
    } else {
        state.set_defer_flush(defer - 1);
    }
}

/// Executes `f` inside a [`begin_simultaneous_batch`] / [`end_simultaneous_batch`] scope,
/// ensuring cleanup and trigger flushing even if `f` returns an error.
pub fn with_simultaneous_batch<R>(
    state: &mut GameState,
    leaving_ids: &[ObjectId],
    f: impl FnOnce(&mut GameState) -> R,
) -> R {
    let _token = begin_simultaneous_batch(state, leaving_ids);
    let out = f(state);
    end_simultaneous_batch(state);
    out
}

/// Queries the pre-batch LKI snapshot and evaluated characteristics for `id` if an active
/// simultaneous batch captured it before moving (CR 603.10a).
#[inline]
pub fn pre_batch_lki(
    state: &GameState,
    id: ObjectId,
) -> Option<(&Arc<LkiSnapshot>, &EvaluatedObject)> {
    for scope in state.batch_lki_scopes().iter().rev() {
        if let Some(entry) = scope.iter().find(|e| e.id == id) {
            return Some((&entry.lki, &entry.eval));
        }
    }
    None
}

/// Builds a full [`LkiSnapshot`] for `id` from `eval_obj` while `id` is still on the battlefield.
pub(crate) fn build_batch_lki_snapshot(
    state: &GameState,
    id: ObjectId,
    eval_obj: &EvaluatedObject,
) -> LkiSnapshot {
    let obj = state.object(id).expect("object checked before LKI capture");
    let mut attached_ids: SmallVec<[ObjectId; 2]> = SmallVec::new();
    if obj.zone == Zone::Battlefield {
        for p in state.players() {
            for &cand in &p.battlefield {
                if state
                    .object(cand)
                    .is_some_and(|o| o.attached_to == Some(id))
                {
                    attached_ids.push(cand);
                }
            }
        }
    }

    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let base_name = if let Some(ov) = &eval_obj.name_override {
        ov.clone()
    } else if !obj.is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        evaluate_token_spec_base(spec, copiable).0
    } else if let Some(opts) = face_opts_for_object(state, id)
        && let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        resolved.name
    } else {
        String::new()
    };

    LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name: base_name,
        owner: obj.owner,
        controller: eval_obj.controller,
        last_zone: obj.zone,
        pt: eval_obj.pt,
        types: eval_obj.types,
        subtypes: eval_obj.subtypes.clone(),
        supertypes: eval_obj.supertypes,
        colors: eval_obj.colors,
        mana_value: eval_obj.mana_value,
        keywords: eval_obj.keywords.clone(),
        counters: state.counters(id).iter().copied().collect(),
        damage_marked: obj.damage_marked,
        is_token: obj.is_token(),
        is_commander: obj.is_commander(),
        tapped: obj.flags.contains(ObjectFlags::TAPPED),
        phased_out: obj.is_phased_out(),
        face_down: obj.is_face_down(),
        lost_all_abilities: eval_obj.lost_all_abilities,
        has_all_creature_types: eval_obj.has_all_creature_types,
        attached_ids,
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.active_face_index(),
    }
}

/// Captures a best-effort [`LkiSnapshot`] for any live object (using `evaluate_board()` if on battlefield).
pub(crate) fn snapshot_for_live_object(state: &GameState, id: ObjectId) -> Option<LkiSnapshot> {
    let obj = state.object(id)?;
    if obj.zone == Zone::Battlefield
        && let Some(eval_obj) = state.evaluate_board().get(id)
    {
        return Some(build_batch_lki_snapshot(state, id, eval_obj));
    }
    let rare = state.rare(id);
    let copiable = rare.and_then(|r| r.copiable.as_deref());
    let (name, pt, types, subtypes, supertypes, colors, mana_value, keywords) = if !obj
        .is_face_down()
        && copiable.is_none_or(|c| c.card_def == TOKEN_CARD_DEF_ID)
        && let Some(spec) = rare.and_then(|r| r.token_spec.as_deref())
    {
        let (nm, ev) = evaluate_token_spec_base(spec, copiable);
        (
            nm,
            ev.pt,
            ev.types,
            ev.subtypes,
            ev.supertypes,
            ev.colors,
            ev.mana_value,
            ev.keywords,
        )
    } else if let Some(opts) = face_opts_for_object(state, id)
        && let Some(resolved) = resolve_object_faces(state.db(), obj.card_def, &opts)
    {
        let mut kws: SmallVec<[KeywordId; 4]> = resolved.extra_keywords.iter().copied().collect();
        let ctx = crate::eval::EvalContext::for_source(obj.controller, id);
        for ab in active_abilities_for_object(state, id, &ctx) {
            if let AbilityDef::Keyword(k) = ab.ability
                && !kws.contains(&k.keyword.keyword)
            {
                kws.push(k.keyword.keyword);
            }
        }
        (
            resolved.name,
            resolved.pt,
            resolved.types,
            resolved.subtypes,
            resolved.supertypes,
            resolved.colors,
            resolved.mana_value,
            kws,
        )
    } else {
        (
            String::new(),
            None,
            CardTypeMask::empty(),
            SmallVec::new(),
            SupertypeMask::empty(),
            ColorMask::empty(),
            0,
            SmallVec::new(),
        )
    };

    Some(LkiSnapshot {
        object_id: id,
        card_def: obj.card_def,
        name,
        owner: obj.owner,
        controller: obj.controller,
        last_zone: obj.zone,
        pt,
        types,
        subtypes,
        supertypes,
        colors,
        mana_value,
        keywords,
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
        chosen_color: rare.and_then(|r| r.chosen_color),
        chosen_subtype: rare.and_then(|r| r.chosen_subtype),
        chosen_card_name: rare.and_then(|r| r.chosen_card_name.clone()),
        active_face_index: obj.active_face_index(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::DeckSpec;
    use crate::config::GameConfig;
    use crate::zones::{MoveRawOptions, move_object_raw};
    use mtg_ir::{
        AbilityDef, Amount, CardDatabase, CardDef, CardLayout, CardTypeMask, Characteristics,
        ColorMask, Compare, CompareOp, EffectOp, EffectStep, EventDetail, EventFilter, EventKind,
        FaceDef, FilterFlags, IR_V2_VERSION, InternTables, IrV2Bundle, Layer, LayerMod, ManaCost,
        ObjectFilter, PlayerRef, PrintedPt, Selector, StaticAbility, StaticBody, StaticDef,
        SupertypeMask, TriggerDef, TriggerSubject, TriggeredAbility, ZoneMask,
    };

    fn make_event_test_bundle() -> Arc<CardDatabase> {
        // Card 0: Synth Wastes (Land)
        // Card 1: Synth Bear (2/2 Creature)
        // Card 2: Synth BuffLord (1/1 Creature, other creatures get +2/+2)
        // Card 3: Synth BigDeathWatch (Enchantment, whenever creature power >= 4 dies, gain 4 life)
        let cards = vec![
            CardDef {
                oracle_id: "synth_wastes_ev".into(),
                compiler_version: 46,
                text_hash: "0000".into(),
                name: "Synth Wastes".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth Wastes".into(),
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
            },
            CardDef {
                oracle_id: "synth_bear_ev".into(),
                compiler_version: 46,
                text_hash: "0001".into(),
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
            },
            CardDef {
                oracle_id: "synth_lord_ev".into(),
                compiler_version: 46,
                text_hash: "0002".into(),
                name: "Synth BuffLord".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth BuffLord".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{2}{G}"),
                        types: CardTypeMask::CREATURE,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::G,
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
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7c,
                                affected: mtg_ir::Affected::Filter {
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
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            },
            CardDef {
                oracle_id: "synth_watch_ev".into(),
                compiler_version: 46,
                text_hash: "0003".into(),
                name: "Synth BigDeathWatch".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Synth BigDeathWatch".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::parse("{B}"),
                        types: CardTypeMask::ENCHANTMENT,
                        subtypes: SmallVec::new(),
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::B,
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Triggered(TriggeredAbility {
                        trigger: TriggerDef {
                            event: EventKind::ZoneChange,
                            filter: EventFilter {
                                object: Some(TriggerSubject::Filter {
                                    filter: Box::new(ObjectFilter {
                                        types_any: CardTypeMask::CREATURE,
                                        power: Some(Compare {
                                            op: CompareOp::Ge,
                                            value: Box::new(Amount::Const { n: 4 }),
                                        }),
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
                            op: EffectOp::GainLife {
                                set_life_total: false,
                            },
                            subject: Selector::PlayerRef {
                                player: PlayerRef::You,
                            },
                            actor: None,
                            amount: Some(Box::new(Amount::Const { n: 4 })),
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
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            },
        ];

        Arc::new(
            CardDatabase::from_bundle(IrV2Bundle {
                ir_version: IR_V2_VERSION,
                source_compiler_version: 46,
                tables: InternTables::default(),
                cards,
                decks: vec![],
            })
            .unwrap(),
        )
    }

    fn setup_state(db: Arc<CardDatabase>) -> GameState {
        let deck = DeckSpec {
            commanders: vec![],
            mainboard: vec![CardDefId(0); 12],
            sideboard: vec![],
        };
        let mut state =
            GameState::new(db, GameConfig::standard_1v1(), &[deck.clone(), deck], 11).unwrap();
        state.set_decision(crate::action::DecisionState::Priority);
        state
    }

    #[test]
    fn emit_event_selective_dirty_mask_and_zero_suppress_cr119_9() {
        let db = make_event_test_bundle();
        let mut state = setup_state(db);
        let _ = state.evaluate_board();
        assert!(state.has_cached_layer_eval());

        // PhaseStepEntered(Main1) has DirtyMask::empty() and must not invalidate LayerCache.
        let s1 = emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Main1,
                active_player: PlayerId(0),
            },
        );
        assert_eq!(s1, 1);
        assert!(state.has_cached_layer_eval());

        // CR 119.9 / 120.8: zero life change and zero damage emit nothing.
        let s_zero_life = emit_event(
            &mut state,
            AtomicEventKind::LifeChange {
                player: PlayerId(0),
                delta: 0,
            },
        );
        assert_eq!(s_zero_life, 1);
        let s_zero_dmg = emit_event(
            &mut state,
            AtomicEventKind::DamageDealt {
                source: None,
                source_controller: PlayerId(0),
                target: TargetRef::Player { id: PlayerId(1) },
                amount: 0,
                is_combat: false,
            },
        );
        assert_eq!(s_zero_dmg, 1);
        assert_eq!(peek_events(&state).len(), 1);
    }

    #[test]
    fn action_brackets_and_targeted_by_recorded_after_triggers() {
        let db = make_event_test_bundle();
        let mut state = setup_state(db);
        let target = state.players()[0].hand[0];

        begin_action(&mut state);
        let seq = emit_event(
            &mut state,
            AtomicEventKind::BecomesTarget {
                target: TargetRef::Object { id: target },
                by_spell: true,
                controller: PlayerId(0),
                stack_item: None,
            },
        );
        assert_eq!(seq, 1);
        assert_eq!(peek_events(&state).len(), 1);
        assert!(
            state
                .turn_history()
                .was_targeted_by_this_turn(PlayerId(0), target)
        );
        let drained = drain_events(&mut state);
        assert_eq!(drained.len(), 1);
        assert!(peek_events(&state).is_empty());
        end_action(&mut state);
    }

    #[test]
    fn simultaneous_batch_captures_pre_batch_lki_even_when_lord_leaves_first() {
        let db = make_event_test_bundle();
        let mut state = setup_state(db);

        let ts1 = state.allocate_timestamp();
        let bear_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(1),
                PlayerId(0),
                Zone::Hand,
                ts1,
                ObjectFlags::empty(),
            ),
            None,
        );
        let ts2 = state.allocate_timestamp();
        let lord_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(2),
                PlayerId(0),
                Zone::Hand,
                ts2,
                ObjectFlags::empty(),
            ),
            None,
        );
        let ts3 = state.allocate_timestamp();
        let watch_id = state.insert_object(
            crate::state::GameObject::new(
                CardDefId(3),
                PlayerId(0),
                Zone::Hand,
                ts3,
                ObjectFlags::empty(),
            ),
            None,
        );
        state
            .player_mut(PlayerId(0))
            .unwrap()
            .hand
            .extend([bear_id, lord_id, watch_id]);

        let bear_bf = move_object_raw(
            &mut state,
            bear_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let lord_bf = move_object_raw(
            &mut state,
            lord_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();
        let _watch_bf = move_object_raw(
            &mut state,
            watch_id,
            Zone::Battlefield,
            MoveRawOptions::default(),
            None,
        )
        .unwrap()
        .new_id
        .unwrap();

        // Sanity check: while both are on BF, Bear is 4/4.
        assert_eq!(
            state.evaluate_board().get(bear_bf).and_then(|e| e.pt),
            Some(crate::view::PowerToughness {
                power: 4,
                toughness: 4
            })
        );

        with_simultaneous_batch(&mut state, &[lord_bf, bear_bf], |st| {
            // Move Lord FIRST (removing the +2/+2 buff from the board).
            let (lord_lki, lord_eval) = pre_batch_lki(st, lord_bf).unwrap();
            let lord_lki = Arc::clone(lord_lki);
            let lord_eval = lord_eval.clone();
            let lord_raw = move_object_raw(
                st,
                lord_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                Some(&lord_eval),
            )
            .unwrap();
            emit_zone_change(
                st,
                lord_raw.old_id,
                lord_raw.new_id,
                Zone::Battlefield,
                Zone::Graveyard,
                Some(PlayerId(0)),
                Some(PlayerId(0)),
                Some(&lord_lki),
                false,
            );

            // Now move Bear AFTER Lord left — pre_batch_lki MUST still report Bear as 4/4!
            let (bear_lki, bear_eval) = pre_batch_lki(st, bear_bf).unwrap();
            assert_eq!(
                bear_lki.pt,
                Some(crate::view::PowerToughness {
                    power: 4,
                    toughness: 4
                })
            );
            let bear_lki = Arc::clone(bear_lki);
            let bear_eval = bear_eval.clone();
            let bear_raw = move_object_raw(
                st,
                bear_bf,
                Zone::Graveyard,
                MoveRawOptions::default(),
                Some(&bear_eval),
            )
            .unwrap();
            emit_zone_change(
                st,
                bear_raw.old_id,
                bear_raw.new_id,
                Zone::Battlefield,
                Zone::Graveyard,
                Some(PlayerId(0)),
                Some(PlayerId(0)),
                Some(&bear_lki),
                false,
            );
        });

        // BigDeathWatch saw Bear die with power >= 4 and queued onto the stack!
        assert_eq!(state.stack().len(), 1);
    }

    #[test]
    fn first_upkeep_seen_recorded_post_trigger_and_event_count_tag_exhaustive() {
        let db = make_event_test_bundle();
        let mut state = setup_state(db);
        assert!(state.turn_history().first_upkeep_seen.is_empty());

        emit_event(
            &mut state,
            AtomicEventKind::PhaseStepEntered {
                phase: GamePhase::Upkeep,
                active_player: PlayerId(0),
            },
        );
        assert_eq!(
            state.turn_history().first_upkeep_seen.as_slice(),
            &[PlayerId(0)]
        );

        // Previously untracked kinds (e.g. TapStateChanged, ZoneChange) increment turn_history deterministically.
        let perm = state.players()[0].hand[0];
        emit_event(
            &mut state,
            AtomicEventKind::TapStateChanged {
                object: perm,
                now_tapped: true,
                actor: Some(PlayerId(0)),
            },
        );
        emit_event(
            &mut state,
            AtomicEventKind::TapStateChanged {
                object: perm,
                now_tapped: true,
                actor: Some(PlayerId(0)),
            },
        );
        let (tap_tag, tap_player) = event_count_tag(&AtomicEventKind::TapStateChanged {
            object: perm,
            now_tapped: true,
            actor: Some(PlayerId(0)),
        });
        assert_eq!(state.turn_history().event_count(tap_tag, tap_player), 2);

        // Cast { is_copy: true } uses a distinct tag from Cast { is_copy: false }.
        let (cast_tag, _) = event_count_tag(&AtomicEventKind::Cast {
            player: PlayerId(0),
            object: perm,
            stack_item: StackItemId(1),
            is_copy: false,
            targets: vec![],
            mana_spent: 0,
            colored_mana_spent: 0,
            kicked: false,
            from_zone: Zone::Hand,
        });
        let (copy_tag, _) = event_count_tag(&AtomicEventKind::Cast {
            player: PlayerId(0),
            object: perm,
            stack_item: StackItemId(2),
            is_copy: true,
            targets: vec![],
            mana_spent: 0,
            colored_mana_spent: 0,
            kicked: false,
            from_zone: Zone::Stack,
        });
        assert_ne!(cast_tag, copy_tag);
    }
}
