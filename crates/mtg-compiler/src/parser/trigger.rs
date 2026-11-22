//! Trigger header, intervening-if / inline-while condition, and runtime parity compilation
//! (Milestone M3).

use mtg_ir::{
    Amount, AttachmentOwnerTarget, AttackedTargetFilter, CardTypeMask, ColorMask, Compare,
    CompareOp, Condition, CounterId, CounterTarget, DamageRecipient, DamageTargetKind,
    DayNightDirection, DuringTurnKind, EventDetail, EventFilter, EventKind, FilterFlags, GamePhase,
    KeywordId, ManaColor, NamedAltCostKind, ObjectFilter, ObjectPredicate,
    OpponentComparisonMetric, OpponentMetricKind, PhaseTurnFilter, PlayerFilter, PlayerRef,
    PlayerRelation, RelativeControl, Span, SpellDelayedDuration, SpellsLastTurnMode, StatKind,
    SubtypeId, SupertypeMask, TargetedByKind, TriggerDef, TriggerScope, TriggerSubject, Zone,
    ZoneMask, ZoneScopeWhose,
};
use smallvec::SmallVec;

use crate::intern::Interner;
use crate::lexer::tokenize;
use crate::parser::ParseCtx;
use crate::parser::cost::lookup_counter_id;
use crate::parser::filter::{
    FilterOpts, NullScope, parse_multi_or_object_phrase, parse_object_phrase, singular_subtype,
};
use crate::parser::value::{parse_condition, parse_small_number};

pub use crate::parser::effect::interpretable_trigger;

/// Result of parsing a trigger header (`When/Whenever/At ...[, if ...]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriggerParse {
    pub trigger: Option<TriggerDef>,
    pub condition: Option<Condition>,
    /// Byte offset in caller's text where effect text starts (relative to `offset`).
    pub effect_start: u32,
    /// Spans in the trigger head or condition that failed to parse.
    pub unparsed: Vec<Span>,
}

/// Extra activation timing/frequency restriction parsed alongside `"Activate only if ..."`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivatedExtraRestriction {
    AsASorcery,
    OnceEachTurn,
    DuringYourTurn,
    DuringYourUpkeep,
    DuringCombat,
}

/// Result of parsing `"Activate only if <cond>[ and only <restriction>]."`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivatedIfConditionParse {
    pub condition: Condition,
    pub extra: Option<ActivatedExtraRestriction>,
}

// -----------------------------------------------------------------------------
// Zero-regex ASCII & slice helpers
// -----------------------------------------------------------------------------

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[inline]
fn starts_with_word_ci(haystack: &str, word: &str) -> bool {
    let hb = haystack.as_bytes();
    let wb = word.as_bytes();
    hb.len() >= wb.len()
        && hb[..wb.len()].eq_ignore_ascii_case(wb)
        && (hb.len() == wb.len() || !is_word_byte(hb[wb.len()]))
}

#[inline]
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let sb = s.as_bytes();
    let pb = prefix.as_bytes();
    if sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb) {
        Some(&s[pb.len()..])
    } else {
        None
    }
}

#[inline]
fn strip_prefix_word_ci<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = strip_prefix_ci(s, word)?;
    match rest.as_bytes().first() {
        None => Some(rest),
        Some(b) if b.is_ascii_whitespace() => Some(rest.trim_start()),
        _ => None,
    }
}

#[inline]
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let sb = s.as_bytes();
    let pb = suffix.as_bytes();
    if sb.len() >= pb.len() && sb[sb.len() - pb.len()..].eq_ignore_ascii_case(pb) {
        Some(&s[..sb.len() - pb.len()])
    } else {
        None
    }
}

#[inline]
fn strip_suffix_ws_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let before = strip_suffix_ci(s, suffix)?;
    if before.is_empty() {
        return None;
    }
    let last = *before.as_bytes().last()?;
    if last.is_ascii_whitespace() {
        Some(before.trim_end())
    } else {
        None
    }
}

#[inline]
fn contains_word_ci(haystack: &str, word: &str) -> bool {
    let hb = haystack.as_bytes();
    let wb = word.as_bytes();
    if wb.is_empty() || hb.len() < wb.len() {
        return false;
    }
    for i in 0..=hb.len() - wb.len() {
        if hb[i..i + wb.len()].eq_ignore_ascii_case(wb) {
            let left_ok = i == 0 || !is_word_byte(hb[i - 1]);
            let right_ok = i + wb.len() == hb.len() || !is_word_byte(hb[i + wb.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

/// Finds all byte ranges `(start, end)` of `sep_word` bounded by ASCII whitespace on both sides.
fn find_ws_word_ci_all(haystack: &str, sep_word: &str) -> Vec<(usize, usize)> {
    let hb = haystack.as_bytes();
    let wb = sep_word.as_bytes();
    let mut out = Vec::new();
    if hb.len() < wb.len() + 2 {
        return out;
    }
    let mut i = 0;
    while i + wb.len() < hb.len() {
        if hb[i].is_ascii_whitespace() {
            let ws_start = i;
            let mut j = i + 1;
            while j < hb.len() && hb[j].is_ascii_whitespace() {
                j += 1;
            }
            if j + wb.len() < hb.len()
                && hb[j..j + wb.len()].eq_ignore_ascii_case(wb)
                && hb[j + wb.len()].is_ascii_whitespace()
            {
                let mut k = j + wb.len() + 1;
                while k < hb.len() && hb[k].is_ascii_whitespace() {
                    k += 1;
                }
                out.push((ws_start, k));
                i = j + wb.len();
                continue;
            }
        }
        i += 1;
    }
    out
}

fn split_once_ws_word_ci<'a>(s: &'a str, word: &str) -> Option<(&'a str, &'a str)> {
    let hits = find_ws_word_ci_all(s, word);
    let (start, end) = *hits.first()?;
    Some((s[..start].trim(), s[end..].trim()))
}

fn rsplit_once_ws_word_ci<'a>(s: &'a str, word: &str) -> Option<(&'a str, &'a str)> {
    let hits = find_ws_word_ci_all(s, word);
    let (start, end) = *hits.last()?;
    Some((s[..start].trim(), s[end..].trim()))
}

fn split_once_ws_phrase_ci<'a>(s: &'a str, phrase: &str) -> Option<(&'a str, &'a str)> {
    let sb = s.as_bytes();
    let pb = phrase.as_bytes();
    if sb.len() < pb.len() + 2 {
        return None;
    }
    let mut i = 0usize;
    while i + pb.len() < sb.len() {
        if sb[i].is_ascii_whitespace() {
            let mut j = i + 1;
            while j < sb.len() && sb[j].is_ascii_whitespace() {
                j += 1;
            }
            if j + pb.len() < sb.len()
                && sb[j..j + pb.len()].eq_ignore_ascii_case(pb)
                && sb[j + pb.len()].is_ascii_whitespace()
            {
                let left = s[..i].trim();
                let right = s[j + pb.len()..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left, right));
                }
            }
        }
        i += 1;
    }
    None
}

fn normalize_negated_adjective_commas(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains(", ") {
        return std::borrow::Cow::Borrowed(s);
    }
    let parts: Vec<&str> = s.split(", ").collect();
    if parts.len() < 2 {
        return std::borrow::Cow::Borrowed(s);
    }
    let is_neg_word = |w: &str| w.len() > 3 && w.as_bytes()[..3].eq_ignore_ascii_case(b"non");
    let mut needs_rewrite = false;
    for idx in 1..parts.len() {
        let prev_last = parts[idx - 1].split_whitespace().last().unwrap_or("");
        let next_first = parts[idx].split_whitespace().next().unwrap_or("");
        if is_neg_word(prev_last) && is_neg_word(next_first) {
            needs_rewrite = true;
            break;
        }
    }
    if !needs_rewrite {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    out.push_str(parts[0]);
    for idx in 1..parts.len() {
        let prev_last = parts[idx - 1].split_whitespace().last().unwrap_or("");
        let next_first = parts[idx].split_whitespace().next().unwrap_or("");
        if is_neg_word(prev_last) && is_neg_word(next_first) {
            out.push(' ');
        } else {
            out.push_str(", ");
        }
        out.push_str(parts[idx]);
    }
    std::borrow::Cow::Owned(out)
}

#[inline]
fn normalize_apostrophes(s: &str) -> String {
    if s.contains('’') || s.contains('‘') {
        s.replace(['’', '‘'], "'")
    } else {
        s.to_string()
    }
}

#[inline]
fn const_amount(n: i32) -> Box<Amount> {
    Box::new(Amount::Const { n })
}

#[inline]
fn parse_small_i32(raw: &str) -> Option<i32> {
    parse_small_number(raw.trim())
}

#[inline]
fn parse_small_u16(raw: &str) -> Option<u16> {
    let v = parse_small_number(raw.trim())?;
    u16::try_from(v).ok()
}

fn parse_extended_ordinal(raw: &str) -> Option<u16> {
    let low = raw.trim().to_ascii_lowercase();
    let named = match low.as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "fifth" => Some(5),
        "sixth" => Some(6),
        "seventh" => Some(7),
        "eighth" => Some(8),
        "ninth" => Some(9),
        "tenth" => Some(10),
        "eleventh" => Some(11),
        "twelfth" => Some(12),
        "thirteenth" => Some(13),
        "fourteenth" => Some(14),
        "fifteenth" => Some(15),
        "sixteenth" => Some(16),
        "seventeenth" => Some(17),
        "eighteenth" => Some(18),
        "nineteenth" => Some(19),
        "twentieth" => Some(20),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(digits) = low.strip_suffix(suffix)
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(val) = digits.parse::<u16>()
            && val >= 1
        {
            return Some(val);
        }
    }
    None
}

fn parse_ordinal_123(raw: &str) -> Option<u16> {
    if raw.eq_ignore_ascii_case("first") {
        Some(1)
    } else if raw.eq_ignore_ascii_case("second") {
        Some(2)
    } else if raw.eq_ignore_ascii_case("third") {
        Some(3)
    } else {
        None
    }
}

fn parse_braced_mana_color(raw: &str) -> Option<ManaColor> {
    let inner = raw.trim().strip_prefix('{')?.strip_suffix('}')?;
    match inner.to_ascii_uppercase().as_str() {
        "W" => Some(ManaColor::W),
        "U" => Some(ManaColor::U),
        "B" => Some(ManaColor::B),
        "R" => Some(ManaColor::R),
        "G" => Some(ManaColor::G),
        "C" => Some(ManaColor::C),
        _ => None,
    }
}

fn parse_braced_wubrg(raw: &str) -> Option<ManaColor> {
    let c = parse_braced_mana_color(raw)?;
    if c == ManaColor::C { None } else { Some(c) }
}

fn parse_color_word_to_mana(raw: &str) -> Option<ManaColor> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "white" => Some(ManaColor::W),
        "blue" => Some(ManaColor::U),
        "black" => Some(ManaColor::B),
        "red" => Some(ManaColor::R),
        "green" => Some(ManaColor::G),
        _ => None,
    }
}

fn parse_color_word_or_colorless(raw: &str) -> Option<ColorMask> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "white" => Some(ColorMask::W),
        "blue" => Some(ColorMask::U),
        "black" => Some(ColorMask::B),
        "red" => Some(ColorMask::R),
        "green" => Some(ColorMask::G),
        "colorless" => Some(ColorMask::C),
        _ => None,
    }
}

/// Canonicalizes kicker tags (`"{1}{U}"` -> `"{1}{U}"`, `"foo"` -> `"foo"`).
pub fn canonical_kicker_tag(raw: &str) -> String {
    let t = raw.trim();
    if !t.is_empty() && t.starts_with('{') && t.ends_with('}') {
        let mut out = String::with_capacity(t.len());
        let mut i = 0;
        let bytes = t.as_bytes();
        let mut ok = true;
        while i < bytes.len() {
            if bytes[i] != b'{' {
                ok = false;
                break;
            }
            let Some(rel_end) = t[i + 1..].find('}') else {
                ok = false;
                break;
            };
            let inner = t[i + 1..i + 1 + rel_end].trim();
            if inner.is_empty() || inner.contains('{') {
                ok = false;
                break;
            }
            out.push('{');
            out.push_str(&inner.to_ascii_uppercase());
            out.push('}');
            i += rel_end + 2;
        }
        if ok && !out.is_empty() {
            return out;
        }
    }
    t.to_ascii_lowercase()
}

// -----------------------------------------------------------------------------
// Counter parsing
// -----------------------------------------------------------------------------

const COUNTER_STOP_WORDS: [&str; 12] = [
    "a", "an", "the", "no", "any", "more", "other", "that", "those", "these", "each", "all",
];

fn parse_trigger_counter(raw: &str, interner: &mut Interner) -> Option<CounterId> {
    let cleaned = raw
        .trim()
        .replace('−', "-")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if cleaned.is_empty() || cleaned == "another" || COUNTER_STOP_WORDS.contains(&cleaned.as_str())
    {
        return None;
    }
    lookup_counter_id(&cleaned, interner)
}

// -----------------------------------------------------------------------------
// EventFilterBuilder -> EventFilter / EventDetail (export-ir-v2.ts parity)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct EventFilterBuilder {
    object: Option<TriggerSubject>,
    or_self: bool,
    or_attached_to_self: bool,
    or_commander: bool,
    player: Option<PlayerFilter>,
    one_or_more: bool,
    during_turn: Option<DuringTurnKind>,
    during_step: Option<GamePhase>,
    during_combat: bool,
    during_main_phase: bool,
    first_time_each_turn: bool,
    nth_this_turn: Option<u16>,
    nth_event_this_turn: Option<u16>,
    attached_filter: Option<Box<ObjectFilter>>,
    watched_bound_object: bool,

    // ZoneChange
    from: ZoneMask,
    to: Option<Zone>,
    from_anywhere: bool,
    not_from_zone: Option<Zone>,
    not_to_zone: Option<Zone>,
    enters_untapped: bool,
    dealt_damage_by_self_this_turn: bool,

    // Cast / LandPlayed
    or_copy: bool,
    targets_self: bool,
    targets_only_self: bool,
    during_opponent_turn: bool,
    cast_from_not_hand: bool,
    cast_from_zone: Option<Zone>,
    was_kicked: bool,
    spell_ordinal: Option<i8>,
    spell_has_keyword: Option<KeywordId>,
    enchanted_player_casts: bool,

    // Combat
    alone: bool,
    saddled: bool,
    min_attackers: Option<u16>,
    dethrone_most_life: bool,
    training_greater_attacker: bool,
    blocked_attacker_filter: Option<Box<ObjectFilter>>,
    host: Option<Box<ObjectFilter>>,
    without_flanking: bool,
    attacked_target: Option<AttackedTargetFilter>,
    co_attacker_filter: Option<Box<ObjectFilter>>,
    co_attacker_min: Option<u16>,
    per_attacked_player: bool,
    attacking_with_filter: Option<Box<ObjectFilter>>,
    min_attacking_with_count: Option<u16>,
    attacking_total_power_min: Option<i32>,
    attack_enchanted_or_attacked_by_enchanted: bool,
    attacking_unblocked: bool,

    // Damage
    combat: Option<bool>,
    noncombat_only: bool,
    damage_to: Option<DamageRecipient>,
    damage_target_kind: Option<DamageTargetKind>,
    damage_player_relation: Option<RelativeControl>,
    damaged_player: Option<RelativeControl>,
    damage_target_filter: Option<Box<ObjectFilter>>,
    damage_target_enchanted_player: bool,
    single_source_damage_min: Option<i32>,
    combat_damage_batch_player: Option<RelativeControl>,

    // Phase
    phase: Option<GamePhase>,
    any_main_phase: bool,
    enchanted_controller_upkeep: bool,
    enchanted_player_upkeep: bool,
    enchanted_opponent_end_step: bool,
    phase_chosen_player: bool,
    turn_filter: Option<PhaseTurnFilter>,

    // Counter
    counter: Option<CounterId>,
    last_counter: bool,
    put_by_player: Option<RelativeControl>,
    nth_counter_on_self: Option<u16>,

    // BecomesTarget
    targeted_by: Option<TargetedByKind>,
    targeted_by_activated_ability: bool,
    first_time_targeted_by_you_this_turn: bool,
    targeting_source_filter: Option<Box<ObjectFilter>>,
    target_player: Option<RelativeControl>,

    // Attach
    host_self: bool,

    // Mana
    tapped_mana_color: Option<ManaColor>,
    tapped_mana_chosen_color: bool,

    // AbilityActivated
    is_loyalty_ability: bool,
    is_non_mana_ability: bool,
    ability_source: Option<TriggerSubject>,
    ability_source_controlled_by_event_player: bool,

    // Discard
    madness_exiled: bool,
    caused_by_opponent: bool,

    // Draw
    except_first_in_draw_step: bool,

    // Transform
    to_face_filter: Option<Box<ObjectFilter>>,

    // Tapped
    tapped_by_player: Option<RelativeControl>,

    // DieOrCoin
    die_roll_min: Option<u16>,
    die_roll_values: SmallVec<[u16; 4]>,
    won_coin_flip: bool,

    // State
    state_condition: Option<Box<Condition>>,

    // Misc
    amass_subtype: Option<SubtypeId>,
    expend_threshold: Option<u16>,
    class_level: Option<u8>,
    this_door_face_index: Option<u8>,
    day_night_direction: Option<DayNightDirection>,
    final_chapter: bool,
    won: Option<bool>,
}

impl EventFilterBuilder {
    fn into_event_filter(self, event: EventKind) -> EventFilter {
        let detail = match event {
            EventKind::ZoneChange => {
                if !self.from.is_empty()
                    || self.to.is_some()
                    || self.from_anywhere
                    || self.not_from_zone.is_some()
                    || self.not_to_zone.is_some()
                    || self.enters_untapped
                    || self.dealt_damage_by_self_this_turn
                {
                    Some(EventDetail::ZoneChange {
                        from: self.from,
                        to: self.to,
                        from_anywhere: self.from_anywhere,
                        not_from_zone: self.not_from_zone,
                        not_to_zone: self.not_to_zone,
                        enters_untapped: self.enters_untapped,
                        dealt_damage_by_self_this_turn: self.dealt_damage_by_self_this_turn,
                    })
                } else {
                    None
                }
            }
            EventKind::Cast => {
                if self.or_copy
                    || self.targets_self
                    || self.targets_only_self
                    || self.during_opponent_turn
                    || self.cast_from_not_hand
                    || self.cast_from_zone.is_some()
                    || self.was_kicked
                    || self.spell_ordinal.is_some()
                    || self.spell_has_keyword.is_some()
                    || self.enchanted_player_casts
                {
                    Some(EventDetail::Cast {
                        or_copy: self.or_copy,
                        targets_self: self.targets_self,
                        targets_only_self: self.targets_only_self,
                        during_opponent_turn: self.during_opponent_turn,
                        cast_from_not_hand: self.cast_from_not_hand,
                        cast_from_zone: self.cast_from_zone,
                        was_kicked: self.was_kicked,
                        spell_ordinal: self.spell_ordinal,
                        spell_has_keyword: self.spell_has_keyword,
                        enchanted_player_casts: self.enchanted_player_casts,
                    })
                } else {
                    None
                }
            }
            EventKind::LandPlayed => {
                if self.cast_from_not_hand || self.cast_from_zone.is_some() {
                    Some(EventDetail::LandPlayed {
                        cast_from_not_hand: self.cast_from_not_hand,
                        cast_from_zone: self.cast_from_zone,
                    })
                } else {
                    None
                }
            }
            EventKind::Attacks
            | EventKind::Blocks
            | EventKind::BecomesBlocked
            | EventKind::BecomesBlockedBy
            | EventKind::BlocksOrBecomesBlocked
            | EventKind::AttackedAndUnblocked => {
                if self.alone
                    || self.saddled
                    || self.min_attackers.is_some()
                    || self.dethrone_most_life
                    || self.training_greater_attacker
                    || self.blocked_attacker_filter.is_some()
                    || self.host.is_some()
                    || self.without_flanking
                    || self.attacked_target.is_some()
                    || self.co_attacker_filter.is_some()
                    || self.co_attacker_min.is_some()
                    || self.per_attacked_player
                    || self.attacking_with_filter.is_some()
                    || self.min_attacking_with_count.is_some()
                    || self.attacking_total_power_min.is_some()
                    || self.attack_enchanted_or_attacked_by_enchanted
                    || self.attacking_unblocked
                {
                    Some(EventDetail::Combat {
                        alone: self.alone,
                        saddled: self.saddled,
                        min_attackers: self.min_attackers,
                        dethrone_most_life: self.dethrone_most_life,
                        training_greater_attacker: self.training_greater_attacker,
                        blocked_attacker_filter: self.blocked_attacker_filter,
                        host: self.host,
                        without_flanking: self.without_flanking,
                        attacked_target: self.attacked_target,
                        co_attacker_filter: self.co_attacker_filter,
                        co_attacker_min: self.co_attacker_min,
                        per_attacked_player: self.per_attacked_player,
                        attacking_with_filter: self.attacking_with_filter,
                        min_attacking_with_count: self.min_attacking_with_count,
                        attacking_total_power_min: self.attacking_total_power_min,
                        attack_enchanted_or_attacked_by_enchanted: self
                            .attack_enchanted_or_attacked_by_enchanted,
                        attacking_unblocked: self.attacking_unblocked,
                    })
                } else {
                    None
                }
            }
            EventKind::DealsDamage
            | EventKind::DealtDamage
            | EventKind::CombatDamageBatchPlayer
            | EventKind::DamageReplacement => {
                if self.combat.is_some()
                    || self.noncombat_only
                    || self.damage_to.is_some()
                    || self.damage_target_kind.is_some()
                    || self.damage_player_relation.is_some()
                    || self.damaged_player.is_some()
                    || self.damage_target_filter.is_some()
                    || self.damage_target_enchanted_player
                    || self.single_source_damage_min.is_some()
                    || self.combat_damage_batch_player.is_some()
                {
                    Some(EventDetail::Damage {
                        combat: self.combat,
                        noncombat_only: self.noncombat_only,
                        damage_to: self.damage_to,
                        damage_target_kind: self.damage_target_kind,
                        damage_player_relation: self.damage_player_relation,
                        damaged_player: self.damaged_player,
                        damage_target_filter: self.damage_target_filter,
                        damage_target_enchanted_player: self.damage_target_enchanted_player,
                        single_source_damage_min: self.single_source_damage_min,
                        combat_damage_batch_player: self.combat_damage_batch_player,
                    })
                } else {
                    None
                }
            }
            EventKind::PhaseBegin => {
                if self.phase.is_some()
                    || self.any_main_phase
                    || self.enchanted_controller_upkeep
                    || self.enchanted_player_upkeep
                    || self.enchanted_opponent_end_step
                    || self.phase_chosen_player
                    || self.turn_filter.is_some()
                {
                    Some(EventDetail::Phase {
                        phase: self.phase,
                        any_main_phase: self.any_main_phase,
                        enchanted_controller_upkeep: self.enchanted_controller_upkeep,
                        enchanted_player_upkeep: self.enchanted_player_upkeep,
                        enchanted_opponent_end_step: self.enchanted_opponent_end_step,
                        phase_chosen_player: self.phase_chosen_player,
                        turn_filter: self.turn_filter,
                    })
                } else {
                    None
                }
            }
            EventKind::CounterPlaced | EventKind::CounterRemoved => {
                if self.counter.is_some()
                    || self.last_counter
                    || self.put_by_player.is_some()
                    || self.nth_counter_on_self.is_some()
                {
                    Some(EventDetail::Counter {
                        counter: self.counter,
                        last_counter: self.last_counter,
                        put_by_player: self.put_by_player,
                        nth_counter_on_self: self.nth_counter_on_self,
                    })
                } else {
                    None
                }
            }
            EventKind::BecomesTarget => {
                if self.targeted_by.is_some()
                    || self.targeted_by_activated_ability
                    || self.first_time_targeted_by_you_this_turn
                    || self.targeting_source_filter.is_some()
                    || self.target_player.is_some()
                {
                    Some(EventDetail::BecomesTarget {
                        targeted_by: self.targeted_by,
                        targeted_by_activated_ability: self.targeted_by_activated_ability,
                        first_time_targeted_by_you_this_turn: self
                            .first_time_targeted_by_you_this_turn,
                        targeting_source_filter: self.targeting_source_filter,
                        target_player: self.target_player,
                    })
                } else {
                    None
                }
            }
            EventKind::Attached | EventKind::Unattached => {
                if self.host.is_some() || self.host_self {
                    Some(EventDetail::Attach {
                        host: self.host,
                        host_self: self.host_self,
                    })
                } else {
                    None
                }
            }
            EventKind::TappedForMana | EventKind::ManaAddedByLandAbility => {
                if self.tapped_mana_color.is_some() || self.tapped_mana_chosen_color {
                    Some(EventDetail::Mana {
                        tapped_mana_color: self.tapped_mana_color,
                        tapped_mana_chosen_color: self.tapped_mana_chosen_color,
                    })
                } else {
                    None
                }
            }
            EventKind::AbilityActivated => {
                if self.is_loyalty_ability
                    || self.is_non_mana_ability
                    || self.ability_source.is_some()
                    || self.ability_source_controlled_by_event_player
                {
                    Some(EventDetail::AbilityActivated {
                        is_loyalty_ability: self.is_loyalty_ability,
                        is_non_mana_ability: self.is_non_mana_ability,
                        ability_source: self.ability_source,
                        ability_source_controlled_by_event_player: self
                            .ability_source_controlled_by_event_player,
                    })
                } else {
                    None
                }
            }
            EventKind::Discard => {
                if self.madness_exiled || self.caused_by_opponent {
                    Some(EventDetail::Discard {
                        madness_exiled: self.madness_exiled,
                        caused_by_opponent: self.caused_by_opponent,
                    })
                } else {
                    None
                }
            }
            EventKind::DrawCard | EventKind::DrawReplacement => {
                if self.except_first_in_draw_step {
                    Some(EventDetail::Draw {
                        except_first_in_draw_step: self.except_first_in_draw_step,
                    })
                } else {
                    None
                }
            }
            EventKind::Transforms => {
                self.to_face_filter
                    .map(|to_face_filter| EventDetail::Transform {
                        to_face_filter: Some(to_face_filter),
                    })
            }
            EventKind::BecomesTapped => {
                self.tapped_by_player
                    .map(|tapped_by_player| EventDetail::Tapped {
                        tapped_by_player: Some(tapped_by_player),
                    })
            }
            EventKind::DieRolled | EventKind::CoinFlipped => {
                if self.die_roll_min.is_some()
                    || !self.die_roll_values.is_empty()
                    || self.won_coin_flip
                {
                    Some(EventDetail::DieOrCoin {
                        die_roll_min: self.die_roll_min,
                        die_roll_values: self.die_roll_values,
                        won_coin_flip: self.won_coin_flip,
                    })
                } else {
                    None
                }
            }
            EventKind::StateTriggered => self
                .state_condition
                .map(|condition| EventDetail::State { condition }),
            _ => {
                if self.amass_subtype.is_some()
                    || self.expend_threshold.is_some()
                    || self.class_level.is_some()
                    || self.this_door_face_index.is_some()
                    || self.day_night_direction.is_some()
                    || self.final_chapter
                    || self.won.is_some()
                {
                    Some(EventDetail::Misc {
                        amass_subtype: self.amass_subtype,
                        expend_threshold: self.expend_threshold,
                        class_level: self.class_level,
                        this_door_face_index: self.this_door_face_index,
                        day_night_direction: self.day_night_direction,
                        final_chapter: self.final_chapter,
                        won: self.won,
                    })
                } else {
                    None
                }
            }
        };

        EventFilter {
            object: self.object,
            or_self: self.or_self,
            or_attached_to_self: self.or_attached_to_self,
            or_commander: self.or_commander,
            player: self.player,
            one_or_more: self.one_or_more,
            during_turn: self.during_turn,
            during_step: self.during_step,
            during_combat: self.during_combat,
            during_main_phase: self.during_main_phase,
            first_time_each_turn: self.first_time_each_turn,
            nth_this_turn: self.nth_this_turn,
            nth_event_this_turn: self.nth_event_this_turn,
            attached_filter: self.attached_filter,
            watched_bound_object: self.watched_bound_object,
            detail,
        }
    }
}

fn make_trigger(event: EventKind, filter: EventFilterBuilder) -> TriggerDef {
    TriggerDef {
        event,
        filter: filter.into_event_filter(event),
        scope: None,
        from_zones: None,
        only_once_per_turn: false,
        do_this_once_per_turn: false,
        is_state_trigger: false,
        or: Vec::new(),
        chapter_numbers: SmallVec::new(),
        is_chapter_ability: false,
        spell_delayed: None,
    }
}

// -----------------------------------------------------------------------------
// Trigger object phrase parsing & runtime filter check
// -----------------------------------------------------------------------------

fn has_flag(f: &ObjectFilter, flag: FilterFlags) -> bool {
    f.flags.intersects(flag)
}

fn has_extra<F: Fn(&ObjectPredicate) -> bool>(f: &ObjectFilter, pred: F) -> bool {
    f.extra.iter().any(pred)
}

fn has_supplementary_deck_subtype(f: &ObjectFilter, interner: &Interner) -> bool {
    for id in f.subtypes_any.iter().chain(f.subtypes_all.iter()) {
        if let Some(name) = interner.subtype_name(*id)
            && (name == "Contraption" || name == "Attraction")
        {
            return true;
        }
    }
    for pred in &f.extra {
        if let ObjectPredicate::AnyOf { filters } = pred
            && filters
                .iter()
                .any(|sub| has_supplementary_deck_subtype(sub, interner))
        {
            return true;
        }
    }
    false
}

fn merge_object_filter(mut base: ObjectFilter, outer: ObjectFilter) -> ObjectFilter {
    base.zones |= outer.zones;
    if outer.stack_kind.is_some() {
        base.stack_kind = outer.stack_kind;
    }
    base.types_any |= outer.types_any;
    base.types_all |= outer.types_all;
    base.types_none |= outer.types_none;
    base.supertypes_any |= outer.supertypes_any;
    base.supertypes_none |= outer.supertypes_none;
    base.colors_any |= outer.colors_any;
    base.colors_all |= outer.colors_all;
    base.colors_none |= outer.colors_none;
    for id in outer.subtypes_any {
        if !base.subtypes_any.contains(&id) {
            base.subtypes_any.push(id);
        }
    }
    for id in outer.subtypes_all {
        if !base.subtypes_all.contains(&id) {
            base.subtypes_all.push(id);
        }
    }
    for id in outer.subtypes_none {
        if !base.subtypes_none.contains(&id) {
            base.subtypes_none.push(id);
        }
    }
    if outer.controller.is_some() {
        base.controller = outer.controller;
    }
    if outer.owner.is_some() {
        base.owner = outer.owner;
    }
    base.flags |= outer.flags;
    if outer.mana_value.is_some() {
        base.mana_value = outer.mana_value;
    }
    if outer.power.is_some() {
        base.power = outer.power;
    }
    if outer.toughness.is_some() {
        base.toughness = outer.toughness;
    }
    for pred in outer.extra {
        if !base.extra.contains(&pred) {
            base.extra.push(pred);
        }
    }
    base
}

fn is_valid_explicit_card_name(name: &str) -> bool {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    for (idx, w) in words.iter().enumerate() {
        if idx > 0 && (*w == "of" || *w == "the") {
            continue;
        }
        let mut chars = w.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !first.is_ascii_uppercase() {
            return false;
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '\'' | ',' | '-')) {
            return false;
        }
    }
    true
}

fn parse_color_list_for_thats(raw: &str) -> Option<String> {
    let normalized = replace_ci_sep(raw.trim(), ", and/or ", ", ");
    let normalized = replace_ci_sep(&normalized, " and/or ", ", ");
    let normalized = replace_ci_sep(&normalized, ", or ", ", ");
    let normalized = replace_ci_sep(&normalized, " or ", ", ");
    let parts: Vec<&str> = normalized
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    for p in &parts {
        parse_color_word_or_colorless(p)?;
    }
    Some(parts.join(" "))
}

fn replace_ci_sep(s: &str, pat: &str, replacement: &str) -> String {
    let sb = s.as_bytes();
    let pb = pat.as_bytes();
    if pb.is_empty() || sb.len() < pb.len() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < sb.len() {
        if i + pb.len() <= sb.len() && sb[i..i + pb.len()].eq_ignore_ascii_case(pb) {
            out.push_str(replacement);
            i += pb.len();
        } else {
            let ch = s[i..].chars().next().expect("valid utf8");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_wave9_object_qualifiers(
    rest: &str,
    outer: &mut ObjectFilter,
    interner: &mut Interner,
) -> Option<String> {
    // "<left> with power or toughness N or (less|lower|greater|more|higher)"
    if !has_extra(outer, |p| {
        matches!(p, ObjectPredicate::PowerOrToughness { .. })
    }) && let Some((left, right)) = rsplit_once_ws_word_ci(rest, "with")
        && let Some(after_pt) = strip_prefix_word_ci(right, "power or toughness")
    {
        let words: Vec<&str> = after_pt.split_whitespace().collect();
        if words.len() == 3
            && words[1].eq_ignore_ascii_case("or")
            && let Some(n) = parse_small_i32(words[0])
        {
            let op = if words[2].eq_ignore_ascii_case("less")
                || words[2].eq_ignore_ascii_case("lower")
            {
                Some(CompareOp::Le)
            } else if words[2].eq_ignore_ascii_case("greater")
                || words[2].eq_ignore_ascii_case("more")
                || words[2].eq_ignore_ascii_case("higher")
            {
                Some(CompareOp::Ge)
            } else {
                None
            };
            if let Some(op) = op {
                outer.extra.push(ObjectPredicate::PowerOrToughness {
                    cmp: Compare {
                        op,
                        value: const_amount(n),
                    },
                });
                return Some(left.trim().to_string());
            }
        }
    }

    // "<left> with a counter on it/them"
    if !has_flag(outer, FilterFlags::HAS_ANY_COUNTER)
        && (strip_suffix_ws_ci(rest, "with a counter on it").is_some()
            || strip_suffix_ws_ci(rest, "with a counter on them").is_some())
    {
        let left = strip_suffix_ws_ci(rest, "with a counter on it")
            .or_else(|| strip_suffix_ws_ci(rest, "with a counter on them"))?;
        outer.flags |= FilterFlags::HAS_ANY_COUNTER;
        return Some(left.trim().to_string());
    }

    // "<left> with (a|an) <counter> counter on (it|them)"
    if !has_extra(outer, |p| matches!(p, ObjectPredicate::HasCounter { .. }))
        && let Some(mid) = strip_suffix_ws_ci(rest, "counter on it")
            .or_else(|| strip_suffix_ws_ci(rest, "counter on them"))
        && let Some((left, right)) = rsplit_once_ws_word_ci(mid, "with")
        && let Some(ctr_raw) =
            strip_prefix_word_ci(right, "a").or_else(|| strip_prefix_word_ci(right, "an"))
        && let Some(ctr_id) = parse_trigger_counter(ctr_raw, interner)
    {
        outer.extra.push(ObjectPredicate::HasCounter {
            counter: CounterTarget::Specific(ctr_id),
        });
        return Some(left.trim().to_string());
    }

    None
}

fn parse_u32_slash_u32_prefix(s: &str) -> Option<(i32, i32, &str)> {
    let ws = s.find(char::is_whitespace)?;
    let head = &s[..ws];
    let rest = s[ws..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let slash = head.find('/')?;
    let p_str = &head[..slash];
    let t_str = &head[slash + 1..];
    if p_str.is_empty()
        || t_str.is_empty()
        || !p_str.bytes().all(|b| b.is_ascii_digit())
        || !t_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let p: i32 = p_str.parse().ok()?;
    let t: i32 = t_str.parse().ok()?;
    Some((p, t, rest))
}

fn is_bare_capitalized_word(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_alphabetic() || c == '\'' || c == '-')
}

/// Trigger-specific `ObjectFilter` parser (`ZoneMask::empty()` default, Wave 9 qualifiers,
/// `sources` -> `permanent`, `Contraption`/`Attraction` exclusion, no `NotSelf` on `other than ~`).
pub fn parse_trigger_object_phrase(
    phrase: &str,
    ctx: &mut ParseCtx<'_>,
    allow_spell_noun: bool,
) -> Option<ObjectFilter> {
    let cp = ctx.interner.checkpoint();
    let mut rest = phrase.trim().to_string();
    if rest.is_empty() {
        return None;
    }

    if rest.eq_ignore_ascii_case("enchanted creature or enchantment creature you control") {
        return Some(ObjectFilter {
            extra: vec![ObjectPredicate::AnyOf {
                filters: vec![
                    ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::ENCHANTED,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    },
                    ObjectFilter {
                        types_any: CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                        types_all: CardTypeMask::ENCHANTMENT | CardTypeMask::CREATURE,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        });
    }

    if allow_spell_noun
        && let Some((spell_part, tgt_part)) = split_once_ws_phrase_ci(&rest, "that targets")
        && !tgt_part.eq_ignore_ascii_case("~")
        && !tgt_part.eq_ignore_ascii_case("only ~")
    {
        let tgt_inner = strip_prefix_word_ci(tgt_part, "a")
            .or_else(|| strip_prefix_word_ci(tgt_part, "an"))
            .unwrap_or(tgt_part);
        if let Some(mut spell_f) = parse_trigger_object_phrase(spell_part, ctx, true)
            && let Some(tgt_f) = parse_trigger_object_phrase(tgt_inner, ctx, false)
        {
            spell_f.extra.push(ObjectPredicate::TargetsFilter {
                filter: Box::new(tgt_f),
            });
            return Some(spell_f);
        }
        ctx.interner.rollback(cp);
    }

    let mut outer = ObjectFilter::default();

    for _ in 0..16 {
        if outer.controller.is_none()
            && !has_extra(&outer, |p| {
                matches!(
                    p,
                    ObjectPredicate::ControllerRef { .. }
                        | ObjectPredicate::ControlledByEnchantedPlayer
                )
            })
        {
            if let Some(left) = strip_suffix_ws_ci(&rest, "enchanted player controls") {
                outer
                    .extra
                    .push(ObjectPredicate::ControlledByEnchantedPlayer);
                rest = left.to_string();
                continue;
            }
            if let Some(left) = strip_suffix_ws_ci(&rest, "you control") {
                outer.controller = Some(RelativeControl::You);
                rest = left.to_string();
                continue;
            }
            if let Some(left) = strip_suffix_ws_ci(&rest, "an opponent controls")
                .or_else(|| strip_suffix_ws_ci(&rest, "your opponents control"))
                .or_else(|| strip_suffix_ws_ci(&rest, "opponents control"))
                .or_else(|| strip_suffix_ws_ci(&rest, "you don't control"))
            {
                outer.controller = Some(RelativeControl::Opponent);
                rest = left.to_string();
                continue;
            }
        }
        if !has_extra(&outer, |p| matches!(p, ObjectPredicate::SameNameAsSource))
            && let Some(left) = strip_suffix_ws_ci(&rest, "named ~")
                .or_else(|| strip_suffix_ws_ci(&rest, "with the same name as ~"))
        {
            outer.extra.push(ObjectPredicate::SameNameAsSource);
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::SameNameAsLinkedExile)
        }) && let Some(left) = strip_suffix_ws_ci(&rest, "with the same name as the exiled card")
            .or_else(|| strip_suffix_ws_ci(&rest, "with the same name as the card exiled with ~"))
            .or_else(|| strip_suffix_ws_ci(&rest, "with the same name as a card exiled with ~"))
            .or_else(|| strip_suffix_ws_ci(&rest, "with the same name as an card exiled with ~"))
        {
            outer.extra.push(ObjectPredicate::SameNameAsLinkedExile);
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(
                p,
                ObjectPredicate::Named { .. } | ObjectPredicate::SameNameAsSource
            )
        }) && let Some((left, name_part)) = rsplit_once_ws_word_ci(&rest, "named")
            && !left.is_empty()
            && is_valid_explicit_card_name(name_part)
        {
            outer.extra.push(ObjectPredicate::Named {
                name: name_part.trim().to_string(),
            });
            rest = left.to_string();
            continue;
        }
        if let Some(left) = strip_suffix_ws_ci(&rest, "other than ~") {
            outer.flags |= FilterFlags::OTHER;
            rest = left.to_string();
            continue;
        }
        if outer.owner.is_none() {
            if let Some(left) = strip_suffix_ws_ci(&rest, "owned by another player") {
                outer.owner = Some(RelativeControl::Opponent);
                rest = left.to_string();
                continue;
            }
            if let Some(left) = strip_suffix_ws_ci(&rest, "owned by you") {
                outer.owner = Some(RelativeControl::You);
                rest = left.to_string();
                continue;
            }
        }
        if !has_flag(&outer, FilterFlags::ENCHANTED)
            && let Some(left) = strip_suffix_ws_ci(&rest, "that's enchanted")
                .or_else(|| strip_suffix_ws_ci(&rest, "that is enchanted"))
        {
            outer.flags |= FilterFlags::ENCHANTED;
            rest = left.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::EQUIPPED)
            && let Some(left) = strip_suffix_ws_ci(&rest, "that's equipped")
                .or_else(|| strip_suffix_ws_ci(&rest, "that is equipped"))
        {
            outer.flags |= FilterFlags::EQUIPPED;
            rest = left.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::HAS_ADVENTURE)
            && let Some(left) = strip_suffix_ws_ci(&rest, "that has an adventure")
                .or_else(|| strip_suffix_ws_ci(&rest, "with an adventure"))
        {
            outer.flags |= FilterFlags::HAS_ADVENTURE;
            rest = left.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::HAS_X_IN_MANA_COST)
            && let Some(left) = strip_suffix_ws_ci(&rest, "with {x} in its mana cost")
        {
            outer.flags |= FilterFlags::HAS_X_IN_MANA_COST;
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::PowerGreaterThanBase)
        }) && let Some(left) =
            strip_suffix_ws_ci(&rest, "each with power greater than its base power")
                .or_else(|| strip_suffix_ws_ci(&rest, "with power greater than its base power"))
        {
            outer.extra.push(ObjectPredicate::PowerGreaterThanBase);
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::PowerEqualsToughness)
        }) && let Some(left) = strip_suffix_ws_ci(&rest, "with power equal to its toughness")
        {
            outer.extra.push(ObjectPredicate::PowerEqualsToughness);
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::ManaValueEqualsSourcePower)
        }) && let Some(left) = strip_suffix_ws_ci(&rest, "with mana value equal to ~'s power")
        {
            outer
                .extra
                .push(ObjectPredicate::ManaValueEqualsSourcePower);
            rest = left.to_string();
            continue;
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::DealtDamageByAttachedThisTurn)
        }) {
            let matched_left = [
                "that was dealt damage this turn by enchanted creature",
                "that was dealt damage this turn by enchanted permanent",
                "that was dealt damage this turn by equipped creature",
                "that was dealt damage this turn by equipped permanent",
                "that were dealt damage this turn by enchanted creature",
                "that were dealt damage this turn by enchanted permanent",
                "that were dealt damage this turn by equipped creature",
                "that were dealt damage this turn by equipped permanent",
                "that has been dealt damage this turn by enchanted creature",
                "that has been dealt damage this turn by enchanted permanent",
                "that has been dealt damage this turn by equipped creature",
                "that has been dealt damage this turn by equipped permanent",
                "that have been dealt damage this turn by enchanted creature",
                "that have been dealt damage this turn by enchanted permanent",
                "that have been dealt damage this turn by equipped creature",
                "that have been dealt damage this turn by equipped permanent",
            ]
            .iter()
            .find_map(|sfx| strip_suffix_ws_ci(&rest, sfx));
            if let Some(left) = matched_left {
                outer
                    .extra
                    .push(ObjectPredicate::DealtDamageByAttachedThisTurn);
                rest = left.to_string();
                continue;
            }
        }
        if !has_extra(&outer, |p| {
            matches!(p, ObjectPredicate::ControllerAndNotOwner { .. })
        }) && let Some(left) = strip_suffix_ws_ci(&rest, "that you control but don't own")
            .or_else(|| strip_suffix_ws_ci(&rest, "you control but don't own"))
        {
            outer.extra.push(ObjectPredicate::ControllerAndNotOwner {
                relation: RelativeControl::You,
            });
            rest = left.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::ENCHANTED_OR_EQUIPPED) {
            let matched_left = [
                "that's enchanted or equipped",
                "that's equipped or enchanted",
                "that is enchanted or equipped",
                "that is equipped or enchanted",
                "that are enchanted or equipped",
                "that are equipped or enchanted",
            ]
            .iter()
            .find_map(|sfx| strip_suffix_ws_ci(&rest, sfx));
            if let Some(left) = matched_left {
                outer.flags |= FilterFlags::ENCHANTED_OR_EQUIPPED;
                rest = left.to_string();
                continue;
            }
        }
        if !has_flag(&outer, FilterFlags::DEALT_DAMAGE_THIS_TURN)
            && let Some(left) = strip_suffix_ws_ci(&rest, "that has been dealt damage this turn")
                .or_else(|| strip_suffix_ws_ci(&rest, "that have been dealt damage this turn"))
        {
            outer.flags |= FilterFlags::DEALT_DAMAGE_THIS_TURN;
            rest = left.to_string();
            continue;
        }
        if outer.colors_any.is_empty()
            && outer.colors_all.is_empty()
            && let Some((left, right)) = rsplit_once_ws_word_ci(&rest, "that's")
                .or_else(|| rsplit_once_ws_word_ci(&rest, "that is"))
        {
            if let Some(both_rest) = strip_prefix_word_ci(right, "both") {
                let words: Vec<&str> = both_rest.split_whitespace().collect();
                if words.len() == 3
                    && words[1].eq_ignore_ascii_case("and")
                    && let (Some(c1), Some(c2)) = (
                        parse_color_word_to_mana(words[0]),
                        parse_color_word_to_mana(words[2]),
                    )
                    && c1 != c2
                {
                    let mask = ColorMask::from_mana_color(c1) | ColorMask::from_mana_color(c2);
                    outer.colors_any = mask;
                    outer.colors_all = mask;
                    rest = left.trim().to_string();
                    continue;
                }
            }
            if let Some(colors_norm) = parse_color_list_for_thats(right) {
                rest = format!("{colors_norm} {}", left.trim());
                continue;
            }
        }
        break;
    }

    for _ in 0..16 {
        if let Some(r) =
            strip_prefix_word_ci(&rest, "another").or_else(|| strip_prefix_word_ci(&rest, "other"))
        {
            outer.flags |= FilterFlags::OTHER;
            rest = r.to_string();
            continue;
        }
        if let Some(next_rest) = parse_wave9_object_qualifiers(&rest, &mut outer, ctx.interner) {
            rest = next_rest;
            continue;
        }
        if !has_flag(&outer, FilterFlags::MODIFIED | FilterFlags::NOT_MODIFIED) {
            if let Some(r) = strip_prefix_word_ci(&rest, "modified") {
                outer.flags |= FilterFlags::MODIFIED;
                rest = r.to_string();
                continue;
            }
            if let Some(r) = strip_prefix_word_ci(&rest, "unmodified") {
                outer.flags |= FilterFlags::NOT_MODIFIED;
                rest = r.to_string();
                continue;
            }
        }
        if !has_flag(&outer, FilterFlags::RENOWNED)
            && let Some(r) = strip_prefix_word_ci(&rest, "renowned")
        {
            outer.flags |= FilterFlags::RENOWNED;
            rest = r.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::GOADED)
            && let Some(r) = strip_prefix_word_ci(&rest, "goaded")
        {
            outer.flags |= FilterFlags::GOADED;
            rest = r.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::ENCHANTED_OR_EQUIPPED)
            && let Some(r) = strip_prefix_word_ci(&rest, "enchanted or equipped")
                .or_else(|| strip_prefix_word_ci(&rest, "equipped or enchanted"))
        {
            outer.flags |= FilterFlags::ENCHANTED_OR_EQUIPPED;
            rest = r.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::ENCHANTED)
            && let Some(r) = strip_prefix_word_ci(&rest, "enchanted")
        {
            outer.flags |= FilterFlags::ENCHANTED;
            rest = r.to_string();
            continue;
        }
        if !has_flag(&outer, FilterFlags::EQUIPPED)
            && let Some(r) = strip_prefix_word_ci(&rest, "equipped")
        {
            outer.flags |= FilterFlags::EQUIPPED;
            rest = r.to_string();
            continue;
        }
        if outer.power.is_none()
            && outer.toughness.is_none()
            && let Some((p, t, tail)) = parse_u32_slash_u32_prefix(&rest)
        {
            outer.power = Some(Compare {
                op: CompareOp::Eq,
                value: const_amount(p),
            });
            outer.toughness = Some(Compare {
                op: CompareOp::Eq,
                value: const_amount(t),
            });
            rest = tail.to_string();
            continue;
        }
        break;
    }

    if rest.eq_ignore_ascii_case("outlaw")
        || rest.eq_ignore_ascii_case("outlaws")
        || rest.eq_ignore_ascii_case("outlaw creature")
        || rest.eq_ignore_ascii_case("outlaw creatures")
    {
        let merged = merge_object_filter(
            ObjectFilter {
                flags: FilterFlags::OUTLAW,
                ..Default::default()
            },
            outer,
        );
        return Some(merged);
    }
    if rest.eq_ignore_ascii_case("source") || rest.eq_ignore_ascii_case("sources") {
        return Some(outer);
    }
    if let Some(left) =
        strip_suffix_ws_ci(&rest, "sources").or_else(|| strip_suffix_ws_ci(&rest, "source"))
    {
        rest = format!("{left} permanent");
    }
    if rest.eq_ignore_ascii_case("snow land") || rest.eq_ignore_ascii_case("snow lands") {
        return Some(merge_object_filter(
            ObjectFilter {
                supertypes_any: SupertypeMask::SNOW,
                types_any: CardTypeMask::LAND,
                ..Default::default()
            },
            outer,
        ));
    }
    if rest.eq_ignore_ascii_case("permanent of the chosen color")
        || rest.eq_ignore_ascii_case("permanents of the chosen color")
    {
        return Some(merge_object_filter(
            ObjectFilter {
                extra: vec![ObjectPredicate::OfChosenColor],
                ..Default::default()
            },
            outer,
        ));
    }

    let normalized_and_or = replace_ci_sep(&rest, ", and/or ", ", or ");
    let normalized_and_or = replace_ci_sep(&normalized_and_or, " and/or ", " or ");
    let normalized_and_or = normalize_negated_adjective_commas(&normalized_and_or);
    let opts = FilterOpts {
        default_zone: ZoneMask::empty(),
        allow_plural_and_disjunction: true,
        allow_spell_noun,
        strip_trailing_card: true,
        expand_outlaw_subtypes: false,
    };
    if let Some(parsed) = parse_object_phrase(&normalized_and_or, ctx, opts) {
        let merged = merge_object_filter(parsed, outer);
        if has_supplementary_deck_subtype(&merged, ctx.interner) {
            ctx.interner.rollback(cp);
            return None;
        }
        return Some(merged);
    }

    if is_bare_capitalized_word(&rest)
        && let Some(sub) = singular_subtype(&rest)
    {
        if sub == "Contraption" || sub == "Attraction" {
            ctx.interner.rollback(cp);
            return None;
        }
        let id = ctx.interner.intern_subtype(&sub);
        let mut base = ObjectFilter::default();
        base.subtypes_any.push(id);
        return Some(merge_object_filter(base, outer));
    }

    ctx.interner.rollback(cp);
    None
}

/// Checks whether an [`ObjectFilter`] inside a trigger or static/card filter uses unsupported
/// runtime predicates.
pub(crate) fn object_filter_problem(f: &ObjectFilter) -> Option<&'static str> {
    if f.stack_kind.is_some() {
        return Some("object filter field \"stackKind\"");
    }
    let unsupported_flags = FilterFlags::IS_COLORED
        | FilterFlags::IS_COLORLESS
        | FilterFlags::HAS_NO_COUNTER
        | FilterFlags::WITH_CYCLING_ABILITY;
    if f.flags.intersects(unsupported_flags) {
        return Some("unsupported filter flag");
    }
    if let Some(cmp) = &f.power
        && !matches!(*cmp.value, Amount::Const { .. })
    {
        return Some("non-constant power filter");
    }
    if let Some(cmp) = &f.toughness
        && !matches!(*cmp.value, Amount::Const { .. })
    {
        return Some("non-constant toughness filter");
    }
    if let Some(cmp) = &f.mana_value
        && !matches!(*cmp.value, Amount::Const { .. })
    {
        return Some("non-constant manaValue filter");
    }
    for pred in &f.extra {
        match pred {
            ObjectPredicate::Named { .. }
            | ObjectPredicate::ControllerAndNotOwner { .. }
            | ObjectPredicate::SameOwnerAsSource
            | ObjectPredicate::ControlledByEnchantedPlayer
            | ObjectPredicate::ControlledByChosenPlayer
            | ObjectPredicate::HasKeywords { .. }
            | ObjectPredicate::LacksKeywords { .. }
            | ObjectPredicate::HasCounter { .. }
            | ObjectPredicate::ExiledWithSelf
            | ObjectPredicate::AssimilatedWithSelf
            | ObjectPredicate::PairedWithSource
            | ObjectPredicate::AttachedToRef { .. }
            | ObjectPredicate::OfChosenType
            | ObjectPredicate::OfChosenColor
            | ObjectPredicate::OfChosenName
            | ObjectPredicate::SameNameAsSource
            | ObjectPredicate::SameNameAsLinkedExile
            | ObjectPredicate::SharesTypeWithCommander
            | ObjectPredicate::SharesColorWithSelf
            | ObjectPredicate::SharesCreatureTypeWithSelf
            | ObjectPredicate::SharesCardTypeWithExiled
            | ObjectPredicate::TargetsSelf
            | ObjectPredicate::TargetsAttached
            | ObjectPredicate::CastFromZones { .. }
            | ObjectPredicate::ManaValueParity { .. }
            | ObjectPredicate::ManaValueEqualsSourcePower
            | ObjectPredicate::PowerGreaterThanBase
            | ObjectPredicate::PowerEqualsToughness
            | ObjectPredicate::ToughnessGreaterThanPower
            | ObjectPredicate::PowerLessThanSource
            | ObjectPredicate::PowerOrToughness { .. }
            | ObjectPredicate::UntappedNonattackingSummoningReady
            | ObjectPredicate::DealtDamageByAttachedThisTurn => {}

            ObjectPredicate::ControllerRef { player } => {
                let ok = matches!(player, PlayerRef::You | PlayerRef::ActivePlayer)
                    || matches!(
                        player,
                        PlayerRef::ControllerOf { of } if matches!(**of, mtg_ir::Ref::AttachedTo)
                    );
                if !ok {
                    return Some("unsupported controllerRef");
                }
            }
            ObjectPredicate::AnyOf { filters } => {
                for branch in filters {
                    if let Some(p) = object_filter_problem(branch) {
                        return Some(p);
                    }
                }
            }
            ObjectPredicate::TargetsFilter { filter }
            | ObjectPredicate::TargetsYouOrFilter { filter }
            | ObjectPredicate::AbilitySourceFilter { filter } => {
                if let Some(p) = object_filter_problem(filter) {
                    return Some(p);
                }
            }
            _ => return Some("unsupported object predicate"),
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Subject, Spell, Discard & Graveyard-Leave filters
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TriggerSubjectParsed {
    object: TriggerSubject,
    attached_filter: Option<ObjectFilter>,
    or_self: bool,
    or_attached_to_self: bool,
    one_or_more: bool,
    is_card_noun: bool,
}

fn attached_subject_type(noun: &str) -> Option<CardTypeMask> {
    if noun.eq_ignore_ascii_case("creature") {
        Some(CardTypeMask::CREATURE)
    } else if noun.eq_ignore_ascii_case("land") {
        Some(CardTypeMask::LAND)
    } else if noun.eq_ignore_ascii_case("artifact") {
        Some(CardTypeMask::ARTIFACT)
    } else if noun.eq_ignore_ascii_case("enchantment") {
        Some(CardTypeMask::ENCHANTMENT)
    } else if noun.eq_ignore_ascii_case("planeswalker") {
        Some(CardTypeMask::PLANESWALKER)
    } else {
        None
    }
}

fn is_self_noun_trigger(noun: &str) -> bool {
    matches!(
        noun.to_ascii_lowercase().as_str(),
        "creature"
            | "permanent"
            | "artifact"
            | "enchantment"
            | "land"
            | "planeswalker"
            | "room"
            | "vehicle"
            | "aura"
            | "equipment"
    )
}

fn object_subject(text: &str, ctx: &mut ParseCtx<'_>) -> Option<TriggerSubjectParsed> {
    let t = text.trim();
    if t == "~" {
        return Some(TriggerSubjectParsed {
            object: TriggerSubject::SelfObj,
            attached_filter: None,
            or_self: false,
            or_attached_to_self: false,
            one_or_more: false,
            is_card_noun: false,
        });
    }
    if let Some(noun) = strip_prefix_word_ci(t, "this")
        && is_self_noun_trigger(noun)
    {
        return Some(TriggerSubjectParsed {
            object: TriggerSubject::SelfObj,
            attached_filter: None,
            or_self: false,
            or_attached_to_self: false,
            one_or_more: false,
            is_card_noun: false,
        });
    }
    if t == "equipped creature" {
        return Some(TriggerSubjectParsed {
            object: TriggerSubject::AttachedTo,
            attached_filter: Some(ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            }),
            or_self: false,
            or_attached_to_self: false,
            one_or_more: false,
            is_card_noun: false,
        });
    }
    if let Some(noun) = strip_prefix_word_ci(t, "enchanted") {
        if noun.eq_ignore_ascii_case("permanent") {
            return Some(TriggerSubjectParsed {
                object: TriggerSubject::AttachedTo,
                attached_filter: None,
                or_self: false,
                or_attached_to_self: false,
                one_or_more: false,
                is_card_noun: false,
            });
        }
        if let Some(ty) = attached_subject_type(noun)
            && noun.bytes().all(|b| b.is_ascii_lowercase())
        {
            return Some(TriggerSubjectParsed {
                object: TriggerSubject::AttachedTo,
                attached_filter: Some(ObjectFilter {
                    types_any: ty,
                    ..Default::default()
                }),
                or_self: false,
                or_attached_to_self: false,
                one_or_more: false,
                is_card_noun: false,
            });
        }
        if is_bare_capitalized_word(noun)
            && let Some(sub) = singular_subtype(noun)
        {
            let id = ctx.interner.intern_subtype(&sub);
            let mut af = ObjectFilter::default();
            af.subtypes_any.push(id);
            return Some(TriggerSubjectParsed {
                object: TriggerSubject::AttachedTo,
                attached_filter: Some(af),
                or_self: false,
                or_attached_to_self: false,
                one_or_more: false,
                is_card_noun: false,
            });
        }
    }
    if let Some(noun) = strip_prefix_word_ci(t, "~ or enchanted") {
        if let Some(ty) = attached_subject_type(noun) {
            return Some(TriggerSubjectParsed {
                object: TriggerSubject::AttachedTo,
                attached_filter: Some(ObjectFilter {
                    types_any: ty,
                    ..Default::default()
                }),
                or_self: false,
                or_attached_to_self: true,
                one_or_more: false,
                is_card_noun: false,
            });
        }
        if is_bare_capitalized_word(noun)
            && let Some(sub) = singular_subtype(noun)
        {
            let id = ctx.interner.intern_subtype(&sub);
            let mut af = ObjectFilter::default();
            af.subtypes_any.push(id);
            return Some(TriggerSubjectParsed {
                object: TriggerSubject::AttachedTo,
                attached_filter: Some(af),
                or_self: false,
                or_attached_to_self: true,
                one_or_more: false,
                is_card_noun: false,
            });
        }
    }
    if t.eq_ignore_ascii_case("~ or equipped creature") {
        return Some(TriggerSubjectParsed {
            object: TriggerSubject::AttachedTo,
            attached_filter: Some(ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            }),
            or_self: false,
            or_attached_to_self: true,
            one_or_more: false,
            is_card_noun: false,
        });
    }
    if t.eq_ignore_ascii_case("your commander") {
        return Some(TriggerSubjectParsed {
            object: TriggerSubject::Filter {
                filter: Box::new(ObjectFilter {
                    flags: FilterFlags::IS_COMMANDER,
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                }),
            },
            attached_filter: None,
            or_self: false,
            or_attached_to_self: false,
            one_or_more: false,
            is_card_noun: false,
        });
    }
    if let Some(after_or) = t.strip_prefix("~ or ")
        && starts_with_word_ci(after_or, "another")
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut inner) = object_subject(after_or, ctx)
            && matches!(inner.object, TriggerSubject::Filter { .. })
        {
            inner.or_self = true;
            return Some(inner);
        }
        ctx.interner.rollback(cp);
        return None;
    }

    let mut phrase_str = t.to_string();
    let mut one_or_more = false;
    if let Some(rest) = phrase_str.strip_prefix("one or more ") {
        phrase_str = rest.to_string();
        one_or_more = true;
    } else if let Some(rest) = phrase_str
        .strip_prefix("a ")
        .or_else(|| phrase_str.strip_prefix("an "))
    {
        phrase_str = rest.to_string();
    } else if !phrase_str.starts_with("another ") {
        return None;
    }

    let is_card_noun =
        contains_word_ci(&phrase_str, "card") || contains_word_ci(&phrase_str, "cards");
    let mut controller: Option<RelativeControl> = None;
    let mut toughness_cmp: Option<Compare> = None;

    for _ in 0..16 {
        if controller.is_none()
            && let Some(left) = strip_suffix_ws_ci(&phrase_str, "an opponent controls")
                .or_else(|| strip_suffix_ws_ci(&phrase_str, "your opponents control"))
                .or_else(|| strip_suffix_ws_ci(&phrase_str, "you don't control"))
        {
            phrase_str = left.to_string();
            controller = Some(RelativeControl::Opponent);
            continue;
        }
        if toughness_cmp.is_none()
            && let Some((left, right)) = rsplit_once_ws_word_ci(&phrase_str, "with")
            && let Some(after_tgh) = strip_prefix_word_ci(right, "toughness")
        {
            let words: Vec<&str> = after_tgh.split_whitespace().collect();
            let parsed_tgh = match words.as_slice() {
                [val] => {
                    if val.eq_ignore_ascii_case("a") || val.eq_ignore_ascii_case("an") {
                        return None;
                    }
                    parse_small_i32(val).map(|n| Compare {
                        op: CompareOp::Eq,
                        value: const_amount(n),
                    })
                }
                [val, or_w, dir_w] if or_w.eq_ignore_ascii_case("or") => {
                    if val.eq_ignore_ascii_case("a") || val.eq_ignore_ascii_case("an") {
                        return None;
                    }
                    let op = if dir_w.eq_ignore_ascii_case("greater")
                        || dir_w.eq_ignore_ascii_case("more")
                    {
                        Some(CompareOp::Ge)
                    } else if dir_w.eq_ignore_ascii_case("less")
                        || dir_w.eq_ignore_ascii_case("fewer")
                    {
                        Some(CompareOp::Le)
                    } else {
                        None
                    };
                    match (op, parse_small_i32(val)) {
                        (Some(op), Some(n)) => Some(Compare {
                            op,
                            value: const_amount(n),
                        }),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(cmp) = parsed_tgh {
                toughness_cmp = Some(cmp);
                phrase_str = left.trim().to_string();
                continue;
            }
        }
        break;
    }

    let cp = ctx.interner.checkpoint();
    let mut filter = parse_trigger_object_phrase(&phrase_str, ctx, false)?;
    if let Some(ctrl) = controller {
        if filter.controller.is_some() && filter.controller != Some(ctrl) {
            ctx.interner.rollback(cp);
            return None;
        }
        filter.controller = Some(ctrl);
    }
    if let Some(cmp) = toughness_cmp {
        if filter.toughness.is_some() {
            ctx.interner.rollback(cp);
            return None;
        }
        filter.toughness = Some(cmp);
    }

    Some(TriggerSubjectParsed {
        object: TriggerSubject::Filter {
            filter: Box::new(filter),
        },
        attached_filter: None,
        or_self: false,
        or_attached_to_self: false,
        one_or_more,
        is_card_noun,
    })
}

fn spell_filter(text: &str, ctx: &mut ParseCtx<'_>) -> Option<TriggerSubject> {
    let t = text.trim();
    if t == "~" {
        return Some(TriggerSubject::SelfObj);
    }
    let core = if let Some(rest) = strip_prefix_word_ci(t, "another") {
        format!("another {rest}")
    } else {
        let rest = strip_prefix_word_ci(t, "a").or_else(|| strip_prefix_word_ci(t, "an"))?;
        rest.to_string()
    };
    if core.eq_ignore_ascii_case("spell") {
        return Some(TriggerSubject::Filter {
            filter: Box::new(ObjectFilter::default()),
        });
    }
    if core.eq_ignore_ascii_case("another spell") {
        return Some(TriggerSubject::Filter {
            filter: Box::new(ObjectFilter {
                flags: FilterFlags::OTHER,
                ..Default::default()
            }),
        });
    }
    if !contains_word_ci(&core, "spell") {
        return None;
    }
    parse_trigger_object_phrase(&core, ctx, true).map(|f| TriggerSubject::Filter {
        filter: Box::new(f),
    })
}

fn discarded_card_filter(text: &str, ctx: &mut ParseCtx<'_>) -> Option<(ObjectFilter, bool)> {
    let t = text.trim();
    if t == "~" {
        return None;
    }
    if t.eq_ignore_ascii_case("another card") {
        return Some((
            ObjectFilter {
                flags: FilterFlags::OTHER,
                ..Default::default()
            },
            false,
        ));
    }
    if let Some(inner) = strip_prefix_word_ci(t, "another") {
        if !strip_suffix_ws_ci(inner, "cards").is_some()
            && !strip_suffix_ws_ci(inner, "card").is_some()
            && !inner.eq_ignore_ascii_case("card")
            && !inner.eq_ignore_ascii_case("cards")
        {
            return None;
        }
        // TS parity (Tergrid, God of Fright): "permanent card" -> `{}` (`types_none: empty()`)
        if inner.eq_ignore_ascii_case("permanent card")
            || inner.eq_ignore_ascii_case("permanent cards")
        {
            return Some((
                ObjectFilter {
                    flags: FilterFlags::OTHER,
                    ..Default::default()
                },
                false,
            ));
        }
        let parsed = parse_trigger_object_phrase(&format!("another {inner}"), ctx, false)?;
        return Some((parsed, false));
    }
    if let Some(inner) = strip_prefix_word_ci(t, "one or more") {
        if inner.eq_ignore_ascii_case("cards") {
            return Some((ObjectFilter::default(), true));
        }
        if !inner.eq_ignore_ascii_case("cards") && strip_suffix_ws_ci(inner, "cards").is_none() {
            return None;
        }
        if inner.eq_ignore_ascii_case("permanent cards") {
            return Some((ObjectFilter::default(), true));
        }
        let parsed = parse_trigger_object_phrase(inner, ctx, false)?;
        return Some((parsed, true));
    }
    let inner = strip_prefix_word_ci(t, "a").or_else(|| strip_prefix_word_ci(t, "an"))?;
    if inner.eq_ignore_ascii_case("card") {
        return Some((ObjectFilter::default(), false));
    }
    strip_suffix_ws_ci(inner, "card")?;
    // TS parity (Tergrid, God of Fright, triggers.ts:1485): "permanent card" in discard filters
    // produced empty ObjectFilter `{}` in TS rather than `types_none: INSTANT | SORCERY`.
    if inner.eq_ignore_ascii_case("permanent card") {
        return Some((ObjectFilter::default(), false));
    }
    let parsed = parse_trigger_object_phrase(inner, ctx, false)?;
    Some((parsed, false))
}

fn parse_graveyard_leave_card_filter(
    raw: Option<&str>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ObjectFilter> {
    let Some(r) = raw else {
        return Some(ObjectFilter::default());
    };
    let trimmed = r.trim();
    if trimmed.is_empty() {
        return Some(ObjectFilter::default());
    }
    if trimmed.eq_ignore_ascii_case("permanent") || trimmed.eq_ignore_ascii_case("permanents") {
        return Some(ObjectFilter {
            types_any: CardTypeMask::PERMANENT,
            ..Default::default()
        });
    }
    let normalized = replace_ci_sep(trimmed, " and/or ", " or ");
    let normalized = replace_ci_sep(&normalized, " and ", " or ");
    parse_trigger_object_phrase(&format!("{normalized} card"), ctx, false)
}

// -----------------------------------------------------------------------------
// Phase, Object, Counter, and State Trigger Events
// -----------------------------------------------------------------------------

fn parse_step_name(s: &str) -> Option<GamePhase> {
    match s.trim().to_ascii_lowercase().as_str() {
        "upkeep" => Some(GamePhase::Upkeep),
        "draw step" => Some(GamePhase::Draw),
        "end step" => Some(GamePhase::EndStep),
        "precombat main phase" | "first main phase" => Some(GamePhase::Main1),
        "postcombat main phase" | "second main phase" => Some(GamePhase::Main2),
        _ => None,
    }
}

fn phase_begin(rest_raw: &str) -> Option<TriggerDef> {
    let rest = normalize_apostrophes(rest_raw.trim()).to_ascii_lowercase();
    let rest = rest.as_str();

    if rest == "your next upkeep" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Upkeep),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        t.spell_delayed = Some(SpellDelayedDuration::NextUpkeep);
        return Some(t);
    }
    if rest == "combat on your turn" || rest == "your combat step" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::BeginCombat),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        return Some(t);
    }
    if rest == "each combat" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::BeginCombat),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }
    if rest == "each of your main phases" || rest == "your main phase" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Main1),
                any_main_phase: true,
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        return Some(t);
    }
    if let Some(step_raw) = rest.strip_prefix("the chosen player's ")
        && matches!(step_raw, "upkeep" | "draw step" | "end step")
        && let Some(phase) = parse_step_name(step_raw)
    {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(phase),
                phase_chosen_player: true,
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }
    if rest == "combat on each opponent's turn" || rest == "combat on an opponent's turn" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::BeginCombat),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::OpponentTurn);
        return Some(t);
    }
    if rest == "your first upkeep" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Upkeep),
                turn_filter: Some(PhaseTurnFilter::YourFirstUpkeep),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        return Some(t);
    }
    if rest == "each other player's first upkeep" || rest == "each opponent's first upkeep" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Upkeep),
                turn_filter: Some(PhaseTurnFilter::FirstOfGame),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::OpponentTurn);
        return Some(t);
    }
    if rest == "the first upkeep" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Upkeep),
                turn_filter: Some(PhaseTurnFilter::FirstOfGame),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }
    if rest == "the monarch's end step" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::EndStep),
                turn_filter: Some(PhaseTurnFilter::Monarch),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }
    if rest == "each of your postcombat main phases" || rest == "each of your second main phases" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Main2),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        return Some(t);
    }
    if rest == "each of your precombat main phases" || rest == "each of your first main phases" {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::Main1),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::ControllerTurn);
        return Some(t);
    }
    if let Some(after_the) = rest.strip_prefix("the ")
        && let Some((step_raw, holder)) = after_the.split_once(" of ")
        && matches!(step_raw, "upkeep" | "draw step" | "end step")
        && let Some(phase) = parse_step_name(step_raw)
        && let Some(noun_raw) = holder
            .strip_prefix("enchanted ")
            .or_else(|| holder.strip_prefix("equipped "))
        && let Some(noun) = noun_raw.strip_suffix("'s controller")
        && matches!(
            noun,
            "creature" | "permanent" | "land" | "artifact" | "enchantment" | "planeswalker"
        )
    {
        let af = attached_subject_type(noun).map(|ty| {
            Box::new(ObjectFilter {
                types_any: ty,
                ..Default::default()
            })
        });
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(phase),
                enchanted_controller_upkeep: true,
                attached_filter: af,
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }
    if let Some(step_raw) = rest.strip_prefix("enchanted player's ")
        && matches!(step_raw, "upkeep" | "draw step" | "end step")
        && let Some(phase) = parse_step_name(step_raw)
    {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(phase),
                enchanted_player_upkeep: true,
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }

    let prefixes = [
        ("each other player's ", TriggerScope::OpponentTurn),
        ("each opponent's ", TriggerScope::OpponentTurn),
        ("each player's ", TriggerScope::EachTurn),
        ("each ", TriggerScope::EachTurn),
        ("your ", TriggerScope::ControllerTurn),
        ("the ", TriggerScope::EachTurn),
    ];
    for (pref, scope) in prefixes {
        if let Some(step_raw) = rest.strip_prefix(pref)
            && let Some(phase) = parse_step_name(step_raw)
        {
            let mut t = make_trigger(
                EventKind::PhaseBegin,
                EventFilterBuilder {
                    phase: Some(phase),
                    ..Default::default()
                },
            );
            t.scope = Some(scope);
            return Some(t);
        }
    }
    None
}

fn object_event(
    verb_raw: &str,
    subject: &TriggerSubjectParsed,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggerDef> {
    let verb_norm = normalize_apostrophes(verb_raw.trim());
    let verb = verb_norm.as_str();
    let verb_low = verb.to_ascii_lowercase();

    let base_builder = || {
        let mut b = EventFilterBuilder {
            object: Some(subject.object.clone()),
            or_self: subject.or_self,
            or_attached_to_self: subject.or_attached_to_self,
            one_or_more: subject.one_or_more,
            ..Default::default()
        };
        if subject.or_attached_to_self
            && let Some(af) = &subject.attached_filter
        {
            b.attached_filter = Some(Box::new(af.clone()));
        }
        b
    };

    let def = |event: EventKind, mut extra: EventFilterBuilder| -> TriggerDef {
        extra.object = Some(subject.object.clone());
        extra.or_self |= subject.or_self;
        extra.or_attached_to_self |= subject.or_attached_to_self;
        extra.one_or_more |= subject.one_or_more;
        if subject.or_attached_to_self
            && extra.attached_filter.is_none()
            && let Some(af) = &subject.attached_filter
        {
            extra.attached_filter = Some(Box::new(af.clone()));
        }
        make_trigger(event, extra)
    };

    let with_attached_filter = |mut b: EventFilterBuilder| -> EventFilterBuilder {
        if let Some(af) = &subject.attached_filter {
            b.attached_filter = Some(Box::new(af.clone()));
        }
        b
    };

    match verb_low.as_str() {
        "enters" | "enter" | "enters the battlefield" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    ..Default::default()
                },
            ));
        }
        "enters untapped"
        | "enter untapped"
        | "enters the battlefield untapped"
        | "enter the battlefield untapped" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    enters_untapped: true,
                    ..Default::default()
                },
            ));
        }
        "enters from a graveyard"
        | "enter from a graveyard"
        | "enters the battlefield from a graveyard" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    from: ZoneMask::GRAVEYARD,
                    to: Some(Zone::Battlefield),
                    ..Default::default()
                },
            ));
        }
        "enters from your graveyard"
        | "enter from your graveyard"
        | "enters the battlefield from your graveyard" => {
            if matches!(subject.object, TriggerSubject::SelfObj) {
                return Some(def(
                    EventKind::ZoneChange,
                    EventFilterBuilder {
                        from: ZoneMask::GRAVEYARD,
                        to: Some(Zone::Battlefield),
                        ..Default::default()
                    },
                ));
            }
            if let TriggerSubject::Filter { filter } = &subject.object {
                let mut f = (**filter).clone();
                f.owner = Some(RelativeControl::You);
                let mut b = base_builder();
                b.object = Some(TriggerSubject::Filter {
                    filter: Box::new(f),
                });
                b.from = ZoneMask::GRAVEYARD;
                b.to = Some(Zone::Battlefield);
                return Some(make_trigger(EventKind::ZoneChange, b));
            }
            return None;
        }
        "enters during your turn" | "enter during your turn" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    during_turn: Some(DuringTurnKind::Your),
                    ..Default::default()
                },
            ));
        }
        "enters from anywhere other than your hand"
        | "enter from anywhere other than your hand" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    not_from_zone: Some(Zone::Hand),
                    ..Default::default()
                },
            ));
        }
        "enters the battlefield under your control" | "enters under your control" => {
            if let TriggerSubject::Filter { filter } = &subject.object {
                let mut f = (**filter).clone();
                f.controller = Some(RelativeControl::You);
                let mut b = base_builder();
                b.object = Some(TriggerSubject::Filter {
                    filter: Box::new(f),
                });
                b.to = Some(Zone::Battlefield);
                return Some(make_trigger(EventKind::ZoneChange, b));
            }
            return None;
        }
        "dies"
        | "die"
        | "is put into a graveyard from the battlefield"
        | "are put into a graveyard from the battlefield"
        | "is put into a player's graveyard from the battlefield"
        | "are put into a player's graveyard from the battlefield" => {
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Graveyard),
                    ..Default::default()
                }),
            ));
        }
        "dies or is put into exile"
        | "die or are put into exile"
        | "dies or is exiled"
        | "die or are exiled"
        | "dies or is put into exile from the battlefield"
        | "die or are put into exile from the battlefield" => {
            let mut trig = def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Graveyard),
                    ..Default::default()
                }),
            );
            trig.or.push(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Exile),
                    ..Default::default()
                }),
            ));
            return Some(trig);
        }
        "is put into a graveyard" | "are put into a graveyard" => {
            if !matches!(
                subject.object,
                TriggerSubject::AttachedTo | TriggerSubject::SelfObj
            ) {
                return None;
            }
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Graveyard),
                    ..Default::default()
                }),
            ));
        }
        "is put into exile from the battlefield"
        | "are put into exile from the battlefield"
        | "is exiled from the battlefield"
        | "are exiled from the battlefield" => {
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Exile),
                    ..Default::default()
                }),
            ));
        }
        "is put into exile" | "are put into exile" => {
            if !matches!(
                subject.object,
                TriggerSubject::AttachedTo | TriggerSubject::SelfObj
            ) {
                return None;
            }
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Exile),
                    ..Default::default()
                }),
            ));
        }
        "leaves the battlefield" | "leave the battlefield" => {
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    ..Default::default()
                }),
            ));
        }
        "leaves the battlefield without dying" | "leave the battlefield without dying" => {
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    not_to_zone: Some(Zone::Graveyard),
                    ..Default::default()
                }),
            ));
        }
        "attacks" | "attack" => {
            return Some(def(EventKind::Attacks, EventFilterBuilder::default()));
        }
        "attacks while saddled" | "attack while saddled" => {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    saddled: true,
                    ..Default::default()
                },
            ));
        }
        "blocks" | "block" => return Some(def(EventKind::Blocks, EventFilterBuilder::default())),
        "becomes blocked" | "become blocked" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::BecomesBlocked, EventFilterBuilder::default()));
        }
        "becomes tapped" => {
            return Some(def(EventKind::BecomesTapped, EventFilterBuilder::default()));
        }
        "becomes crewed" => {
            return Some(def(EventKind::BecomesCrewed, EventFilterBuilder::default()));
        }
        "becomes saddled" => {
            return Some(def(
                EventKind::BecomesSaddled,
                EventFilterBuilder::default(),
            ));
        }
        "is exerted" | "are exerted" => {
            return Some(def(EventKind::Exert, EventFilterBuilder::default()));
        }
        "is dealt damage" | "are dealt damage" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::DealtDamage, EventFilterBuilder::default()));
        }
        "is dealt combat damage" | "are dealt combat damage" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealtDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        ..Default::default()
                    },
                )
            });
        }
        "is dealt noncombat damage" | "are dealt noncombat damage" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealtDamage,
                    EventFilterBuilder {
                        noncombat_only: true,
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to a player" | "deal combat damage to a player" => {
            return Some(if subject.one_or_more {
                def(
                    EventKind::CombatDamageBatchPlayer,
                    EventFilterBuilder {
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Any,
                        }),
                        ..Default::default()
                    },
                )
            } else {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Player),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to an opponent"
        | "deals combat damage to one of your opponents"
        | "deal combat damage to an opponent"
        | "deal combat damage to one of your opponents" => {
            return Some(if subject.one_or_more {
                def(
                    EventKind::CombatDamageBatchPlayer,
                    EventFilterBuilder {
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        }),
                        ..Default::default()
                    },
                )
            } else {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to you" | "deal combat damage to you" => {
            return Some(if subject.one_or_more {
                def(
                    EventKind::CombatDamageBatchPlayer,
                    EventFilterBuilder {
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                )
            } else {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to a creature" | "deal combat damage to a creature" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Creature),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to a player or planeswalker"
        | "deal combat damage to a player or planeswalker" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::PlayerOrPlaneswalker),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage to a player or battle"
        | "deal combat damage to a player or battle" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    with_attached_filter(EventFilterBuilder {
                        combat: Some(true),
                        damage_target_kind: Some(DamageTargetKind::PlayerOrBattle),
                        ..Default::default()
                    }),
                )
            });
        }
        "deals combat damage to a planeswalker" | "deal combat damage to a planeswalker" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_target_kind: Some(DamageTargetKind::Planeswalker),
                        ..Default::default()
                    },
                )
            });
        }
        "deals combat damage" | "deal combat damage" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Any),
                        ..Default::default()
                    },
                )
            });
        }
        "deals damage to a player" | "deal damage to a player" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Player),
                        ..Default::default()
                    },
                )
            });
        }
        "deals damage to a creature" | "deal damage to a creature" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    with_attached_filter(EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Creature),
                        ..Default::default()
                    }),
                )
            });
        }
        "deals damage to a planeswalker" | "deal damage to a planeswalker" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        damage_target_kind: Some(DamageTargetKind::Planeswalker),
                        ..Default::default()
                    },
                )
            });
        }
        "deals damage to an opponent"
        | "deals damage to one of your opponents"
        | "deal damage to an opponent"
        | "deal damage to one of your opponents" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals damage to you" | "deal damage to you" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals damage" | "deal damage" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Any),
                        ..Default::default()
                    },
                )
            });
        }
        "becomes the target of a spell or ability" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BecomesTarget,
                    EventFilterBuilder {
                        targeted_by: Some(TargetedByKind::SpellOrAbility),
                        ..Default::default()
                    },
                )
            });
        }
        "becomes the target of a spell" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BecomesTarget,
                    EventFilterBuilder {
                        targeted_by: Some(TargetedByKind::Spell),
                        ..Default::default()
                    },
                )
            });
        }
        "becomes the target of an ability" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BecomesTarget,
                    EventFilterBuilder {
                        targeted_by: Some(TargetedByKind::Ability),
                        ..Default::default()
                    },
                )
            });
        }
        "is tapped for mana" => {
            let mut b = EventFilterBuilder::default();
            if subject
                .attached_filter
                .as_ref()
                .is_some_and(|af| !af.subtypes_any.is_empty())
            {
                b = with_attached_filter(b);
            }
            return Some(def(EventKind::TappedForMana, b));
        }
        "is tapped for mana of the chosen color" => {
            return Some(def(
                EventKind::TappedForMana,
                with_attached_filter(EventFilterBuilder {
                    tapped_mana_chosen_color: true,
                    ..Default::default()
                }),
            ));
        }
        "enters during the declare attackers step"
        | "enter during the declare attackers step"
        | "enters the battlefield during the declare attackers step"
        | "enter the battlefield during the declare attackers step" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    during_step: Some(GamePhase::DeclareAttackers),
                    ..Default::default()
                },
            ));
        }
        "enters during combat"
        | "enter during combat"
        | "enters the battlefield during combat"
        | "enter the battlefield during combat" => {
            return Some(def(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    to: Some(Zone::Battlefield),
                    during_combat: true,
                    ..Default::default()
                },
            ));
        }
        "dies during combat" | "die during combat" => {
            return Some(def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Graveyard),
                    during_combat: true,
                    ..Default::default()
                }),
            ));
        }
        "deals combat damage to enchanted player" | "deal combat damage to enchanted player" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    with_attached_filter(EventFilterBuilder {
                        combat: Some(true),
                        damage_to: Some(DamageRecipient::Player),
                        damage_target_enchanted_player: true,
                        ..Default::default()
                    }),
                )
            });
        }
        "deals damage to enchanted player" | "deal damage to enchanted player" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    with_attached_filter(EventFilterBuilder {
                        damage_to: Some(DamageRecipient::Player),
                        damage_target_enchanted_player: true,
                        ..Default::default()
                    }),
                )
            });
        }
        "becomes plotted" => {
            if matches!(subject.object, TriggerSubject::SelfObj) {
                let mut t = make_trigger(
                    EventKind::Plot,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::SelfObj),
                        ..Default::default()
                    },
                );
                t.from_zones = Some(ZoneMask::EXILE);
                return Some(t);
            }
            return None;
        }
        "evolves" | "evolve" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::Evolved, EventFilterBuilder::default()));
        }
        "becomes crewed for the first time each turn" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BecomesCrewed,
                    EventFilterBuilder {
                        first_time_each_turn: true,
                        ..Default::default()
                    },
                )
            });
        }
        "crews a vehicle" | "crew a vehicle" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::CrewedVehicle, EventFilterBuilder::default()));
        }
        "attacks this turn" | "attack this turn" => {
            let mut t = def(EventKind::Attacks, EventFilterBuilder::default());
            t.spell_delayed = Some(SpellDelayedDuration::ThisTurn);
            return Some(t);
        }
        "blocks this turn" | "block this turn" => {
            let mut t = def(EventKind::Blocks, EventFilterBuilder::default());
            t.spell_delayed = Some(SpellDelayedDuration::ThisTurn);
            return Some(t);
        }
        "dies this turn" | "die this turn" => {
            let mut t = def(
                EventKind::ZoneChange,
                with_attached_filter(EventFilterBuilder {
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Graveyard),
                    ..Default::default()
                }),
            );
            t.spell_delayed = Some(SpellDelayedDuration::ThisTurn);
            return Some(t);
        }
        "becomes attached to a creature" => {
            return Some(def(
                EventKind::Attached,
                EventFilterBuilder {
                    host: Some(Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ));
        }
        "becomes attached to a creature you control" => {
            return Some(def(
                EventKind::Attached,
                EventFilterBuilder {
                    host: Some(Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        controller: Some(RelativeControl::You),
                        ..Default::default()
                    })),
                    ..Default::default()
                },
            ));
        }
        "transforms" | "transform" => {
            return Some(def(
                EventKind::Transforms,
                with_attached_filter(EventFilterBuilder::default()),
            ));
        }
        "is turned face up" | "are turned face up" => {
            return Some(def(
                EventKind::TurnFaceUp,
                with_attached_filter(EventFilterBuilder::default()),
            ));
        }
        "explores" | "explore" => {
            return Some(def(EventKind::Explore, EventFilterBuilder::default()));
        }
        "connives" | "connive" => {
            return Some(def(EventKind::Connive, EventFilterBuilder::default()));
        }
        "becomes monstrous" | "become monstrous" => {
            return Some(def(
                EventKind::BecameMonstrous,
                EventFilterBuilder::default(),
            ));
        }
        "attacks and isn't blocked"
        | "attack and aren't blocked"
        | "attacks and is not blocked"
        | "attack and are not blocked" => {
            return Some(def(
                EventKind::AttackedAndUnblocked,
                with_attached_filter(EventFilterBuilder::default()),
            ));
        }
        "attacks alone" | "attack alone" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::Attacks,
                    with_attached_filter(EventFilterBuilder {
                        alone: true,
                        ..Default::default()
                    }),
                )
            });
        }
        "becomes untapped" | "become untapped" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BecomesUntapped,
                    with_attached_filter(EventFilterBuilder::default()),
                )
            });
        }
        "deals noncombat damage to a player" | "deal noncombat damage to a player" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(false),
                        damage_to: Some(DamageRecipient::Player),
                        ..Default::default()
                    },
                )
            });
        }
        "deals noncombat damage to an opponent"
        | "deals noncombat damage to one of your opponents"
        | "deal noncombat damage to an opponent"
        | "deal noncombat damage to one of your opponents" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(false),
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals noncombat damage to you" | "deal noncombat damage to you" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(false),
                        damage_to: Some(DamageRecipient::Player),
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                )
            });
        }
        "deals noncombat damage to a creature" | "deal noncombat damage to a creature" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(false),
                        damage_to: Some(DamageRecipient::Creature),
                        ..Default::default()
                    },
                )
            });
        }
        "deals noncombat damage" | "deal noncombat damage" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::DealsDamage,
                    EventFilterBuilder {
                        combat: Some(false),
                        damage_to: Some(DamageRecipient::Any),
                        ..Default::default()
                    },
                )
            });
        }
        "exploits a creature" | "exploit a creature" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::Exploited, EventFilterBuilder::default()));
        }
        "blocks or becomes blocked" | "block or become blocked" => {
            return (!subject.one_or_more).then(|| {
                def(
                    EventKind::BlocksOrBecomesBlocked,
                    with_attached_filter(EventFilterBuilder::default()),
                )
            });
        }
        "mutates" | "mutate" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::Mutates, EventFilterBuilder::default()));
        }
        "phases in" | "phase in" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::PhasedIn, EventFilterBuilder::default()));
        }
        "becomes unattached"
        | "becomes unattached from a permanent"
        | "becomes unattached from a creature" => {
            return (!subject.one_or_more)
                .then(|| def(EventKind::Unattached, EventFilterBuilder::default()));
        }
        "becomes paired with another creature" => {
            return matches!(subject.object, TriggerSubject::SelfObj)
                .then(|| def(EventKind::SoulbondPaired, EventFilterBuilder::default()));
        }
        "attacks a planeswalker" | "attack a planeswalker" => {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::Planeswalker),
                    ..Default::default()
                },
            ));
        }
        "attacks a battle" | "attack a battle" => {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::Battle),
                    ..Default::default()
                },
            ));
        }
        _ => {}
    }

    // Dynamic suffixes in objectEvent default branch
    if let Some(rest) = verb_low
        .strip_prefix("is put into ")
        .or_else(|| verb_low.strip_prefix("are put into "))
    {
        for (whose_str, whose_owner) in [
            ("your graveyard from anywhere", Some(RelativeControl::You)),
            (
                "an opponent's graveyard from anywhere",
                Some(RelativeControl::Opponent),
            ),
            ("a player's graveyard from anywhere", None),
            ("a graveyard from anywhere", None),
        ] {
            if let Some(after) = rest.strip_prefix(whose_str) {
                let (other_than_bf, during_your_turn) = match after {
                    "" => (false, false),
                    " other than the battlefield" => (true, false),
                    " during your turn" => (false, true),
                    " other than the battlefield during your turn" => (true, true),
                    _ => continue,
                };
                let mut b = base_builder();
                b.to = Some(Zone::Graveyard);
                b.from_anywhere = true;
                if other_than_bf {
                    b.not_from_zone = Some(Zone::Battlefield);
                }
                if during_your_turn {
                    b.during_turn = Some(DuringTurnKind::Your);
                }
                if matches!(subject.object, TriggerSubject::SelfObj) {
                    let mut t = make_trigger(EventKind::ZoneChange, b);
                    t.from_zones = Some(ZoneMask::GRAVEYARD);
                    return Some(t);
                }
                if let TriggerSubject::Filter { filter } = &subject.object {
                    let mut f = (**filter).clone();
                    if subject.is_card_noun
                        && !f
                            .flags
                            .intersects(FilterFlags::TOKEN | FilterFlags::NONTOKEN)
                    {
                        f.flags |= FilterFlags::NONTOKEN;
                    }
                    if let Some(own) = whose_owner {
                        f.owner = Some(own);
                    }
                    b.object = Some(TriggerSubject::Filter {
                        filter: Box::new(f),
                    });
                    return Some(make_trigger(EventKind::ZoneChange, b));
                }
            }
        }

        if let Some((whose, _)) = [
            ("your graveyard from the battlefield", RelativeControl::You),
            (
                "an opponent's graveyard from the battlefield",
                RelativeControl::Opponent,
            ),
        ]
        .iter()
        .find(|(pat, _)| rest == *pat)
        {
            let owner = if whose.starts_with("your") {
                RelativeControl::You
            } else {
                RelativeControl::Opponent
            };
            if matches!(subject.object, TriggerSubject::SelfObj) {
                if owner != RelativeControl::You {
                    return None;
                }
                return Some(def(
                    EventKind::ZoneChange,
                    EventFilterBuilder {
                        from: ZoneMask::BATTLEFIELD,
                        to: Some(Zone::Graveyard),
                        ..Default::default()
                    },
                ));
            }
            if let TriggerSubject::Filter { filter } = &subject.object {
                let mut f = (**filter).clone();
                f.owner = Some(owner);
                let mut b = base_builder();
                b.object = Some(TriggerSubject::Filter {
                    filter: Box::new(f),
                });
                b.from = ZoneMask::BATTLEFIELD;
                b.to = Some(Zone::Graveyard);
                return Some(make_trigger(EventKind::ZoneChange, b));
            }
        }

        for (whose_str, whose_owner) in [
            ("your graveyard from ", Some(RelativeControl::You)),
            (
                "an opponent's graveyard from ",
                Some(RelativeControl::Opponent),
            ),
            ("a graveyard from ", None),
        ] {
            if let Some(lib_part) = rest.strip_prefix(whose_str)
                && matches!(
                    lib_part,
                    "your library" | "their library" | "its owner's library" | "a library"
                )
            {
                if matches!(subject.object, TriggerSubject::SelfObj) {
                    if whose_owner == Some(RelativeControl::Opponent) {
                        return None;
                    }
                    let mut t = def(
                        EventKind::ZoneChange,
                        EventFilterBuilder {
                            from: ZoneMask::LIBRARY,
                            to: Some(Zone::Graveyard),
                            ..Default::default()
                        },
                    );
                    t.from_zones = Some(ZoneMask::GRAVEYARD);
                    return Some(t);
                }
                if let TriggerSubject::Filter { filter } = &subject.object {
                    let mut f = (**filter).clone();
                    if let Some(own) = whose_owner {
                        f.owner = Some(own);
                    }
                    let mut b = base_builder();
                    b.object = Some(TriggerSubject::Filter {
                        filter: Box::new(f),
                    });
                    b.from = ZoneMask::LIBRARY;
                    b.to = Some(Zone::Graveyard);
                    return Some(make_trigger(EventKind::ZoneChange, b));
                }
            }
        }
    }

    if matches!(
        subject.object,
        TriggerSubject::Filter { .. } | TriggerSubject::SelfObj
    ) {
        if matches!(
            verb_low.as_str(),
            "attacks the player with the most life"
                | "attack the player with the most life"
                | "attacks a player with the most life"
                | "attack a player with the most life"
                | "attacks the player with the most life or tied for most life"
                | "attack the player with the most life or tied for most life"
                | "attacks a player with the most life or tied for most life"
                | "attack a player with the most life or tied for most life"
        ) {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::PlayerWithMostLife),
                    ..Default::default()
                },
            ));
        }
        if verb_low == "attacks you" || verb_low == "attack you" {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::You),
                    ..Default::default()
                },
            ));
        }
        if verb_low == "attacks you or a planeswalker you control"
            || verb_low == "attack you or a planeswalker you control"
        {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::YouOrPlaneswalkerYouControl),
                    ..Default::default()
                },
            ));
        }
        if verb_low == "attacks enchanted player" || verb_low == "attack enchanted player" {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::EnchantedPlayer),
                    ..Default::default()
                },
            ));
        }
    }

    if matches!(subject.object, TriggerSubject::Filter { .. }) {
        if matches!(
            verb_low.as_str(),
            "attacks an opponent"
                | "attack an opponent"
                | "attacks one of your opponents"
                | "attack one of your opponents"
        ) {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(AttackedTargetFilter::Opponent),
                    ..Default::default()
                },
            ));
        }
        if matches!(
            verb_low.as_str(),
            "attacks an opponent or a planeswalker an opponent controls"
                | "attack an opponent or a planeswalker an opponent controls"
                | "attacks one of your opponents or a planeswalker an opponent controls"
                | "attack one of your opponents or a planeswalker an opponent controls"
        ) {
            return Some(def(
                EventKind::Attacks,
                EventFilterBuilder {
                    attacked_target: Some(
                        AttackedTargetFilter::OpponentOrPlaneswalkerOpponentControls,
                    ),
                    ..Default::default()
                },
            ));
        }
    }

    if let Some(mana_str) = strip_prefix_ci(verb, "is tapped for ")
        && let Some(mc) = parse_braced_mana_color(mana_str)
    {
        return Some(def(
            EventKind::TappedForMana,
            with_attached_filter(EventFilterBuilder {
                tapped_mana_color: Some(mc),
                ..Default::default()
            }),
        ));
    }

    if !subject.one_or_more
        && let Some(ord_raw) = strip_prefix_word_ci(&verb_low, "attacks for the")
            .or_else(|| strip_prefix_word_ci(&verb_low, "attack for the"))
            .and_then(|r| strip_suffix_ws_ci(r, "time each turn"))
        && let Some(ord) = parse_extended_ordinal(ord_raw)
    {
        let mut b = with_attached_filter(EventFilterBuilder::default());
        if ord == 1 {
            b.first_time_each_turn = true;
        } else {
            b.nth_event_this_turn = Some(ord);
        }
        return Some(def(EventKind::Attacks, b));
    }

    if !subject.one_or_more
        && let Some(mid) = strip_prefix_word_ci(&verb_low, "becomes the target of")
            .or_else(|| strip_prefix_word_ci(&verb_low, "become the target of"))
            .and_then(|r| strip_suffix_ws_ci(r, "for the first time each turn"))
    {
        for (kind_str, kind_val) in [
            ("a spell or ability", TargetedByKind::SpellOrAbility),
            ("a spell", TargetedByKind::Spell),
            ("an ability", TargetedByKind::Ability),
        ] {
            if let Some(ctrl_raw) = mid.strip_prefix(kind_str) {
                let ctrl = ctrl_raw.trim();
                let player = match ctrl {
                    "" => None,
                    "you control" => Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    "an opponent controls" | "your opponents control" | "you don't control" => {
                        Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        })
                    }
                    _ => continue,
                };
                if ctrl == "you control" {
                    return Some(def(
                        EventKind::BecomesTarget,
                        EventFilterBuilder {
                            targeted_by: Some(kind_val),
                            player,
                            first_time_targeted_by_you_this_turn: true,
                            ..Default::default()
                        },
                    ));
                } else {
                    return Some(def(
                        EventKind::BecomesTarget,
                        with_attached_filter(EventFilterBuilder {
                            targeted_by: Some(kind_val),
                            player,
                            first_time_each_turn: true,
                            ..Default::default()
                        }),
                    ));
                }
            }
        }
    }

    if !subject.one_or_more
        && let Some(after_deals) =
            strip_prefix_word_ci(verb, "deals").or_else(|| strip_prefix_word_ci(verb, "deal"))
    {
        let (combat_opt, after_kind) = if let Some(r) = strip_prefix_word_ci(after_deals, "combat")
        {
            (Some(true), r)
        } else if let Some(r) = strip_prefix_word_ci(after_deals, "noncombat") {
            (Some(false), r)
        } else {
            (None, after_deals)
        };
        if let Some(after_dmg) = strip_prefix_word_ci(after_kind, "damage to")
            && let Some(tgt_raw) = strip_prefix_word_ci(after_dmg, "a")
                .or_else(|| strip_prefix_word_ci(after_dmg, "an"))
            && !contains_word_ci(tgt_raw, "player")
            && !contains_word_ci(tgt_raw, "opponent")
            && let Some(tgt_filter) = parse_trigger_object_phrase(tgt_raw, ctx, false)
        {
            return Some(def(
                EventKind::DealsDamage,
                with_attached_filter(EventFilterBuilder {
                    combat: combat_opt,
                    damage_target_kind: Some(DamageTargetKind::Permanent),
                    damage_target_filter: Some(Box::new(tgt_filter)),
                    ..Default::default()
                }),
            ));
        }
    }

    if !subject.one_or_more {
        if let Some(rest) = strip_prefix_word_ci(verb, "blocks or becomes blocked by")
            .or_else(|| strip_prefix_word_ci(verb, "blocks or become blocked by"))
            .or_else(|| strip_prefix_word_ci(verb, "block or becomes blocked by"))
            .or_else(|| strip_prefix_word_ci(verb, "block or become blocked by"))
            && let Some(host_raw) =
                strip_prefix_word_ci(rest, "a").or_else(|| strip_prefix_word_ci(rest, "an"))
            && let Some(host) = parse_trigger_object_phrase(host_raw, ctx, false)
        {
            return Some(def(
                EventKind::BlocksOrBecomesBlocked,
                with_attached_filter(EventFilterBuilder {
                    host: Some(Box::new(host)),
                    ..Default::default()
                }),
            ));
        }
        if let Some(rest) =
            strip_prefix_word_ci(verb, "blocks").or_else(|| strip_prefix_word_ci(verb, "block"))
            && let Some(host_raw) =
                strip_prefix_word_ci(rest, "a").or_else(|| strip_prefix_word_ci(rest, "an"))
            && let Some(host) = parse_trigger_object_phrase(host_raw, ctx, false)
        {
            return Some(def(
                EventKind::Blocks,
                with_attached_filter(EventFilterBuilder {
                    host: Some(Box::new(host)),
                    ..Default::default()
                }),
            ));
        }
        if let Some(rest) = strip_prefix_word_ci(verb, "becomes blocked by")
            .or_else(|| strip_prefix_word_ci(verb, "become blocked by"))
            && let Some(host_raw) =
                strip_prefix_word_ci(rest, "a").or_else(|| strip_prefix_word_ci(rest, "an"))
            && let Some(host) = parse_trigger_object_phrase(host_raw, ctx, false)
        {
            return Some(def(
                EventKind::BecomesBlocked,
                with_attached_filter(EventFilterBuilder {
                    host: Some(Box::new(host)),
                    ..Default::default()
                }),
            ));
        }
    }

    if (verb_low == "transforms into ~" || verb_low == "transform into ~")
        && matches!(subject.object, TriggerSubject::SelfObj)
    {
        return Some(def(EventKind::Transforms, EventFilterBuilder::default()));
    }
    if !subject.one_or_more
        && (verb_low == "becomes attached to ~" || verb_low == "become attached to ~")
    {
        return Some(def(
            EventKind::Attached,
            EventFilterBuilder {
                host_self: true,
                ..Default::default()
            },
        ));
    }

    if !subject.one_or_more
        && let Some(rest) = strip_prefix_word_ci(&verb_low, "saddles a mount")
            .or_else(|| strip_prefix_word_ci(&verb_low, "saddle a mount"))
    {
        if rest.is_empty() {
            return Some(def(EventKind::SaddledMount, EventFilterBuilder::default()));
        }
        if rest == "during your main phase" {
            return Some(def(
                EventKind::SaddledMount,
                EventFilterBuilder {
                    during_main_phase: true,
                    during_turn: Some(DuringTurnKind::Your),
                    ..Default::default()
                },
            ));
        }
        if rest == "during main phase" {
            return Some(def(
                EventKind::SaddledMount,
                EventFilterBuilder {
                    during_main_phase: true,
                    ..Default::default()
                },
            ));
        }
    }
    if !subject.one_or_more
        && let Some(rest) = strip_prefix_word_ci(&verb_low, "crews a vehicle")
            .or_else(|| strip_prefix_word_ci(&verb_low, "crew a vehicle"))
    {
        if rest.is_empty() {
            return Some(def(EventKind::CrewedVehicle, EventFilterBuilder::default()));
        }
        if rest == "during your main phase" {
            return Some(def(
                EventKind::CrewedVehicle,
                EventFilterBuilder {
                    during_main_phase: true,
                    during_turn: Some(DuringTurnKind::Your),
                    ..Default::default()
                },
            ));
        }
        if rest == "during main phase" {
            return Some(def(
                EventKind::CrewedVehicle,
                EventFilterBuilder {
                    during_main_phase: true,
                    ..Default::default()
                },
            ));
        }
    }

    if !subject.one_or_more
        && let Some(rest) = strip_prefix_word_ci(verb, "becomes attached to")
            .or_else(|| strip_prefix_word_ci(verb, "become attached to"))
        && let Some(host_raw) =
            strip_prefix_word_ci(rest, "a").or_else(|| strip_prefix_word_ci(rest, "an"))
        && let Some(host) = parse_trigger_object_phrase(host_raw, ctx, false)
    {
        return Some(def(
            EventKind::Attached,
            EventFilterBuilder {
                host: Some(Box::new(host)),
                ..Default::default()
            },
        ));
    }

    if let Some(rest) = strip_prefix_word_ci(verb, "transforms into")
        .or_else(|| strip_prefix_word_ci(verb, "transform into"))
    {
        let phrase = strip_prefix_word_ci(rest, "a")
            .or_else(|| strip_prefix_word_ci(rest, "an"))
            .unwrap_or(rest)
            .trim();
        if let Some(qual) = parse_trigger_object_phrase(phrase, ctx, false) {
            return Some(def(
                EventKind::Transforms,
                EventFilterBuilder {
                    to_face_filter: Some(Box::new(qual)),
                    ..Default::default()
                },
            ));
        }
    }

    if !subject.one_or_more
        && let Some(rest) = strip_prefix_word_ci(verb, "becomes the target of")
            .or_else(|| strip_prefix_word_ci(verb, "become the target of"))
    {
        for (kind_str, kind_val) in [
            ("a spell or ability", TargetedByKind::SpellOrAbility),
            ("a spell", TargetedByKind::Spell),
            ("an ability", TargetedByKind::Ability),
        ] {
            if let Some(ctrl_raw) = strip_prefix_ci(rest, kind_str) {
                let ctrl = ctrl_raw.trim();
                let player = if ctrl.is_empty() {
                    None
                } else if ctrl.eq_ignore_ascii_case("you control") {
                    Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    })
                } else if ctrl.eq_ignore_ascii_case("an opponent controls")
                    || ctrl.eq_ignore_ascii_case("your opponents control")
                    || ctrl.eq_ignore_ascii_case("you don't control")
                {
                    Some(PlayerFilter {
                        relation: PlayerRelation::Opponent,
                    })
                } else {
                    continue;
                };
                return Some(def(
                    EventKind::BecomesTarget,
                    EventFilterBuilder {
                        targeted_by: Some(kind_val),
                        player,
                        ..Default::default()
                    },
                ));
            }
        }

        if let Some(inner) =
            strip_prefix_word_ci(rest, "a").or_else(|| strip_prefix_word_ci(rest, "an"))
        {
            let (before_ctrl, player) = if let Some(l) = strip_suffix_ws_ci(inner, "you control") {
                (
                    l,
                    Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                )
            } else if let Some(l) = strip_suffix_ws_ci(inner, "an opponent controls")
                .or_else(|| strip_suffix_ws_ci(inner, "your opponents control"))
                .or_else(|| strip_suffix_ws_ci(inner, "you don't control"))
            {
                (
                    l,
                    Some(PlayerFilter {
                        relation: PlayerRelation::Opponent,
                    }),
                )
            } else {
                (inner, None)
            };

            let kind_match = [
                ("spell or ability", TargetedByKind::SpellOrAbility),
                ("spell", TargetedByKind::Spell),
                ("ability", TargetedByKind::Ability),
            ]
            .iter()
            .find_map(|(sfx, k)| strip_suffix_ws_ci(before_ctrl, sfx).map(|qual| (qual, *k)));

            if let Some((qual, targeted_by)) = kind_match
                && !contains_word_ci(qual, "backup")
            {
                let phrase = if targeted_by == TargetedByKind::Spell {
                    format!("{qual} spell")
                } else {
                    qual.to_string()
                };
                if let Some(src_filter) = parse_trigger_object_phrase(
                    &phrase,
                    ctx,
                    targeted_by != TargetedByKind::Ability,
                ) && src_filter != ObjectFilter::default()
                {
                    return Some(def(
                        EventKind::BecomesTarget,
                        EventFilterBuilder {
                            targeted_by: Some(targeted_by),
                            targeting_source_filter: Some(Box::new(src_filter)),
                            player,
                            ..Default::default()
                        },
                    ));
                }
            }
        }
    }

    None
}

fn split_verb_or_list(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            if i + 4 <= bytes.len()
                && bytes[i + 1] == b' '
                && bytes[i + 2..i + 4].eq_ignore_ascii_case(b"or")
                && i + 4 < bytes.len()
                && bytes[i + 4] == b' '
            {
                parts.push(s[start..i].trim());
                i += 5;
                start = i;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b' ' {
                parts.push(s[start..i].trim());
                i += 2;
                start = i;
                continue;
            }
        } else if i + 4 <= bytes.len()
            && bytes[i] == b' '
            && bytes[i + 1..i + 3].eq_ignore_ascii_case(b"or")
            && bytes[i + 3] == b' '
        {
            parts.push(s[start..i].trim());
            i += 4;
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(s[start..].trim());
    parts
}

fn object_events(
    verbs_raw: &str,
    subject: &TriggerSubjectParsed,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggerDef> {
    if let Some(whole) = object_event(verbs_raw, subject, ctx) {
        return Some(whole);
    }
    let verbs = normalize_apostrophes(verbs_raw.trim());
    let verbs_low = verbs.to_ascii_lowercase();
    if matches!(subject.object, TriggerSubject::SelfObj)
        && matches!(
            verbs_low.as_str(),
            "enters or the creature it haunts dies"
                | "enters or the creature ~ haunts dies"
                | "enters the battlefield or the creature it haunts dies"
                | "enters the battlefield or the creature ~ haunts dies"
        )
    {
        let mut haunt = make_trigger(
            EventKind::HauntedCreatureDies,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                ..Default::default()
            },
        );
        haunt.from_zones = Some(ZoneMask::EXILE);
        let mut main = make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                to: Some(Zone::Battlefield),
                ..Default::default()
            },
        );
        main.or.push(haunt);
        return Some(main);
    }
    if !contains_word_ci(&verbs, "or") {
        return None;
    }

    if let Some((left_verbs, trail)) = strip_suffix_ws_ci(&verbs, "during your main phase")
        .map(|l| (l, "during your main phase"))
        .or_else(|| {
            strip_suffix_ws_ci(&verbs, "during main phase").map(|l| (l, "during main phase"))
        })
        && (contains_word_ci(left_verbs, "saddles")
            || contains_word_ci(left_verbs, "saddle")
            || contains_word_ci(left_verbs, "crews")
            || contains_word_ci(left_verbs, "crew"))
    {
        let sub_parts = split_verb_or_list(left_verbs);
        if sub_parts.len() >= 2 {
            let mut defs = Vec::with_capacity(sub_parts.len());
            for p in sub_parts {
                let d = object_event(&format!("{p} {trail}"), subject, ctx)?;
                defs.push(d);
            }
            let mut first = defs.remove(0);
            first.or.extend(defs);
            return Some(first);
        }
    }

    let parts = split_verb_or_list(&verbs);
    if parts.len() < 2 {
        return None;
    }
    let mut defs = Vec::with_capacity(parts.len());
    for p in parts {
        defs.push(object_event(p, subject, ctx)?);
    }
    let mut first = defs.remove(0);
    first.or.extend(defs);
    Some(first)
}

fn split_optional_counter_prefix<'a>(
    raw: &'a str,
    interner: &mut Interner,
) -> Option<(Option<CounterId>, &'a str)> {
    // Returns (Some(ctr), "") if raw is "<ctr> " (already stripped), or checks raw directly
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some((None, ""));
    }
    if let Some(id) = parse_trigger_counter(trimmed, interner) {
        return Some((Some(id), ""));
    }
    None
}

fn counter_event(head: &str, ctx: &mut ParseCtx<'_>) -> Option<TriggerDef> {
    let head_norm = normalize_apostrophes(head.trim());
    let h = head_norm.as_str();

    // Counter removed: "(the last|one or more|a|an) [<ctr> ]counters? (is|are) removed from (~|<subj>)[ while it's exiled]"
    let (last_counter, rem_after_qty) = if let Some(r) = strip_prefix_word_ci(h, "the last") {
        (true, Some(r))
    } else if let Some(r) = strip_prefix_word_ci(h, "one or more")
        .or_else(|| strip_prefix_word_ci(h, "a"))
        .or_else(|| strip_prefix_word_ci(h, "an"))
    {
        (false, Some(r))
    } else {
        (false, None)
    };

    if let Some(after_qty) = rem_after_qty
        && let Some((ctr_part, target_raw_full)) =
            split_once_ws_word_ci(after_qty, "counters is removed from")
                .or_else(|| split_once_ws_word_ci(after_qty, "counters are removed from"))
                .or_else(|| split_once_ws_word_ci(after_qty, "counter is removed from"))
                .or_else(|| split_once_ws_word_ci(after_qty, "counter are removed from"))
                .or_else(|| {
                    strip_prefix_word_ci(after_qty, "counters is removed from")
                        .or_else(|| strip_prefix_word_ci(after_qty, "counters are removed from"))
                        .or_else(|| strip_prefix_word_ci(after_qty, "counter is removed from"))
                        .or_else(|| strip_prefix_word_ci(after_qty, "counter are removed from"))
                        .map(|t| ("", t))
                })
    {
        let cp = ctx.interner.checkpoint();
        if let Some((ctr, _)) = split_optional_counter_prefix(ctr_part, ctx.interner) {
            let (target_raw, while_exiled) =
                if let Some(left) = strip_suffix_ws_ci(target_raw_full, "while it's exiled") {
                    (left.trim(), true)
                } else {
                    (target_raw_full.trim(), false)
                };
            if target_raw == "~" {
                let mut t = make_trigger(
                    EventKind::CounterRemoved,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::SelfObj),
                        counter: ctr,
                        last_counter,
                        ..Default::default()
                    },
                );
                if while_exiled {
                    t.from_zones = Some(ZoneMask::EXILE);
                }
                return Some(t);
            }
            if !while_exiled
                && let Some(subj) = object_subject(target_raw, ctx)
                && matches!(subj.object, TriggerSubject::Filter { .. })
            {
                return Some(make_trigger(
                    EventKind::CounterRemoved,
                    EventFilterBuilder {
                        object: Some(subj.object),
                        counter: ctr,
                        last_counter,
                        ..Default::default()
                    },
                ));
            }
        }
        ctx.interner.rollback(cp);
    }

    // Ordinal counter placed on self: "the <ord> [<ctr> ]counter is (put|placed) on ~"
    if let Some(after_the) = strip_prefix_word_ci(h, "the")
        && let Some(ws) = after_the.find(char::is_whitespace)
    {
        let ord_word = &after_the[..ws];
        let rest_after_ord = after_the[ws..].trim_start();
        if let Some(ord) = parse_extended_ordinal(ord_word)
            && let Some(ctr_part) = strip_suffix_ws_ci(rest_after_ord, "counter is put on ~")
                .or_else(|| strip_suffix_ws_ci(rest_after_ord, "counter is placed on ~"))
                .or_else(|| {
                    (rest_after_ord.eq_ignore_ascii_case("counter is put on ~")
                        || rest_after_ord.eq_ignore_ascii_case("counter is placed on ~"))
                    .then_some("")
                })
        {
            let cp = ctx.interner.checkpoint();
            if let Some((ctr, _)) = split_optional_counter_prefix(ctr_part, ctx.interner) {
                return Some(make_trigger(
                    EventKind::CounterPlaced,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::SelfObj),
                        counter: ctr,
                        nth_counter_on_self: Some(ord),
                        ..Default::default()
                    },
                ));
            }
            ctx.interner.rollback(cp);
        }
    }

    // Counter placed: "(one or more|a|an) [<ctr> ]counters? (is|are) (put|placed) on (~|<subj>)"
    let (is_one_or_more, after_qty) = if let Some(r) = strip_prefix_word_ci(h, "one or more") {
        (true, r)
    } else {
        let r = strip_prefix_word_ci(h, "a").or_else(|| strip_prefix_word_ci(h, "an"))?;
        (false, r)
    };

    for sep in [
        "counters is put on",
        "counters are put on",
        "counter is put on",
        "counter are put on",
        "counters is placed on",
        "counters are placed on",
        "counter is placed on",
        "counter are placed on",
    ] {
        let matched = split_once_ws_word_ci(after_qty, sep)
            .or_else(|| strip_prefix_word_ci(after_qty, sep).map(|target| ("", target)));
        if let Some((ctr_part, target_raw)) = matched {
            let cp = ctx.interner.checkpoint();
            if let Some((ctr, _)) = split_optional_counter_prefix(ctr_part, ctx.interner) {
                let target_trimmed = target_raw.trim();
                if target_trimmed == "~" {
                    return Some(make_trigger(
                        EventKind::CounterPlaced,
                        EventFilterBuilder {
                            object: Some(TriggerSubject::SelfObj),
                            counter: ctr,
                            ..Default::default()
                        },
                    ));
                }
                if let Some(subj) = object_subject(target_trimmed, ctx)
                    && matches!(subj.object, TriggerSubject::Filter { .. })
                {
                    return Some(make_trigger(
                        EventKind::CounterPlaced,
                        EventFilterBuilder {
                            object: Some(subj.object),
                            counter: ctr,
                            one_or_more: is_one_or_more || subj.one_or_more,
                            ..Default::default()
                        },
                    ));
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    None
}

fn parse_state_trigger_body(body_raw: &str, ctx: &mut ParseCtx<'_>) -> Option<TriggerDef> {
    let body_norm = normalize_apostrophes(body_raw.trim());
    let body = body_norm.as_str();
    let make_state = |cond: Condition| -> TriggerDef {
        let mut t = make_trigger(
            EventKind::StateTriggered,
            EventFilterBuilder {
                state_condition: Some(Box::new(cond)),
                ..Default::default()
            },
        );
        t.is_state_trigger = true;
        t
    };

    if let Some(left) = strip_suffix_ws_ci(body, "and have no cards in hand")
        && let Some(noun) = strip_prefix_word_ci(left, "you control no")
        && let Some(c1) =
            parse_trigger_condition(&format!("if you control no {}", noun.trim()), false, ctx)
    {
        let c2 = Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        };
        return Some(make_state(Condition::And { of: vec![c1, c2] }));
    }
    if let Some(noun) = strip_prefix_word_ci(body, "you control no")
        && let Some(cond) =
            parse_trigger_condition(&format!("if you control no {}", noun.trim()), false, ctx)
    {
        return Some(make_state(cond));
    }
    if body.eq_ignore_ascii_case("a player other than ~'s owner controls it")
        || body.eq_ignore_ascii_case("a player other than ~'s owner controls ~")
    {
        return Some(make_state(Condition::ControlledByOtherThanOwner));
    }

    // "there are N or more <ctr> counters on ~" / "~ has N or more <ctr> counters on it"
    if let Some(rest) =
        strip_prefix_word_ci(body, "there are").or_else(|| strip_prefix_word_ci(body, "~ has"))
        && let Some((num_w, after_num)) = rest.split_once(char::is_whitespace)
        && let Some(cnt) = parse_small_i32(num_w)
        && cnt >= 1
        && let Some(after_or_more) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        && let Some(ctr_raw) = strip_suffix_ws_ci(after_or_more, "counters on ~")
            .or_else(|| strip_suffix_ws_ci(after_or_more, "counters on it"))
        && let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner)
    {
        return Some(make_state(Condition::Compare {
            left: Box::new(Amount::CountersOn {
                counter: CounterTarget::Specific(ctr),
                of: mtg_ir::Ref::SelfObj,
            }),
            op: CompareOp::Ge,
            right: const_amount(cnt),
        }));
    }

    // "there are no <ctr> counters on ~" / "~ has no <ctr> counters on it"
    if let Some(rest) = strip_prefix_word_ci(body, "there are no")
        .or_else(|| strip_prefix_word_ci(body, "~ has no"))
        && let Some(ctr_raw) = strip_suffix_ws_ci(rest, "counters on ~")
            .or_else(|| strip_suffix_ws_ci(rest, "counters on it"))
        && let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner)
    {
        return Some(make_state(Condition::Compare {
            left: Box::new(Amount::CountersOn {
                counter: CounterTarget::Specific(ctr),
                of: mtg_ir::Ref::SelfObj,
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        }));
    }

    // "you have N or (less|fewer) life" / "your life total is N or (less|fewer)"
    if let Some(rest) = strip_prefix_word_ci(body, "you have")
        .and_then(|r| {
            strip_suffix_ws_ci(r, "or less life").or_else(|| strip_suffix_ws_ci(r, "or fewer life"))
        })
        .or_else(|| {
            strip_prefix_word_ci(body, "your life total is").and_then(|r| {
                strip_suffix_ws_ci(r, "or less").or_else(|| strip_suffix_ws_ci(r, "or fewer"))
            })
        })
        && let Some(val) = parse_small_i32(rest)
    {
        return Some(make_state(Condition::Compare {
            left: Box::new(Amount::Life { of: PlayerRef::You }),
            op: CompareOp::Le,
            right: const_amount(val),
        }));
    }

    if body.eq_ignore_ascii_case("you have no cards in hand") {
        return Some(make_state(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        }));
    }

    None
}

// -----------------------------------------------------------------------------
// Player Events & Wave 9 Trigger Heads
// -----------------------------------------------------------------------------

fn parse_zone_name(s: &str) -> Option<Zone> {
    match s.trim().to_ascii_lowercase().as_str() {
        "library" => Some(Zone::Library),
        "hand" => Some(Zone::Hand),
        "battlefield" => Some(Zone::Battlefield),
        "graveyard" => Some(Zone::Graveyard),
        "exile" => Some(Zone::Exile),
        "command" => Some(Zone::Command),
        "stack" => Some(Zone::Stack),
        _ => None,
    }
}

fn merge_compound_trigger_defs(mut left: TriggerDef, mut right: TriggerDef) -> TriggerDef {
    let mut right_alts = Vec::new();
    let tail = std::mem::take(&mut right.or);
    right_alts.push(right);
    right_alts.extend(tail);
    left.or.extend(right_alts);
    left
}

fn player_event(
    player: PlayerFilter,
    rest_raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggerDef> {
    let rest_norm = normalize_apostrophes(rest_raw.trim());
    let rest = rest_norm.as_str();
    let rest_low = rest.to_ascii_lowercase();
    let rel = player.relation;

    if rel == PlayerRelation::You && rest == "cycle ~" {
        let mut t = make_trigger(
            EventKind::Cycle,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                ..Default::default()
            },
        );
        t.from_zones = Some(ZoneMask::ALL);
        return Some(t);
    }
    if rel == PlayerRelation::You && rest_low == "cast or cycle ~" {
        let mut cyc = make_trigger(
            EventKind::Cycle,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                ..Default::default()
            },
        );
        cyc.from_zones = Some(ZoneMask::ALL);
        let mut cast = make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                ..Default::default()
            },
        );
        cast.or.push(cyc);
        return Some(cast);
    }
    if rest == "discard ~" || rest == "discards ~" {
        let mut t = make_trigger(
            EventKind::Discard,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                ..Default::default()
            },
        );
        t.from_zones = Some(ZoneMask::ALL);
        return Some(t);
    }
    if let Some(target) = rest
        .strip_prefix("cycle or discard ")
        .or_else(|| rest.strip_prefix("cycles or discards "))
        && let Some((obj, one_or_more)) = discarded_card_filter(target, ctx)
    {
        return Some(make_trigger(
            EventKind::Discard,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                one_or_more,
                ..Default::default()
            },
        ));
    }
    if let Some(target) =
        strip_prefix_word_ci(rest, "cycles").or_else(|| strip_prefix_word_ci(rest, "cycle"))
        && let Some((obj, one_or_more)) = discarded_card_filter(target, ctx)
        && !one_or_more
    {
        return Some(make_trigger(
            EventKind::Cycle,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                ..Default::default()
            },
        ));
    }
    if let Some(target) =
        strip_prefix_word_ci(rest, "discards").or_else(|| strip_prefix_word_ci(rest, "discard"))
        && let Some((obj, one_or_more)) = discarded_card_filter(target, ctx)
    {
        return Some(make_trigger(
            EventKind::Discard,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                one_or_more,
                ..Default::default()
            },
        ));
    }

    // "casts (your|their) (first|second|third) spell (each turn|in a turn)"
    if let Some(after_cast) =
        strip_prefix_word_ci(&rest_low, "casts").or_else(|| strip_prefix_word_ci(&rest_low, "cast"))
    {
        for (poss, is_your) in [("your ", true), ("their ", false)] {
            if let Some(after_poss) = after_cast.strip_prefix(poss) {
                for (ord_w, ord_v) in [("first ", 1u16), ("second ", 2u16), ("third ", 3u16)] {
                    if let Some(after_ord) = after_poss.strip_prefix(ord_w)
                        && (after_ord == "spell each turn" || after_ord == "spell in a turn")
                    {
                        if is_your != (rel == PlayerRelation::You) {
                            return None;
                        }
                        return Some(make_trigger(
                            EventKind::Cast,
                            EventFilterBuilder {
                                object: Some(TriggerSubject::Filter {
                                    filter: Box::new(ObjectFilter::default()),
                                }),
                                player: Some(player),
                                nth_this_turn: Some(ord_v),
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
        }
    }

    // Filtered & timing-scoped ordinal cast triggers
    if let Some(after_cast) =
        strip_prefix_word_ci(rest, "casts").or_else(|| strip_prefix_word_ci(rest, "cast"))
    {
        let (poss_is_your, after_poss) = if let Some(r) = strip_prefix_word_ci(after_cast, "your") {
            (Some(true), r)
        } else if let Some(r) = strip_prefix_word_ci(after_cast, "their") {
            (Some(false), r)
        } else {
            (None, after_cast)
        };
        if let Some(is_your) = poss_is_your
            && let Some((ord_w, after_ord)) = after_poss.split_once(char::is_whitespace)
            && let Some(ord) = parse_ordinal_123(ord_w)
        {
            let timings = [
                ("each turn", None),
                ("in a turn", None),
                (
                    "during each opponent's turn",
                    Some(DuringTurnKind::Opponent),
                ),
                ("during an opponent's turn", Some(DuringTurnKind::Opponent)),
                ("during each of your turns", Some(DuringTurnKind::Your)),
                ("during your turn", Some(DuringTurnKind::Your)),
                ("during their turn", Some(DuringTurnKind::EventPlayer)),
            ];
            for (timing_sfx, during_turn) in timings {
                if let Some(before_timing) = strip_suffix_ws_ci(after_ord.trim_start(), timing_sfx)
                {
                    let (before_zone, cast_from_zone) = [
                        ("from your graveyard", Zone::Graveyard),
                        ("from their graveyard", Zone::Graveyard),
                        ("from a graveyard", Zone::Graveyard),
                        ("from graveyard", Zone::Graveyard),
                        ("from your exile", Zone::Exile),
                        ("from their exile", Zone::Exile),
                        ("from a exile", Zone::Exile),
                        ("from exile", Zone::Exile),
                    ]
                    .iter()
                    .find_map(|(zs, z)| {
                        strip_suffix_ws_ci(before_timing, zs).map(|l| (l, Some(*z)))
                    })
                    .unwrap_or((before_timing, None));

                    if let Some((pre_mod, post_mod)) = split_once_ws_word_ci(before_zone, "spell")
                        .or_else(|| {
                            strip_prefix_word_ci(before_zone, "spell")
                                .map(|post| ("", post))
                                .or_else(|| {
                                    strip_suffix_ws_ci(before_zone, "spell").map(|pre| (pre, ""))
                                })
                                .or_else(|| {
                                    before_zone
                                        .eq_ignore_ascii_case("spell")
                                        .then_some(("", ""))
                                })
                        })
                    {
                        if is_your != (rel == PlayerRelation::You) {
                            return None;
                        }
                        let obj_filter = if pre_mod.is_empty() && post_mod.is_empty() {
                            ObjectFilter::default()
                        } else {
                            let raw_spell = format!(
                                "a {}{}{}",
                                if pre_mod.is_empty() {
                                    String::new()
                                } else {
                                    format!("{pre_mod} ")
                                },
                                "spell",
                                if post_mod.is_empty() {
                                    String::new()
                                } else {
                                    format!(" {post_mod}")
                                }
                            );
                            let Some(TriggerSubject::Filter { filter }) =
                                spell_filter(&raw_spell, ctx)
                            else {
                                return None;
                            };
                            *filter
                        };
                        return Some(make_trigger(
                            EventKind::Cast,
                            EventFilterBuilder {
                                object: Some(TriggerSubject::Filter {
                                    filter: Box::new(obj_filter),
                                }),
                                player: Some(player),
                                spell_ordinal: Some(ord as i8),
                                cast_from_zone,
                                during_turn,
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
        }
    }

    // "casts a [<mod> ]spell [during (your|their) turn ]other than (your|their|the) first [<mod2> ]spell[ (they|that player|you) casts?] (each|that) turn"
    if let Some(after_cast) =
        strip_prefix_word_ci(rest, "casts").or_else(|| strip_prefix_word_ci(rest, "cast"))
        && let Some(after_a) = strip_prefix_word_ci(after_cast, "a")
        && let Some((lhs_mod_raw, rhs_raw)) = split_once_ws_word_ci(after_a, "other than")
    {
        let (spell_side, during_turn) =
            if let Some(l) = strip_suffix_ws_ci(lhs_mod_raw, "during your turn") {
                (l, Some(DuringTurnKind::Your))
            } else if let Some(l) = strip_suffix_ws_ci(lhs_mod_raw, "during their turn") {
                (l, Some(DuringTurnKind::EventPlayer))
            } else {
                (lhs_mod_raw, None)
            };
        let mod1 = if spell_side.eq_ignore_ascii_case("spell") {
            Some("")
        } else {
            strip_suffix_ws_ci(spell_side, "spell")
        };
        let rhs_after_poss = strip_prefix_word_ci(rhs_raw, "your")
            .or_else(|| strip_prefix_word_ci(rhs_raw, "their"))
            .or_else(|| strip_prefix_word_ci(rhs_raw, "the"))
            .and_then(|r| strip_prefix_word_ci(r, "first"));
        let rhs_before_turn = rhs_after_poss.and_then(|r| {
            strip_suffix_ws_ci(r, "each turn").or_else(|| strip_suffix_ws_ci(r, "that turn"))
        });
        let rhs_before_casts = rhs_before_turn.map(|r| {
            [
                "they casts",
                "they cast",
                "that player casts",
                "that player cast",
                "you casts",
                "you cast",
            ]
            .iter()
            .find_map(|sfx| strip_suffix_ws_ci(r, sfx))
            .unwrap_or(r)
        });
        let mod2 = rhs_before_casts.and_then(|r| {
            if r.eq_ignore_ascii_case("spell") {
                Some("")
            } else {
                strip_suffix_ws_ci(r, "spell")
            }
        });
        if let (Some(m1), Some(m2)) = (mod1, mod2)
            && m1.eq_ignore_ascii_case(m2)
        {
            let obj_filter = if m1.is_empty() {
                ObjectFilter::default()
            } else {
                let Some(TriggerSubject::Filter { filter }) =
                    spell_filter(&format!("a {m1} spell"), ctx)
                else {
                    return None;
                };
                *filter
            };
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj_filter),
                    }),
                    player: Some(player),
                    spell_ordinal: Some(-1),
                    during_turn,
                    ..Default::default()
                },
            ));
        }
    }

    fn parse_cast_verb(s: &str) -> Option<(bool, &str)> {
        if let Some(r) = strip_prefix_word_ci(s, "cast or copy")
            .or_else(|| strip_prefix_word_ci(s, "casts or copies"))
        {
            Some((true, r))
        } else if let Some(r) =
            strip_prefix_word_ci(s, "casts").or_else(|| strip_prefix_word_ci(s, "cast"))
        {
            Some((false, r))
        } else {
            None
        }
    }

    if let Some((or_copy, after_verb)) = parse_cast_verb(rest) {
        if let Some(spell_raw) = strip_suffix_ws_ci(after_verb, "during your turn") {
            let obj = spell_filter(spell_raw, ctx)?;
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    during_turn: Some(DuringTurnKind::Your),
                    or_copy,
                    ..Default::default()
                },
            ));
        }
        if let Some(spell_raw) = strip_suffix_ws_ci(after_verb, "that targets only ~")
            && let Some(obj) = spell_filter(spell_raw, ctx)
            && !matches!(obj, TriggerSubject::SelfObj)
        {
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    targets_only_self: true,
                    or_copy,
                    ..Default::default()
                },
            ));
        }
        if let Some(after_a) =
            strip_prefix_word_ci(after_verb, "a").or_else(|| strip_prefix_word_ci(after_verb, "an"))
        {
            let (is_other, after_other) = if let Some(r) = strip_prefix_word_ci(after_a, "other") {
                (true, r)
            } else {
                (false, after_a)
            };
            if let Some(after_sw) = strip_prefix_word_ci(after_other, "spell with") {
                let (kw_raw, cast_from_zone) = [
                    ("from your graveyard", Zone::Graveyard),
                    ("from their graveyard", Zone::Graveyard),
                    ("from a graveyard", Zone::Graveyard),
                    ("from graveyard", Zone::Graveyard),
                    ("from your exile", Zone::Exile),
                    ("from their exile", Zone::Exile),
                    ("from a exile", Zone::Exile),
                    ("from exile", Zone::Exile),
                    ("from your library", Zone::Library),
                    ("from their library", Zone::Library),
                    ("from a library", Zone::Library),
                    ("from library", Zone::Library),
                    ("from your hand", Zone::Hand),
                    ("from their hand", Zone::Hand),
                    ("from a hand", Zone::Hand),
                    ("from hand", Zone::Hand),
                ]
                .iter()
                .find_map(|(sfx, z)| strip_suffix_ws_ci(after_sw, sfx).map(|l| (l, Some(*z))))
                .unwrap_or((after_sw, None));
                let raw_kw = kw_raw.trim().to_ascii_lowercase();
                if !raw_kw.is_empty()
                    && raw_kw != "the chosen name"
                    && !starts_with_word_ci(&raw_kw, "mana value")
                    && !starts_with_word_ci(&raw_kw, "power")
                    && !starts_with_word_ci(&raw_kw, "toughness")
                    && raw_kw
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b' ' || b == b'-')
                {
                    let sf_phrase = format!(
                        "a {}spell with {}",
                        if is_other { "other " } else { "" },
                        kw_raw.trim()
                    );
                    let sf = spell_filter(&sf_phrase, ctx);
                    let (obj, spell_has_keyword) = match sf {
                        Some(TriggerSubject::Filter { filter }) => (*filter, None),
                        _ => (
                            if is_other {
                                ObjectFilter {
                                    flags: FilterFlags::OTHER,
                                    ..Default::default()
                                }
                            } else {
                                ObjectFilter::default()
                            },
                            Some(ctx.interner.intern_keyword(&raw_kw)),
                        ),
                    };
                    return Some(make_trigger(
                        EventKind::Cast,
                        EventFilterBuilder {
                            object: Some(TriggerSubject::Filter {
                                filter: Box::new(obj),
                            }),
                            player: Some(player),
                            spell_has_keyword,
                            cast_from_zone,
                            or_copy,
                            ..Default::default()
                        },
                    ));
                }
            }
        }
        if let Some(spell_raw) = strip_suffix_ws_ci(after_verb, "that targets ~") {
            let obj = spell_filter(spell_raw, ctx)?;
            if matches!(obj, TriggerSubject::SelfObj) {
                return None;
            }
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    targets_self: true,
                    or_copy,
                    ..Default::default()
                },
            ));
        }
        if let Some(spell_raw) = strip_suffix_ws_ci(after_verb, "during an opponent's turn") {
            let obj = spell_filter(spell_raw, ctx)?;
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    during_opponent_turn: true,
                    or_copy,
                    ..Default::default()
                },
            ));
        }
        if let Some(spell_raw) =
            strip_suffix_ws_ci(after_verb, "from anywhere other than your hand")
                .or_else(|| strip_suffix_ws_ci(after_verb, "from anywhere other than their hand"))
                .or_else(|| {
                    strip_suffix_ws_ci(after_verb, "from anywhere other than its owner's hand")
                })
        {
            let obj = spell_filter(spell_raw, ctx)?;
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    cast_from_not_hand: true,
                    or_copy,
                    ..Default::default()
                },
            ));
        }
        if let Some((spell_raw, z)) = [
            ("from your graveyard", Zone::Graveyard),
            ("from a graveyard", Zone::Graveyard),
            ("from their graveyard", Zone::Graveyard),
            ("from graveyard", Zone::Graveyard),
            ("from your exile", Zone::Exile),
            ("from a exile", Zone::Exile),
            ("from their exile", Zone::Exile),
            ("from exile", Zone::Exile),
            ("from your library", Zone::Library),
            ("from a library", Zone::Library),
            ("from their library", Zone::Library),
            ("from library", Zone::Library),
            ("from your hand", Zone::Hand),
            ("from a hand", Zone::Hand),
            ("from their hand", Zone::Hand),
            ("from hand", Zone::Hand),
        ]
        .iter()
        .find_map(|(sfx, z)| strip_suffix_ws_ci(after_verb, sfx).map(|l| (l, *z)))
        {
            let obj = spell_filter(spell_raw, ctx)?;
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    player: Some(player),
                    cast_from_zone: Some(z),
                    or_copy,
                    ..Default::default()
                },
            ));
        }
    }

    if rel == PlayerRelation::You
        && let Some(spell_raw) =
            strip_prefix_word_ci(rest, "next cast").and_then(|r| strip_suffix_ws_ci(r, "this turn"))
        && let Some(obj) = spell_filter(spell_raw, ctx)
        && matches!(obj, TriggerSubject::Filter { .. })
    {
        let mut t = make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(obj),
                player: Some(player),
                ..Default::default()
            },
        );
        t.spell_delayed = Some(SpellDelayedDuration::NextThisTurn);
        return Some(t);
    }
    if let Some((or_copy, after_verb)) = parse_cast_verb(rest)
        && let Some(spell_raw) = strip_suffix_ws_ci(after_verb, "this turn")
        && let Some(obj) = spell_filter(spell_raw, ctx)
        && matches!(obj, TriggerSubject::Filter { .. })
    {
        let mut t = make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(obj),
                player: Some(player),
                or_copy,
                ..Default::default()
            },
        );
        t.spell_delayed = Some(SpellDelayedDuration::ThisTurn);
        return Some(t);
    }
    if let Some((or_copy, after_verb)) = parse_cast_verb(rest)
        && !after_verb.is_empty()
    {
        let obj = spell_filter(after_verb, ctx)?;
        return Some(make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(obj),
                player: Some(player),
                or_copy,
                ..Default::default()
            },
        ));
    }

    if rest_low == "attack" || rest_low == "attacks" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "attacks the player with the most life"
            | "attack the player with the most life"
            | "attacks a player with the most life"
            | "attack a player with the most life"
            | "attacks the player with the most life or tied for most life"
            | "attack the player with the most life or tied for most life"
            | "attacks a player with the most life or tied for most life"
            | "attack a player with the most life or tied for most life"
    ) {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::PlayerWithMostLife),
                ..Default::default()
            },
        ));
    }
    if rest_low == "attacks you" || rest_low == "attack you" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::You),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "attacks you or a planeswalker you control"
            | "attack you or a planeswalker you control"
            | "attacks you and/or a planeswalker you control"
            | "attack you and/or a planeswalker you control"
            | "attacks you or one or more planeswalkers you control"
            | "attack you or one or more planeswalkers you control"
            | "attacks you and/or one or more planeswalkers you control"
            | "attack you and/or one or more planeswalkers you control"
    ) {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::YouOrPlaneswalkerYouControl),
                ..Default::default()
            },
        ));
    }
    if rel == PlayerRelation::You && rest_low == "attack an opponent" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::Opponent),
                ..Default::default()
            },
        ));
    }
    if rel == PlayerRelation::You && rest_low == "attack a player" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                per_attacked_player: true,
                ..Default::default()
            },
        ));
    }
    if rest_low == "attacks enchanted player" || rest_low == "attack enchanted player" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::EnchantedPlayer),
                ..Default::default()
            },
        ));
    }
    if let Some(mid) = strip_prefix_word_ci(&rest_low, "attacks enchanted player with")
        .or_else(|| strip_prefix_word_ci(&rest_low, "attack enchanted player with"))
        && let Some(num_w) = strip_suffix_ws_ci(mid, "or more creatures")
        && let Some(min_n) = parse_small_u16(num_w)
        && min_n >= 1
    {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(player),
                one_or_more: true,
                min_attackers: Some(min_n),
                attacked_target: Some(AttackedTargetFilter::EnchantedPlayer),
                ..Default::default()
            },
        ));
    }
    if rel == PlayerRelation::You
        && let Some((is_per_player, after_tgt)) =
            strip_prefix_word_ci(rest, "attacks a player with")
                .or_else(|| strip_prefix_word_ci(rest, "attack a player with"))
                .map(|r| (true, r))
                .or_else(|| {
                    strip_prefix_word_ci(rest, "attacks an opponent with")
                        .or_else(|| strip_prefix_word_ci(rest, "attack an opponent with"))
                        .map(|r| (false, r))
                })
    {
        let parsed = if let Some((num_w, after_num)) = after_tgt.split_once(char::is_whitespace)
            && let Some(min_n) = parse_small_u16(num_w)
            && let Some(obj_raw) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((min_n, obj_raw))
        } else {
            strip_prefix_word_ci(after_tgt, "a")
                .or_else(|| strip_prefix_word_ci(after_tgt, "an"))
                .map(|obj_raw| (1u16, obj_raw))
        };
        if let Some((min_n, obj_raw)) = parsed
            && min_n >= 1
            && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
        {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    player: Some(player),
                    one_or_more: true,
                    min_attackers: Some(min_n),
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj),
                    }),
                    per_attacked_player: is_per_player,
                    attacked_target: (!is_per_player).then_some(AttackedTargetFilter::Opponent),
                    ..Default::default()
                },
            ));
        }
    }

    if rest == "gain life" || rest == "gains life" {
        return Some(make_trigger(
            EventKind::GainLife,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(ord_raw) = strip_prefix_word_ci(&rest_low, "gains life for the")
        .or_else(|| strip_prefix_word_ci(&rest_low, "gain life for the"))
        .and_then(|r| strip_suffix_ws_ci(r, "time each turn"))
        && let Some(ord) = parse_extended_ordinal(ord_raw)
    {
        let mut b = EventFilterBuilder {
            player: Some(player),
            ..Default::default()
        };
        if ord == 1 {
            b.first_time_each_turn = true;
        } else {
            b.nth_event_this_turn = Some(ord);
        }
        return Some(make_trigger(EventKind::GainLife, b));
    }
    if let Some(ord_raw) = strip_prefix_word_ci(&rest_low, "loses life for the")
        .or_else(|| strip_prefix_word_ci(&rest_low, "lose life for the"))
        .and_then(|r| strip_suffix_ws_ci(r, "time each turn"))
        && let Some(ord) = parse_extended_ordinal(ord_raw)
    {
        let mut b = EventFilterBuilder {
            player: Some(player),
            ..Default::default()
        };
        if ord == 1 {
            b.first_time_each_turn = true;
        } else {
            b.nth_event_this_turn = Some(ord);
        }
        return Some(make_trigger(EventKind::LoseLife, b));
    }
    if rest == "lose life" || rest == "loses life" {
        return Some(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "loses life during your turn" || rest_low == "lose life during your turn" {
        return Some(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn: Some(DuringTurnKind::Your),
                ..Default::default()
            },
        ));
    }
    if rest_low == "loses life during their turn" || rest_low == "lose life during their turn" {
        return Some(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn: Some(DuringTurnKind::EventPlayer),
                ..Default::default()
            },
        ));
    }
    if rest_low == "gains life during your turn" || rest_low == "gain life during your turn" {
        return Some(make_trigger(
            EventKind::GainLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn: Some(DuringTurnKind::Your),
                ..Default::default()
            },
        ));
    }
    if rest_low == "gains life during their turn" || rest_low == "gain life during their turn" {
        return Some(make_trigger(
            EventKind::GainLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn: Some(DuringTurnKind::EventPlayer),
                ..Default::default()
            },
        ));
    }
    if let Some((base_phrase, during_turn)) = match rest_low.as_str() {
        "gain or lose life" | "gains or loses life" => Some((true, None)),
        "gain or lose life during your turn" | "gains or loses life during your turn" => {
            Some((true, Some(DuringTurnKind::Your)))
        }
        "gain or lose life during their turn" | "gains or loses life during their turn" => {
            Some((true, Some(DuringTurnKind::EventPlayer)))
        }
        _ => None,
    } && base_phrase
    {
        let mut t = make_trigger(
            EventKind::GainLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn,
                ..Default::default()
            },
        );
        t.or.push(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(player),
                during_turn,
                ..Default::default()
            },
        ));
        return Some(t);
    }
    if rest_low == "scry or surveil" || rest_low == "scries or surveils" {
        let mut t = make_trigger(
            EventKind::Scry,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        );
        t.or.push(make_trigger(
            EventKind::Surveil,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
        return Some(t);
    }
    if matches!(
        rest_low.as_str(),
        "faces a dilemma"
            | "face a dilemma"
            | "faces a villainous choice"
            | "face a villainous choice"
    ) {
        return Some(make_trigger(
            EventKind::FacedDilemma,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "activates an exhaust ability" || rest_low == "activate an exhaust ability" {
        return Some(make_trigger(
            EventKind::ExhaustActivated,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "collects evidence" || rest_low == "collect evidence" {
        return Some(make_trigger(
            EventKind::CollectEvidence,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "plays a land" || rest_low == "play a land" {
        return Some(make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "plays a land from exile" || rest_low == "play a land from exile" {
        return Some(make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                cast_from_zone: Some(Zone::Exile),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "plays a land from anywhere other than your hand"
            | "play a land from anywhere other than your hand"
            | "plays a land from anywhere other than their hand"
            | "play a land from anywhere other than their hand"
    ) {
        return Some(make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                cast_from_not_hand: true,
                ..Default::default()
            },
        ));
    }
    if rest_low == "plays a card from exile" || rest_low == "play a card from exile" {
        let mut t = make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                cast_from_zone: Some(Zone::Exile),
                ..Default::default()
            },
        );
        t.or.push(make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter::default()),
                }),
                player: Some(player),
                cast_from_zone: Some(Zone::Exile),
                ..Default::default()
            },
        ));
        return Some(t);
    }
    if matches!(
        rest_low.as_str(),
        "plays a card from anywhere other than your hand"
            | "play a card from anywhere other than your hand"
            | "plays a card from anywhere other than their hand"
            | "play a card from anywhere other than their hand"
    ) {
        let mut t = make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                cast_from_not_hand: true,
                ..Default::default()
            },
        );
        t.or.push(make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter::default()),
                }),
                player: Some(player),
                cast_from_not_hand: true,
                ..Default::default()
            },
        ));
        return Some(t);
    }
    if let Some(after_play) =
        strip_prefix_word_ci(rest, "plays").or_else(|| strip_prefix_word_ci(rest, "play"))
        && let Some(land_phrase) =
            strip_prefix_word_ci(after_play, "a").or_else(|| strip_prefix_word_ci(after_play, "an"))
        && contains_word_ci(land_phrase, "land")
        && let Some(obj) = parse_trigger_object_phrase(land_phrase, ctx, false)
    {
        return Some(make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                ..Default::default()
            },
        ));
    }
    if rest_low == "commits a crime during your turn"
        || rest_low == "commit a crime during your turn"
    {
        return Some(make_trigger(
            EventKind::CommittedCrime,
            EventFilterBuilder {
                player: Some(player),
                during_turn: Some(DuringTurnKind::Your),
                ..Default::default()
            },
        ));
    }
    if rest == "tap ~ for mana" || rest == "taps ~ for mana" {
        return Some(make_trigger(
            EventKind::TappedForMana,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                ..Default::default()
            },
        ));
    }

    // "puts (one or more|a|an) [<ctr> ]counters? on (~|<subj>)"
    if let Some(after_puts) =
        strip_prefix_word_ci(rest, "puts").or_else(|| strip_prefix_word_ci(rest, "put"))
    {
        let (is_one_or_more, after_qty) =
            if let Some(r) = strip_prefix_word_ci(after_puts, "one or more") {
                (true, Some(r))
            } else if let Some(r) = strip_prefix_word_ci(after_puts, "a")
                .or_else(|| strip_prefix_word_ci(after_puts, "an"))
            {
                (false, Some(r))
            } else {
                (false, None)
            };
        if let Some(after_q) = after_qty
            && let Some((ctr_part, target_raw)) = split_once_ws_word_ci(after_q, "counters on")
                .or_else(|| split_once_ws_word_ci(after_q, "counter on"))
                .or_else(|| {
                    strip_prefix_word_ci(after_q, "counters on")
                        .or_else(|| strip_prefix_word_ci(after_q, "counter on"))
                        .map(|t| ("", t))
                })
        {
            let cp = ctx.interner.checkpoint();
            if let Some((ctr, _)) = split_optional_counter_prefix(ctr_part, ctx.interner) {
                let put_by = match rel {
                    PlayerRelation::You => RelativeControl::You,
                    PlayerRelation::Opponent => RelativeControl::Opponent,
                    _ => RelativeControl::Any,
                };
                if target_raw.trim() == "~" {
                    return Some(make_trigger(
                        EventKind::CounterPlaced,
                        EventFilterBuilder {
                            object: Some(TriggerSubject::SelfObj),
                            put_by_player: Some(put_by),
                            counter: ctr,
                            ..Default::default()
                        },
                    ));
                }
                if let Some(subj) = object_subject(target_raw.trim(), ctx)
                    && matches!(subj.object, TriggerSubject::Filter { .. })
                {
                    return Some(make_trigger(
                        EventKind::CounterPlaced,
                        EventFilterBuilder {
                            object: Some(subj.object),
                            put_by_player: Some(put_by),
                            counter: ctr,
                            one_or_more: is_one_or_more || subj.one_or_more,
                            ..Default::default()
                        },
                    ));
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    if rel == PlayerRelation::You
        && let Some(after_with) = strip_prefix_word_ci(rest, "attacks with ~ and")
            .or_else(|| strip_prefix_word_ci(rest, "attack with ~ and"))
    {
        let co_phrase = if starts_with_word_ci(after_with, "another") {
            Some(after_with)
        } else {
            strip_prefix_word_ci(after_with, "a").or_else(|| strip_prefix_word_ci(after_with, "an"))
        };
        if let Some(p) = co_phrase
            && let Some(co_filter) = parse_trigger_object_phrase(p, ctx, false)
        {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    co_attacker_filter: Some(Box::new(co_filter)),
                    co_attacker_min: Some(1),
                    ..Default::default()
                },
            ));
        }
    }
    if let Some(other_phrase) = strip_prefix_word_ci(rest, "sacrifices ~ or")
        .or_else(|| strip_prefix_word_ci(rest, "sacrifice ~ or"))
        && starts_with_word_ci(other_phrase, "another")
        && let Some(obj) = parse_trigger_object_phrase(other_phrase, ctx, false)
    {
        return Some(make_trigger(
            EventKind::Sacrificed,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                or_self: true,
                ..Default::default()
            },
        ));
    }

    // "taps a/an <obj> for mana"
    if let Some(after_taps) = rest
        .strip_prefix("taps ")
        .or_else(|| rest.strip_prefix("tap "))
        && let Some(after_a) = after_taps
            .strip_prefix("a ")
            .or_else(|| after_taps.strip_prefix("an "))
        && let Some(obj_raw) = after_a.strip_suffix(" for mana")
        && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
    {
        return Some(make_trigger(
            EventKind::TappedForMana,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                player: Some(player),
                ..Default::default()
            },
        ));
    }

    if let Some(mana_str) =
        strip_prefix_word_ci(rest, "taps ~ for").or_else(|| strip_prefix_word_ci(rest, "tap ~ for"))
        && let Some(mc) = parse_braced_mana_color(mana_str)
    {
        return Some(make_trigger(
            EventKind::TappedForMana,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                tapped_mana_color: Some(mc),
                ..Default::default()
            },
        ));
    }
    if rest_low == "taps ~ for mana of the chosen color"
        || rest_low == "tap ~ for mana of the chosen color"
    {
        return Some(make_trigger(
            EventKind::TappedForMana,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                tapped_mana_chosen_color: true,
                ..Default::default()
            },
        ));
    }
    if let Some(after_taps) =
        strip_prefix_word_ci(rest, "taps").or_else(|| strip_prefix_word_ci(rest, "tap"))
        && let Some(after_a) =
            strip_prefix_word_ci(after_taps, "a").or_else(|| strip_prefix_word_ci(after_taps, "an"))
    {
        if let Some((obj_raw, mana_str)) = split_once_ws_word_ci(after_a, "for")
            && let Some(mc) = parse_braced_mana_color(mana_str)
        {
            let obj = if obj_raw.eq_ignore_ascii_case("permanent")
                || obj_raw.eq_ignore_ascii_case("permanents")
            {
                Some(ObjectFilter::default())
            } else {
                parse_trigger_object_phrase(obj_raw, ctx, false)
            };
            if let Some(obj) = obj {
                return Some(make_trigger(
                    EventKind::TappedForMana,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        player: Some(player),
                        tapped_mana_color: Some(mc),
                        ..Default::default()
                    },
                ));
            }
        }
        if let Some(obj_raw) = strip_suffix_ws_ci(after_a, "for mana of the chosen color") {
            let obj = if obj_raw.eq_ignore_ascii_case("permanent")
                || obj_raw.eq_ignore_ascii_case("permanents")
            {
                Some(ObjectFilter::default())
            } else {
                parse_trigger_object_phrase(obj_raw, ctx, false)
            };
            if let Some(obj) = obj {
                return Some(make_trigger(
                    EventKind::TappedForMana,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        player: Some(player),
                        tapped_mana_chosen_color: true,
                        ..Default::default()
                    },
                ));
            }
        }
    }

    if matches!(
        rest_low.as_str(),
        "draws a card except the first one you draws in each of your draw steps"
            | "draws a card except the first one you draw in each of your draw steps"
            | "draw a card except the first one you draws in each of your draw steps"
            | "draw a card except the first one you draw in each of your draw steps"
            | "draws a card except the first one they draws in each of their draw steps"
            | "draws a card except the first one they draw in each of their draw steps"
            | "draw a card except the first one they draws in each of their draw steps"
            | "draw a card except the first one they draw in each of their draw steps"
    ) {
        return Some(make_trigger(
            EventKind::DrawCard,
            EventFilterBuilder {
                player: Some(player),
                except_first_in_draw_step: true,
                ..Default::default()
            },
        ));
    }

    if let Some(after_draw) = rest
        .strip_prefix("draws ")
        .or_else(|| rest.strip_prefix("draw "))
    {
        for (poss, is_your) in [("your ", true), ("their ", false)] {
            if let Some(after_poss) = after_draw.strip_prefix(poss) {
                for (ord_w, ord_v) in [("first ", 1u16), ("second ", 2u16), ("third ", 3u16)] {
                    if let Some(after_ord) = after_poss.strip_prefix(ord_w)
                        && (after_ord == "card each turn" || after_ord == "card in a turn")
                    {
                        if is_your != (rel == PlayerRelation::You) {
                            return None;
                        }
                        return Some(make_trigger(
                            EventKind::DrawCard,
                            EventFilterBuilder {
                                player: Some(player),
                                nth_this_turn: Some(ord_v),
                                ..Default::default()
                            },
                        ));
                    }
                }
            }
        }
    }
    if rest == "draw a card" || rest == "draws a card" {
        return Some(make_trigger(
            EventKind::DrawCard,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest == "investigate" || rest == "investigates" {
        return Some(make_trigger(
            EventKind::Investigate,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest == "surveil" || rest == "surveils" {
        return Some(make_trigger(
            EventKind::Surveil,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest == "amass" || rest == "amasses" {
        return Some(make_trigger(
            EventKind::Amass,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(sub_raw) = rest
        .strip_prefix("amasses ")
        .or_else(|| rest.strip_prefix("amass "))
        && is_bare_capitalized_word(sub_raw)
        && let Some(sub) = singular_subtype(sub_raw)
    {
        let id = ctx.interner.intern_subtype(&sub);
        return Some(make_trigger(
            EventKind::Amass,
            EventFilterBuilder {
                player: Some(player),
                amass_subtype: Some(id),
                ..Default::default()
            },
        ));
    }
    if rest == "manifest dread" || rest == "manifests dread" {
        return Some(make_trigger(
            EventKind::ManifestDread,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "becomes the monarch" || rest_low == "become the monarch" {
        return Some(make_trigger(
            EventKind::BecameMonarch,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(after_turn) = rest
        .strip_prefix("turns ")
        .or_else(|| rest.strip_prefix("turn "))
        && let Some(after_a) = after_turn
            .strip_prefix("a ")
            .or_else(|| after_turn.strip_prefix("an "))
        && let Some(obj_raw) = after_a.strip_suffix(" face up")
        && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
    {
        return Some(make_trigger(
            EventKind::TurnFaceUp,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(target) = rest
        .strip_prefix("foretells ")
        .or_else(|| rest.strip_prefix("foretell "))
        && let Some((obj, one_or_more)) = discarded_card_filter(target, ctx)
        && !one_or_more
    {
        return Some(make_trigger(
            EventKind::Foretell,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(target) = rest
        .strip_prefix("plots ")
        .or_else(|| rest.strip_prefix("plot "))
        && let Some((obj, one_or_more)) = discarded_card_filter(target, ctx)
        && !one_or_more
    {
        return Some(make_trigger(
            EventKind::Plot,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(after_exert) = rest
        .strip_prefix("exerts ")
        .or_else(|| rest.strip_prefix("exert "))
    {
        let phrase = if after_exert.starts_with("another ") {
            Some(after_exert)
        } else {
            after_exert
                .strip_prefix("a ")
                .or_else(|| after_exert.strip_prefix("an "))
        };
        if let Some(p) = phrase
            && let Some(obj) = parse_trigger_object_phrase(p, ctx, false)
        {
            return Some(make_trigger(
                EventKind::Exert,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj),
                    }),
                    player: Some(player),
                    ..Default::default()
                },
            ));
        }
    }

    if let Some(after_with) = strip_prefix_word_ci(rest, "attacks with")
        .or_else(|| strip_prefix_word_ci(rest, "attack with"))
    {
        if let Some((num_w, after_num)) = after_with.split_once(char::is_whitespace)
            && let Some(min_n) = parse_small_u16(num_w)
            && min_n >= 1
            && let Some(obj_raw) = strip_prefix_word_ci(after_num.trim_start(), "or more")
            && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
        {
            if rel == PlayerRelation::You {
                return Some(make_trigger(
                    EventKind::Attacks,
                    EventFilterBuilder {
                        player: Some(player),
                        one_or_more: true,
                        min_attackers: Some(min_n),
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        ..Default::default()
                    },
                ));
            } else if object_filter_problem(&obj).is_none() {
                return Some(make_trigger(
                    EventKind::Attacks,
                    EventFilterBuilder {
                        player: Some(player),
                        one_or_more: true,
                        attacking_with_filter: Some(Box::new(obj)),
                        min_attacking_with_count: Some(min_n),
                        ..Default::default()
                    },
                ));
            }
        }
        if rel == PlayerRelation::You
            && let Some(obj_raw) = strip_prefix_word_ci(after_with, "a")
                .or_else(|| strip_prefix_word_ci(after_with, "an"))
            && !strip_suffix_ws_ci(obj_raw, "alone").is_some()
            && !obj_raw.eq_ignore_ascii_case("alone")
            && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
        {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    player: Some(player),
                    one_or_more: true,
                    min_attackers: Some(1),
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj),
                    }),
                    ..Default::default()
                },
            ));
        }
    }

    if rest == "sacrifice ~" || rest == "sacrifices ~" {
        return Some(make_trigger(
            EventKind::Sacrificed,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::SelfObj),
                ..Default::default()
            },
        ));
    }
    if let Some(after_sac) =
        strip_prefix_word_ci(rest, "sacrifices").or_else(|| strip_prefix_word_ci(rest, "sacrifice"))
    {
        let (one_or_more, phrase) = if let Some(r) = strip_prefix_word_ci(after_sac, "one or more")
        {
            (true, Some(r.to_string()))
        } else if let Some(r) = strip_prefix_word_ci(after_sac, "another") {
            (false, Some(format!("another {r}")))
        } else if let Some(r) =
            strip_prefix_word_ci(after_sac, "a").or_else(|| strip_prefix_word_ci(after_sac, "an"))
        {
            (false, Some(r.to_string()))
        } else {
            (false, None)
        };
        if let Some(p) = phrase
            && let Some(obj) = parse_trigger_object_phrase(&p, ctx, false)
        {
            return Some(make_trigger(
                EventKind::Sacrificed,
                EventFilterBuilder {
                    player: Some(player),
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj),
                    }),
                    one_or_more,
                    ..Default::default()
                },
            ));
        }
    }

    if rest == "scry" || rest == "scries" {
        return Some(make_trigger(
            EventKind::Scry,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest == "commit a crime" || rest == "commits a crime" {
        return Some(make_trigger(
            EventKind::CommittedCrime,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(thresh_w) =
        strip_prefix_word_ci(rest, "expends").or_else(|| strip_prefix_word_ci(rest, "expend"))
        && !thresh_w.contains(char::is_whitespace)
        && let Some(thresh) = parse_small_u16(thresh_w)
        && thresh >= 1
    {
        return Some(make_trigger(
            EventKind::Expended,
            EventFilterBuilder {
                player: Some(player),
                expend_threshold: Some(thresh),
                ..Default::default()
            },
        ));
    }
    if rest == "discover" || rest == "discovers" {
        return Some(make_trigger(
            EventKind::Discover,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest == "forage" || rest == "forages" {
        return Some(make_trigger(
            EventKind::Forage,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rel == PlayerRelation::You && rest_low == "unlock this door" {
        return Some(make_trigger(
            EventKind::DoorUnlocked,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                player: Some(player),
                this_door_face_index: Some(u8::MAX),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "unlocks a door"
            | "unlock a door"
            | "unlocks a door of a room you control"
            | "unlock a door of a room you control"
    ) {
        return Some(make_trigger(
            EventKind::DoorUnlocked,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "fully unlocks a room" || rest_low == "fully unlock a room" {
        return Some(make_trigger(
            EventKind::RoomFullyUnlocked,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "ventures into the dungeon"
            | "venture into the dungeon"
            | "ventures into a dungeon"
            | "venture into a dungeon"
            | "ventures into the undercity"
            | "venture into the undercity"
            | "ventures into dungeon"
            | "venture into dungeon"
            | "ventures into undercity"
            | "venture into undercity"
    ) {
        return Some(make_trigger(
            EventKind::DungeonRoomEntered,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "completes a dungeon" || rest_low == "complete a dungeon" {
        return Some(make_trigger(
            EventKind::DungeonCompleted,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "takes the initiative" || rest_low == "take the initiative" {
        return Some(make_trigger(
            EventKind::TookInitiative,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }

    if let Some(after_mill) =
        strip_prefix_word_ci(rest, "mills").or_else(|| strip_prefix_word_ci(rest, "mill"))
    {
        let (one_or_more, after_qty) =
            if let Some(r) = strip_prefix_word_ci(after_mill, "one or more") {
                (true, Some(r))
            } else if let Some(r) = strip_prefix_word_ci(after_mill, "a")
                .or_else(|| strip_prefix_word_ci(after_mill, "an"))
            {
                (false, Some(r))
            } else {
                (false, None)
            };
        if let Some(aq) = after_qty {
            let qual = if aq.eq_ignore_ascii_case("card") || aq.eq_ignore_ascii_case("cards") {
                Some(None)
            } else {
                strip_suffix_ws_ci(aq, "cards")
                    .or_else(|| strip_suffix_ws_ci(aq, "card"))
                    .map(Some)
            };
            if let Some(q_opt) = qual
                && let Some(obj_filter) = parse_graveyard_leave_card_filter(q_opt, ctx)
            {
                return Some(make_trigger(
                    EventKind::ZoneChange,
                    EventFilterBuilder {
                        player: Some(player),
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj_filter),
                        }),
                        from: ZoneMask::LIBRARY,
                        to: Some(Zone::Graveyard),
                        one_or_more,
                        ..Default::default()
                    },
                ));
            }
        }
    }

    if matches!(
        rest_low.as_str(),
        "rolls one or more dice" | "roll one or more dice" | "rolls a die" | "roll a die"
    ) {
        return Some(make_trigger(
            EventKind::DieRolled,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(after_rolls) =
        strip_prefix_word_ci(&rest_low, "rolls").or_else(|| strip_prefix_word_ci(&rest_low, "roll"))
    {
        let after_a = strip_prefix_word_ci(after_rolls, "a").unwrap_or(after_rolls);
        let without_die = strip_suffix_ws_ci(after_a, "on a die").unwrap_or(after_a);
        if let Some(num_str) = strip_suffix_ws_ci(without_die, "or higher")
            && !num_str.is_empty()
            && num_str.bytes().all(|b| b.is_ascii_digit())
            && let Ok(min_roll) = num_str.parse::<u16>()
        {
            return Some(make_trigger(
                EventKind::DieRolled,
                EventFilterBuilder {
                    player: Some(player),
                    die_roll_min: Some(min_roll),
                    ..Default::default()
                },
            ));
        }
        let normalized_vals = replace_ci_sep(without_die, ", or ", ", ");
        let normalized_vals = replace_ci_sep(&normalized_vals, " or ", ", ");
        let mut vals: SmallVec<[u16; 4]> = SmallVec::new();
        let mut all_nums = true;
        for part in normalized_vals.split(',').map(str::trim) {
            if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(v) = part.parse::<u16>() {
                    vals.push(v);
                } else {
                    all_nums = false;
                    break;
                }
            } else {
                all_nums = false;
                break;
            }
        }
        if all_nums && !vals.is_empty() {
            return Some(make_trigger(
                EventKind::DieRolled,
                EventFilterBuilder {
                    player: Some(player),
                    die_roll_values: vals,
                    ..Default::default()
                },
            ));
        }
    }

    if rest_low == "wins a coin flip" || rest_low == "win a coin flip" {
        return Some(make_trigger(
            EventKind::CoinFlipped,
            EventFilterBuilder {
                player: Some(player),
                won_coin_flip: true,
                ..Default::default()
            },
        ));
    }
    if rest_low == "loses a coin flip" || rest_low == "lose a coin flip" {
        return Some(make_trigger(
            EventKind::CoinFlipped,
            EventFilterBuilder {
                player: Some(player),
                won_coin_flip: false,
                ..Default::default()
            },
        ));
    }
    if rest_low == "flips a coin" || rest_low == "flip a coin" {
        return Some(make_trigger(
            EventKind::CoinFlipped,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }

    if let Some(after_taps) =
        strip_prefix_word_ci(rest, "taps").or_else(|| strip_prefix_word_ci(rest, "tap"))
        && let Some(obj_raw) = strip_prefix_word_ci(after_taps, "a untapped")
            .or_else(|| strip_prefix_word_ci(after_taps, "an untapped"))
        && let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
    {
        let tapped_by = match rel {
            PlayerRelation::You => RelativeControl::You,
            PlayerRelation::Opponent => RelativeControl::Opponent,
            _ => RelativeControl::Any,
        };
        return Some(make_trigger(
            EventKind::BecomesTapped,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(obj),
                }),
                tapped_by_player: Some(tapped_by),
                ..Default::default()
            },
        ));
    }

    if matches!(
        rest_low.as_str(),
        "searches your library"
            | "search your library"
            | "searches their library"
            | "search their library"
            | "searches a library"
            | "search a library"
    ) {
        return Some(make_trigger(
            EventKind::LibrarySearched,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "loses the game" {
        return Some(make_trigger(
            EventKind::PlayerLostGame,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "gets {e}" | "get {e}" | "gets one or more {e}" | "get one or more {e}"
    ) {
        return Some(make_trigger(
            EventKind::EnergyGained,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "proliferate" || rest_low == "proliferates" {
        return Some(make_trigger(
            EventKind::Proliferated,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "plays a card" || rest_low == "play a card" {
        let lp = make_trigger(
            EventKind::LandPlayed,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter::default()),
                }),
                ..Default::default()
            },
        );
        let cast = make_trigger(
            EventKind::Cast,
            EventFilterBuilder {
                player: Some(player),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter::default()),
                }),
                ..Default::default()
            },
        );
        return Some(merge_compound_trigger_defs(lp, cast));
    }
    if rest_low == "clash" || rest_low == "clashes" {
        return Some(make_trigger(
            EventKind::Clash,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if rest_low == "wins a clash" || rest_low == "win a clash" {
        return Some(make_trigger(
            EventKind::Clash,
            EventFilterBuilder {
                player: Some(player),
                won: Some(true),
                ..Default::default()
            },
        ));
    }
    if rest_low == "loses a clash" || rest_low == "lose a clash" {
        return Some(make_trigger(
            EventKind::Clash,
            EventFilterBuilder {
                player: Some(player),
                won: Some(false),
                ..Default::default()
            },
        ));
    }
    if matches!(
        rest_low.as_str(),
        "shuffle"
            | "shuffles"
            | "shuffle your library"
            | "shuffles your library"
            | "shuffle their library"
            | "shuffles their library"
            | "shuffle a library"
            | "shuffles a library"
    ) {
        return Some(make_trigger(
            EventKind::LibraryShuffled,
            EventFilterBuilder {
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(spell_raw) =
        strip_prefix_word_ci(rest, "copies").or_else(|| strip_prefix_word_ci(rest, "copy"))
        && let Some(sf) = spell_filter(spell_raw, ctx)
    {
        return Some(make_trigger(
            EventKind::SpellCopied,
            EventFilterBuilder {
                object: Some(sf),
                player: Some(player),
                ..Default::default()
            },
        ));
    }
    if let Some(spell_raw) =
        strip_prefix_word_ci(rest, "counters").or_else(|| strip_prefix_word_ci(rest, "counter"))
        && let Some(sf) = spell_filter(spell_raw, ctx)
    {
        return Some(make_trigger(
            EventKind::SpellCountered,
            EventFilterBuilder {
                object: Some(sf),
                player: Some(player),
                ..Default::default()
            },
        ));
    }

    if let Some(after_act) =
        strip_prefix_word_ci(rest, "activates").or_else(|| strip_prefix_word_ci(rest, "activate"))
        && let Some(after_a) =
            strip_prefix_word_ci(after_act, "a").or_else(|| strip_prefix_word_ci(after_act, "an"))
    {
        let (is_loyalty_ability, after_loy) =
            if let Some(r) = strip_prefix_word_ci(after_a, "loyalty") {
                (true, r)
            } else {
                (false, after_a)
            };
        if let Some(after_ab) = strip_prefix_word_ci(after_loy, "ability")
            .or_else(|| after_loy.eq_ignore_ascii_case("ability").then_some(""))
        {
            let (before_mana, is_non_mana_ability) =
                if let Some(l) = strip_suffix_ws_ci(after_ab, "that isn't a mana ability") {
                    (l, true)
                } else if after_ab.eq_ignore_ascii_case("that isn't a mana ability") {
                    ("", true)
                } else {
                    (after_ab, false)
                };
            let ability_source = if before_mana.is_empty() {
                None
            } else {
                let src_raw = strip_prefix_word_ci(before_mana, "of")?;
                if src_raw == "~" {
                    Some(TriggerSubject::SelfObj)
                } else if let Some(noun) = strip_prefix_word_ci(src_raw, "enchanted")
                    .or_else(|| strip_prefix_word_ci(src_raw, "equipped"))
                    && matches!(
                        noun.to_ascii_lowercase().as_str(),
                        "creature"
                            | "permanent"
                            | "artifact"
                            | "land"
                            | "enchantment"
                            | "planeswalker"
                    )
                {
                    Some(TriggerSubject::AttachedTo)
                } else {
                    let sub = object_subject(src_raw, ctx)?;
                    if !matches!(sub.object, TriggerSubject::Filter { .. }) {
                        return None;
                    }
                    Some(sub.object)
                }
            };
            return Some(make_trigger(
                EventKind::AbilityActivated,
                EventFilterBuilder {
                    player: Some(player),
                    is_loyalty_ability,
                    is_non_mana_ability,
                    ability_source,
                    ..Default::default()
                },
            ));
        }
    }

    None
}

fn parse_any_of_type_list(raw: &str, zone: ZoneMask) -> Option<ObjectFilter> {
    let mut cleaned = raw.trim();
    if let Some(r) =
        strip_prefix_word_ci(cleaned, "a").or_else(|| strip_prefix_word_ci(cleaned, "an"))
    {
        cleaned = r;
    }
    let normalized = replace_ci_sep(cleaned, ", or ", ", ");
    let normalized = replace_ci_sep(&normalized, " or ", ", ");
    let parts: Vec<&str> = normalized
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let mut mapped = Vec::with_capacity(parts.len());
    for p in parts {
        let ty = match p.to_ascii_lowercase().as_str() {
            "artifact" | "artifacts" => CardTypeMask::ARTIFACT,
            "creature" | "creatures" => CardTypeMask::CREATURE,
            "land" | "lands" => CardTypeMask::LAND,
            "enchantment" | "enchantments" => CardTypeMask::ENCHANTMENT,
            "planeswalker" | "planeswalkers" => CardTypeMask::PLANESWALKER,
            "battle" | "battles" => CardTypeMask::BATTLE,
            _ => return None,
        };
        mapped.push(ObjectFilter {
            types_any: ty,
            zones: zone,
            ..Default::default()
        });
    }
    if mapped.len() == 1 {
        Some(mapped.remove(0))
    } else {
        Some(ObjectFilter {
            zones: zone,
            extra: vec![ObjectPredicate::AnyOf { filters: mapped }],
            ..Default::default()
        })
    }
}

fn parse_trigger_head_wave9(head: &str, ctx: &mut ParseCtx<'_>) -> Option<TriggerDef> {
    let t = normalize_apostrophes(head.trim());
    let t = t.as_str();

    if let Some(rest) = strip_prefix_word_ci(t, "at the beginning of") {
        if rest.eq_ignore_ascii_case("enchanted opponent's end step") {
            let mut trig = make_trigger(
                EventKind::PhaseBegin,
                EventFilterBuilder {
                    phase: Some(GamePhase::EndStep),
                    enchanted_opponent_end_step: true,
                    ..Default::default()
                },
            );
            trig.scope = Some(TriggerScope::OpponentTurn);
            return Some(trig);
        }
        return None;
    }

    let body = strip_prefix_word_ci(t, "when").or_else(|| strip_prefix_word_ci(t, "whenever"))?;

    // 1. Ainok Strike Leader
    if body.eq_ignore_ascii_case("you attack with ~ and/or your commander") {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                object: Some(TriggerSubject::SelfObj),
                or_commander: true,
                ..Default::default()
            },
        ));
    }

    // 2. Tenuous Truce
    if body.eq_ignore_ascii_case(
        "you attack enchanted opponent or a planeswalker they control or when they attack you or a planeswalker you control",
    ) {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                attack_enchanted_or_attacked_by_enchanted: true,
                ..Default::default()
            },
        ));
    }

    // 3. Avalanche of Sector 7, Harsh Mentor
    if let Some((pl_rel, after_pl)) = strip_prefix_word_ci(body, "you")
        .map(|r| (PlayerRelation::You, r))
        .or_else(|| strip_prefix_word_ci(body, "a player").map(|r| (PlayerRelation::Any, r)))
        .or_else(|| {
            strip_prefix_word_ci(body, "an opponent").map(|r| (PlayerRelation::Opponent, r))
        })
        && let Some(src_raw) = strip_prefix_word_ci(after_pl, "activates an ability of")
            .or_else(|| strip_prefix_word_ci(after_pl, "activate an ability of"))
    {
        let (mut src_desc, is_non_mana_ability) = if let Some(l) =
            strip_suffix_ws_ci(src_raw, ", if it isn't a mana ability")
                .or_else(|| strip_suffix_ci(src_raw, ", if it isn't a mana ability"))
        {
            (l.trim_end_matches(',').trim(), true)
        } else {
            (src_raw.trim(), false)
        };
        let mut controlled_by_event_player = false;
        if let Some(l) = strip_suffix_ws_ci(src_desc, "they control") {
            controlled_by_event_player = true;
            src_desc = l;
        }
        let mut on_battlefield = false;
        if let Some(l) = strip_suffix_ws_ci(src_desc, "on the battlefield") {
            on_battlefield = true;
            src_desc = l;
        }
        let zone = if on_battlefield {
            ZoneMask::BATTLEFIELD
        } else {
            ZoneMask::empty()
        };
        let cp = ctx.interner.checkpoint();
        let ability_source = if src_desc == "~" {
            Some(TriggerSubject::SelfObj)
        } else if let Some(any_of) = parse_any_of_type_list(src_desc, zone) {
            Some(TriggerSubject::Filter {
                filter: Box::new(any_of),
            })
        } else if let Some(mut parsed) = parse_trigger_object_phrase(src_desc, ctx, false) {
            if on_battlefield {
                parsed.zones = ZoneMask::BATTLEFIELD;
            }
            Some(TriggerSubject::Filter {
                filter: Box::new(parsed),
            })
        } else {
            None
        };
        if let Some(src) = ability_source {
            let bad = match &src {
                TriggerSubject::Filter { filter } => object_filter_problem(filter).is_some(),
                _ => false,
            };
            if !bad {
                return Some(make_trigger(
                    EventKind::AbilityActivated,
                    EventFilterBuilder {
                        player: Some(PlayerFilter { relation: pl_rel }),
                        ability_source: Some(src),
                        ability_source_controlled_by_event_player: controlled_by_event_player,
                        is_non_mana_ability,
                        ..Default::default()
                    },
                ));
            }
        }
        ctx.interner.rollback(cp);
    }

    // 4. Coveted Jewel
    if body.eq_ignore_ascii_case(
        "one or more creatures an opponent controls attack you and aren't blocked",
    ) {
        return Some(make_trigger(
            EventKind::AttackedAndUnblocked,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        controller: Some(RelativeControl::Opponent),
                        ..Default::default()
                    }),
                }),
                attacked_target: Some(AttackedTargetFilter::You),
                one_or_more: true,
                attacking_unblocked: true,
                ..Default::default()
            },
        ));
    }

    // 5. Crawling Sensation
    if let Some(after_oom) = strip_prefix_word_ci(body, "one or more")
        && let Some((filter_part, whose_your)) = [
            (
                "cards are put into your graveyard from anywhere for the first time each turn",
                true,
            ),
            (
                "cards is put into your graveyard from anywhere for the first time each turn",
                true,
            ),
            (
                "card is put into your graveyard from anywhere for the first time each turn",
                true,
            ),
            (
                "card are put into your graveyard from anywhere for the first time each turn",
                true,
            ),
            (
                "cards are put into a graveyard from anywhere for the first time each turn",
                false,
            ),
            (
                "cards is put into a graveyard from anywhere for the first time each turn",
                false,
            ),
            (
                "card is put into a graveyard from anywhere for the first time each turn",
                false,
            ),
            (
                "card are put into a graveyard from anywhere for the first time each turn",
                false,
            ),
        ]
        .iter()
        .find_map(|(sfx, y)| strip_suffix_ws_ci(after_oom, sfx).map(|l| (l, *y)))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(filter_part, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            f.zones = ZoneMask::empty();
            if !f
                .flags
                .intersects(FilterFlags::TOKEN | FilterFlags::NONTOKEN)
            {
                f.flags |= FilterFlags::NONTOKEN;
            }
            if whose_your {
                f.owner = Some(RelativeControl::You);
            }
            return Some(make_trigger(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(f),
                    }),
                    from_anywhere: true,
                    to: Some(Zone::Graveyard),
                    one_or_more: true,
                    first_time_each_turn: true,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // Mangara, the Diplomat: "<who> attacks? with creatures"
    if let Some(rel) = match body.to_ascii_lowercase().as_str() {
        "an opponent attacks with creatures" | "an opponent attack with creatures" => {
            Some(PlayerRelation::Opponent)
        }
        "you attack with creatures" | "you attacks with creatures" => Some(PlayerRelation::You),
        "a player attacks with creatures" | "a player attack with creatures" => {
            Some(PlayerRelation::Any)
        }
        _ => None,
    } {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(PlayerFilter { relation: rel }),
                one_or_more: true,
                ..Default::default()
            },
        ));
    }

    // 6. Emet-Selch
    if body.eq_ignore_ascii_case("one or more opponents lose life") {
        return Some(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                one_or_more: true,
                ..Default::default()
            },
        ));
    }

    // 7. Valgavoth, Harrower of Souls
    if body.eq_ignore_ascii_case(
        "an opponent loses life for the first time during each of their turns",
    ) {
        return Some(make_trigger(
            EventKind::LoseLife,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                first_time_each_turn: true,
                during_turn: Some(DuringTurnKind::EventPlayer),
                ..Default::default()
            },
        ));
    }

    // 8. Firebender Ascension
    if let Some(before_trig) = strip_suffix_ws_ci(body, "to trigger")
        && let Some((subj_raw, noun_part)) =
            split_once_ws_word_ci(before_trig, "attacking causes a triggered ability of that")
        && !noun_part.is_empty()
        && !noun_part.contains(char::is_whitespace)
    {
        let cp = ctx.interner.checkpoint();
        if let Some(subj) = object_subject(subj_raw, ctx)
            && matches!(subj.object, TriggerSubject::Filter { .. })
            && let TriggerSubject::Filter { ref filter } = subj.object
            && object_filter_problem(filter).is_none()
        {
            return Some(make_trigger(
                EventKind::AttackTriggered,
                EventFilterBuilder {
                    object: Some(subj.object),
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 9. Jiang Yanggu, Alone
    if let Some(subj_raw) = strip_suffix_ws_ci(body, "attacks a player alone")
        .or_else(|| strip_suffix_ws_ci(body, "attack a player alone"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(subj) = object_subject(subj_raw, ctx) {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    object: Some(subj.object),
                    alone: true,
                    attacked_target: Some(AttackedTargetFilter::AnyPlayer),
                    or_self: subj.or_self,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 10. Shredder, Shadow Master
    if let Some(subj_raw) = strip_suffix_ws_ci(body, "attacks a player")
        .or_else(|| strip_suffix_ws_ci(body, "attack a player"))
    {
        let cp = ctx.interner.checkpoint();
        if subj_raw == "~" {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    attacked_target: Some(AttackedTargetFilter::AnyPlayer),
                    per_attacked_player: true,
                    ..Default::default()
                },
            ));
        }
        if let Some(subj) = object_subject(subj_raw, ctx) {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    object: Some(subj.object),
                    attacked_target: Some(AttackedTargetFilter::AnyPlayer),
                    per_attacked_player: true,
                    or_self: subj.or_self,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 11. Jolly Gerbils
    if body.eq_ignore_ascii_case("you give a gift") {
        return Some(make_trigger(
            EventKind::GiftGiven,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                ..Default::default()
            },
        ));
    }

    // 12. Laelia, the Blade Reforged
    if let Some(after_ex) =
        strip_prefix_word_ci(body, "one or more cards are put into exile from your")
            .or_else(|| strip_prefix_word_ci(body, "one or more cards is put into exile from your"))
            .or_else(|| strip_prefix_word_ci(body, "one or more card is put into exile from your"))
            .or_else(|| strip_prefix_word_ci(body, "one or more card are put into exile from your"))
        && let Some((z1_raw, z2_raw)) = split_once_ws_word_ci(after_ex, "and/or your")
        && let (Some(z1), Some(z2)) = (parse_zone_name(z1_raw), parse_zone_name(z2_raw))
        && matches!(z1, Zone::Library | Zone::Graveyard)
        && matches!(z2, Zone::Library | Zone::Graveyard)
    {
        return Some(make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        owner: Some(RelativeControl::You),
                        ..Default::default()
                    }),
                }),
                to: Some(Zone::Exile),
                from: ZoneMask::from_zone(z1) | ZoneMask::from_zone(z2),
                one_or_more: true,
                ..Default::default()
            },
        ));
    }

    // 13. Mindblade Render
    if body.eq_ignore_ascii_case("your opponents are dealt combat damage") {
        return Some(make_trigger(
            EventKind::CombatDamageBatchPlayer,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Opponent,
                }),
                combat_damage_batch_player: Some(RelativeControl::Opponent),
                one_or_more: true,
                ..Default::default()
            },
        ));
    }

    // 14. Pain Magnification
    if let Some((who_rel, after_who)) = strip_prefix_word_ci(body, "an opponent")
        .map(|r| (RelativeControl::Opponent, r))
        .or_else(|| strip_prefix_word_ci(body, "a player").map(|r| (RelativeControl::Any, r)))
        .or_else(|| strip_prefix_word_ci(body, "you").map(|r| (RelativeControl::You, r)))
        && let Some(after_dealt) = strip_prefix_word_ci(after_who, "is dealt")
            .or_else(|| strip_prefix_word_ci(after_who, "are dealt"))
        && let Some(num_w) = strip_suffix_ws_ci(after_dealt, "or more damage by a single source")
        && let Some(min_dmg) = parse_small_i32(num_w)
        && min_dmg >= 1
    {
        return Some(make_trigger(
            EventKind::DealtDamage,
            EventFilterBuilder {
                damaged_player: Some(who_rel),
                single_source_damage_min: Some(min_dmg),
                ..Default::default()
            },
        ));
    }

    // 15. Professor Hojo
    if let Some(subj_raw) = strip_suffix_ws_ci(body, "becomes the target of an activated ability")
        .or_else(|| strip_suffix_ws_ci(body, "become the target of an activated ability"))
    {
        let cp = ctx.interner.checkpoint();
        let has_oom = starts_with_word_ci(subj_raw, "one or more");
        if let Some(subj) = object_subject(subj_raw, ctx) {
            return Some(make_trigger(
                EventKind::BecomesTarget,
                EventFilterBuilder {
                    object: Some(subj.object),
                    targeted_by: Some(TargetedByKind::Ability),
                    targeted_by_activated_ability: true,
                    one_or_more: has_oom || subj.one_or_more,
                    or_self: subj.or_self,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 16. Rosie Cotton of South Lane
    if body.eq_ignore_ascii_case("you create a token") {
        return Some(make_trigger(
            EventKind::TokenCreated,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                ..Default::default()
            },
        ));
    }

    // 17. Szarel, Genesis Shepherd
    if let Some((pl_rel, after_pl)) = strip_prefix_word_ci(body, "you")
        .map(|r| (PlayerRelation::You, r))
        .or_else(|| strip_prefix_word_ci(body, "a player").map(|r| (PlayerRelation::Any, r)))
        .or_else(|| {
            strip_prefix_word_ci(body, "an opponent").map(|r| (PlayerRelation::Opponent, r))
        })
        && let Some(after_sac) = strip_prefix_word_ci(after_pl, "sacrifices")
            .or_else(|| strip_prefix_word_ci(after_pl, "sacrifice"))
        && let Some((subj_raw, during_turn)) = [
            ("during your turn", DuringTurnKind::Your),
            ("during their turn", DuringTurnKind::EventPlayer),
            ("during an opponent's turn", DuringTurnKind::Opponent),
        ]
        .iter()
        .find_map(|(sfx, dt)| strip_suffix_ws_ci(after_sac, sfx).map(|l| (l, *dt)))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(subj) = object_subject(subj_raw, ctx) {
            return Some(make_trigger(
                EventKind::Sacrificed,
                EventFilterBuilder {
                    player: Some(PlayerFilter { relation: pl_rel }),
                    object: Some(subj.object),
                    during_turn: Some(during_turn),
                    or_self: subj.or_self,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 18. Tameshi, Reality Architect
    if let Some(subj_raw) = [
        "is returned to their owners' hands",
        "is returned to their owners' hand",
        "are returned to their owners' hands",
        "are returned to their owners' hand",
        "is returned to its owner's hand",
        "are returned to its owner's hand",
        "is returned to hand",
        "are returned to hand",
    ]
    .iter()
    .find_map(|sfx| strip_suffix_ws_ci(body, sfx))
    {
        let cp = ctx.interner.checkpoint();
        let has_oom = starts_with_word_ci(subj_raw, "one or more");
        if let Some(subj) = object_subject(subj_raw, ctx) {
            return Some(make_trigger(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    object: Some(subj.object),
                    from: ZoneMask::BATTLEFIELD,
                    to: Some(Zone::Hand),
                    one_or_more: has_oom || subj.one_or_more,
                    or_self: subj.or_self,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 19. Ugin's Mastery
    if let Some(mid) = strip_prefix_word_ci(body, "you attack with creatures with total power")
        .and_then(|r| strip_suffix_ws_ci(r, "or greater"))
        && let Some(min_pwr) = parse_small_i32(mid)
        && min_pwr >= 1
    {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                one_or_more: true,
                attacking_total_power_min: Some(min_pwr),
                ..Default::default()
            },
        ));
    }

    // 20. Delayed combat damage standalone
    if let Some(after_subj) = strip_prefix_word_ci(body, "that creature deals combat damage")
        .or_else(|| strip_prefix_word_ci(body, "either of those creatures deals combat damage"))
    {
        let (rem, has_this_turn) = if let Some(r) = strip_suffix_ws_ci(after_subj, "this turn") {
            (r, true)
        } else if after_subj.eq_ignore_ascii_case("this turn") {
            ("", true)
        } else {
            (after_subj, false)
        };
        let dmg_to = if rem.eq_ignore_ascii_case("to a player or planeswalker") {
            Some(DamageRecipient::PlayerOrPlaneswalker)
        } else if rem.eq_ignore_ascii_case("to a player") {
            Some(DamageRecipient::Player)
        } else if rem.is_empty() {
            Some(DamageRecipient::Any)
        } else {
            None
        };
        if let Some(damage_to) = dmg_to {
            let mut trig = make_trigger(
                EventKind::DealsDamage,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            ..Default::default()
                        }),
                    }),
                    combat: Some(true),
                    damage_to: Some(damage_to),
                    watched_bound_object: true,
                    ..Default::default()
                },
            );
            if has_this_turn {
                trig.spell_delayed = Some(SpellDelayedDuration::ThisTurn);
            }
            return Some(trig);
        }
    }

    // 21. Syr Konrad, the Grim
    if body.eq_ignore_ascii_case(
        "another creature dies, or a creature card is put into a graveyard from anywhere other than the battlefield, or a creature card leaves your graveyard",
    ) {
        let mut main = make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::OTHER,
                        ..Default::default()
                    }),
                }),
                from: ZoneMask::BATTLEFIELD,
                to: Some(Zone::Graveyard),
                ..Default::default()
            },
        );
        main.or.push(make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::NONTOKEN,
                        ..Default::default()
                    }),
                }),
                from_anywhere: true,
                to: Some(Zone::Graveyard),
                not_from_zone: Some(Zone::Battlefield),
                ..Default::default()
            },
        ));
        main.or.push(make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        flags: FilterFlags::NONTOKEN,
                        owner: Some(RelativeControl::You),
                        ..Default::default()
                    }),
                }),
                from: ZoneMask::GRAVEYARD,
                ..Default::default()
            },
        ));
        return Some(main);
    }

    None
}

// -----------------------------------------------------------------------------
// Main trigger_head dispatch
// -----------------------------------------------------------------------------

fn normalize_compound_right_pronoun(left: &TriggerDef, right_raw: &str) -> String {
    if !matches!(left.filter.object, Some(TriggerSubject::SelfObj)) {
        return right_raw.to_string();
    }
    if let Some(kw) = strip_suffix_ws_ci(right_raw, "you sacrifice it")
        && (kw.eq_ignore_ascii_case("when") || kw.eq_ignore_ascii_case("whenever"))
    {
        return format!("{kw} you sacrifice ~");
    }
    for kw in ["whenever", "when"] {
        if let Some(rest) = strip_prefix_word_ci(right_raw, kw)
            && let Some(after_it) = strip_prefix_word_ci(rest, "it")
        {
            return format!("{kw} ~ {after_it}");
        }
    }
    right_raw.to_string()
}

fn is_at_phase_start(s: &str) -> bool {
    starts_with_word_ci(s, "at the beginning of")
        || s.eq_ignore_ascii_case("at end of combat")
        || s.eq_ignore_ascii_case("at the end of combat")
}

fn is_when_start(s: &str) -> bool {
    starts_with_word_ci(s, "when") || starts_with_word_ci(s, "whenever")
}

fn split_compound_head_pairs(head: &str) -> Vec<(&str, &str)> {
    let mut pairs = Vec::new();
    for sep in ["and", "or"] {
        for (start, end) in find_ws_word_ci_all(head, sep) {
            let left = head[..start].trim();
            let right = head[end..].trim();
            if !left.is_empty() && !right.is_empty() {
                pairs.push((left, right));
            }
        }
    }
    pairs
}

fn trigger_head(head_raw: &str, ctx: &mut ParseCtx<'_>, depth: u8) -> Option<TriggerDef> {
    let head = head_raw.trim();
    if head.is_empty() {
        return None;
    }

    if depth < 2 && head.len() <= 512 {
        let mut pairs = split_compound_head_pairs(head);
        pairs.truncate(8);
        // Mixed "<when/whenever ...> (and|or) <at ...>"
        for &(left_raw, right_raw) in &pairs {
            if is_when_start(left_raw) && is_at_phase_start(right_raw) {
                let cp = ctx.interner.checkpoint();
                if let (Some(left), Some(right)) = (
                    trigger_head(left_raw, ctx, depth + 1),
                    trigger_head(right_raw, ctx, depth + 1),
                ) {
                    return Some(merge_compound_trigger_defs(left, right));
                }
                ctx.interner.rollback(cp);
            }
        }
        // Mixed "<at ...> (and|or) <when/whenever ...>"
        for &(left_raw, right_raw) in &pairs {
            if is_at_phase_start(left_raw) && is_when_start(right_raw) {
                let cp = ctx.interner.checkpoint();
                if let (Some(left), Some(right)) = (
                    trigger_head(left_raw, ctx, depth + 1),
                    trigger_head(right_raw, ctx, depth + 1),
                ) {
                    return Some(merge_compound_trigger_defs(left, right));
                }
                ctx.interner.rollback(cp);
            }
        }
        // Dual "<at ...> (and|or) <at ...>"
        for &(left_raw, right_raw) in &pairs {
            if is_at_phase_start(left_raw) && is_at_phase_start(right_raw) {
                let cp = ctx.interner.checkpoint();
                if let (Some(left), Some(right)) = (
                    trigger_head(left_raw, ctx, depth + 1),
                    trigger_head(right_raw, ctx, depth + 1),
                ) {
                    return Some(merge_compound_trigger_defs(left, right));
                }
                ctx.interner.rollback(cp);
            }
        }
        // Wave 9 heads checked BEFORE generic "<when/whenever> (and|or) <when/whenever>"
        {
            let cp = ctx.interner.checkpoint();
            if let Some(w9) = parse_trigger_head_wave9(head, ctx) {
                return Some(w9);
            }
            ctx.interner.rollback(cp);
        }
        // Dual "<when/whenever ...> (and|or) <when/whenever ...>"
        for &(left_raw, right_raw) in &pairs {
            if is_when_start(left_raw) && is_when_start(right_raw) {
                let cp = ctx.interner.checkpoint();
                if let Some(left) = trigger_head(left_raw, ctx, depth + 1) {
                    let norm_right = normalize_compound_right_pronoun(&left, right_raw);
                    if let Some(right) = trigger_head(&norm_right, ctx, depth + 1) {
                        return Some(merge_compound_trigger_defs(left, right));
                    }
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    if let Some(rest) = strip_prefix_word_ci(head, "at the beginning of") {
        return phase_begin(rest).or_else(|| parse_trigger_head_wave9(head, ctx));
    }
    if head.eq_ignore_ascii_case("at end of combat")
        || head.eq_ignore_ascii_case("at the end of combat")
    {
        let mut t = make_trigger(
            EventKind::PhaseBegin,
            EventFilterBuilder {
                phase: Some(GamePhase::EndCombat),
                ..Default::default()
            },
        );
        t.scope = Some(TriggerScope::EachTurn);
        return Some(t);
    }

    let (kw_word, body_raw) = if let Some(b) = strip_prefix_word_ci(head, "whenever") {
        ("whenever", b)
    } else {
        ("when", strip_prefix_word_ci(head, "when")?)
    };

    let body_norm = normalize_apostrophes(body_raw.trim());
    let body = body_norm.as_str();
    let body_low = body.to_ascii_lowercase();

    if kw_word == "when" {
        let cp = ctx.interner.checkpoint();
        if let Some(st) = parse_state_trigger_body(body, ctx) {
            return Some(st);
        }
        ctx.interner.rollback(cp);
    }

    if body_low == "day becomes night or night becomes day" {
        return Some(make_trigger(
            EventKind::DayNightChanged,
            EventFilterBuilder {
                day_night_direction: Some(DayNightDirection::DayNightFlip),
                ..Default::default()
            },
        ));
    }
    if body_low == "it becomes day" {
        return Some(make_trigger(
            EventKind::DayNightChanged,
            EventFilterBuilder {
                day_night_direction: Some(DayNightDirection::ToDay),
                ..Default::default()
            },
        ));
    }
    if body_low == "it becomes night" {
        return Some(make_trigger(
            EventKind::DayNightChanged,
            EventFilterBuilder {
                day_night_direction: Some(DayNightDirection::ToNight),
                ..Default::default()
            },
        ));
    }
    if body_low == "it becomes day or night" {
        return Some(make_trigger(
            EventKind::DayNightChanged,
            EventFilterBuilder {
                day_night_direction: Some(DayNightDirection::Any),
                ..Default::default()
            },
        ));
    }
    if body_low == "the creature ~ haunts dies" {
        let mut t = make_trigger(
            EventKind::HauntedCreatureDies,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                ..Default::default()
            },
        );
        t.from_zones = Some(ZoneMask::EXILE);
        return Some(t);
    }

    if let Some(tgt_raw) = strip_prefix_word_ci(
        body,
        "a spell or ability an opponent controls causes you to discard",
    ) {
        if tgt_raw == "~" {
            let mut t = make_trigger(
                EventKind::Discard,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    caused_by_opponent: true,
                    ..Default::default()
                },
            );
            t.from_zones = Some(ZoneMask::ALL);
            return Some(t);
        }
        let cp = ctx.interner.checkpoint();
        if let Some((obj, one_or_more)) = discarded_card_filter(tgt_raw, ctx) {
            return Some(make_trigger(
                EventKind::Discard,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(obj),
                    }),
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    caused_by_opponent: true,
                    one_or_more,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some((damaged_player, after_dealt)) = strip_prefix_word_ci(&body_low, "you're dealt")
        .or_else(|| strip_prefix_word_ci(&body_low, "you are dealt"))
        .map(|r| (RelativeControl::You, r))
        .or_else(|| {
            strip_prefix_word_ci(&body_low, "an opponent is dealt")
                .or_else(|| strip_prefix_word_ci(&body_low, "your opponents are dealt"))
                .map(|r| (RelativeControl::Opponent, r))
        })
    {
        let (combat, noncombat_only) = match after_dealt {
            "damage" => (None, false),
            "combat damage" => (Some(true), false),
            "noncombat damage" => (None, true),
            _ => (None, false),
        };
        if matches!(after_dealt, "damage" | "combat damage" | "noncombat damage") {
            return Some(make_trigger(
                EventKind::DealtDamage,
                EventFilterBuilder {
                    damaged_player: Some(damaged_player),
                    combat,
                    noncombat_only,
                    ..Default::default()
                },
            ));
        }
    }

    if let Some(after_tgt) = strip_prefix_word_ci(&body_low, "you become the target of") {
        for (kind_str, kind_val) in [
            ("a spell or ability", TargetedByKind::SpellOrAbility),
            ("a spell", TargetedByKind::Spell),
            ("an ability", TargetedByKind::Ability),
        ] {
            if let Some(ctrl_raw) = after_tgt.strip_prefix(kind_str) {
                let ctrl = ctrl_raw.trim();
                let player = match ctrl {
                    "" => None,
                    "you control" => Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    "an opponent controls" | "your opponents control" | "you don't control" => {
                        Some(PlayerFilter {
                            relation: PlayerRelation::Opponent,
                        })
                    }
                    _ => continue,
                };
                return Some(make_trigger(
                    EventKind::BecomesTarget,
                    EventFilterBuilder {
                        target_player: Some(RelativeControl::You),
                        targeted_by: Some(kind_val),
                        player,
                        ..Default::default()
                    },
                ));
            }
        }
    }

    if let Some(after_deals) = strip_prefix_word_ci(&body_low, "a source deals") {
        let (combat, noncombat_only) = match after_deals {
            "damage to ~" => Some((None, false)),
            "combat damage to ~" => Some((Some(true), false)),
            "noncombat damage to ~" => Some((None, true)),
            _ => None,
        }
        .unzip();
        if let (Some(c), Some(nc)) = (combat, noncombat_only) {
            return Some(make_trigger(
                EventKind::DealtDamage,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    combat: c,
                    noncombat_only: nc,
                    ..Default::default()
                },
            ));
        }
    }

    if let Some(counter) = counter_event(body, ctx) {
        return Some(counter);
    }

    if body_low == "the ring tempts you" {
        return Some(make_trigger(
            EventKind::RingTempted,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                ..Default::default()
            },
        ));
    }

    if matches!(
        body_low.as_str(),
        "a creature dealt damage by ~ this turn dies"
            | "creature dealt damage by ~ this turn dies"
            | "a creature dealt combat damage by ~ this turn dies"
            | "creature dealt combat damage by ~ this turn dies"
            | "a creature dealt damage by ~ this turn is put into a graveyard from the battlefield"
            | "creature dealt damage by ~ this turn is put into a graveyard from the battlefield"
            | "a creature dealt combat damage by ~ this turn is put into a graveyard from the battlefield"
            | "creature dealt combat damage by ~ this turn is put into a graveyard from the battlefield"
    ) {
        return Some(make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::Filter {
                    filter: Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    }),
                }),
                from: ZoneMask::BATTLEFIELD,
                to: Some(Zone::Graveyard),
                dealt_damage_by_self_this_turn: true,
                ..Default::default()
            },
        ));
    }

    // "(one or more|a|an) [<qual> ]cards? leaves? your graveyard[ during your turn]"
    if let Some((one_or_more, after_qty)) = strip_prefix_word_ci(body, "one or more")
        .map(|r| (true, r))
        .or_else(|| strip_prefix_word_ci(body, "a").map(|r| (false, r)))
        .or_else(|| strip_prefix_word_ci(body, "an").map(|r| (false, r)))
    {
        let (before_turn, during_turn) =
            if let Some(l) = strip_suffix_ws_ci(after_qty, "during your turn") {
                (l, Some(DuringTurnKind::Your))
            } else {
                (after_qty, None)
            };
        let qual_opt = [
            "cards leave your graveyard",
            "cards leaves your graveyard",
            "card leave your graveyard",
            "card leaves your graveyard",
        ]
        .iter()
        .find_map(|sfx| {
            if before_turn.eq_ignore_ascii_case(sfx) {
                Some(None)
            } else {
                strip_suffix_ws_ci(before_turn, sfx).map(Some)
            }
        });
        if let Some(q_opt) = qual_opt {
            let cp = ctx.interner.checkpoint();
            if let Some(obj) = parse_graveyard_leave_card_filter(q_opt, ctx) {
                return Some(make_trigger(
                    EventKind::GraveyardLeaveBatch,
                    EventFilterBuilder {
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        one_or_more,
                        during_turn,
                        ..Default::default()
                    },
                ));
            }
            ctx.interner.rollback(cp);
        }
    }

    if let Some(mid) = strip_prefix_word_ci(body, "~ and at least").and_then(|r| {
        strip_suffix_ws_ci(r, "other creatures attack")
            .or_else(|| strip_suffix_ws_ci(r, "other creature attack"))
    }) && let Some(n_other) = parse_small_u16(mid)
        && n_other >= 1
    {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                min_attackers: Some(n_other + 1),
                ..Default::default()
            },
        ));
    }
    if let Some(mid) = strip_prefix_word_ci(body, "equipped creature and at least").and_then(|r| {
        strip_suffix_ws_ci(r, "other creatures attack")
            .or_else(|| strip_suffix_ws_ci(r, "other creature attack"))
    }) && let Some(n_other) = parse_small_u16(mid)
        && n_other >= 1
    {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                object: Some(TriggerSubject::AttachedTo),
                min_attackers: Some(n_other + 1),
                ..Default::default()
            },
        ));
    }
    if let Some(after_al) = strip_prefix_word_ci(body, "~ and at least")
        && let Some((num_w, after_num)) = after_al.split_once(char::is_whitespace)
        && let Some(min_co) = parse_small_u16(num_w)
        && min_co >= 1
        && let Some(co_raw) = strip_suffix_ws_ci(after_num.trim_start(), "attack")
    {
        let cp = ctx.interner.checkpoint();
        if let Some(co_filter) = parse_trigger_object_phrase(co_raw, ctx, false) {
            return Some(make_trigger(
                EventKind::Attacks,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    co_attacker_filter: Some(Box::new(co_filter)),
                    co_attacker_min: Some(min_co),
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    if body_low == "enchanted player is attacked" {
        return Some(make_trigger(
            EventKind::Attacks,
            EventFilterBuilder {
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Any,
                }),
                one_or_more: true,
                attacked_target: Some(AttackedTargetFilter::EnchantedPlayer),
                ..Default::default()
            },
        ));
    }

    // "a <subj>'s ability causes (you|a player|an opponent) to add [one or more ](mana of the chosen color|{C})"
    if let Some(after_a) = strip_prefix_word_ci(body, "a")
        && let Some((subj_inner, after_ability)) =
            split_once_ws_word_ci(after_a, "'s ability causes")
                .or_else(|| after_a.split_once("'s ability causes "))
    {
        let pl_parsed = [
            ("you to add", PlayerRelation::You),
            ("a player to add", PlayerRelation::Any),
            ("an opponent to add", PlayerRelation::Opponent),
        ]
        .iter()
        .find_map(|(pref, rel)| {
            strip_prefix_word_ci(after_ability.trim_start(), pref).map(|r| (*rel, r))
        });
        if let Some((pl_rel, after_add)) = pl_parsed {
            let after_oom = strip_prefix_word_ci(after_add, "one or more").unwrap_or(after_add);
            let mana_kind = if after_oom.eq_ignore_ascii_case("mana of the chosen color") {
                Some(None)
            } else {
                parse_braced_mana_color(after_oom).map(Some)
            };
            if let Some(mc_opt) = mana_kind {
                let cp = ctx.interner.checkpoint();
                if let Some(subj) = object_subject(&format!("a {subj_inner}"), ctx)
                    && let TriggerSubject::Filter { ref filter } = subj.object
                    && object_filter_problem(filter).is_none()
                {
                    return Some(make_trigger(
                        EventKind::ManaAddedByLandAbility,
                        EventFilterBuilder {
                            object: Some(subj.object),
                            player: Some(PlayerFilter { relation: pl_rel }),
                            tapped_mana_color: mc_opt,
                            tapped_mana_chosen_color: mc_opt.is_none(),
                            ..Default::default()
                        },
                    ));
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // "you or (a|an|another) <perm> you control becomes? the target of ..."
    if let Some(after_you_or) = strip_prefix_word_ci(body, "you or")
        && let Some((art, after_art)) = strip_prefix_word_ci(after_you_or, "a")
            .map(|r| ("a", r))
            .or_else(|| strip_prefix_word_ci(after_you_or, "an").map(|r| ("an", r)))
            .or_else(|| strip_prefix_word_ci(after_you_or, "another").map(|r| ("another", r)))
        && let Some((perm_raw, after_becomes)) =
            split_once_ws_word_ci(after_art, "you control becomes the target of")
                .or_else(|| split_once_ws_word_ci(after_art, "you control become the target of"))
    {
        let (after_by, first_time) =
            if let Some(l) = strip_suffix_ws_ci(after_becomes, "for the first time each turn") {
                (l, true)
            } else {
                (after_becomes, false)
            };
        for (kind_str, kind_val) in [
            ("a spell or ability", TargetedByKind::SpellOrAbility),
            ("a spell", TargetedByKind::Spell),
            ("an ability", TargetedByKind::Ability),
        ] {
            if let Some(ctrl_raw) = strip_prefix_ci(after_by, kind_str) {
                let ctrl = ctrl_raw.trim();
                let player = if ctrl.is_empty() {
                    None
                } else if ctrl.eq_ignore_ascii_case("you control") {
                    Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    })
                } else if ctrl.eq_ignore_ascii_case("an opponent controls")
                    || ctrl.eq_ignore_ascii_case("your opponents control")
                    || ctrl.eq_ignore_ascii_case("you don't control")
                {
                    Some(PlayerFilter {
                        relation: PlayerRelation::Opponent,
                    })
                } else {
                    continue;
                };
                let cp = ctx.interner.checkpoint();
                if let Some(perm_subj) =
                    object_subject(&format!("{art} {perm_raw} you control"), ctx)
                    && let TriggerSubject::Filter { ref filter } = perm_subj.object
                    && object_filter_problem(filter).is_none()
                {
                    let t1 = make_trigger(
                        EventKind::BecomesTarget,
                        EventFilterBuilder {
                            target_player: Some(RelativeControl::You),
                            targeted_by: Some(kind_val),
                            player,
                            first_time_each_turn: first_time,
                            ..Default::default()
                        },
                    );
                    let t2 = make_trigger(
                        EventKind::BecomesTarget,
                        EventFilterBuilder {
                            object: Some(perm_subj.object),
                            targeted_by: Some(kind_val),
                            player,
                            first_time_each_turn: first_time,
                            ..Default::default()
                        },
                    );
                    return Some(merge_compound_trigger_defs(t1, t2));
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    if body_low == "~ is milled" {
        let mut t = make_trigger(
            EventKind::ZoneChange,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                from: ZoneMask::LIBRARY,
                to: Some(Zone::Graveyard),
                ..Default::default()
            },
        );
        t.from_zones = Some(ZoneMask::GRAVEYARD);
        return Some(t);
    }

    // "(one or more|a|an) [<qual> ]cards? (is|are) milled[ during your turn]"
    if let Some((one_or_more, after_qty)) = strip_prefix_word_ci(body, "one or more")
        .map(|r| (true, r))
        .or_else(|| strip_prefix_word_ci(body, "a").map(|r| (false, r)))
        .or_else(|| strip_prefix_word_ci(body, "an").map(|r| (false, r)))
    {
        let (before_turn, during_turn) =
            if let Some(l) = strip_suffix_ws_ci(after_qty, "during your turn") {
                (l, Some(DuringTurnKind::Your))
            } else {
                (after_qty, None)
            };
        let qual_opt = [
            "cards is milled",
            "cards are milled",
            "card is milled",
            "card are milled",
        ]
        .iter()
        .find_map(|sfx| {
            if before_turn.eq_ignore_ascii_case(sfx) {
                Some(None)
            } else {
                strip_suffix_ws_ci(before_turn, sfx).map(Some)
            }
        });
        if let Some(q_opt) = qual_opt {
            let cp = ctx.interner.checkpoint();
            if let Some(obj) = parse_graveyard_leave_card_filter(q_opt, ctx) {
                return Some(make_trigger(
                    EventKind::ZoneChange,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        from: ZoneMask::LIBRARY,
                        to: Some(Zone::Graveyard),
                        one_or_more,
                        during_turn,
                        ..Default::default()
                    },
                ));
            }
            ctx.interner.rollback(cp);
        }
    }

    // "<N> or more <obj> attack[ a player| an opponent| enchanted player]"
    if let Some((num_w, after_num)) = body.split_once(char::is_whitespace)
        && let Some(min_n) = parse_small_u16(num_w)
        && let Some(after_or_more) = strip_prefix_word_ci(after_num.trim_start(), "or more")
    {
        let (obj_raw, suffix_kind) =
            if let Some(l) = strip_suffix_ws_ci(after_or_more, "attack a player") {
                (l, Some(1))
            } else if let Some(l) = strip_suffix_ws_ci(after_or_more, "attack an opponent") {
                (l, Some(2))
            } else if let Some(l) = strip_suffix_ws_ci(after_or_more, "attack enchanted player") {
                (l, Some(3))
            } else if let Some(l) = strip_suffix_ws_ci(after_or_more, "attack") {
                (l, None)
            } else {
                ("", Some(99))
            };
        if !obj_raw.is_empty()
            && suffix_kind != Some(99)
            && (if suffix_kind.is_some() {
                min_n >= 1
            } else {
                min_n >= 2
            })
        {
            let cp = ctx.interner.checkpoint();
            if let Some(obj) = parse_trigger_object_phrase(obj_raw, ctx, false)
                && object_filter_problem(&obj).is_none()
                && matches!(
                    obj.controller,
                    None | Some(RelativeControl::You) | Some(RelativeControl::Opponent)
                )
            {
                let player =
                    (obj.controller == Some(RelativeControl::You)).then_some(PlayerFilter {
                        relation: PlayerRelation::You,
                    });
                return Some(make_trigger(
                    EventKind::Attacks,
                    EventFilterBuilder {
                        player,
                        one_or_more: true,
                        min_attackers: (min_n >= 2).then_some(min_n),
                        object: Some(TriggerSubject::Filter {
                            filter: Box::new(obj),
                        }),
                        per_attacked_player: suffix_kind == Some(1),
                        attacked_target: match suffix_kind {
                            Some(2) => Some(AttackedTargetFilter::Opponent),
                            Some(3) => Some(AttackedTargetFilter::EnchantedPlayer),
                            _ => None,
                        },
                        ..Default::default()
                    },
                ));
            }
            ctx.interner.rollback(cp);
        }
    }

    if body_low == "players finish voting" {
        return Some(make_trigger(
            EventKind::PlayersFinishedVoting,
            EventFilterBuilder::default(),
        ));
    }
    if body_low == "the final chapter ability of ~ resolves" {
        return Some(make_trigger(
            EventKind::SagaChapterResolved,
            EventFilterBuilder {
                object: Some(TriggerSubject::SelfObj),
                final_chapter: true,
                ..Default::default()
            },
        ));
    }
    if let Some(subj_raw) = strip_prefix_word_ci(body, "the final chapter ability of")
        .and_then(|r| strip_suffix_ws_ci(r, "resolves"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(sub) = object_subject(subj_raw, ctx)
            && !sub.one_or_more
        {
            return Some(make_trigger(
                EventKind::SagaChapterResolved,
                EventFilterBuilder {
                    object: Some(sub.object),
                    final_chapter: true,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }
    if let Some(subj_raw) = strip_prefix_word_ci(body, "a chapter ability of")
        .and_then(|r| strip_suffix_ws_ci(r, "resolves"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(sub) = object_subject(subj_raw, ctx)
            && !sub.one_or_more
        {
            return Some(make_trigger(
                EventKind::SagaChapterResolved,
                EventFilterBuilder {
                    object: Some(sub.object),
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(spell_raw) = strip_prefix_word_ci(body, "enchanted player casts")
        .or_else(|| strip_prefix_word_ci(body, "enchanted player cast"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(obj) = spell_filter(spell_raw, ctx) {
            return Some(make_trigger(
                EventKind::Cast,
                EventFilterBuilder {
                    object: Some(obj),
                    enchanted_player_casts: true,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // "<spell> is countered[ by (a|an) (spell or ability|spell|ability)[ (you control|an opponent controls)]]"
    if let Some((spell_raw, after_ctr)) = split_once_ws_word_ci(body, "is countered")
        .or_else(|| strip_suffix_ws_ci(body, "is countered").map(|l| (l, "")))
    {
        let cp = ctx.interner.checkpoint();
        let ctrl_ok = if after_ctr.is_empty() {
            Some(None)
        } else if let Some(after_by) = strip_prefix_word_ci(after_ctr, "by")
            .and_then(|r| strip_prefix_word_ci(r, "a").or_else(|| strip_prefix_word_ci(r, "an")))
        {
            let kind_rem = strip_prefix_word_ci(after_by, "spell or ability")
                .or_else(|| strip_prefix_word_ci(after_by, "spell"))
                .or_else(|| strip_prefix_word_ci(after_by, "ability"))
                .or_else(|| {
                    matches!(
                        after_by.to_ascii_lowercase().as_str(),
                        "spell or ability" | "spell" | "ability"
                    )
                    .then_some("")
                });
            match kind_rem {
                Some("") => Some(None),
                Some(c) if c.eq_ignore_ascii_case("you control") => Some(Some(PlayerFilter {
                    relation: PlayerRelation::You,
                })),
                Some(c) if c.eq_ignore_ascii_case("an opponent controls") => {
                    Some(Some(PlayerFilter {
                        relation: PlayerRelation::Opponent,
                    }))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(player) = ctrl_ok
            && let Some(sf) = spell_filter(spell_raw, ctx)
        {
            return Some(make_trigger(
                EventKind::SpellCountered,
                EventFilterBuilder {
                    object: Some(sf),
                    player,
                    ..Default::default()
                },
            ));
        }
        ctx.interner.rollback(cp);
    }

    // "an? [loyalty ]ability of (~|<subj>) is activated[ (and|if) it isn't a mana ability]"
    if let Some(after_a) =
        strip_prefix_word_ci(body, "a").or_else(|| strip_prefix_word_ci(body, "an"))
    {
        let (is_loyalty_ability, after_loy) =
            if let Some(r) = strip_prefix_word_ci(after_a, "loyalty") {
                (true, r)
            } else {
                (false, after_a)
            };
        if let Some(after_ab_of) = strip_prefix_word_ci(after_loy, "ability of") {
            let (before_mana, is_non_mana_ability) = if let Some(l) =
                strip_suffix_ws_ci(after_ab_of, "and it isn't a mana ability")
                    .or_else(|| strip_suffix_ws_ci(after_ab_of, "if it isn't a mana ability"))
            {
                (l, true)
            } else {
                (after_ab_of, false)
            };
            if let Some(src_raw) = strip_suffix_ws_ci(before_mana, "is activated") {
                let cp = ctx.interner.checkpoint();
                let ability_source = if src_raw == "~" {
                    Some(TriggerSubject::SelfObj)
                } else if let Some(noun) = strip_prefix_word_ci(src_raw, "enchanted")
                    .or_else(|| strip_prefix_word_ci(src_raw, "equipped"))
                    && matches!(
                        noun.to_ascii_lowercase().as_str(),
                        "creature"
                            | "permanent"
                            | "artifact"
                            | "land"
                            | "enchantment"
                            | "planeswalker"
                    )
                {
                    Some(TriggerSubject::AttachedTo)
                } else if let Some(sub) = object_subject(src_raw, ctx)
                    && matches!(sub.object, TriggerSubject::Filter { .. })
                {
                    Some(sub.object)
                } else {
                    None
                };
                if let Some(src) = ability_source {
                    return Some(make_trigger(
                        EventKind::AbilityActivated,
                        EventFilterBuilder {
                            is_loyalty_ability,
                            is_non_mana_ability,
                            ability_source: Some(src),
                            ..Default::default()
                        },
                    ));
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // Player subjects: "^(you|a player|an opponent) <rest>"
    for (p_str, p_rel) in [
        ("you", PlayerRelation::You),
        ("a player", PlayerRelation::Any),
        ("an opponent", PlayerRelation::Opponent),
    ] {
        if let Some(rest) = strip_prefix_word_ci(body, p_str) {
            let cp = ctx.interner.checkpoint();
            if let Some(ev) = player_event(PlayerFilter { relation: p_rel }, rest, ctx) {
                return Some(ev);
            }
            ctx.interner.rollback(cp);
        }
    }

    // Object subjects: split at spaces longest subject prefix first
    let ws_positions: Vec<usize> = body
        .char_indices()
        .filter_map(|(idx, ch)| (ch == ' ').then_some(idx))
        .collect();
    for &sp_idx in ws_positions.iter().rev() {
        let subj_slice = body[..sp_idx].trim();
        let verb_slice = body[sp_idx + 1..].trim();
        if subj_slice.is_empty() || verb_slice.is_empty() {
            continue;
        }
        let cp = ctx.interner.checkpoint();
        if let Some(subject) = object_subject(subj_slice, ctx)
            && let Some(ev) = object_events(verb_slice, &subject, ctx)
        {
            return Some(ev);
        }
        ctx.interner.rollback(cp);
    }

    // Shared-prefix "or" split (depth < 2)
    if depth < 2 {
        let or_hits = find_ws_word_ci_all(body, "or");
        for (start, end) in or_hits.into_iter().take(12) {
            let left_body = body[..start].trim();
            let right_body = body[end..].trim();
            if left_body.is_empty() || right_body.is_empty() {
                continue;
            }
            let cp = ctx.interner.checkpoint();
            if let Some(left) = trigger_head(&format!("{kw_word} {left_body}"), ctx, depth + 1) {
                if let Some(right) =
                    trigger_head(&format!("{kw_word} {right_body}"), ctx, depth + 1)
                {
                    return Some(merge_compound_trigger_defs(left, right));
                }
                for p_str in ["you", "a player", "an opponent"] {
                    if strip_prefix_word_ci(left_body, p_str).is_some()
                        && let Some(right) =
                            trigger_head(&format!("{kw_word} {p_str} {right_body}"), ctx, depth + 1)
                    {
                        return Some(merge_compound_trigger_defs(left, right));
                    }
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    None
}

// -----------------------------------------------------------------------------
// Intervening-if and inline-while condition parsing
// -----------------------------------------------------------------------------

fn parse_trigger_condition_wave9(t: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let low = t.to_ascii_lowercase();
    if low == "if it's your main phase" {
        return Some(Condition::IsYourMainPhase);
    }
    if low == "if a counter was put on ~ this turn" {
        return Some(Condition::CounterPutOnSelfThisTurn);
    }
    if low == "if that ability isn't a mana ability" || low == "if it isn't a mana ability" {
        return Some(Condition::EventAbilityIsNotMana);
    }
    if low == "if that player had another land enter the battlefield under their control this turn"
    {
        return Some(Condition::PlayerHadAnotherLandEnterThisTurn);
    }
    if low == "if one or more of them entered from a graveyard or was cast from a graveyard" {
        return Some(Condition::EventEnteredOrCastFromGraveyardAny);
    }
    if matches!(
        low.as_str(),
        "if there are cards exiled with it"
            | "if there are cards exiled with ~"
            | "if there are card exiled with it"
            | "if there are card exiled with ~"
    ) {
        return Some(Condition::HasCardsExiledWithSelf);
    }
    if low
        == "if it doesn't have the same name as another creature you control or a creature card in your graveyard"
    {
        return Some(Condition::NoSameNameControlledOrInGraveyard);
    }
    if let Some(kw_raw) = strip_prefix_word_ci(&low, "if you control no creatures with")
        && !kw_raw.is_empty()
        && kw_raw.bytes().all(|b| b.is_ascii_lowercase())
    {
        let kw_id = ctx.interner.intern_keyword(kw_raw);
        let mut f = ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            controller: Some(RelativeControl::You),
            ..Default::default()
        };
        let mut kws = SmallVec::new();
        kws.push(kw_id);
        f.extra.push(ObjectPredicate::HasKeywords { keywords: kws });
        return Some(Condition::Compare {
            left: Box::new(Amount::Count { of: Box::new(f) }),
            op: CompareOp::Eq,
            right: const_amount(0),
        });
    }

    // "if (a|an|<N> or more) <filter> (was|were) put into your graveyard from the battlefield this turn"
    if let Some(after_if) = strip_prefix_word_ci(t, "if")
        && let Some(mid) = strip_suffix_ws_ci(
            after_if,
            "was put into your graveyard from the battlefield this turn",
        )
        .or_else(|| {
            strip_suffix_ws_ci(
                after_if,
                "were put into your graveyard from the battlefield this turn",
            )
        })
    {
        let parsed = if let Some(rest) =
            strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
        {
            Some((1u16, rest))
        } else if let Some((num_w, after_num)) = mid.split_once(char::is_whitespace)
            && let Some(n) = parse_small_u16(num_w)
            && let Some(rest) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((n, rest))
        } else {
            None
        };
        if let Some((min_n, filter_raw)) = parsed {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) = parse_trigger_object_phrase(filter_raw, ctx, false)
                && object_filter_problem(&f).is_none()
            {
                f.owner = Some(RelativeControl::You);
                return Some(Condition::ZoneChangeFromToThisTurn {
                    from: Some(Zone::Battlefield),
                    to: Zone::Graveyard,
                    filter: Box::new(f),
                    min: min_n,
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    if low == "if it wasn't put onto the battlefield with this ability" {
        return Some(Condition::NotPutOntoBattlefieldByThisAbility);
    }
    if low == "if the amount of mana spent to cast that spell is greater than ~'s power" {
        return Some(Condition::ManaSpentGreaterThanSelfPower);
    }

    // "if there are N or more counters among <group> you control"
    if let Some(after_ta) = strip_prefix_word_ci(t, "if there are")
        && let Some((num_w, after_num)) = after_ta.split_once(char::is_whitespace)
        && let Some(min) = parse_small_u16(num_w)
        && min >= 1
        && let Some(after_ca) =
            strip_prefix_word_ci(after_num.trim_start(), "or more counters among")
        && let Some(group_raw) = strip_suffix_ws_ci(after_ca, "you control")
    {
        let cp = ctx.interner.checkpoint();
        let parsed_group = if group_raw.eq_ignore_ascii_case("artifacts and creatures") {
            Some(ObjectFilter {
                extra: vec![ObjectPredicate::AnyOf {
                    filters: vec![
                        ObjectFilter {
                            types_any: CardTypeMask::ARTIFACT,
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        },
                        ObjectFilter {
                            types_any: CardTypeMask::CREATURE,
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        },
                    ],
                }],
                ..Default::default()
            })
        } else {
            parse_trigger_object_phrase(&format!("{group_raw} you control"), ctx, false)
        };
        if let Some(f) = parsed_group
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::CountersAmongFilterAtLeast {
                filter: Box::new(f),
                min,
            });
        }
        ctx.interner.rollback(cp);
    }

    // "if N or more of those creatures are attacking you and/or planeswalkers you control"
    if let Some(after_if) = strip_prefix_word_ci(&low, "if")
        && let Some(num_w) = strip_suffix_ws_ci(
            after_if,
            "or more of those creatures are attacking you and/or planeswalkers you control",
        )
        && let Some(min) = parse_small_u16(num_w)
        && min >= 1
    {
        return Some(Condition::EventMinAttackersToYouOrPlaneswalkers { min });
    }

    // "if its toughness was less than N"
    if let Some(num_w) = strip_prefix_word_ci(&low, "if its toughness was less than")
        && !num_w.contains(char::is_whitespace)
        && let Some(max) = parse_small_i32(num_w)
    {
        return Some(Condition::LkiToughnessLessThan { max });
    }

    // "if any of that damage was dealt by (a|an) <Subtype>"
    if let Some(after_dmg) = strip_prefix_word_ci(t, "if any of that damage was dealt by")
        && let Some(sub_raw) =
            strip_prefix_word_ci(after_dmg, "a").or_else(|| strip_prefix_word_ci(after_dmg, "an"))
        && is_bare_capitalized_word(sub_raw)
    {
        let sub_name = singular_subtype(sub_raw).unwrap_or_else(|| sub_raw.to_string());
        let id = ctx.interner.intern_subtype(&sub_name);
        return Some(Condition::EventDamageDealtBySubtype { subtype: id });
    }

    if low == "if it's attacking the player with the most life or tied for most life" {
        return Some(Condition::AnyAttackedPlayerWithMostLife);
    }
    if low == "if you didn't activate a loyalty ability of a planeswalker this turn" {
        return Some(Condition::DidntActivateLoyaltyThisTurn);
    }
    if let Some(after_exact) = strip_prefix_word_ci(&low, "if you have exactly")
        && let Some(num_w) = strip_suffix_ws_ci(after_exact, "cards in your hand")
            .or_else(|| strip_suffix_ws_ci(after_exact, "card in your hand"))
        && let Some(n) = parse_small_i32(num_w)
        && n >= 0
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: const_amount(n),
        });
    }

    None
}

fn parse_trigger_condition_p22b(t: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let low = t.to_ascii_lowercase();

    if matches!(
        low.as_str(),
        "if that ability isn't a mana ability"
            | "if that ability is not a mana ability"
            | "if the ability isn't a mana ability"
            | "if the ability is not a mana ability"
    ) {
        return Some(Condition::EventAbilityIsNotMana);
    }
    if low == "if life was paid to activate it"
        || low == "if life was paid to activate that ability"
    {
        return Some(Condition::EventAbilityLifePaid);
    }
    if low == "if that spell was kicked" || low == "if the spell was kicked" {
        return Some(Condition::EventSpellWasKicked);
    }
    if let Some(kw_raw) = strip_prefix_word_ci(&low, "if that spell has")
        .or_else(|| strip_prefix_word_ci(&low, "if the spell has"))
        && !kw_raw.is_empty()
        && kw_raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b' ' || b == b'-')
    {
        return Some(Condition::EventSpellHasKeyword {
            keyword: kw_raw.trim().to_string(),
        });
    }
    if low == "if no colored mana was spent to cast that spell"
        || low == "if no colored mana was spent to cast the spell"
    {
        return Some(Condition::EventSpellNoColoredManaSpent);
    }
    if matches!(
        low.as_str(),
        "if the amount of mana spent to cast that spell is less than its mana value"
            | "if the amount of mana spent to cast that spell was less than its mana value"
            | "if the amount of mana spent to cast the spell is less than its mana value"
            | "if the amount of mana spent to cast the spell was less than its mana value"
            | "if mana spent to cast that spell is less than its mana value"
            | "if mana spent to cast that spell was less than its mana value"
            | "if mana spent to cast the spell is less than its mana value"
            | "if mana spent to cast the spell was less than its mana value"
    ) {
        return Some(Condition::EventSpellManaSpentLessThanMv);
    }

    // "if (it|that <noun>) (entered[ the battlefield]|was cast) from [your |a |an opponent's |their ](graveyard|exile|hand|library)"
    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        let after_subj = strip_prefix_word_ci(after_if, "it").or_else(|| {
            strip_prefix_word_ci(after_if, "that").and_then(|r| {
                r.split_once(char::is_whitespace)
                    .map(|(_, tail)| tail.trim_start())
            })
        });
        if let Some(rest_subj) = after_subj
            && let Some(after_verb) =
                strip_prefix_word_ci(rest_subj, "entered the battlefield from")
                    .or_else(|| strip_prefix_word_ci(rest_subj, "entered from"))
                    .or_else(|| strip_prefix_word_ci(rest_subj, "was cast from"))
        {
            let zone_raw = strip_prefix_word_ci(after_verb, "your")
                .or_else(|| strip_prefix_word_ci(after_verb, "a"))
                .or_else(|| strip_prefix_word_ci(after_verb, "an opponent's"))
                .or_else(|| strip_prefix_word_ci(after_verb, "their"))
                .unwrap_or(after_verb);
            if matches!(zone_raw, "graveyard" | "exile" | "hand" | "library")
                && let Some(zone) = parse_zone_name(zone_raw)
            {
                return Some(Condition::EventEnteredOrCastFromZone { zone });
            }
        }
    }

    if matches!(
        low.as_str(),
        "if its power or toughness is greater than ~'s"
            | "if its power or toughness is greater than ~'s power or toughness"
            | "if that creature's power or toughness is greater than ~'s"
            | "if that creature's power or toughness is greater than ~'s power or toughness"
            | "if that permanent's power or toughness is greater than ~'s"
            | "if that permanent's power or toughness is greater than ~'s power or toughness"
    ) {
        return Some(Condition::EventObjectPowerOrToughnessGreaterThanSelf);
    }

    // "if (that|the) (creature|permanent|spell|card) (wasn't|was not|isn't|is not) (a|an) <Subtype>"
    if let Some(after_noun) = [
        "if that creature",
        "if that permanent",
        "if that spell",
        "if that card",
        "if the creature",
        "if the permanent",
        "if the spell",
        "if the card",
    ]
    .iter()
    .find_map(|p| strip_prefix_word_ci(t, p))
        && let Some(after_neg) = ["wasn't", "was not", "isn't", "is not"]
            .iter()
            .find_map(|v| strip_prefix_word_ci(after_noun, v))
        && let Some(sub_raw) =
            strip_prefix_word_ci(after_neg, "a").or_else(|| strip_prefix_word_ci(after_neg, "an"))
        && is_bare_capitalized_word(sub_raw)
        && let Some(sub) = singular_subtype(sub_raw)
    {
        let id = ctx.interner.intern_subtype(&sub);
        return Some(Condition::EventObjectWasNotSubtype { subtype: id });
    }

    if matches!(
        low.as_str(),
        "if it was enchanted or equipped"
            | "if it was equipped or enchanted"
            | "if that creature was enchanted or equipped"
            | "if that creature was equipped or enchanted"
            | "if that permanent was enchanted or equipped"
            | "if that permanent was equipped or enchanted"
    ) {
        return Some(Condition::EventWasAttachedEnchantedOrEquipped);
    }

    // "if (<N> creatures? crewed it[ this way]|it was crewed by [exactly ]<N> creatures?)"
    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        let crew_num = if let Some(num_w) = strip_prefix_word_ci(after_if, "it was crewed by")
            .map(|r| strip_prefix_word_ci(r, "exactly").unwrap_or(r))
            .and_then(|r| {
                strip_suffix_ws_ci(r, "creatures").or_else(|| strip_suffix_ws_ci(r, "creature"))
            }) {
            parse_small_u16(num_w)
        } else if let Some((num_w, after_num)) = after_if.split_once(char::is_whitespace)
            && matches!(
                after_num.trim_start(),
                "creatures crewed it"
                    | "creature crewed it"
                    | "creatures crewed it this way"
                    | "creature crewed it this way"
            )
        {
            parse_small_u16(num_w)
        } else {
            None
        };
        if let Some(count) = crew_num {
            return Some(Condition::EventCrewCountEquals { count });
        }
    }

    // "if (the|that) damage was dealt by (a|an) <Subtype>"
    if let Some(after_by) = strip_prefix_word_ci(t, "if the damage was dealt by")
        .or_else(|| strip_prefix_word_ci(t, "if that damage was dealt by"))
        && let Some(sub_raw) =
            strip_prefix_word_ci(after_by, "a").or_else(|| strip_prefix_word_ci(after_by, "an"))
        && is_bare_capitalized_word(sub_raw)
        && let Some(sub) = singular_subtype(sub_raw)
    {
        let id = ctx.interner.intern_subtype(&sub);
        return Some(Condition::EventDamageDealtBySubtype { subtype: id });
    }

    if matches!(
        low.as_str(),
        "if that creature was dealt excess damage this turn"
            | "if that permanent was dealt excess damage this turn"
            | "if that planeswalker was dealt excess damage this turn"
            | "if the creature was dealt excess damage this turn"
            | "if the permanent was dealt excess damage this turn"
            | "if the planeswalker was dealt excess damage this turn"
    ) {
        return Some(Condition::EventDamagedTargetDealtExcessDamageThisTurn);
    }

    if matches!(
        low.as_str(),
        "if you're attacking the player with the most life"
            | "if you're attacking the player with the most life or tied for most life"
            | "if that player is attacking the player with the most life"
            | "if that player is attacking the player with the most life or tied for most life"
            | "if they're attacking the player with the most life"
            | "if they're attacking the player with the most life or tied for most life"
            | "if they are attacking the player with the most life"
            | "if they are attacking the player with the most life or tied for most life"
    ) {
        return Some(Condition::EventAttackingPlayerWithMostLife);
    }

    if matches!(
        low.as_str(),
        "if there's another opponent didn't get attacked"
            | "if there's another opponent that didn't get attacked"
            | "if there is another opponent didn't get attacked"
            | "if there is another opponent that didn't get attacked"
            | "if another opponent wasn't attacked"
            | "if another opponent wasn't attacked this combat"
    ) {
        return Some(Condition::EventAttackerHasAnotherUnattackedOpponent);
    }

    if let Some(after_if) = strip_prefix_word_ci(&low, "if")
        && let Some((num_w, after_num)) = after_if.split_once(char::is_whitespace)
        && let Some(min) = parse_small_u16(num_w)
        && let Some(after_om) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        && matches!(
            after_om,
            "creatures are attacking you and/or planeswalkers you control"
                | "creatures are attacking you and/or planeswalker you control"
                | "creatures are attacking you or planeswalkers you control"
                | "creatures are attacking you or planeswalker you control"
                | "creatures were attacking you and/or planeswalkers you control"
                | "creatures were attacking you or planeswalkers you control"
                | "creature are attacking you and/or planeswalkers you control"
                | "creature are attacking you or planeswalkers you control"
                | "creature were attacking you and/or planeswalkers you control"
                | "creature were attacking you or planeswalkers you control"
        )
    {
        return Some(Condition::EventMinAttackersToYouOrPlaneswalkers { min });
    }

    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        let after_subj = strip_prefix_word_ci(after_if, "it").or_else(|| {
            strip_prefix_word_ci(after_if, "that").and_then(|r| {
                r.split_once(char::is_whitespace)
                    .map(|(_, tail)| tail.trim_start())
            })
        });
        if let Some(rest_subj) = after_subj
            && matches!(
                rest_subj,
                "shares a card type with the exiled card"
                    | "share a card type with the exiled card"
            )
        {
            return Some(Condition::EventSharesCardTypeWithLinkedExile);
        }
    }

    if let Some(mid) = strip_prefix_word_ci(t, "if that player controls more")
        .and_then(|r| strip_suffix_ws_ci(r, "than you"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(mid, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::EventPlayerControlsMoreThanYou {
                filter: Box::new(f),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(mid) = strip_prefix_word_ci(t, "if that player controls more")
        .and_then(|r| strip_suffix_ws_ci(r, "than each other player"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(mid, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::EventPlayerControlsMoreThanEachOther {
                filter: Box::new(f),
            });
        }
        ctx.interner.rollback(cp);
    }

    if matches!(
        low.as_str(),
        "if you have the most life" | "if you have the most life or are tied for the most life"
    ) {
        return Some(Condition::YouHaveMostLife);
    }
    if matches!(
        low.as_str(),
        "if that player lost life last turn"
            | "if that player lost life their last turn"
            | "if they lost life last turn"
            | "if they lost life their last turn"
    ) {
        return Some(Condition::PlayerLostLifeLastTurn);
    }
    if let Some((who, after_who)) = [
        ("if you", RelativeControl::You),
        ("if an opponent", RelativeControl::Opponent),
        ("if a player", RelativeControl::Any),
    ]
    .iter()
    .find_map(|(pref, rel)| strip_prefix_word_ci(&low, pref).map(|r| (*rel, r)))
        && after_who == "discarded a card this turn"
    {
        return Some(Condition::DiscardedCardThisTurn { who });
    }
    if low == "if you put a counter on a creature this turn" {
        return Some(Condition::PutCounterOnCreatureThisTurn {
            who: RelativeControl::You,
        });
    }
    if low == "if a player put a counter on a creature this turn" {
        return Some(Condition::PutCounterOnCreatureThisTurn {
            who: RelativeControl::Any,
        });
    }
    if low == "if a counter was put on a creature you control this turn" {
        return Some(Condition::PutCounterOnCreatureThisTurn {
            who: RelativeControl::You,
        });
    }
    if low == "if a counter was put on a creature this turn" {
        return Some(Condition::PutCounterOnCreatureThisTurn {
            who: RelativeControl::Any,
        });
    }

    if let Some((who, after_sac)) = strip_prefix_word_ci(t, "if you sacrificed")
        .map(|r| (RelativeControl::You, r))
        .or_else(|| {
            strip_prefix_word_ci(t, "if a player sacrificed").map(|r| (RelativeControl::Any, r))
        })
        && let Some(mid) = strip_suffix_ws_ci(after_sac, "this turn")
    {
        let parsed = if let Some(r) =
            strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
        {
            Some((1u16, r))
        } else if let Some((num_w, after_num)) = mid.split_once(char::is_whitespace)
            && let Some(n) = parse_small_u16(num_w)
            && let Some(r) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((n, r))
        } else {
            None
        };
        if let Some((min_n, f_raw)) = parsed {
            let cp = ctx.interner.checkpoint();
            if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && object_filter_problem(&f).is_none()
            {
                return Some(Condition::SacrificedFilterThisTurn {
                    filter: Box::new(f),
                    min: min_n,
                    who,
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    if low == "if no creatures attacked this turn" {
        return Some(Condition::NoCreaturesAttackedThisTurn);
    }
    if low == "if you've cast both a creature spell and a noncreature spell this turn" {
        return Some(Condition::CastCreatureAndNoncreatureThisTurn);
    }
    if let Some(num_w) =
        strip_prefix_word_ci(&low, "if enchanted creature has power").and_then(|r| {
            strip_suffix_ws_ci(r, "or greater").or_else(|| strip_suffix_ws_ci(r, "or more"))
        })
        && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::EnchantedCreaturePowerAtLeast { min });
    }
    if let Some(sub_raw) = strip_prefix_word_ci(t, "if every land on the battlefield is a")
        .or_else(|| strip_prefix_word_ci(t, "if every land on the battlefield is an"))
        .or_else(|| strip_prefix_word_ci(t, "if all lands are"))
        && is_bare_capitalized_word(sub_raw)
        && let Some(sub) = singular_subtype(sub_raw)
    {
        let id = ctx.interner.intern_subtype(&sub);
        return Some(Condition::AllLandsAreSubtype { subtype: id });
    }
    if let Some(after_ctrl) = strip_prefix_word_ci(t, "if you control the")
        && let Some((f_raw, _)) = split_once_ws_word_ci(after_ctrl, "with the greatest mana value")
            .or_else(|| {
                strip_suffix_ws_ci(after_ctrl, "with the greatest mana value").map(|l| (l, ""))
            })
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::YouControlGreatestManaValueFilter {
                filter: Box::new(f),
            });
        }
        ctx.interner.rollback(cp);
    }
    if low == "if your team gained life this turn" {
        return Some(Condition::YourTeamGainedLifeThisTurn);
    }
    if let Some(f_raw) = strip_prefix_word_ci(t, "if your team controls another") {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::YourTeamControlsOtherFilter {
                filter: Box::new(f),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if")
        .and_then(|r| strip_suffix_ws_ci(r, "or more players have lost the game"))
        && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::PlayersLostGameAtLeast { min });
    }
    if low == "if ~ attacked this combat" {
        return Some(Condition::SelfAttackedThisCombat);
    }
    if low == "if ~ didn't attack this turn" || low == "if ~ did not attack this turn" {
        return Some(Condition::SelfDidntAttackThisTurn);
    }
    if matches!(
        low.as_str(),
        "if this ability hasn't added mana this turn"
            | "if ~ hasn't added mana this turn"
            | "if it hasn't been triggered to add mana this turn"
    ) {
        return Some(Condition::SelfHaventAddedManaThisTurn);
    }
    if let Some(after_if) = strip_prefix_word_ci(t, "if")
        && let Some(mid) = strip_suffix_ws_ci(
            after_if,
            "was put into a graveyard from the battlefield this turn",
        )
        .or_else(|| {
            strip_suffix_ws_ci(
                after_if,
                "were put into a graveyard from the battlefield this turn",
            )
        })
    {
        let parsed = if let Some(r) =
            strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
        {
            Some((1u16, r))
        } else if let Some((num_w, after_num)) = mid.split_once(char::is_whitespace)
            && let Some(n) = parse_small_u16(num_w)
            && let Some(r) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((n, r))
        } else {
            None
        };
        if let Some((min_n, f_raw)) = parsed {
            let cp = ctx.interner.checkpoint();
            if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && object_filter_problem(&f).is_none()
            {
                return Some(Condition::ZoneChangeFromToThisTurn {
                    from: Some(Zone::Battlefield),
                    to: Zone::Graveyard,
                    filter: Box::new(f),
                    min: min_n,
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    if let Some(after_if) = strip_prefix_word_ci(t, "if")
        && let Some(mid) = strip_suffix_ws_ci(
            after_if,
            "entered the battlefield under your control this turn",
        )
        .or_else(|| strip_suffix_ws_ci(after_if, "entered under your control this turn"))
    {
        let parsed = if let Some(r) =
            strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
        {
            Some((1u16, r))
        } else if let Some((num_w, after_num)) = mid.split_once(char::is_whitespace)
            && let Some(n) = parse_small_u16(num_w)
            && let Some(r) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((n, r))
        } else {
            None
        };
        if let Some((min_n, f_raw)) = parsed {
            let cp = ctx.interner.checkpoint();
            if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && object_filter_problem(&f).is_none()
            {
                return Some(Condition::EnteredUnderYourControlThisTurn {
                    filter: Box::new(f),
                    min: Some(min_n),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    if low == "if you have less life than your starting life total" {
        return Some(Condition::YourLifeLessThanStarting);
    }

    parse_trigger_condition_wave9(t, ctx)
}

fn parse_trigger_condition_core(
    t: &str,
    allow_dungeon_completion: bool,
    ctx: &mut ParseCtx<'_>,
) -> Option<Condition> {
    let low = t.to_ascii_lowercase();

    // Kicker / Escape / Evoke / Foretell / Saddle / Monstrous / Blitz / Exert / Commander
    if low == "if it was kicked" || low == "if ~ was kicked" {
        return Some(Condition::Kicked {
            tag: None,
            min_count: None,
        });
    }
    if matches!(
        low.as_str(),
        "if it wasn't kicked"
            | "if it was not kicked"
            | "if ~ wasn't kicked"
            | "if ~ was not kicked"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Kicked {
                tag: None,
                min_count: None,
            }),
        });
    }
    if low == "if it was kicked twice" || low == "if ~ was kicked twice" {
        return Some(Condition::Kicked {
            tag: None,
            min_count: Some(2),
        });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if it was kicked")
        .or_else(|| strip_prefix_word_ci(&low, "if ~ was kicked"))
        .and_then(|r| strip_suffix_ws_ci(r, "times").or_else(|| strip_suffix_ws_ci(r, "time")))
    {
        let min_count = if num_w == "once" {
            Some(1u16)
        } else {
            parse_small_u16(num_w)
        };
        if let Some(mc) = min_count
            && mc >= 1
        {
            return Some(Condition::Kicked {
                tag: None,
                min_count: Some(mc),
            });
        }
    }
    if let Some(tag_raw) = strip_prefix_word_ci(t, "if it was kicked with its")
        .or_else(|| strip_prefix_word_ci(t, "if ~ was kicked with its"))
        .and_then(|r| strip_suffix_ws_ci(r, "kicker"))
    {
        return Some(Condition::Kicked {
            tag: Some(canonical_kicker_tag(tag_raw)),
            min_count: None,
        });
    }

    if low == "if it escaped" || low == "if ~ escaped" {
        return Some(Condition::Escaped);
    }
    if matches!(
        low.as_str(),
        "if it didn't escape"
            | "if it did not escape"
            | "if ~ didn't escape"
            | "if ~ did not escape"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Escaped),
        });
    }
    if matches!(
        low.as_str(),
        "if its evoke cost was paid" | "if it was evoked" | "if ~ was evoked"
    ) {
        return Some(Condition::Evoked);
    }
    if matches!(
        low.as_str(),
        "if its evoke cost wasn't paid"
            | "if its evoke cost was not paid"
            | "if it wasn't evoked"
            | "if it was not evoked"
            | "if ~ wasn't evoked"
            | "if ~ was not evoked"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Evoked),
        });
    }
    if low == "if it was foretold" || low == "if ~ was foretold" {
        return Some(Condition::Foretold);
    }
    if matches!(
        low.as_str(),
        "if it wasn't foretold"
            | "if it was not foretold"
            | "if ~ wasn't foretold"
            | "if ~ was not foretold"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Foretold),
        });
    }
    if matches!(
        low.as_str(),
        "if ~ is saddled" | "if it is saddled" | "if it's saddled"
    ) {
        return Some(Condition::Saddled);
    }
    if matches!(
        low.as_str(),
        "if ~ isn't saddled"
            | "if ~ is not saddled"
            | "if it isn't saddled"
            | "if it is not saddled"
            | "if it's not saddled"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Saddled),
        });
    }
    if matches!(
        low.as_str(),
        "if ~ is monstrous"
            | "if ~ was monstrous"
            | "if it is monstrous"
            | "if it was monstrous"
            | "if it's monstrous"
    ) {
        return Some(Condition::Monstrous);
    }
    if matches!(
        low.as_str(),
        "if ~ isn't monstrous"
            | "if ~ is not monstrous"
            | "if ~ wasn't monstrous"
            | "if ~ was not monstrous"
            | "if it isn't monstrous"
            | "if it is not monstrous"
            | "if it wasn't monstrous"
            | "if it was not monstrous"
            | "if it's not monstrous"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Monstrous),
        });
    }
    if matches!(
        low.as_str(),
        "if its blitz cost was paid" | "if it was blitzed" | "if ~ was blitzed"
    ) {
        return Some(Condition::Blitzed);
    }
    if matches!(
        low.as_str(),
        "if its blitz cost wasn't paid"
            | "if its blitz cost was not paid"
            | "if it wasn't blitzed"
            | "if it was not blitzed"
            | "if ~ wasn't blitzed"
            | "if ~ was not blitzed"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::Blitzed),
        });
    }

    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        let base_exerted = strip_suffix_ws_ci(after_if, "this turn").unwrap_or(after_if);
        for subj in ["~", "it", "he", "she", "they"] {
            if let Some(after_s) = strip_prefix_word_ci(base_exerted, subj) {
                if matches!(
                    after_s,
                    "is exerted"
                        | "are exerted"
                        | "was exerted"
                        | "were exerted"
                        | "has been exerted"
                        | "have been exerted"
                ) {
                    return Some(Condition::ExertedThisTurn);
                }
                if matches!(
                    after_s,
                    "isn't exerted"
                        | "is not exerted"
                        | "aren't exerted"
                        | "are not exerted"
                        | "wasn't exerted"
                        | "was not exerted"
                        | "weren't exerted"
                        | "were not exerted"
                        | "hasn't been exerted"
                        | "has not been exerted"
                        | "haven't been exerted"
                        | "have not been exerted"
                ) {
                    return Some(Condition::Not {
                        of: Box::new(Condition::ExertedThisTurn),
                    });
                }
            }
        }
        if base_exerted == "it's not exerted" {
            return Some(Condition::Not {
                of: Box::new(Condition::ExertedThisTurn),
            });
        }
    }

    if low == "if you control a commander" || low == "if you control your commander" {
        return Some(Condition::ControlCommander);
    }

    if let Some(num_w) =
        strip_prefix_word_ci(t, "if you have").and_then(|r| strip_suffix_ws_ci(r, "or more life"))
        && let Some(n) = parse_small_i32(num_w)
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::Life { of: PlayerRef::You }),
            op: CompareOp::Ge,
            right: const_amount(n),
        });
    }
    if let Some(after_ctrl) = strip_prefix_word_ci(t, "if you control")
        && let Some((num_w, after_num)) = after_ctrl.split_once(char::is_whitespace)
        && let Some(n) = parse_small_i32(num_w)
        && let Some(f_raw) = strip_prefix_word_ci(after_num.trim_start(), "or more")
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
        {
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: const_amount(n),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_ctrl) = strip_prefix_word_ci(t, "if you control") {
        // Check "a X or there is a Y card in your graveyard" before generic "a X"
        if let Some((bf_raw, gy_raw)) =
            split_once_ws_word_ci(after_ctrl, "or there is").and_then(|(l, r)| {
                strip_prefix_word_ci(l, "a")
                    .or_else(|| strip_prefix_word_ci(l, "an"))
                    .and_then(|bf| {
                        strip_prefix_word_ci(r, "a")
                            .or_else(|| strip_prefix_word_ci(r, "an"))
                            .and_then(|g| strip_suffix_ws_ci(g, "card in your graveyard"))
                            .map(|gy| (bf, gy))
                    })
            })
        {
            let cp = ctx.interner.checkpoint();
            if let (Some(mut bf_f), Some(mut gy_f)) = (
                parse_trigger_object_phrase(bf_raw, ctx, false),
                parse_trigger_object_phrase(&format!("{gy_raw} card"), ctx, false),
            ) && bf_f.controller.is_none()
            {
                bf_f.controller = Some(RelativeControl::You);
                gy_f.zones = ZoneMask::GRAVEYARD;
                gy_f.controller = Some(RelativeControl::You);
                return Some(Condition::Or {
                    of: vec![
                        Condition::Compare {
                            left: Box::new(Amount::Count { of: Box::new(bf_f) }),
                            op: CompareOp::Ge,
                            right: const_amount(1),
                        },
                        Condition::Compare {
                            left: Box::new(Amount::Count { of: Box::new(gy_f) }),
                            op: CompareOp::Ge,
                            right: const_amount(1),
                        },
                    ],
                });
            }
            ctx.interner.rollback(cp);
        }

        // Check "a X and a Y" before generic "a X"
        for conj in ["and a", "and an"] {
            if let Some((f1_raw, f2_raw)) = split_once_ws_word_ci(after_ctrl, conj)
                && let Some(f1_inner) =
                    strip_prefix_word_ci(f1_raw, "a").or_else(|| strip_prefix_word_ci(f1_raw, "an"))
            {
                let cp = ctx.interner.checkpoint();
                if let (Some(mut f1), Some(mut f2)) = (
                    parse_trigger_object_phrase(f1_inner, ctx, false),
                    parse_trigger_object_phrase(f2_raw, ctx, false),
                ) && f1.controller.is_none()
                    && f2.controller.is_none()
                    && object_filter_problem(&f1).is_none()
                    && object_filter_problem(&f2).is_none()
                {
                    f1.controller = Some(RelativeControl::You);
                    f2.controller = Some(RelativeControl::You);
                    return Some(Condition::And {
                        of: vec![
                            Condition::Compare {
                                left: Box::new(Amount::Count { of: Box::new(f1) }),
                                op: CompareOp::Ge,
                                right: const_amount(1),
                            },
                            Condition::Compare {
                                left: Box::new(Amount::Count { of: Box::new(f2) }),
                                op: CompareOp::Ge,
                                right: const_amount(1),
                            },
                        ],
                    });
                }
                ctx.interner.rollback(cp);
            }
        }

        if let Some(f_raw) = strip_prefix_word_ci(after_ctrl, "a")
            .or_else(|| strip_prefix_word_ci(after_ctrl, "an"))
            .or_else(|| starts_with_word_ci(after_ctrl, "another").then_some(after_ctrl))
        {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && f.controller.is_none()
            {
                if f.zones.is_empty() || f.zones == ZoneMask::BATTLEFIELD {
                    f.zones = ZoneMask::BATTLEFIELD;
                    f.controller = Some(RelativeControl::You);
                    return Some(Condition::ControlsFilter {
                        filter: Box::new(f),
                    });
                }
                f.controller = Some(RelativeControl::You);
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: const_amount(1),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    if let Some(f_raw) = strip_prefix_word_ci(t, "if you control no") {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
        {
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Eq,
                right: const_amount(0),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(num_w) = strip_prefix_word_ci(t, "if ~ is in your graveyard with")
        .and_then(|r| strip_suffix_ws_ci(r, "or more creature cards above it"))
        && let Some(count) = parse_small_u16(num_w)
    {
        return Some(Condition::GraveyardAbove {
            filter: Box::new(ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                ..Default::default()
            }),
            count,
        });
    }
    if t == "if you have no cards in hand" {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        });
    }
    if t == "if it's your turn" {
        return Some(Condition::IsTurnOf {
            player: PlayerRef::You,
        });
    }
    if t == "if it's not your turn" {
        return Some(Condition::Not {
            of: Box::new(Condition::IsTurnOf {
                player: PlayerRef::You,
            }),
        });
    }
    if low == "if you're the monarch" || low == "if you are the monarch" {
        return Some(Condition::IsMonarch {
            player: Some(PlayerRef::You),
            relation: None,
        });
    }
    if matches!(
        low.as_str(),
        "if you're not the monarch" | "if you aren't the monarch" | "if you are not the monarch"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::IsMonarch {
                player: Some(PlayerRef::You),
                relation: None,
            }),
        });
    }
    if low == "if an opponent is the monarch" {
        return Some(Condition::IsMonarch {
            player: None,
            relation: Some(RelativeControl::Opponent),
        });
    }
    if low == "if a player is the monarch" {
        return Some(Condition::IsMonarch {
            player: None,
            relation: Some(RelativeControl::Any),
        });
    }
    if low == "if there is no monarch" {
        return Some(Condition::Not {
            of: Box::new(Condition::IsMonarch {
                player: None,
                relation: Some(RelativeControl::Any),
            }),
        });
    }

    if low == "if a creature died this turn" {
        return Some(Condition::CreatureDiedThisTurn);
    }
    if matches!(
        low.as_str(),
        "if no creatures died this turn"
            | "if no creature died this turn"
            | "if a creature didn't die this turn"
            | "if a creature did not die this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::CreatureDiedThisTurn),
        });
    }
    if low == "if you attacked this turn" || low == "if you attacked with a creature this turn" {
        return Some(Condition::AttackedThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you didn't attack this turn"
            | "if you did not attack this turn"
            | "if you didn't attack with a creature this turn"
            | "if you did not attack with a creature this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::AttackedThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if a permanent you controlled left the battlefield this turn"
            | "if a permanent left the battlefield under your control this turn"
    ) {
        return Some(Condition::PermanentLeftBattlefieldThisTurn);
    }
    if matches!(
        low.as_str(),
        "if no permanents you controlled left the battlefield this turn"
            | "if no permanent you controlled left the battlefield this turn"
            | "if no permanents left the battlefield under your control this turn"
            | "if no permanent left the battlefield under your control this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::PermanentLeftBattlefieldThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if you gained life this turn"
            | "if you've gained life this turn"
            | "if you have gained life this turn"
    ) {
        return Some(Condition::GainedLifeThisTurn { min: None });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if you've gained")
        .or_else(|| strip_prefix_word_ci(&low, "if you have gained"))
        .or_else(|| strip_prefix_word_ci(&low, "if you gained"))
        .and_then(|r| strip_suffix_ws_ci(r, "or more life this turn"))
        && let Some(n) = parse_small_i32(num_w)
    {
        return Some(Condition::GainedLifeThisTurn {
            min: Some(const_amount(n)),
        });
    }
    if matches!(
        low.as_str(),
        "if you didn't gain life this turn"
            | "if you did not gain life this turn"
            | "if you haven't gained life this turn"
            | "if you have not gained life this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::GainedLifeThisTurn { min: None }),
        });
    }
    if low == "if you cast it from your hand" || low == "if you cast ~ from your hand" {
        return Some(Condition::WasCast { from_hand: true });
    }
    if matches!(
        low.as_str(),
        "if you didn't cast it from your hand"
            | "if you did not cast it from your hand"
            | "if you didn't cast ~ from your hand"
            | "if you did not cast ~ from your hand"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::WasCast { from_hand: true }),
        });
    }
    if matches!(
        low.as_str(),
        "if you cast it" | "if you cast ~" | "if it was cast" | "if ~ was cast"
    ) {
        return Some(Condition::WasCast { from_hand: false });
    }
    if matches!(
        low.as_str(),
        "if you didn't cast it"
            | "if you did not cast it"
            | "if you didn't cast ~"
            | "if you did not cast ~"
            | "if it wasn't cast"
            | "if it was not cast"
            | "if ~ wasn't cast"
            | "if ~ was not cast"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::WasCast { from_hand: false }),
        });
    }

    if low == "if an opponent lost life this turn" {
        return Some(Condition::OpponentLostLifeThisTurn);
    }
    if matches!(
        low.as_str(),
        "if no opponents lost life this turn" | "if no opponent lost life this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::OpponentLostLifeThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if you descended this turn"
            | "if you've descended this turn"
            | "if you have descended this turn"
    ) {
        return Some(Condition::DescendedThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you didn't descend this turn"
            | "if you did not descend this turn"
            | "if you haven't descended this turn"
            | "if you have not descended this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::DescendedThisTurn),
        });
    }

    // "if (it|~|that creature|that permanent) had [(a|an|one or more) ][<ctr> ]counters? on it"
    for subj in ["if it", "if ~", "if that creature", "if that permanent"] {
        if let Some(after_s) = strip_prefix_word_ci(t, subj) {
            if let Some(after_had) = strip_prefix_word_ci(after_s, "had")
                && let Some(ctr_mid) = strip_suffix_ws_ci(after_had, "counters on it")
                    .or_else(|| strip_suffix_ws_ci(after_had, "counter on it"))
                    .or_else(|| {
                        (after_had.eq_ignore_ascii_case("counters on it")
                            || after_had.eq_ignore_ascii_case("counter on it"))
                        .then_some("")
                    })
            {
                let after_qty = strip_prefix_word_ci(ctr_mid, "one or more")
                    .or_else(|| strip_prefix_word_ci(ctr_mid, "a"))
                    .or_else(|| strip_prefix_word_ci(ctr_mid, "an"))
                    .unwrap_or(ctr_mid);
                if after_qty.is_empty()
                    || after_qty.eq_ignore_ascii_case("a")
                    || after_qty.eq_ignore_ascii_case("an")
                    || after_qty.eq_ignore_ascii_case("one or more")
                {
                    return Some(Condition::HadCounterOnIt {
                        counter: CounterTarget::Any,
                    });
                }
                let cp = ctx.interner.checkpoint();
                if let Some(id) = parse_trigger_counter(after_qty, ctx.interner) {
                    return Some(Condition::HadCounterOnIt {
                        counter: CounterTarget::Specific(id),
                    });
                }
                ctx.interner.rollback(cp);
            }
            if let Some(after_neg) = [
                "had no",
                "didn't have a",
                "didn't have an",
                "didn't have any",
                "did not have a",
                "did not have an",
                "did not have any",
            ]
            .iter()
            .find_map(|p| strip_prefix_word_ci(after_s, p))
                && let Some(ctr_mid) = strip_suffix_ws_ci(after_neg, "counters on it")
                    .or_else(|| strip_suffix_ws_ci(after_neg, "counter on it"))
                    .or_else(|| {
                        (after_neg.eq_ignore_ascii_case("counters on it")
                            || after_neg.eq_ignore_ascii_case("counter on it"))
                        .then_some("")
                    })
            {
                if ctr_mid.is_empty() {
                    return Some(Condition::Not {
                        of: Box::new(Condition::HadCounterOnIt {
                            counter: CounterTarget::Any,
                        }),
                    });
                }
                let cp = ctx.interner.checkpoint();
                if let Some(id) = parse_trigger_counter(ctr_mid, ctx.interner) {
                    return Some(Condition::Not {
                        of: Box::new(Condition::HadCounterOnIt {
                            counter: CounterTarget::Specific(id),
                        }),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // Mana color spent to cast
    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        if let Some(mana_str) = strip_suffix_ws_ci(after_if, "was spent to cast it")
            .or_else(|| strip_suffix_ws_ci(after_if, "was spent to cast ~"))
        {
            if let Some(c) = parse_braced_wubrg(mana_str) {
                return Some(Condition::ManaColorSpentToCast {
                    color: Some(c),
                    min_total: None,
                });
            }
            if let Some((c1s, c2s)) = split_once_ws_word_ci(mana_str, "or")
                && let (Some(c1), Some(c2)) = (parse_braced_wubrg(c1s), parse_braced_wubrg(c2s))
            {
                return Some(Condition::Or {
                    of: vec![
                        Condition::ManaColorSpentToCast {
                            color: Some(c1),
                            min_total: None,
                        },
                        Condition::ManaColorSpentToCast {
                            color: Some(c2),
                            min_total: None,
                        },
                    ],
                });
            }
            if mana_str.len() == 6
                && mana_str.as_bytes()[0] == b'{'
                && mana_str.as_bytes()[2] == b'}'
                && mana_str.as_bytes()[3] == b'{'
                && mana_str.as_bytes()[5] == b'}'
                && mana_str.as_bytes()[1].eq_ignore_ascii_case(&mana_str.as_bytes()[4])
                && let Some(c) = parse_braced_wubrg(&mana_str[..3])
            {
                return Some(Condition::SameColorManaSpentAtLeast {
                    min: 2,
                    color: Some(c),
                });
            }
            if mana_str.len() == 9
                && mana_str.as_bytes()[0] == b'{'
                && mana_str.as_bytes()[2] == b'}'
                && mana_str.as_bytes()[3] == b'{'
                && mana_str.as_bytes()[5] == b'}'
                && mana_str.as_bytes()[6] == b'{'
                && mana_str.as_bytes()[8] == b'}'
                && mana_str.as_bytes()[1].eq_ignore_ascii_case(&mana_str.as_bytes()[4])
                && mana_str.as_bytes()[1].eq_ignore_ascii_case(&mana_str.as_bytes()[7])
                && let Some(c) = parse_braced_wubrg(&mana_str[..3])
            {
                return Some(Condition::SameColorManaSpentAtLeast {
                    min: 3,
                    color: Some(c),
                });
            }
        }
        if let Some(mana_str) = strip_suffix_ws_ci(after_if, "were spent to cast it")
            .or_else(|| strip_suffix_ws_ci(after_if, "were spent to cast ~"))
        {
            let stripped_both = strip_prefix_word_ci(mana_str, "both").unwrap_or(mana_str);
            if let Some((c1s, c2s)) = split_once_ws_word_ci(stripped_both, "and")
                && let (Some(c1), Some(c2)) = (parse_braced_wubrg(c1s), parse_braced_wubrg(c2s))
            {
                return Some(Condition::And {
                    of: vec![
                        Condition::ManaColorSpentToCast {
                            color: Some(c1),
                            min_total: None,
                        },
                        Condition::ManaColorSpentToCast {
                            color: Some(c2),
                            min_total: None,
                        },
                    ],
                });
            }
        }
    }

    if low == "if an opponent controls more lands than you" {
        return Some(Condition::OpponentComparison {
            metric: OpponentComparisonMetric::Lands,
        });
    }
    if low == "if an opponent controls more creatures than you" {
        return Some(Condition::OpponentComparison {
            metric: OpponentComparisonMetric::Creatures,
        });
    }
    if low == "if an opponent has more life than you" {
        return Some(Condition::OpponentComparison {
            metric: OpponentComparisonMetric::Life,
        });
    }
    if low == "if an opponent has more cards in hand than you" {
        return Some(Condition::OpponentComparison {
            metric: OpponentComparisonMetric::Hand,
        });
    }

    if let Some(after_if) = strip_prefix_word_ci(&low, "if") {
        let after_lead = strip_prefix_word_ci(after_if, "there are")
            .or_else(|| strip_prefix_word_ci(after_if, "you have"))
            .unwrap_or(after_if);
        if let Some((num_w, after_num)) = after_lead.split_once(char::is_whitespace)
            && let Some(n) = parse_small_i32(num_w)
            && let Some(after_om) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            if matches!(
                after_om,
                "cards in your graveyard" | "cards are in your graveyard"
            ) {
                return Some(Condition::Compare {
                    left: Box::new(Amount::CardsIn {
                        zone: Zone::Graveyard,
                        of: PlayerRef::You,
                    }),
                    op: CompareOp::Ge,
                    right: const_amount(n),
                });
            }
            if matches!(
                after_om,
                "card types among cards in your graveyard"
                    | "card types are among cards in your graveyard"
            ) {
                return Some(Condition::Compare {
                    left: Box::new(Amount::DistinctCardTypesInZone {
                        zone: Zone::Graveyard,
                        whose: ZoneScopeWhose::You,
                    }),
                    op: CompareOp::Ge,
                    right: const_amount(n),
                });
            }
        }
        // Typed graveyard card count (case-preserving via t)
        let after_if_orig = strip_prefix_word_ci(t, "if")?;
        let after_lead_orig = strip_prefix_word_ci(after_if_orig, "there are")
            .or_else(|| strip_prefix_word_ci(after_if_orig, "you have"))
            .unwrap_or(after_if_orig);
        if let Some((num_w, after_num)) = after_lead_orig.split_once(char::is_whitespace)
            && let Some(n) = parse_small_i32(num_w)
            && let Some(after_om) = strip_prefix_word_ci(after_num.trim_start(), "or more")
            && let Some(qual) = strip_suffix_ws_ci(after_om, "cards are in your graveyard")
                .or_else(|| strip_suffix_ws_ci(after_om, "card are in your graveyard"))
                .or_else(|| strip_suffix_ws_ci(after_om, "cards in your graveyard"))
                .or_else(|| strip_suffix_ws_ci(after_om, "card in your graveyard"))
        {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) = parse_graveyard_leave_card_filter(Some(qual), ctx) {
                f.zones = ZoneMask::GRAVEYARD;
                f.controller = Some(RelativeControl::You);
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: const_amount(n),
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    if low == "if ~ is in your graveyard" {
        return Some(Condition::SelfInZone {
            zone: Zone::Graveyard,
        });
    }
    if low == "if ~ is in exile" {
        return Some(Condition::SelfInZone { zone: Zone::Exile });
    }
    if low == "if ~ is on the battlefield" {
        return Some(Condition::SelfInZone {
            zone: Zone::Battlefield,
        });
    }
    if low == "if no spells were cast last turn" {
        return Some(Condition::SpellsCastLastTurn {
            mode: SpellsLastTurnMode::None,
            count: 0,
        });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if a player cast")
        .and_then(|r| strip_suffix_ws_ci(r, "or more spells last turn"))
        && let Some(count) = parse_small_u16(num_w)
    {
        return Some(Condition::SpellsCastLastTurn {
            mode: SpellsLastTurnMode::SinglePlayerAtLeast,
            count,
        });
    }
    if allow_dungeon_completion {
        if low == "if you've completed a dungeon" || low == "if you have completed a dungeon" {
            return Some(Condition::HasCompletedDungeon);
        }
        if low == "if you haven't completed a dungeon"
            || low == "if you have not completed a dungeon"
        {
            return Some(Condition::Not {
                of: Box::new(Condition::HasCompletedDungeon),
            });
        }
    }
    if low == "if you have the initiative" {
        return Some(Condition::HasInitiative);
    }
    if low == "if you have the city's blessing" {
        return Some(Condition::HasCitysBlessing);
    }
    if low == "if you don't have the city's blessing"
        || low == "if you do not have the city's blessing"
    {
        return Some(Condition::Not {
            of: Box::new(Condition::HasCitysBlessing),
        });
    }
    if low == "if you have max speed" {
        return Some(Condition::HasMaxSpeed);
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if an opponent has")
        .and_then(|r| strip_suffix_ws_ci(r, "or more poison counters"))
        && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::OpponentPoisonAtLeast { min });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if").and_then(|r| {
        strip_suffix_ws_ci(
            r,
            "or more nonland permanents entered under your control this turn",
        )
        .or_else(|| {
            strip_suffix_ws_ci(
                r,
                "or more nonland permanents entered the battlefield under your control this turn",
            )
        })
    }) && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::NonlandEnteredThisTurn { min });
    }
    if low
        == "if a nonland permanent left the battlefield this turn or a spell was warped this turn"
    {
        return Some(Condition::VoidThisTurn);
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if at least").and_then(|r| {
        strip_suffix_ws_ci(r, "mana of the same color was spent to cast it")
            .or_else(|| strip_suffix_ws_ci(r, "mana of the same color was spent to cast ~"))
            .or_else(|| {
                strip_suffix_ws_ci(r, "mana of the same color was spent to cast this spell")
            })
    }) && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::SameColorManaSpentAtLeast { min, color: None });
    }
    if let Some(mid) = strip_prefix_word_ci(&low, "if at least").and_then(|r| {
        strip_suffix_ws_ci(r, "mana was spent to cast it")
            .or_else(|| strip_suffix_ws_ci(r, "mana was spent to cast ~"))
            .or_else(|| strip_suffix_ws_ci(r, "mana was spent to cast this spell"))
    }) {
        if let Some((num_w, col_w)) = mid.split_once(char::is_whitespace)
            && let Some(min) = parse_small_u16(num_w)
            && let Some(c) = parse_color_word_to_mana(col_w).or_else(|| {
                col_w
                    .eq_ignore_ascii_case("colorless")
                    .then_some(mtg_ir::ManaColor::C)
            })
        {
            return Some(Condition::SameColorManaSpentAtLeast {
                min,
                color: Some(c),
            });
        }
        if let Some(min_total) = parse_small_u16(mid) {
            return Some(Condition::ManaColorSpentToCast {
                color: None,
                min_total: Some(min_total),
            });
        }
    }
    if matches!(
        low.as_str(),
        "if no mana was spent to cast it"
            | "if no mana was spent to cast ~"
            | "if no mana was spent to cast this spell"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::ManaColorSpentToCast {
                color: None,
                min_total: Some(1),
            }),
        });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if you control")
        .and_then(|r| strip_suffix_ws_ci(r, "or more creatures with different powers"))
        && let Some(count) = parse_small_u16(num_w)
    {
        return Some(Condition::DistinctControlledCreaturePowers { count });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if creatures you control have total power")
        .and_then(|r| {
            strip_suffix_ws_ci(r, "or greater").or_else(|| strip_suffix_ws_ci(r, "or more"))
        })
        && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::TotalPowerControlled { min });
    }
    if let Some(after_cast) = strip_prefix_word_ci(t, "if you've cast")
        .or_else(|| strip_prefix_word_ci(t, "if you have cast"))
    {
        if let Some(num_w) = strip_suffix_ws_ci(after_cast, "or more spells this turn")
            && let Some(min) = parse_small_u16(num_w)
        {
            return Some(Condition::SpellsCastThisTurnAtLeast { min, filter: None });
        }
        if let Some(mid) = strip_suffix_ws_ci(after_cast, "spells this turn")
            .or_else(|| strip_suffix_ws_ci(after_cast, "spell this turn"))
        {
            let parsed = if let Some(r) =
                strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
            {
                Some((1u16, r))
            } else if let Some((num_w, after_num)) = mid.split_once(char::is_whitespace)
                && let Some(n) = parse_small_u16(num_w)
                && let Some(r) = strip_prefix_word_ci(after_num.trim_start(), "or more")
            {
                Some((n, r))
            } else {
                None
            };
            if let Some((min, qual)) = parsed {
                let cp = ctx.interner.checkpoint();
                if let Some(TriggerSubject::Filter { filter }) =
                    spell_filter(&format!("a {qual} spell"), ctx)
                {
                    return Some(Condition::SpellsCastThisTurnAtLeast {
                        min,
                        filter: Some(filter),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if").and_then(|r| {
        strip_suffix_ws_ci(
            r,
            "or more cards were put into your graveyard from anywhere this turn",
        )
    }) && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::CardsPutIntoGraveyardFromAnywhereThisTurn {
            min,
            filter: None,
            whose: None,
        });
    }
    if low == "if ~ attacked this turn" {
        return Some(Condition::BoastAttackedThisTurn);
    }
    if let Some(after_stat) = strip_prefix_word_ci(&low, "if ~'s power is")
        .map(|r| (StatKind::Power, r))
        .or_else(|| {
            strip_prefix_word_ci(&low, "if ~'s toughness is").map(|r| (StatKind::Toughness, r))
        })
        && let Some((num_w, dir_w)) = after_stat.1.split_once(" or ")
        && let Some(n) = parse_small_i32(num_w)
    {
        let op = match dir_w.trim() {
            "greater" | "more" => Some(CompareOp::Ge),
            "less" | "fewer" => Some(CompareOp::Le),
            _ => None,
        };
        if let Some(op) = op {
            return Some(Condition::Compare {
                left: Box::new(Amount::Stat {
                    of: mtg_ir::Ref::SelfObj,
                    stat: after_stat.0,
                    lki: false,
                }),
                op,
                right: const_amount(n),
            });
        }
    }

    // Self state conditions: "if (~|it) (is|isn't|is not) <state>" / "if it's [not] <state>"
    let self_state_parsed = [
        ("if it's not ", true),
        ("if it's ", false),
        ("if ~ isn't ", true),
        ("if ~ is not ", true),
        ("if ~ is ", false),
        ("if it isn't ", true),
        ("if it is not ", true),
        ("if it is ", false),
    ]
    .iter()
    .find_map(|(pref, neg)| low.strip_prefix(pref).map(|st| (*neg, st.trim())));
    if let Some((neg, st)) = self_state_parsed {
        let obj_state = |tapped, attacking, blocking, monstrous, saddled, equipped, enchanted| {
            Condition::ObjectState {
                of: AttachmentOwnerTarget::SelfObj,
                tapped,
                attacking,
                blocking,
                monstrous,
                saddled,
                equipped,
                enchanted,
                filter: None,
            }
        };
        let base_cond = match st {
            "tapped" => Some(obj_state(Some(true), None, None, None, None, None, None)),
            "untapped" => Some(obj_state(Some(false), None, None, None, None, None, None)),
            "attacking" => Some(obj_state(None, Some(true), None, None, None, None, None)),
            "blocking" => Some(obj_state(None, None, Some(true), None, None, None, None)),
            "enchanted" => Some(obj_state(None, None, None, None, None, None, Some(true))),
            "equipped" => Some(obj_state(None, None, None, None, None, Some(true), None)),
            "saddled" => Some(obj_state(None, None, None, None, Some(true), None, None)),
            "monstrous" => Some(obj_state(None, None, None, Some(true), None, None, None)),
            "modified" => Some(Condition::MatchesFilter {
                target: Some(mtg_ir::Ref::SelfObj),
                filter: Box::new(ObjectFilter {
                    flags: FilterFlags::MODIFIED,
                    ..Default::default()
                }),
                any: false,
            }),
            "renowned" => Some(Condition::IsRenowned),
            "harnessed" => Some(Condition::IsHarnessed),
            _ => None,
        };
        if let Some(bc) = base_cond {
            return Some(if neg {
                Condition::Not { of: Box::new(bc) }
            } else {
                bc
            });
        }
    }

    if low == "if ~ is paired with another creature"
        || low == "if ~ is paired with a creature"
        || low == "if it is paired with another creature"
        || low == "if it is paired with a creature"
    {
        return Some(Condition::IsPaired);
    }
    if matches!(
        low.as_str(),
        "if ~ isn't paired with another creature"
            | "if ~ is not paired with another creature"
            | "if ~ isn't paired with a creature"
            | "if ~ is not paired with a creature"
            | "if it isn't paired with another creature"
            | "if it is not paired with another creature"
            | "if it isn't paired with a creature"
            | "if it is not paired with a creature"
            | "if it's not paired with another creature"
            | "if it's not paired with a creature"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::IsPaired),
        });
    }
    if low == "if tribute was paid" {
        return Some(Condition::TributePaid);
    }
    if low == "if tribute wasn't paid" || low == "if tribute was not paid" {
        return Some(Condition::Not {
            of: Box::new(Condition::TributePaid),
        });
    }
    if low == "if ~ has an enduring story" || low == "if it has an enduring story" {
        return Some(Condition::HasEnduringStory);
    }
    if matches!(
        low.as_str(),
        "if ~ doesn't have an enduring story"
            | "if ~ does not have an enduring story"
            | "if it doesn't have an enduring story"
            | "if it does not have an enduring story"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::HasEnduringStory),
        });
    }
    if matches!(
        low.as_str(),
        "if ~ wasn't renowned"
            | "if ~ was not renowned"
            | "if it wasn't renowned"
            | "if it was not renowned"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::IsRenowned),
        });
    }
    if low == "if ~ was renowned" || low == "if it was renowned" {
        return Some(Condition::IsRenowned);
    }
    if low == "if it's day" || low == "if it is day" {
        return Some(Condition::IsDay);
    }
    if low == "if it's night" || low == "if it is night" {
        return Some(Condition::IsNight);
    }
    if low == "if it's neither day nor night" || low == "if it is neither day nor night" {
        return Some(Condition::And {
            of: vec![
                Condition::Not {
                    of: Box::new(Condition::IsDay),
                },
                Condition::Not {
                    of: Box::new(Condition::IsNight),
                },
            ],
        });
    }
    if low == "if the gift was promised" {
        return Some(Condition::GiftPromised);
    }
    if low == "if the gift wasn't promised" || low == "if the gift was not promised" {
        return Some(Condition::Not {
            of: Box::new(Condition::GiftPromised),
        });
    }

    if low == "if it's the first combat phase of the turn"
        || low == "if this is the first combat phase of the turn"
    {
        return Some(Condition::FirstCombatPhaseOfTurn);
    }
    if let Some(num_w) =
        strip_prefix_word_ci(&low, "if you attacked with creatures with total power").and_then(
            |r| {
                let r2 = strip_suffix_ws_ci(r, "this combat")
                    .or_else(|| strip_suffix_ws_ci(r, "this turn"))
                    .unwrap_or(r);
                strip_suffix_ws_ci(r2, "or greater").or_else(|| strip_suffix_ws_ci(r2, "or more"))
            },
        )
        && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::AttackingTotalPowerAtLeast { min });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if")
        .and_then(|r| strip_suffix_ws_ci(r, "or more creatures attacked this turn"))
        && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::CreaturesAttackedCountThisTurn { min });
    }
    if matches!(
        low.as_str(),
        "if it's not their turn"
            | "if it isn't their turn"
            | "if it is not their turn"
            | "if it's not that player's turn"
            | "if it isn't that player's turn"
            | "if it is not that player's turn"
            | "if it's not its controller's turn"
            | "if it isn't its controller's turn"
            | "if it is not its controller's turn"
    ) {
        return Some(Condition::NotEventPlayerTurn);
    }
    if low == "if that player has no cards in hand" {
        return Some(Condition::EventPlayerHandCount {
            op: CompareOp::Eq,
            count: 0,
        });
    }
    if let Some(mid) = strip_prefix_word_ci(&low, "if that player has").and_then(|r| {
        strip_suffix_ws_ci(r, "cards in hand").or_else(|| strip_suffix_ws_ci(r, "card in hand"))
    }) && let Some((num_w, dir_w)) = mid.split_once(" or ")
        && let Some(count) = parse_small_u16(num_w)
    {
        let op = match dir_w.trim() {
            "fewer" | "less" => Some(CompareOp::Le),
            "more" | "greater" => Some(CompareOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            return Some(Condition::EventPlayerHandCount { op, count });
        }
    }
    if let Some(mid) = strip_prefix_word_ci(&low, "if you have").and_then(|r| {
        strip_suffix_ws_ci(r, "cards in hand").or_else(|| strip_suffix_ws_ci(r, "card in hand"))
    }) && let Some((num_w, dir_w)) = mid.split_once(" or ")
        && let Some(n) = parse_small_i32(num_w)
    {
        let op = match dir_w.trim() {
            "fewer" | "less" => Some(CompareOp::Le),
            "more" | "greater" => Some(CompareOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            return Some(Condition::Compare {
                left: Box::new(Amount::CardsIn {
                    zone: Zone::Hand,
                    of: PlayerRef::You,
                }),
                op,
                right: const_amount(n),
            });
        }
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if you have fewer than").and_then(|r| {
        strip_suffix_ws_ci(r, "cards in hand").or_else(|| strip_suffix_ws_ci(r, "card in hand"))
    }) && let Some(n) = parse_small_i32(num_w)
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Lt,
            right: const_amount(n),
        });
    }
    if low == "if you have more cards in hand than each opponent" {
        return Some(Condition::MoreCardsInHandThanEachOpponent);
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if you have").and_then(|r| {
        strip_suffix_ws_ci(r, "or less life").or_else(|| strip_suffix_ws_ci(r, "or fewer life"))
    }) && let Some(n) = parse_small_i32(num_w)
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::Life { of: PlayerRef::You }),
            op: CompareOp::Le,
            right: const_amount(n),
        });
    }
    if let Some(after_have) = strip_prefix_word_ci(&low, "if you have")
        .map(|r| strip_prefix_word_ci(r, "exactly").unwrap_or(r))
        && let Some(num_w) = strip_suffix_ws_ci(after_have, "life")
        && let Some(n) = parse_small_i32(num_w)
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::Life { of: PlayerRef::You }),
            op: CompareOp::Eq,
            right: const_amount(n),
        });
    }
    if let Some(mid) =
        strip_prefix_word_ci(&low, "if a player has").and_then(|r| strip_suffix_ws_ci(r, "life"))
        && let Some((num_w, dir_w)) = mid.split_once(" or ")
        && let Some(count) = parse_small_i32(num_w)
    {
        let op = match dir_w.trim() {
            "less" | "fewer" => Some(CompareOp::Le),
            "more" | "greater" => Some(CompareOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            return Some(Condition::AnyPlayerLife { op, count });
        }
    }
    if let Some(after_opp) = strip_prefix_word_ci(&low, "if an opponent has") {
        let metric_match = [
            ("life", OpponentMetricKind::Life),
            ("cards in hand", OpponentMetricKind::Hand),
            ("cards in their graveyard", OpponentMetricKind::Graveyard),
        ]
        .iter()
        .find_map(|(sfx, m)| strip_suffix_ws_ci(after_opp, sfx).map(|l| (l, *m)));
        if let Some((mid, metric)) = metric_match
            && let Some((num_w, dir_w)) = mid.split_once(" or ")
            && let Some(count) = parse_small_i32(num_w)
        {
            let op = match dir_w.trim() {
                "fewer" | "less" => Some(CompareOp::Le),
                "more" | "greater" => Some(CompareOp::Ge),
                _ => None,
            };
            if let Some(op) = op {
                return Some(Condition::AnyOpponentMetric { metric, op, count });
            }
        }
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if you've drawn")
        .or_else(|| strip_prefix_word_ci(&low, "if you have drawn"))
        .and_then(|r| strip_suffix_ws_ci(r, "or more cards this turn"))
        && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::CardsDrawnThisTurnAtLeast { min });
    }
    if low == "if you have a full party" {
        return Some(Condition::Compare {
            left: Box::new(Amount::PartyCount {
                player: PlayerRef::You,
            }),
            op: CompareOp::Ge,
            right: const_amount(4),
        });
    }
    if low == "if no creatures are on the battlefield" {
        return Some(Condition::Compare {
            left: Box::new(Amount::Count {
                of: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        });
    }

    // Counter on self conditions
    if let Some(after_if) = strip_prefix_word_ci(t, "if") {
        let subj_ctr_ge = strip_prefix_word_ci(after_if, "there is")
            .or_else(|| strip_prefix_word_ci(after_if, "there are"))
            .or_else(|| strip_prefix_word_ci(after_if, "~ has"))
            .or_else(|| strip_prefix_word_ci(after_if, "it has"));
        if let Some(rest_c) = subj_ctr_ge
            && let Some((num_w, after_num)) = rest_c.split_once(char::is_whitespace)
            && let Some(cnt) = parse_small_i32(num_w)
        {
            let after_trim = after_num.trim_start();
            if let Some(ctr_raw) = strip_prefix_word_ci(after_trim, "or more").and_then(|r| {
                strip_suffix_ws_ci(r, "counters on ~")
                    .or_else(|| strip_suffix_ws_ci(r, "counter on ~"))
                    .or_else(|| strip_suffix_ws_ci(r, "counters on it"))
                    .or_else(|| strip_suffix_ws_ci(r, "counter on it"))
            }) {
                let cp = ctx.interner.checkpoint();
                if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
                    return Some(Condition::Compare {
                        left: Box::new(Amount::CountersOn {
                            counter: CounterTarget::Specific(ctr),
                            of: mtg_ir::Ref::SelfObj,
                        }),
                        op: CompareOp::Ge,
                        right: const_amount(cnt),
                    });
                }
                ctx.interner.rollback(cp);
            }
            if let Some(ctr_raw) = strip_prefix_word_ci(after_trim, "or fewer")
                .or_else(|| strip_prefix_word_ci(after_trim, "or less"))
                .and_then(|r| {
                    strip_suffix_ws_ci(r, "counters on ~")
                        .or_else(|| strip_suffix_ws_ci(r, "counter on ~"))
                        .or_else(|| strip_suffix_ws_ci(r, "counters on it"))
                        .or_else(|| strip_suffix_ws_ci(r, "counter on it"))
                })
            {
                let cp = ctx.interner.checkpoint();
                if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
                    return Some(Condition::Compare {
                        left: Box::new(Amount::CountersOn {
                            counter: CounterTarget::Specific(ctr),
                            of: mtg_ir::Ref::SelfObj,
                        }),
                        op: CompareOp::Le,
                        right: const_amount(cnt),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }
    if matches!(
        low.as_str(),
        "if there are no counters on ~"
            | "if there are no counters on it"
            | "if ~ has no counters on ~"
            | "if ~ has no counters on it"
            | "if it has no counters on ~"
            | "if it has no counters on it"
    ) {
        return Some(Condition::Compare {
            left: Box::new(Amount::CountersOn {
                counter: CounterTarget::Any,
                of: mtg_ir::Ref::SelfObj,
            }),
            op: CompareOp::Eq,
            right: const_amount(0),
        });
    }
    if let Some(ctr_raw) = strip_prefix_word_ci(t, "if there are no")
        .or_else(|| strip_prefix_word_ci(t, "if ~ has no"))
        .or_else(|| strip_prefix_word_ci(t, "if it has no"))
        .and_then(|r| {
            strip_suffix_ws_ci(r, "counters on ~")
                .or_else(|| strip_suffix_ws_ci(r, "counter on ~"))
                .or_else(|| strip_suffix_ws_ci(r, "counters on it"))
                .or_else(|| strip_suffix_ws_ci(r, "counter on it"))
        })
    {
        let cp = ctx.interner.checkpoint();
        if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
            return Some(Condition::Compare {
                left: Box::new(Amount::CountersOn {
                    counter: CounterTarget::Specific(ctr),
                    of: mtg_ir::Ref::SelfObj,
                }),
                op: CompareOp::Eq,
                right: const_amount(0),
            });
        }
        ctx.interner.rollback(cp);
    }
    if matches!(
        low.as_str(),
        "if ~ has counters on it"
            | "if ~ has one or more counters on it"
            | "if it has counters on it"
            | "if it has one or more counters on it"
    ) {
        return Some(Condition::Compare {
            left: Box::new(Amount::CountersOn {
                counter: CounterTarget::Any,
                of: mtg_ir::Ref::SelfObj,
            }),
            op: CompareOp::Ge,
            right: const_amount(1),
        });
    }
    if let Some(after_has) =
        strip_prefix_word_ci(t, "if ~ has").or_else(|| strip_prefix_word_ci(t, "if it has"))
        && let Some(ctr_raw) = strip_prefix_word_ci(after_has, "one or more")
            .or_else(|| strip_prefix_word_ci(after_has, "a"))
            .or_else(|| strip_prefix_word_ci(after_has, "an"))
            .and_then(|r| {
                strip_suffix_ws_ci(r, "counters on it")
                    .or_else(|| strip_suffix_ws_ci(r, "counter on it"))
            })
    {
        let cp = ctx.interner.checkpoint();
        if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
            return Some(Condition::Compare {
                left: Box::new(Amount::CountersOn {
                    counter: CounterTarget::Specific(ctr),
                    of: mtg_ir::Ref::SelfObj,
                }),
                op: CompareOp::Ge,
                right: const_amount(1),
            });
        }
        ctx.interner.rollback(cp);
    }

    if low == "if a card left your graveyard this turn" {
        return Some(Condition::CardLeftYourGraveyardThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you committed a crime this turn"
            | "if you've committed a crime this turn"
            | "if you have committed a crime this turn"
    ) {
        return Some(Condition::CommittedCrimeThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you lost life this turn"
            | "if you've lost life this turn"
            | "if you have lost life this turn"
    ) {
        return Some(Condition::YouLostLifeThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you gained or lost life this turn"
            | "if you've gained or lost life this turn"
            | "if you have gained or lost life this turn"
    ) {
        return Some(Condition::Or {
            of: vec![
                Condition::GainedLifeThisTurn { min: None },
                Condition::YouLostLifeThisTurn,
            ],
        });
    }
    if let Some((opp_only, after_lost)) = strip_prefix_word_ci(&low, "if a player lost")
        .map(|r| (false, r))
        .or_else(|| strip_prefix_word_ci(&low, "if an opponent lost").map(|r| (true, r)))
        && let Some(num_w) = strip_suffix_ws_ci(after_lost, "or more life this turn")
        && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::PlayerLostLifeAtLeastThisTurn {
            min,
            opponent_only: opp_only,
        });
    }
    if matches!(
        low.as_str(),
        "if you haven't cast a spell this turn"
            | "if you have not cast a spell this turn"
            | "if you didn't cast a spell this turn"
            | "if you did not cast a spell this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::SpellsCastThisTurnAtLeast {
                min: 1,
                filter: None,
            }),
        });
    }
    if low == "if a player other than ~'s owner controls it"
        || low == "if a player other than ~'s owner controls ~"
    {
        return Some(Condition::ControlledByOtherThanOwner);
    }

    // "if (~ is|it is|it's)[ not|n't] (a|an) <filter>"
    let self_filter_parsed = [
        ("if it's not ", true),
        ("if it's ", false),
        ("if ~ isn't ", true),
        ("if ~ is not ", true),
        ("if ~ is ", false),
        ("if it isn't ", true),
        ("if it is not ", true),
        ("if it is ", false),
    ]
    .iter()
    .find_map(|(pref, neg)| strip_prefix_ci(t, pref).map(|r| (*neg, r.trim_start())));
    if let Some((neg, after_is)) = self_filter_parsed
        && let Some(f_raw) =
            strip_prefix_word_ci(after_is, "a").or_else(|| strip_prefix_word_ci(after_is, "an"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && object_filter_problem(&f).is_none()
        {
            let base = Condition::MatchesFilter {
                target: Some(mtg_ir::Ref::SelfObj),
                filter: Box::new(f),
                any: false,
            };
            return Some(if neg {
                Condition::Not { of: Box::new(base) }
            } else {
                base
            });
        }
        ctx.interner.rollback(cp);
    }

    // LKI characteristic check on event object: "if (it|that creature|that permanent) was [not ](a|an) <filter>"
    if let Some(after_was) = strip_prefix_word_ci(t, "if it was")
        .or_else(|| strip_prefix_word_ci(t, "if that creature was"))
        .or_else(|| strip_prefix_word_ci(t, "if that permanent was"))
    {
        let (neg, after_neg) = if let Some(r) = strip_prefix_word_ci(after_was, "not") {
            (true, r)
        } else {
            (false, after_was)
        };
        if let Some(f_raw) =
            strip_prefix_word_ci(after_neg, "a").or_else(|| strip_prefix_word_ci(after_neg, "an"))
        {
            let cp = ctx.interner.checkpoint();
            if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && object_filter_problem(&f).is_none()
            {
                let base = Condition::EventObjectMatchedLki {
                    filter: Box::new(f),
                };
                return Some(if neg {
                    Condition::Not { of: Box::new(base) }
                } else {
                    base
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    if low == "if a creature died under your control this turn" {
        return Some(Condition::CreatureDiedUnderYourControlThisTurn);
    }
    if matches!(
        low.as_str(),
        "if no creatures died under your control this turn"
            | "if no creature died under your control this turn"
            | "if a creature didn't die under your control this turn"
            | "if a creature did not die under your control this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::CreatureDiedUnderYourControlThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if mana from a treasure was spent to cast it"
            | "if mana from a treasure was spent to cast ~"
            | "if mana from a treasure was spent to cast this spell"
    ) {
        return Some(Condition::TreasureManaSpentToCast);
    }
    if matches!(
        low.as_str(),
        "if mana from a treasure wasn't spent to cast it"
            | "if mana from a treasure was not spent to cast it"
            | "if mana from a treasure wasn't spent to cast ~"
            | "if mana from a treasure was not spent to cast ~"
            | "if mana from a treasure wasn't spent to cast this spell"
            | "if mana from a treasure was not spent to cast this spell"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::TreasureManaSpentToCast),
        });
    }
    if matches!(
        low.as_str(),
        "if you haven't cast a spell from your hand this turn"
            | "if you have not cast a spell from your hand this turn"
            | "if you didn't cast a spell from your hand this turn"
            | "if you did not cast a spell from your hand this turn"
    ) {
        return Some(Condition::NoSpellCastFromHandThisTurn);
    }
    if matches!(
        low.as_str(),
        "if you cast a spell from your hand this turn"
            | "if you've cast a spell from your hand this turn"
            | "if you have cast a spell from your hand this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::NoSpellCastFromHandThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if ~ is suspended" | "if it is suspended" | "if it's suspended"
    ) {
        return Some(Condition::IsSuspended);
    }
    if matches!(
        low.as_str(),
        "if ~ isn't suspended"
            | "if ~ is not suspended"
            | "if it isn't suspended"
            | "if it is not suspended"
            | "if it's not suspended"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::IsSuspended),
        });
    }
    if low == "if you chose a creature other than ~ as your ring-bearer" {
        return Some(Condition::ChoseOtherRingBearer);
    }
    if let Some((paid, kind)) = [
        (
            "if its surge cost was paid",
            (true, NamedAltCostKind::Surge),
        ),
        (
            "if the surge cost was paid",
            (true, NamedAltCostKind::Surge),
        ),
        (
            "if its prowl cost was paid",
            (true, NamedAltCostKind::Prowl),
        ),
        (
            "if the prowl cost was paid",
            (true, NamedAltCostKind::Prowl),
        ),
        (
            "if its spectacle cost was paid",
            (true, NamedAltCostKind::Spectacle),
        ),
        (
            "if the spectacle cost was paid",
            (true, NamedAltCostKind::Spectacle),
        ),
        (
            "if its surge cost wasn't paid",
            (false, NamedAltCostKind::Surge),
        ),
        (
            "if its surge cost was not paid",
            (false, NamedAltCostKind::Surge),
        ),
        (
            "if the surge cost wasn't paid",
            (false, NamedAltCostKind::Surge),
        ),
        (
            "if the surge cost was not paid",
            (false, NamedAltCostKind::Surge),
        ),
        (
            "if its prowl cost wasn't paid",
            (false, NamedAltCostKind::Prowl),
        ),
        (
            "if its prowl cost was not paid",
            (false, NamedAltCostKind::Prowl),
        ),
        (
            "if the prowl cost wasn't paid",
            (false, NamedAltCostKind::Prowl),
        ),
        (
            "if the prowl cost was not paid",
            (false, NamedAltCostKind::Prowl),
        ),
        (
            "if its spectacle cost wasn't paid",
            (false, NamedAltCostKind::Spectacle),
        ),
        (
            "if its spectacle cost was not paid",
            (false, NamedAltCostKind::Spectacle),
        ),
        (
            "if the spectacle cost wasn't paid",
            (false, NamedAltCostKind::Spectacle),
        ),
        (
            "if the spectacle cost was not paid",
            (false, NamedAltCostKind::Spectacle),
        ),
    ]
    .iter()
    .find_map(|(pat, res)| (low == *pat).then_some(*res))
    {
        let base = Condition::PaidNamedAltCost { kind };
        return Some(if paid {
            base
        } else {
            Condition::Not { of: Box::new(base) }
        });
    }

    // "if ((a|an|one or more) <f>|another <f>) entered[ the battlefield] under your control this turn"
    if let Some(after_if) = strip_prefix_word_ci(t, "if")
        && let Some(mid) = strip_suffix_ws_ci(
            after_if,
            "entered the battlefield under your control this turn",
        )
        .or_else(|| strip_suffix_ws_ci(after_if, "entered under your control this turn"))
        && let Some(f_raw) = strip_prefix_word_ci(mid, "one or more")
            .or_else(|| strip_prefix_word_ci(mid, "a"))
            .or_else(|| strip_prefix_word_ci(mid, "an"))
            .or_else(|| starts_with_word_ci(mid, "another").then_some(mid))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::EnteredUnderYourControlThisTurn {
                filter: Box::new(f),
                min: None,
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_had) = strip_prefix_word_ci(t, "if you had")
        && let Some(mid) = strip_suffix_ws_ci(
            after_had,
            "enter the battlefield under your control this turn",
        )
        .or_else(|| strip_suffix_ws_ci(after_had, "enter under your control this turn"))
        && let Some(f_raw) =
            strip_prefix_word_ci(mid, "a").or_else(|| strip_prefix_word_ci(mid, "an"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::EnteredUnderYourControlThisTurn {
                filter: Box::new(f),
                min: None,
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(f_raw) = strip_prefix_word_ci(t, "if no").and_then(|r| {
        strip_suffix_ws_ci(r, "entered the battlefield under your control this turn")
            .or_else(|| strip_suffix_ws_ci(r, "entered under your control this turn"))
    }) {
        let cp = ctx.interner.checkpoint();
        if let Some(f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            return Some(Condition::Not {
                of: Box::new(Condition::EnteredUnderYourControlThisTurn {
                    filter: Box::new(f),
                    min: None,
                }),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_lead) = ["if there's", "if there is", "if there are", "if you have"]
        .iter()
        .find_map(|p| strip_prefix_word_ci(t, p))
        && let Some(after_qty) = strip_prefix_word_ci(after_lead, "one or more")
            .or_else(|| strip_prefix_word_ci(after_lead, "a"))
            .or_else(|| strip_prefix_word_ci(after_lead, "an"))
        && let Some(qual) = strip_suffix_ws_ci(after_qty, "cards in your graveyard")
            .or_else(|| strip_suffix_ws_ci(after_qty, "card in your graveyard"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_graveyard_leave_card_filter(Some(qual), ctx)
            .or_else(|| parse_trigger_object_phrase(&format!("{qual} card"), ctx, false))
            && object_filter_problem(&f).is_none()
        {
            f.zones = ZoneMask::GRAVEYARD;
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: const_amount(1),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(num_w) = strip_prefix_word_ci(&low, "if there are")
        .and_then(|r| strip_suffix_ws_ci(r, "or more mana values among cards in your graveyard"))
        && let Some(min) = parse_small_u16(num_w)
    {
        return Some(Condition::DistinctManaValuesInGraveyardAtLeast { min });
    }
    if let Some(after_exact) = strip_prefix_word_ci(t, "if you control exactly")
        && let Some((num_w, f_raw)) = after_exact.split_once(char::is_whitespace)
        && let Some(n) = parse_small_i32(num_w)
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(f_raw.trim_start(), ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Eq,
                right: const_amount(n),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_dont) = strip_prefix_word_ci(t, "if you don't control")
        .or_else(|| strip_prefix_word_ci(t, "if you do not control"))
        && let Some(f_raw) = strip_prefix_word_ci(after_dont, "a")
            .or_else(|| strip_prefix_word_ci(after_dont, "an"))
            .or_else(|| strip_prefix_word_ci(after_dont, "any"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Eq,
                right: const_amount(0),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(f_raw) = strip_prefix_word_ci(t, "if your opponents control no")
        .or_else(|| strip_prefix_word_ci(t, "if an opponent controls no"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
            && f.controller.is_none()
            && object_filter_problem(&f).is_none()
        {
            f.controller = Some(RelativeControl::Opponent);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Eq,
                right: const_amount(0),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_opp) = strip_prefix_word_ci(t, "if your opponents control")
        .or_else(|| strip_prefix_word_ci(t, "if an opponent controls"))
    {
        let parsed = if let Some(r) =
            strip_prefix_word_ci(after_opp, "a").or_else(|| strip_prefix_word_ci(after_opp, "an"))
        {
            Some((1i32, r))
        } else if let Some((num_w, after_num)) = after_opp.split_once(char::is_whitespace)
            && let Some(n) = parse_small_i32(num_w)
            && let Some(r) = strip_prefix_word_ci(after_num.trim_start(), "or more")
        {
            Some((n, r))
        } else {
            None
        };
        if let Some((cnt, f_raw)) = parsed {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) = parse_trigger_object_phrase(f_raw, ctx, false)
                && f.controller.is_none()
                && object_filter_problem(&f).is_none()
            {
                f.controller = Some(RelativeControl::Opponent);
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: const_amount(cnt),
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    if matches!(
        low.as_str(),
        "if ~ entered this turn"
            | "if ~ entered the battlefield this turn"
            | "if it entered this turn"
            | "if it entered the battlefield this turn"
    ) {
        return Some(Condition::SelfEnteredThisTurn);
    }
    if matches!(
        low.as_str(),
        "if ~ didn't enter this turn"
            | "if ~ did not enter this turn"
            | "if ~ didn't enter the battlefield this turn"
            | "if ~ did not enter the battlefield this turn"
            | "if it didn't enter this turn"
            | "if it did not enter this turn"
            | "if it didn't enter the battlefield this turn"
            | "if it did not enter the battlefield this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::SelfEnteredThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if that creature entered this turn"
            | "if that creature entered the battlefield this turn"
            | "if that permanent entered this turn"
            | "if that permanent entered the battlefield this turn"
    ) {
        return Some(Condition::EventObjectEnteredThisTurn);
    }
    if matches!(
        low.as_str(),
        "if that creature didn't enter this turn"
            | "if that creature did not enter this turn"
            | "if that creature didn't enter the battlefield this turn"
            | "if that creature did not enter the battlefield this turn"
            | "if that permanent didn't enter this turn"
            | "if that permanent did not enter this turn"
            | "if that permanent didn't enter the battlefield this turn"
            | "if that permanent did not enter the battlefield this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::EventObjectEnteredThisTurn),
        });
    }
    if matches!(
        low.as_str(),
        "if ~ isn't being declared as an attacker"
            | "if ~ is not being declared as an attacker"
            | "if it isn't being declared as an attacker"
            | "if it is not being declared as an attacker"
            | "if it's not being declared as an attacker"
    ) {
        return Some(Condition::NotBeingDeclaredAsAttacker);
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if ~ was dealt")
        .and_then(|r| strip_suffix_ws_ci(r, "or more damage this turn"))
        && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::DamageDealtToSelfThisTurnAtLeast { min });
    }
    if let Some(num_w) = strip_prefix_word_ci(&low, "if").and_then(|r| {
        strip_suffix_ws_ci(r, "or more damage was dealt to it this turn")
            .or_else(|| strip_suffix_ws_ci(r, "or more damage was dealt to ~ this turn"))
    }) && let Some(min) = parse_small_i32(num_w)
    {
        return Some(Condition::DamageDealtToSelfThisTurnAtLeast { min });
    }
    if low == "if ~ didn't attack or come under your control this turn"
        || low == "if ~ did not attack or come under your control this turn"
    {
        return Some(Condition::And {
            of: vec![
                Condition::Not {
                    of: Box::new(Condition::BoastAttackedThisTurn),
                },
                Condition::Not {
                    of: Box::new(Condition::SelfEnteredThisTurn),
                },
            ],
        });
    }
    if let Some(qual) = strip_prefix_word_ci(t, "if ~ is the only")
        .and_then(|r| strip_suffix_ws_ci(r, "card in your graveyard"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_graveyard_leave_card_filter(Some(qual), ctx)
            .or_else(|| parse_trigger_object_phrase(&format!("{qual} card"), ctx, false))
            && object_filter_problem(&f).is_none()
        {
            f.zones = ZoneMask::GRAVEYARD;
            f.controller = Some(RelativeControl::You);
            return Some(Condition::And {
                of: vec![
                    Condition::SelfInZone {
                        zone: Zone::Graveyard,
                    },
                    Condition::Compare {
                        left: Box::new(Amount::Count { of: Box::new(f) }),
                        op: CompareOp::Eq,
                        right: const_amount(1),
                    },
                ],
            });
        }
        ctx.interner.rollback(cp);
    }
    if matches!(
        low.as_str(),
        "if ~ dealt damage to a player this turn"
            | "if ~ dealt combat damage to a player this turn"
            | "if ~ dealt damage to an opponent this turn"
            | "if ~ dealt combat damage to an opponent this turn"
    ) {
        return Some(Condition::SelfDealtDamageToOpponentThisTurn);
    }
    if matches!(
        low.as_str(),
        "if ~ didn't deal damage to a player this turn"
            | "if ~ did not deal damage to a player this turn"
            | "if ~ didn't deal combat damage to a player this turn"
            | "if ~ did not deal combat damage to a player this turn"
            | "if ~ didn't deal damage to an opponent this turn"
            | "if ~ did not deal damage to an opponent this turn"
            | "if ~ didn't deal combat damage to an opponent this turn"
            | "if ~ did not deal combat damage to an opponent this turn"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::SelfDealtDamageToOpponentThisTurn),
        });
    }

    parse_trigger_condition_p22b(t, ctx)
}

fn cond_uses_dungeon_completion(c: &Condition) -> bool {
    match c {
        Condition::HasCompletedDungeon => true,
        Condition::Not { of } => cond_uses_dungeon_completion(of),
        Condition::And { of } | Condition::Or { of } => of.iter().any(cond_uses_dungeon_completion),
        _ => false,
    }
}

fn parse_trigger_condition_depth(
    text: &str,
    allow_dungeon_completion: bool,
    ctx: &mut ParseCtx<'_>,
    depth: u8,
) -> Option<Condition> {
    let t = normalize_apostrophes(text.trim());
    let t = t.as_str();
    if t.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();
    if let Some(c) = parse_trigger_condition_core(t, allow_dungeon_completion, ctx) {
        return Some(c);
    }
    ctx.interner.rollback(cp);

    // Compound "and" / "or" at condition level before falling back to value::parse_condition
    if depth < 2 {
        for (sep, is_or) in [("or", true), ("and", false)] {
            for (start, end) in find_ws_word_ci_all(t, sep) {
                let left = t[..start].trim();
                let right_raw = t[end..].trim();
                let right = strip_prefix_word_ci(right_raw, "if").unwrap_or(right_raw);
                if left.is_empty() || right.is_empty() {
                    continue;
                }
                let cp = ctx.interner.checkpoint();
                if let (Some(lc), Some(rc)) = (
                    parse_trigger_condition_depth(left, allow_dungeon_completion, ctx, depth + 1),
                    parse_trigger_condition_depth(
                        &format!("if {right}"),
                        allow_dungeon_completion,
                        ctx,
                        depth + 1,
                    ),
                ) {
                    return Some(if is_or {
                        Condition::Or { of: vec![lc, rc] }
                    } else {
                        Condition::And { of: vec![lc, rc] }
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // Fallback to general M2 value::parse_condition
    let cp = ctx.interner.checkpoint();
    let toks = tokenize(t, 0);
    if !toks.is_empty() {
        let mut scope = NullScope;
        let mut sub_ctx = ctx.with_normalized_text(t);
        if let Some((cond, used)) = parse_condition(&toks, &mut sub_ctx, &mut scope)
            && used == toks.len()
            && (allow_dungeon_completion || !cond_uses_dungeon_completion(&cond))
        {
            return Some(cond);
        }
    }
    ctx.interner.rollback(cp);
    None
}

/// Parses an intervening-if or inline-while condition string (e.g. `"if ~ was kicked"`).
pub fn parse_trigger_condition(
    text: &str,
    allow_dungeon_completion: bool,
    ctx: &mut ParseCtx<'_>,
) -> Option<Condition> {
    parse_trigger_condition_depth(text, allow_dungeon_completion, ctx, 0)
}

fn parse_extra_restriction_str(s: &str) -> Option<ActivatedExtraRestriction> {
    match s.trim().to_ascii_lowercase().as_str() {
        "as a sorcery" => Some(ActivatedExtraRestriction::AsASorcery),
        "once each turn" => Some(ActivatedExtraRestriction::OnceEachTurn),
        "during your turn" => Some(ActivatedExtraRestriction::DuringYourTurn),
        "during your upkeep" => Some(ActivatedExtraRestriction::DuringYourUpkeep),
        "during combat" => Some(ActivatedExtraRestriction::DuringCombat),
        _ => None,
    }
}

/// Parses `"Activate only if <cond>[ and only <restriction>]."` (used by `effect.rs` / `static_def.rs`).
pub fn parse_activated_if_condition_sentence(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ActivatedIfConditionParse> {
    let trimmed = normalize_apostrophes(raw.trim());
    let t = trimmed.trim_end_matches('.').trim();
    let low = t.to_ascii_lowercase();
    if low.contains("~ is in your hand")
        || low.contains("~ is in your graveyard")
        || low.contains("~ is in exile")
        || low.contains("~ is suspended")
    {
        return None;
    }

    if let Some(after_if) = strip_prefix_word_ci(t, "activate only if") {
        let (cond_part, extra) = if let Some((l, r)) = rsplit_once_ws_word_ci(after_if, "and only")
            && let Some(ext) = parse_extra_restriction_str(r)
        {
            (l, Some(ext))
        } else {
            (after_if, None)
        };
        let condition = parse_p15_activated_if_clause(cond_part, ctx)?;
        return Some(ActivatedIfConditionParse { condition, extra });
    }

    if let Some(after_only) = strip_prefix_word_ci(t, "activate only")
        && let Some((ext_raw, cond_raw)) = split_once_ws_word_ci(after_only, "and only if")
        && let Some(extra) = parse_extra_restriction_str(ext_raw)
    {
        let condition = parse_p15_activated_if_clause(cond_raw, ctx)?;
        return Some(ActivatedIfConditionParse {
            condition,
            extra: Some(extra),
        });
    }

    None
}

// -----------------------------------------------------------------------------
// parse_trigger_header, parse_p12_trigger_header, runtime_trigger_problem
// -----------------------------------------------------------------------------

fn trigger_head_with_inline_while(
    raw_head: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<(TriggerDef, Option<Condition>)> {
    {
        let cp = ctx.interner.checkpoint();
        if let Some(direct) = trigger_head(raw_head, ctx, 0) {
            return Some((direct, None));
        }
        ctx.interner.rollback(cp);
    }
    let while_hits = find_ws_word_ci_all(raw_head, "while");
    if while_hits.is_empty() {
        return None;
    }
    for (start, end) in while_hits.into_iter().rev() {
        let left_text = raw_head[..start].trim();
        let right_clause = raw_head[end..].trim();
        if left_text.is_empty() || right_clause.is_empty() {
            continue;
        }
        let cp = ctx.interner.checkpoint();
        if let Some(inline_cond) = parse_trigger_condition(&format!("if {right_clause}"), true, ctx)
            && let Some(left_trig) = trigger_head(left_text, ctx, 0)
        {
            return Some((left_trig, Some(inline_cond)));
        }
        ctx.interner.rollback(cp);
    }
    None
}

fn condition_implies_suspended_exile(cond: Option<&Condition>) -> bool {
    match cond {
        None => false,
        Some(Condition::IsSuspended) => true,
        Some(Condition::And { of }) => of
            .iter()
            .any(|c| condition_implies_suspended_exile(Some(c))),
        _ => false,
    }
}

fn condition_implies_exile(cond: Option<&Condition>) -> bool {
    match cond {
        None => false,
        Some(Condition::SelfInZone { zone: Zone::Exile }) => true,
        Some(c) if condition_implies_suspended_exile(Some(c)) => true,
        Some(Condition::And { of }) => of.iter().any(|c| condition_implies_exile(Some(c))),
        _ => false,
    }
}

fn condition_implies_graveyard(cond: Option<&Condition>) -> bool {
    match cond {
        None => false,
        Some(Condition::GraveyardAbove { .. }) => true,
        Some(Condition::SelfInZone {
            zone: Zone::Graveyard,
        }) => true,
        Some(Condition::And { of }) => of.iter().any(|c| condition_implies_graveyard(Some(c))),
        _ => false,
    }
}

fn candidate_sentence_commas(body: &str) -> Vec<usize> {
    let mut commas = Vec::new();
    let mut in_quotes = false;
    let mut chars = body.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '"' || ch == '“' || ch == '”' {
            in_quotes = !in_quotes;
            continue;
        }
        if in_quotes {
            continue;
        }
        if ch == '.' {
            match chars.peek() {
                None => break,
                Some(&(_, next_ch)) if next_ch.is_whitespace() => break,
                _ => {}
            }
        }
        if ch == ',' {
            commas.push(i);
        }
    }
    commas
}

/// Main entry point for trigger headers (`When/Whenever/At ...`).
/// Returns `None` if `body` does not start with `when`/`whenever`/`at` (word-bounded, CI).
pub fn parse_trigger_header(
    body: &str,
    offset: u32,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggerParse> {
    if !(starts_with_word_ci(body, "when")
        || starts_with_word_ci(body, "whenever")
        || starts_with_word_ci(body, "at"))
    {
        return None;
    }

    let commas = candidate_sentence_commas(body);
    let mut comma: Option<usize> = None;
    let mut trigger: Option<TriggerDef> = None;
    let mut inline_condition: Option<Condition> = None;

    if !commas.is_empty() {
        for c in commas
            .iter()
            .copied()
            .filter(|&c| c <= 512)
            .take(16)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            if let Some((trig, inline_cond)) = trigger_head_with_inline_while(&body[..c], ctx) {
                comma = Some(c);
                trigger = Some(trig);
                inline_condition = inline_cond;
                break;
            }
        }
        if comma.is_none() {
            comma = Some(commas[0]);
        }
    } else {
        let Some(c) = body.find(',') else {
            return Some(TriggerParse {
                trigger: None,
                condition: None,
                effect_start: offset + body.len() as u32,
                unparsed: vec![Span {
                    start: offset,
                    end: offset + body.len() as u32,
                }],
            });
        };
        comma = Some(c);
        if let Some((trig, inline_cond)) = trigger_head_with_inline_while(&body[..c], ctx) {
            trigger = Some(trig);
            inline_condition = inline_cond;
        }
    }

    let comma_idx = comma.unwrap_or(0);
    let mut unparsed = Vec::new();
    let head = &body[..comma_idx];
    if trigger.is_none() {
        unparsed.push(Span {
            start: offset,
            end: offset + head.len() as u32,
        });
    }

    let mut effect_start = comma_idx + 1;
    let mut condition = inline_condition.clone();
    let rest = &body[effect_start..];
    let rest_trimmed = rest.trim_start();
    let lead = rest.len() - rest_trimmed.len();

    if starts_with_word_ci(rest_trimmed, "if") {
        let cond_start = effect_start + lead;
        let cond_commas: Vec<usize> = commas.iter().copied().filter(|&c| c > cond_start).collect();
        let mut cond_end: Option<usize> = None;
        let mut post_comma_cond: Option<Condition> = None;

        if !cond_commas.is_empty() {
            for &c in cond_commas.iter().rev() {
                if let Some(parsed_cond) = parse_trigger_condition(&body[cond_start..c], true, ctx)
                {
                    cond_end = Some(c);
                    post_comma_cond = Some(parsed_cond);
                    break;
                }
            }
            if cond_end.is_none() {
                cond_end = Some(cond_commas[0]);
            }
        } else if let Some(rel_c) = body[cond_start..].find(',') {
            let abs_c = cond_start + rel_c;
            cond_end = Some(abs_c);
            post_comma_cond = parse_trigger_condition(&body[cond_start..abs_c], true, ctx);
        }

        let Some(ce) = cond_end else {
            unparsed.push(Span {
                start: offset + cond_start as u32,
                end: offset + body.len() as u32,
            });
            return Some(TriggerParse {
                trigger,
                condition,
                effect_start: offset + body.len() as u32,
                unparsed,
            });
        };

        if let Some(post_cond) = post_comma_cond {
            condition = match inline_condition {
                Some(ic) => Some(Condition::And {
                    of: vec![ic, post_cond],
                }),
                None => Some(post_cond),
            };
        } else {
            unparsed.push(Span {
                start: offset + cond_start as u32,
                end: offset + ce as u32,
            });
            condition = None;
        }
        effect_start = ce + 1;
    }

    if let Some(ref mut trig) = trigger {
        if condition_implies_graveyard(condition.as_ref()) {
            trig.from_zones = Some(ZoneMask::GRAVEYARD);
            for alt in &mut trig.or {
                if alt.from_zones.is_none() {
                    alt.from_zones = Some(ZoneMask::GRAVEYARD);
                }
            }
        } else if condition_implies_exile(condition.as_ref()) && trig.from_zones.is_none() {
            trig.from_zones = Some(ZoneMask::EXILE);
            for alt in &mut trig.or {
                if alt.from_zones.is_none() {
                    alt.from_zones = Some(ZoneMask::EXILE);
                }
            }
        }
    }

    let after = &body[effect_start..];
    effect_start += after.len() - after.trim_start().len();

    Some(TriggerParse {
        trigger,
        condition,
        effect_start: offset + effect_start as u32,
        unparsed,
    })
}

fn parse_p15_activated_if_clause(raw_clause: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let trimmed = normalize_apostrophes(raw_clause.trim());
    let s = trimmed.trim_end_matches('.').trim();
    let low = s.to_ascii_lowercase();
    if low == "you have max speed" {
        return Some(Condition::HasMaxSpeed);
    }
    if low == "you don't have max speed" || low == "you do not have max speed" {
        return Some(Condition::Not {
            of: Box::new(Condition::HasMaxSpeed),
        });
    }
    if low == "you have the city's blessing" || low == "you've got the city's blessing" {
        return Some(Condition::HasCitysBlessing);
    }
    if low == "you don't have the city's blessing" || low == "you do not have the city's blessing" {
        return Some(Condition::Not {
            of: Box::new(Condition::HasCitysBlessing),
        });
    }
    if matches!(
        low.as_str(),
        "~ is prepared" | "it is prepared" | "it's prepared"
    ) {
        return Some(Condition::SelfPrepared);
    }
    if matches!(
        low.as_str(),
        "~ isn't prepared"
            | "~ is not prepared"
            | "~ is unprepared"
            | "it isn't prepared"
            | "it is not prepared"
            | "it is unprepared"
            | "it's not prepared"
    ) {
        return Some(Condition::Not {
            of: Box::new(Condition::SelfPrepared),
        });
    }
    if low == "it's day" || low == "it is day" {
        return Some(Condition::IsDay);
    }
    if low == "it's night" || low == "it is night" {
        return Some(Condition::IsNight);
    }
    parse_trigger_condition(&format!("if {s}"), true, ctx)
}

fn parse_p18b_intervening_condition_clause(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let trimmed = normalize_apostrophes(raw.trim());
    let s = trimmed.trim_end_matches('.').trim();

    if let Some(after_ta) = strip_prefix_word_ci(s, "there are")
        && let Some((num_w, after_num)) = after_ta.split_once(char::is_whitespace)
        && let Some(n) = parse_small_i32(num_w)
        && let Some(qual) = strip_prefix_word_ci(after_num.trim_start(), "or more").and_then(|r| {
            strip_suffix_ws_ci(r, "cards in your graveyard")
                .or_else(|| strip_suffix_ws_ci(r, "card in your graveyard"))
        })
    {
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) = parse_multi_or_object_phrase(
            qual,
            ctx,
            FilterOpts {
                default_zone: ZoneMask::GRAVEYARD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
        ) {
            f.controller = Some(RelativeControl::You);
            f.zones = ZoneMask::GRAVEYARD;
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: const_amount(n),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_ta) =
        strip_prefix_word_ci(s, "there are").or_else(|| strip_prefix_word_ci(s, "there is"))
        && let Some((num_w, after_num)) = after_ta.split_once(char::is_whitespace)
        && let Some(n) = parse_small_i32(num_w)
        && let Some(ctr_raw) =
            strip_prefix_word_ci(after_num.trim_start(), "or more").and_then(|r| {
                strip_suffix_ws_ci(r, "counters on ~")
                    .or_else(|| strip_suffix_ws_ci(r, "counter on ~"))
            })
    {
        let cp = ctx.interner.checkpoint();
        if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
            return Some(Condition::Compare {
                left: Box::new(Amount::CountersOn {
                    counter: CounterTarget::Specific(ctr),
                    of: mtg_ir::Ref::SelfObj,
                }),
                op: CompareOp::Ge,
                right: const_amount(n),
            });
        }
        ctx.interner.rollback(cp);
    }

    let low = s.to_ascii_lowercase();
    if matches!(
        low.as_str(),
        "you've been attacked this step" | "you're being attacked" | "a creature is attacking you"
    ) {
        return Some(Condition::YouWereAttackedThisCombat);
    }

    if let Some(q) = strip_prefix_word_ci(s, "you've cast another")
        .and_then(|r| strip_suffix_ws_ci(r, "spell this turn"))
    {
        let cp = ctx.interner.checkpoint();
        let opts = FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: true,
            ..Default::default()
        };
        if let Some(f) = parse_multi_or_object_phrase(q, ctx, opts)
            .or_else(|| parse_multi_or_object_phrase(&format!("{q} card"), ctx, opts))
        {
            return Some(Condition::SpellsCastThisTurnAtLeast {
                min: 1,
                filter: Some(Box::new(f)),
            });
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_ctrl) = strip_prefix_word_ci(s, "you control") {
        for conj in ["and a", "and an"] {
            if let Some((f1_raw, f2_raw)) = split_once_ws_word_ci(after_ctrl, conj)
                && let Some(f1_inner) =
                    strip_prefix_word_ci(f1_raw, "a").or_else(|| strip_prefix_word_ci(f1_raw, "an"))
            {
                let cp = ctx.interner.checkpoint();
                let opts = FilterOpts {
                    default_zone: ZoneMask::BATTLEFIELD,
                    allow_plural_and_disjunction: true,
                    ..Default::default()
                };
                if let (Some(mut f1), Some(mut f2)) = (
                    parse_multi_or_object_phrase(f1_inner, ctx, opts),
                    parse_multi_or_object_phrase(f2_raw, ctx, opts),
                ) {
                    f1.controller = Some(RelativeControl::You);
                    f1.zones = ZoneMask::BATTLEFIELD;
                    f2.controller = Some(RelativeControl::You);
                    f2.zones = ZoneMask::BATTLEFIELD;
                    return Some(Condition::And {
                        of: vec![
                            Condition::Compare {
                                left: Box::new(Amount::Count { of: Box::new(f1) }),
                                op: CompareOp::Ge,
                                right: const_amount(1),
                            },
                            Condition::Compare {
                                left: Box::new(Amount::Count { of: Box::new(f2) }),
                                op: CompareOp::Ge,
                                right: const_amount(1),
                            },
                        ],
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    parse_p15_activated_if_clause(s, ctx)
}

fn comma_ws_prefix<'a>(s: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let after_pref = strip_prefix_ci(s, prefix)?;
    let comma_rel = after_pref.find(',')?;
    let after_comma = &after_pref[comma_rel + 1..];
    let after_ws = after_comma.trim_start();
    let consumed = prefix.len() + comma_rel + 1 + (after_comma.len() - after_ws.len());
    Some((consumed, &after_pref[..comma_rel]))
}

/// Specialized P12 trigger header parser (Class level, Case solved, `becomes renowned`,
/// Room doors/Eerie, Meld step triggers, Kamigawa Flip end-step triggers).
pub fn parse_p12_trigger_header(
    body: &str,
    offset: u32,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggerParse> {
    if !(starts_with_word_ci(body, "when")
        || starts_with_word_ci(body, "whenever")
        || starts_with_word_ci(body, "at"))
    {
        return None;
    }

    // 1. Class level: "when ~ becomes level <N>, "
    if let Some((consumed, lvl_str)) = comma_ws_prefix(body, "when ~ becomes level ") {
        let lvl_trimmed = lvl_str.trim();
        if !lvl_trimmed.is_empty()
            && lvl_trimmed.bytes().all(|b| b.is_ascii_digit())
            && let Ok(lvl) = lvl_trimmed.parse::<u8>()
        {
            return Some(TriggerParse {
                trigger: Some(make_trigger(
                    EventKind::ClassLevelGained,
                    EventFilterBuilder {
                        object: Some(TriggerSubject::SelfObj),
                        class_level: Some(lvl),
                        ..Default::default()
                    },
                )),
                condition: None,
                effect_start: offset + consumed as u32,
                unparsed: Vec::new(),
            });
        }
    }

    // Helper for exact prefix + ",\s*"
    let exact_comma_head = |prefix: &str| -> Option<usize> {
        let after_pref = strip_prefix_ci(body, prefix)?;
        let after_comma = after_pref.strip_prefix(',')?;
        let ws_len = after_comma.len() - after_comma.trim_start().len();
        Some(prefix.len() + 1 + ws_len)
    };

    if let Some(consumed) = exact_comma_head("when ~ becomes solved") {
        return Some(TriggerParse {
            trigger: Some(make_trigger(
                EventKind::CaseSolved,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    ..Default::default()
                },
            )),
            condition: None,
            effect_start: offset + consumed as u32,
            unparsed: Vec::new(),
        });
    }
    if let Some(consumed) = exact_comma_head("whenever you solve a case") {
        return Some(TriggerParse {
            trigger: Some(make_trigger(
                EventKind::CaseSolved,
                EventFilterBuilder {
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    ..Default::default()
                },
            )),
            condition: None,
            effect_start: offset + consumed as u32,
            unparsed: Vec::new(),
        });
    }
    if let Some(consumed) = exact_comma_head("when ~ becomes renowned") {
        return Some(TriggerParse {
            trigger: Some(make_trigger(
                EventKind::BecameRenowned,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    ..Default::default()
                },
            )),
            condition: None,
            effect_start: offset + consumed as u32,
            unparsed: Vec::new(),
        });
    }
    if let Some(consumed) = exact_comma_head("when you unlock this door") {
        return Some(TriggerParse {
            trigger: Some(make_trigger(
                EventKind::DoorUnlocked,
                EventFilterBuilder {
                    object: Some(TriggerSubject::SelfObj),
                    ..Default::default()
                },
            )),
            condition: None,
            effect_start: offset + consumed as u32,
            unparsed: Vec::new(),
        });
    }
    for pref in [
        "whenever you unlock a door of a room you control",
        "whenever you unlock a door of a room",
        "whenever you unlock a door",
    ] {
        if let Some(consumed) = exact_comma_head(pref) {
            return Some(TriggerParse {
                trigger: Some(make_trigger(
                    EventKind::DoorUnlocked,
                    EventFilterBuilder {
                        player: Some(PlayerFilter {
                            relation: PlayerRelation::You,
                        }),
                        ..Default::default()
                    },
                )),
                condition: None,
                effect_start: offset + consumed as u32,
                unparsed: Vec::new(),
            });
        }
    }
    if let Some(consumed) = exact_comma_head("whenever you fully unlock a room") {
        return Some(TriggerParse {
            trigger: Some(make_trigger(
                EventKind::RoomFullyUnlocked,
                EventFilterBuilder {
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    ..Default::default()
                },
            )),
            condition: None,
            effect_start: offset + consumed as u32,
            unparsed: Vec::new(),
        });
    }

    for (pref, is_self_or_other) in [
        (
            "whenever ~ or another enchantment you control enters and whenever you fully unlock a room",
            true,
        ),
        (
            "whenever an enchantment you control enters and whenever you fully unlock a room",
            false,
        ),
        (
            "whenever a enchantment you control enters and whenever you fully unlock a room",
            false,
        ),
    ] {
        if let Some(consumed) = exact_comma_head(pref) {
            let mut trig = make_trigger(
                EventKind::ZoneChange,
                EventFilterBuilder {
                    object: Some(TriggerSubject::Filter {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::ENCHANTMENT,
                            controller: Some(RelativeControl::You),
                            flags: if is_self_or_other {
                                FilterFlags::OTHER
                            } else {
                                FilterFlags::empty()
                            },
                            ..Default::default()
                        }),
                    }),
                    or_self: is_self_or_other,
                    to: Some(Zone::Battlefield),
                    ..Default::default()
                },
            );
            trig.or.push(make_trigger(
                EventKind::RoomFullyUnlocked,
                EventFilterBuilder {
                    player: Some(PlayerFilter {
                        relation: PlayerRelation::You,
                    }),
                    ..Default::default()
                },
            ));
            return Some(TriggerParse {
                trigger: Some(trig),
                condition: None,
                effect_start: offset + consumed as u32,
                unparsed: Vec::new(),
            });
        }
    }

    // Meld step triggers
    if let Some(after_at) = strip_prefix_ci(body, "at the beginning of ") {
        let step_match = [
            ("combat on your turn", GamePhase::BeginCombat),
            ("your upkeep", GamePhase::Upkeep),
            ("your end step", GamePhase::EndStep),
            ("your first main phase", GamePhase::Main1),
            ("your precombat main phase", GamePhase::Main1),
        ]
        .iter()
        .find_map(|(step_s, phase)| {
            strip_prefix_ci(after_at, step_s).and_then(|after_step| {
                let after_comma = after_step.strip_prefix(',')?;
                let after_ws = after_comma.trim_start();
                let prefix_len = "at the beginning of ".len()
                    + step_s.len()
                    + 1
                    + (after_comma.len() - after_ws.len());
                Some((*phase, prefix_len, after_ws))
            })
        });

        if let Some((phase, before_if_offset, after_ws)) = step_match
            && let Some(after_if) = strip_prefix_word_ci(after_ws, "if")
        {
            let meld_marker = "you both own and control ~ and ";
            if let Some((extra_raw, after_own)) =
                split_once_ws_word_ci(after_if, "and you both own and control ~ and")
                    .map(|(l, r)| (Some(l.trim()), r))
                    .or_else(|| {
                        strip_prefix_ci(after_if, meld_marker).map(|r| (None, r.trim_start()))
                    })
                && let Some(after_a) = strip_prefix_word_ci(after_own, "a")
                    .or_else(|| strip_prefix_word_ci(after_own, "an"))
                && let Some((type_phrase, after_named)) = split_once_ws_word_ci(after_a, "named")
                && let Some(comma_rel) = after_named.find(',')
            {
                let partner_name = after_named[..comma_rel].trim();
                let after_partner_comma = &after_named[comma_rel + 1..];
                let effect_tail = after_partner_comma.trim_start();
                if starts_with_word_ci(effect_tail, "exile them")
                    || starts_with_word_ci(effect_tail, "you may pay")
                    || starts_with_word_ci(effect_tail, "then meld them")
                {
                    let effect_start_abs = body.len() - effect_tail.len();
                    let _ = before_if_offset;
                    let cp = ctx.interner.checkpoint();
                    let mut partner_filter = parse_multi_or_object_phrase(
                        type_phrase,
                        ctx,
                        FilterOpts {
                            default_zone: ZoneMask::BATTLEFIELD,
                            allow_plural_and_disjunction: true,
                            ..Default::default()
                        },
                    )
                    .unwrap_or_default();
                    partner_filter.zones = ZoneMask::BATTLEFIELD;
                    partner_filter.controller = Some(RelativeControl::You);
                    partner_filter.flags |= FilterFlags::OTHER;
                    partner_filter.extra.push(ObjectPredicate::Named {
                        name: partner_name.to_string(),
                    });
                    let partner_cond = Condition::Compare {
                        left: Box::new(Amount::Count {
                            of: Box::new(partner_filter),
                        }),
                        op: CompareOp::Ge,
                        right: const_amount(1),
                    };
                    let cond = match extra_raw {
                        Some(ec) => parse_p18b_intervening_condition_clause(ec, ctx).map(|c| {
                            Condition::And {
                                of: vec![c, partner_cond],
                            }
                        }),
                        None => Some(partner_cond),
                    };
                    if let Some(condition) = cond {
                        let mut trig = make_trigger(
                            EventKind::PhaseBegin,
                            EventFilterBuilder {
                                phase: Some(phase),
                                ..Default::default()
                            },
                        );
                        trig.scope = Some(TriggerScope::ControllerTurn);
                        return Some(TriggerParse {
                            trigger: Some(trig),
                            condition: Some(condition),
                            effect_start: offset + effect_start_abs as u32,
                            unparsed: Vec::new(),
                        });
                    }
                    ctx.interner.rollback(cp);
                }
            }
        }
    }

    // Kamigawa Flip end-step triggers
    if let Some(after_at) = strip_prefix_ci(body, "at the beginning of ") {
        for (scope_w, scope) in [
            ("the end step,", TriggerScope::EachTurn),
            ("each end step,", TriggerScope::EachTurn),
            ("your end step,", TriggerScope::ControllerTurn),
        ] {
            if let Some(after_step) = strip_prefix_ci(after_at, scope_w) {
                let after_ws = after_step.trim_start();
                if let Some(first_comma_rel) = after_ws.find(',') {
                    let cond_clause = after_ws[..first_comma_rel].trim();
                    let tail = &after_ws[first_comma_rel + 1..];
                    let tail_trimmed = tail.trim_start();
                    let effect_start_abs = body.len() - tail_trimmed.len();

                    // flipCounterEndStep
                    if let Some(after_there) = strip_prefix_word_ci(cond_clause, "if there is")
                        .or_else(|| strip_prefix_word_ci(cond_clause, "if there are"))
                        && let Some((num_w, after_num)) =
                            after_there.split_once(char::is_whitespace)
                        && let Some(n) = parse_small_i32(num_w)
                        && let Some(ctr_raw) =
                            strip_prefix_word_ci(after_num.trim_start(), "or more").and_then(|r| {
                                strip_suffix_ws_ci(r, "counters on ~")
                                    .or_else(|| strip_suffix_ws_ci(r, "counter on ~"))
                            })
                    {
                        let cp = ctx.interner.checkpoint();
                        if let Some(ctr) = parse_trigger_counter(ctr_raw, ctx.interner) {
                            let mut trig = make_trigger(
                                EventKind::PhaseBegin,
                                EventFilterBuilder {
                                    phase: Some(GamePhase::EndStep),
                                    ..Default::default()
                                },
                            );
                            trig.scope = Some(scope);
                            return Some(TriggerParse {
                                trigger: Some(trig),
                                condition: Some(Condition::Compare {
                                    left: Box::new(Amount::CountersOn {
                                        counter: CounterTarget::Specific(ctr),
                                        of: mtg_ir::Ref::SelfObj,
                                    }),
                                    op: CompareOp::Ge,
                                    right: const_amount(n),
                                }),
                                effect_start: offset + effect_start_abs as u32,
                                unparsed: Vec::new(),
                            });
                        }
                        ctx.interner.rollback(cp);
                    }

                    // flipDiedEndStep
                    if let Some(num_w) = strip_prefix_word_ci(cond_clause, "if")
                        .and_then(|r| strip_suffix_ws_ci(r, "or more creatures died this turn"))
                        && let Some(n) = parse_small_i32(num_w)
                    {
                        let mut trig = make_trigger(
                            EventKind::PhaseBegin,
                            EventFilterBuilder {
                                phase: Some(GamePhase::EndStep),
                                ..Default::default()
                            },
                        );
                        trig.scope = Some(scope);
                        return Some(TriggerParse {
                            trigger: Some(trig),
                            condition: Some(Condition::Compare {
                                left: Box::new(Amount::GravestormCount),
                                op: CompareOp::Ge,
                                right: const_amount(n),
                            }),
                            effect_start: offset + effect_start_abs as u32,
                            unparsed: Vec::new(),
                        });
                    }

                    // flipEnchantedEndStep
                    if let Some(num_w) = strip_prefix_word_ci(cond_clause, "if ~ is enchanted by")
                        .and_then(|r| strip_suffix_ws_ci(r, "or more auras"))
                        && let Some(n) = parse_small_i32(num_w)
                    {
                        let aura_id = ctx.interner.intern_subtype("Aura");
                        let mut aura_filter = ObjectFilter::default();
                        aura_filter.subtypes_any.push(aura_id);
                        let mut trig = make_trigger(
                            EventKind::PhaseBegin,
                            EventFilterBuilder {
                                phase: Some(GamePhase::EndStep),
                                ..Default::default()
                            },
                        );
                        trig.scope = Some(scope);
                        return Some(TriggerParse {
                            trigger: Some(trig),
                            condition: Some(Condition::Compare {
                                left: Box::new(Amount::AttachmentsOn {
                                    target: AttachmentOwnerTarget::SelfObj,
                                    filter: Some(Box::new(aura_filter)),
                                }),
                                op: CompareOp::Ge,
                                right: const_amount(n),
                            }),
                            effect_start: offset + effect_start_abs as u32,
                            unparsed: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    None
}

// -----------------------------------------------------------------------------
// Runtime parity gate (matching TS runtimeTriggerProblem)
// -----------------------------------------------------------------------------

fn is_runtime_phase(p: GamePhase) -> bool {
    matches!(
        p,
        GamePhase::Upkeep
            | GamePhase::Draw
            | GamePhase::Main1
            | GamePhase::BeginCombat
            | GamePhase::EndCombat
            | GamePhase::Main2
            | GamePhase::EndStep
    )
}

fn is_attached_supported_event(ev: EventKind, f: &EventFilter) -> bool {
    if matches!(
        ev,
        EventKind::Attacks
            | EventKind::Blocks
            | EventKind::BecomesBlocked
            | EventKind::DealsDamage
            | EventKind::DealtDamage
            | EventKind::BecomesTarget
            | EventKind::TappedForMana
            | EventKind::BecomesTapped
            | EventKind::AttackedAndUnblocked
            | EventKind::BecomesUntapped
            | EventKind::BlocksOrBecomesBlocked
            | EventKind::Transforms
            | EventKind::TurnFaceUp
            | EventKind::Mutates
            | EventKind::PhasedIn
            | EventKind::CounterPlaced
            | EventKind::SaddledMount
            | EventKind::CrewedVehicle
            | EventKind::AbilityActivated
    ) {
        return true;
    }
    ev == EventKind::ZoneChange
        && matches!(
            f.detail,
            Some(EventDetail::ZoneChange { from, .. }) if from == ZoneMask::BATTLEFIELD
        )
}

/// Runtime parity gate matching TS `runtimeTriggerProblem`:
/// Returns `Some(reason)` if the runtime cannot honour this trigger accurately.
pub fn runtime_trigger_problem(def: &TriggerDef) -> Option<&'static str> {
    let ev = def.event;
    if matches!(
        ev,
        EventKind::BecomesBlockedBy
            | EventKind::ClassLevelGained
            | EventKind::CaseSolved
            | EventKind::BecameRenowned
            | EventKind::DrawReplacement
            | EventKind::DamageReplacement
            | EventKind::LifeGainReplacement
            | EventKind::ProliferateReplacement
    ) {
        return Some("event is not emitted");
    }

    let f = &def.filter;
    if f.object.is_some()
        && matches!(
            ev,
            EventKind::PhaseBegin
                | EventKind::GainLife
                | EventKind::DrawCard
                | EventKind::Investigate
                | EventKind::Surveil
                | EventKind::BecameMonarch
                | EventKind::Amass
                | EventKind::ManifestDread
                | EventKind::Scry
                | EventKind::CommittedCrime
                | EventKind::Expended
                | EventKind::Discover
                | EventKind::Forage
                | EventKind::RingTempted
                | EventKind::DungeonRoomEntered
                | EventKind::DungeonCompleted
                | EventKind::TookInitiative
                | EventKind::StateTriggered
                | EventKind::DayNightChanged
                | EventKind::FacedDilemma
                | EventKind::CollectEvidence
                | EventKind::LibrarySearched
                | EventKind::PlayerLostGame
                | EventKind::CoinFlipped
                | EventKind::DieRolled
                | EventKind::EnergyGained
                | EventKind::Proliferated
                | EventKind::PlayersFinishedVoting
                | EventKind::Clash
                | EventKind::LibraryShuffled
                | EventKind::GiftGiven
        )
    {
        return Some("unsupported event filter field \"object\"");
    }

    if f.or_self
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Attacks
                | EventKind::Blocks
                | EventKind::BecomesBlocked
                | EventKind::DealsDamage
                | EventKind::DealtDamage
                | EventKind::CounterPlaced
                | EventKind::BecomesTarget
                | EventKind::BecomesTapped
                | EventKind::Transforms
                | EventKind::TurnFaceUp
                | EventKind::Explore
                | EventKind::Connive
                | EventKind::BecomesCrewed
                | EventKind::BecomesSaddled
                | EventKind::Exert
                | EventKind::BecameMonstrous
                | EventKind::AttackedAndUnblocked
                | EventKind::BecomesUntapped
                | EventKind::Exploited
                | EventKind::Sacrificed
                | EventKind::BlocksOrBecomesBlocked
                | EventKind::Unattached
                | EventKind::PhasedIn
                | EventKind::Mutates
                | EventKind::Evolved
                | EventKind::CrewedVehicle
                | EventKind::AbilityActivated
                | EventKind::SagaChapterResolved
                | EventKind::SaddledMount
        )
    {
        return Some("unsupported event filter field \"orSelf\"");
    }

    if f.or_attached_to_self
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Attacks
                | EventKind::Blocks
                | EventKind::BecomesBlocked
                | EventKind::DealsDamage
                | EventKind::DealtDamage
                | EventKind::CounterPlaced
                | EventKind::BecomesTarget
                | EventKind::BecomesTapped
                | EventKind::Transforms
                | EventKind::TurnFaceUp
                | EventKind::AttackedAndUnblocked
                | EventKind::BecomesUntapped
                | EventKind::Sacrificed
                | EventKind::BlocksOrBecomesBlocked
                | EventKind::Unattached
                | EventKind::PhasedIn
                | EventKind::Mutates
        )
    {
        return Some("unsupported event filter field \"orAttachedToSelf\"");
    }

    if f.or_commander && ev != EventKind::Attacks {
        return Some("unsupported event filter field \"orCommander\"");
    }

    if f.player.is_some()
        && matches!(
            ev,
            EventKind::BecomesBlocked
                | EventKind::PhaseBegin
                | EventKind::CounterPlaced
                | EventKind::Attached
                | EventKind::BecomesTapped
                | EventKind::Transforms
                | EventKind::Explore
                | EventKind::Connive
                | EventKind::BecomesCrewed
                | EventKind::BecomesSaddled
                | EventKind::BecameMonstrous
                | EventKind::CounterRemoved
                | EventKind::BecomesUntapped
                | EventKind::Exploited
                | EventKind::BlocksOrBecomesBlocked
                | EventKind::StateTriggered
                | EventKind::DayNightChanged
                | EventKind::Unattached
                | EventKind::PhasedIn
                | EventKind::SoulbondPaired
                | EventKind::HauntedCreatureDies
                | EventKind::Evolved
                | EventKind::CrewedVehicle
                | EventKind::SagaChapterResolved
                | EventKind::SaddledMount
                | EventKind::AttackTriggered
        )
    {
        return Some("unsupported event filter field \"player\"");
    }

    if f.one_or_more
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Attacks
                | EventKind::CombatDamageBatchPlayer
                | EventKind::Blocks
                | EventKind::CounterPlaced
                | EventKind::BecomesTarget
                | EventKind::Discard
                | EventKind::AttackedAndUnblocked
                | EventKind::Sacrificed
                | EventKind::GraveyardLeaveBatch
                | EventKind::LoseLife
                | EventKind::CoinFlipped
                | EventKind::DieRolled
        )
    {
        return Some("unsupported event filter field \"oneOrMore\"");
    }

    if f.during_turn.is_some()
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Cast
                | EventKind::GainLife
                | EventKind::CounterPlaced
                | EventKind::Cycle
                | EventKind::Discard
                | EventKind::Surveil
                | EventKind::BecameMonarch
                | EventKind::ManifestDread
                | EventKind::Scry
                | EventKind::CommittedCrime
                | EventKind::Sacrificed
                | EventKind::GraveyardLeaveBatch
                | EventKind::Discover
                | EventKind::Forage
                | EventKind::LoseLife
                | EventKind::LandPlayed
                | EventKind::CrewedVehicle
                | EventKind::SaddledMount
        )
    {
        return Some("unsupported event filter field \"duringTurn\"");
    }

    if f.during_step.is_some() && ev != EventKind::ZoneChange {
        return Some("unsupported event filter field \"duringStep\"");
    }
    if f.during_combat && ev != EventKind::ZoneChange {
        return Some("unsupported event filter field \"duringCombat\"");
    }
    if f.during_main_phase && !matches!(ev, EventKind::CrewedVehicle | EventKind::SaddledMount) {
        return Some("unsupported event filter field \"duringPhase\"");
    }
    if f.first_time_each_turn
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Attacks
                | EventKind::BecomesTarget
                | EventKind::GainLife
                | EventKind::BecomesCrewed
                | EventKind::LoseLife
        )
    {
        return Some("unsupported event filter field \"firstTimeEachTurn\"");
    }
    if f.nth_this_turn.is_some() && !matches!(ev, EventKind::Cast | EventKind::DrawCard) {
        return Some("unsupported event filter field \"nthThisTurn\"");
    }
    if f.nth_event_this_turn.is_some()
        && !matches!(
            ev,
            EventKind::Attacks | EventKind::GainLife | EventKind::LoseLife
        )
    {
        return Some("unsupported event filter field \"nthEventThisTurn\"");
    }
    if f.attached_filter.is_some()
        && !matches!(
            ev,
            EventKind::ZoneChange
                | EventKind::Attacks
                | EventKind::Blocks
                | EventKind::BecomesBlocked
                | EventKind::DealsDamage
                | EventKind::DealtDamage
                | EventKind::PhaseBegin
                | EventKind::BecomesTarget
                | EventKind::TappedForMana
                | EventKind::BecomesTapped
                | EventKind::Transforms
                | EventKind::TurnFaceUp
                | EventKind::AttackedAndUnblocked
                | EventKind::BecomesUntapped
                | EventKind::BlocksOrBecomesBlocked
        )
    {
        return Some("unsupported event filter field \"attachedFilter\"");
    }
    if f.watched_bound_object && ev != EventKind::DealsDamage {
        return Some("unsupported event filter field \"watchedBoundObject\"");
    }

    if let Some(TriggerSubject::Filter { filter }) = &f.object
        && let Some(p) = object_filter_problem(filter)
    {
        return Some(p);
    }
    if let Some(af) = &f.attached_filter
        && let Some(p) = object_filter_problem(af)
    {
        return Some(p);
    }

    if let Some(detail) = &f.detail {
        match detail {
            EventDetail::ZoneChange { .. } => {
                if ev != EventKind::ZoneChange {
                    return Some("mismatched ZoneChange detail");
                }
            }
            EventDetail::Cast { .. } => {
                if ev != EventKind::Cast {
                    return Some("mismatched Cast detail");
                }
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
                per_attacked_player,
                attacking_with_filter,
                min_attacking_with_count,
                attacking_total_power_min,
                attack_enchanted_or_attacked_by_enchanted,
                attacking_unblocked,
            } => {
                if *dethrone_most_life || *training_greater_attacker || *without_flanking {
                    return Some("unsupported combat detail field");
                }
                if let Some(baf) = blocked_attacker_filter {
                    if ev != EventKind::Blocks {
                        return Some("unsupported blockedAttackerFilter");
                    }
                    if let Some(p) = object_filter_problem(baf) {
                        return Some(p);
                    }
                }
                if let Some(h) = host {
                    if !matches!(
                        ev,
                        EventKind::Blocks
                            | EventKind::BecomesBlocked
                            | EventKind::BlocksOrBecomesBlocked
                    ) {
                        return Some("unsupported host");
                    }
                    if let Some(p) = object_filter_problem(h) {
                        return Some(p);
                    }
                }
                if let Some(caf) = co_attacker_filter {
                    if ev != EventKind::Attacks {
                        return Some("unsupported coAttackerFilter");
                    }
                    if let Some(p) = object_filter_problem(caf) {
                        return Some(p);
                    }
                }
                if let Some(awf) = attacking_with_filter {
                    if ev != EventKind::Attacks {
                        return Some("unsupported attackingWithFilter");
                    }
                    if let Some(p) = object_filter_problem(awf) {
                        return Some(p);
                    }
                }
                if (*alone
                    || *saddled
                    || min_attackers.is_some()
                    || co_attacker_min.is_some()
                    || *per_attacked_player
                    || min_attacking_with_count.is_some()
                    || attacking_total_power_min.is_some()
                    || *attack_enchanted_or_attacked_by_enchanted)
                    && ev != EventKind::Attacks
                {
                    return Some("unsupported combat detail field");
                }
                if *attacking_unblocked && ev != EventKind::AttackedAndUnblocked {
                    return Some("unsupported attackingUnblocked");
                }
                if attacked_target.is_some()
                    && !matches!(ev, EventKind::Attacks | EventKind::AttackedAndUnblocked)
                {
                    return Some("unsupported attackedTarget");
                }
            }
            EventDetail::LandPlayed { .. } => {
                if ev != EventKind::LandPlayed {
                    return Some("mismatched LandPlayed detail");
                }
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
                if damage_player_relation.is_some() {
                    return Some("unsupported damagePlayerRelation");
                }
                if (*combat).is_some()
                    && !matches!(ev, EventKind::DealsDamage | EventKind::DealtDamage)
                {
                    return Some("unsupported combat damage detail");
                }
                if *noncombat_only && !matches!(ev, EventKind::DealsDamage | EventKind::DealtDamage)
                {
                    return Some("unsupported noncombatOnly");
                }
                if (damage_to.is_some()
                    || damage_target_kind.is_some()
                    || damage_target_filter.is_some()
                    || *damage_target_enchanted_player)
                    && ev != EventKind::DealsDamage
                {
                    return Some("unsupported dealsDamage detail field");
                }
                if let Some(dtf) = damage_target_filter
                    && let Some(p) = object_filter_problem(dtf)
                {
                    return Some(p);
                }
                if (damaged_player.is_some() || single_source_damage_min.is_some())
                    && ev != EventKind::DealtDamage
                {
                    return Some("unsupported dealtDamage detail field");
                }
                if combat_damage_batch_player.is_some() && ev != EventKind::CombatDamageBatchPlayer
                {
                    return Some("unsupported combatDamageBatchPlayer");
                }
            }
            EventDetail::Phase { .. } => {
                if ev != EventKind::PhaseBegin {
                    return Some("mismatched Phase detail");
                }
            }
            EventDetail::Counter {
                put_by_player,
                nth_counter_on_self,
                ..
            } => {
                if !matches!(ev, EventKind::CounterPlaced | EventKind::CounterRemoved) {
                    return Some("mismatched Counter detail");
                }
                if (put_by_player.is_some() || nth_counter_on_self.is_some())
                    && ev != EventKind::CounterPlaced
                {
                    return Some("unsupported counter detail on counterRemoved");
                }
            }
            EventDetail::BecomesTarget {
                targeting_source_filter,
                ..
            } => {
                if ev != EventKind::BecomesTarget {
                    return Some("mismatched BecomesTarget detail");
                }
                if let Some(tsf) = targeting_source_filter
                    && let Some(p) = object_filter_problem(tsf)
                {
                    return Some(p);
                }
            }
            EventDetail::Attach { host, .. } => {
                if ev != EventKind::Attached {
                    return Some("unsupported attach detail");
                }
                if let Some(h) = host
                    && let Some(p) = object_filter_problem(h)
                {
                    return Some(p);
                }
            }
            EventDetail::Mana { .. } => {
                if !matches!(
                    ev,
                    EventKind::TappedForMana | EventKind::ManaAddedByLandAbility
                ) {
                    return Some("mismatched Mana detail");
                }
            }
            EventDetail::AbilityActivated { ability_source, .. } => {
                if ev != EventKind::AbilityActivated {
                    return Some("mismatched AbilityActivated detail");
                }
                if let Some(TriggerSubject::Filter { filter }) = ability_source
                    && let Some(p) = object_filter_problem(filter)
                {
                    return Some(p);
                }
            }
            EventDetail::Discard { madness_exiled, .. } => {
                if ev != EventKind::Discard || *madness_exiled {
                    return Some("unsupported discard detail");
                }
            }
            EventDetail::Draw { .. } => {
                if ev != EventKind::DrawCard {
                    return Some("mismatched Draw detail");
                }
            }
            EventDetail::Transform { to_face_filter } => {
                if ev != EventKind::Transforms {
                    return Some("mismatched Transform detail");
                }
                if let Some(tff) = to_face_filter
                    && let Some(p) = object_filter_problem(tff)
                {
                    return Some(p);
                }
            }
            EventDetail::Tapped { .. } => {
                if ev != EventKind::BecomesTapped {
                    return Some("mismatched Tapped detail");
                }
            }
            EventDetail::DieOrCoin {
                die_roll_min,
                die_roll_values,
                won_coin_flip,
            } => {
                if (*die_roll_min).is_some() || !die_roll_values.is_empty() {
                    if ev != EventKind::DieRolled {
                        return Some("unsupported die roll detail");
                    }
                } else if *won_coin_flip && ev != EventKind::CoinFlipped {
                    return Some("unsupported coin flip detail");
                }
            }
            EventDetail::State { .. } => {
                if ev != EventKind::StateTriggered {
                    return Some("mismatched State detail");
                }
            }
            EventDetail::Misc {
                amass_subtype,
                expend_threshold,
                class_level,
                this_door_face_index,
                day_night_direction,
                final_chapter,
                won,
            } => {
                if amass_subtype.is_some() && ev != EventKind::Amass {
                    return Some("unsupported amassSubtype");
                }
                if expend_threshold.is_some() && ev != EventKind::Expended {
                    return Some("unsupported expendThreshold");
                }
                if class_level.is_some() {
                    return Some("unsupported classLevel");
                }
                if this_door_face_index.is_some() && ev != EventKind::DoorUnlocked {
                    return Some("unsupported thisDoorFaceIndex");
                }
                if day_night_direction.is_some() && ev != EventKind::DayNightChanged {
                    return Some("unsupported dayNightDirection");
                }
                if *final_chapter && ev != EventKind::SagaChapterResolved {
                    return Some("unsupported finalChapter");
                }
                if won.is_some() && ev != EventKind::Clash {
                    return Some("unsupported won");
                }
            }
        }
    }

    if f.object == Some(TriggerSubject::AttachedTo) && !is_attached_supported_event(ev, f) {
        return Some("event of the attached-to object");
    }

    if ev == EventKind::PhaseBegin {
        match &f.detail {
            Some(EventDetail::Phase { phase: Some(p), .. }) if is_runtime_phase(*p) => {}
            _ => return Some("unsupported phase"),
        }
    }

    if matches!(ev, EventKind::BecomesTarget | EventKind::DealsDamage)
        && let Some(p) = &f.player
        && !matches!(
            p.relation,
            PlayerRelation::You | PlayerRelation::Opponent | PlayerRelation::Any
        )
    {
        return Some("unsupported player relation");
    }

    if ev == EventKind::Cast
        && let Some(p) = &f.player
        && !matches!(
            p.relation,
            PlayerRelation::You | PlayerRelation::Opponent | PlayerRelation::Any
        )
    {
        return Some("unsupported caster relation");
    }

    if ev == EventKind::Attacks
        && let Some(p) = &f.player
    {
        let ok = if f.object.is_some() {
            p.relation == PlayerRelation::You
        } else {
            matches!(
                p.relation,
                PlayerRelation::You | PlayerRelation::Opponent | PlayerRelation::Any
            )
        };
        if !ok {
            return Some("attacking player");
        }
    }

    if ev == EventKind::CombatDamageBatchPlayer {
        let ok = f.one_or_more
            && f.player.as_ref().is_some_and(|p| {
                matches!(
                    p.relation,
                    PlayerRelation::You | PlayerRelation::Opponent | PlayerRelation::Any
                )
            });
        if !ok {
            return Some("combatDamageBatchPlayer filter");
        }
    }

    if let Some(fz) = def.from_zones {
        let ok = fz == ZoneMask::GRAVEYARD
            || fz == ZoneMask::EXILE
            || (fz == ZoneMask::ALL && matches!(ev, EventKind::Cycle | EventKind::Discard));
        if !ok {
            return Some("functions from unsupported zone");
        }
    }

    for alt in &def.or {
        if let Some(p) = runtime_trigger_problem(alt) {
            return Some(p);
        }
    }

    None
}
