//! Continuous static abilities and replacement effect compilation
//! (Milestone M3 — zero-regex TokenCursor/slice implementation).

use std::borrow::Cow;

use smallvec::{SmallVec, smallvec};

use mtg_ir::{
    AbilityDef, AbilityGrantSource, ActivatedAbility, ActivationRestriction, Affected, Amount,
    AttachmentOwnerTarget, CardTypeMask, ColorMask, CombatRuleSpec, CompareOp, Condition,
    ConditionalTypeCounter, CopyModificationSpec, CostDef, CostDelta, CostDeltaMode, CounterId,
    CounterTarget, CrewSaddleScope, DamageModMode, DamageModTarget, DamageScaleSpec, DrawScaleSpec,
    EffectOp, EffectStep, EtbChoiceSpec, EtbCopySpec, EventDetail, EventFilter, EventKind,
    EventPattern, FilterFlags, KeywordDef, KeywordId, Layer, LayerMod, LifeCompareWho,
    LifeGainScaleSpec, LoyaltySign, ManaCost, ManaSourceKind, MultiplierTarget, ObjectFilter,
    ObjectPredicate, OptionalMode, OptionalSpec, PhantomCounterRemoval, PlayerFilter, PlayerRef,
    PlayerRelation, ProtectionAttachmentExemption, Ref, RelativeControl, ReplacementDef,
    ReplacementWith, RestrictionKind, RuleMod, SelectionMod, Selector, Span,
    SpellCastRestrictionKind, SpellOrAbilityFilter, StackTargetKind, StatKind, StaticBody,
    StaticDamageAmount, StaticDamageTarget, StaticDef, StaticOnPrevented, SubTypeChoiceScope,
    SubtypeId, SupertypeMask, TargetKind, TargetSpec, Timing, TokenSpec, TriggerSubject,
    TriggeredAbility, Zone, ZoneChangeMod, ZoneMask, ZoneScopeWhose,
};

use crate::lexer::tokenize;
use crate::normalize::split_sentences;
use crate::parser::cost::{
    is_cast_payable_cost, is_spell_alt_cost_compatible, lookup_counter_id,
    parse_additional_cost_sentence, parse_cost_list, parse_cost_modifier_sentence,
};
use crate::parser::effect::{parse_effects, parse_static_extra_token, statically_interpretable};
use crate::parser::filter::{
    FilterOpts, NullScope, OUTLAW_SUBTYPES, ParserScope, PronounSubject, is_known_card_type_word,
    is_known_supertype_word, parse_color_word, parse_object_phrase, parse_target_spec,
    singular_subtype,
};
use crate::parser::keyword::{get_keyword_spec, is_known_keyword, make_empty_kw_def};
use crate::parser::trigger::{
    ActivatedExtraRestriction, parse_activated_if_condition_sentence, parse_trigger_header,
    runtime_trigger_problem,
};
use crate::parser::value::{
    parse_amount, parse_condition, parse_count_word, parse_devotion_color_list, parse_small_number,
};
use crate::parser::{AbilityContext, ParseCtx, strip_leading_ability_word};

/// Single compiled item produced from a static/replacement ability line.
#[derive(Debug, Clone, PartialEq)]
pub enum StaticItem {
    Static(StaticDef),
    Replacement(ReplacementDef),
}

/// Result of parsing a static or replacement ability block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StaticParse {
    pub items: Vec<StaticItem>,
    pub unparsed: Vec<Span>,
}

/// Parses a static/replacement ability line or multi-sentence block (`offset` is the byte
/// position of `text` in the caller's normalized text).
pub fn parse_statics(text: &str, offset: u32, ctx: &mut ParseCtx<'_>) -> StaticParse {
    parse_statics_impl(text, offset, ctx)
}

/// Alias entry point for `parse_statics` matching `PROJECT.md` / cross-module naming.
pub fn parse_static_or_replacement(text: &str, offset: u32, ctx: &mut ParseCtx<'_>) -> StaticParse {
    parse_statics(text, offset, ctx)
}

/// Parses a P/T characteristic-defining static ability (`*/*`, `1+*/*`, `*+1/*+1`, etc.)
/// into a Layer 7a `StaticBody::Continuous` definition.
pub fn parse_static_cda_pt(text: &str, ctx: &mut ParseCtx<'_>) -> Option<StaticDef> {
    parse_static_cda_pt_impl(text, ctx)
}

/// Parses spell-level static restrictions or cast modifiers (`"~ can't be countered"`,
/// `"~ can't be copied"`, `"You may cast ~ as though it had flash"`, etc.).
pub fn parse_static_spell_restriction_or_cast_mod(
    text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    parse_static_spell_restriction_or_cast_mod_impl(text, ctx)
}

/// Parses a zone-change replacement clause for spells or ETB riders
/// (e.g., `"If ~ would be put into a graveyard from anywhere, ..."`).
pub fn parse_zone_change_clause_for_spell_or_etb(
    text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ReplacementDef> {
    parse_zone_change_clause_for_spell_or_etb_impl(text, ctx)
}

/// Parses a standalone dynamic keyword grant line (`"Mobilize X, where X is ..."`,
/// `"Firebending X, where X is ..."`) into its dynamic `KeywordDef`.
pub(crate) fn parse_dynamic_keyword_grant_def(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<KeywordDef> {
    parse_dynamic_keyword_grant_def_impl(raw, ctx)
}

/// Parses spell-level alternative cast or conditional flash-tax sentences
/// (`"You may pay ... rather than pay ~'s mana cost"`, `"You may cast ~ as though it had flash if you pay ... more to cast it"`).
pub fn parse_spell_alt_cast_or_flash_sentence(
    sentence: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    parse_spell_alt_cast_or_flash_sentence_impl(sentence, ctx)
}

// ============================================================================
// Section 1: Zero-Regex ASCII & Slice Helpers
// ============================================================================

#[inline]
fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[inline]
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[inline]
fn ends_with_ci(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.is_char_boundary(s.len() - suffix.len())
        && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

#[inline]
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if starts_with_ci(s, prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[inline]
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if ends_with_ci(s, suffix) {
        Some(&s[..s.len() - suffix.len()])
    } else {
        None
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let nb = needle.as_bytes();
    let hb = haystack.as_bytes();
    if nb.len() > hb.len() {
        return None;
    }
    let b0_lo = nb[0].to_ascii_lowercase();
    let b0_hi = nb[0].to_ascii_uppercase();
    let limit = hb.len() - nb.len();
    for i in 0..=limit {
        let b = hb[i];
        if (b == b0_lo || b == b0_hi)
            && hb[i..i + nb.len()].eq_ignore_ascii_case(nb)
            && haystack.is_char_boundary(i)
            && haystack.is_char_boundary(i + nb.len())
        {
            return Some(i);
        }
    }
    None
}

fn split_once_ci<'a>(haystack: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    let idx = find_ci(haystack, sep)?;
    Some((&haystack[..idx], &haystack[idx + sep.len()..]))
}

#[inline]
fn contains_ci(haystack: &str, needle: &str) -> bool {
    find_ci(haystack, needle).is_some()
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'~'
}

fn contains_word_ci(haystack: &str, word: &str) -> bool {
    if word.is_empty() || word.len() > haystack.len() {
        return false;
    }
    let nb = word.as_bytes();
    let hb = haystack.as_bytes();
    let b0_lo = nb[0].to_ascii_lowercase();
    let b0_hi = nb[0].to_ascii_uppercase();
    let limit = hb.len() - nb.len();
    for i in 0..=limit {
        let b = hb[i];
        if (b != b0_lo && b != b0_hi) || !hb[i..i + nb.len()].eq_ignore_ascii_case(nb) {
            continue;
        }
        if !haystack.is_char_boundary(i) || !haystack.is_char_boundary(i + nb.len()) {
            continue;
        }
        let left_ok = i == 0 || !is_word_byte(hb[i - 1]);
        let right_ok = i + nb.len() == hb.len() || !is_word_byte(hb[i + nb.len()]);
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

/// Normalizes curly apostrophes/quotes into ASCII `'` / `"` on a per-sentence local string.
/// Called strictly AFTER `split_sentences(text, offset)` so `Span` byte offsets stay accurate.
fn normalize_quotes(s: &str) -> Cow<'_, str> {
    if !s.bytes().any(|b| b >= 0x80)
        || !s.contains(['\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2212}'])
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2212}' => out.push('-'),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[inline]
fn needs_reminder_or_ws_clean(s: &str) -> bool {
    s.bytes()
        .any(|b| matches!(b, b'(' | b'\n' | b'\r' | b'\t') || b >= 0x80)
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains("  ")
}

fn strip_all_reminders(s: &str) -> Cow<'_, str> {
    if !needs_reminder_or_ws_clean(s) {
        return Cow::Borrowed(s);
    }
    if !s.as_bytes().contains(&b'(') {
        return Cow::Owned(collapse_ws(s));
    }
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth > 0 {
                    depth -= 1;
                } else {
                    out.push(ch);
                }
            }
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    Cow::Owned(collapse_ws(&out))
}

#[inline]
fn normalize_and_strip(s: &str) -> Cow<'_, str> {
    if !needs_reminder_or_ws_clean(s) {
        return Cow::Borrowed(s);
    }
    match normalize_quotes(s) {
        Cow::Borrowed(b) => strip_all_reminders(b),
        Cow::Owned(q) => Cow::Owned(strip_all_reminders(&q).into_owned()),
    }
}

#[inline]
fn trim_sentence(s: &str) -> &str {
    s.trim().trim_end_matches('.').trim()
}

#[inline]
fn trim_clause_punct(s: &str) -> &str {
    s.trim()
        .trim_matches(|c| matches!(c, '.' | ',' | ';'))
        .trim()
}

fn strip_leading_article(s: &str) -> &str {
    let t = s.trim();
    for art in ["the ", "an ", "a "] {
        if let Some(rest) = strip_prefix_ci(t, art) {
            return rest.trim_start();
        }
    }
    t
}

fn strip_self_subject(s: &str) -> Option<&str> {
    let t = s.trim();
    for pat in [
        "~ ",
        "this creature ",
        "this permanent ",
        "this artifact ",
        "this enchantment ",
        "this land ",
        "this planeswalker ",
        "this battle ",
        "this spell ",
        "this card ",
        "this vehicle ",
        "this aura ",
        "this equipment ",
    ] {
        if let Some(rest) = strip_prefix_ci(t, pat) {
            return Some(rest.trim_start());
        }
    }
    None
}

fn is_self_phrase(s: &str) -> bool {
    let t = s.trim();
    match t.as_bytes().first().copied() {
        Some(b'~') => t.len() == 1,
        Some(b'i' | b'I') => eq_ci(t, "it"),
        Some(b't' | b'T') => {
            eq_ci(t, "this")
                || eq_ci(t, "this creature")
                || eq_ci(t, "this permanent")
                || eq_ci(t, "this artifact")
                || eq_ci(t, "this enchantment")
                || eq_ci(t, "this land")
                || eq_ci(t, "this planeswalker")
                || eq_ci(t, "this battle")
                || eq_ci(t, "this spell")
                || eq_ci(t, "this card")
                || eq_ci(t, "this vehicle")
                || eq_ci(t, "this aura")
                || eq_ci(t, "this equipment")
        }
        _ => false,
    }
}

fn strip_self_possessive(s: &str) -> Option<&str> {
    let t = s.trim();
    for pat in [
        "~'s ",
        "its ",
        "this creature's ",
        "this permanent's ",
        "this artifact's ",
        "this enchantment's ",
        "this land's ",
        "this spell's ",
        "this card's ",
    ] {
        if let Some(rest) = strip_prefix_ci(t, pat) {
            return Some(rest.trim_start());
        }
    }
    None
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::with_capacity(s.len());
            for up in first.to_uppercase() {
                out.push(up);
            }
            out.push_str(chars.as_str());
            out
        }
        None => String::new(),
    }
}

fn parse_signed_int(s: &str) -> Option<i32> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (neg, digits) = if let Some(d) = t.strip_prefix('+') {
        (false, d)
    } else if let Some(d) = t.strip_prefix('-') {
        (true, d)
    } else {
        (false, t)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let val: i32 = digits.parse().ok()?;
    Some(if neg { -val } else { val })
}

fn parse_num_word_or_int(s: &str) -> Option<i32> {
    let t = s.trim();
    if let Some(v) = parse_signed_int(t) {
        return Some(v);
    }
    if let Some(n) = parse_small_number(t) {
        return Some(n);
    }
    match parse_count_word(t) {
        Some(Amount::Const { n }) => Some(n),
        _ => None,
    }
}

fn parse_pt_mod_pair(s: &str) -> Option<(i32, i32)> {
    let (p_str, t_str) = s.trim().split_once('/')?;
    if !(p_str.starts_with('+') || p_str.starts_with('-'))
        || !(t_str.starts_with('+') || t_str.starts_with('-'))
    {
        return None;
    }
    Some((parse_signed_int(p_str)?, parse_signed_int(t_str)?))
}

fn parse_fixed_pt_pair(s: &str) -> Option<(i32, i32)> {
    let (p_str, t_str) = s.trim().split_once('/')?;
    if p_str.starts_with('+')
        || p_str.starts_with('-')
        || t_str.starts_with('+')
        || t_str.starts_with('-')
    {
        return None;
    }
    Some((parse_signed_int(p_str)?, parse_signed_int(t_str)?))
}

#[inline]
fn fixed_amt(n: i32) -> Amount {
    Amount::Const { n }
}

fn scale_amount(amt: Amount, factor: i32) -> Amount {
    if factor == 0 {
        return fixed_amt(0);
    }
    if factor == 1 {
        return amt;
    }
    match amt {
        Amount::Const { n } => Amount::Const { n: n * factor },
        other => Amount::Mul {
            factor,
            of: Box::new(other),
        },
    }
}

fn negate_amount(amt: Amount) -> Amount {
    scale_amount(amt, -1)
}

fn add_amounts(left: Amount, right: Amount) -> Amount {
    match (&left, &right) {
        (Amount::Const { n: 0 }, _) => right,
        (_, Amount::Const { n: 0 }) => left,
        (Amount::Const { n: a }, Amount::Const { n: b }) => Amount::Const { n: a + b },
        _ => Amount::Sum {
            args: vec![left, right],
        },
    }
}

fn wrap_layer_mod(layer: Layer, affected: Affected, modifier: LayerMod) -> StaticBody {
    StaticBody::Continuous {
        layer,
        affected,
        modifier,
        active_in_zone: None,
    }
}

fn make_static(body: StaticBody, condition: Option<Condition>) -> StaticDef {
    StaticDef {
        body,
        condition: condition.map(Box::new),
    }
}

fn make_replacement(
    replaces: EventPattern,
    with: ReplacementWith,
    self_replacement: bool,
    who: Option<PlayerFilter>,
    condition: Option<Condition>,
) -> ReplacementDef {
    ReplacementDef {
        replaces,
        with,
        self_replacement,
        who,
        condition: condition.map(Box::new),
    }
}

fn make_zone_change_pattern(
    subject: Option<TriggerSubject>,
    from: ZoneMask,
    to: Option<Zone>,
    from_anywhere: bool,
) -> EventPattern {
    EventPattern {
        event: EventKind::ZoneChange,
        filter: EventFilter {
            object: subject,
            detail: Some(EventDetail::ZoneChange {
                from,
                to,
                from_anywhere,
                not_from_zone: None,
                not_to_zone: None,
                enters_untapped: false,
                dealt_damage_by_self_this_turn: false,
            }),
            ..Default::default()
        },
    }
}

fn make_simple_event_pattern(event: EventKind) -> EventPattern {
    EventPattern {
        event,
        filter: EventFilter::default(),
    }
}

// ============================================================================
// Section 2: Color, Type, Counter & Keyword Parsers
// ============================================================================

fn parse_basic_land_type_word(w: &str) -> Option<&'static str> {
    match w.trim().to_ascii_lowercase().as_str() {
        "plains" => Some("Plains"),
        "island" | "islands" => Some("Island"),
        "swamp" | "swamps" => Some("Swamp"),
        "mountain" | "mountains" => Some("Mountain"),
        "forest" | "forests" => Some("Forest"),
        _ => None,
    }
}

fn parse_card_type_word(w: &str) -> Option<CardTypeMask> {
    let low = w.trim().to_ascii_lowercase();
    let sg = low.strip_suffix('s').unwrap_or(&low);
    let m = is_known_card_type_word(sg);
    if m.is_empty() { None } else { Some(m) }
}

fn parse_supertype_word(w: &str) -> Option<SupertypeMask> {
    let m = is_known_supertype_word(&w.trim().to_ascii_lowercase());
    if m.is_empty() { None } else { Some(m) }
}

fn lookup_static_counter(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<CounterId> {
    let t = trim_clause_punct(raw);
    let trimmed = strip_prefix_ci(t, "an ")
        .or_else(|| strip_prefix_ci(t, "a "))
        .unwrap_or(t)
        .trim();
    let stripped = strip_suffix_ci(trimmed, " counters")
        .or_else(|| strip_suffix_ci(trimmed, " counter"))
        .unwrap_or(trimmed)
        .trim();
    if stripped.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();
    if let Some(id) = lookup_counter_id(stripped, ctx.interner) {
        return Some(id);
    }
    ctx.interner.rollback(cp);
    None
}

#[derive(Default)]
struct ParsedTypes {
    types: CardTypeMask,
    subtypes: SmallVec<[SubtypeId; 2]>,
    colors: Option<ColorMask>,
    colorless: bool,
}

fn parse_type_description(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<ParsedTypes> {
    let cp = ctx.interner.checkpoint();
    let t = strip_leading_article(trim_clause_punct(raw));
    if t.is_empty() {
        return None;
    }
    let words_raw: Vec<&str> = t.split_whitespace().collect();
    if words_raw.is_empty() {
        return None;
    }
    let mut res = ParsedTypes::default();
    let mut i = 0usize;
    while i < words_raw.len() {
        let w = words_raw[i].trim_matches(|c: char| matches!(c, ',' | ';'));
        if w.is_empty() || eq_ci(w, "and") {
            i += 1;
            continue;
        }
        if eq_ci(w, "artifact") && words_raw.get(i + 1).is_some_and(|n| eq_ci(n, "creature")) {
            res.types |= CardTypeMask::ARTIFACT | CardTypeMask::CREATURE;
            i += 2;
            continue;
        }
        if eq_ci(w, "colorless") {
            res.colorless = true;
            i += 1;
            continue;
        }
        if let Some(cm) = parse_color_word(w) {
            let cur = res.colors.get_or_insert(ColorMask::empty());
            *cur |= cm;
            i += 1;
            continue;
        }
        if parse_supertype_word(w).is_some() {
            i += 1;
            continue;
        }
        if let Some(ct) = parse_card_type_word(w) {
            res.types |= ct;
            i += 1;
            continue;
        }
        if eq_ci(w, "permanent")
            || eq_ci(w, "permanents")
            || eq_ci(w, "spell")
            || eq_ci(w, "spells")
        {
            i += 1;
            continue;
        }
        if let Some(bl) = parse_basic_land_type_word(w) {
            res.subtypes.push(ctx.interner.intern_subtype(bl));
            i += 1;
            continue;
        }
        if let Some(sub) = singular_subtype(w) {
            res.subtypes.push(ctx.interner.intern_subtype(&sub));
            i += 1;
            continue;
        }
        ctx.interner.rollback(cp);
        return None;
    }
    if res.types.is_empty() && res.subtypes.is_empty() && res.colors.is_none() && !res.colorless {
        ctx.interner.rollback(cp);
        return None;
    }
    Some(res)
}

type AdditiveTypesAndColors = (
    CardTypeMask,
    SmallVec<[SubtypeId; 2]>,
    Option<ColorMask>,
    bool,
);

fn parse_additive_types_and_colors(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<AdditiveTypesAndColors> {
    let t = trim_clause_punct(raw);
    let body = strip_suffix_ci(t, " in addition to its other types")
        .or_else(|| strip_suffix_ci(t, " in addition to its other colors and types"))
        .or_else(|| strip_suffix_ci(t, " in addition to its other types and colors"))
        .or_else(|| strip_suffix_ci(t, " in addition to their other types"))
        .or_else(|| strip_suffix_ci(t, " in addition to their other colors and types"))?;
    let parsed = parse_type_description(body, ctx)?;
    Some((
        parsed.types,
        parsed.subtypes,
        parsed.colors,
        parsed.colorless,
    ))
}

fn normalize_protection_keyword(raw: &str) -> Option<String> {
    let rest = strip_prefix_ci(raw.trim(), "protection from ")?;
    let rest = trim_clause_punct(rest);
    if rest.is_empty() {
        return None;
    }
    for fixed in [
        "everything",
        "multicolored",
        "monocolored",
        "instants",
        "sorceries",
        "artifacts",
        "enchantments",
        "creatures",
        "planeswalkers",
        "lands",
        "all colors",
        "the chosen color",
        "each of your opponents",
    ] {
        if eq_ci(rest, fixed) {
            return Some(format!("protection from {fixed}"));
        }
    }
    let mut has_color = false;
    let mut all_valid = true;
    for w in rest.split_whitespace() {
        if eq_ci(w, "from") || eq_ci(w, "and") {
            continue;
        }
        if parse_color_word(w).is_some() {
            has_color = true;
        } else {
            all_valid = false;
            break;
        }
    }
    if has_color && all_valid {
        let joined: Vec<String> = rest
            .split_whitespace()
            .map(|p| p.to_ascii_lowercase())
            .collect();
        return Some(format!("protection from {}", joined.join(" ")));
    }
    if let Some(sub) = singular_subtype(rest) {
        return Some(format!("protection from {}", sub.to_ascii_lowercase()));
    }
    if rest.split_whitespace().count() <= 5 {
        return Some(format!("protection from {}", rest.to_ascii_lowercase()));
    }
    None
}

fn is_valid_single_keyword(kw: &str, ctx: &mut ParseCtx<'_>) -> Option<String> {
    let t = trim_clause_punct(kw);
    if t.is_empty() {
        return None;
    }
    if let Some(prot) = normalize_protection_keyword(t) {
        return Some(prot);
    }
    if let Some(cost_str) = strip_prefix_ci(t, "ward ") {
        let cp = ctx.interner.checkpoint();
        let cparse = parse_cost_list(cost_str.trim(), 0, ctx);
        ctx.interner.rollback(cp);
        if !cparse.defs.is_empty() && cparse.unparsed.is_empty() {
            return Some(format!("ward {}", cost_str.trim()));
        }
        if cost_str.trim().starts_with('{') || contains_ci(cost_str, "pay ") {
            return Some(format!("ward {}", cost_str.trim()));
        }
        return None;
    }
    let mut buf = [0u8; 64];
    let check_kw = |low: &str| -> Option<String> {
        if is_known_keyword(low) || get_keyword_spec(low).is_some() {
            return Some(low.to_string());
        }
        if let Some((head, tail)) = low.split_once(' ')
            && (is_known_keyword(head) || get_keyword_spec(head).is_some())
            && parse_num_word_or_int(tail.trim()).is_some()
        {
            return Some(low.to_string());
        }
        None
    };
    if t.len() <= buf.len() && t.is_ascii() {
        let len = t.len();
        buf[..len].copy_from_slice(t.as_bytes());
        buf[..len].make_ascii_lowercase();
        if let Ok(low) = std::str::from_utf8(&buf[..len]) {
            return check_kw(low);
        }
    }
    let low = t.to_ascii_lowercase();
    check_kw(&low)
}

fn parse_keyword_id_list(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<SmallVec<[KeywordId; 2]>> {
    let cp = ctx.interner.checkpoint();
    let mut t = trim_clause_punct(raw);
    t = strip_suffix_ci(t, " until end of turn").unwrap_or(t).trim();
    if t.is_empty() {
        return None;
    }
    let mut raw_items: Vec<&str> = Vec::new();
    for comma_part in t.split(',') {
        let p = comma_part.trim();
        let p = strip_prefix_ci(p, "and ").unwrap_or(p).trim();
        if p.is_empty() {
            continue;
        }
        if starts_with_ci(p, "protection from ") {
            raw_items.push(p);
        } else if let Some((l, r)) = split_once_ci(p, " and ") {
            if !l.trim().is_empty() {
                raw_items.push(l.trim());
            }
            if !r.trim().is_empty() {
                raw_items.push(r.trim());
            }
        } else {
            raw_items.push(p);
        }
    }
    if raw_items.is_empty() {
        return None;
    }
    let mut out = SmallVec::new();
    for item in raw_items {
        if let Some(norm) = is_valid_single_keyword(item, ctx) {
            out.push(ctx.interner.intern_keyword(&norm));
        } else {
            ctx.interner.rollback(cp);
            return None;
        }
    }
    Some(out)
}

// ============================================================================
// Section 3: Affected & ObjectFilter Helpers (Zero-Leak Checkpointing)
// ============================================================================

fn parse_object_with_qualifiers(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
) -> Option<ObjectFilter> {
    let cp = ctx.interner.checkpoint();
    let mut t = trim_clause_punct(raw);
    if t.is_empty() {
        return None;
    }
    let mut extra_flags = FilterFlags::empty();
    let mut extra_chosen_type = false;
    let mut extra_chosen_color = false;
    let mut extra_chosen_name = false;
    let mut extra_same_name_affected = false;
    let mut extra_same_name_source = false;
    let mut extra_not_same_name_source = false;
    let mut extra_owner: Option<RelativeControl> = None;
    let mut extra_controller: Option<RelativeControl> = None;

    loop {
        if let Some(rest) = strip_prefix_ci(t, "other ").or_else(|| strip_prefix_ci(t, "another "))
        {
            extra_flags |= FilterFlags::OTHER;
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "each ") {
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "commander ") {
            extra_flags |= FilterFlags::IS_COMMANDER;
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "attacking ") {
            extra_flags |= FilterFlags::ATTACKING;
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "blocking ") {
            extra_flags |= FilterFlags::BLOCKING;
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "tapped ") {
            extra_flags |= FilterFlags::TAPPED;
            t = rest.trim_start();
            continue;
        }
        if let Some(rest) = strip_prefix_ci(t, "untapped ") {
            extra_flags |= FilterFlags::UNTAPPED;
            t = rest.trim_start();
            continue;
        }
        break;
    }

    loop {
        if let Some(rest) = strip_suffix_ci(t, " you control but don't own") {
            extra_controller = Some(RelativeControl::You);
            extra_owner = Some(RelativeControl::Opponent);
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " you own") {
            extra_owner = Some(RelativeControl::You);
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " of the chosen type") {
            extra_chosen_type = true;
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " of the chosen color") {
            extra_chosen_color = true;
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " with the chosen name")
            .or_else(|| strip_suffix_ci(t, " of the chosen name"))
        {
            extra_chosen_name = true;
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " that share a creature type with it") {
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " with the same name as that creature")
            .or_else(|| strip_suffix_ci(t, " with the same name as that permanent"))
            .or_else(|| strip_suffix_ci(t, " with the same name as that card"))
            .or_else(|| strip_suffix_ci(t, " with the same name as it"))
        {
            extra_same_name_affected = true;
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " with the same name as ~")
            .or_else(|| strip_suffix_ci(t, " with the same name as this creature"))
            .or_else(|| strip_suffix_ci(t, " with the same name as this permanent"))
            .or_else(|| strip_suffix_ci(t, " with the same name as this artifact"))
        {
            extra_same_name_source = true;
            t = rest.trim_end();
            continue;
        }
        if let Some(rest) = strip_suffix_ci(t, " that don't have the same name as ~")
            .or_else(|| strip_suffix_ci(t, " that don't have the same name as this creature"))
            .or_else(|| strip_suffix_ci(t, " that don't have the same name as this permanent"))
            .or_else(|| strip_suffix_ci(t, " that doesn't have the same name as ~"))
            .or_else(|| strip_suffix_ci(t, " that doesn't have the same name as this creature"))
        {
            extra_not_same_name_source = true;
            t = rest.trim_end();
            continue;
        }
        break;
    }

    if eq_ci(t, "outlaws") || eq_ci(t, "outlaw") {
        let mut f = ObjectFilter {
            types_any: CardTypeMask::CREATURE,
            subtypes_any: OUTLAW_SUBTYPES
                .iter()
                .map(|s| ctx.interner.intern_subtype(s))
                .collect(),
            flags: extra_flags,
            controller: extra_controller,
            owner: extra_owner,
            ..Default::default()
        };
        if extra_chosen_type {
            f.extra.push(ObjectPredicate::OfChosenType);
        }
        if extra_chosen_color {
            f.extra.push(ObjectPredicate::OfChosenColor);
        }
        if extra_chosen_name {
            f.extra.push(ObjectPredicate::OfChosenName);
        }
        if extra_same_name_affected {
            f.extra.push(ObjectPredicate::SameNameAsAffected);
        }
        if extra_same_name_source {
            f.extra.push(ObjectPredicate::SameNameAsSource);
        }
        if extra_not_same_name_source {
            f.extra.push(ObjectPredicate::NotSameNameAsSource);
        }
        return Some(f);
    }

    let mut f = match parse_object_phrase(t, ctx, opts) {
        Some(parsed) => parsed,
        None => {
            ctx.interner.rollback(cp);
            if eq_ci(t, "permanents")
                || eq_ci(t, "permanent")
                || eq_ci(t, "sources")
                || eq_ci(t, "source")
            {
                ObjectFilter::default()
            } else {
                return None;
            }
        }
    };

    f.flags |= extra_flags;
    if extra_chosen_type && !f.extra.contains(&ObjectPredicate::OfChosenType) {
        f.extra.push(ObjectPredicate::OfChosenType);
    }
    if extra_chosen_color && !f.extra.contains(&ObjectPredicate::OfChosenColor) {
        f.extra.push(ObjectPredicate::OfChosenColor);
    }
    if extra_chosen_name && !f.extra.contains(&ObjectPredicate::OfChosenName) {
        f.extra.push(ObjectPredicate::OfChosenName);
    }
    if extra_same_name_affected && !f.extra.contains(&ObjectPredicate::SameNameAsAffected) {
        f.extra.push(ObjectPredicate::SameNameAsAffected);
    }
    if extra_same_name_source && !f.extra.contains(&ObjectPredicate::SameNameAsSource) {
        f.extra.push(ObjectPredicate::SameNameAsSource);
    }
    if extra_not_same_name_source && !f.extra.contains(&ObjectPredicate::NotSameNameAsSource) {
        f.extra.push(ObjectPredicate::NotSameNameAsSource);
    }
    if let Some(o) = extra_owner {
        f.owner = Some(o);
    }
    if let Some(c) = extra_controller {
        f.controller = Some(c);
    }
    Some(f)
}

fn parse_affected(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Affected> {
    let t = trim_clause_punct(raw);
    if t.is_empty() {
        return None;
    }
    if is_self_phrase(t) || eq_ci(t, "self") {
        return Some(Affected::SelfObj);
    }
    if eq_ci(t, "equipped creature")
        || eq_ci(t, "equipped permanent")
        || eq_ci(t, "equipped artifact")
        || eq_ci(t, "equipped land")
        || eq_ci(t, "equipped planeswalker")
        || eq_ci(t, "equipped vehicle")
        || eq_ci(t, "equipped spacecraft")
        || eq_ci(t, "enchanted creature")
        || eq_ci(t, "enchanted permanent")
        || eq_ci(t, "enchanted land")
        || eq_ci(t, "enchanted artifact")
        || eq_ci(t, "enchanted enchantment")
        || eq_ci(t, "enchanted planeswalker")
        || eq_ci(t, "enchanted wall")
        || eq_ci(t, "enchanted battle")
        || eq_ci(t, "enchanted vehicle")
        || eq_ci(t, "enchanted spacecraft")
    {
        return Some(Affected::AttachedTo);
    }
    let phrase = strip_prefix_ci(t, "all ")
        .or_else(|| strip_prefix_ci(t, "each "))
        .unwrap_or(t);
    let opts = FilterOpts {
        allow_plural_and_disjunction: true,
        ..FilterOpts::default()
    };
    let mut filter = parse_object_with_qualifiers(phrase, ctx, opts)?;
    if !contains_ci(phrase, "graveyard")
        && !contains_ci(phrase, "exile")
        && !contains_ci(phrase, "hand")
        && !contains_ci(phrase, "library")
        && !contains_ci(phrase, "stack")
    {
        filter.zones = ZoneMask::empty();
    }
    Some(Affected::Filter {
        filter: Box::new(filter),
    })
}

fn split_object_and_controller(raw: &str) -> (&str, Option<RelativeControl>) {
    let t = raw.trim();
    if let Some(r) = strip_suffix_ci(t, " you control") {
        return (r.trim(), Some(RelativeControl::You));
    }
    if let Some(r) = strip_suffix_ci(t, " your opponents control")
        .or_else(|| strip_suffix_ci(t, " an opponent controls"))
        .or_else(|| strip_suffix_ci(t, " opponents control"))
    {
        return (r.trim(), Some(RelativeControl::Opponent));
    }
    (t, None)
}

// ============================================================================
// Section 4: Count Amount & Static Condition Parsers
// ============================================================================

fn try_parse_wave9_count_amount(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Amount> {
    let t = trim_clause_punct(raw);
    let cp = ctx.interner.checkpoint();

    if eq_ci(
        t,
        "the number of card types among cards in your opponents' graveyards",
    ) {
        return Some(Amount::DistinctCardTypesInZone {
            zone: Zone::Graveyard,
            whose: ZoneScopeWhose::Opponents,
        });
    }
    if eq_ci(t, "the number of card types among cards in your graveyard") {
        return Some(Amount::DistinctCardTypesInZone {
            zone: Zone::Graveyard,
            whose: ZoneScopeWhose::You,
        });
    }
    if eq_ci(t, "the number of card types among cards in all graveyards") {
        return Some(Amount::DistinctCardTypesInZone {
            zone: Zone::Graveyard,
            whose: ZoneScopeWhose::All,
        });
    }

    if eq_ci(t, "your life total") {
        return Some(Amount::Life { of: PlayerRef::You });
    }
    if eq_ci(t, "the number of cards in your hand") {
        return Some(Amount::CardsIn {
            zone: Zone::Hand,
            of: PlayerRef::You,
        });
    }
    if eq_ci(t, "the number of lands you control") {
        return Some(Amount::Count {
            of: Box::new(ObjectFilter {
                types_any: CardTypeMask::LAND,
                controller: Some(RelativeControl::You),
                zones: ZoneMask::BATTLEFIELD,
                ..Default::default()
            }),
        });
    }
    if let Some(rest) = strip_prefix_ci(t, "your devotion to ")
        && let Some(colors) = parse_devotion_color_list(rest.trim())
    {
        return Some(Amount::Devotion {
            colors,
            player: Some(PlayerRef::You),
        });
    }
    if eq_ci(
        t,
        "the number of times you've cast your commander from the command zone this game",
    ) || eq_ci(
        t,
        "the number of time you've cast your commander from the command zone this game",
    ) {
        return Some(Amount::CommanderCastCount);
    }
    if eq_ci(t, "the number of colors among permanents you control")
        || eq_ci(t, "the number of color among permanents you control")
    {
        return Some(Amount::ColorsAmongPermanents {
            filter: None,
            player: PlayerRef::You,
        });
    }
    if eq_ci(
        t,
        "the greatest number of cards an opponent has drawn this turn",
    ) {
        return Some(Amount::GreatestOpponentCardsDrawnThisTurn);
    }
    if eq_ci(t, "the number of creatures that died this turn") {
        return Some(Amount::CreaturesDiedThisTurn { controller: None });
    }
    if let Some(after_great) = strip_prefix_ci(t, "the greatest number of ")
        && let Some((obj_s, _)) = split_once_ci(after_great, " a player controls")
        && let Some(mut f) = parse_object_with_qualifiers(obj_s.trim(), ctx, FilterOpts::default())
    {
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::GreatestControlledCount {
            filter: Box::new(f),
        });
    }
    if let Some(rest) = strip_prefix_ci(t, "twice ") {
        if let Some(inner) = parse_count_amount(rest, ctx) {
            return Some(scale_amount(inner, 2));
        }
        ctx.interner.rollback(cp);
    }
    None
}

fn parse_count_amount(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Amount> {
    let t = trim_clause_punct(raw);
    if t.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();

    if let Some((left, right)) = split_once_ci(t, " plus ") {
        if let Some(n) = parse_num_word_or_int(left) {
            if let Some(rhs) = parse_count_amount(right, ctx) {
                return Some(add_amounts(fixed_amt(n), rhs));
            }
            ctx.interner.rollback(cp);
        }
        if let Some((l_amt, r_amt)) =
            parse_count_amount(left, ctx).zip(parse_count_amount(right, ctx))
        {
            return Some(add_amounts(l_amt, r_amt));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(amt) = try_parse_wave9_count_amount(t, ctx) {
        return Some(amt);
    }

    if eq_ci(t, "this creature's power") || eq_ci(t, "its power") || eq_ci(t, "~'s power") {
        return Some(Amount::Stat {
            of: Ref::SelfObj,
            stat: StatKind::Power,
            lki: false,
        });
    }
    if eq_ci(t, "this creature's toughness")
        || eq_ci(t, "its toughness")
        || eq_ci(t, "~'s toughness")
    {
        return Some(Amount::Stat {
            of: Ref::SelfObj,
            stat: StatKind::Toughness,
            lki: false,
        });
    }
    if eq_ci(t, "enchanted creature's power") || eq_ci(t, "equipped creature's power") {
        return Some(Amount::Stat {
            of: Ref::AttachedTo,
            stat: StatKind::Power,
            lki: false,
        });
    }

    if let Some(rest) = strip_prefix_ci(t, "the number of ") {
        let rest_trim = rest.trim();
        for (suf, target) in [
            (" attached to it", AttachmentOwnerTarget::AttachedTo),
            (
                " attached to equipped creature",
                AttachmentOwnerTarget::AttachedTo,
            ),
            (
                " attached to enchanted creature",
                AttachmentOwnerTarget::AttachedTo,
            ),
            (" attached to ~", AttachmentOwnerTarget::SelfObj),
            (" attached to this creature", AttachmentOwnerTarget::SelfObj),
        ] {
            if let Some(obj_s) = strip_suffix_ci(rest_trim, suf) {
                let obj_clean = obj_s.trim();
                let filter = if obj_clean.is_empty()
                    || eq_ci(obj_clean, "permanents")
                    || eq_ci(obj_clean, "permanent")
                    || eq_ci(obj_clean, "cards")
                    || eq_ci(obj_clean, "card")
                {
                    None
                } else {
                    let f_opts = FilterOpts {
                        allow_plural_and_disjunction: true,
                        ..FilterOpts::default()
                    };
                    if let Some(mut f) = parse_object_with_qualifiers(obj_clean, ctx, f_opts) {
                        f.zones = ZoneMask::empty();
                        Some(Box::new(f))
                    } else {
                        ctx.interner.rollback(cp);
                        None
                    }
                };
                if filter.is_some()
                    || obj_clean.is_empty()
                    || eq_ci(obj_clean, "permanents")
                    || eq_ci(obj_clean, "permanent")
                {
                    return Some(Amount::AttachmentsOn { target, filter });
                }
            }
        }
        for suf in [
            " exiled with it",
            " exiled with ~",
            " exiled with this artifact",
            " exiled with this creature",
            " exiled with this enchantment",
        ] {
            if let Some(obj_s) = strip_suffix_ci(rest_trim, suf) {
                let obj_clean = obj_s.trim();
                let mut f = if eq_ci(obj_clean, "cards") || eq_ci(obj_clean, "card") {
                    ObjectFilter::default()
                } else {
                    let f_opts = FilterOpts {
                        strip_trailing_card: true,
                        ..FilterOpts::default()
                    };
                    match parse_object_with_qualifiers(obj_clean, ctx, f_opts) {
                        Some(parsed) => parsed,
                        None => {
                            ctx.interner.rollback(cp);
                            break;
                        }
                    }
                };
                f.zones = ZoneMask::EXILE;
                if !f.extra.contains(&ObjectPredicate::ExiledWithSelf) {
                    f.extra.push(ObjectPredicate::ExiledWithSelf);
                }
                return Some(Amount::Count { of: Box::new(f) });
            }
        }
        if let Some((left_each, right_each)) = split_once_ci(rest, " and each ") {
            let l_q = format!("the number of {}", left_each.trim());
            let r_q = format!("the number of {}", right_each.trim());
            if let Some((la, ra)) = parse_count_amount(&l_q, ctx).zip(parse_count_amount(&r_q, ctx))
            {
                return Some(add_amounts(la, ra));
            }
            ctx.interner.rollback(cp);
        }
        if let Some((cnt_part, on_part)) = split_once_ci(rest, " on ")
            && (is_self_phrase(on_part.trim()) || eq_ci(on_part.trim(), "each creature"))
        {
            if eq_ci(cnt_part.trim(), "counter") || eq_ci(cnt_part.trim(), "counters") {
                return Some(Amount::CountersOn {
                    counter: CounterTarget::Any,
                    of: mtg_ir::Ref::SelfObj,
                });
            }
            if let Some(cid) = lookup_static_counter(cnt_part, ctx) {
                return Some(Amount::CountersOn {
                    counter: CounterTarget::Specific(cid),
                    of: mtg_ir::Ref::SelfObj,
                });
            }
        }
        if eq_ci(rest.trim(), "cards in your opponents' graveyards") {
            return Some(Amount::Count {
                of: Box::new(ObjectFilter {
                    zones: ZoneMask::GRAVEYARD,
                    controller: Some(RelativeControl::Opponent),
                    ..Default::default()
                }),
            });
        }
        let (same_name_exile, clean_rest) = if let Some(p) =
            strip_suffix_ci(rest.trim(), " with the same name as the exiled card")
                .or_else(|| strip_suffix_ci(rest.trim(), " with the same name as that exiled card"))
        {
            (true, p.trim())
        } else {
            (false, rest)
        };
        let f_opts = FilterOpts {
            strip_trailing_card: contains_ci(clean_rest, "graveyard")
                || contains_ci(clean_rest, "exile"),
            ..FilterOpts::default()
        };
        if let Some(mut f) = parse_object_with_qualifiers(clean_rest, ctx, f_opts) {
            if f.zones.is_empty() {
                if contains_ci(clean_rest, "graveyard") {
                    f.zones = ZoneMask::GRAVEYARD;
                } else if contains_ci(clean_rest, "exile") {
                    f.zones = ZoneMask::EXILE;
                } else {
                    f.zones = ZoneMask::BATTLEFIELD;
                }
            }
            if same_name_exile {
                f.extra.push(ObjectPredicate::SameNameAsLinkedExile);
            }
            return Some(Amount::Count { of: Box::new(f) });
        }
        ctx.interner.rollback(cp);
    }

    let toks = tokenize(t, 0);
    if !toks.is_empty() {
        let mut scope = NullScope;
        if let Some((amt, used)) = parse_amount(&toks, ctx, &mut scope)
            && used == toks.len()
        {
            return Some(amt);
        }
        ctx.interner.rollback(cp);
    }
    None
}

fn try_parse_wave9_condition(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let t = trim_clause_punct(raw);
    let cp = ctx.interner.checkpoint();

    if eq_ci(t, "you control a commander") || eq_ci(t, "you control your commander") {
        return Some(Condition::ControlsFilter {
            filter: Box::new(ObjectFilter {
                flags: FilterFlags::IS_COMMANDER,
                controller: Some(RelativeControl::You),
                zones: ZoneMask::BATTLEFIELD,
                ..Default::default()
            }),
        });
    }
    if eq_ci(t, "you don't control a commander") || eq_ci(t, "you don't control your commander") {
        return Some(Condition::Not {
            of: Box::new(Condition::ControlsFilter {
                filter: Box::new(ObjectFilter {
                    flags: FilterFlags::IS_COMMANDER,
                    controller: Some(RelativeControl::You),
                    zones: ZoneMask::BATTLEFIELD,
                    ..Default::default()
                }),
            }),
        });
    }
    if eq_ci(t, "it's your turn") || eq_ci(t, "it is your turn") {
        return Some(Condition::IsYourTurn);
    }
    if eq_ci(t, "it's not your turn") || eq_ci(t, "it isn't your turn") {
        return Some(Condition::NotYourTurn);
    }
    if eq_ci(t, "you have the city's blessing") {
        return Some(Condition::HasCitysBlessing);
    }
    if let Some(after_have) = strip_prefix_ci(t, "you have ")
        && let Some((num_s, tail_s)) = split_once_ci(after_have, " or more mana in your mana pool")
            .or_else(|| split_once_ci(after_have, " or more unspent mana"))
        && tail_s.trim().is_empty()
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::UnspentManaTotal {
                player: Some(PlayerRef::You),
            }),
            op: CompareOp::Ge,
            right: Box::new(fixed_amt(n)),
        });
    }
    if let Some(rest) = strip_prefix_ci(t, "you control ") {
        if let Some((num_s, obj_s)) = split_once_ci(rest, " or more ")
            && let Some(n) = parse_num_word_or_int(num_s)
        {
            if let Some(mut f) = parse_object_with_qualifiers(obj_s, ctx, FilterOpts::default()) {
                f.controller = Some(RelativeControl::You);
                if n <= 1 {
                    f.zones = ZoneMask::BATTLEFIELD;
                    return Some(Condition::ControlsFilter {
                        filter: Box::new(f),
                    });
                }
                f.zones = ZoneMask::empty();
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: Box::new(fixed_amt(n)),
                });
            }
            ctx.interner.rollback(cp);
        }
        if let Some(mut f) = parse_object_with_qualifiers(rest, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::BATTLEFIELD;
            f.controller = Some(RelativeControl::You);
            return Some(Condition::ControlsFilter {
                filter: Box::new(f),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rest) = strip_prefix_ci(t, "you don't control ") {
        if let Some(mut f) = parse_object_with_qualifiers(rest, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::BATTLEFIELD;
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Not {
                of: Box::new(Condition::ControlsFilter {
                    filter: Box::new(f),
                }),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rest) = strip_prefix_ci(t, "you have ")
        && let Some((num_s, tail_s)) = split_once_ci(rest, " or more ")
        && (eq_ci(tail_s.trim(), "opponents") || eq_ci(tail_s.trim(), "opponent"))
        && let Some(n) = parse_num_word_or_int(num_s)
    {
        return Some(Condition::OpponentCount {
            op: CompareOp::Ge,
            count: n as u16,
        });
    }
    if let Some(rest) = strip_prefix_ci(t, "~ is attached to ")
        .or_else(|| strip_prefix_ci(t, "this equipment is attached to "))
        .or_else(|| strip_prefix_ci(t, "this aura is attached to "))
        .or_else(|| strip_prefix_ci(t, "this creature is attached to "))
        .or_else(|| strip_prefix_ci(t, "it is attached to "))
    {
        let clean_r = strip_leading_article(rest.trim());
        if let Some(mut f) = parse_object_with_qualifiers(clean_r, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::empty();
            return Some(Condition::ObjectState {
                of: mtg_ir::AttachmentOwnerTarget::AttachedTo,
                tapped: None,
                attacking: None,
                blocking: None,
                monstrous: None,
                saddled: None,
                equipped: None,
                enchanted: None,
                filter: Some(Box::new(f)),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rest) = strip_prefix_ci(t, "enchanted permanent is ")
        .or_else(|| strip_prefix_ci(t, "enchanted creature is "))
        .or_else(|| strip_prefix_ci(t, "equipped creature is "))
        .or_else(|| strip_prefix_ci(t, "equipped permanent is "))
    {
        let clean_r = strip_leading_article(rest.trim());
        if let Some(mut f) = parse_object_with_qualifiers(clean_r, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::empty();
            return Some(Condition::ObjectState {
                of: mtg_ir::AttachmentOwnerTarget::AttachedTo,
                tapped: None,
                attacking: None,
                blocking: None,
                monstrous: None,
                saddled: None,
                equipped: None,
                enchanted: None,
                filter: Some(Box::new(f)),
            });
        }
        ctx.interner.rollback(cp);
    }
    if eq_ci(t, "an opponent lost life this turn") {
        return Some(Condition::OpponentLostLifeThisTurn);
    }
    if eq_ci(t, "~ is prepared") || eq_ci(t, "it is prepared") || eq_ci(t, "it's prepared") {
        return Some(Condition::SelfPrepared);
    }
    if eq_ci(t, "~ isn't prepared")
        || eq_ci(t, "~ is not prepared")
        || eq_ci(t, "~ is unprepared")
        || eq_ci(t, "it isn't prepared")
        || eq_ci(t, "it is not prepared")
        || eq_ci(t, "it's not prepared")
        || eq_ci(t, "it is unprepared")
    {
        return Some(Condition::Not {
            of: Box::new(Condition::SelfPrepared),
        });
    }
    if let Some(rest) =
        strip_prefix_ci(t, "a player has ").or_else(|| strip_prefix_ci(t, "any player has "))
        && let Some(body) = strip_suffix_ci(rest.trim(), " life")
    {
        if let Some(num_s) = strip_suffix_ci(body.trim(), " or less")
            && let Some(n) = parse_num_word_or_int(num_s.trim())
        {
            return Some(Condition::AnyPlayerLife {
                op: CompareOp::Le,
                count: n,
            });
        }
        if let Some(num_s) = strip_suffix_ci(body.trim(), " or more")
            && let Some(n) = parse_num_word_or_int(num_s.trim())
        {
            return Some(Condition::AnyPlayerLife {
                op: CompareOp::Ge,
                count: n,
            });
        }
    }
    if let Some(rest) = strip_prefix_ci(t, "your opponents control ")
        .or_else(|| strip_prefix_ci(t, "an opponent controls "))
    {
        if let Some((num_s, obj_s)) = split_once_ci(rest, " or more ")
            && let Some(n) = parse_num_word_or_int(num_s.trim())
        {
            if let Some(mut f) = parse_object_with_qualifiers(obj_s, ctx, FilterOpts::default()) {
                f.controller = Some(RelativeControl::Opponent);
                f.zones = ZoneMask::empty();
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: Box::new(fixed_amt(n)),
                });
            }
            ctx.interner.rollback(cp);
        }
        if let Some(mut f) = parse_object_with_qualifiers(rest, ctx, FilterOpts::default()) {
            f.controller = Some(RelativeControl::Opponent);
            f.zones = ZoneMask::empty();
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: Box::new(fixed_amt(1)),
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_have) = strip_prefix_ci(t, "you have ")
        && let Some((num_s, tail_s)) = split_once_ci(after_have, " or more ")
        && (eq_ci(tail_s.trim(), "cards in hand") || eq_ci(tail_s.trim(), "cards in your hand"))
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Ge,
            right: Box::new(fixed_amt(n)),
        });
    }
    if let Some(after_there) = strip_prefix_ci(t, "there are fewer than ")
        && let Some((num_s, tail_s)) = split_once_ci(after_there, " cards in your graveyard")
        && tail_s.trim().is_empty()
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Graveyard,
                of: PlayerRef::You,
            }),
            op: CompareOp::Lt,
            right: Box::new(fixed_amt(n)),
        });
    }
    if let Some(after_dev) = strip_prefix_ci(t, "your devotion to ")
        && let Some((color_s, num_s)) = split_once_ci(after_dev, " is less than ")
        && let Some(colors) = parse_devotion_color_list(color_s.trim())
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::Devotion {
                colors,
                player: Some(PlayerRef::You),
            }),
            op: CompareOp::Lt,
            right: Box::new(fixed_amt(n)),
        });
    }
    if let Some(after_there) = strip_prefix_ci(t, "there are ")
        && let Some((num_s, rest_cnt)) = split_once_ci(after_there, " or more ")
        && let Some(n) = parse_num_word_or_int(num_s.trim())
        && let Some((ctr_s, filt_s)) = split_once_ci(rest_cnt, " among ")
        && let Some(cid) = lookup_static_counter(ctr_s.trim(), ctx)
    {
        if let Some(mut f) = parse_object_with_qualifiers(filt_s.trim(), ctx, FilterOpts::default())
        {
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            return Some(Condition::Compare {
                left: Box::new(Amount::CountersAmong {
                    filter: Box::new(f),
                    counter: cid,
                }),
                op: CompareOp::Ge,
                right: Box::new(fixed_amt(n)),
            });
        }
        ctx.interner.rollback(cp);
    }
    if eq_ci(t, "~ is paired with another creature")
        || eq_ci(t, "this creature is paired with another creature")
        || eq_ci(t, "it is paired with another creature")
    {
        return Some(Condition::IsPaired);
    }
    if let Some(after_ctrl) = strip_prefix_ci(t, "you control ")
        && let Some((num_s, tail_s)) = split_once_ci(after_ctrl, " or more ")
        && eq_ci(tail_s.trim(), "creatures with different powers")
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(Condition::DistinctControlledCreaturePowers { count: n as u16 });
    }
    if let Some(after_sac) =
        strip_prefix_ci(t, "you've sacrificed ").or_else(|| strip_prefix_ci(t, "you sacrificed "))
        && let Some(obj_s) = strip_suffix_ci(after_sac.trim(), " this turn")
    {
        let obj_clean = strip_leading_article(obj_s.trim());
        if let Some(mut f) = parse_object_with_qualifiers(obj_clean, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::empty();
            return Some(Condition::SacrificedFilterThisTurn {
                filter: Box::new(f),
                min: 1,
                who: RelativeControl::You,
            });
        }
        ctx.interner.rollback(cp);
    }
    if let Some(after_tgt) = strip_prefix_ci(t, "it targets ") {
        let mut stack_kind = None;
        let mut inner_tgt = None;
        for (prefix, sk) in [
            (
                "a spell or ability that targets ",
                StackTargetKind::SpellOrAbility,
            ),
            ("a spell that targets ", StackTargetKind::Spell),
            ("an ability that targets ", StackTargetKind::Ability),
        ] {
            if let Some(rest) = strip_prefix_ci(after_tgt.trim(), prefix) {
                stack_kind = Some(sk);
                inner_tgt = Some(rest.trim());
                break;
            }
        }
        if let (Some(sk), Some(filt_s)) = (stack_kind, inner_tgt) {
            let filt_clean = strip_leading_article(filt_s);
            if let Some(mut f) =
                parse_object_with_qualifiers(filt_clean, ctx, FilterOpts::default())
            {
                f.zones = ZoneMask::empty();
                return Some(Condition::TargetsStackSpell {
                    filter: Some(Box::new(ObjectFilter {
                        zones: ZoneMask::STACK,
                        stack_kind: Some(sk),
                        extra: vec![ObjectPredicate::TargetsFilter {
                            filter: Box::new(f),
                        }],
                        ..Default::default()
                    })),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    if eq_ci(
        t,
        "a player's life total is less than or equal to half their starting life total",
    ) {
        return Some(Condition::Or {
            of: vec![
                Condition::LifeCompareHalfStarting {
                    who: LifeCompareWho::You,
                    op: CompareOp::Le,
                },
                Condition::LifeCompareHalfStarting {
                    who: LifeCompareWho::AnyOpponent,
                    op: CompareOp::Le,
                },
            ],
        });
    }
    if eq_ci(t, "mana from a treasure was spent to cast it")
        || eq_ci(t, "that mana from a treasure was spent to cast")
    {
        return Some(Condition::FirstSpellThisTurnWithManaFrom {
            mana_source: ManaSourceKind::Treasure,
        });
    }
    None
}

struct AttachedStaticScope;

impl ParserScope for AttachedStaticScope {
    fn default_pronoun_subject(&self) -> PronounSubject {
        PronounSubject::AttachedTo
    }
}

fn parse_static_condition(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    parse_static_condition_with_attached(raw, false, ctx)
}

fn parse_static_condition_for_affected(
    raw: &str,
    affected: &Affected,
    ctx: &mut ParseCtx<'_>,
) -> Option<Condition> {
    parse_static_condition_with_attached(raw, matches!(affected, Affected::AttachedTo), ctx)
}

fn parse_static_condition_with_attached(
    raw: &str,
    attached_pronoun: bool,
    ctx: &mut ParseCtx<'_>,
) -> Option<Condition> {
    let mut t = trim_clause_punct(raw);
    t = strip_prefix_ci(t, "as long as ")
        .or_else(|| strip_prefix_ci(t, "if "))
        .unwrap_or(t)
        .trim();
    if t.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();
    let toks = tokenize(t, 0);
    if !toks.is_empty() {
        let parsed = if attached_pronoun {
            let mut scope = AttachedStaticScope;
            parse_condition(&toks, ctx, &mut scope)
        } else {
            let mut scope = NullScope;
            parse_condition(&toks, ctx, &mut scope)
        };
        if let Some((cond, used)) = parsed
            && used == toks.len()
        {
            return Some(cond);
        }
        ctx.interner.rollback(cp);
    }
    if let Some(cond) = try_parse_wave9_condition(t, ctx) {
        return Some(cond);
    }
    ctx.interner.rollback(cp);
    None
}

fn strip_implicit_bf_zone_in_cond(cond: &mut Condition) {
    match cond {
        Condition::Compare { left, .. } => {
            if let Amount::Count { of } = left.as_mut() {
                of.zones.remove(ZoneMask::BATTLEFIELD);
            }
        }
        Condition::Not { of } => strip_implicit_bf_zone_in_cond(of),
        Condition::And { of } | Condition::Or { of } => {
            for c in of {
                strip_implicit_bf_zone_in_cond(c);
            }
        }
        _ => {}
    }
}

fn parse_unless_etb_condition(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let t = trim_clause_punct(raw);
    let explicit_bf = contains_ci(t, "on the battlefield") || contains_ci(t, "battlefield");
    let cp = ctx.interner.checkpoint();
    if let Some(rest) = strip_prefix_ci(t, "unless ") {
        if let Some(mut c) = parse_static_condition(rest, ctx) {
            if !explicit_bf {
                strip_implicit_bf_zone_in_cond(&mut c);
            }
            return Some(negate_condition(&c));
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rest) = strip_prefix_ci(t, "if ") {
        if let Some(mut c) = parse_static_condition(rest, ctx) {
            if !explicit_bf {
                strip_implicit_bf_zone_in_cond(&mut c);
            }
            return Some(c);
        }
        ctx.interner.rollback(cp);
    }
    None
}

fn negate_condition(cond: &Condition) -> Condition {
    match cond {
        Condition::Not { of } => (**of).clone(),
        other => Condition::Not {
            of: Box::new(other.clone()),
        },
    }
}

// ============================================================================
// Section 5: Granted Abilities, Tail Parsers & Copy Modifications
// ============================================================================

#[derive(Clone)]
enum GrantedItem {
    Keyword(KeywordId),
    Ability(Box<AbilityDef>),
}

fn parse_dynamic_keyword_grant_def_impl(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<KeywordDef> {
    let t = trim_clause_punct(raw);
    for (prefix, kw_name) in [
        ("mobilize x, where x is ", "mobilize"),
        ("firebending x, where x is ", "firebending"),
    ] {
        if let Some(expr) = strip_prefix_ci(t, prefix)
            && let Some(amt) = parse_count_amount(expr.trim(), ctx)
        {
            let kw_id = ctx.interner.intern_keyword(kw_name);
            let mut kw = make_empty_kw_def(kw_id, None);
            kw.dynamic_amount = Some(Box::new(amt));
            return Some(kw);
        }
    }
    None
}

fn parse_single_param_keyword_grant(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<GrantedItem> {
    let t = trim_clause_punct(raw);
    let cp = ctx.interner.checkpoint();
    if let Some(kw) = parse_dynamic_keyword_grant_def_impl(t, ctx) {
        return Some(GrantedItem::Ability(Box::new(AbilityDef::Keyword(
            mtg_ir::KeywordAbility {
                keyword: kw,
                id: None,
                span: Span::default(),
            },
        ))));
    }
    if let Some(rest) = strip_prefix_ci(t, "crew ")
        && let Some(n) = parse_num_word_or_int(rest)
    {
        let crew_cost = CostDef::TapSubsetSum {
            min_total: Box::new(Amount::Const { n }),
            filter: Box::new(ObjectFilter {
                types_any: CardTypeMask::CREATURE,
                controller: Some(RelativeControl::You),
                flags: FilterFlags::UNTAPPED,
                ..Default::default()
            }),
            exclude_self: true,
            restriction_tag: Some("crew".to_string()),
        };
        let anim_step = EffectStep {
            op: EffectOp::Animate {
                duration: Some(mtg_ir::Duration::EndOfTurn),
                add_types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                set_types: None,
                remove_types: CardTypeMask::empty(),
                add_subtypes: SmallVec::new(),
                set_subtypes: None,
                set_colors: None,
                add_colors: ColorMask::empty(),
                add_keywords: SmallVec::new(),
                set_power: None,
                set_toughness: None,
                keep_existing_types: false,
                still_a_land: false,
                remove_all_abilities: false,
                granted_ability_texts: Vec::new(),
                choose_creature_type: false,
                choose_basic_land_type: false,
                choose_color: None,
            },
            subject: Selector::SelfObj,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        };
        let ab = ActivatedAbility {
            id: String::new(),
            cost: vec![crew_cost],
            targets: Vec::new(),
            effect: vec![anim_step],
            modal: None,
            timing: Timing::Instant,
            is_mana: false,
            activation_restrictions: Vec::new(),
            activation_condition: None,
            activator_who: None,
            power_up_reduction: false,
            x_cant_be_zero: false,
            activation_cost_reduction: None,
            activation_zones: ZoneMask::BATTLEFIELD,
            cost_keyword: Some(ctx.interner.intern_keyword("crew")),
            intrinsic: false,
            text: Some(t.to_string()),
            grant_condition: None,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
            span: Span::default(),
        };
        return Some(GrantedItem::Ability(Box::new(AbilityDef::Activated(ab))));
    }
    if let Some(rest) = strip_prefix_ci(t, "saddle ")
        && let Some(n) = parse_num_word_or_int(rest)
    {
        let id = ctx.interner.intern_keyword(&format!("saddle {n}"));
        return Some(GrantedItem::Keyword(id));
    }
    if let Some(rest) = strip_prefix_ci(t, "ward ") {
        let cost_s = rest.trim();
        let cparse = parse_cost_list(cost_s, 0, ctx);
        ctx.interner.rollback(cp);
        if (!cparse.defs.is_empty() && cparse.unparsed.is_empty())
            || cost_s.starts_with('{')
            || contains_ci(cost_s, "pay ")
        {
            let id = ctx.interner.intern_keyword(&format!("ward {cost_s}"));
            return Some(GrantedItem::Keyword(id));
        }
    }
    if let Some(rest) = strip_prefix_ci(t, "annihilator ")
        && let Some(n) = parse_num_word_or_int(rest)
    {
        let trig = mtg_ir::TriggerDef {
            event: EventKind::Attacks,
            filter: EventFilter {
                object: Some(TriggerSubject::SelfObj),
                ..Default::default()
            },
            scope: None,
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: Vec::new(),
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        };
        let step = EffectStep {
            op: EffectOp::Sacrifice {
                select: SelectionMod::default(),
            },
            subject: Selector::All {
                filter: Box::new(ObjectFilter::default()),
            },
            actor: Some(PlayerRef::DefendingPlayer),
            amount: Some(Box::new(fixed_amt(n))),
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: Span::default(),
        };
        let tr = TriggeredAbility {
            trigger: trig,
            condition: None,
            grant_condition: None,
            targets: Vec::new(),
            effect: vec![step],
            modal: None,
            text: Some(format!("annihilator {n}")),
            span: Span::default(),
        };
        return Some(GrantedItem::Ability(Box::new(AbilityDef::Triggered(tr))));
    }
    if let Some(rest) = strip_prefix_ci(t, "equip ") {
        let cparse = parse_cost_list(rest, 0, ctx);
        if !cparse.defs.is_empty() && cparse.unparsed.is_empty() {
            let equip_targets = {
                let toks = tokenize("target creature you control", 0);
                let mut scope = NullScope;
                parse_target_spec(&toks, ctx, &mut scope)
                    .map(|(ts, _)| vec![ts])
                    .unwrap_or_else(|| {
                        vec![TargetSpec {
                            kind: TargetKind::Object,
                            filter: ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                zones: ZoneMask::BATTLEFIELD,
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
                        }]
                    })
            };
            let ab = ActivatedAbility {
                id: String::new(),
                cost: cparse.defs,
                targets: equip_targets,
                effect: Vec::new(),
                modal: None,
                timing: Timing::Sorcery,
                is_mana: false,
                activation_restrictions: Vec::new(),
                activation_condition: None,
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: ZoneMask::BATTLEFIELD,
                cost_keyword: Some(ctx.interner.intern_keyword("equip")),
                intrinsic: false,
                text: Some(t.to_string()),
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: Span::default(),
            };
            return Some(GrantedItem::Ability(Box::new(AbilityDef::Activated(ab))));
        }
        ctx.interner.rollback(cp);
    }
    for kw_name in [
        "annihilator",
        "bushido",
        "afterlife",
        "soulshift",
        "frenzy",
        "poisonous",
        "renown",
        "tribute",
        "extort",
        "exalted",
        "melee",
        "myriad",
        "provoke",
        "afflict",
        "rampage",
        "flanking",
        "dethrone",
        "toxic",
        "ward",
    ] {
        if let Some(rest) = strip_prefix_ci(t, &format!("{kw_name} "))
            && let Some(n) = parse_num_word_or_int(rest)
        {
            let id = ctx.interner.intern_keyword(&format!("{kw_name} {n}"));
            return Some(GrantedItem::Keyword(id));
        }
    }
    ctx.interner.rollback(cp);
    None
}

fn map_extra_restriction(r: ActivatedExtraRestriction) -> Option<ActivationRestriction> {
    match r {
        ActivatedExtraRestriction::AsASorcery => None,
        ActivatedExtraRestriction::OnceEachTurn => Some(ActivationRestriction::OnceEachTurn),
        ActivatedExtraRestriction::DuringYourTurn => Some(ActivationRestriction::DuringYourTurn),
        ActivatedExtraRestriction::DuringYourUpkeep => {
            Some(ActivationRestriction::DuringYourUpkeep)
        }
        ActivatedExtraRestriction::DuringCombat => Some(ActivationRestriction::DuringCombat),
    }
}

fn replace_granted_self_refs(raw: &str) -> String {
    let mut s = raw.to_string();
    for pat in [
        "this creature",
        "this permanent",
        "this artifact",
        "this enchantment",
        "this land",
        "this equipment",
        "this aura",
    ] {
        loop {
            let low = s.to_ascii_lowercase();
            let Some(pos) = low.find(pat) else { break };
            let end = pos + pat.len();
            let left_ok = pos == 0 || !s.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let right_ok = end == s.len() || !s.as_bytes()[end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                s.replace_range(pos..end, "~");
            } else {
                break;
            }
        }
    }
    s
}

fn parse_bracketed_loyalty_cost(s: &str) -> Option<CostDef> {
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner == "0" {
        return Some(CostDef::Loyalty {
            delta: 0,
            is_x: false,
            sign: None,
        });
    }
    let norm = if inner.contains('\u{2212}') {
        Cow::Owned(inner.replace('\u{2212}', "-"))
    } else {
        Cow::Borrowed(inner)
    };
    let t = norm.trim();
    if t.eq_ignore_ascii_case("+x") {
        return Some(CostDef::Loyalty {
            delta: 0,
            is_x: true,
            sign: Some(LoyaltySign::Plus),
        });
    }
    if t.eq_ignore_ascii_case("-x") {
        return Some(CostDef::Loyalty {
            delta: 0,
            is_x: true,
            sign: Some(LoyaltySign::Minus),
        });
    }
    if let Ok(delta) = t.parse::<i32>() {
        return Some(CostDef::Loyalty {
            delta,
            is_x: false,
            sign: None,
        });
    }
    None
}

fn parse_granted_activated_ability(
    quoted: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ActivatedAbility> {
    let norm = replace_granted_self_refs(quoted);
    let t = trim_sentence(&norm);
    let cp = ctx.interner.checkpoint();
    let (cost_str, eff_str) = t.split_once(':')?;
    let cost_defs = if let Some(loy) = parse_bracketed_loyalty_cost(cost_str) {
        vec![loy]
    } else {
        let cparse = parse_cost_list(cost_str.trim(), 0, ctx);
        if cparse.defs.is_empty() || !cparse.unparsed.is_empty() {
            ctx.interner.rollback(cp);
            return None;
        }
        cparse.defs
    };

    let has_loyalty = cost_defs
        .iter()
        .any(|c| matches!(c, CostDef::Loyalty { .. }));
    let mut eff_body = eff_str.trim();
    let mut timing_sorcery = has_loyalty;
    let mut activation_restrictions: Vec<ActivationRestriction> = Vec::new();
    let mut activation_condition: Option<Box<Condition>> = None;

    if let Some((main_eff, act_tail)) = split_once_ci(eff_body, ". Activate ") {
        eff_body = main_eff.trim();
        let full_tail = format!("Activate {}", act_tail.trim());
        if contains_ci(&full_tail, "only as a sorcery") {
            timing_sorcery = true;
        }
        if contains_ci(&full_tail, "only once each turn") {
            activation_restrictions.push(ActivationRestriction::OnceEachTurn);
        }
        if let Some(cond_parse) = parse_activated_if_condition_sentence(&full_tail, ctx) {
            activation_condition = Some(Box::new(cond_parse.condition));
            if let Some(r) = cond_parse.extra {
                if matches!(r, ActivatedExtraRestriction::AsASorcery) {
                    timing_sorcery = true;
                } else if let Some(ar) = map_extra_restriction(r) {
                    activation_restrictions.push(ar);
                }
            }
        }
    } else if let Some(idx) = find_ci(eff_body, " Activate ") {
        let main_eff = eff_body[..idx].trim();
        let full_tail = eff_body[idx + 1..].trim();
        eff_body = main_eff;
        if contains_ci(full_tail, "only as a sorcery") {
            timing_sorcery = true;
        }
        if contains_ci(full_tail, "only once each turn") {
            activation_restrictions.push(ActivationRestriction::OnceEachTurn);
        }
        if let Some(cond_parse) = parse_activated_if_condition_sentence(full_tail, ctx) {
            activation_condition = Some(Box::new(cond_parse.condition));
            if let Some(r) = cond_parse.extra {
                if matches!(r, ActivatedExtraRestriction::AsASorcery) {
                    timing_sorcery = true;
                } else if let Some(ar) = map_extra_restriction(r) {
                    activation_restrictions.push(ar);
                }
            }
        }
    }

    let eff_parse = parse_effects(eff_body, 0, AbilityContext::Activated, None, ctx);
    let is_mana = !eff_parse.steps.is_empty()
        && eff_parse.unparsed.is_empty()
        && eff_parse.targets.is_empty()
        && !has_loyalty
        && eff_parse
            .steps
            .iter()
            .all(|s| matches!(s.op, EffectOp::AddMana { .. }));

    if !is_mana && !statically_interpretable(&eff_parse, false) {
        ctx.interner.rollback(cp);
        return None;
    }

    let timing = if is_mana {
        Timing::Mana
    } else if timing_sorcery {
        Timing::Sorcery
    } else {
        Timing::Instant
    };

    Some(ActivatedAbility {
        id: String::new(),
        cost: cost_defs,
        targets: eff_parse.targets,
        effect: eff_parse.steps,
        modal: None,
        timing,
        is_mana,
        activation_restrictions,
        activation_condition,
        activator_who: None,
        power_up_reduction: false,
        x_cant_be_zero: false,
        activation_cost_reduction: None,
        activation_zones: ZoneMask::BATTLEFIELD,
        cost_keyword: None,
        intrinsic: false,
        text: Some(norm),
        grant_condition: None,
        x_spend_colors: ColorMask::empty(),
        x_colored_only: false,
        x_max_one_per_color: false,
        span: Span::default(),
    })
}

fn parse_granted_triggered_ability(
    quoted: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<TriggeredAbility> {
    let norm = replace_granted_self_refs(quoted);
    let t = trim_sentence(&norm);
    let cp = ctx.interner.checkpoint();
    let trig_parse = match parse_trigger_header(t, 0, ctx) {
        Some(p) => p,
        None => {
            ctx.interner.rollback(cp);
            return None;
        }
    };
    let trig = match trig_parse.trigger {
        Some(tr) => tr,
        None => {
            ctx.interner.rollback(cp);
            return None;
        }
    };
    if runtime_trigger_problem(&trig).is_some() {
        ctx.interner.rollback(cp);
        return None;
    }
    let eff_start = trig_parse.effect_start as usize;
    let eff_str = if eff_start < t.len() {
        t[eff_start..].trim()
    } else {
        ""
    };
    let (eff_clean, optional) = if let Some(rest) = strip_prefix_ci(eff_str, "you may ") {
        (capitalize_first(rest), true)
    } else {
        (eff_str.to_string(), false)
    };
    let eff_parse = parse_effects(&eff_clean, 0, AbilityContext::Triggered, Some(&trig), ctx);
    if !statically_interpretable(&eff_parse, optional) {
        ctx.interner.rollback(cp);
        return None;
    }
    let mut steps = eff_parse.steps;
    if optional && !steps.is_empty() && steps[0].optional.is_none() {
        steps[0].optional = Some(OptionalSpec {
            chooser: PlayerRef::You,
            cost: Vec::new(),
            mode: OptionalMode::DoIfPaid,
        });
    }
    let text_norm = if t.ends_with(['.', '!', '?']) {
        t.to_string()
    } else {
        format!("{t}.")
    };

    Some(TriggeredAbility {
        trigger: trig,
        condition: trig_parse.condition.map(Box::new),
        grant_condition: None,
        targets: eff_parse.targets,
        effect: steps,
        modal: None,
        text: Some(text_norm),
        span: Span::default(),
    })
}

fn extract_quoted_segments(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start: Option<usize> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if let Some(st) = start {
                out.push(s[st..i].trim());
                start = None;
            } else {
                start = Some(i + 1);
            }
        }
        i += 1;
    }
    out
}

fn parse_granted_abilities(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Vec<GrantedItem>> {
    let t = trim_clause_punct(raw);
    let cp = ctx.interner.checkpoint();

    let quotes = extract_quoted_segments(t);
    if !quotes.is_empty() {
        let mut outside = String::new();
        let mut in_q = false;
        for ch in t.chars() {
            if ch == '"' || ch == '\u{201C}' || ch == '\u{201D}' {
                in_q = !in_q;
            } else if !in_q {
                outside.push(ch);
            }
        }
        let only_connectors = outside
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .all(|w| w.eq_ignore_ascii_case("and"));
        if only_connectors {
            let mut grants = Vec::with_capacity(quotes.len());
            for q in quotes {
                let q_no_comma = q.trim_end_matches(',').trim();
                let q_clean = q.trim_end_matches([',', '.']).trim();
                if let Some(ab) = parse_granted_activated_ability(q_no_comma, ctx) {
                    grants.push(GrantedItem::Ability(Box::new(AbilityDef::Activated(ab))));
                } else if let Some(tr) = parse_granted_triggered_ability(q_no_comma, ctx) {
                    grants.push(GrantedItem::Ability(Box::new(AbilityDef::Triggered(tr))));
                } else if !q_clean.contains('"')
                    && let Some(items) = parse_sentence(q_clean, ctx)
                    && !items.is_empty()
                    && items.iter().all(|it| matches!(it, StaticItem::Static(_)))
                {
                    for it in items {
                        if let StaticItem::Static(st) = it {
                            grants.push(GrantedItem::Ability(Box::new(AbilityDef::Static(
                                mtg_ir::StaticAbility {
                                    static_def: st,
                                    span: mtg_ir::Span::default(),
                                },
                            ))));
                        }
                    }
                } else if let Some(kw) = parse_single_param_keyword_grant(q_clean, ctx) {
                    grants.push(kw);
                } else if let Some(kws) = parse_keyword_id_list(q_clean, ctx) {
                    for kw in kws {
                        grants.push(GrantedItem::Keyword(kw));
                    }
                } else {
                    ctx.interner.rollback(cp);
                    return None;
                }
            }
            if !grants.is_empty() {
                return Some(grants);
            }
        }
    }

    if let Some(kw) = parse_single_param_keyword_grant(t, ctx) {
        return Some(vec![kw]);
    }

    let mut comma_parts: Vec<String> = Vec::new();
    for raw_part in t.split(',') {
        let trimmed = raw_part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if starts_with_ci(trimmed, "where x is ")
            && let Some(prev) = comma_parts.last_mut()
        {
            prev.push_str(", ");
            prev.push_str(trimmed);
        } else {
            comma_parts.push(trimmed.to_string());
        }
    }
    if !comma_parts.is_empty() {
        let mut grants: Vec<GrantedItem> = Vec::new();
        let mut ok = true;
        for part in &comma_parts {
            let p = strip_prefix_ci(part.as_str(), "and ")
                .unwrap_or(part.as_str())
                .trim();
            if p.is_empty() {
                continue;
            }
            if starts_with_ci(p, "protection from ") {
                if let Some(norm) = is_valid_single_keyword(p, ctx) {
                    grants.push(GrantedItem::Keyword(ctx.interner.intern_keyword(&norm)));
                } else {
                    ok = false;
                    break;
                }
            } else if let Some(kw) = parse_single_param_keyword_grant(p, ctx) {
                grants.push(kw);
            } else if let Some((l, r)) = split_once_ci(p, " and ") {
                for sub in [l.trim(), r.trim()] {
                    if sub.is_empty() {
                        continue;
                    }
                    if let Some(kw) = parse_single_param_keyword_grant(sub, ctx) {
                        grants.push(kw);
                    } else if let Some(norm) = is_valid_single_keyword(sub, ctx) {
                        grants.push(GrantedItem::Keyword(ctx.interner.intern_keyword(&norm)));
                    } else {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            } else if let Some(norm) = is_valid_single_keyword(p, ctx) {
                grants.push(GrantedItem::Keyword(ctx.interner.intern_keyword(&norm)));
            } else {
                ok = false;
                break;
            }
        }
        if ok && !grants.is_empty() {
            return Some(grants);
        }
        ctx.interner.rollback(cp);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_combat_object_phrase(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<ObjectFilter> {
    let cp = ctx.interner.checkpoint();
    let trimmed = strip_leading_article(trim_clause_punct(raw));
    if trimmed.is_empty() {
        return None;
    }
    let clear_implicit_bf = |phrase: &str, f: &mut ObjectFilter| {
        if !contains_ci(phrase, "graveyard")
            && !contains_ci(phrase, "exile")
            && !contains_ci(phrase, "hand")
            && !contains_ci(phrase, "library")
            && !contains_ci(phrase, "stack")
        {
            f.zones = ZoneMask::empty();
        }
    };
    if let Some(mut f) = parse_object_phrase(trimmed, ctx, FilterOpts::default()) {
        clear_implicit_bf(trimmed, &mut f);
        return Some(f);
    }
    ctx.interner.rollback(cp);
    // Color disjunction: "white or blue creatures", "black and/or red creatures"
    if let Some((c1_w, rest1)) = trimmed.split_once(' ')
        && let Some(c1) = parse_color_word(c1_w)
    {
        let rest1 = rest1.trim_start();
        if let Some(after_or) =
            strip_prefix_ci(rest1, "or ").or_else(|| strip_prefix_ci(rest1, "and/or "))
            && let Some((c2_w, tail)) = after_or.trim_start().split_once(' ')
            && let Some(c2) = parse_color_word(c2_w)
            && let Some(mut base) = parse_object_phrase(tail.trim(), ctx, FilterOpts::default())
            && base.colors_any.is_empty()
            && base.colors_all.is_empty()
        {
            base.colors_any = c1 | c2;
            clear_implicit_bf(trimmed, &mut base);
            return Some(base);
        }
    }
    ctx.interner.rollback(cp);
    if let Some(mut f) = parse_object_with_qualifiers(trimmed, ctx, FilterOpts::default()) {
        clear_implicit_bf(trimmed, &mut f);
        return Some(f);
    }
    ctx.interner.rollback(cp);
    None
}

fn parse_inline_combat_tail(
    tail: &str,
    affected: &Affected,
    condition: Option<&Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_clause_punct(tail);
    let cp = ctx.interner.checkpoint();
    if eq_ci(t, "can't attack") || eq_ci(t, "cannot attack") {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::CantAttack,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't block") || eq_ci(t, "cannot block") {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::CantBlock,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't attack or block") {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::CantAttackOrBlock,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't be blocked") {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::CantBeBlocked,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "must be blocked if able") {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::MustBeBlockedIfAble,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't attack alone") {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantAttackAlone,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't block alone") {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBlockAlone,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can attack as though it didn't have defender")
        || eq_ci(t, "can attack as though they didn't have defender")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CanAttackDespiteDefender,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(
        t,
        "assigns combat damage equal to its toughness rather than its power",
    ) || eq_ci(
        t,
        "assign combat damage equal to their toughness rather than their power",
    ) {
        return Some(make_static(
            StaticBody::AssignCombatDamageByToughness {
                affected: affected.clone(),
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "assigns its combat damage as though it weren't blocked")
        || eq_ci(
            t,
            "assign their combat damage as though they weren't blocked",
        )
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't be blocked except by ") {
        if let Some(num_s) = strip_suffix_ci(rest, " or more creatures")
            && let Some(n) = parse_num_word_or_int(num_s.trim())
            && n >= 2
        {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::MinBlockers { min: n as u16 },
                },
                condition.cloned(),
            ));
        }
        if let Some(f) = parse_combat_object_phrase(rest, ctx) {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::CantBeBlockedExceptBy {
                        filter: Box::new(f),
                        or_filter: None,
                    },
                },
                condition.cloned(),
            ));
        }
        ctx.interner.rollback(cp);
        if let Some((left_s, right_raw)) =
            split_once_ci(rest, " and/or ").or_else(|| split_once_ci(rest, " or "))
        {
            let right_s_owned: Cow<'_, str> =
                if parse_combat_object_phrase(right_raw.trim(), ctx).is_some() {
                    ctx.interner.rollback(cp);
                    Cow::Borrowed(right_raw.trim())
                } else if let Some((head, _)) = split_once_ci(left_s.trim(), " with ") {
                    Cow::Owned(format!("{} with {}", head.trim(), right_raw.trim()))
                } else {
                    Cow::Borrowed(right_raw.trim())
                };
            if let Some(f1) = parse_combat_object_phrase(left_s.trim(), ctx)
                && let Some(f2) = parse_combat_object_phrase(&right_s_owned, ctx)
            {
                return Some(make_static(
                    StaticBody::CombatRule {
                        affected: affected.clone(),
                        rule: CombatRuleSpec::CantBeBlockedExceptBy {
                            filter: Box::new(f1),
                            or_filter: Some(Box::new(f2)),
                        },
                    },
                    condition.cloned(),
                ));
            }
        }
        ctx.interner.rollback(cp);
        return None;
    }
    if eq_ci(t, "doesn't untap during its controller's untap step")
        || eq_ci(t, "doesn't untap during your untap step")
        || eq_ci(t, "don't untap during their controllers' untap steps")
        || eq_ci(t, "don't untap during their controller's untap step")
        || eq_ci(t, "don't untap during your untap step")
    {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::DoesntUntap,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't be blocked by more than ")
        && let Some(num_s) =
            strip_suffix_ci(rest, " creatures").or_else(|| strip_suffix_ci(rest, " creature"))
        && let Some(n) = parse_num_word_or_int(num_s.trim())
        && n >= 1
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::MaxBlockers { max: n as u16 },
            },
            condition.cloned(),
        ));
    }
    if (eq_ci(t, "can't be blocked by creatures with greater power")
        || eq_ci(
            t,
            "can't be blocked by creatures with power greater than ~'s",
        )
        || eq_ci(
            t,
            "can't be blocked by creatures with power greater than ~'s power",
        )
        || eq_ci(
            t,
            "can't be blocked by creatures with power greater than its power",
        )
        || eq_ci(
            t,
            "can't be blocked by creatures with power greater than their power",
        ))
        && (!contains_ci(t, "~'s") || matches!(affected, Affected::SelfObj))
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBeBlockedByPowerGreaterThanSelf,
            },
            condition.cloned(),
        ));
    }
    if (eq_ci(t, "can't be blocked by creatures with less power")
        || eq_ci(t, "can't be blocked by creatures with power less than ~'s")
        || eq_ci(
            t,
            "can't be blocked by creatures with power less than ~'s power",
        )
        || eq_ci(
            t,
            "can't be blocked by creatures with power less than its power",
        )
        || eq_ci(
            t,
            "can't be blocked by creatures with power less than their power",
        ))
        && (!contains_ci(t, "~'s") || matches!(affected, Affected::SelfObj))
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBeBlockedByPowerLessThanSelf,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't be blocked as long as defending player controls ")
        && let Some(f) = parse_combat_object_phrase(rest, ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBeBlockedIfDefenderControls {
                    filter: Box::new(f),
                },
            },
            condition.cloned(),
        ));
    }
    ctx.interner.rollback(cp);
    if let Some(rest) = strip_prefix_ci(t, "can't be blocked by ") {
        if let Some(f) = parse_combat_object_phrase(rest, ctx) {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::CantBeBlockedBy {
                        filter: Box::new(f),
                    },
                },
                condition.cloned(),
            ));
        }
        ctx.interner.rollback(cp);
        return None;
    }
    if eq_ci(t, "attacks each combat if able")
        || eq_ci(t, "attack each combat if able")
        || eq_ci(t, "attacks each turn if able")
        || eq_ci(t, "attack each turn if able")
    {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::AttacksEachCombat,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "blocks each combat if able") || eq_ci(t, "block each combat if able") {
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::BlocksEachCombat,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can attack as though it had haste")
        || eq_ci(t, "can attack as though they had haste")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CanAttackAsThoughHaste,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't attack unless defending player controls ")
        .or_else(|| strip_prefix_ci(t, "cannot attack unless defending player controls "))
        && let Some(f) = parse_combat_object_phrase(rest, ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantAttackUnlessDefenderControls {
                    filter: Box::new(f),
                },
            },
            condition.cloned(),
        ));
    }
    ctx.interner.rollback(cp);
    if eq_ci(
        t,
        "can't attack unless you control more creatures than defending player",
    ) {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantAttackUnlessMoreCreaturesThanDefender,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(
        t,
        "can't block unless you control more creatures than attacking player",
    ) {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBlockUnlessMoreCreaturesThanAttacker,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't attack a player it has already attacked this turn")
        || eq_ci(
            t,
            "can't attack a player they have already attacked this turn",
        )
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantAttackAlreadyAttackedPlayerThisTurn,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't attack if defending player controls ")
        && let Some(f) = parse_combat_object_phrase(rest, ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantAttackIfDefenderControls {
                    filter: Box::new(f),
                },
            },
            condition.cloned(),
        ));
    }
    ctx.interner.rollback(cp);
    if let Some(rest) = strip_prefix_ci(t, "can't block if you control ")
        && let Some(f) = parse_combat_object_phrase(rest, ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBlockIfYouControl {
                    filter: Box::new(f),
                },
            },
            condition.cloned(),
        ));
    }
    ctx.interner.rollback(cp);
    if let Some(cond_s) = strip_prefix_ci(t, "can't attack unless ")
        .or_else(|| strip_prefix_ci(t, "cannot attack unless "))
        && let Some(unless_c) = parse_static_condition(cond_s.trim(), ctx)
    {
        let neg = Condition::Not {
            of: Box::new(unless_c),
        };
        let combined = match condition {
            Some(outer) => Condition::And {
                of: vec![outer.clone(), neg],
            },
            None => neg,
        };
        return Some(make_static(
            StaticBody::Restriction {
                affected: affected.clone(),
                restriction: RestrictionKind::CantAttack,
            },
            Some(combined),
        ));
    }
    ctx.interner.rollback(cp);
    if eq_ci(t, "is goaded") || eq_ci(t, "are goaded") {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::IsGoaded,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can block any number of creatures")
        || eq_ci(t, "can block any number of creatures each combat")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::AdditionalBlockCount { count: None },
            },
            condition.cloned(),
        ));
    }
    if let Some(after_each) =
        strip_prefix_ci(t, "can block an additional creature each combat for each ")
            .or_else(|| strip_prefix_ci(t, "can block an additional creature for each "))
    {
        let probe = format!("the number of {}", after_each.trim());
        if let Some(amount) = parse_count_amount(&probe, ctx) {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::AdditionalBlockCountDynamic {
                        amount: Box::new(amount),
                    },
                },
                condition.cloned(),
            ));
        }
        ctx.interner.rollback(cp);
    }
    if eq_ci(t, "can block an additional creature")
        || eq_ci(t, "can block an additional creature each combat")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::AdditionalBlockCount { count: Some(1) },
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can block ") {
        let stripped_combat = strip_suffix_ci(rest, " each combat").unwrap_or(rest).trim();
        let num_opt = if let Some(mid) = strip_prefix_ci(stripped_combat, "an additional ")
            .and_then(|m| {
                strip_suffix_ci(m, " creatures").or_else(|| strip_suffix_ci(m, " creature"))
            }) {
            parse_num_word_or_int(mid.trim())
        } else if let Some(mid) = strip_prefix_ci(stripped_combat, "up to ")
            .unwrap_or(stripped_combat)
            .strip_suffix(" additional creatures")
            .or_else(|| {
                strip_prefix_ci(stripped_combat, "up to ")
                    .unwrap_or(stripped_combat)
                    .strip_suffix(" additional creature")
            })
        {
            parse_num_word_or_int(mid.trim())
        } else {
            None
        };
        if let Some(n) = num_opt
            && n >= 1
        {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::AdditionalBlockCount {
                        count: Some(n as u16),
                    },
                },
                condition.cloned(),
            ));
        }
    }
    if eq_ci(t, "can block as though it had reach")
        || eq_ci(t, "can block as though they had reach")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CanBlockAsThoughReach { filter: None },
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can block ")
        && let Some(mid) = strip_suffix_ci(rest, " as though it had reach")
            .or_else(|| strip_suffix_ci(rest, " as though they had reach"))
        && let Some(f) = parse_combat_object_phrase(mid.trim(), ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CanBlockAsThoughReach {
                    filter: Some(Box::new(f)),
                },
            },
            condition.cloned(),
        ));
    }
    ctx.interner.rollback(cp);
    if let Some(rest) = strip_prefix_ci(t, "can block only ") {
        if let Some(f) = parse_combat_object_phrase(rest.trim(), ctx) {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::CantBlockExcept {
                        filter: Box::new(f),
                    },
                },
                condition.cloned(),
            ));
        }
        ctx.interner.rollback(cp);
    }
    if eq_ci(t, "can't block creatures with power greater than ~'s power")
        || eq_ci(t, "can't block creatures with power greater than its power")
        || eq_ci(
            t,
            "can't block creatures with power greater than their power",
        )
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBlockPowerGreaterThanSelf,
            },
            condition.cloned(),
        ));
    }
    if eq_ci(t, "can't block creatures with power less than ~'s power")
        || eq_ci(t, "can't block creatures with power less than its power")
        || eq_ci(t, "can't block creatures with power less than their power")
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: affected.clone(),
                rule: CombatRuleSpec::CantBlockPowerLessThanSelf,
            },
            condition.cloned(),
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "can't block ")
        && !eq_ci(rest, "alone")
        && !eq_ci(rest, "and can't be blocked")
        && !starts_with_ci(rest, "or be blocked")
        && !starts_with_ci(rest, "if ")
        && !starts_with_ci(rest, "unless ")
    {
        if let Some(f) = parse_combat_object_phrase(rest, ctx) {
            return Some(make_static(
                StaticBody::CombatRule {
                    affected: affected.clone(),
                    rule: CombatRuleSpec::CantBlockFilter {
                        filter: Box::new(f),
                    },
                },
                condition.cloned(),
            ));
        }
        ctx.interner.rollback(cp);
    }
    if eq_ci(t, "its activated abilities can't be activated")
        || eq_ci(
            t,
            "its activated abilities can't be activated unless they're mana abilities",
        )
    {
        return Some(make_static(
            StaticBody::CantActivateAbilities {
                affected: affected.clone(),
                allow_mana_abilities: contains_ci(t, "unless they're mana abilities"),
            },
            condition.cloned(),
        ));
    }
    None
}

fn find_outside_quotes_ci(s: &str, needle: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let nbytes = needle.as_bytes();
    if bytes.len() < nbytes.len() {
        return None;
    }
    let mut in_quotes = false;
    let mut i = 0usize;
    while i <= bytes.len() - nbytes.len() {
        let ch = match s[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        if ch == '"' || ch == '\u{201C}' || ch == '\u{201D}' {
            in_quotes = !in_quotes;
        } else if !in_quotes
            && s.is_char_boundary(i)
            && s.is_char_boundary(i + nbytes.len())
            && s[i..i + nbytes.len()].eq_ignore_ascii_case(needle)
        {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

fn parse_has_grant_tail(
    tail: &str,
    affected: &Affected,
    condition: Option<&Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_clause_punct(tail);
    if let Some((left, cond_str)) = split_once_ci(t, " as long as ")
        && let Some(inner_cond) = parse_static_condition_for_affected(cond_str, affected, ctx)
    {
        let combined = match condition {
            Some(outer) => Condition::And {
                of: vec![outer.clone(), inner_cond],
            },
            None => inner_cond,
        };
        return parse_has_grant_tail(left, affected, Some(&combined), ctx);
    }
    if let Some(grants) = parse_granted_abilities(t, ctx) {
        let mut out = Vec::new();
        let mut kw_ids: SmallVec<[KeywordId; 2]> = SmallVec::new();
        for g in grants {
            match g {
                GrantedItem::Keyword(id) => kw_ids.push(id),
                GrantedItem::Ability(ab) => {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer6,
                            affected.clone(),
                            LayerMod::GrantAbility { ability: ab },
                        ),
                        condition.cloned(),
                    ));
                }
            }
        }
        if !kw_ids.is_empty() {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    affected.clone(),
                    LayerMod::AddKeywords {
                        keywords: kw_ids,
                        protection_attachment_exemption: None,
                    },
                ),
                condition.cloned(),
            ));
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn parse_inline_static_tail(
    tail: &str,
    affected: &Affected,
    condition: Option<&Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let mut t = trim_clause_punct(tail);
    if let Some(rest) = strip_prefix_ci(t, "it ").or_else(|| strip_prefix_ci(t, "they ")) {
        t = rest.trim_start();
    }
    for sep in [", and is ", ", and are ", ", and becomes ", ", and become "] {
        if let Some(pos) = find_outside_quotes_ci(t, sep) {
            let left = t[..pos].trim();
            let right = t[pos + 5..].trim();
            if let Some(mut s_left) = parse_inline_static_tail(left, affected, condition, ctx)
                && let Some(s_right) = parse_inline_static_tail(right, affected, condition, ctx)
            {
                s_left.extend(s_right);
                return Some(s_left);
            }
        }
    }
    let cp = ctx.interner.checkpoint();
    if let Some(combat) = parse_inline_combat_tail(t, affected, condition, ctx) {
        return Some(vec![combat]);
    }
    if let Some(neg_s) = strip_prefix_ci(t, "isn't ").or_else(|| strip_prefix_ci(t, "aren't ")) {
        if let Some(parsed) = parse_type_description(neg_s.trim(), ctx)
            && !parsed.types.is_empty()
            && parsed.subtypes.is_empty()
        {
            return Some(vec![make_static(
                wrap_layer_mod(
                    Layer::Layer4,
                    affected.clone(),
                    LayerMod::RemoveTypes {
                        types: parsed.types,
                    },
                ),
                condition.cloned(),
            )]);
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rem_s) = strip_prefix_ci(t, "loses ").or_else(|| strip_prefix_ci(t, "lose ")) {
        let rem_t = rem_s.trim();
        if eq_ci(rem_t, "all abilities")
            || eq_ci(rem_t, "all other abilities")
            || eq_ci(rem_t, "all other abilities, card types, and creature types")
            || eq_ci(rem_t, "all other card types and abilities")
            || eq_ci(rem_t, "all card types and abilities")
            || eq_ci(rem_t, "all other abilities and card types")
            || eq_ci(rem_t, "all creature types and abilities")
        {
            return Some(vec![make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    affected.clone(),
                    LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                ),
                condition.cloned(),
            )]);
        }
        if let Some(kws) = parse_keyword_id_list(rem_t, ctx) {
            return Some(vec![make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    affected.clone(),
                    LayerMod::RemoveKeywords {
                        keywords: kws,
                        cant_have_or_gain: false,
                    },
                ),
                condition.cloned(),
            )]);
        }
        ctx.interner.rollback(cp);
    }
    if let Some(pt_s) = strip_prefix_ci(t, "has base power and toughness ")
        .or_else(|| strip_prefix_ci(t, "have base power and toughness "))
        .or_else(|| strip_prefix_ci(t, "has base power and base toughness "))
        .or_else(|| strip_prefix_ci(t, "have base power and base toughness "))
        .or_else(|| strip_prefix_ci(t, "base power and toughness "))
    {
        let pt_t = pt_s.trim();
        if eq_ci(pt_t, "each equal to its mana value")
            || eq_ci(pt_t, "each equal to their mana values")
            || eq_ci(pt_t, "equal to its mana value")
        {
            let mv = Amount::Stat {
                of: Ref::Affected,
                stat: StatKind::ManaValue,
                lki: false,
            };
            return Some(vec![make_static(
                wrap_layer_mod(
                    Layer::Layer7b,
                    affected.clone(),
                    LayerMod::SetBasePt {
                        power: Box::new(mv.clone()),
                        toughness: Box::new(mv),
                    },
                ),
                condition.cloned(),
            )]);
        }
        if let Some((ps, ts)) = pt_t.split_once('/')
            && let (Ok(p), Ok(tg)) = (ps.trim().parse::<i32>(), ts.trim().parse::<i32>())
        {
            return Some(vec![make_static(
                wrap_layer_mod(
                    Layer::Layer7b,
                    affected.clone(),
                    LayerMod::SetBasePt {
                        power: Box::new(fixed_amt(p)),
                        toughness: Box::new(fixed_amt(tg)),
                    },
                ),
                condition.cloned(),
            )]);
        }
    }
    if let Some(rest) = strip_prefix_ci(t, "is ")
        .or_else(|| strip_prefix_ci(t, "are "))
        .or_else(|| strip_prefix_ci(t, "becomes "))
        .or_else(|| strip_prefix_ci(t, "become "))
    {
        if let Some((types, subtypes, colors, colorless)) =
            parse_additive_types_and_colors(rest, ctx)
        {
            let mut statics = Vec::new();
            if !types.is_empty() || !subtypes.is_empty() {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer4,
                        affected.clone(),
                        LayerMod::AddTypes { types, subtypes },
                    ),
                    condition.cloned(),
                ));
            }
            if let Some(c) = colors {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer5,
                        affected.clone(),
                        LayerMod::AddColors { colors: c },
                    ),
                    condition.cloned(),
                ));
            } else if colorless {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer5,
                        affected.clone(),
                        LayerMod::SetColors {
                            colors: ColorMask::empty(),
                        },
                    ),
                    condition.cloned(),
                ));
            }
            if !statics.is_empty() {
                return Some(statics);
            }
        }
        ctx.interner.rollback(cp);

        let (rest_core, with_grants) = if let Some(idx) = find_outside_quotes_ci(rest, " with \"") {
            let q_slice = rest[idx + 6..].trim();
            if let Some(grants) = parse_has_grant_tail(q_slice, affected, condition, ctx) {
                (rest[..idx].trim(), Some(grants))
            } else {
                (rest, None)
            }
        } else {
            (rest, None)
        };
        let (before_name, set_name) = if let Some((l, r)) = split_once_ci(rest_core, " named ") {
            (l.trim(), Some(r.trim().to_string()))
        } else {
            (rest_core, None)
        };
        let (desc_s, base_pt) =
            if let Some((l, r)) = split_once_ci(before_name, " with base power and toughness ") {
                if let Some((ps, ts)) = r.trim().split_once('/')
                    && let (Ok(p), Ok(tg)) = (ps.trim().parse::<i32>(), ts.trim().parse::<i32>())
                {
                    (l.trim(), Some((p, tg)))
                } else {
                    (before_name, None)
                }
            } else {
                (before_name, None)
            };
        if let Some(parsed) = parse_type_description(desc_s, ctx) {
            let mut statics = Vec::new();
            if let Some(c) = parsed.colors {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer5,
                        affected.clone(),
                        LayerMod::SetColors { colors: c },
                    ),
                    condition.cloned(),
                ));
            } else if parsed.colorless {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer5,
                        affected.clone(),
                        LayerMod::SetColors {
                            colors: ColorMask::empty(),
                        },
                    ),
                    condition.cloned(),
                ));
            }
            if !parsed.types.is_empty() {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer4,
                        affected.clone(),
                        LayerMod::SetTypes {
                            types: parsed.types,
                            subtypes: parsed.subtypes,
                        },
                    ),
                    condition.cloned(),
                ));
            } else if !parsed.subtypes.is_empty() {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer4,
                        affected.clone(),
                        LayerMod::SetSubtypes {
                            subtypes: parsed.subtypes,
                        },
                    ),
                    condition.cloned(),
                ));
            }
            if let Some((p, tg)) = base_pt {
                statics.push(make_static(
                    wrap_layer_mod(
                        Layer::Layer7b,
                        affected.clone(),
                        LayerMod::SetBasePt {
                            power: Box::new(fixed_amt(p)),
                            toughness: Box::new(fixed_amt(tg)),
                        },
                    ),
                    condition.cloned(),
                ));
            }
            if let Some(name) = set_name {
                statics.push(make_static(
                    wrap_layer_mod(Layer::Layer3, affected.clone(), LayerMod::SetName { name }),
                    condition.cloned(),
                ));
            }
            if let Some(grants) = with_grants {
                statics.extend(grants);
            }
            if !statics.is_empty() {
                return Some(statics);
            }
        }
    }
    ctx.interner.rollback(cp);

    let grant_body = strip_prefix_ci(t, "has ")
        .or_else(|| strip_prefix_ci(t, "have "))
        .or_else(|| strip_prefix_ci(t, "gains "))
        .or_else(|| strip_prefix_ci(t, "gain "))
        .unwrap_or(t);
    if let Some(res) = parse_has_grant_tail(grant_body, affected, condition, ctx) {
        return Some(res);
    }

    if eq_ci(
        t,
        "can't be the target of spells or abilities your opponents control",
    ) || eq_ci(t, "can't be the target of spells or abilities")
    {
        let id = ctx.interner.intern_keyword(if contains_ci(t, "opponents") {
            "hexproof"
        } else {
            "shroud"
        });
        return Some(vec![make_static(
            wrap_layer_mod(
                Layer::Layer6,
                affected.clone(),
                LayerMod::AddKeywords {
                    keywords: smallvec![id],
                    protection_attachment_exemption: None,
                },
            ),
            condition.cloned(),
        )]);
    }

    // Compound predicate tails outside quotes: "<A>, and <B>" / "<A>, <B>" / "<A> and <B>"
    for sep in [", and ", ", ", " and "] {
        let mut positions = Vec::new();
        let mut scan_from = 0usize;
        while scan_from < t.len() {
            if let Some(rel) = find_outside_quotes_ci(&t[scan_from..], sep) {
                // Verify quote parity up to scan_from + rel by checking from 0 only when scan_from == 0;
                // Wait: slice at scan_from only if scan_from was right after a non-quote sep match,
                // which preserves quote parity!
                let pos = scan_from + rel;
                positions.push(pos);
                scan_from = pos + sep.len();
            } else {
                break;
            }
        }
        for pos in positions {
            let left = t[..pos].trim();
            let right = t[pos + sep.len()..].trim();
            if !left.is_empty() && !right.is_empty() {
                let cp_split = ctx.interner.checkpoint();
                if let Some(mut l_res) = parse_inline_static_tail(left, affected, condition, ctx)
                    && let Some(mut r_res) =
                        parse_inline_static_tail(right, affected, condition, ctx)
                {
                    l_res.append(&mut r_res);
                    return Some(l_res);
                }
                ctx.interner.rollback(cp_split);
            }
        }
    }

    ctx.interner.rollback(cp);
    None
}

fn split_pt_modifier_and_tail(raw: &str) -> Option<(Amount, Amount, Option<&str>)> {
    let t = raw.trim();
    let bytes = t.as_bytes();
    if bytes.is_empty() || (bytes[0] != b'+' && bytes[0] != b'-') {
        return None;
    }
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    let pt_tok = t[..end].trim_end_matches(',');
    let rest = t[end..].trim().trim_start_matches(',').trim();
    let tail = if let Some(after_and) = strip_prefix_ci(rest, "and ") {
        Some(after_and.trim())
    } else if rest.is_empty() {
        None
    } else {
        Some(rest)
    };

    if eq_ci(pt_tok, "+X/+X") {
        return Some((Amount::X, Amount::X, tail));
    }
    if eq_ci(pt_tok, "-X/-X") {
        return Some((negate_amount(Amount::X), negate_amount(Amount::X), tail));
    }
    if eq_ci(pt_tok, "+X/+0") {
        return Some((Amount::X, fixed_amt(0), tail));
    }
    if eq_ci(pt_tok, "+0/+X") {
        return Some((fixed_amt(0), Amount::X, tail));
    }
    if eq_ci(pt_tok, "-X/-0") || eq_ci(pt_tok, "-X/+0") {
        return Some((negate_amount(Amount::X), fixed_amt(0), tail));
    }
    let (p, th) = parse_pt_mod_pair(pt_tok)?;
    Some((fixed_amt(p), fixed_amt(th), tail))
}

fn split_outside_quotes_clauses(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut in_q = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_q = !in_q;
            i += 1;
            continue;
        }
        if !in_q {
            if s[i..].starts_with(", and ") {
                let piece = s[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                i += 6;
                start = i;
                continue;
            }
            if s[i..].starts_with(", ") {
                let piece = s[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                i += 2;
                start = i;
                continue;
            }
            if s[i..].starts_with(" and ") {
                let piece = s[start..i].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                i += 5;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn parse_copy_modifications(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<EtbCopySpec> {
    let cp = ctx.interner.checkpoint();
    let t = trim_clause_punct(raw);
    let (filter_part, extra_part) = if let Some((l, r)) = split_once_ci(t, ", except ") {
        (l.trim(), Some(r.trim()))
    } else {
        (t, None)
    };

    let from_gy = contains_ci(filter_part, "graveyard");
    let mut target_filter = parse_object_with_qualifiers(
        filter_part,
        ctx,
        FilterOpts {
            default_zone: ZoneMask::empty(),
            allow_plural_and_disjunction: true,
            ..Default::default()
        },
    )?;
    if from_gy {
        target_filter.zones = ZoneMask::empty();
    } else {
        if target_filter.zones.is_empty() {
            target_filter.zones = ZoneMask::BATTLEFIELD;
        }
        if starts_with_ci(filter_part.trim(), "another ")
            || starts_with_ci(filter_part.trim(), "other ")
        {
            target_filter.flags |= FilterFlags::OTHER;
        }
    }
    let mut mod_spec = CopyModificationSpec::default();
    let mut has_mod = false;

    if let Some(extra) = extra_part {
        for c in split_outside_quotes_clauses(extra) {
            if eq_ci(c, "it isn't legendary") || eq_ci(c, "it's not legendary") {
                mod_spec.remove_supertypes |= SupertypeMask::LEGENDARY;
                has_mod = true;
            } else if let Some(ew_rest) = strip_prefix_ci(c, "it enters with ") {
                let (cnt_part, cond_type) =
                    if let Some((l, r)) = split_once_ci(ew_rest, " if it's ") {
                        let type_s = strip_leading_article(r.trim());
                        let ct = parse_type_description(type_s, ctx)
                            .map(|p| p.types)
                            .unwrap_or_default();
                        (l.trim(), if ct.is_empty() { None } else { Some(ct) })
                    } else {
                        (ew_rest.trim(), None)
                    };
                if let Some(before_on) = strip_suffix_ci(cnt_part, " on it").or(Some(cnt_part))
                    && let Some((amt_ctr_s, _)) = split_once_ci(before_on, " counter")
                    && let Some((amt_s, ctr_s)) = split_once_ci(amt_ctr_s.trim(), " additional ")
                {
                    let amt = if eq_ci(amt_s.trim(), "a") || eq_ci(amt_s.trim(), "an") {
                        Some(fixed_amt(1))
                    } else {
                        let amt_tok = strip_leading_article(amt_s.trim());
                        if eq_ci(amt_tok, "x") {
                            Some(Amount::X)
                        } else {
                            parse_num_word_or_int(amt_tok).map(fixed_amt)
                        }
                    };
                    let cid = lookup_static_counter(ctr_s.trim(), ctx);
                    if let (Some(amt_val), Some(cid_val)) = (amt, cid) {
                        if let Some(ct) = cond_type {
                            if let Amount::Const { n } = amt_val {
                                mod_spec
                                    .conditional_type_counters
                                    .push(ConditionalTypeCounter {
                                        card_type: ct,
                                        counter: cid_val,
                                        count: n as u16,
                                    });
                                has_mod = true;
                            }
                        } else {
                            mod_spec.additional_counters = Some((cid_val, Box::new(amt_val)));
                            has_mod = true;
                        }
                    }
                }
            } else if let Some(kw_part) = strip_prefix_ci(c, "it has ") {
                let quoted = extract_quoted_segments(kw_part.trim());
                if !quoted.is_empty() {
                    for q in quoted {
                        mod_spec
                            .added_ability_texts
                            .push(replace_granted_self_refs(q));
                        has_mod = true;
                    }
                } else if let Some(kws) = parse_keyword_id_list(kw_part, ctx) {
                    mod_spec.add_keywords.extend(kws);
                    has_mod = true;
                }
            } else if let Some(type_part) =
                strip_prefix_ci(c, "it's a ").or_else(|| strip_prefix_ci(c, "it's an "))
            {
                if let Some((types, subtypes, colors, _)) =
                    parse_additive_types_and_colors(type_part, ctx)
                {
                    mod_spec.add_types |= types;
                    mod_spec.add_subtypes.extend(subtypes);
                    if let Some(cm) = colors {
                        mod_spec.add_colors |= cm;
                    }
                    has_mod = true;
                }
            } else if let Some(pt_str) = strip_prefix_ci(c, "it's ")
                && let Some((p, th)) = parse_fixed_pt_pair(pt_str)
            {
                mod_spec.power = Some(p);
                mod_spec.toughness = Some(th);
                has_mod = true;
            }
        }
    }

    let _ = cp;
    Some(EtbCopySpec {
        filter: Box::new(target_filter),
        optional: true,
        from_graveyards: contains_ci(filter_part, "graveyard"),
        tapped_if_copied: false,
        modifications: if has_mod {
            Some(Box::new(mod_spec))
        } else {
            None
        },
    })
}

// ============================================================================
// Section 6: Replacement, Damage Prevention, Multiplier, Rule & Zone Play
// ============================================================================

fn parse_replacement_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<ReplacementDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    let self_etb_pattern = || {
        make_zone_change_pattern(
            Some(TriggerSubject::SelfObj),
            ZoneMask::empty(),
            Some(Zone::Battlefield),
            false,
        )
    };

    if let Some(after_dbl) = strip_prefix_ci(t, "double all damage that ")
        && let Some((src_s, tail_s)) = split_once_ci(after_dbl, " would deal")
    {
        let tail_t = tail_s.trim();
        let noncombat_only = contains_ci(tail_t, "noncombat");
        let combat_only = !noncombat_only && contains_ci(tail_t, "combat");
        let src_clean = src_s.trim();
        let source_filter = if eq_ci(src_clean, "a source") || eq_ci(src_clean, "sources") {
            None
        } else if let Some(mut f) =
            parse_object_with_qualifiers(src_clean, ctx, FilterOpts::default())
        {
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            Some(Affected::Filter {
                filter: Box::new(f),
            })
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        let target_filter = if contains_ci(tail_t, "to a permanent an opponent controls") {
            DamageModTarget::OpponentOrOpponentPerm
        } else if contains_ci(tail_t, "to an opponent") {
            DamageModTarget::Opponent
        } else {
            DamageModTarget::Any
        };
        return Some(vec![make_replacement(
            make_simple_event_pattern(EventKind::DamageReplacement),
            ReplacementWith::DamageScale {
                spec: DamageScaleSpec {
                    mode: DamageModMode::Double,
                    add: None,
                    noncombat_only,
                    combat_only,
                    source_filter,
                    target_filter,
                },
            },
            false,
            None,
            condition,
        )]);
    }

    // "If a creature dealt damage by ~ this turn would die, exile it instead." (Incendiary Oracle)
    if eq_ci(
        t,
        "if a creature dealt damage by ~ this turn would die, exile it instead",
    ) || eq_ci(
        t,
        "if a creature dealt damage by this creature this turn would die, exile it instead",
    ) || eq_ci(
        t,
        "if a creature or planeswalker dealt damage by ~ this turn would die, exile it instead",
    ) {
        let types = if contains_ci(t, "planeswalker") {
            CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER
        } else {
            CardTypeMask::CREATURE
        };
        let pat = make_zone_change_pattern(
            Some(TriggerSubject::Filter {
                filter: Box::new(ObjectFilter {
                    types_any: types,
                    ..Default::default()
                }),
            }),
            ZoneMask::BATTLEFIELD,
            Some(Zone::Graveyard),
            false,
        );
        return Some(vec![make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    to: Some(Zone::Exile),
                    dealt_damage_exile_on_die: Some(mtg_ir::DealtDamageExileScope::SelfSource),
                    ..Default::default()
                }),
            },
            false,
            None,
            condition,
        )]);
    }

    // "If a spell or ability an opponent controls causes you to discard ~, put it onto the battlefield instead of putting it into your graveyard." (Loxodon Smiter)
    if eq_ci(
        t,
        "if a spell or ability an opponent controls causes you to discard ~, put it onto the battlefield instead of putting it into your graveyard",
    ) || eq_ci(
        t,
        "if a spell or ability an opponent controls causes you to discard this card, put it onto the battlefield instead of putting it into your graveyard",
    ) {
        let mut pat = make_simple_event_pattern(EventKind::Discard);
        pat.filter.object = Some(TriggerSubject::SelfObj);
        return Some(vec![make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    opponent_discard_to_battlefield: Some(
                        mtg_ir::OpponentDiscardToBattlefield::Plain,
                    ),
                    ..Default::default()
                }),
            },
            false,
            None,
            condition,
        )]);
    }

    if eq_ci(
        t,
        "if it's neither day nor night, it becomes day as ~ enters",
    ) || eq_ci(
        t,
        "if it's neither day nor night, it becomes day as ~ enters the battlefield",
    ) || eq_ci(
        t,
        "if it's neither day nor night, it becomes day as it enters",
    ) || eq_ci(
        t,
        "if it's neither day nor night, it becomes day as it enters the battlefield",
    ) || eq_ci(
        t,
        "as ~ enters, if it's neither day nor night, it becomes day",
    ) || eq_ci(
        t,
        "as ~ enters the battlefield, if it's neither day nor night, it becomes day",
    ) {
        return Some(vec![make_replacement(
            self_etb_pattern(),
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    becomes_day_if_neither: true,
                    ..Default::default()
                }),
            },
            false,
            None,
            condition,
        )]);
    }

    if let Some(after_if) = strip_prefix_ci(t, "if ")
        && let Some((cond_s, rest_s)) = after_if.split_once(',')
        && strip_self_subject(rest_s.trim()).is_some_and(|pred| starts_with_ci(pred, "enters"))
        && let Some(mut lead_c) = parse_static_condition(cond_s.trim(), ctx)
    {
        if !contains_ci(cond_s, "on the battlefield") && !contains_ci(cond_s, "battlefield") {
            strip_implicit_bf_zone_in_cond(&mut lead_c);
        }
        let combined = match condition.clone() {
            Some(outer) => Some(Condition::And {
                of: vec![outer, lead_c],
            }),
            None => Some(lead_c),
        };
        if let Some(reps) = parse_replacement_sentence(rest_s.trim(), combined, ctx) {
            return Some(reps);
        }
        ctx.interner.rollback(cp);
    }

    if let Some(pred) = strip_self_subject(t) {
        if eq_ci(pred, "enters prepared")
            || eq_ci(pred, "enters the battlefield prepared")
            || eq_ci(pred, "enters tapped and prepared")
            || eq_ci(pred, "enters the battlefield tapped and prepared")
        {
            let tapped = contains_ci(pred, "tapped");
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: if tapped { Some(true) } else { None },
                        enters_prepared: true,
                        ..Default::default()
                    }),
                },
                false,
                None,
                condition,
            )]);
        }
        if eq_ci(pred, "enters under an opponent's control")
            || eq_ci(pred, "enters the battlefield under an opponent's control")
            || eq_ci(pred, "enters tapped under an opponent's control")
            || eq_ci(
                pred,
                "enters the battlefield tapped under an opponent's control",
            )
            || eq_ci(
                pred,
                "enters under the control of an opponent of your choice",
            )
            || eq_ci(
                pred,
                "enters the battlefield under the control of an opponent of your choice",
            )
            || eq_ci(pred, "enters under the control of an opponent")
            || eq_ci(
                pred,
                "enters tapped under the control of an opponent of your choice",
            )
        {
            let tapped = contains_ci(pred, "tapped");
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: if tapped { Some(true) } else { None },
                        enter_under_opponent_control: true,
                        ..Default::default()
                    }),
                },
                false,
                None,
                condition,
            )]);
        }
        if eq_ci(pred, "enters tapped") || eq_ci(pred, "enters the battlefield tapped") {
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(true),
                        ..Default::default()
                    }),
                },
                false,
                None,
                condition,
            )]);
        }
        if eq_ci(pred, "enters untapped") || eq_ci(pred, "enters the battlefield untapped") {
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: Some(false),
                        ..Default::default()
                    }),
                },
                false,
                None,
                condition,
            )]);
        }
        if let Some(cond_str) = strip_prefix_ci(pred, "enters tapped ")
            .or_else(|| strip_prefix_ci(pred, "enters the battlefield tapped "))
        {
            if let Some(etb_cond) = parse_unless_etb_condition(cond_str, ctx) {
                let combined = match condition {
                    Some(c) => Some(Condition::And {
                        of: vec![c, etb_cond],
                    }),
                    None => Some(etb_cond),
                };
                return Some(vec![make_replacement(
                    self_etb_pattern(),
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            tapped: Some(true),
                            ..Default::default()
                        }),
                    },
                    true,
                    None,
                    combined,
                )]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(counter_tail) = strip_prefix_ci(pred, "enters tapped with ")
            .or_else(|| strip_prefix_ci(pred, "enters the battlefield tapped with "))
        {
            if let Some(mut rep) =
                parse_enters_with_counters_clause(counter_tail, condition.clone(), true, None, ctx)
            {
                if let ReplacementWith::ZoneChange { mod_spec } = &mut rep.with {
                    mod_spec.tapped = Some(true);
                }
                return Some(vec![rep]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(counter_tail) = strip_prefix_ci(pred, "enters with ")
            .or_else(|| strip_prefix_ci(pred, "enters the battlefield with "))
        {
            if let Some(base_tail) = strip_suffix_ci(counter_tail, " if you cast it from your hand")
                .or_else(|| strip_suffix_ci(counter_tail, " if you cast ~ from your hand"))
            {
                let combined = Some(match &condition {
                    Some(prev) => Condition::And {
                        of: vec![prev.clone(), Condition::WasCast { from_hand: true }],
                    },
                    None => Condition::WasCast { from_hand: true },
                });
                if let Some(rep) =
                    parse_enters_with_counters_clause(base_tail.trim(), combined, true, None, ctx)
                {
                    return Some(vec![rep]);
                }
                ctx.interner.rollback(cp);
            }
            if let Some(rep) =
                parse_enters_with_counters_clause(counter_tail, condition.clone(), true, None, ctx)
            {
                return Some(vec![rep]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(counter_tail) = strip_prefix_ci(pred, "escapes with ")
            .or_else(|| strip_prefix_ci(pred, "escapes the battlefield with "))
        {
            let combined = Some(match &condition {
                Some(prev) => Condition::And {
                    of: vec![prev.clone(), Condition::Escaped],
                },
                None => Condition::Escaped,
            });
            if let Some(rep) =
                parse_enters_with_counters_clause(counter_tail, combined, true, None, ctx)
            {
                return Some(vec![rep]);
            }
            ctx.interner.rollback(cp);
        }
    }

    if let Some(rest) = strip_prefix_ci(t, "as ")
        && let Some((subj_s, action_s)) = split_once_ci(rest, " enters, ")
            .or_else(|| split_once_ci(rest, " enters the battlefield, "))
        && is_self_phrase(subj_s.trim())
    {
        let act = action_s.trim();
        let mut choice_spec: Option<EtbChoiceSpec> = None;
        if contains_ci(act, "choose a color") {
            let exclude = split_once_ci(act, "other than ").and_then(|(_, col_s)| {
                let cw = col_s.split_whitespace().next().unwrap_or("");
                if eq_ci(cw, "white") {
                    Some(mtg_ir::ManaColor::W)
                } else if eq_ci(cw, "blue") {
                    Some(mtg_ir::ManaColor::U)
                } else if eq_ci(cw, "black") {
                    Some(mtg_ir::ManaColor::B)
                } else if eq_ci(cw, "red") {
                    Some(mtg_ir::ManaColor::R)
                } else if eq_ci(cw, "green") {
                    Some(mtg_ir::ManaColor::G)
                } else {
                    None
                }
            });
            choice_spec = Some(EtbChoiceSpec::Color { exclude });
        } else if contains_ci(act, "choose a creature type") {
            choice_spec = Some(EtbChoiceSpec::SubType {
                scope: SubTypeChoiceScope::Creature,
            });
        } else if contains_ci(act, "choose a basic land type") {
            choice_spec = Some(EtbChoiceSpec::SubType {
                scope: SubTypeChoiceScope::BasicLand,
            });
        } else if contains_ci(act, "choose a land type") {
            choice_spec = Some(EtbChoiceSpec::SubType {
                scope: SubTypeChoiceScope::Land,
            });
        } else if eq_ci(act, "choose odd or even") {
            choice_spec = Some(EtbChoiceSpec::Parity);
        } else if contains_ci(act, "choose a card name")
            || contains_ci(act, "choose a nonland card name")
        {
            choice_spec = Some(EtbChoiceSpec::CardName {
                nonland_only: contains_ci(act, "nonland"),
                look_at_opponent_hand: false,
            });
        } else if contains_ci(act, "choose an opponent") || contains_ci(act, "choose a player") {
            choice_spec = Some(EtbChoiceSpec::Player {
                opponent_only: contains_ci(act, "opponent"),
            });
        } else if let Some(list_raw) = strip_prefix_ci(act, "choose ") {
            let mut options = CardTypeMask::empty();
            let mut count = 0usize;
            let mut valid = true;
            for tok in list_raw
                .split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|s| !s.is_empty() && !eq_ci(s, "or") && !eq_ci(s, "and"))
            {
                let ct = match tok.to_ascii_lowercase().as_str() {
                    "artifact" => Some(CardTypeMask::ARTIFACT),
                    "creature" => Some(CardTypeMask::CREATURE),
                    "enchantment" => Some(CardTypeMask::ENCHANTMENT),
                    "instant" => Some(CardTypeMask::INSTANT),
                    "sorcery" => Some(CardTypeMask::SORCERY),
                    "land" => Some(CardTypeMask::LAND),
                    "planeswalker" => Some(CardTypeMask::PLANESWALKER),
                    "battle" => Some(CardTypeMask::BATTLE),
                    _ => None,
                };
                if let Some(m) = ct {
                    options |= m;
                    count += 1;
                } else {
                    valid = false;
                    break;
                }
            }
            if valid && count >= 2 {
                choice_spec = Some(EtbChoiceSpec::CardType {
                    exclude: CardTypeMask::empty(),
                    options,
                    look_at_opponent_hand: false,
                });
            }
        }
        if let Some(spec) = choice_spec {
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        etb_choices: vec![spec],
                        ..Default::default()
                    }),
                },
                true,
                None,
                condition,
            )]);
        }

        if let Some((pay_s, tap_s)) = split_once_ci(act, ". if you don't, ") {
            if let Some(life_s) = strip_prefix_ci(pay_s.trim(), "you may pay ")
                && let Some(num_s) = strip_suffix_ci(life_s.trim(), " life")
                && let Some(n) = parse_num_word_or_int(num_s)
                && contains_ci(tap_s, "enters tapped")
            {
                return Some(vec![make_replacement(
                    self_etb_pattern(),
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            pay_life_or_enter_tapped: Some(n as u16),
                            ..Default::default()
                        }),
                    },
                    true,
                    None,
                    condition,
                )]);
            }
            if let Some(rev_obj) = strip_prefix_ci(pay_s.trim(), "you may reveal ")
                && let Some((card_s, _)) = split_once_ci(rev_obj, " from your hand")
                && contains_ci(tap_s, "enters tapped")
            {
                if let Some(f) = parse_object_with_qualifiers(card_s, ctx, FilterOpts::default()) {
                    return Some(vec![make_replacement(
                        self_etb_pattern(),
                        ReplacementWith::ZoneChange {
                            mod_spec: Box::new(ZoneChangeMod {
                                reveal_from_hand_or_enter_tapped: Some(Box::new(f)),
                                ..Default::default()
                            }),
                        },
                        true,
                        None,
                        condition,
                    )]);
                }
                ctx.interner.rollback(cp);
            }
        }
        if let Some((pay_s, tap_s)) = split_once_ci(act, ". ~ enters tapped unless ")
            && let Some(rev_obj) = strip_prefix_ci(pay_s.trim(), "you may reveal ")
            && let Some((card_s, _)) = split_once_ci(rev_obj, " from your hand")
            && let Some((_, ctrl_s)) = split_once_ci(tap_s, " or you control ")
        {
            if let Some(f_hand) = parse_object_with_qualifiers(card_s, ctx, FilterOpts::default())
                && let Some(f_ctrl) =
                    parse_object_with_qualifiers(ctrl_s.trim(), ctx, FilterOpts::default())
            {
                return Some(vec![make_replacement(
                    self_etb_pattern(),
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            reveal_from_hand_or_enter_tapped: Some(Box::new(f_hand)),
                            reveal_unless_controls: Some(Box::new(f_ctrl)),
                            ..Default::default()
                        }),
                    },
                    true,
                    None,
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }

        if let Some(copy_rest) = strip_prefix_ci(act, "you may have ")
            && let Some((copy_tapped, copy_target)) =
                split_once_ci(copy_rest, " enter tapped as a copy of ")
                    .or_else(|| {
                        split_once_ci(copy_rest, " enter the battlefield tapped as a copy of ")
                    })
                    .map(|(_, r)| (true, r))
                    .or_else(|| {
                        split_once_ci(copy_rest, " enter as a copy of ")
                            .or_else(|| {
                                split_once_ci(copy_rest, " enter the battlefield as a copy of ")
                            })
                            .map(|(_, r)| (false, r))
                    })
        {
            if let Some(mut spec) = parse_copy_modifications(copy_target, ctx) {
                let from_gy = spec.from_graveyards;
                if from_gy && copy_tapped {
                    spec.tapped_if_copied = true;
                }
                return Some(vec![make_replacement(
                    self_etb_pattern(),
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            tapped: if copy_tapped && !from_gy {
                                Some(true)
                            } else {
                                None
                            },
                            copy: Some(spec),
                            ..Default::default()
                        }),
                    },
                    !from_gy || copy_tapped,
                    None,
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
    }

    if let Some(copy_rest) = strip_prefix_ci(t, "you may have ")
        && let Some((copy_tapped, subj_s, copy_target)) =
            split_once_ci(copy_rest, " enter tapped as a copy of ")
                .or_else(|| split_once_ci(copy_rest, " enter the battlefield tapped as a copy of "))
                .map(|(l, r)| (true, l, r))
                .or_else(|| {
                    split_once_ci(copy_rest, " enter as a copy of ")
                        .or_else(|| {
                            split_once_ci(copy_rest, " enter the battlefield as a copy of ")
                        })
                        .map(|(l, r)| (false, l, r))
                })
        && is_self_phrase(subj_s.trim())
    {
        if let Some(mut spec) = parse_copy_modifications(copy_target, ctx) {
            let from_gy = spec.from_graveyards;
            if from_gy && copy_tapped {
                spec.tapped_if_copied = true;
            }
            return Some(vec![make_replacement(
                self_etb_pattern(),
                ReplacementWith::ZoneChange {
                    mod_spec: Box::new(ZoneChangeMod {
                        tapped: if copy_tapped && !from_gy {
                            Some(true)
                        } else {
                            None
                        },
                        copy: Some(spec),
                        ..Default::default()
                    }),
                },
                !from_gy || copy_tapped,
                None,
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some((subj_s, etb_tail)) =
        split_once_ci(t, " enter ").or_else(|| split_once_ci(t, " enters "))
        && !is_self_phrase(subj_s.trim())
        && !starts_with_ci(subj_s.trim(), "if ")
    {
        if let Some(mut f) = parse_object_with_qualifiers(
            subj_s,
            ctx,
            FilterOpts {
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
        ) {
            f.zones = ZoneMask::empty();
            let tail =
                strip_prefix_ci(etb_tail.trim(), "the battlefield ").unwrap_or(etb_tail.trim());
            let pat = make_zone_change_pattern(
                Some(TriggerSubject::Filter {
                    filter: Box::new(f.clone()),
                }),
                ZoneMask::empty(),
                Some(Zone::Battlefield),
                false,
            );
            if eq_ci(tail, "tapped") {
                return Some(vec![make_replacement(
                    pat,
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            tapped: Some(true),
                            ..Default::default()
                        }),
                    },
                    false,
                    None,
                    condition,
                )]);
            }
            if eq_ci(tail, "untapped") {
                return Some(vec![make_replacement(
                    pat,
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            tapped: Some(false),
                            ..Default::default()
                        }),
                    },
                    false,
                    None,
                    condition,
                )]);
            }
            if let Some(with_tail) = strip_prefix_ci(tail, "with ")
                && let Some(rep) = parse_enters_with_counters_clause(
                    with_tail,
                    condition.clone(),
                    false,
                    Some(f),
                    ctx,
                )
            {
                return Some(vec![rep]);
            }
        }
        ctx.interner.rollback(cp);
    }

    if let Some(if_rest) = strip_prefix_ci(t, "if ")
        && let Some((event_part, rep_part)) = split_once_ci(if_rest, ", ")
    {
        if let Some(k_subj) = strip_suffix_ci(event_part.trim(), " was kicked")
            && is_self_phrase(k_subj.trim())
            && let Some((rep_subj, counter_tail)) = split_once_ci(rep_part.trim(), " enters with ")
                .or_else(|| split_once_ci(rep_part.trim(), " enters the battlefield with "))
            && is_self_phrase(rep_subj.trim())
        {
            let kicked_cond = Condition::Kicked {
                tag: None,
                min_count: None,
            };
            let combined = Some(match condition.clone() {
                Some(outer) => Condition::And {
                    of: vec![outer, kicked_cond],
                },
                None => kicked_cond,
            });
            if let Some(rep) =
                parse_enters_with_counters_clause(counter_tail.trim(), combined, true, None, ctx)
            {
                return Some(vec![rep]);
            }
            ctx.interner.rollback(cp);
        }

        if let Some(rep) = parse_if_would_zone_change_clause(
            event_part.trim(),
            rep_part.trim(),
            condition.clone(),
            ctx,
        ) {
            return Some(vec![rep]);
        }
        ctx.interner.rollback(cp);

        if contains_ci(event_part, "would deal ") && contains_ci(event_part, "damage") {
            if let Some(rep) = parse_damage_scale_replacement(
                event_part.trim(),
                rep_part.trim(),
                condition.clone(),
                ctx,
            ) {
                return Some(vec![rep]);
            }
            ctx.interner.rollback(cp);
        }

        if let Some((draw_ev, draw_cond)) = split_once_ci(event_part, " while ")
            .map(|(a, b)| (a.trim(), Some(b.trim())))
            .or(Some((event_part.trim(), None)))
            && eq_ci(draw_ev, "you would draw a card")
            && contains_ci(rep_part, "instead")
            && contains_ci(rep_part, "draw two cards")
        {
            let merged_cond = match draw_cond {
                Some(cs) => {
                    let parsed = parse_static_condition(cs, ctx)?;
                    Some(match condition {
                        Some(outer) => Condition::And {
                            of: vec![outer, parsed],
                        },
                        None => parsed,
                    })
                }
                None => condition,
            };
            return Some(vec![make_replacement(
                make_simple_event_pattern(EventKind::DrawReplacement),
                ReplacementWith::DrawScale {
                    spec: DrawScaleSpec {
                        factor: Some(2),
                        delta: None,
                        skip_first_in_your_draw_step: false,
                    },
                },
                false,
                Some(PlayerFilter {
                    relation: PlayerRelation::You,
                }),
                merged_cond,
            )]);
        }

        if contains_ci(event_part, "you would gain life") {
            if let Some(scale) = parse_multiplier_factor_clause(rep_part.trim()) {
                return Some(vec![make_replacement(
                    make_simple_event_pattern(EventKind::LifeGainReplacement),
                    ReplacementWith::LifeGainScale {
                        spec: LifeGainScaleSpec {
                            double: scale == 2,
                            add: None,
                        },
                    },
                    false,
                    None,
                    condition,
                )]);
            }
            if let Some((_, plus_s)) = split_once_ci(rep_part.trim(), "that much life plus ")
                && let Some(num_tok) = plus_s.split_whitespace().next()
                && let Some(n) = parse_num_word_or_int(num_tok)
            {
                let mut pat = make_simple_event_pattern(EventKind::LifeGainReplacement);
                pat.filter.player = Some(PlayerFilter {
                    relation: PlayerRelation::You,
                });
                return Some(vec![make_replacement(
                    pat,
                    ReplacementWith::LifeGainScale {
                        spec: LifeGainScaleSpec {
                            double: false,
                            add: Some(Box::new(fixed_amt(n))),
                        },
                    },
                    false,
                    None,
                    condition,
                )]);
            }
        }

        if contains_ci(event_part, "you would proliferate")
            && contains_ci(rep_part, "proliferate twice instead")
        {
            let mut pat = make_simple_event_pattern(EventKind::ProliferateReplacement);
            pat.filter.player = Some(PlayerFilter {
                relation: PlayerRelation::You,
            });
            return Some(vec![make_replacement(
                pat,
                ReplacementWith::ProliferateMultiplier { factor: 2 },
                false,
                None,
                condition,
            )]);
        }

        if contains_ci(event_part, "an opponent would draw")
            && contains_ci(rep_part, "instead")
            && (contains_ci(rep_part, "you draw") || contains_ci(rep_part, "draw two"))
        {
            let mut pat = make_simple_event_pattern(EventKind::DrawReplacement);
            pat.filter.player = Some(PlayerFilter {
                relation: PlayerRelation::Opponent,
            });
            return Some(vec![make_replacement(
                pat,
                ReplacementWith::StealOpponentExtraDraws,
                false,
                None,
                condition,
            )]);
        }
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_enters_with_counters_clause(
    raw: &str,
    condition: Option<Condition>,
    self_only: bool,
    filter: Option<ObjectFilter>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ReplacementDef> {
    let cp = ctx.interner.checkpoint();
    let t = trim_clause_punct(raw);
    let additional = contains_ci(t, "an additional ") || contains_ci(t, "additional ");
    let cleaned = strip_prefix_ci(t, "an additional ")
        .or_else(|| strip_prefix_ci(t, "additional "))
        .unwrap_or(t)
        .trim();

    let (left_part, suffix_part) = if let Some((l, r)) = split_once_ci(cleaned, " on it") {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = split_once_ci(cleaned, " on them") {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = split_once_ci(cleaned, " on ~") {
        (l.trim(), r.trim())
    } else {
        (cleaned, "")
    };

    let mut final_cond = condition;
    let mut count_suffix = suffix_part;
    if let Some((before_cond, cond_s)) = strip_prefix_ci(suffix_part, "if ")
        .map(|r| ("", r))
        .or_else(|| split_once_ci(suffix_part, " if "))
    {
        if let Some(c) = parse_static_condition(cond_s, ctx) {
            final_cond = Some(match final_cond {
                Some(prev) => Condition::And { of: vec![prev, c] },
                None => c,
            });
            count_suffix = before_cond.trim();
        } else {
            ctx.interner.rollback(cp);
            return None;
        }
    }

    let (count_amt, cid) = if let Some(rest) = strip_prefix_ci(left_part, "a number of ") {
        (fixed_amt(1), lookup_static_counter(rest, ctx)?)
    } else if let Some(rest) = strip_prefix_ci(left_part, "twice x ") {
        (
            scale_amount(Amount::X, 2),
            lookup_static_counter(rest, ctx)?,
        )
    } else if let Some(rest) =
        strip_prefix_ci(left_part, "a ").or_else(|| strip_prefix_ci(left_part, "an "))
    {
        (fixed_amt(1), lookup_static_counter(rest, ctx)?)
    } else if eq_ci(left_part, "X +1/+1 counters") {
        (Amount::X, lookup_static_counter("+1/+1", ctx)?)
    } else if let Some((num_tok, rest)) = left_part.split_once(' ') {
        if eq_ci(num_tok, "X") {
            (Amount::X, lookup_static_counter(rest, ctx)?)
        } else if let Some(n) = parse_num_word_or_int(num_tok) {
            (fixed_amt(n), lookup_static_counter(rest, ctx)?)
        } else {
            (fixed_amt(1), lookup_static_counter(left_part, ctx)?)
        }
    } else {
        ctx.interner.rollback(cp);
        return None;
    };

    let subj = if self_only {
        Some(TriggerSubject::SelfObj)
    } else {
        Some(TriggerSubject::Filter {
            filter: Box::new(filter.unwrap_or_default()),
        })
    };

    if let Some(each_rest) = strip_prefix_ci(count_suffix, "for each ")
        && (eq_ci(
            each_rest.trim(),
            "mana from an artifact source spent to cast it",
        ) || eq_ci(
            each_rest.trim(),
            "mana from an artifact source spent to cast that spell",
        ))
    {
        return Some(make_replacement(
            make_zone_change_pattern(subj, ZoneMask::empty(), Some(Zone::Battlefield), false),
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    counters_per_artifact_mana: Some(cid),
                    ..Default::default()
                }),
            },
            false,
            None,
            final_cond,
        ));
    }

    let clean_suffix = count_suffix.trim_start_matches(',').trim();
    let final_amt = if let Some(wx) = strip_prefix_ci(clean_suffix, "where x is ") {
        let x_amt = parse_count_amount(wx, ctx)?;
        match count_amt {
            Amount::X => x_amt,
            Amount::Mul { factor, of } if matches!(*of, Amount::X) => scale_amount(x_amt, factor),
            other => other,
        }
    } else if let Some(eq_rest) = strip_prefix_ci(clean_suffix, "equal to ") {
        parse_count_amount(eq_rest, ctx)?
    } else if let Some(each_rest) = strip_prefix_ci(clean_suffix, "for each ") {
        let each_t = each_rest.trim();
        let amt = if eq_ci(each_t, "time it was kicked") || eq_ci(each_t, "times it was kicked") {
            Amount::KickedCount {
                tag: None,
                multiplier: None,
            }
        } else if eq_ci(each_t, "color of mana spent to cast it")
            || eq_ci(each_t, "colors of mana spent to cast it")
        {
            Amount::ColorsOfManaSpentToCast
        } else {
            parse_count_amount(&format!("the number of {each_t}"), ctx)?
        };
        if let Amount::Const { n } = count_amt {
            scale_amount(amt, n)
        } else {
            amt
        }
    } else {
        count_amt
    };

    let self_rep = self_only && matches!(final_amt, Amount::GreatestOpponentCardsDrawnThisTurn);
    let mod_spec = if additional {
        ZoneChangeMod {
            additional_counters: Some((cid, Box::new(final_amt))),
            ..Default::default()
        }
    } else {
        ZoneChangeMod {
            counters: Some((cid, Box::new(final_amt))),
            ..Default::default()
        }
    };
    Some(make_replacement(
        make_zone_change_pattern(subj, ZoneMask::empty(), Some(Zone::Battlefield), false),
        ReplacementWith::ZoneChange {
            mod_spec: Box::new(mod_spec),
        },
        self_rep,
        None,
        final_cond,
    ))
}

fn parse_if_would_zone_change_clause(
    event_part: &str,
    rep_part: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ReplacementDef> {
    let cp = ctx.interner.checkpoint();
    let (subj_s, to_part) = if let Some((l, r)) = split_once_ci(event_part, " would be put into ") {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = split_once_ci(event_part, " would die") {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = split_once_ci(event_part, " would leave the battlefield") {
        (l.trim(), r.trim())
    } else {
        return None;
    };

    let self_only = is_self_phrase(subj_s);
    let from = if contains_ci(event_part, "would die")
        || contains_ci(event_part, "from the battlefield")
    {
        ZoneMask::BATTLEFIELD
    } else {
        ZoneMask::empty()
    };
    let to = if contains_ci(event_part, "into a graveyard")
        || contains_ci(event_part, "opponent's graveyard")
        || contains_ci(event_part, "would die")
    {
        Some(Zone::Graveyard)
    } else {
        None
    };

    let opponent_gy = contains_ci(to_part, "opponent's graveyard");
    let subj = if self_only {
        Some(TriggerSubject::SelfObj)
    } else if eq_ci(subj_s, "a card or token") || (eq_ci(subj_s, "a card") && !opponent_gy) {
        None
    } else if eq_ci(subj_s, "a card") && opponent_gy {
        Some(TriggerSubject::Filter {
            filter: Box::new(ObjectFilter {
                flags: FilterFlags::NONTOKEN,
                ..Default::default()
            }),
        })
    } else if let Some(mut f) = parse_object_with_qualifiers(subj_s, ctx, FilterOpts::default()) {
        f.zones = ZoneMask::empty();
        Some(TriggerSubject::Filter {
            filter: Box::new(f),
        })
    } else {
        ctx.interner.rollback(cp);
        return None;
    };

    let mut pat = make_zone_change_pattern(subj, from, to, false);
    if opponent_gy {
        pat.filter.player = Some(PlayerFilter {
            relation: PlayerRelation::Opponent,
        });
    }
    let rep_clean = trim_clause_punct(rep_part);

    if contains_ci(rep_clean, "shuffle ")
        && contains_ci(rep_clean, "into its owner's library instead")
    {
        return Some(make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    to: Some(Zone::Library),
                    shuffle_into_library: true,
                    ..Default::default()
                }),
            },
            self_only,
            None,
            condition,
        ));
    }
    if contains_ci(rep_clean, "exile it and take an extra turn") {
        return Some(make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    to: Some(Zone::Exile),
                    exile_and_extra_turn_on_bf_to_gy: true,
                    ..Default::default()
                }),
            },
            self_only,
            None,
            condition,
        ));
    }
    if contains_ci(rep_clean, "exile it instead")
        || contains_ci(rep_clean, "exile that card instead")
        || contains_ci(rep_clean, "instead exile it")
        || contains_ci(rep_clean, "instead exile that card")
        || (contains_ci(rep_clean, "instead exile it with a ")
            && contains_ci(rep_clean, " counter on it"))
        || (contains_ci(rep_clean, "exile it with a ") && contains_ci(rep_clean, " counter on it"))
    {
        if let Some((_, counter_tail)) = split_once_ci(rep_clean, " with a ") {
            if let Some(cid) = lookup_static_counter(
                counter_tail.split(" on it").next().unwrap_or(counter_tail),
                ctx,
            ) {
                return Some(make_replacement(
                    pat,
                    ReplacementWith::ZoneChange {
                        mod_spec: Box::new(ZoneChangeMod {
                            to: Some(Zone::Exile),
                            exile_with_counter: Some(cid),
                            ..Default::default()
                        }),
                    },
                    self_only,
                    None,
                    condition,
                ));
            }
            ctx.interner.rollback(cp);
        }
        return Some(make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    to: Some(Zone::Exile),
                    ..Default::default()
                }),
            },
            self_only,
            None,
            condition,
        ));
    }
    if contains_ci(rep_clean, "put it into its owner's hand instead") {
        return Some(make_replacement(
            pat,
            ReplacementWith::ZoneChange {
                mod_spec: Box::new(ZoneChangeMod {
                    to: Some(Zone::Hand),
                    ..Default::default()
                }),
            },
            self_only,
            None,
            condition,
        ));
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_damage_scale_replacement(
    event_part: &str,
    rep_part: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ReplacementDef> {
    let cp = ctx.interner.checkpoint();
    let (src_str, dmg_tail) = split_once_ci(event_part, " would deal ")?;
    let source_filter = if eq_ci(src_str.trim(), "a source you control") {
        Some(Affected::Filter {
            filter: Box::new(ObjectFilter {
                controller: Some(RelativeControl::You),
                ..Default::default()
            }),
        })
    } else if !eq_ci(src_str.trim(), "a source") {
        parse_affected(src_str, ctx)
    } else {
        None
    };

    let noncombat_only = contains_ci(dmg_tail, "noncombat damage");
    let combat_only = !noncombat_only && contains_ci(dmg_tail, "combat damage");

    let (mode, add) =
        if contains_ci(rep_part, "twice that much") || contains_ci(rep_part, "double that") {
            (DamageModMode::Double, None)
        } else if contains_ci(rep_part, "triple that much")
            || contains_ci(rep_part, "three times that much")
        {
            (DamageModMode::Triple, None)
        } else if let Some((_, expr_raw)) = split_once_ci(
            rep_part,
            "that much damage plus an amount of damage equal to ",
        ) {
            let expr_clean = strip_suffix_ci(trim_clause_punct(expr_raw), " instead")
                .unwrap_or(trim_clause_punct(expr_raw))
                .trim();
            let amt = parse_count_amount(expr_clean, ctx)?;
            (DamageModMode::Add, Some(Box::new(amt)))
        } else if let Some((_, plus_s)) = split_once_ci(rep_part, "that much damage plus ") {
            let num_tok = plus_s.split_whitespace().next().unwrap_or("");
            let n = parse_num_word_or_int(num_tok)?;
            (DamageModMode::Add, Some(Box::new(fixed_amt(n))))
        } else {
            ctx.interner.rollback(cp);
            return None;
        };

    let target_filter = if contains_ci(dmg_tail, "permanent an opponent controls") {
        DamageModTarget::OpponentOrOpponentPerm
    } else if contains_ci(dmg_tail, "to an opponent") {
        DamageModTarget::Opponent
    } else {
        DamageModTarget::Any
    };

    Some(make_replacement(
        make_simple_event_pattern(EventKind::DamageReplacement),
        ReplacementWith::DamageScale {
            spec: DamageScaleSpec {
                mode,
                add,
                noncombat_only,
                combat_only,
                source_filter,
                target_filter,
            },
        },
        false,
        None,
        condition,
    ))
}

fn parse_multiplier_factor_clause(rep_part: &str) -> Option<u32> {
    if contains_ci(rep_part, "twice that") || contains_ci(rep_part, "twice as much") {
        Some(2)
    } else if contains_ci(rep_part, "three times that") || contains_ci(rep_part, "triple that") {
        Some(3)
    } else {
        None
    }
}

fn parse_damage_prevention_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if eq_ci(t, "damage can't be prevented") || eq_ci(t, "combat damage can't be prevented") {
        let combat_only = starts_with_ci(t, "combat ");
        return Some(vec![make_static(
            StaticBody::UnpreventableDamage {
                combat_only,
                source_filter: None,
            },
            condition,
        )]);
    }
    if let Some(after_dmg) = strip_prefix_ci(t, "combat damage that would be dealt by ")
        .or_else(|| strip_prefix_ci(t, "damage that would be dealt by "))
        && let Some(src_s) = strip_suffix_ci(after_dmg.trim(), " can't be prevented")
    {
        let combat_only = starts_with_ci(t, "combat ");
        if let Some(mut aff) = parse_affected(src_s.trim(), ctx) {
            if let Affected::Filter { filter } = &mut aff {
                filter.zones = ZoneMask::empty();
            }
            return Some(vec![make_static(
                StaticBody::UnpreventableDamage {
                    combat_only,
                    source_filter: Some(aff),
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some(rest) = strip_prefix_ci(t, "prevent all ") {
        let (combat_only, noncombat_only, after_dmg) =
            if let Some(r) = strip_prefix_ci(rest, "combat damage ") {
                (true, false, r.trim())
            } else if let Some(r) = strip_prefix_ci(rest, "noncombat damage ") {
                (false, true, r.trim())
            } else {
                let r = strip_prefix_ci(rest, "damage ")?;
                (false, false, r.trim())
            };

        if let Some(target_s) = strip_prefix_ci(after_dmg, "that would be dealt to ") {
            let target_clean = strip_suffix_ci(target_s, " this turn")
                .unwrap_or(target_s)
                .trim();
            if let Some(rest_after_you) = strip_prefix_ci(target_clean, "you and ")
                && let Some(mut f) =
                    parse_object_with_qualifiers(rest_after_you.trim(), ctx, FilterOpts::default())
            {
                f.zones = ZoneMask::empty();
                return Some(vec![
                    make_static(
                        StaticBody::StaticDamagePrevention {
                            amount: StaticDamageAmount::All,
                            combat_only,
                            noncombat_only,
                            target: StaticDamageTarget::You,
                            also_prevent_dealt_by: false,
                            dealt_by_only: None,
                            dealt_by_target_filter: None,
                            source_filter: None,
                            on_prevented: None,
                            phantom_counter_removal: None,
                            prevent_opponent_only: false,
                        },
                        condition.clone(),
                    ),
                    make_static(
                        StaticBody::StaticDamagePrevention {
                            amount: StaticDamageAmount::All,
                            combat_only,
                            noncombat_only,
                            target: StaticDamageTarget::Filter {
                                filter: Box::new(f),
                            },
                            also_prevent_dealt_by: false,
                            dealt_by_only: None,
                            dealt_by_target_filter: None,
                            source_filter: None,
                            on_prevented: None,
                            phantom_counter_removal: None,
                            prevent_opponent_only: false,
                        },
                        condition,
                    ),
                ]);
            }
            let target = if is_self_phrase(target_clean) {
                StaticDamageTarget::SelfObj
            } else if eq_ci(target_clean, "you") {
                StaticDamageTarget::You
            } else if let Some(mut f) =
                parse_object_with_qualifiers(target_clean, ctx, FilterOpts::default())
            {
                f.zones = ZoneMask::empty();
                StaticDamageTarget::Filter {
                    filter: Box::new(f),
                }
            } else {
                ctx.interner.rollback(cp);
                return None;
            };
            return Some(vec![make_static(
                StaticBody::StaticDamagePrevention {
                    amount: StaticDamageAmount::All,
                    combat_only,
                    noncombat_only,
                    target,
                    also_prevent_dealt_by: false,
                    dealt_by_only: None,
                    dealt_by_target_filter: None,
                    source_filter: None,
                    on_prevented: None,
                    phantom_counter_removal: None,
                    prevent_opponent_only: false,
                },
                condition,
            )]);
        }
    }

    if let Some(after_if) = strip_prefix_ci(t, "if ~ would deal ")
        .or_else(|| strip_prefix_ci(t, "if this creature would deal "))
    {
        let (combat_only, after_dmg) =
            if let Some(r) = strip_prefix_ci(after_if, "combat damage to ") {
                (true, r)
            } else if let Some(r) = strip_prefix_ci(after_if, "damage to ") {
                (false, r)
            } else {
                (false, "")
            };
        if !after_dmg.is_empty()
            && let Some((targ_s, rep_s)) = split_once_ci(after_dmg, ", ")
            && contains_ci(rep_s, "prevent that damage")
            && contains_ci(rep_s, "shuffles it into their library")
            && let Some(mut f) = parse_object_with_qualifiers(
                strip_leading_article(targ_s.trim()),
                ctx,
                FilterOpts::default(),
            )
        {
            f.zones = ZoneMask::empty();
            return Some(vec![make_static(
                StaticBody::StaticDamagePrevention {
                    amount: StaticDamageAmount::All,
                    combat_only,
                    noncombat_only: false,
                    target: StaticDamageTarget::Filter {
                        filter: Box::new(f),
                    },
                    also_prevent_dealt_by: false,
                    dealt_by_only: Some(Affected::SelfObj),
                    dealt_by_target_filter: None,
                    source_filter: None,
                    on_prevented: Some(StaticOnPrevented::ShuffleTargetIntoLibrary),
                    phantom_counter_removal: None,
                    prevent_opponent_only: false,
                },
                condition,
            )]);
        }
    }

    if let Some(rest) = strip_prefix_ci(t, "if a source would deal damage to ")
        && let Some((targ_s, rep_s)) = split_once_ci(rest, ", ")
        && eq_ci(targ_s.trim(), "you or a permanent you control")
        && contains_ci(rep_s, "prevent half that damage, rounded up")
    {
        return Some(vec![make_static(
            StaticBody::StaticDamagePrevention {
                amount: StaticDamageAmount::HalfRoundedUp,
                combat_only: false,
                noncombat_only: false,
                target: StaticDamageTarget::YouAndControlled,
                also_prevent_dealt_by: false,
                dealt_by_only: None,
                dealt_by_target_filter: None,
                source_filter: None,
                on_prevented: None,
                phantom_counter_removal: None,
                prevent_opponent_only: false,
            },
            condition,
        )]);
    }

    if let Some(rest) = strip_prefix_ci(t, "if damage would be dealt to ")
        && let Some((targ_s, rep_s)) = split_once_ci(rest, ", ")
        && contains_ci(rep_s, "prevent that damage")
    {
        let target = if is_self_phrase(targ_s.trim()) {
            StaticDamageTarget::SelfObj
        } else if let Some(f) =
            parse_object_with_qualifiers(targ_s.trim(), ctx, FilterOpts::default())
        {
            StaticDamageTarget::Filter {
                filter: Box::new(f),
            }
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        return Some(vec![make_static(
            StaticBody::StaticDamagePrevention {
                amount: StaticDamageAmount::All,
                combat_only: false,
                noncombat_only: false,
                target,
                also_prevent_dealt_by: false,
                dealt_by_only: None,
                dealt_by_target_filter: None,
                source_filter: None,
                on_prevented: None,
                phantom_counter_removal: None,
                prevent_opponent_only: false,
            },
            condition,
        )]);
    }

    if let Some(rest) = strip_prefix_ci(t, "prevent ")
        && let Some((num_s, tail_s)) = split_once_ci(rest, " of the damage that would be dealt to ")
        && let Some(n) = parse_num_word_or_int(num_s)
    {
        let target = if is_self_phrase(tail_s.trim()) {
            StaticDamageTarget::SelfObj
        } else if let Some(f) =
            parse_object_with_qualifiers(tail_s.trim(), ctx, FilterOpts::default())
        {
            StaticDamageTarget::Filter {
                filter: Box::new(f),
            }
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        return Some(vec![make_static(
            StaticBody::StaticDamagePrevention {
                amount: StaticDamageAmount::Fixed { n: n as u16 },
                combat_only: false,
                noncombat_only: false,
                target,
                also_prevent_dealt_by: false,
                dealt_by_only: None,
                dealt_by_target_filter: None,
                source_filter: None,
                on_prevented: None,
                phantom_counter_removal: None,
                prevent_opponent_only: false,
            },
            condition,
        )]);
    }

    // "Prevent all [combat|noncombat] damage that would be dealt [to and dealt by | by | to] <subject>"
    if let Some(after_prev) = strip_prefix_ci(t, "prevent all ") {
        let (combat_only, noncombat_only, rest) =
            if let Some(r) = strip_prefix_ci(after_prev, "combat damage ") {
                (true, false, r)
            } else if let Some(r) = strip_prefix_ci(after_prev, "noncombat damage ") {
                (false, true, r)
            } else if let Some(r) = strip_prefix_ci(after_prev, "damage ") {
                (false, false, r)
            } else {
                (false, false, "")
            };
        if let Some(subj) = strip_prefix_ci(rest, "that would be dealt to and dealt by ") {
            if let Some(aff) = parse_affected(subj.trim(), ctx) {
                let target = match aff {
                    Affected::SelfObj => StaticDamageTarget::SelfObj,
                    Affected::AttachedTo => StaticDamageTarget::AttachedTo,
                    Affected::Filter { filter } => StaticDamageTarget::Filter { filter },
                };
                return Some(vec![make_static(
                    StaticBody::StaticDamagePrevention {
                        amount: StaticDamageAmount::All,
                        combat_only,
                        noncombat_only,
                        target,
                        also_prevent_dealt_by: true,
                        dealt_by_only: None,
                        dealt_by_target_filter: None,
                        source_filter: None,
                        on_prevented: None,
                        phantom_counter_removal: None,
                        prevent_opponent_only: false,
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(subj) = strip_prefix_ci(rest, "that would be dealt by ") {
            if let Some(aff) = parse_affected(subj.trim(), ctx) {
                return Some(vec![make_static(
                    StaticBody::StaticDamagePrevention {
                        amount: StaticDamageAmount::All,
                        combat_only,
                        noncombat_only,
                        target: StaticDamageTarget::SelfObj,
                        also_prevent_dealt_by: false,
                        dealt_by_only: Some(aff),
                        dealt_by_target_filter: None,
                        source_filter: None,
                        on_prevented: None,
                        phantom_counter_removal: None,
                        prevent_opponent_only: false,
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(subj) = strip_prefix_ci(rest, "that would be dealt to ") {
            let s_trim = subj.trim();
            let target_opt = if eq_ci(s_trim, "you") {
                Some(StaticDamageTarget::You)
            } else if eq_ci(s_trim, "you and permanents you control") {
                Some(StaticDamageTarget::YouAndControlled)
            } else {
                parse_affected(s_trim, ctx).map(|aff| match aff {
                    Affected::SelfObj => StaticDamageTarget::SelfObj,
                    Affected::AttachedTo => StaticDamageTarget::AttachedTo,
                    Affected::Filter { filter } => StaticDamageTarget::Filter { filter },
                })
            };
            if let Some(target) = target_opt {
                return Some(vec![make_static(
                    StaticBody::StaticDamagePrevention {
                        amount: StaticDamageAmount::All,
                        combat_only,
                        noncombat_only,
                        target,
                        also_prevent_dealt_by: false,
                        dealt_by_only: None,
                        dealt_by_target_filter: None,
                        source_filter: None,
                        on_prevented: None,
                        phantom_counter_removal: None,
                        prevent_opponent_only: false,
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
    }

    ctx.interner.rollback(cp);
    None
}

// ============================================================================
// Section 7: Multipliers, Rule Modifiers, Zone Play, Cost Modifiers, CDAs,
//            Global/Attached Type & Control, Combat, Restrictions & Grants
// ============================================================================

fn parse_multiplier_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some(after_if) = strip_prefix_ci(
        t,
        "if one or more tokens would be created under your control, those tokens plus ",
    ) && let Some(extra_s) = strip_suffix_ci(after_if.trim(), " are created instead")
        .or_else(|| strip_suffix_ci(after_if.trim(), " is created instead"))
        && let Some(extra_tok) =
            crate::parser::effect::parse_static_extra_token(extra_s.trim(), ctx)
    {
        let matching = starts_with_ci(extra_s.trim(), "that many ");
        return Some(make_static(
            StaticBody::Multiplier {
                what: MultiplierTarget::Tokens,
                factor: 1.0,
                delta: 0,
                cause: None,
                cause_or: Vec::new(),
                filter: Some(Box::new(ObjectFilter {
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                })),
                counter: None,
                round: None,
                applies_to_players: false,
                who_places: None,
                who_creates: None,
                extra_token: Some(Box::new(extra_tok)),
                extra_token_matching_count: matching,
                trigger_source_attached_to: false,
            },
            condition,
        ));
    }

    if contains_ci(t, "would create one or more tokens under your control")
        && contains_ci(t, "instead")
    {
        let factor = parse_multiplier_factor_clause(t).unwrap_or(2) as f32;
        return Some(make_static(
            StaticBody::Multiplier {
                what: MultiplierTarget::Tokens,
                factor,
                delta: 0,
                cause: None,
                cause_or: Vec::new(),
                filter: Some(Box::new(ObjectFilter {
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                })),
                counter: None,
                round: None,
                applies_to_players: false,
                who_places: None,
                who_creates: Some(RelativeControl::You),
                extra_token: None,
                extra_token_matching_count: false,
                trigger_source_attached_to: false,
            },
            condition,
        ));
    }

    // Passive & filtered token multipliers:
    // "If one or more [X ]tokens would be created[ under your control], twice/three times that many of those tokens are created instead"
    // "If an effect would create one or more [X ]tokens[ under your control], it creates twice/three times that many of those tokens instead"
    // "If you would create one or more [X ]tokens, instead create twice/three times that many of those tokens"
    if (contains_ci(t, "tokens would be created") || contains_ci(t, "would create one or more"))
        && contains_ci(t, "instead")
        && (contains_ci(t, "twice that many") || contains_ci(t, "three times that many"))
    {
        let factor = if contains_ci(t, "three times that many") {
            3.0f32
        } else {
            2.0f32
        };
        let under_you =
            contains_ci(t, "under your control") || starts_with_ci(t, "if you would create ");
        let raw_tok_filter: Option<&str> =
            if let Some((_, after)) = split_once_ci(t, "one or more ") {
                if starts_with_ci(after.trim_start(), "tokens") {
                    None
                } else if let Some((mid, _)) = split_once_ci(after, " tokens") {
                    let m = mid.trim();
                    if m.is_empty() { None } else { Some(m) }
                } else {
                    None
                }
            } else {
                None
            };
        let mut ok = true;
        let mut filter = ObjectFilter::default();
        if let Some(rf) = raw_tok_filter {
            let norm_rf = if contains_ci(rf, " and ") {
                Cow::Owned(rf.replace(" and ", " or "))
            } else {
                Cow::Borrowed(rf)
            };
            if let Some(mut f) = parse_object_phrase(
                &norm_rf,
                ctx,
                FilterOpts {
                    allow_plural_and_disjunction: true,
                    ..FilterOpts::default()
                },
            ) {
                f.zones = ZoneMask::empty();
                filter = f;
            } else {
                ctx.interner.rollback(cp);
                ok = false;
            }
        }
        if ok {
            if under_you {
                filter.controller = Some(RelativeControl::You);
            }
            return Some(make_static(
                StaticBody::Multiplier {
                    what: MultiplierTarget::Tokens,
                    factor,
                    delta: 0,
                    cause: None,
                    cause_or: Vec::new(),
                    filter: Some(Box::new(filter)),
                    counter: None,
                    round: None,
                    applies_to_players: false,
                    who_places: None,
                    who_creates: if under_you {
                        None
                    } else {
                        Some(RelativeControl::All)
                    },
                    extra_token: None,
                    extra_token_matching_count: false,
                    trigger_source_attached_to: false,
                },
                condition,
            ));
        }
    }

    // Active-voice counter multiplier:
    // "If an effect would put one or more [X ]counters on a <target>, it puts twice/three times that many ... instead"
    if let Some(after_put) = strip_prefix_ci(t, "if an effect would put one or more ")
        && let Some((head, tail)) = split_once_ci(after_put, ",")
        && contains_ci(tail, "instead")
        && let Some((ctr_part, target_raw)) = split_once_ci(head, "counters on ")
    {
        let factor_delta = if contains_ci(tail, "three times that many") {
            Some((3.0f32, 0i16))
        } else if contains_ci(tail, "twice that many") || contains_ci(tail, "twice as many") {
            Some((2.0f32, 0i16))
        } else if let Some((_, plus_tail)) = split_once_ci(tail, "that many plus ")
            && let Some(num_tok) = plus_tail.split_whitespace().next()
            && let Some(n) = parse_num_word_or_int(num_tok)
        {
            Some((1.0f32, n as i16))
        } else {
            None
        };
        if let Some((factor, delta)) = factor_delta {
            let ctr_trim = ctr_part.trim();
            let counter = if ctr_trim.is_empty() {
                Some(None)
            } else {
                lookup_static_counter(ctr_trim, ctx).map(Some)
            };
            let target_clean = strip_leading_article(target_raw.trim());
            let filter_opt = if eq_ci(target_clean, "permanent you control")
                || eq_ci(target_clean, "permanent your team controls")
            {
                Some(ObjectFilter {
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                })
            } else if eq_ci(target_clean, "permanent") {
                Some(ObjectFilter::default())
            } else {
                parse_object_with_qualifiers(target_clean, ctx, FilterOpts::default()).map(
                    |mut f| {
                        f.zones = ZoneMask::empty();
                        f
                    },
                )
            };
            if let (Some(counter), Some(filter)) = (counter, filter_opt) {
                return Some(make_static(
                    StaticBody::Multiplier {
                        what: MultiplierTarget::Counters,
                        factor,
                        delta,
                        cause: None,
                        cause_or: Vec::new(),
                        filter: Some(Box::new(filter)),
                        counter,
                        round: None,
                        applies_to_players: false,
                        who_places: None,
                        who_creates: None,
                        extra_token: None,
                        extra_token_matching_count: false,
                        trigger_source_attached_to: false,
                    },
                    condition,
                ));
            }
            ctx.interner.rollback(cp);
        }
    }

    if contains_ci(t, "counters would be put on") && contains_ci(t, "instead") {
        let (factor, delta) = if let Some(s) = parse_multiplier_factor_clause(t) {
            (s as f32, 0i16)
        } else if contains_ci(t, "twice that many") || contains_ci(t, "twice as many") {
            (2.0f32, 0i16)
        } else if let Some((_, plus_tail)) = split_once_ci(t, "that many plus ")
            && let Some(num_tok) = plus_tail.split_whitespace().next()
            && let Some(n) = parse_num_word_or_int(num_tok)
        {
            (1.0f32, n as i16)
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        let counter = if contains_ci(t, "+1/+1") {
            lookup_static_counter("+1/+1", ctx)
        } else {
            None
        };
        let filt_opt = split_once_ci(t, "put on ")
            .and_then(|(_, after_on)| after_on.split(',').next())
            .and_then(|obj_s| {
                let clean = strip_leading_article(obj_s.trim());
                let mut f = parse_object_with_qualifiers(clean, ctx, FilterOpts::default())?;
                f.zones = ZoneMask::empty();
                Some(Box::new(f))
            });
        return Some(make_static(
            StaticBody::Multiplier {
                what: MultiplierTarget::Counters,
                factor,
                delta,
                cause: None,
                cause_or: Vec::new(),
                filter: filt_opt,
                counter,
                round: None,
                applies_to_players: false,
                who_places: None,
                who_creates: None,
                extra_token: None,
                extra_token_matching_count: false,
                trigger_source_attached_to: false,
            },
            condition,
        ));
    }

    if contains_ci(
        t,
        "causes a triggered ability of a permanent you control to trigger, that ability triggers an additional time",
    ) || contains_ci(t, "trigger an additional time")
    {
        let cause = if contains_ci(t, "drawing a card") {
            let mut pat = make_simple_event_pattern(EventKind::DrawCard);
            pat.filter.player = Some(PlayerFilter {
                relation: PlayerRelation::Any,
            });
            Some(pat)
        } else if contains_ci(t, "casting") {
            let or_copy = contains_ci(t, "casting or copying");
            let player = contains_ci(t, "you casting").then_some(PlayerFilter {
                relation: PlayerRelation::You,
            });
            let spell_after = split_once_ci(t, "casting or copying ")
                .or_else(|| split_once_ci(t, "casting "))
                .and_then(|(_, r)| split_once_ci(r, " causes "))
                .map(|(s, _)| s.trim());
            let spell_subj = spell_after.and_then(|raw_s| {
                let no_spell = strip_suffix_ci(raw_s, " spells")
                    .or_else(|| strip_suffix_ci(raw_s, " spell"))
                    .unwrap_or(raw_s)
                    .trim();
                let no_art = strip_leading_article(no_spell);
                if eq_ci(no_art, "instant or sorcery") {
                    Some(TriggerSubject::Filter {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::INSTANT | CardTypeMask::SORCERY,
                            ..Default::default()
                        }),
                    })
                } else {
                    parse_object_with_qualifiers(no_art, ctx, FilterOpts::default()).map(|mut f| {
                        f.zones = ZoneMask::empty();
                        TriggerSubject::Filter {
                            filter: Box::new(f),
                        }
                    })
                }
            });
            Some(EventPattern {
                event: EventKind::Cast,
                filter: EventFilter {
                    object: spell_subj,
                    player,
                    detail: Some(EventDetail::Cast {
                        or_copy,
                        targets_self: false,
                        targets_only_self: false,
                        during_opponent_turn: false,
                        cast_from_not_hand: false,
                        cast_from_zone: None,
                        was_kicked: false,
                        spell_ordinal: None,
                        spell_has_keyword: None,
                        enchanted_player_casts: false,
                    }),
                    ..Default::default()
                },
            })
        } else if contains_ci(t, "entering") {
            let enter_subj = split_once_ci(t, " entering")
                .map(|(before, _)| {
                    split_once_ci(before, "if ")
                        .map(|(_, after_if)| after_if)
                        .unwrap_or(before)
                })
                .map(|s| strip_leading_article(s.trim()))
                .and_then(|clean_s| {
                    if eq_ci(clean_s, "permanent") || eq_ci(clean_s, "permanents") {
                        None
                    } else if contains_ci(clean_s, " or ") || contains_ci(clean_s, " and ") {
                        let sep = if contains_ci(clean_s, " or ") {
                            " or "
                        } else {
                            " and "
                        };
                        let mut combined_types = CardTypeMask::empty();
                        let mut ok = true;
                        for part in clean_s.split(sep) {
                            let pt = strip_leading_article(part.trim());
                            if let Some(parsed) = parse_type_description(pt, ctx)
                                && !parsed.types.is_empty()
                                && parsed.subtypes.is_empty()
                            {
                                combined_types |= parsed.types;
                            } else {
                                ok = false;
                                break;
                            }
                        }
                        if ok && !combined_types.is_empty() {
                            Some(TriggerSubject::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: combined_types,
                                    ..Default::default()
                                }),
                            })
                        } else {
                            None
                        }
                    } else {
                        parse_object_with_qualifiers(clean_s, ctx, FilterOpts::default()).map(
                            |mut f| {
                                f.zones = ZoneMask::empty();
                                TriggerSubject::Filter {
                                    filter: Box::new(f),
                                }
                            },
                        )
                    }
                });
            Some(make_zone_change_pattern(
                enter_subj,
                ZoneMask::empty(),
                Some(Zone::Battlefield),
                false,
            ))
        } else if contains_ci(t, "dying") {
            Some(make_zone_change_pattern(
                None,
                ZoneMask::BATTLEFIELD,
                Some(Zone::Graveyard),
                false,
            ))
        } else {
            None
        };
        let trig_filter = split_once_ci(t, "triggered ability of ")
            .and_then(|(_, after)| split_once_ci(after, " to trigger"))
            .and_then(|(f_raw, _)| {
                parse_object_with_qualifiers(f_raw.trim(), ctx, FilterOpts::default())
            })
            .or_else(|| {
                contains_ci(t, "permanent you control").then(|| ObjectFilter {
                    controller: Some(RelativeControl::You),
                    ..Default::default()
                })
            });
        return Some(make_static(
            StaticBody::Multiplier {
                what: MultiplierTarget::Triggers,
                factor: 2.0,
                delta: 0,
                cause,
                cause_or: Vec::new(),
                filter: trig_filter.map(Box::new),
                counter: None,
                round: None,
                applies_to_players: false,
                who_places: None,
                who_creates: None,
                extra_token: None,
                extra_token_matching_count: false,
                trigger_source_attached_to: false,
            },
            condition,
        ));
    }

    if contains_ci(t, "entering don't cause abilities")
        || contains_ci(t, "entering the battlefield don't cause abilities")
        || contains_ci(t, "doesn't cause abilities to trigger")
    {
        let enter_subj = split_once_ci(t, " entering")
            .map(|(before, _)| strip_leading_article(before.trim()))
            .and_then(|clean_s| {
                if eq_ci(clean_s, "permanent") || eq_ci(clean_s, "permanents") {
                    None
                } else if contains_ci(clean_s, " or ") || contains_ci(clean_s, " and ") {
                    let sep = if contains_ci(clean_s, " or ") {
                        " or "
                    } else {
                        " and "
                    };
                    let mut combined_types = CardTypeMask::empty();
                    let mut ok = true;
                    for part in clean_s.split(sep) {
                        let pt = strip_leading_article(part.trim());
                        if let Some(parsed) = parse_type_description(pt, ctx)
                            && !parsed.types.is_empty()
                            && parsed.subtypes.is_empty()
                        {
                            combined_types |= parsed.types;
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok && !combined_types.is_empty() {
                        Some(TriggerSubject::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: combined_types,
                                ..Default::default()
                            }),
                        })
                    } else {
                        None
                    }
                } else {
                    parse_object_with_qualifiers(clean_s, ctx, FilterOpts::default()).map(
                        |mut f| {
                            f.zones = ZoneMask::empty();
                            TriggerSubject::Filter {
                                filter: Box::new(f),
                            }
                        },
                    )
                }
            });
        let trig_filter = split_once_ci(t, "abilities of ")
            .and_then(|(_, after)| split_once_ci(after, " to trigger"))
            .and_then(|(f_raw, _)| {
                let mut f = parse_object_with_qualifiers(f_raw.trim(), ctx, FilterOpts::default())?;
                f.zones = ZoneMask::empty();
                Some(Box::new(f))
            });
        return Some(make_static(
            StaticBody::Multiplier {
                what: MultiplierTarget::Triggers,
                factor: 0.0,
                delta: 0,
                cause: Some(make_zone_change_pattern(
                    enter_subj,
                    ZoneMask::empty(),
                    Some(Zone::Battlefield),
                    false,
                )),
                cause_or: Vec::new(),
                filter: trig_filter,
                counter: None,
                round: None,
                applies_to_players: false,
                who_places: None,
                who_creates: None,
                extra_token: None,
                extra_token_matching_count: false,
                trigger_source_attached_to: false,
            },
            condition,
        ));
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_rule_modifier_multi_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if eq_ci(
        t,
        "you can't lose the game and your opponents can't win the game",
    ) {
        return Some(vec![
            make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::CantLose,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition.clone(),
            ),
            make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::OpponentsCantWin,
                    who: PlayerFilter {
                        relation: PlayerRelation::EachOpponent,
                    },
                },
                condition,
            ),
        ]);
    }
    if eq_ci(t, "you can't lose the game") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantLose,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "your opponents can't win the game") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::OpponentsCantWin,
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "skip your draw step") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SkipDrawStep,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        )]);
    }
    if eq_ci(
        t,
        "your opponents can't cast spells from anywhere other than their hands",
    ) {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CastOnlyFromHand,
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "players can't cast spells from graveyards or libraries")
        || eq_ci(t, "players can't cast spells from graveyards and libraries")
    {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantCastMatchingSpells {
                    filter: None,
                    from_zones: ZoneMask::GRAVEYARD | ZoneMask::LIBRARY,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        )]);
    }
    if eq_ci(
        t,
        "you control your opponents while they're searching their libraries",
    ) {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SearchControl {
                    exiles_found_cards: false,
                    grant_play_from_exile: false,
                    spend_mana_as_any_color: false,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "damage can't be prevented") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::DamageCantBePrevented,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "all damage is dealt as though its source had wither") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::AllDamageHasWither,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        )]);
    }
    if eq_ci(
        t,
        "all cards that aren't on the battlefield, spells, and permanents are colorless",
    ) {
        return Some(vec![
            make_static(
                wrap_layer_mod(
                    Layer::Layer5,
                    Affected::Filter {
                        filter: Box::new(ObjectFilter::default()),
                    },
                    LayerMod::SetColors {
                        colors: ColorMask::empty(),
                    },
                ),
                condition.clone(),
            ),
            make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::AllOffBattlefieldAndSpellsColorless,
                    who: PlayerFilter {
                        relation: PlayerRelation::EachPlayer,
                    },
                },
                condition,
            ),
        ]);
    }
    if eq_ci(
        t,
        "players may spend mana as though it were mana of any color",
    ) {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SpendManaAsAnyColor {
                    abilities_only: false,
                    source_filter: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        )]);
    }
    if let Some(after_spend) =
        strip_prefix_ci(t, "you may spend mana as though it were mana of any color")
    {
        let tail = after_spend.trim();
        if tail.is_empty() {
            return Some(vec![make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::SpendManaAsAnyColor {
                        abilities_only: false,
                        source_filter: None,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition,
            )]);
        }
        if let Some(act_s) = strip_prefix_ci(tail, "to activate abilities of ")
            && let Some(mut f) =
                parse_object_with_qualifiers(act_s.trim(), ctx, FilterOpts::default())
        {
            if f.zones.is_empty() {
                f.zones = ZoneMask::BATTLEFIELD;
            }
            return Some(vec![make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::SpendManaAsAnyColor {
                        abilities_only: true,
                        source_filter: Some(Box::new(f)),
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition,
            )]);
        }
        if let Some(cast_s) = strip_prefix_ci(tail, "to cast ") {
            let clean = strip_suffix_ci(cast_s.trim(), " spells")
                .map(|s| format!("{s} cards"))
                .or_else(|| strip_suffix_ci(cast_s.trim(), " spell").map(|s| format!("{s} card")))
                .unwrap_or_else(|| cast_s.trim().to_string());
            if let Some(mut f) = parse_object_with_qualifiers(&clean, ctx, FilterOpts::default()) {
                f.zones = ZoneMask::empty();
                return Some(vec![make_static(
                    StaticBody::RuleModifier {
                        rule: RuleMod::SpendManaAsAnyColor {
                            abilities_only: false,
                            source_filter: Some(Box::new(f)),
                        },
                        who: PlayerFilter {
                            relation: PlayerRelation::You,
                        },
                    },
                    condition,
                )]);
            }
        }
        ctx.interner.rollback(cp);
    }
    if let Some(rest) = strip_prefix_ci(t, "your maximum hand size is ")
        && let Some(n) = parse_num_word_or_int(rest.trim())
    {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SetMaxHandSize { size: n as u16 },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        )]);
    }
    if eq_ci(t, "you have hexproof") {
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::PlayerHexproof,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        )]);
    }
    if let Some(rest) = strip_prefix_ci(t, "you, ")
        && let Some((subj_list, kw_tail)) = split_once_ci(rest, " have ")
        && let Some(kws) = parse_keyword_id_list(kw_tail, ctx)
    {
        let mut out = vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::PlayerHexproof,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition.clone(),
        )];
        let mut all_ok = contains_ci(kw_tail, "hexproof");
        if all_ok {
            for part in subj_list.split(',') {
                let p = strip_prefix_ci(part.trim(), "and ")
                    .unwrap_or(part.trim())
                    .trim();
                if p.is_empty() {
                    continue;
                }
                if let Some(mut f) = parse_object_with_qualifiers(p, ctx, FilterOpts::default()) {
                    f.zones = ZoneMask::empty();
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer6,
                            Affected::Filter {
                                filter: Box::new(f),
                            },
                            LayerMod::AddKeywords {
                                keywords: kws.clone(),
                                protection_attachment_exemption: None,
                            },
                        ),
                        condition.clone(),
                    ));
                } else {
                    all_ok = false;
                    break;
                }
            }
        }
        if all_ok && out.len() > 1 {
            return Some(out);
        }
        ctx.interner.rollback(cp);
    }

    None
}

fn parse_rule_modifier_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if eq_ci(t, "you have no maximum hand size") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::NoMaxHandSize,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "players have no maximum hand size") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::NoMaxHandSize,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "your maximum hand size is reduced by ")
        && let Some(n) = parse_num_word_or_int(rest)
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::MaxHandSizeDelta { delta: -n },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "your maximum hand size is increased by ")
        && let Some(n) = parse_num_word_or_int(rest)
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::MaxHandSizeDelta { delta: n },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }

    if eq_ci(t, "the \"legend rule\" doesn't apply")
        || eq_ci(
            t,
            "the \"legend rule\" doesn't apply to permanents you control",
        )
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::LegendRuleExempt {
                    filter: if contains_ci(t, "you control") {
                        Some(Box::new(ObjectFilter {
                            controller: Some(RelativeControl::You),
                            ..Default::default()
                        }))
                    } else {
                        None
                    },
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }

    if eq_ci(t, "you may play an additional land on each of your turns") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::ExtraLandPlays { count: 1 },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }
    if eq_ci(
        t,
        "each player may play an additional land on each of their turns",
    ) {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::ExtraLandPlays { count: 1 },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "you may play ")
        && let Some(num_s) = strip_suffix_ci(rest, " additional lands on each of your turns")
        && let Some(n) = parse_num_word_or_int(num_s)
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::ExtraLandPlays { count: n as u16 },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }

    if let Some(after_untap) = strip_prefix_ci(t, "untap all ")
        && let Some((what_s, tail_s)) =
            split_once_ci(after_untap, " during each other player's untap step")
        && tail_s.trim().is_empty()
    {
        let norm_what = what_s.trim().replace(" and/or ", " or ");
        if let Some(mut f) = parse_object_with_qualifiers(
            norm_what.trim(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::empty(),
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
        ) {
            f.zones = ZoneMask::empty();
            return Some(make_static(
                StaticBody::UntapDuringOtherUntapSteps {
                    affected: Affected::Filter {
                        filter: Box::new(f),
                    },
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_lose) =
        strip_prefix_ci(t, "if you would lose unspent mana, that mana becomes ")
        && let Some((col_s, tail_s)) = split_once_ci(after_lose, " instead")
        && tail_s.trim().is_empty()
        && let Some(col_mask) = parse_color_word(col_s.trim())
    {
        let convert_to = if col_mask == ColorMask::W {
            Some(mtg_ir::ManaColor::W)
        } else if col_mask == ColorMask::U {
            Some(mtg_ir::ManaColor::U)
        } else if col_mask == ColorMask::B {
            Some(mtg_ir::ManaColor::B)
        } else if col_mask == ColorMask::R {
            Some(mtg_ir::ManaColor::R)
        } else if col_mask == ColorMask::G {
            Some(mtg_ir::ManaColor::G)
        } else {
            None
        };
        if convert_to.is_some() {
            return Some(make_static(
                StaticBody::RetainUnspentMana {
                    colors: ColorMask::empty(),
                    all_players: false,
                    convert_to,
                },
                condition,
            ));
        }
    }

    if contains_ci(t, "don't lose unspent ") && contains_ci(t, "as steps and phases end") {
        let mut color: Option<ColorMask> = None;
        for cword in ["white", "blue", "black", "red", "green"] {
            if contains_word_ci(t, cword) {
                color = parse_color_word(cword);
                break;
            }
        }
        return Some(make_static(
            StaticBody::RetainUnspentMana {
                colors: color.unwrap_or(ColorMask::ALL_COLORS | ColorMask::C),
                all_players: false,
                convert_to: None,
            },
            condition,
        ));
    }

    if let Some(cap_body) = strip_prefix_ci(t, "each player can't cast more than one")
        .or_else(|| strip_prefix_ci(t, "players can't cast more than one"))
        && let Some(mid) = strip_suffix_ci(cap_body.trim(), "spell each turn")
    {
        let mid_trim = mid.trim();
        let cap_filter = if mid_trim.is_empty() {
            None
        } else {
            let mut sub_ctx = ctx.with_normalized_text(mid_trim);
            parse_object_phrase(
                mid_trim,
                &mut sub_ctx,
                FilterOpts {
                    default_zone: ZoneMask::empty(),
                    allow_plural_and_disjunction: true,
                    allow_spell_noun: false,
                    strip_trailing_card: false,
                    ..Default::default()
                },
            )
            .map(Box::new)
        };
        if mid_trim.is_empty() || cap_filter.is_some() {
            let rule = match cap_filter {
                None => RuleMod::OneSpellPerTurn,
                Some(f) => RuleMod::FilteredSpellCapPerTurn {
                    filter: Some(f),
                    max: 1,
                },
            };
            return Some(make_static(
                StaticBody::RuleModifier {
                    rule,
                    who: PlayerFilter {
                        relation: PlayerRelation::EachPlayer,
                    },
                },
                condition,
            ));
        }
    }

    if eq_ci(
        t,
        "permanent cards in graveyards can't enter the battlefield",
    ) {
        return Some(make_static(
            StaticBody::CantEnterFromZones {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::PERMANENT,
                    ..Default::default()
                }),
                zones: ZoneMask::GRAVEYARD,
                who: Some(RelativeControl::All),
            },
            condition,
        ));
    }

    if eq_ci(
        t,
        "players can't cast noncreature spells from graveyards or exile",
    ) {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantCastMatchingSpells {
                    filter: Some(Box::new(ObjectFilter {
                        types_none: CardTypeMask::CREATURE,
                        extra: vec![ObjectPredicate::CastFromZones {
                            zones: ZoneMask::GRAVEYARD | ZoneMask::EXILE,
                        }],
                        ..Default::default()
                    })),
                    from_zones: ZoneMask::empty(),
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }

    if eq_ci(t, "all creatures able to block enchanted creature do so") {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: Affected::AttachedTo,
                rule: CombatRuleSpec::AllAbleBlockers,
            },
            condition,
        ));
    }

    if contains_ci(t, "a deck can have any number of cards named") {
        return Some(make_static(
            StaticBody::DeckCopyLimit { limit: None },
            condition,
        ));
    }
    if let Some(rest) = strip_prefix_ci(t, "a deck can have up to ")
        && contains_ci(rest, "cards named")
    {
        let num_tok = rest.split_whitespace().next().unwrap_or("");
        if let Some(n) = parse_num_word_or_int(num_tok) {
            return Some(make_static(
                StaticBody::DeckCopyLimit {
                    limit: Some(n as u16),
                },
                condition,
            ));
        }
    }

    if contains_ci(
        t,
        "in your opening hand, you may begin the game with it on the battlefield",
    ) {
        return Some(make_static(
            StaticBody::PregameBeginOnBattlefield,
            condition,
        ));
    }

    if eq_ci(t, "your opponents play with their hands revealed")
        || eq_ci(t, "each opponent plays with their hand revealed")
    {
        return Some(make_static(
            StaticBody::HandVisibility {
                players: RelativeControl::Opponent,
            },
            condition,
        ));
    }
    if eq_ci(t, "players play with their hands revealed")
        || eq_ci(t, "each player plays with their hand revealed")
    {
        return Some(make_static(
            StaticBody::HandVisibility {
                players: RelativeControl::All,
            },
            condition,
        ));
    }

    if contains_ci(
        t,
        "creatures your opponents control can be the targets of spells and abilities you control as though they didn't have hexproof",
    ) {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::IgnoreHexproof,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }
    if contains_ci(t, "ward abilities of") && contains_ci(t, "don't trigger") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SuppressWardTriggers,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }

    if contains_ci(
        t,
        "creature cards in graveyards and libraries can't enter the battlefield",
    ) || contains_ci(
        t,
        "creature cards in graveyards or libraries can't enter the battlefield",
    ) {
        return Some(make_static(
            StaticBody::CantEnterFromZones {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                zones: ZoneMask::GRAVEYARD | ZoneMask::LIBRARY,
                who: Some(RelativeControl::All),
            },
            condition,
        ));
    }

    if eq_ci(t, "you have shroud") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::PlayerShroud,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            condition,
        ));
    }

    if eq_ci(t, "draft ~ face up")
        || contains_ci(t, "as you draft")
        || contains_ci(t, "reveal ~ as you draft it")
        || starts_with_ci(t, "during the draft,")
        || starts_with_ci(t, "immediately after the draft,")
        || starts_with_ci(t, "instead of drafting a card from a booster pack,")
        || starts_with_ci(t, "as long as ~ is face up during the draft,")
    {
        return Some(make_static(StaticBody::DraftNoOp, condition));
    }

    // "Players / Each player / Your opponents can't untap more than <N> <filter> during their untap steps"
    if let Some((who, after_cant)) = strip_prefix_ci(t, "players can't untap more than ")
        .map(|r| (PlayerRelation::EachPlayer, r))
        .or_else(|| {
            strip_prefix_ci(t, "each player can't untap more than ")
                .map(|r| (PlayerRelation::EachPlayer, r))
        })
        .or_else(|| {
            strip_prefix_ci(t, "your opponents can't untap more than ")
                .map(|r| (PlayerRelation::EachOpponent, r))
        })
        .or_else(|| {
            strip_prefix_ci(t, "opponents can't untap more than ")
                .map(|r| (PlayerRelation::EachOpponent, r))
        })
        && let Some((num_and_obj, _)) = split_once_ci(after_cant, " during their untap steps")
            .or_else(|| split_once_ci(after_cant, " during their untap step"))
            .or_else(|| split_once_ci(after_cant, " during each of their untap steps"))
        && let Some((num_w, obj_raw)) = num_and_obj.trim().split_once(' ')
        && let Some(n) = parse_num_word_or_int(num_w.trim())
        && n >= 0
    {
        let obj_s = obj_raw.trim();
        let filter_opt = if eq_ci(obj_s, "permanent") || eq_ci(obj_s, "permanents") {
            Some(None)
        } else if let Some(mut f) = parse_object_with_qualifiers(obj_s, ctx, FilterOpts::default())
        {
            f.zones = ZoneMask::empty();
            Some(Some(Box::new(f)))
        } else {
            ctx.interner.rollback(cp);
            None
        };
        if let Some(filter) = filter_opt {
            return Some(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::UntapStepCap {
                        filter,
                        max: n as u16,
                    },
                    who: PlayerFilter { relation: who },
                },
                condition,
            ));
        }
    }

    if let Some(subj) = strip_suffix_ci(t, " can be your commander")
        && is_self_phrase(subj.trim())
    {
        return Some(make_static(StaticBody::CommanderDesignation, condition));
    }

    if let Some(after_pay) = strip_prefix_ci(
        t,
        "creatures can't attack you or planeswalkers you control unless their controller pays ",
    ) && let Some((cost_raw, tail)) = split_once_ci(after_pay, " for each ")
        && contains_ci(tail, "of those creatures")
    {
        let defs = parse_cost_list(cost_raw.trim(), 0, ctx);
        if let [CostDef::Mana { cost, .. }] = defs.defs.as_slice() {
            return Some(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::AttackTax {
                        generic: Some(cost.generic as u16),
                        cost: None,
                        mana_symbol: None,
                        dynamic_amount: None,
                        affected: None,
                        defending: Some("youOrPlaneswalkersYouControl".to_string()),
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::EachPlayer,
                    },
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_pay) = strip_prefix_ci(
        t,
        "creatures can't attack you unless their controller pays ",
    ) && let Some((cost_raw, _tail)) = split_once_ci(after_pay, " for each creature")
    {
        let defs = parse_cost_list(cost_raw.trim(), 0, ctx);
        if let [CostDef::Mana { cost, .. }] = defs.defs.as_slice() {
            return Some(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::AttackTax {
                        generic: None,
                        cost: Some(cost.clone()),
                        mana_symbol: None,
                        dynamic_amount: None,
                        affected: None,
                        defending: None,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_have) = strip_prefix_ci(t, "you may have ")
        && let Some((subj_s, tail)) = split_once_ci(after_have, " assign ")
        && (eq_ci(
            tail.trim(),
            "its combat damage as though it weren't blocked",
        ) || eq_ci(
            tail.trim(),
            "their combat damage as though they weren't blocked",
        ))
        && let Some(affected) = parse_affected(subj_s.trim(), ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected,
                rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
            },
            condition,
        ));
    }
    if eq_ci(
        t,
        "enchanted creature's controller may have it assign its combat damage as though it weren't blocked",
    ) || eq_ci(
        t,
        "equipped creature's controller may have it assign its combat damage as though it weren't blocked",
    ) {
        return Some(make_static(
            StaticBody::CombatRule {
                affected: Affected::AttachedTo,
                rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
            },
            condition,
        ));
    }
    if let Some(after_each) = strip_prefix_ci(t, "for each ")
        && let Some((subj_s, tail)) = split_once_ci(after_each, ", you may have that ")
        && (eq_ci(
            tail.trim(),
            "creature assign its combat damage as though it weren't blocked",
        ) || eq_ci(
            tail.trim(),
            "permanent assign its combat damage as though it weren't blocked",
        ))
        && let Some(affected) = parse_affected(subj_s.trim(), ctx)
    {
        return Some(make_static(
            StaticBody::CombatRule {
                affected,
                rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
            },
            condition,
        ));
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_zone_play_and_visibility_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some(after_once) = strip_prefix_ci(t, "once during each of your turns, you may cast ")
        && let Some((what_s, after_gy)) = split_once_ci(
            after_once,
            " from among cards in your graveyard that were put there from anywhere other than the battlefield this turn",
        )
        && after_gy.trim().is_empty()
    {
        let clean_what = strip_suffix_ci(what_s.trim(), " spells")
            .or_else(|| strip_suffix_ci(what_s.trim(), " spell"))
            .unwrap_or(what_s.trim());
        let clean_what = strip_leading_article(clean_what);
        let mut f = parse_object_with_qualifiers(clean_what, ctx, FilterOpts::default())
            .unwrap_or_default();
        f.zones = ZoneMask::empty();
        return Some(vec![make_static(
            StaticBody::ZonePlayPermission {
                zone: Zone::Graveyard,
                allow_lands: false,
                allow_spells: true,
                spell_filter: Some(Box::new(f)),
                self_only: false,
                during_your_turn_only: true,
                slot_mode: Some(mtg_ir::SlotMode::OncePerTurn),
                exile_if_put_into_graveyard: false,
                exiled_with_self: false,
                require_counter: None,
                card_owner: None,
                spend_mana_as_any_color: false,
                enters_tapped: false,
                grant_haste_until_eot: false,
                put_into_graveyard_from_non_battlefield_this_turn: true,
                alt_cost_defs: Vec::new(),
                grant_flash: false,
            },
            condition,
        )]);
    }

    if let Some(after_once) = strip_prefix_ci(t, "once during each of your turns, you may cast ")
        && let Some((what_s, after_gy)) =
            split_once_ci(after_once, " from your graveyard by exiling ")
        && let Some((ex_cnt_s, _tail)) = split_once_ci(after_gy, " other cards from your graveyard")
        && let Some(ex_n) = parse_num_word_or_int(ex_cnt_s.trim())
    {
        let clean_what = strip_suffix_ci(what_s.trim(), " spell")
            .or_else(|| strip_suffix_ci(what_s.trim(), " spells"))
            .unwrap_or(what_s.trim());
        let mut f = parse_object_with_qualifiers(clean_what, ctx, FilterOpts::default())
            .unwrap_or_default();
        if (eq_ci(clean_what, "permanent") || eq_ci(clean_what, "permanents"))
            && f.types_any.is_empty()
        {
            f.types_any = CardTypeMask::CREATURE
                | CardTypeMask::ARTIFACT
                | CardTypeMask::ENCHANTMENT
                | CardTypeMask::PLANESWALKER
                | CardTypeMask::BATTLE;
        }
        f.zones = ZoneMask::empty();
        return Some(vec![make_static(
            StaticBody::AltCastPermission {
                mana_cost: ManaCost {
                    raw: "{0}".to_string(),
                    ..Default::default()
                },
                use_printed_mana_cost: false,
                cost_defs: vec![CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(fixed_amt(ex_n)),
                    target: Some(mtg_ir::CostObjectTarget::Filter {
                        filter: Box::new(ObjectFilter {
                            flags: FilterFlags::OTHER,
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
                affected: Affected::Filter {
                    filter: Box::new(f),
                },
                allowed_zones: ZoneMask::GRAVEYARD,
                once_each_turn: true,
                during_your_turn_only: true,
                grant_flash: false,
                any_player: false,
                dynamic_mana_cost_from_mv: false,
                max_mv_by_self_counters: None,
                max_mv_by_controlled_count: None,
                enters_with_counters: Vec::new(),
                additional_only: true,
                additional_per_metric: None,
                require_zone_permission: false,
                pay_life_equal_to_mv: false,
                slot_mode: None,
            },
            condition,
        )]);
    }

    if let Some(after_once) = strip_prefix_ci(t, "once during each of your turns, you may cast ")
        && let Some((what_s, after_gy)) = split_once_ci(after_once, " from your graveyard by ")
        && let Some(cost_raw) =
            strip_suffix_ci(after_gy.trim(), " in addition to paying its other costs").or_else(
                || strip_suffix_ci(after_gy.trim(), " in addition to paying their other costs"),
            )
    {
        let cost_norm = if let Some(r) = strip_prefix_ci(cost_raw.trim(), "sacrificing ") {
            format!("Sacrifice {}", r.trim())
        } else if let Some(r) = strip_prefix_ci(cost_raw.trim(), "discarding ") {
            format!("Discard {}", r.trim())
        } else if let Some(r) = strip_prefix_ci(cost_raw.trim(), "paying ") {
            format!("Pay {}", r.trim())
        } else {
            cost_raw.trim().to_string()
        };
        let cparse = parse_cost_list(&cost_norm, 0, ctx);
        if !cparse.defs.is_empty() && cparse.unparsed.is_empty() {
            let clean_what = strip_suffix_ci(what_s.trim(), " spell")
                .or_else(|| strip_suffix_ci(what_s.trim(), " spells"))
                .unwrap_or(what_s.trim());
            let clean_what = strip_leading_article(clean_what);
            let mut f = parse_object_with_qualifiers(clean_what, ctx, FilterOpts::default())
                .unwrap_or_default();
            if (eq_ci(clean_what, "permanent") || eq_ci(clean_what, "permanents"))
                && f.types_any.is_empty()
            {
                f.types_any = CardTypeMask::CREATURE
                    | CardTypeMask::ARTIFACT
                    | CardTypeMask::ENCHANTMENT
                    | CardTypeMask::PLANESWALKER
                    | CardTypeMask::BATTLE;
            }
            f.zones = ZoneMask::empty();
            return Some(vec![make_static(
                StaticBody::AltCastPermission {
                    mana_cost: ManaCost {
                        raw: "{0}".to_string(),
                        ..Default::default()
                    },
                    use_printed_mana_cost: false,
                    cost_defs: cparse.defs,
                    affected: Affected::Filter {
                        filter: Box::new(f),
                    },
                    allowed_zones: ZoneMask::GRAVEYARD,
                    once_each_turn: true,
                    during_your_turn_only: true,
                    grant_flash: false,
                    any_player: false,
                    dynamic_mana_cost_from_mv: false,
                    max_mv_by_self_counters: None,
                    max_mv_by_controlled_count: None,
                    enters_with_counters: Vec::new(),
                    additional_only: true,
                    additional_per_metric: None,
                    require_zone_permission: false,
                    pay_life_equal_to_mv: false,
                    slot_mode: None,
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if eq_ci(
        t,
        "during each of your turns, you may play a land and cast a permanent spell of each permanent type from your graveyard",
    ) {
        return Some(vec![make_static(
            StaticBody::ZonePlayPermission {
                zone: Zone::Graveyard,
                allow_lands: true,
                allow_spells: true,
                spell_filter: Some(Box::new(ObjectFilter {
                    types_any: CardTypeMask::PERMANENT,
                    ..Default::default()
                })),
                self_only: false,
                during_your_turn_only: true,
                slot_mode: Some(mtg_ir::SlotMode::PerPermanentType),
                exile_if_put_into_graveyard: false,
                exiled_with_self: false,
                require_counter: None,
                card_owner: None,
                spend_mana_as_any_color: false,
                enters_tapped: false,
                grant_haste_until_eot: false,
                put_into_graveyard_from_non_battlefield_this_turn: false,
                alt_cost_defs: Vec::new(),
                grant_flash: false,
            },
            condition,
        )]);
    }

    if eq_ci(t, "play with the top card of your library revealed") {
        return Some(vec![make_static(
            StaticBody::TopOfLibraryVisibility {
                reveal_to_all: true,
                all_players: false,
            },
            condition,
        )]);
    }
    if eq_ci(t, "you may look at the top card of your library any time") {
        return Some(vec![make_static(
            StaticBody::TopOfLibraryVisibility {
                reveal_to_all: false,
                all_players: false,
            },
            condition,
        )]);
    }
    if let Some(rest_after_look) = strip_prefix_ci(
        t,
        "you may look at the top card of your library any time, and ",
    )
    .or_else(|| {
        strip_prefix_ci(
            t,
            "you may look at the top card of your library any time and ",
        )
    }) && let Some(mut sub) =
        parse_zone_play_and_visibility_sentence(rest_after_look.trim(), condition.clone(), ctx)
    {
        let mut out = vec![make_static(
            StaticBody::TopOfLibraryVisibility {
                reveal_to_all: false,
                all_players: false,
            },
            condition,
        )];
        out.append(&mut sub);
        return Some(out);
    }

    if eq_ci(t, "you may look at cards exiled with ~")
        || eq_ci(t, "you may look at face-down cards exiled with ~")
    {
        return Some(vec![make_static(
            StaticBody::MayLookAtExiledWithSelf,
            condition,
        )]);
    }

    let (lead_once, body_no_lead) = if let Some(r) =
        strip_prefix_ci(t, "once during each of your turns, ")
            .or_else(|| strip_prefix_ci(t, "once on each of your turns, "))
    {
        (true, r.trim())
    } else {
        (false, t)
    };

    if let Some(rest) = strip_prefix_ci(body_no_lead, "you may ") {
        if let Some((what_s, _)) = split_once_ci(rest, " from the top of your library") {
            let what_trimmed = what_s.trim();
            let allow_lands =
                contains_ci(what_trimmed, "play lands") || contains_ci(what_trimmed, "play land");
            let mut allow_spells =
                contains_ci(what_trimmed, "cast ") || contains_ci(what_trimmed, "play cards");
            let mut spell_filter: Option<Box<ObjectFilter>> = None;
            if let Some(cast_part) = split_once_ci(what_trimmed, "cast ")
                .map(|(_, r)| r.trim())
                .or((!allow_lands).then_some(what_trimmed))
            {
                if eq_ci(cast_part, "spells") || eq_ci(cast_part, "a spell") {
                    allow_spells = true;
                } else {
                    let norm_cast = cast_part
                        .replace(" and ", " or ")
                        .replace(" AND ", " or ")
                        .replace(" spells ", " cards ")
                        .replace(" spell ", " card ");
                    let query = if let Some(base) = strip_suffix_ci(norm_cast.trim(), " spells")
                        .or_else(|| strip_suffix_ci(norm_cast.trim(), " spell"))
                    {
                        format!("{} cards", base.trim())
                    } else {
                        norm_cast
                    };
                    if let Some(mut f) =
                        parse_object_with_qualifiers(&query, ctx, FilterOpts::default())
                    {
                        f.zones = ZoneMask::empty();
                        allow_spells = true;
                        spell_filter = Some(Box::new(f));
                    }
                }
            }
            if allow_lands || allow_spells {
                return Some(vec![make_static(
                    StaticBody::ZonePlayPermission {
                        zone: Zone::Library,
                        allow_lands,
                        allow_spells,
                        spell_filter,
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
                        alt_cost_defs: Vec::new(),
                        grant_flash: false,
                    },
                    condition,
                )]);
            }
        }

        if let Some((_, rest_after)) = split_once_ci(rest, "cast ")
            && let Some((obj_s, zone_part)) = split_once_ci(rest_after, " from ")
            && is_self_phrase(obj_s.trim())
        {
            let (zone_only, trailing_cond) =
                if let Some((zp, cond_s)) = split_once_ci(zone_part, " as long as ") {
                    (zp.trim(), parse_static_condition(cond_s.trim(), ctx))
                } else {
                    (zone_part.trim(), None)
                };
            let final_cond = match (condition, trailing_cond) {
                (Some(a), Some(b)) => Some(Condition::And { of: vec![a, b] }),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
            if let Some((zone_base, by_cost_raw)) = split_once_ci(zone_only, " by ") {
                let allowed_zones = if eq_ci(zone_base.trim(), "your graveyard") {
                    ZoneMask::GRAVEYARD
                } else if eq_ci(zone_base.trim(), "exile") {
                    ZoneMask::EXILE
                } else {
                    ctx.interner.rollback(cp);
                    return None;
                };
                let (cost_body, additional_only) = if let Some(cb) =
                    strip_suffix_ci(by_cost_raw.trim(), " rather than paying its mana cost")
                        .or_else(|| {
                            strip_suffix_ci(
                                by_cost_raw.trim(),
                                " rather than paying this card's mana cost",
                            )
                        })
                        .or_else(|| {
                            strip_suffix_ci(
                                by_cost_raw.trim(),
                                " rather than paying this spell's mana cost",
                            )
                        }) {
                    (cb.trim(), false)
                } else if let Some(cb) =
                    strip_suffix_ci(by_cost_raw.trim(), " in addition to paying its other costs")
                        .or_else(|| {
                            strip_suffix_ci(
                                by_cost_raw.trim(),
                                " in addition to paying other costs",
                            )
                        })
                {
                    (cb.trim(), true)
                } else {
                    ctx.interner.rollback(cp);
                    return None;
                };
                let norm_cost = cost_body
                    .split(" and ")
                    .map(str::trim)
                    .map(|part| {
                        if let Some(r) = strip_prefix_ci(part, "paying ") {
                            let r = r.trim();
                            if r.starts_with('{') {
                                r.to_string()
                            } else {
                                format!("Pay {r}")
                            }
                        } else if let Some(r) = strip_prefix_ci(part, "exiling ") {
                            format!("Exile {}", r.trim())
                        } else if let Some(r) = strip_prefix_ci(part, "sacrificing ") {
                            format!("Sacrifice {}", r.trim())
                        } else if let Some(r) = strip_prefix_ci(part, "discarding ") {
                            format!("Discard {}", r.trim())
                        } else if let Some(r) = strip_prefix_ci(part, "returning ") {
                            format!("Return {}", r.trim())
                        } else if let Some(r) = strip_prefix_ci(part, "removing ") {
                            format!("Remove {}", r.trim())
                        } else {
                            part.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let cparse = parse_cost_list(&norm_cost, 0, ctx);
                if cparse.defs.is_empty() || !cparse.unparsed.is_empty() {
                    ctx.interner.rollback(cp);
                    return None;
                }
                let mut mana_cost = ManaCost {
                    raw: "{0}".to_string(),
                    ..Default::default()
                };
                let mut cost_defs = Vec::new();
                for d in cparse.defs {
                    if let CostDef::Mana { cost, .. } = &d
                        && mana_cost.raw == "{0}"
                        && !additional_only
                    {
                        mana_cost = cost.clone();
                    } else {
                        cost_defs.push(d);
                    }
                }
                return Some(vec![make_static(
                    StaticBody::AltCastPermission {
                        mana_cost,
                        use_printed_mana_cost: false,
                        cost_defs,
                        affected: Affected::SelfObj,
                        allowed_zones,
                        once_each_turn: false,
                        during_your_turn_only: false,
                        grant_flash: false,
                        any_player: false,
                        dynamic_mana_cost_from_mv: false,
                        max_mv_by_controlled_count: None,
                        max_mv_by_self_counters: None,
                        enters_with_counters: Vec::new(),
                        additional_only,
                        additional_per_metric: None,
                        require_zone_permission: false,
                        pay_life_equal_to_mv: false,
                        slot_mode: None,
                    },
                    final_cond,
                )]);
            }
            let zone = if starts_with_ci(zone_only, "your graveyard") {
                if contains_ci(zone_only, "rather than paying")
                    || contains_ci(zone_only, "in addition to paying")
                {
                    ctx.interner.rollback(cp);
                    return None;
                }
                if let Some((_, kw_tail)) = split_once_ci(zone_only, "using its ")
                    && let Some(kw_raw) =
                        strip_suffix_ci(kw_tail.trim().trim_end_matches('.'), " ability")
                {
                    let kw_enum = match kw_raw.trim().to_ascii_lowercase().as_str() {
                        "blitz" => Some(mtg_ir::GraveyardSelfCastKeyword::Blitz),
                        "dash" => Some(mtg_ir::GraveyardSelfCastKeyword::Dash),
                        "bestow" => Some(mtg_ir::GraveyardSelfCastKeyword::Bestow),
                        "mutate" => Some(mtg_ir::GraveyardSelfCastKeyword::Mutate),
                        "warp" => Some(mtg_ir::GraveyardSelfCastKeyword::Warp),
                        _ => None,
                    };
                    if let Some(keyword) = kw_enum {
                        return Some(vec![make_static(
                            StaticBody::SelfGraveyardKeywordCast { keyword },
                            final_cond,
                        )]);
                    }
                }
                Zone::Graveyard
            } else if starts_with_ci(zone_only, "exile") {
                if contains_ci(zone_only, "rather than paying")
                    || contains_ci(zone_only, "in addition to paying")
                {
                    ctx.interner.rollback(cp);
                    return None;
                }
                Zone::Exile
            } else {
                ctx.interner.rollback(cp);
                return None;
            };
            return Some(vec![make_static(
                StaticBody::ZonePlayPermission {
                    zone,
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
                    alt_cost_defs: Vec::new(),
                    grant_flash: false,
                },
                final_cond,
            )]);
        }

        if contains_ci(rest, "exiled with ") || contains_ci(rest, "from exile") {
            let (exile_body, spend_any) =
                if let Some((l, r)) = split_once_ci(rest, ", and mana of any type can be spent") {
                    let _ = r;
                    (l.trim(), true)
                } else {
                    (rest.trim(), false)
                };
            let grant_flash = contains_ci(exile_body, "as though they had flash")
                || contains_ci(exile_body, "as though it had flash");
            let exile_core = split_once_ci(exile_body, " and you may cast ")
                .map(|(l, _)| l.trim())
                .unwrap_or(exile_body);
            let exiled_with_self = contains_ci(exile_core, "exiled with ~")
                || contains_ci(exile_core, "exiled with this ");
            let allow_lands = contains_ci(exile_core, "play lands")
                || contains_ci(exile_core, "play land")
                || contains_ci(exile_core, "play cards");
            let allow_spells =
                contains_ci(exile_core, "cast ") || contains_ci(exile_core, "play cards");
            let card_owner = if contains_ci(exile_core, "you don't own") {
                Some(RelativeControl::Opponent)
            } else if contains_ci(exile_core, "you own") {
                Some(RelativeControl::You)
            } else {
                None
            };
            let require_counter = if let Some((_, after_with)) = split_once_ci(exile_core, " with ")
                && let Some((ctr_part, _)) = split_once_ci(after_with, " counter")
            {
                let ctr_clean = strip_leading_article(ctr_part.trim());
                lookup_static_counter(ctr_clean, ctx)
            } else {
                None
            };
            if allow_lands || allow_spells {
                let during_your_turn_only = matches!(
                    condition,
                    Some(Condition::IsTurnOf {
                        player: PlayerRef::You
                    })
                );
                return Some(vec![make_static(
                    StaticBody::ZonePlayPermission {
                        zone: Zone::Exile,
                        allow_lands,
                        allow_spells,
                        spell_filter: None,
                        self_only: false,
                        during_your_turn_only,
                        slot_mode: None,
                        exile_if_put_into_graveyard: false,
                        exiled_with_self,
                        require_counter,
                        card_owner,
                        spend_mana_as_any_color: spend_any,
                        enters_tapped: false,
                        grant_haste_until_eot: false,
                        put_into_graveyard_from_non_battlefield_this_turn: false,
                        alt_cost_defs: Vec::new(),
                        grant_flash,
                    },
                    condition,
                )]);
            }
        }

        if let Some((what_s, after_gy)) = split_once_ci(rest, " from your graveyard") {
            let once_per_turn = lead_once
                || contains_ci(after_gy, "once during each of your turns")
                || contains_ci(after_gy, "once on each of your turns");
            let allow_lands = contains_ci(what_s, "play lands") || contains_ci(what_s, "play land");
            let clean_what = strip_prefix_ci(what_s.trim(), "play ")
                .or_else(|| strip_prefix_ci(what_s.trim(), "cast "))
                .unwrap_or(what_s.trim());
            let clean_what = strip_suffix_ci(clean_what.trim(), " spells")
                .or_else(|| strip_suffix_ci(clean_what.trim(), " spell"))
                .unwrap_or(clean_what.trim());
            let clean_what = strip_leading_article(clean_what.trim());
            let allow_spells = !allow_lands || contains_ci(what_s, "spell");
            let spell_filter = if !allow_spells {
                None
            } else if allow_lands {
                Some(Box::new(ObjectFilter {
                    types_any: CardTypeMask::LAND,
                    ..Default::default()
                }))
            } else if eq_ci(clean_what, "spells")
                || eq_ci(clean_what, "cards")
                || clean_what.is_empty()
            {
                None
            } else {
                Some(Box::new(
                    parse_object_with_qualifiers(clean_what, ctx, FilterOpts::default())
                        .unwrap_or_default(),
                ))
            };
            return Some(vec![make_static(
                StaticBody::ZonePlayPermission {
                    zone: Zone::Graveyard,
                    allow_lands,
                    allow_spells,
                    spell_filter,
                    self_only: false,
                    during_your_turn_only: lead_once
                        || contains_ci(after_gy, "during each of your turns")
                        || contains_ci(after_gy, "on each of your turns"),
                    slot_mode: if once_per_turn {
                        Some(mtg_ir::SlotMode::OncePerTurn)
                    } else {
                        None
                    },
                    exile_if_put_into_graveyard: false,
                    exiled_with_self: false,
                    require_counter: None,
                    card_owner: None,
                    spend_mana_as_any_color: false,
                    enters_tapped: false,
                    grant_haste_until_eot: false,
                    put_into_graveyard_from_non_battlefield_this_turn: false,
                    alt_cost_defs: Vec::new(),
                    grant_flash: false,
                },
                condition,
            )]);
        }
    }

    if let Some(after_once) = strip_prefix_ci(
        t,
        "once each turn, you may pay {0} rather than pay the mana cost for ",
    ) && let Some((what_s, zone_s)) = split_once_ci(after_once, " you cast from ")
    {
        let q = strip_suffix_ci(what_s.trim(), " spells")
            .map(|p| format!("{p} cards"))
            .or_else(|| strip_suffix_ci(what_s.trim(), " spell").map(|p| format!("{p} card")))
            .unwrap_or_else(|| what_s.trim().to_string());
        let mut f =
            parse_object_with_qualifiers(&q, ctx, FilterOpts::default()).unwrap_or_default();
        f.zones = ZoneMask::empty();
        let allowed_zones = if contains_ci(zone_s, "graveyard") {
            ZoneMask::GRAVEYARD
        } else if contains_ci(zone_s, "exile") {
            ZoneMask::EXILE
        } else {
            ZoneMask::HAND
        };
        return Some(vec![make_static(
            StaticBody::AltCastPermission {
                mana_cost: ManaCost {
                    raw: "{0}".to_string(),
                    ..Default::default()
                },
                use_printed_mana_cost: false,
                cost_defs: Vec::new(),
                affected: Affected::Filter {
                    filter: Box::new(f),
                },
                allowed_zones,
                once_each_turn: true,
                during_your_turn_only: false,
                grant_flash: false,
                any_player: false,
                dynamic_mana_cost_from_mv: false,
                max_mv_by_controlled_count: None,
                max_mv_by_self_counters: None,
                enters_with_counters: Vec::new(),
                additional_only: false,
                additional_per_metric: None,
                require_zone_permission: false,
                pay_life_equal_to_mv: false,
                slot_mode: None,
            },
            condition,
        )]);
    }

    // "If <cond>, you may cast ~ without paying its mana cost" (e.g. Deepwood Legate)
    if let Some(after_if) = strip_prefix_ci(t, "if ")
        && let Some((cond_s, rest_s)) = split_once_ci(after_if, ", you may cast ")
        && let Some((what_s, _)) = split_once_ci(rest_s, " without paying its mana cost")
        && is_self_phrase(what_s.trim())
        && let Some(c) = parse_static_condition(cond_s.trim(), ctx)
    {
        let combined = match condition.clone() {
            Some(outer) => Some(Condition::And { of: vec![outer, c] }),
            None => Some(c),
        };
        return Some(vec![make_static(
            StaticBody::AltCastPermission {
                mana_cost: ManaCost {
                    raw: "{0}".to_string(),
                    ..Default::default()
                },
                use_printed_mana_cost: false,
                cost_defs: Vec::new(),
                affected: Affected::SelfObj,
                allowed_zones: ZoneMask::empty(),
                once_each_turn: false,
                during_your_turn_only: false,
                grant_flash: false,
                any_player: false,
                dynamic_mana_cost_from_mv: false,
                max_mv_by_controlled_count: None,
                max_mv_by_self_counters: None,
                enters_with_counters: Vec::new(),
                additional_only: false,
                additional_per_metric: None,
                require_zone_permission: false,
                pay_life_equal_to_mv: false,
                slot_mode: None,
            },
            combined,
        )]);
    }
    ctx.interner.rollback(cp);

    // "You may pay <costs> rather than pay ~'s mana cost" / "... the mana cost for <spells> you cast"
    if let Some(after_pay) = strip_prefix_ci(t, "you may pay ")
        && let Some((cost_raw, rhs)) = split_once_ci(after_pay, " rather than pay ")
    {
        let target_opt =
            if eq_ci(rhs.trim(), "~'s mana cost") || eq_ci(rhs.trim(), "this spell's mana cost") {
                Some(Affected::SelfObj)
            } else if let Some(what_s) = strip_prefix_ci(rhs.trim(), "the mana cost for ")
                .and_then(|r| strip_suffix_ci(r.trim(), " you cast"))
            {
                let w = what_s.trim();
                if eq_ci(w, "spells") || eq_ci(w, "a spell") {
                    Some(Affected::Filter {
                        filter: Box::new(ObjectFilter {
                            types_any: CardTypeMask::CREATURE
                                | CardTypeMask::ARTIFACT
                                | CardTypeMask::ENCHANTMENT
                                | CardTypeMask::INSTANT
                                | CardTypeMask::SORCERY
                                | CardTypeMask::PLANESWALKER
                                | CardTypeMask::BATTLE,
                            ..Default::default()
                        }),
                    })
                } else {
                    let q = strip_suffix_ci(w, " spells")
                        .map(|p| format!("{p} cards"))
                        .or_else(|| strip_suffix_ci(w, " spell").map(|p| format!("{p} card")))
                        .unwrap_or_else(|| w.to_string());
                    parse_object_with_qualifiers(&q, ctx, FilterOpts::default()).map(|mut f| {
                        f.zones = ZoneMask::empty();
                        Affected::Filter {
                            filter: Box::new(f),
                        }
                    })
                }
            } else {
                None
            };
        if let Some(affected) = target_opt {
            let cost_str = cost_raw.trim();
            let norm_cost = if contains_ci(cost_str, " and ") {
                Cow::Owned(cost_str.replace(" and ", ", "))
            } else {
                Cow::Borrowed(cost_str)
            };
            let parsed_costs = parse_cost_list(&norm_cost, 0, ctx);
            if parsed_costs.unparsed.is_empty()
                && !parsed_costs.defs.is_empty()
                && parsed_costs.defs.iter().all(is_spell_alt_cost_compatible)
            {
                let mut mana_cost = ManaCost {
                    raw: "{0}".to_string(),
                    ..Default::default()
                };
                let mut cost_defs = Vec::new();
                for cd in parsed_costs.defs {
                    if let CostDef::Mana { cost, .. } = cd {
                        mana_cost = cost;
                    } else {
                        cost_defs.push(cd);
                    }
                }
                return Some(vec![make_static(
                    StaticBody::AltCastPermission {
                        mana_cost,
                        use_printed_mana_cost: false,
                        cost_defs,
                        affected,
                        allowed_zones: ZoneMask::empty(),
                        once_each_turn: false,
                        during_your_turn_only: false,
                        grant_flash: false,
                        any_player: false,
                        dynamic_mana_cost_from_mv: false,
                        max_mv_by_controlled_count: None,
                        max_mv_by_self_counters: None,
                        enters_with_counters: Vec::new(),
                        additional_only: false,
                        additional_per_metric: None,
                        require_zone_permission: false,
                        pay_life_equal_to_mv: false,
                        slot_mode: None,
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
    }

    if contains_ci(t, "without paying ")
        && contains_ci(t, "mana cost")
        && let Some(rest) = strip_prefix_ci(t, "you may cast ")
        && let Some((what_s, _)) = split_once_ci(rest, " without paying")
    {
        if is_self_phrase(what_s.trim()) {
            return Some(vec![make_static(
                StaticBody::AltCastPermission {
                    mana_cost: ManaCost {
                        raw: "{0}".to_string(),
                        ..Default::default()
                    },
                    use_printed_mana_cost: false,
                    cost_defs: Vec::new(),
                    affected: Affected::SelfObj,
                    allowed_zones: ZoneMask::empty(),
                    once_each_turn: false,
                    during_your_turn_only: false,
                    grant_flash: false,
                    any_player: false,
                    dynamic_mana_cost_from_mv: false,
                    max_mv_by_controlled_count: None,
                    max_mv_by_self_counters: None,
                    enters_with_counters: Vec::new(),
                    additional_only: false,
                    additional_per_metric: None,
                    require_zone_permission: false,
                    pay_life_equal_to_mv: false,
                    slot_mode: None,
                },
                condition,
            )]);
        }
        let mut f =
            parse_object_with_qualifiers(what_s, ctx, FilterOpts::default()).unwrap_or_default();
        f.zones = ZoneMask::empty();
        return Some(vec![make_static(
            StaticBody::AltCastPermission {
                mana_cost: ManaCost::default(),
                use_printed_mana_cost: false,
                cost_defs: Vec::new(),
                affected: Affected::Filter {
                    filter: Box::new(f),
                },
                allowed_zones: ZoneMask::HAND,
                once_each_turn: false,
                during_your_turn_only: false,
                grant_flash: false,
                any_player: false,
                dynamic_mana_cost_from_mv: false,
                max_mv_by_controlled_count: None,
                max_mv_by_self_counters: None,
                enters_with_counters: Vec::new(),
                additional_only: false,
                additional_per_metric: None,
                require_zone_permission: false,
                pay_life_equal_to_mv: false,
                slot_mode: None,
            },
            condition,
        )]);
    }

    if let Some(rest) = strip_prefix_ci(t, "you may cast ")
        && let Some((what_s_raw, after_flash)) = split_once_ci(rest, " as though it had flash")
            .or_else(|| split_once_ci(rest, " as though they had flash"))
    {
        let flash_cond = if let Some(if_rest) = strip_prefix_ci(after_flash.trim(), "if ") {
            parse_static_condition(if_rest, ctx)
        } else {
            None
        };
        let combined = match (condition.clone(), flash_cond) {
            (Some(a), Some(b)) => Some(Condition::And { of: vec![a, b] }),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let mut what_s = what_s_raw.trim();
        let mut first_matching_this_turn = false;
        if let Some(after_first) = strip_prefix_ci(what_s, "the first ")
            && let Some(core) = strip_suffix_ci(after_first.trim(), " you cast each turn")
                .or_else(|| strip_suffix_ci(after_first.trim(), " you cast"))
        {
            first_matching_this_turn = true;
            what_s = core.trim();
        }
        if is_self_phrase(what_s) {
            let kw = ctx.interner.intern_keyword("flash");
            return Some(vec![make_static(
                StaticBody::SpellGrantKeyword {
                    filter: Box::new(ObjectFilter::default()),
                    keywords: smallvec![kw],
                    evoke_cost: None,
                    cascade_count: None,
                    param_n: None,
                },
                combined,
            )]);
        }
        let what_clean = what_s.trim();
        let spells_opt: Option<Box<ObjectFilter>> = if eq_ci(what_clean, "spells") {
            None
        } else if let Some((left_s, right_s)) = split_once_ci(what_clean, " spells and ")
            && let Some(r_base) = strip_suffix_ci(right_s.trim(), " spells")
        {
            let l_q = format!("{} card", left_s.trim());
            let r_q = format!("{} card", r_base.trim());
            if let Some(mut f1) = parse_object_with_qualifiers(&l_q, ctx, FilterOpts::default())
                && let Some(mut f2) = parse_object_with_qualifiers(&r_q, ctx, FilterOpts::default())
            {
                f1.zones = ZoneMask::empty();
                f2.zones = ZoneMask::empty();
                Some(Box::new(ObjectFilter {
                    extra: vec![ObjectPredicate::AnyOf {
                        filters: vec![f1, f2],
                    }],
                    ..Default::default()
                }))
            } else {
                ctx.interner.rollback(cp);
                return None;
            }
        } else {
            let query = if let Some(base) = strip_suffix_ci(what_clean, " spells")
                .or_else(|| strip_suffix_ci(what_clean, " spell"))
            {
                format!("{} card", base.trim())
            } else {
                what_clean.to_string()
            };
            if let Some(mut parsed) =
                parse_object_with_qualifiers(&query, ctx, FilterOpts::default())
            {
                parsed.zones = ZoneMask::empty();
                Some(Box::new(parsed))
            } else {
                ctx.interner.rollback(cp);
                return None;
            }
        };
        return Some(vec![make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CastAsThoughFlash {
                    spells: spells_opt,
                    self_only: false,
                    first_matching_this_turn,
                    sacrifice_at_next_cleanup_if_non_sorcery: false,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
            },
            combined,
        )]);
    }

    if let Some(after_first) = strip_prefix_ci(t, "the first spell you cast each turn ")
        && let Some(after_cond) = strip_prefix_ci(
            after_first.trim(),
            "that mana from a treasure was spent to cast ",
        )
        && let Some(kw_body) = strip_prefix_ci(after_cond.trim(), "has ")
            .or_else(|| strip_prefix_ci(after_cond.trim(), "have "))
    {
        let kw_clean = trim_clause_punct(kw_body.trim());
        if let Some(kws) = parse_keyword_id_list(kw_clean, ctx) {
            let treasure_cond = Condition::FirstSpellThisTurnWithManaFrom {
                mana_source: ManaSourceKind::Treasure,
            };
            let final_cond = match condition {
                Some(prev) => Condition::And {
                    of: vec![prev, treasure_cond],
                },
                None => treasure_cond,
            };
            return Some(vec![make_static(
                StaticBody::SpellGrantKeyword {
                    filter: Box::new(ObjectFilter::default()),
                    keywords: kws,
                    evoke_cost: None,
                    cascade_count: None,
                    param_n: None,
                },
                Some(final_cond),
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some((left_spells, kw_tail_raw)) = strip_prefix_ci(t, "spells you cast ")
        .or_else(|| strip_prefix_ci(t, "spell you cast "))
        .or_else(|| strip_prefix_ci(t, "spells you control "))
        .or_else(|| strip_prefix_ci(t, "spell you control "))
        .map(|r| ("", r))
        .or_else(|| split_once_ci(t, " spells you cast "))
        .or_else(|| split_once_ci(t, " spell you cast "))
        .or_else(|| split_once_ci(t, " spells you control "))
        .or_else(|| split_once_ci(t, " spell you control "))
    {
        let mut kw_tail = kw_tail_raw.trim();
        let mut cast_zone: Option<ZoneMask> = None;
        if let Some(r) = strip_prefix_ci(kw_tail, "from your hand ") {
            cast_zone = Some(ZoneMask::HAND);
            kw_tail = r.trim();
        } else if let Some(r) = strip_prefix_ci(kw_tail, "from your graveyard ") {
            cast_zone = Some(ZoneMask::GRAVEYARD);
            kw_tail = r.trim();
        } else if let Some(r) = strip_prefix_ci(kw_tail, "from exile ") {
            cast_zone = Some(ZoneMask::EXILE);
            kw_tail = r.trim();
        }
        let mut mv_ge: Option<u16> = None;
        if let Some(after_mv) = strip_prefix_ci(kw_tail, "with mana value ")
            && let Some((num_s, rest_after)) = split_once_ci(after_mv, " or greater ")
            && let Some(n) = parse_num_word_or_int(num_s.trim())
        {
            mv_ge = Some(n as u16);
            kw_tail = rest_after.trim();
        }
        let kw_body_raw = strip_prefix_ci(kw_tail, "have ")
            .or_else(|| strip_prefix_ci(kw_tail, "has "))
            .or_else(|| strip_prefix_ci(kw_tail, "gain "))
            .or_else(|| strip_prefix_ci(kw_tail, "gains "));
        if let Some(kw_body) = kw_body_raw {
            let kw_trimmed = strip_suffix_ci(kw_body.trim(), " as you cast them")
                .or_else(|| strip_suffix_ci(kw_body.trim(), " as you cast it"))
                .unwrap_or(kw_body.trim())
                .trim();
            let kw_clean = kw_trimmed
                .trim_start_matches('"')
                .trim_end_matches('"')
                .trim_end_matches('.')
                .trim();
            let left_clean =
                strip_prefix_ci(left_spells.trim(), "each ").unwrap_or(left_spells.trim());
            let subj_norm = left_clean.replace(" and ", " or ").replace(" AND ", " or ");
            let f_opt = if subj_norm.trim().is_empty() {
                Some(ObjectFilter::default())
            } else {
                let subj_query = format!("{} card", subj_norm.trim());
                parse_object_with_qualifiers(&subj_query, ctx, FilterOpts::default())
            };
            if let Some(mut f) = f_opt {
                f.zones = ZoneMask::empty();
                if let Some(cz) = cast_zone {
                    f.extra.push(ObjectPredicate::CastFromZones { zones: cz });
                }
                if let Some(mv) = mv_ge {
                    f.mana_value = Some(mtg_ir::Compare {
                        op: CompareOp::Ge,
                        value: Box::new(fixed_amt(mv as i32)),
                    });
                }
                if eq_ci(kw_clean, "cascade, cascade") {
                    let kw = ctx.interner.intern_keyword("cascade");
                    return Some(vec![make_static(
                        StaticBody::SpellGrantKeyword {
                            filter: Box::new(f),
                            keywords: smallvec![kw],
                            evoke_cost: None,
                            cascade_count: Some(2),
                            param_n: None,
                        },
                        condition,
                    )]);
                } else if let Some(rep_n_s) = strip_prefix_ci(kw_clean, "ripple ")
                    && let Some(n) = parse_num_word_or_int(rep_n_s.trim())
                {
                    let kw = ctx.interner.intern_keyword("ripple");
                    return Some(vec![make_static(
                        StaticBody::SpellGrantKeyword {
                            filter: Box::new(f),
                            keywords: smallvec![kw],
                            evoke_cost: None,
                            cascade_count: None,
                            param_n: Some(n as u16),
                        },
                        condition,
                    )]);
                } else if let Some(ev_cost_s) = strip_prefix_ci(kw_clean, "evoke ") {
                    let cparse = parse_cost_list(ev_cost_s.trim(), 0, ctx);
                    if let [CostDef::Mana { cost, .. }] = cparse.defs.as_slice() {
                        let kw = ctx.interner.intern_keyword("evoke");
                        return Some(vec![make_static(
                            StaticBody::SpellGrantKeyword {
                                filter: Box::new(f),
                                keywords: smallvec![kw],
                                evoke_cost: Some(cost.clone()),
                                cascade_count: None,
                                param_n: None,
                            },
                            condition,
                        )]);
                    }
                } else if let Some(kws) = parse_keyword_id_list(kw_clean, ctx) {
                    return Some(vec![make_static(
                        StaticBody::SpellGrantKeyword {
                            filter: Box::new(f),
                            keywords: kws,
                            evoke_cost: None,
                            cascade_count: None,
                            param_n: None,
                        },
                        condition,
                    )]);
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    if let Some(rest) = strip_prefix_ci(t, "each ")
        && let Some((what_s, has_s)) = split_once_ci(rest, " in your graveyard has ")
    {
        if let Some(f) = parse_object_with_qualifiers(what_s, ctx, FilterOpts::default()) {
            if contains_ci(has_s, "unearth") {
                return Some(vec![make_static(
                    StaticBody::GrantGraveyardKeywordAbility {
                        filter: Box::new(f),
                        keyword: mtg_ir::GraveyardGrantKeyword::Unearth,
                        cost_override: None,
                    },
                    condition,
                )]);
            } else if contains_ci(has_s, "scavenge") {
                return Some(vec![make_static(
                    StaticBody::GrantGraveyardKeywordAbility {
                        filter: Box::new(f),
                        keyword: mtg_ir::GraveyardGrantKeyword::Scavenge,
                        cost_override: None,
                    },
                    condition,
                )]);
            } else if contains_ci(has_s, "escape") {
                return Some(vec![make_static(
                    StaticBody::GrantGraveyardEscape {
                        filter: Box::new(f),
                        exile_count: 3,
                    },
                    condition,
                )]);
            }
        }
        ctx.interner.rollback(cp);
    }

    ctx.interner.rollback(cp);
    None
}

fn make_cost_delta(mode: CostDeltaMode, generic: u16, per: Option<Box<Amount>>) -> CostDelta {
    CostDelta {
        mode,
        generic,
        per,
        additional_costs: Vec::new(),
        colored: SmallVec::new(),
        hybrid_colored: SmallVec::new(),
        colored_only: false,
        min_total_mana: None,
        max_reduction: None,
        per_target_life: None,
    }
}

fn parse_cost_modifier_static_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    let looks_cost = contains_ci(t, "cost")
        || contains_ci(t, "costs")
        || contains_ci(t, "affinity for ")
        || contains_ci(t, "rather than pay")
        || contains_ci(t, "additional cost");
    if !looks_cost {
        return None;
    }

    if let Some(rest) = strip_prefix_ci(t, "affinity for ") {
        if let Some(mut f) = parse_object_with_qualifiers(rest, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::BATTLEFIELD;
            f.controller = Some(RelativeControl::You);
            return Some(make_static(
                StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::SelfObj,
                    delta: make_cost_delta(
                        CostDeltaMode::Reduce,
                        1,
                        Some(Box::new(Amount::Count { of: Box::new(f) })),
                    ),
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::BATTLEFIELD,
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(mut cm) = parse_cost_modifier_sentence(t, ctx) {
        if let Some(c) = condition {
            cm.condition = Some(Box::new(match cm.condition {
                Some(prev) => Condition::And { of: vec![*prev, c] },
                None => c,
            }));
        }
        return Some(cm);
    }
    ctx.interner.rollback(cp);

    if starts_with_ci(t, "as an additional cost to cast") {
        let cp2 = ctx.interner.checkpoint();
        if let Some(ac) = parse_additional_cost_sentence(t, 0, false, ctx)
            && !ac.defs.is_empty()
            && ac.unparsed.is_empty()
            && ac.defs.iter().all(is_cast_payable_cost)
        {
            return Some(make_static(
                StaticBody::AdditionalCastCost { costs: ac.defs },
                condition,
            ));
        }
        ctx.interner.rollback(cp2);
    }

    if let Some((subj_s, cost_tail)) =
        split_once_ci(t, " cost ").or_else(|| split_once_ci(t, " costs "))
        && let Some((less_s, trailing_cond)) = split_once_ci(cost_tail.trim(), " to cast if ")
            .map(|(l, r)| (l, Some(r)))
            .or_else(|| strip_suffix_ci(cost_tail.trim(), " to cast").map(|l| (l, None)))
    {
        let (mode, amt_s) = if let Some(a) = strip_suffix_ci(less_s.trim(), " less") {
            (CostDeltaMode::Reduce, a.trim())
        } else if let Some(a) = strip_suffix_ci(less_s.trim(), " more") {
            (CostDeltaMode::Increase, a.trim())
        } else {
            ctx.interner.rollback(cp);
            return None;
        };

        let mut final_cond = condition;
        if let Some(cond_s) = trailing_cond {
            let Some(c) = parse_static_condition(cond_s.trim(), ctx) else {
                ctx.interner.rollback(cp);
                return None;
            };
            final_cond = Some(match final_cond {
                Some(prev) => Condition::And { of: vec![prev, c] },
                None => c,
            });
        }

        let is_self_spell = is_self_phrase(subj_s.trim()) || eq_ci(subj_s.trim(), "this spell");
        let affects = if is_self_spell {
            SpellOrAbilityFilter::SelfObj
        } else {
            let (obj_s, _ctrl) = split_object_and_controller(
                strip_suffix_ci(subj_s.trim(), " you cast")
                    .or_else(|| strip_suffix_ci(subj_s.trim(), " opponents cast"))
                    .unwrap_or(subj_s.trim()),
            );
            let mut f =
                parse_object_with_qualifiers(obj_s, ctx, FilterOpts::default()).unwrap_or_default();
            f.zones = ZoneMask::empty();
            SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            }
        };

        let who = PlayerFilter {
            relation: if contains_ci(subj_s, "opponents cast") {
                PlayerRelation::Opponent
            } else {
                PlayerRelation::You
            },
        };

        if let Ok(generic) = amt_s.trim_matches(|c| c == '{' || c == '}').parse::<u16>() {
            return Some(make_static(
                StaticBody::CostModifier {
                    affects,
                    delta: make_cost_delta(mode, generic, None),
                    who,
                    who_enchanted_player: false,
                    active_zones: if is_self_spell {
                        ZoneMask::STACK
                    } else {
                        ZoneMask::BATTLEFIELD
                    },
                },
                final_cond,
            ));
        }
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_cda_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();
    let after_poss = strip_self_possessive(t)?;

    // "~'s power is equal to <A> and its toughness is equal to that number plus <N>" (Tarmogoyf / Lhurgoyf)
    if let Some(after_p) = strip_prefix_ci(after_poss, "power is equal to ")
        && let Some((p_expr, t_expr)) = split_once_ci(after_p, " and its toughness is equal to ")
            .or_else(|| split_once_ci(after_p, ", and its toughness is equal to "))
        && let Some(p_amt) = parse_count_amount(p_expr.trim(), ctx)
    {
        let t_clean = t_expr.trim();
        let t_amt_opt = if let Some(plus_s) = strip_prefix_ci(t_clean, "that number plus ")
            && let Some(n) = parse_num_word_or_int(plus_s.trim())
        {
            Some(add_amounts(p_amt.clone(), fixed_amt(n)))
        } else if let Some(minus_s) = strip_prefix_ci(t_clean, "that number minus ")
            && let Some(n) = parse_num_word_or_int(minus_s.trim())
        {
            Some(add_amounts(p_amt.clone(), fixed_amt(-n)))
        } else {
            parse_count_amount(t_clean, ctx)
        };
        if let Some(t_amt) = t_amt_opt {
            return Some(make_static(
                wrap_layer_mod(
                    Layer::Layer7a,
                    Affected::SelfObj,
                    LayerMod::SetBasePt {
                        power: Box::new(p_amt),
                        toughness: Box::new(t_amt),
                    },
                ),
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    let (which, expr_s) =
        if let Some(r) = strip_prefix_ci(after_poss, "power and toughness are each equal to ") {
            (0u8, r.trim())
        } else if let Some(r) = strip_prefix_ci(after_poss, "power is equal to ") {
            (1u8, r.trim())
        } else {
            let r = strip_prefix_ci(after_poss, "toughness is equal to ")?;
            (2u8, r.trim())
        };

    let amt = match parse_count_amount(expr_s, ctx) {
        Some(a) => a,
        None => {
            ctx.interner.rollback(cp);
            return None;
        }
    };

    let layer_mod = match which {
        0 => LayerMod::SetBasePt {
            power: Box::new(amt.clone()),
            toughness: Box::new(amt),
        },
        1 => LayerMod::SetBasePower {
            power: Box::new(amt),
        },
        _ => LayerMod::SetBaseToughness {
            toughness: Box::new(amt),
        },
    };

    Some(make_static(
        wrap_layer_mod(Layer::Layer7a, Affected::SelfObj, layer_mod),
        condition,
    ))
}

fn parse_attached_type_change_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    let (subj_s, rhs) = split_once_ci(t, " is a ")
        .or_else(|| split_once_ci(t, " is an "))
        .or_else(|| split_once_ci(t, " is "))?;

    if !eq_ci(subj_s.trim(), "enchanted land")
        && !eq_ci(subj_s.trim(), "enchanted creature")
        && !eq_ci(subj_s.trim(), "enchanted permanent")
        && !eq_ci(subj_s.trim(), "equipped creature")
    {
        return None;
    }

    if let Some((types, subtypes, colors, colorless)) = parse_additive_types_and_colors(rhs, ctx) {
        let mut out = Vec::new();
        if !types.is_empty() || !subtypes.is_empty() {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer4,
                    Affected::AttachedTo,
                    LayerMod::AddTypes { types, subtypes },
                ),
                condition.clone(),
            ));
        }
        if let Some(c) = colors {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer5,
                    Affected::AttachedTo,
                    LayerMod::AddColors { colors: c },
                ),
                condition.clone(),
            ));
        } else if colorless {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer5,
                    Affected::AttachedTo,
                    LayerMod::SetColors {
                        colors: ColorMask::empty(),
                    },
                ),
                condition.clone(),
            ));
        }
        if !out.is_empty() {
            return Some(out);
        }
    }

    if let Some(parsed) = parse_type_description(rhs, ctx) {
        let mut out = Vec::new();
        if !parsed.types.is_empty() || !parsed.subtypes.is_empty() {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer4,
                    Affected::AttachedTo,
                    LayerMod::SetTypes {
                        types: parsed.types,
                        subtypes: parsed.subtypes,
                    },
                ),
                condition.clone(),
            ));
        }
        if let Some(c) = parsed.colors {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer5,
                    Affected::AttachedTo,
                    LayerMod::SetColors { colors: c },
                ),
                condition.clone(),
            ));
        } else if parsed.colorless {
            out.push(make_static(
                wrap_layer_mod(
                    Layer::Layer5,
                    Affected::AttachedTo,
                    LayerMod::SetColors {
                        colors: ColorMask::empty(),
                    },
                ),
                condition.clone(),
            ));
        }
        if !out.is_empty() {
            return Some(out);
        }
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_global_change_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some(rest) = strip_prefix_ci(t, "each ")
        && let Some((subj_s, rhs)) = split_once_ci(rest, " is a ")
            .or_else(|| split_once_ci(rest, " is an "))
            .or_else(|| split_once_ci(rest, " is "))
    {
        if let Some(aff) = parse_affected(subj_s, ctx) {
            let norm_rhs = strip_suffix_ci(rhs.trim(), " land types")
                .map(|p| format!("{p} types"))
                .unwrap_or_else(|| rhs.to_string());
            if let Some((types, subtypes, colors, _)) =
                parse_additive_types_and_colors(&norm_rhs, ctx)
            {
                let mut out = Vec::new();
                if !types.is_empty() || !subtypes.is_empty() {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer4,
                            aff.clone(),
                            LayerMod::AddTypes { types, subtypes },
                        ),
                        condition.clone(),
                    ));
                }
                if let Some(c) = colors {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer5,
                            aff.clone(),
                            LayerMod::AddColors { colors: c },
                        ),
                        condition.clone(),
                    ));
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
        ctx.interner.rollback(cp);
    }

    if let Some((subj_s, rhs)) = split_once_ci(t, " are ")
        .or_else(|| split_once_ci(t, " become "))
        .or_else(|| split_once_ci(t, " becomes "))
        .or_else(|| {
            if let Some(r) = strip_prefix_ci(t, "it's ") {
                Some(("it", r))
            } else {
                split_once_ci(t, " is ").filter(|(s, _)| is_self_phrase(s.trim()))
            }
        })
        && !contains_ci(rhs, "power")
        && !contains_ci(rhs, "toughness")
    {
        if let Some(aff) = parse_affected(subj_s, ctx) {
            let rhs_t = rhs.trim();
            if eq_ci(rhs_t, "every creature type") {
                return Some(vec![make_static(
                    wrap_layer_mod(Layer::Layer4, aff, LayerMod::AllCreatureTypes),
                    condition,
                )]);
            }
            if eq_ci(
                rhs_t,
                "every basic land type in addition to their other types",
            ) || eq_ci(rhs_t, "every basic land type")
            {
                let basics: SmallVec<[SubtypeId; 2]> =
                    ["Plains", "Island", "Swamp", "Mountain", "Forest"]
                        .iter()
                        .map(|s| ctx.interner.intern_subtype(s))
                        .collect();
                let mod_l4 = if contains_ci(rhs_t, "in addition to") {
                    LayerMod::AddTypes {
                        types: CardTypeMask::empty(),
                        subtypes: basics,
                    }
                } else {
                    LayerMod::SetSubtypes { subtypes: basics }
                };
                return Some(vec![make_static(
                    wrap_layer_mod(Layer::Layer4, aff, mod_l4),
                    condition,
                )]);
            }
            let rhs_trim = trim_clause_punct(rhs);
            if eq_ci(rhs_trim, "the chosen type in addition to its other types")
                || eq_ci(
                    rhs_trim,
                    "the chosen creature type in addition to its other types",
                )
                || eq_ci(rhs_trim, "the chosen type in addition to their other types")
                || eq_ci(
                    rhs_trim,
                    "the chosen creature type in addition to their other types",
                )
            {
                return Some(vec![make_static(
                    wrap_layer_mod(Layer::Layer4, aff, LayerMod::AddChosenSubtype),
                    condition,
                )]);
            }
            if let Some((types, subtypes, colors, _)) = parse_additive_types_and_colors(rhs, ctx) {
                let mut out = Vec::new();
                if !types.is_empty() || !subtypes.is_empty() {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer4,
                            aff.clone(),
                            LayerMod::AddTypes { types, subtypes },
                        ),
                        condition.clone(),
                    ));
                }
                if let Some(c) = colors {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer5,
                            aff.clone(),
                            LayerMod::AddColors { colors: c },
                        ),
                        condition.clone(),
                    ));
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
            if let Some(parsed) = parse_type_description(rhs, ctx) {
                let mut out = Vec::new();
                if !parsed.types.is_empty() {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer4,
                            aff.clone(),
                            LayerMod::SetTypes {
                                types: parsed.types,
                                subtypes: parsed.subtypes,
                            },
                        ),
                        condition.clone(),
                    ));
                } else if !parsed.subtypes.is_empty() {
                    out.push(make_static(
                        wrap_layer_mod(
                            Layer::Layer4,
                            aff.clone(),
                            LayerMod::SetSubtypes {
                                subtypes: parsed.subtypes,
                            },
                        ),
                        condition.clone(),
                    ));
                }
                if let Some(c) = parsed.colors {
                    out.push(make_static(
                        wrap_layer_mod(Layer::Layer5, aff, LayerMod::SetColors { colors: c }),
                        condition,
                    ));
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
        ctx.interner.rollback(cp);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_control_change_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();
    if let Some(rest) = strip_prefix_ci(t, "you control ") {
        let target = rest.trim();
        if eq_ci(target, "enchanted creature")
            || eq_ci(target, "enchanted permanent")
            || eq_ci(target, "enchanted artifact")
            || eq_ci(target, "enchanted land")
            || eq_ci(target, "equipped creature")
        {
            return Some(make_static(
                wrap_layer_mod(
                    Layer::Layer2,
                    Affected::AttachedTo,
                    LayerMod::SetController {
                        controller: PlayerRef::You,
                    },
                ),
                condition,
            ));
        }
        if let Some(aff) = parse_affected(target, ctx) {
            return Some(make_static(
                wrap_layer_mod(
                    Layer::Layer2,
                    aff,
                    LayerMod::SetController {
                        controller: PlayerRef::You,
                    },
                ),
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }
    None
}

fn parse_combat_and_activation_rule_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    // "You may choose not to untap <affected> during your untap step"
    if let Some(subj) = strip_prefix_ci(t, "you may choose not to untap ")
        .and_then(|r| strip_suffix_ci(r.trim(), " during your untap step"))
    {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![make_static(
                StaticBody::MayChooseNotToUntap { affected: aff },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some(subj) = strip_prefix_ci(t, "untap ")
        .and_then(|r| strip_suffix_ci(r.trim(), " during each other player's untap step"))
    {
        if is_self_phrase(subj.trim()) {
            return Some(vec![make_static(
                StaticBody::UntapDuringOtherUntapSteps {
                    affected: Affected::SelfObj,
                },
                condition,
            )]);
        }
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![make_static(
                StaticBody::UntapDuringOtherUntapSteps { affected: aff },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some(subj) = strip_suffix_ci(t, " untaps during each other player's untap step")
        && let Some(aff) = parse_affected(subj.trim(), ctx)
    {
        return Some(vec![make_static(
            StaticBody::UntapDuringOtherUntapSteps { affected: aff },
            condition,
        )]);
    }
    ctx.interner.rollback(cp);

    if eq_ci(t, "creatures with power less than ~'s power can't block it")
        || eq_ci(t, "creatures with power less than ~'s power can't block ~")
        || eq_ci(
            t,
            "creatures with power less than this creature's power can't block it",
        )
        || eq_ci(
            t,
            "creatures with power less than this creature's power can't block this creature",
        )
    {
        return Some(vec![make_static(
            StaticBody::CombatRule {
                affected: Affected::SelfObj,
                rule: CombatRuleSpec::CantBeBlockedByPowerLessThanSelf,
            },
            condition,
        )]);
    }

    if eq_ci(
        t,
        "creatures with power greater than ~'s power can't block it",
    ) || eq_ci(
        t,
        "creatures with power greater than ~'s power can't block ~",
    ) || eq_ci(
        t,
        "creatures with power greater than enchanted creature's power can't block it",
    ) || eq_ci(
        t,
        "creatures with power greater than enchanted creature's power can't block enchanted creature",
    ) || eq_ci(
        t,
        "creatures with power greater than equipped creature's power can't block it",
    ) || eq_ci(
        t,
        "creatures with power greater than equipped creature's power can't block equipped creature",
    ) {
        let affected = if contains_ci(t, "enchanted") || contains_ci(t, "equipped") {
            Affected::AttachedTo
        } else {
            Affected::SelfObj
        };
        return Some(vec![make_static(
            StaticBody::CombatRule {
                affected,
                rule: CombatRuleSpec::CantBeBlockedByPowerGreaterThanSelf,
            },
            condition,
        )]);
    }

    // "All creatures able to block <affected> do so"
    if let Some(subj) = strip_prefix_ci(t, "all creatures able to block ")
        .and_then(|r| strip_suffix_ci(r.trim(), " do so"))
    {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![make_static(
                StaticBody::CombatRule {
                    affected: aff,
                    rule: CombatRuleSpec::AllAbleBlockers,
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    // Super-trample variants
    if eq_ci(
        t,
        "enchanted creature's controller may have it assign its combat damage as though it weren't blocked",
    ) || eq_ci(
        t,
        "equipped creature's controller may have it assign its combat damage as though it weren't blocked",
    ) {
        return Some(vec![make_static(
            StaticBody::CombatRule {
                affected: Affected::AttachedTo,
                rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
            },
            condition,
        )]);
    }
    if let Some(subj) = strip_prefix_ci(t, "you may have ").and_then(|r| {
        strip_suffix_ci(r, " assign its combat damage as though it weren't blocked").or_else(|| {
            strip_suffix_ci(
                r,
                " assign their combat damage as though they weren't blocked",
            )
        })
    }) {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![make_static(
                StaticBody::CombatRule {
                    affected: aff,
                    rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }
    if let Some(subj) = strip_prefix_ci(t, "for each ").and_then(|r| {
        strip_suffix_ci(
            r,
            ", you may have that creature assign its combat damage as though it weren't blocked",
        )
        .or_else(|| {
            strip_suffix_ci(
                r,
                ", you may have that permanent assign its combat damage as though it weren't blocked",
            )
        })
    }) {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![make_static(
                StaticBody::CombatRule {
                    affected: aff,
                    rule: CombatRuleSpec::AssignCombatDamageAsThoughUnblocked,
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    // Combined toughness damage + defender override
    if let Some(subj) = strip_suffix_ci(
        t,
        " assigns combat damage equal to its toughness rather than its power and can attack as though it didn't have defender",
    )
    .or_else(|| {
        strip_suffix_ci(
            t,
            " assign combat damage equal to their toughness rather than their power and can attack as though they didn't have defender",
        )
    })
    .or_else(|| {
        strip_suffix_ci(
            t,
            " can attack as though it didn't have defender and assigns combat damage equal to its toughness rather than its power",
        )
    })
    .or_else(|| {
        strip_suffix_ci(
            t,
            " can attack as though they didn't have defender and assign combat damage equal to their toughness rather than their power",
        )
    }) {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![
                make_static(
                    StaticBody::AssignCombatDamageByToughness {
                        affected: aff.clone(),
                    },
                    condition.clone(),
                ),
                make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CanAttackDespiteDefender,
                    },
                    condition,
                ),
            ]);
        }
        ctx.interner.rollback(cp);
    }

    if contains_ci(t, "assigns combat damage equal to its toughness")
        || contains_ci(t, "assign combat damage equal to their toughness")
    {
        let subj = split_once_ci(t, " assigns combat damage")
            .or_else(|| split_once_ci(t, " assign combat damage"))
            .map(|(l, _)| l)
            .unwrap_or("creatures");
        if let Some(aff) = parse_affected(subj, ctx) {
            return Some(vec![make_static(
                StaticBody::AssignCombatDamageByToughness { affected: aff },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    // Combined "can't attack or block alone" -> [CantAttackAlone, CantBlockAlone]
    if let Some(subj) = strip_suffix_ci(t, " can't attack or block alone") {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![
                make_static(
                    StaticBody::CombatRule {
                        affected: aff.clone(),
                        rule: CombatRuleSpec::CantAttackAlone,
                    },
                    condition.clone(),
                ),
                make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CantBlockAlone,
                    },
                    condition,
                ),
            ]);
        }
        ctx.interner.rollback(cp);
    }

    // Combined "<affected> can't block or be blocked by <filter>"
    if let Some((subj, rest)) = split_once_ci(t, " can't block or be blocked by ") {
        if let Some(aff) = parse_affected(subj.trim(), ctx)
            && let Some(filter) = parse_combat_object_phrase(rest.trim(), ctx)
        {
            return Some(vec![
                make_static(
                    StaticBody::CombatRule {
                        affected: aff.clone(),
                        rule: CombatRuleSpec::CantBlockFilter {
                            filter: Box::new(filter.clone()),
                        },
                    },
                    condition.clone(),
                ),
                make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CantBeBlockedBy {
                            filter: Box::new(filter),
                        },
                    },
                    condition,
                ),
            ]);
        }
        ctx.interner.rollback(cp);
    }

    // Combined "<affected> can't attack you or block creatures you control"
    if let Some(subj) = strip_suffix_ci(t, " can't attack you or block creatures you control") {
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(vec![
                make_static(
                    StaticBody::Restriction {
                        affected: aff.clone(),
                        restriction: RestrictionKind::CantAttackYou,
                    },
                    condition.clone(),
                ),
                make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CantBlockFilter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                controller: Some(RelativeControl::You),
                                ..Default::default()
                            }),
                        },
                    },
                    condition,
                ),
            ]);
        }
        ctx.interner.rollback(cp);
    }

    // "No more than <N> creature(s) can attack [you] / block each combat"
    if let Some(rest) = strip_prefix_ci(t, "no more than ")
        && let Some((num_s, tail)) =
            split_once_ci(rest, " creatures can ").or_else(|| split_once_ci(rest, " creature can "))
        && let Some(n) = parse_num_word_or_int(num_s.trim())
        && n >= 1
    {
        if eq_ci(tail, "attack each combat") || eq_ci(tail, "attack you each combat") {
            return Some(vec![make_static(
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::GlobalMaxAttackers {
                        max: n as u16,
                        against_you_only: contains_ci(tail, " you "),
                    },
                },
                condition,
            )]);
        }
        if eq_ci(tail, "block each combat") {
            return Some(vec![make_static(
                StaticBody::CombatRule {
                    affected: Affected::SelfObj,
                    rule: CombatRuleSpec::GlobalMaxBlockers { max: n as u16 },
                },
                condition,
            )]);
        }
    }

    // "Creatures with <landwalk> can be blocked as though they didn't have <landwalk>"
    if let Some(rest) = strip_prefix_ci(t, "creatures with ") {
        if eq_ci(
            rest,
            "landwalk abilities can be blocked as though they didn't have those abilities",
        ) {
            let walks = [
                "plainswalk",
                "islandwalk",
                "swampwalk",
                "mountainwalk",
                "forestwalk",
                "nonbasic landwalk",
                "snow landwalk",
            ];
            let out = walks
                .iter()
                .map(|kw| {
                    let kw_id = ctx.interner.intern_keyword(kw);
                    make_static(
                        StaticBody::CombatRule {
                            affected: Affected::Filter {
                                filter: Box::new(ObjectFilter {
                                    types_any: CardTypeMask::CREATURE,
                                    ..Default::default()
                                }),
                            },
                            rule: CombatRuleSpec::IgnoreLandwalk { keyword: kw_id },
                        },
                        condition.clone(),
                    )
                })
                .collect();
            return Some(out);
        }
        if let Some((kw_raw, tail)) =
            split_once_ci(rest, " can be blocked as though they didn't have ")
            && eq_ci(kw_raw.trim(), tail.trim())
        {
            let kw_low = kw_raw.trim().to_ascii_lowercase();
            if matches!(
                kw_low.as_str(),
                "plainswalk"
                    | "islandwalk"
                    | "swampwalk"
                    | "mountainwalk"
                    | "forestwalk"
                    | "nonbasic landwalk"
                    | "snow landwalk"
            ) {
                let kw_id = ctx.interner.intern_keyword(&kw_low);
                return Some(vec![make_static(
                    StaticBody::CombatRule {
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter {
                                types_any: CardTypeMask::CREATURE,
                                ..Default::default()
                            }),
                        },
                        rule: CombatRuleSpec::IgnoreLandwalk { keyword: kw_id },
                    },
                    condition,
                )]);
            }
        }
    }

    // Crew/Saddle power mod
    if contains_ci(t, "crews vehicles")
        || contains_ci(t, "crew vehicles")
        || contains_ci(t, "saddles mounts")
        || contains_ci(t, "saddle mounts")
    {
        if let Some((left, right)) = split_once_ci(t, " as though ")
            && let Some(num_s) = strip_prefix_ci(right, "its power were ")
                .or_else(|| strip_prefix_ci(right, "their power were "))
                .and_then(|r| strip_suffix_ci(r, " greater"))
            && let Ok(delta) = num_s.trim().parse::<i32>()
            && delta > 0
        {
            let (subj_opt, scope) = if let Some(s) =
                strip_suffix_ci(left, " crews vehicles and saddles mounts")
                    .or_else(|| strip_suffix_ci(left, " crew vehicles and saddle mounts"))
                    .or_else(|| strip_suffix_ci(left, " saddles mounts and crews vehicles"))
                    .or_else(|| strip_suffix_ci(left, " saddle mounts and crew vehicles"))
            {
                (Some(s), CrewSaddleScope::Both)
            } else if let Some(s) = strip_suffix_ci(left, " crews vehicles")
                .or_else(|| strip_suffix_ci(left, " crew vehicles"))
            {
                (Some(s), CrewSaddleScope::Crew)
            } else if let Some(s) = strip_suffix_ci(left, " saddles mounts")
                .or_else(|| strip_suffix_ci(left, " saddle mounts"))
            {
                (Some(s), CrewSaddleScope::Saddle)
            } else {
                (None, CrewSaddleScope::Crew)
            };
            if let Some(subj) = subj_opt
                && let Some(aff) = parse_affected(subj.trim(), ctx)
            {
                return Some(vec![make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CrewSaddlePowerMod {
                            applies_to: scope,
                            delta: Some(delta),
                            use_toughness: false,
                        },
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
        if let Some(left) = strip_suffix_ci(t, " using its toughness rather than its power")
            .or_else(|| strip_suffix_ci(t, " using their toughness rather than their power"))
        {
            let (subj_opt, scope) = if let Some(s) =
                strip_suffix_ci(left, " crews vehicles and saddles mounts")
                    .or_else(|| strip_suffix_ci(left, " crew vehicles and saddle mounts"))
            {
                (Some(s), CrewSaddleScope::Both)
            } else if let Some(s) = strip_suffix_ci(left, " crews vehicles")
                .or_else(|| strip_suffix_ci(left, " crew vehicles"))
            {
                (Some(s), CrewSaddleScope::Crew)
            } else {
                (None, CrewSaddleScope::Crew)
            };
            if let Some(subj) = subj_opt
                && let Some(aff) = parse_affected(subj.trim(), ctx)
            {
                return Some(vec![make_static(
                    StaticBody::CombatRule {
                        affected: aff,
                        rule: CombatRuleSpec::CrewSaddlePowerMod {
                            applies_to: scope,
                            delta: None,
                            use_toughness: true,
                        },
                    },
                    condition,
                )]);
            }
            ctx.interner.rollback(cp);
        }
    }

    // "(Your opponents|Each opponent) can't block with <filter>"
    if let Some(rest) = strip_prefix_ci(t, "your opponents can't block with ")
        .or_else(|| strip_prefix_ci(t, "each opponent can't block with "))
    {
        if let Some(mut f) = parse_object_with_qualifiers(rest.trim(), ctx, FilterOpts::default())
            && f.controller.is_none()
        {
            f.controller = Some(RelativeControl::Opponent);
            f.zones = ZoneMask::empty();
            return Some(vec![make_static(
                StaticBody::Restriction {
                    affected: Affected::Filter {
                        filter: Box::new(f),
                    },
                    restriction: RestrictionKind::CantBlock,
                },
                condition,
            )]);
        }
        ctx.interner.rollback(cp);
    }

    if let Some((subj_s, _)) = split_once_ci(
        t,
        " attack enchanted creature's controller each combat if able",
    )
    .or_else(|| {
        split_once_ci(
            t,
            " attacks enchanted creature's controller each combat if able",
        )
    }) && let Some(mut aff) = parse_affected(subj_s.trim(), ctx)
    {
        if let Affected::Filter { filter } = &mut aff
            && filter.zones.is_empty()
        {
            filter.zones = ZoneMask::BATTLEFIELD;
        }
        return Some(vec![make_static(
            StaticBody::CombatRule {
                affected: aff,
                rule: CombatRuleSpec::MustAttackAttachedController,
            },
            condition,
        )]);
    }

    if (starts_with_ci(t, "activated abilities of ")
        || contains_ci(t, "'s activated abilities")
        || eq_ci(t, "activated abilities can't be activated")
        || eq_ci(
            t,
            "activated abilities can't be activated unless they're mana abilities",
        ))
        && contains_ci(t, "can't be activated")
    {
        let unless_mana = contains_ci(t, "unless they're mana abilities");
        let aff = if let Some((_, after_of)) = split_once_ci(t, "activated abilities of ") {
            let f_str = split_once_ci(after_of, " can't be activated")
                .map(|(l, _)| l.trim())
                .unwrap_or("");
            if f_str.is_empty() {
                Affected::Filter {
                    filter: Box::new(ObjectFilter::default()),
                }
            } else {
                parse_affected(f_str, ctx)?
            }
        } else if let Some((poss_subj, _)) = split_once_ci(t, "'s activated abilities") {
            parse_affected(poss_subj.trim(), ctx)?
        } else {
            Affected::Filter {
                filter: Box::new(ObjectFilter::default()),
            }
        };
        return Some(vec![make_static(
            StaticBody::CantActivateAbilities {
                affected: aff,
                allow_mana_abilities: unless_mana,
            },
            condition,
        )]);
    }

    for sep in [
        " can't attack or block",
        " can't attack",
        " can't block",
        " can block ",
        " can't be blocked",
        " must be blocked if able",
        " can attack as though ",
        " attacks each combat if able",
        " attack each combat if able",
        " attacks each turn if able",
        " attack each turn if able",
        " blocks each combat if able",
        " block each combat if able",
        " doesn't untap",
        " don't untap",
        " is goaded",
        " are goaded",
        " assigns its combat damage as though",
        " assign their combat damage as though",
    ] {
        if let Some(idx) = find_ci(t, sep) {
            let subj = t[..idx].trim();
            let raw_tail = t[idx..].trim();
            if let Some(aff) = parse_affected(subj, ctx) {
                // First try matching raw_tail directly (handles "can't attack unless defending player controls X", etc.)
                if let Some(c) = parse_inline_combat_tail(raw_tail, &aff, condition.as_ref(), ctx) {
                    return Some(vec![c]);
                }
                ctx.interner.rollback(cp);
                // Fallback: split trailing " unless <cond>" or " if <cond>"
                let split_cond = if let Some((l_tail, unless_s)) =
                    split_once_ci(raw_tail, " unless ")
                    && let Some(uc) = parse_static_condition(unless_s.trim(), ctx)
                {
                    let negated = Condition::Not { of: Box::new(uc) };
                    let combined = match condition.as_ref() {
                        Some(outer) => Condition::And {
                            of: vec![outer.clone(), negated],
                        },
                        None => negated,
                    };
                    Some((l_tail.trim(), Some(combined)))
                } else if let Some((l_tail, if_s)) = split_once_ci(raw_tail, " if ")
                    && let Some(ic) = parse_static_condition(if_s.trim(), ctx)
                {
                    let combined = match condition.as_ref() {
                        Some(outer) => Condition::And {
                            of: vec![outer.clone(), ic],
                        },
                        None => ic,
                    };
                    Some((l_tail.trim(), Some(combined)))
                } else {
                    None
                };
                if let Some((tail, eff_cond)) = split_cond
                    && let Some(c) = parse_inline_combat_tail(tail, &aff, eff_cond.as_ref(), ctx)
                {
                    return Some(vec![c]);
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    if contains_ci(t, "activate ") && contains_ci(t, "as though") && contains_ci(t, "haste") {
        let f = if let Some((_, after_of)) = split_once_ci(t, "activated abilities of ")
            .or_else(|| split_once_ci(t, "abilities of "))
            && let Some((subj_s, _)) = split_once_ci(after_of, " as though")
        {
            let mut parsed =
                parse_object_with_qualifiers(subj_s.trim(), ctx, FilterOpts::default())?;
            parsed.zones = ZoneMask::empty();
            parsed
        } else {
            ObjectFilter::default()
        };
        return Some(vec![make_static(
            StaticBody::ActivateAsThoughHaste {
                filter: Box::new(f),
            },
            condition,
        )]);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_restriction_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some(pred) = strip_self_subject(t) {
        if eq_ci(pred, "can't be countered") {
            return Some(make_static(
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::CantBeCountered,
                },
                condition,
            ));
        }
        if eq_ci(pred, "can't be copied") {
            return Some(make_static(
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::CantBeCopied,
                },
                condition,
            ));
        }
        if eq_ci(pred, "doesn't untap during your untap step")
            || eq_ci(pred, "doesn't untap during its controller's untap step")
        {
            return Some(make_static(
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::DoesntUntap,
                },
                condition,
            ));
        }
    }

    if eq_ci(t, "your opponents can't cast spells during your turn")
        || eq_ci(t, "opponents can't cast spells during your turn")
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::OpponentsCantCastDuringYourTurn { spells_only: true },
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        ));
    }
    if contains_ci(t, "during your turn")
        && contains_ci(t, "opponents can't cast spells")
        && contains_ci(t, "activate abilities")
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::OpponentsCantCastDuringYourTurn { spells_only: false },
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "each player can't cast more than one spell each turn")
        || eq_ci(t, "players can't cast more than one spell each turn")
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::OneSpellPerTurn,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }
    if eq_ci(
        t,
        "each player who has cast a nonartifact spell this turn can't cast additional nonartifact spells",
    ) {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::FilteredSpellCapPerTurn {
                    filter: Some(Box::new(ObjectFilter {
                        types_none: CardTypeMask::ARTIFACT,
                        ..Default::default()
                    })),
                    max: 1,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "your opponents can't gain life") || eq_ci(t, "opponents can't gain life") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantGainLife,
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "players can't gain life") {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantGainLife,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "players can't search libraries")
        || eq_ci(t, "opponents can't search libraries")
        || eq_ci(
            t,
            "spells and abilities your opponents control can't cause their controller to search their library",
        )
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantSearchLibraries { top_n: None },
                who: PlayerFilter {
                    relation: if contains_ci(t, "opponents") {
                        PlayerRelation::EachOpponent
                    } else {
                        PlayerRelation::EachPlayer
                    },
                },
            },
            condition,
        ));
    }
    if let Some(after_if) = strip_prefix_ci(
        t,
        "if an opponent would search a library, that player searches the top ",
    ) && let Some((num_s, rest_s)) = split_once_ci(after_if, " cards of that library instead")
        && rest_s.trim().is_empty()
        && let Some(n) = parse_num_word_or_int(num_s.trim())
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantSearchLibraries {
                    top_n: Some(n.max(0) as u16),
                },
                who: PlayerFilter {
                    relation: PlayerRelation::EachOpponent,
                },
            },
            condition,
        ));
    }
    if eq_ci(t, "each player can't draw more than one card each turn")
        || eq_ci(t, "your opponents can't draw more than one card each turn")
        || eq_ci(t, "each opponent can't draw more than one card each turn")
    {
        return Some(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::CantDrawExtraCards,
                who: PlayerFilter {
                    relation: if contains_ci(t, "opponents") {
                        PlayerRelation::Opponent
                    } else if contains_ci(t, "opponent") {
                        PlayerRelation::EachOpponent
                    } else {
                        PlayerRelation::EachPlayer
                    },
                },
            },
            condition,
        ));
    }

    if let Some(subj) = strip_suffix_ci(t, " can't be countered") {
        if let Some(mut f) = parse_object_with_qualifiers(subj, ctx, FilterOpts::default()) {
            f.zones = ZoneMask::empty();
            return Some(make_static(
                StaticBody::Restriction {
                    affected: Affected::Filter {
                        filter: Box::new(f),
                    },
                    restriction: RestrictionKind::CantBeCountered,
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(rest) = strip_prefix_ci(t, "you can't cast ")
        && let Some((obj_s, cond_s)) = split_once_ci(rest, " unless ")
        && is_self_phrase(obj_s.trim())
        && let Some(cond) = parse_static_condition(cond_s, ctx)
    {
        return Some(make_static(
            StaticBody::SelfCastRestriction {
                restrictions: Vec::new(),
                cast_condition: Some(Box::new(cond)),
                x_cant_be_zero: false,
            },
            condition,
        ));
    }

    if let Some(subj) = strip_suffix_ci(t, " can't be sacrificed") {
        if is_self_phrase(subj.trim()) {
            return Some(make_static(
                StaticBody::Restriction {
                    affected: Affected::SelfObj,
                    restriction: RestrictionKind::CantBeSacrificed,
                },
                condition,
            ));
        }
        if let Some(aff) = parse_affected(subj.trim(), ctx) {
            return Some(make_static(
                StaticBody::Restriction {
                    affected: aff,
                    restriction: RestrictionKind::CantBeSacrificed,
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(rest) = strip_prefix_ci(t, "cast ")
        && let Some((obj_s, only_s)) = split_once_ci(rest, " only ")
        && is_self_phrase(obj_s.trim())
    {
        let kind = if contains_ci(only_s, "during combat") {
            Some(SpellCastRestrictionKind::DuringCombat)
        } else if contains_ci(only_s, "during your turn") {
            Some(SpellCastRestrictionKind::DuringYourTurn)
        } else if contains_ci(only_s, "during an opponent's turn")
            || contains_ci(only_s, "during opponents' turns")
        {
            Some(SpellCastRestrictionKind::DuringOpponentsTurn)
        } else {
            None
        };
        let cond = if let Some(cond_s) = strip_prefix_ci(only_s.trim(), "if ") {
            parse_static_condition(cond_s, ctx)
        } else {
            None
        };
        if kind.is_some() || cond.is_some() {
            return Some(make_static(
                StaticBody::SelfCastRestriction {
                    restrictions: kind.into_iter().collect(),
                    cast_condition: cond.map(Box::new),
                    x_cant_be_zero: false,
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some((subj_s, src_raw)) = split_once_ci(t, " can't be the target of ")
        .or_else(|| split_once_ci(t, " can't be targeted by "))
    {
        if let Some(aff) = parse_affected(subj_s.trim(), ctx) {
            let opponent_controller_only = contains_ci(src_raw, "opponents control")
                || contains_ci(src_raw, "an opponent controls");
            let has_spells = contains_ci(src_raw, "spells") || contains_ci(src_raw, "spell");
            let has_abilities =
                contains_ci(src_raw, "abilities") || contains_ci(src_raw, "ability");
            let spells_only = has_spells && !has_abilities;
            let abilities_only = has_abilities && !has_spells;

            let first_clause = split_once_ci(src_raw, " or abilities")
                .map(|(l, _)| l.trim())
                .unwrap_or(src_raw.trim());
            let clean_src = strip_suffix_ci(first_clause, " your opponents control")
                .or_else(|| strip_suffix_ci(first_clause, " an opponent controls"))
                .unwrap_or(first_clause)
                .trim();
            let clean_src = strip_suffix_ci(clean_src, " spells or abilities")
                .or_else(|| strip_suffix_ci(clean_src, " spells"))
                .or_else(|| strip_suffix_ci(clean_src, " spell"))
                .or_else(|| strip_suffix_ci(clean_src, " abilities"))
                .or_else(|| strip_suffix_ci(clean_src, " ability"))
                .or_else(|| strip_suffix_ci(clean_src, " sources"))
                .unwrap_or(clean_src)
                .trim();

            let src_filter = if clean_src.is_empty() {
                None
            } else {
                let query = if let Some(non_col) = strip_prefix_ci(clean_src, "non")
                    && let Some(c) = parse_color_word(non_col)
                {
                    return Some(make_static(
                        StaticBody::CantBeTargetedBy {
                            affected: aff,
                            source_filter: Some(Box::new(ObjectFilter {
                                colors_none: c,
                                ..Default::default()
                            })),
                            spells_only,
                            abilities_only,
                            opponent_controller_only,
                        },
                        condition,
                    ));
                } else {
                    format!("{clean_src} cards")
                };
                parse_object_with_qualifiers(&query, ctx, FilterOpts::default()).map(|mut f| {
                    f.zones = ZoneMask::empty();
                    Box::new(f)
                })
            };

            if src_filter.is_some() || clean_src.is_empty() {
                return Some(make_static(
                    StaticBody::CantBeTargetedBy {
                        affected: aff,
                        source_filter: src_filter,
                        spells_only,
                        abilities_only,
                        opponent_controller_only,
                    },
                    condition,
                ));
            }
        }
        ctx.interner.rollback(cp);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_keyword_removal_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    let (subj_s, rem_s) = if let Some((l, r)) = split_once_ci(t, " loses ") {
        (l.trim(), r.trim())
    } else if let Some((l, r)) = split_once_ci(t, " lose ") {
        (l.trim(), r.trim())
    } else {
        return None;
    };

    let aff = match parse_affected(subj_s, ctx) {
        Some(a) => a,
        None => {
            ctx.interner.rollback(cp);
            return None;
        }
    };

    if eq_ci(rem_s, "all abilities") || eq_ci(rem_s, "all other abilities") {
        return Some(vec![make_static(
            wrap_layer_mod(
                Layer::Layer6,
                aff,
                LayerMod::RemoveAllAbilities {
                    keep_mana_abilities: false,
                },
            ),
            condition,
        )]);
    }

    if let Some((lost_s, cant_gain_s)) = split_once_ci(rem_s, " and can't have or gain ")
        .or_else(|| split_once_ci(rem_s, " and cannot have or gain "))
        && let Some(kws) = parse_keyword_id_list(lost_s.trim(), ctx)
        && let Some(kws2) = parse_keyword_id_list(cant_gain_s.trim(), ctx)
        && kws == kws2
    {
        return Some(vec![make_static(
            wrap_layer_mod(
                Layer::Layer6,
                aff,
                LayerMod::RemoveKeywords {
                    keywords: kws,
                    cant_have_or_gain: true,
                },
            ),
            condition,
        )]);
    }

    if let Some(tail) = strip_prefix_ci(rem_s, "all other abilities and ")
        .or_else(|| strip_prefix_ci(rem_s, "all abilities and "))
        && let Some(extra) = parse_inline_combat_tail(tail, &aff, condition.as_ref(), ctx)
    {
        return Some(vec![
            make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    aff,
                    LayerMod::RemoveAllAbilities {
                        keep_mana_abilities: false,
                    },
                ),
                condition,
            ),
            extra,
        ]);
    }

    if let Some(kws) = parse_keyword_id_list(rem_s, ctx) {
        return Some(vec![make_static(
            wrap_layer_mod(
                Layer::Layer6,
                aff,
                LayerMod::RemoveKeywords {
                    keywords: kws,
                    cant_have_or_gain: false,
                },
            ),
            condition,
        )]);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_grant_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticDef>> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some((subj_s, rhs)) = split_once_ci(t, " gets ").or_else(|| split_once_ci(t, " get ")) {
        if let Some(aff) = parse_affected(subj_s, ctx)
            && let Some((mut p_amt, mut t_amt, tail)) = split_pt_modifier_and_tail(rhs)
        {
            let mut extra_statics = Vec::new();
            let mut cond_out = condition;
            if let Some(mut tail_s) = tail {
                if let Some(pos) = find_outside_quotes_ci(tail_s, " as long as ") {
                    let prefix = tail_s[..pos].trim();
                    let cond_s = tail_s[pos + " as long as ".len()..].trim();
                    if !prefix.is_empty()
                        && let Some(inner_c) =
                            parse_static_condition_for_affected(cond_s, &aff, ctx)
                    {
                        cond_out = Some(match cond_out {
                            Some(outer) => Condition::And {
                                of: vec![outer, inner_c],
                            },
                            None => inner_c,
                        });
                        tail_s = prefix;
                    }
                }
                if let Some(each_raw) = strip_prefix_ci(tail_s, "for each ") {
                    let mut each_s = each_raw;
                    let mut trailing_tail: Option<&str> = None;
                    for kw_sep in [
                        " and has ",
                        " and have ",
                        " and gains ",
                        " and gain ",
                        " and loses ",
                        " and lose ",
                        " and can't ",
                        " and can ",
                        " and is ",
                        " and are ",
                    ] {
                        if let Some(pos) = find_outside_quotes_ci(each_raw, kw_sep) {
                            each_s = each_raw[..pos].trim();
                            trailing_tail = Some(each_raw[pos + 5..].trim());
                            break;
                        }
                    }
                    let cnt_opt = if eq_ci(each_s, "time it has attacked this turn") {
                        Some(Amount::TimesAttackedThisTurn {
                            of: Some(mtg_ir::Ref::Affected),
                        })
                    } else if eq_ci(each_s, "time ~ has attacked this turn") {
                        Some(Amount::TimesAttackedThisTurn {
                            of: Some(mtg_ir::Ref::SelfObj),
                        })
                    } else {
                        parse_count_amount(&format!("the number of {each_s}"), ctx)
                    };
                    if let Some(cnt) = cnt_opt {
                        if let Amount::Const { n } = p_amt {
                            p_amt = scale_amount(cnt.clone(), n);
                        }
                        if let Amount::Const { n } = t_amt {
                            t_amt = scale_amount(cnt, n);
                        }
                        if let Some(tr) = trailing_tail {
                            if let Some(more) =
                                parse_inline_static_tail(tr, &aff, cond_out.as_ref(), ctx)
                            {
                                extra_statics = more;
                            } else {
                                ctx.interner.rollback(cp);
                                return None;
                            }
                        }
                    } else {
                        ctx.interner.rollback(cp);
                        return None;
                    }
                } else if let Some(where_x) = strip_prefix_ci(tail_s, "where X is ") {
                    if let Some(cnt) = parse_count_amount(where_x, ctx) {
                        fn subst_x(a: Amount, x_val: &Amount) -> Amount {
                            match a {
                                Amount::X => x_val.clone(),
                                Amount::Negate { of } => Amount::Negate {
                                    of: Box::new(subst_x(*of, x_val)),
                                },
                                Amount::Mul { factor, of } => Amount::Mul {
                                    factor,
                                    of: Box::new(subst_x(*of, x_val)),
                                },
                                Amount::Half { of, round } => Amount::Half {
                                    of: Box::new(subst_x(*of, x_val)),
                                    round,
                                },
                                Amount::Sum { args } => Amount::Sum {
                                    args: args.into_iter().map(|s| subst_x(s, x_val)).collect(),
                                },
                                Amount::Min { args } => Amount::Min {
                                    args: args.into_iter().map(|s| subst_x(s, x_val)).collect(),
                                },
                                Amount::Max { args } => Amount::Max {
                                    args: args.into_iter().map(|s| subst_x(s, x_val)).collect(),
                                },
                                other => other,
                            }
                        }
                        p_amt = subst_x(p_amt, &cnt);
                        t_amt = subst_x(t_amt, &cnt);
                    } else {
                        ctx.interner.rollback(cp);
                        return None;
                    }
                } else if let Some(cond_s) = strip_prefix_ci(tail_s, "as long as ")
                    && let Some(inner_c) = parse_static_condition_for_affected(cond_s, &aff, ctx)
                {
                    cond_out = Some(match cond_out {
                        Some(outer) => Condition::And {
                            of: vec![outer, inner_c],
                        },
                        None => inner_c,
                    });
                } else if let Some(more) =
                    parse_inline_static_tail(tail_s, &aff, cond_out.as_ref(), ctx)
                {
                    extra_statics = more;
                } else {
                    ctx.interner.rollback(cp);
                    return None;
                }
            }

            let mut out = vec![make_static(
                wrap_layer_mod(
                    Layer::Layer7c,
                    aff,
                    LayerMod::Pt {
                        power: Box::new(p_amt),
                        toughness: Box::new(t_amt),
                    },
                ),
                cond_out,
            )];
            out.extend(extra_statics);
            return Some(out);
        }
        ctx.interner.rollback(cp);
    }

    if let Some((subj_s, rhs)) = split_once_ci(t, " has ")
        .or_else(|| split_once_ci(t, " have "))
        .or_else(|| split_once_ci(t, " gains "))
        .or_else(|| split_once_ci(t, " gain "))
    {
        if let Some(aff) = parse_affected(subj_s, ctx)
            && let Some(statics) = parse_inline_static_tail(rhs, &aff, condition.as_ref(), ctx)
        {
            return Some(statics);
        }
        ctx.interner.rollback(cp);
    }

    for (sep, prefix) in [
        (" can attack ", "can attack "),
        (" can't ", "can't "),
        (" loses ", "loses "),
        (" lose ", "lose "),
        (" isn't ", "isn't "),
        (" aren't ", "aren't "),
        (" is ", "is "),
        (" are ", "are "),
    ] {
        if let Some((subj_s, rhs)) = split_once_ci(t, sep) {
            if let Some(aff) = parse_affected(subj_s, ctx) {
                let full_pred = format!("{prefix}{rhs}");
                if let Some(statics) =
                    parse_inline_static_tail(&full_pred, &aff, condition.as_ref(), ctx)
                {
                    return Some(statics);
                }
            }
            ctx.interner.rollback(cp);
        }
    }

    ctx.interner.rollback(cp);
    None
}

// ============================================================================
// Section 8: Wave 9 Special Statics & Top-Level Static/Replacement Pipeline
// ============================================================================

fn try_parse_wave9_whole_static(text: &str, _ctx: &mut ParseCtx<'_>) -> Option<Vec<StaticItem>> {
    if !(contains_ci(text, "land or nonland")
        || contains_ci(text, "library has no cards")
        || contains_ci(text, "extra turn")
        || contains_ci(text, "hexproof"))
    {
        return None;
    }
    let clean = normalize_and_strip(text);
    let t = trim_sentence(&clean);

    if contains_ci(
        t,
        "if you would draw a card, you may instead choose land or nonland",
    ) {
        return Some(vec![StaticItem::Replacement(make_replacement(
            make_simple_event_pattern(EventKind::DrawReplacement),
            ReplacementWith::AbundanceDrawChoice,
            false,
            Some(PlayerFilter {
                relation: PlayerRelation::You,
            }),
            None,
        ))]);
    }

    if contains_ci(
        t,
        "if you would draw a card while your library has no cards in it",
    ) {
        let mut pat = make_simple_event_pattern(EventKind::DrawReplacement);
        pat.filter.player = Some(PlayerFilter {
            relation: PlayerRelation::You,
        });
        let cond = Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Library,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: Box::new(Amount::Const { n: 0 }),
        });
        if contains_ci(t, "you win the game instead") {
            let win_step = EffectStep {
                op: EffectOp::Win { condition: None },
                subject: Selector::Player {
                    filter: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                actor: None,
                amount: None,
                optional: None,
                gate: None,
                bind_as: None,
                do_this_once_per_turn: false,
                reflexive_trigger: None,
                span: Span::default(),
            };
            return Some(vec![StaticItem::Replacement(make_replacement(
                pat,
                ReplacementWith::Execute {
                    steps: vec![win_step],
                },
                false,
                None,
                cond,
            ))]);
        }
        return Some(vec![StaticItem::Replacement(make_replacement(
            pat,
            ReplacementWith::EmptyLibraryGraveyardReanimate {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }),
                return_other_cards_to_hand: false,
            },
            false,
            None,
            cond,
        ))]);
    }

    if contains_ci(
        t,
        "if a player would begin an extra turn, that player skips that turn instead",
    ) {
        return Some(vec![StaticItem::Static(make_static(
            StaticBody::RuleModifier {
                rule: RuleMod::SkipExtraTurns,
                who: PlayerFilter {
                    relation: PlayerRelation::EachPlayer,
                },
            },
            None,
        ))]);
    }

    if contains_ci(t, "as though they didn't have hexproof") && contains_ci(t, "ward abilities") {
        return Some(vec![
            StaticItem::Static(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::IgnoreHexproof,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                None,
            )),
            StaticItem::Static(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::SuppressWardTriggers,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                },
                None,
            )),
        ]);
    }

    None
}

fn parse_static_token_with_keywords(phrase: &str, ctx: &mut ParseCtx<'_>) -> Option<TokenSpec> {
    let t = trim_clause_punct(phrase);
    if let Some((head, kws_s)) =
        split_once_ci(t, " tokens with ").or_else(|| split_once_ci(t, " token with "))
    {
        let mut spec = parse_static_extra_token(&format!("{} token", head.trim()), ctx)?;
        let kws = parse_keyword_id_list(kws_s.trim(), ctx)?;
        spec.keywords.extend(kws);
        return Some(spec);
    }
    parse_static_extra_token(t, ctx)
}

fn parse_grant_abilities_from_objects_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();
    let (subj_s, tail_raw) =
        split_once_ci(t, " has all ").or_else(|| split_once_ci(t, " have all "))?;
    let mut aff = parse_affected(subj_s.trim(), ctx)?;
    if let Affected::Filter { filter } = &mut aff
        && filter.zones.is_empty()
    {
        filter.zones = ZoneMask::BATTLEFIELD;
    }

    let (loyalty_only, after_kind) =
        if let Some(r) = strip_prefix_ci(tail_raw.trim(), "activated abilities of ") {
            (false, r.trim())
        } else if let Some(r) = strip_prefix_ci(tail_raw.trim(), "loyalty abilities of ") {
            (true, r.trim())
        } else {
            ctx.interner.rollback(cp);
            return None;
        };

    let src_clean = strip_prefix_ci(after_kind, "all ")
        .unwrap_or(after_kind)
        .trim();

    for suf in [
        " exiled with ~",
        " exiled with this creature",
        " exiled with this artifact",
        " exiled with this enchantment",
        " exiled with it",
    ] {
        if let Some(card_s) = strip_suffix_ci(src_clean, suf) {
            let card_trim = card_s.trim();
            let mut card_f = if eq_ci(card_trim, "cards") || eq_ci(card_trim, "card") {
                ObjectFilter::default()
            } else {
                parse_object_with_qualifiers(
                    card_trim,
                    ctx,
                    FilterOpts {
                        strip_trailing_card: true,
                        ..FilterOpts::default()
                    },
                )?
            };
            card_f.zones = ZoneMask::empty();
            return Some(make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    aff,
                    LayerMod::GrantAbilitiesFromObjects {
                        from: AbilityGrantSource::ExiledWithSelf {
                            filter: Box::new(card_f),
                        },
                        mana_as_any_color: false,
                        once_each_turn: false,
                        loyalty_only,
                    },
                ),
                condition,
            ));
        }
    }

    let perm_clean = strip_suffix_ci(src_clean, " on the battlefield")
        .unwrap_or(src_clean)
        .trim();
    if let Some(mut perm_f) = parse_object_with_qualifiers(perm_clean, ctx, FilterOpts::default()) {
        if perm_f.zones.is_empty() {
            perm_f.zones = ZoneMask::BATTLEFIELD;
        }
        return Some(make_static(
            wrap_layer_mod(
                Layer::Layer6,
                aff,
                LayerMod::GrantAbilitiesFromObjects {
                    from: AbilityGrantSource::Battlefield {
                        filter: Box::new(perm_f),
                    },
                    mana_as_any_color: false,
                    once_each_turn: false,
                    loyalty_only,
                },
            ),
            condition,
        ));
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_token_replacement_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let t = trim_sentence(raw);
    let cp = ctx.interner.checkpoint();

    if let Some(after_if) = strip_prefix_ci(t, "if you would create ")
        && let Some((match_s, rep_raw)) = split_once_ci(after_if, ", create ")
        && let Some(rep_tok_s) = strip_suffix_ci(rep_raw.trim(), " instead")
    {
        let match_t = strip_leading_article(match_s.trim());
        if let Some(sub_s) =
            strip_suffix_ci(match_t, " token").or_else(|| strip_suffix_ci(match_t, " tokens"))
            && let Some(sub_clean) = singular_subtype(sub_s.trim()).or_else(|| {
                is_known_card_type_word(sub_s.trim())
                    .is_empty()
                    .then(|| sub_s.trim().to_string())
            })
            && let Some(spec) = parse_static_token_with_keywords(rep_tok_s.trim(), ctx)
        {
            let sub_id = ctx.interner.intern_subtype(&sub_clean);
            return Some(make_static(
                StaticBody::TokenReplacement {
                    one_of_each: false,
                    match_subtypes: smallvec![sub_id],
                    replacement_token: Some(Box::new(spec)),
                },
                condition,
            ));
        }
        ctx.interner.rollback(cp);
    }

    if let Some(after_if) = strip_prefix_ci(
        t,
        "if one or more creature tokens would be created under your control, ",
    ) && let Some(rep_tok_s) = strip_suffix_ci(after_if.trim(), " are created instead")
        .or_else(|| strip_suffix_ci(after_if.trim(), " is created instead"))
        && let Some(spec) = parse_static_token_with_keywords(rep_tok_s.trim(), ctx)
    {
        return Some(make_static(
            StaticBody::TokenReplacement {
                one_of_each: false,
                match_subtypes: SmallVec::new(),
                replacement_token: Some(Box::new(spec)),
            },
            condition,
        ));
    }

    ctx.interner.rollback(cp);
    None
}

fn try_parse_wave9_sentence(
    raw: &str,
    condition: Option<Condition>,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<StaticItem>> {
    let t = trim_sentence(raw);

    if contains_ci(t, " abilities of ")
        && (contains_ci(t, " has all ") || contains_ci(t, " have all "))
        && let Some(st) = parse_grant_abilities_from_objects_sentence(t, condition.clone(), ctx)
    {
        return Some(vec![StaticItem::Static(st)]);
    }

    if contains_ci(t, "token")
        && contains_ci(t, "instead")
        && let Some(st) = parse_token_replacement_sentence(t, condition.clone(), ctx)
    {
        return Some(vec![StaticItem::Static(st)]);
    }

    let copy_stats_core =
        strip_prefix_ci(t, "as long as a card exiled with ~ is a creature card, ")
            .or_else(|| {
                strip_prefix_ci(
                    t,
                    "as long as a card exiled with this creature is a creature card, ",
                )
            })
            .unwrap_or(t)
            .trim();
    if let Some((subj_s, tail_s)) = split_once_ci(
        copy_stats_core,
        " has the power, toughness, and creature types of the last ",
    ) && is_self_phrase(subj_s.trim())
        && (contains_ci(tail_s, "exiled with it") || contains_ci(tail_s, "exiled with ~"))
    {
        return Some(vec![
            StaticItem::Static(make_static(
                wrap_layer_mod(
                    Layer::Layer4,
                    Affected::SelfObj,
                    LayerMod::CopyStatsAndSubtypesFromLastExiled {
                        include_subtypes: true,
                        keep_self_name: false,
                    },
                ),
                None,
            )),
            StaticItem::Static(make_static(
                wrap_layer_mod(
                    Layer::Layer7b,
                    Affected::SelfObj,
                    LayerMod::CopyStatsAndSubtypesFromLastExiled {
                        include_subtypes: false,
                        keep_self_name: false,
                    },
                ),
                None,
            )),
        ]);
    }

    let _ = condition;
    None
}

fn might_be_static_or_replacement(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    let has_quote = t.as_bytes().contains(&b'"') || t.contains('\u{201C}');
    if !has_quote {
        if (starts_with_ci(t, "when ")
            || starts_with_ci(t, "whenever ")
            || starts_with_ci(t, "at the beginning of "))
            && !contains_ci(t, "would ")
        {
            return false;
        }
        if let Some(colon_idx) = t.as_bytes().iter().position(|&b| b == b':') {
            let before = &t[..colon_idx];
            if !before.is_empty() && !contains_ci(before, "choose one") {
                return false;
            }
        }
    }
    true
}

fn parse_sentence(raw: &str, ctx: &mut ParseCtx<'_>) -> Option<Vec<StaticItem>> {
    let clean = normalize_and_strip(raw);
    parse_sentence_cleaned(&clean, ctx)
}

fn parse_sentence_cleaned(clean: &str, ctx: &mut ParseCtx<'_>) -> Option<Vec<StaticItem>> {
    let trimmed = strip_leading_ability_word(clean).trim();
    let t = trim_sentence(trimmed);
    if t.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();

    let mut active_zone: Option<Zone> = None;
    let (condition, body_str) = if let Some(after) = strip_prefix_ci(t, "as long as ") {
        if let Some((cond_s, rest_s)) = after.split_once(',') {
            let cond_trim = cond_s.trim();
            if contains_ci(cond_trim, "exiled with ")
                && contains_ci(rest_s, "has the power, toughness, and creature types")
                && let Some(items) = try_parse_wave9_sentence(t, None, ctx)
            {
                return Some(items);
            }
            let rem_cond = if let Some(r) = strip_prefix_ci(cond_trim, "~ is in your graveyard")
                .or_else(|| strip_prefix_ci(cond_trim, "this card is in your graveyard"))
                .or_else(|| strip_prefix_ci(cond_trim, "it is in your graveyard"))
            {
                active_zone = Some(Zone::Graveyard);
                Some(
                    strip_prefix_ci(r.trim(), "and ")
                        .map(str::trim)
                        .unwrap_or(r.trim()),
                )
            } else if let Some((l, _r)) = split_once_ci(cond_trim, " and ~ is in your graveyard")
                .or_else(|| split_once_ci(cond_trim, " and this card is in your graveyard"))
            {
                active_zone = Some(Zone::Graveyard);
                Some(l.trim())
            } else {
                None
            };
            if let Some(sub_cond) = rem_cond {
                if sub_cond.is_empty() {
                    (None, rest_s.trim())
                } else if let Some(c) = parse_static_condition(sub_cond, ctx) {
                    (Some(c), rest_s.trim())
                } else {
                    ctx.interner.rollback(cp);
                    active_zone = None;
                    (None, t)
                }
            } else if let Some(c) = parse_static_condition(cond_trim, ctx) {
                (Some(c), rest_s.trim())
            } else {
                ctx.interner.rollback(cp);
                (None, t)
            }
        } else {
            (None, t)
        }
    } else if let Some(after) = strip_prefix_ci(t, "while ") {
        if let Some((cond_s, rest_s)) = after.split_once(',') {
            if let Some(c) = parse_static_condition(cond_s, ctx) {
                (Some(c), rest_s.trim())
            } else {
                ctx.interner.rollback(cp);
                (None, t)
            }
        } else {
            (None, t)
        }
    } else if let Some(rest_s) = strip_prefix_ci(t, "during your turn, ") {
        if let Some(sts) = parse_rule_modifier_multi_sentence(t, None, ctx) {
            return Some(sts.into_iter().map(StaticItem::Static).collect());
        }
        if let Some(st) = parse_rule_modifier_sentence(t, None, ctx)
            .or_else(|| parse_restriction_sentence(t, None, ctx))
        {
            return Some(vec![StaticItem::Static(st)]);
        }
        let c1 = Condition::IsTurnOf {
            player: PlayerRef::You,
        };
        let rest_trimmed = rest_s.trim();
        let nested = strip_prefix_ci(rest_trimmed, "as long as ")
            .or_else(|| strip_prefix_ci(rest_trimmed, "if "))
            .and_then(|after| after.split_once(','));
        if let Some((cond2_s, body2_s)) = nested
            && let Some(c2) = parse_static_condition(cond2_s.trim(), ctx)
        {
            (Some(Condition::And { of: vec![c1, c2] }), body2_s.trim())
        } else {
            ctx.interner.rollback(cp);
            (Some(c1), rest_trimmed)
        }
    } else {
        (None, t)
    };

    if matches!(condition, Some(Condition::IsPaired))
        && let Some(tail_s) = strip_prefix_ci(body_str, "each of those creatures has ")
            .or_else(|| strip_prefix_ci(body_str, "those creatures have "))
        && let Some(grants) = parse_has_grant_tail(
            tail_s,
            &Affected::Filter {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    extra: vec![ObjectPredicate::PairedWithSource],
                    ..Default::default()
                }),
            },
            condition.as_ref(),
            ctx,
        )
    {
        return Some(grants.into_iter().map(StaticItem::Static).collect());
    }

    let has_instead = contains_ci(body_str, "instead");
    let has_would = contains_ci(body_str, "would ");
    let mut out_items: Option<Vec<StaticItem>> = None;
    if (contains_ci(body_str, "abilities of")
        || contains_ci(body_str, "token")
        || contains_ci(body_str, "exiled with"))
        && let Some(items) = try_parse_wave9_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(items);
    } else if (has_would
        || has_instead
        || contains_ci(body_str, "enter")
        || contains_ci(body_str, "escape")
        || starts_with_ci(body_str, "as ")
        || contains_ci(body_str, "copy of")
        || contains_ci(body_str, "double all damage"))
        && let Some(reps) = parse_replacement_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(reps.into_iter().map(StaticItem::Replacement).collect());
    } else if (contains_ci(body_str, "prevent") || contains_ci(body_str, "damage"))
        && let Some(sts) = parse_damage_prevention_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if (has_instead
        || contains_ci(body_str, "additional time")
        || contains_ci(body_str, "cause abilities"))
        && let Some(st) = parse_multiplier_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(vec![StaticItem::Static(st)]);
    } else if let Some(sts) = parse_rule_modifier_multi_sentence(body_str, condition.clone(), ctx) {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if let Some(st) = parse_rule_modifier_sentence(body_str, condition.clone(), ctx) {
        out_items = Some(vec![StaticItem::Static(st)]);
    } else if (contains_ci(body_str, "graveyard")
        || contains_ci(body_str, "exile")
        || contains_ci(body_str, "library")
        || contains_ci(body_str, "flash")
        || contains_ci(body_str, "cast")
        || contains_ci(body_str, "play")
        || contains_ci(body_str, "mana cost"))
        && let Some(sts) = parse_zone_play_and_visibility_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if let Some(st) = parse_cost_modifier_static_sentence(body_str, condition.clone(), ctx) {
        out_items = Some(vec![StaticItem::Static(st)]);
    } else if contains_ci(body_str, "equal to")
        && let Some(st) = parse_cda_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(vec![StaticItem::Static(st)]);
    } else if (starts_with_ci(body_str, "enchanted ") || starts_with_ci(body_str, "equipped "))
        && let Some(sts) = parse_attached_type_change_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if (contains_ci(body_str, " is ")
        || contains_ci(body_str, " are ")
        || contains_ci(body_str, " become")
        || starts_with_ci(body_str, "it's "))
        && let Some(sts) = parse_global_change_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if starts_with_ci(body_str, "you control ")
        && let Some(st) = parse_control_change_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(vec![StaticItem::Static(st)]);
    }
    let rewrite_it_to_attached = condition
        .as_ref()
        .is_some_and(condition_refers_to_attached_permanent)
        && (starts_with_ci(body_str, "it ")
            || starts_with_ci(body_str, "it's ")
            || starts_with_ci(body_str, "its "));

    let mut out_items = out_items;
    let may_be_combat_or_act = contains_ci(body_str, "untap")
        || contains_ci(body_str, "block")
        || contains_ci(body_str, "attack")
        || contains_ci(body_str, "damage")
        || contains_ci(body_str, "activat")
        || contains_ci(body_str, "ability")
        || contains_ci(body_str, "abilities")
        || contains_ci(body_str, "mana")
        || contains_ci(body_str, "crew")
        || contains_ci(body_str, "saddle")
        || contains_ci(body_str, "goad")
        || contains_ci(body_str, "landwalk")
        || contains_ci(body_str, "defender");
    if out_items.is_some() {
        // already matched above
    } else if may_be_combat_or_act
        && let Some((left, right)) =
            split_once_ci(body_str, ", and its activated abilities can't be activated").or_else(
                || split_once_ci(body_str, " and its activated abilities can't be activated"),
            )
        && (right.trim().is_empty() || eq_ci(right.trim(), "unless they're mana abilities"))
        && let Some(mut sts_left) =
            parse_combat_and_activation_rule_sentence(left.trim(), condition.clone(), ctx)
    {
        let aff_opt = sts_left.first().and_then(|st_left| match &st_left.body {
            StaticBody::CombatRule { affected, .. }
            | StaticBody::Restriction { affected, .. }
            | StaticBody::Continuous { affected, .. }
            | StaticBody::CantActivateAbilities { affected, .. }
            | StaticBody::AssignCombatDamageByToughness { affected } => Some(affected.clone()),
            _ => None,
        });
        if let Some(aff) = aff_opt {
            let allow_mana = contains_ci(right, "unless they're mana abilities");
            sts_left.push(make_static(
                StaticBody::CantActivateAbilities {
                    affected: aff,
                    allow_mana_abilities: allow_mana,
                },
                condition,
            ));
            out_items = Some(sts_left.into_iter().map(StaticItem::Static).collect());
        }
    } else if may_be_combat_or_act
        && let Some(sts) =
            parse_combat_and_activation_rule_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if (contains_ci(body_str, "can't")
        || contains_ci(body_str, "cannot")
        || starts_with_ci(body_str, "cast ")
        || contains_ci(body_str, "search")
        || contains_ci(body_str, "doesn't untap"))
        && let Some(st) = parse_restriction_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(vec![StaticItem::Static(st)]);
    } else if contains_ci(body_str, "lose")
        && let Some(sts) = parse_keyword_removal_sentence(body_str, condition.clone(), ctx)
    {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    } else if let Some(sts) = parse_grant_sentence(body_str, condition, ctx) {
        out_items = Some(sts.into_iter().map(StaticItem::Static).collect());
    }

    if let Some(mut items) = out_items {
        if rewrite_it_to_attached {
            for it in &mut items {
                if let StaticItem::Static(st) = it {
                    match &mut st.body {
                        StaticBody::Continuous { affected, .. }
                        | StaticBody::Restriction { affected, .. }
                        | StaticBody::CantActivateAbilities { affected, .. }
                        | StaticBody::CombatRule { affected, .. }
                        | StaticBody::AssignCombatDamageByToughness { affected }
                        | StaticBody::MayChooseNotToUntap { affected }
                        | StaticBody::UntapDuringOtherUntapSteps { affected }
                            if matches!(affected, Affected::SelfObj) =>
                        {
                            *affected = Affected::AttachedTo;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(az) = active_zone {
            for it in &mut items {
                if let StaticItem::Static(st) = it
                    && let StaticBody::Continuous { active_in_zone, .. } = &mut st.body
                {
                    *active_in_zone = Some(az);
                }
            }
        }
        return Some(items);
    }

    ctx.interner.rollback(cp);
    None
}

fn condition_refers_to_attached_permanent(c: &Condition) -> bool {
    match c {
        Condition::AttachedPermanentMatches { .. } => true,
        Condition::ObjectState {
            of: mtg_ir::AttachmentOwnerTarget::AttachedTo,
            ..
        }
        | Condition::MatchesFilter {
            target: Some(mtg_ir::Ref::AttachedTo),
            ..
        } => true,
        Condition::And { of } | Condition::Or { of } => {
            of.iter().any(condition_refers_to_attached_permanent)
        }
        Condition::Not { of } => condition_refers_to_attached_permanent(of),
        _ => false,
    }
}

fn assign_granted_ids(items: &mut [StaticItem]) {
    let mut grant_idx = 0usize;
    for item in items.iter_mut() {
        if let StaticItem::Static(def) = item
            && let StaticBody::Continuous {
                modifier: LayerMod::GrantAbility { ability },
                ..
            } = &mut def.body
            && let AbilityDef::Activated(ab) = ability.as_mut()
            && ab.id.is_empty()
        {
            ab.id = format!("granted_{grant_idx}");
            grant_idx += 1;
        }
    }
}

fn parse_statics_impl(text: &str, offset: u32, ctx: &mut ParseCtx<'_>) -> StaticParse {
    let mut result = StaticParse::default();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return result;
    }
    if !might_be_static_or_replacement(trimmed) {
        for sent in split_sentences(text, offset) {
            if !sent.text.trim().is_empty() {
                result.unparsed.push(sent.span);
            }
        }
        return result;
    }

    if let Some(mut whole) = try_parse_wave9_whole_static(trimmed, ctx) {
        assign_granted_ids(&mut whole);
        result.items = whole;
        return result;
    }

    if starts_with_ci(trimmed, "as ")
        && (contains_ci(trimmed, "if you don't") || contains_ci(trimmed, "enters tapped unless"))
    {
        let norm_whole = normalize_and_strip(trimmed);
        if (contains_ci(&norm_whole, ". if you don't, ")
            || contains_ci(&norm_whole, ". ~ enters tapped unless "))
            && starts_with_ci(&norm_whole, "as ")
            && let Some(reps) = parse_replacement_sentence(&norm_whole, None, ctx)
        {
            result.items = reps.into_iter().map(StaticItem::Replacement).collect();
            return result;
        }
    }

    let sentences = split_sentences(text, offset);
    if sentences.is_empty() {
        return result;
    }

    let mut i = 0usize;
    while i < sentences.len() {
        let sent = &sentences[i];
        let s_trim = sent.text.trim();
        if s_trim.is_empty() {
            i += 1;
            continue;
        }
        let s_cleaned = normalize_and_strip(s_trim);
        let s_norm = trim_sentence(&s_cleaned);

        if !result.items.is_empty() {
            // Multi-sentence fusion 1: "This effect doesn't remove ~ / Auras / Auras you control ..."
            if eq_ci(
                s_norm,
                "if you cast it any time a sorcery couldn't have been cast, the controller of the permanent it becomes sacrifices it at the beginning of the next cleanup step",
            ) && let Some(StaticItem::Static(st)) = result.items.last_mut()
            {
                let mut fused = false;
                if let StaticBody::RuleModifier {
                    rule:
                        RuleMod::CastAsThoughFlash {
                            self_only: true,
                            sacrifice_at_next_cleanup_if_non_sorcery,
                            ..
                        },
                    ..
                } = &mut st.body
                {
                    *sacrifice_at_next_cleanup_if_non_sorcery = true;
                    fused = true;
                } else if let StaticBody::SpellGrantKeyword { keywords, .. } = &st.body
                    && keywords.len() == 1
                    && ctx.interner.keyword_name(keywords[0]) == Some("flash")
                {
                    st.body = StaticBody::RuleModifier {
                        rule: RuleMod::CastAsThoughFlash {
                            spells: None,
                            self_only: true,
                            first_matching_this_turn: false,
                            sacrifice_at_next_cleanup_if_non_sorcery: true,
                        },
                        who: PlayerFilter {
                            relation: PlayerRelation::You,
                        },
                    };
                    fused = true;
                }
                if fused {
                    i += 1;
                    continue;
                }
            }
            if let Some(rest) = strip_prefix_ci(s_norm, "this effect doesn't remove ") {
                let ex = if is_self_phrase(rest.trim()) {
                    Some(ProtectionAttachmentExemption::SelfObj)
                } else if eq_ci(rest.trim(), "auras") {
                    Some(ProtectionAttachmentExemption::AllAuras)
                } else if eq_ci(rest.trim(), "auras you control") {
                    Some(ProtectionAttachmentExemption::ControlledAuras)
                } else if contains_ci(rest, "auras and equipment you control") {
                    Some(ProtectionAttachmentExemption::ControlledAttachments)
                } else {
                    None
                };
                if let Some(exemption) = ex {
                    let mut fused = false;
                    for item in result.items.iter_mut().rev() {
                        if let StaticItem::Static(st) = item
                            && let StaticBody::Continuous { modifier, .. } = &mut st.body
                        {
                            match modifier {
                                LayerMod::AddKeywords {
                                    protection_attachment_exemption,
                                    ..
                                }
                                | LayerMod::ProtectionFromChosenColor {
                                    protection_attachment_exemption,
                                } => {
                                    *protection_attachment_exemption = Some(exemption);
                                    fused = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    if fused {
                        i += 1;
                        continue;
                    }
                }
            }

            // Multi-sentence fusion 2: Phantom counter removal ("Remove a +1/+1 counter from ~.")
            if let Some(rem_rest) = strip_prefix_ci(s_norm, "remove a ")
                .or_else(|| strip_prefix_ci(s_norm, "remove an "))
                && let Some((cnt_s, on_s)) = split_once_ci(rem_rest, " from ")
                && is_self_phrase(on_s.trim())
                && let Some(cid) = lookup_static_counter(cnt_s, ctx)
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::StaticDamagePrevention {
                    phantom_counter_removal,
                    ..
                } = &mut st.body
            {
                *phantom_counter_removal = Some(PhantomCounterRemoval {
                    counter: cid,
                    remove_that_many: false,
                    require_counter_present: true,
                });
                i += 1;
                continue;
            }

            // Multi-sentence fusion 2b: Phyrexian Vindicator reflexive damage on prevented
            if (starts_with_ci(s_norm, "when damage is prevented this way, ")
                || starts_with_ci(s_norm, "whenever damage is prevented this way, "))
                && let Some((_, dmg_tail)) = split_once_ci(s_norm, " that much damage to ")
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::StaticDamagePrevention { on_prevented, .. } = &mut st.body
            {
                let toks = tokenize(dmg_tail.trim(), 0);
                let mut scope = NullScope;
                if let Some((ts, used)) = parse_target_spec(&toks, ctx, &mut scope)
                    && used == toks.len()
                {
                    *on_prevented = Some(StaticOnPrevented::ReflexiveDamageTrigger {
                        target: Box::new(ts),
                        to_amount: Some(mtg_ir::PreventedToAmount::Prevented),
                    });
                    i += 1;
                    continue;
                }
            }

            // Multi-sentence fusion 2c: Vigor +1/+1 counters per damage prevented
            if contains_ci(
                s_norm,
                "put a +1/+1 counter on that creature for each 1 damage prevented this way",
            ) && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::StaticDamagePrevention { on_prevented, .. } = &mut st.body
            {
                *on_prevented = Some(StaticOnPrevented::PutPlusCountersPerDamage);
                i += 1;
                continue;
            }

            // Multi-sentence fusion 3: ZonePlayPermission riders (Meren/Karador/Bolas/Thundermane/Edgar/Azula)
            if (contains_ci(s_norm, "cast this way would die, exile it instead")
                || contains_ci(
                    s_norm,
                    "cast this way would be put into a graveyard, exile it instead",
                ))
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::ZonePlayPermission {
                    exile_if_put_into_graveyard,
                    ..
                } = &mut st.body
            {
                *exile_if_put_into_graveyard = true;
                i += 1;
                continue;
            }
            if contains_ci(s_norm, "this way")
                && contains_ci(
                    s_norm,
                    "pay life equal to its mana value rather than pay its mana cost",
                )
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::ZonePlayPermission {
                    zone, allow_spells, ..
                } = &mut st.body
            {
                let allowed_zones = ZoneMask::from_zone(*zone);
                *allow_spells = false;
                result.items.push(StaticItem::Static(make_static(
                    StaticBody::AltCastPermission {
                        mana_cost: ManaCost {
                            raw: "{0}".to_string(),
                            ..Default::default()
                        },
                        use_printed_mana_cost: false,
                        cost_defs: Vec::new(),
                        affected: Affected::Filter {
                            filter: Box::new(ObjectFilter::default()),
                        },
                        allowed_zones,
                        once_each_turn: false,
                        during_your_turn_only: false,
                        grant_flash: false,
                        any_player: false,
                        dynamic_mana_cost_from_mv: false,
                        max_mv_by_self_counters: None,
                        max_mv_by_controlled_count: None,
                        enters_with_counters: Vec::new(),
                        additional_only: false,
                        additional_per_metric: None,
                        require_zone_permission: false,
                        pay_life_equal_to_mv: true,
                        slot_mode: None,
                    },
                    None,
                )));
                i += 1;
                continue;
            }
            if contains_ci(s_norm, "this way")
                && contains_ci(s_norm, "gains haste until end of turn")
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::ZonePlayPermission {
                    grant_haste_until_eot,
                    ..
                } = &mut st.body
            {
                *grant_haste_until_eot = true;
                i += 1;
                continue;
            }
            if contains_ci(s_norm, "this way")
                && contains_ci(s_norm, "enters tapped")
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::ZonePlayPermission { enters_tapped, .. } = &mut st.body
            {
                *enters_tapped = true;
                i += 1;
                continue;
            }
            if (eq_ci(s_norm, "mana of any type can be spent to cast those spells")
                || eq_ci(
                    s_norm,
                    "you may spend mana as though it were mana of any color to cast those spells",
                ))
                && let Some(StaticItem::Static(st)) = result.items.last_mut()
                && let StaticBody::ZonePlayPermission {
                    spend_mana_as_any_color,
                    ..
                } = &mut st.body
            {
                *spend_mana_as_any_color = true;
                i += 1;
                continue;
            }

            // Multi-sentence fusion 3b: Djinn Illuminatus replicate cost sentence
            if eq_ci(s_norm, "the replicate cost is equal to its mana cost")
                && let Some(StaticItem::Static(st)) = result.items.last()
                && matches!(&st.body, StaticBody::SpellGrantKeyword { .. })
            {
                i += 1;
                continue;
            }

            // Multi-sentence fusion 3c: Valentin, Dean of the Vein 3-sentence dies->exile + reflexive token
            if let Some(pay_s) = strip_prefix_ci(s_norm, "when you do, you may pay ")
                && i + 1 < sentences.len()
                && let Some(StaticItem::Replacement(_)) = result.items.last()
            {
                let next_clean = normalize_and_strip(sentences[i + 1].text.trim());
                let next_norm = trim_sentence(&next_clean);
                if let Some(create_raw) = strip_prefix_ci(next_norm, "if you do, create ") {
                    let cparse = parse_cost_list(pay_s.trim(), 0, ctx);
                    if let [CostDef::Mana { cost, .. }] = cparse.defs.as_slice() {
                        let (tok_phrase, quoted_text) =
                            if let Some((l, r)) = split_once_ci(create_raw, " with \"") {
                                let q = r.trim().trim_end_matches('"').trim();
                                (l.trim(), replace_granted_self_refs(q))
                            } else {
                                (create_raw.trim(), String::new())
                            };
                        if let Some(tok_spec) = parse_static_extra_token(tok_phrase, ctx)
                            && let Some(StaticItem::Replacement(rep)) = result.items.last_mut()
                            && let ReplacementWith::ZoneChange { mod_spec } = &mut rep.with
                            && mod_spec.to == Some(Zone::Exile)
                        {
                            mod_spec.dies_exile_and_reflexive =
                                Some((cost.clone(), Box::new(tok_spec), quoted_text));
                            i += 2;
                            continue;
                        }
                    }
                }
            }
        }

        // Multi-sentence fusion 3d: Urborg Scavengers CopyKeywordsFromExiled
        if let Some((left_kw, right_kw)) =
            split_once_ci(s_norm, " as long as a card exiled with it has ")
            && let Some((subj_s, kw1_s)) =
                split_once_ci(left_kw, " has ").or_else(|| split_once_ci(left_kw, " have "))
            && is_self_phrase(subj_s.trim())
            && eq_ci(kw1_s.trim(), right_kw.trim())
            && let Some(mut kws) = parse_keyword_id_list(kw1_s.trim(), ctx)
        {
            let mut extra_consumed = false;
            if i + 1 < sentences.len() {
                let next_clean = normalize_and_strip(sentences[i + 1].text.trim());
                let next_norm = trim_sentence(&next_clean);
                if let Some(rest_kws) = strip_prefix_ci(next_norm, "the same is true for ")
                    && let Some(more_kws) = parse_keyword_id_list(rest_kws.trim(), ctx)
                {
                    kws.extend(more_kws);
                    extra_consumed = true;
                }
            }
            result.items.push(StaticItem::Static(make_static(
                wrap_layer_mod(
                    Layer::Layer6,
                    Affected::SelfObj,
                    LayerMod::CopyKeywordsFromExiled {
                        keywords: kws.into_iter().collect(),
                    },
                ),
                None,
            )));
            i += if extra_consumed { 2 } else { 1 };
            continue;
        }

        // Multi-sentence fusion 3e: Duplicant "It's still a Shapeshifter."
        if result.items.len() >= 2
            && let Some(sub_raw) = strip_prefix_ci(s_norm, "it's still a ")
                .or_else(|| strip_prefix_ci(s_norm, "it's still an "))
            && let Some(StaticItem::Static(last_st)) = result.items.last()
            && matches!(
                &last_st.body,
                StaticBody::Continuous {
                    layer: Layer::Layer7b,
                    modifier: LayerMod::CopyStatsAndSubtypesFromLastExiled { .. },
                    ..
                }
            )
            && is_known_card_type_word(sub_raw.trim()).is_empty()
        {
            let sub_id = ctx.interner.intern_subtype(sub_raw.trim());
            let insert_idx = result.items.len() - 1;
            result.items.insert(
                insert_idx,
                StaticItem::Static(make_static(
                    wrap_layer_mod(
                        Layer::Layer4,
                        Affected::SelfObj,
                        LayerMod::AddTypes {
                            types: CardTypeMask::empty(),
                            subtypes: smallvec![sub_id],
                        },
                    ),
                    None,
                )),
            );
            i += 1;
            continue;
        }

        // Multi-sentence fusion 3f: Maskwood Nexus OffBattlefieldCharacteristics
        if !result.items.is_empty()
            && eq_ci(
                s_norm,
                "the same is true for creature spells you control and creature cards you own that aren't on the battlefield",
            )
            && let Some(StaticItem::Static(st)) = result.items.last()
            && matches!(
                &st.body,
                StaticBody::Continuous {
                    modifier: LayerMod::AllCreatureTypes,
                    ..
                }
            )
        {
            result.items.push(StaticItem::Static(make_static(
                StaticBody::OffBattlefieldCharacteristics {
                    zones: ZoneMask::ALL & !ZoneMask::BATTLEFIELD,
                    filter: Some(Box::new(ObjectFilter {
                        types_any: CardTypeMask::CREATURE,
                        ..Default::default()
                    })),
                    all_players: false,
                    add_all_creature_types: true,
                    add_chosen_subtype: false,
                    add_types: CardTypeMask::empty(),
                    add_subtypes: SmallVec::new(),
                    add_chosen_color: false,
                    add_colors: ColorMask::empty(),
                },
                None,
            )));
            i += 1;
            continue;
        }

        // Multi-sentence fusion 3g: Opposition Agent search-exile + play-from-exile
        if eq_ci(
            s_norm,
            "while an opponent is searching their library, they exile each card they find",
        ) {
            result.items.push(StaticItem::Static(make_static(
                StaticBody::RuleModifier {
                    rule: RuleMod::SearchControl {
                        exiles_found_cards: true,
                        grant_play_from_exile: false,
                        spend_mana_as_any_color: false,
                    },
                    who: PlayerFilter {
                        relation: PlayerRelation::EachOpponent,
                    },
                },
                None,
            )));
            if i + 1 < sentences.len() {
                let next_clean = normalize_and_strip(sentences[i + 1].text.trim());
                let next_norm = trim_sentence(&next_clean);
                if contains_ci(
                    next_norm,
                    "you may play those cards for as long as they remain exiled",
                ) && contains_ci(next_norm, "mana of any color")
                {
                    result.items.push(StaticItem::Static(make_static(
                        StaticBody::RuleModifier {
                            rule: RuleMod::SearchControl {
                                exiles_found_cards: false,
                                grant_play_from_exile: true,
                                spend_mana_as_any_color: true,
                            },
                            who: PlayerFilter {
                                relation: PlayerRelation::EachOpponent,
                            },
                        },
                        None,
                    )));
                    i += 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        // Multi-sentence fusion 4: "Otherwise, <body>" reusing negated preceding condition
        if !result.items.is_empty()
            && let Some(otherwise_rest) = strip_prefix_ci(s_norm, "otherwise, ")
        {
            let prev_cond = result.items.last().and_then(|item| match item {
                StaticItem::Static(s) => s.condition.as_deref().cloned(),
                StaticItem::Replacement(r) => r.condition.as_deref().cloned(),
            });
            if let Some(pc) = prev_cond
                && let Some(mut items) = parse_sentence_cleaned(otherwise_rest, ctx)
            {
                let neg = Box::new(negate_condition(&pc));
                for item in items.iter_mut() {
                    match item {
                        StaticItem::Static(s) if s.condition.is_none() => {
                            s.condition = Some(neg.clone());
                        }
                        StaticItem::Replacement(r) if r.condition.is_none() => {
                            r.condition = Some(neg.clone());
                        }
                        _ => {}
                    }
                }
                result.items.extend(items);
                i += 1;
                continue;
            }
        }

        if !might_be_static_or_replacement(&s_cleaned) {
            result.unparsed.push(sent.span);
            i += 1;
            continue;
        }
        if let Some(items) = parse_sentence_cleaned(&s_cleaned, ctx) {
            result.items.extend(items);
        } else {
            result.unparsed.push(sent.span);
        }
        i += 1;
    }

    assign_granted_ids(&mut result.items);
    result
}

fn parse_static_cda_pt_impl(text: &str, ctx: &mut ParseCtx<'_>) -> Option<StaticDef> {
    let clean = normalize_and_strip(text);
    let trimmed = strip_leading_ability_word(&clean).trim();
    parse_cda_sentence(trimmed, None, ctx)
}

fn parse_static_spell_restriction_or_cast_mod_impl(
    text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let clean = normalize_and_strip(text);
    let trimmed = trim_sentence(strip_leading_ability_word(&clean).trim());
    if trimmed.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();

    if eq_ci(trimmed, "x can't be 0") {
        return Some(make_static(
            StaticBody::SelfCastRestriction {
                restrictions: Vec::new(),
                cast_condition: None,
                x_cant_be_zero: true,
            },
            None,
        ));
    }

    if eq_ci(trimmed, "this spell has flash") || eq_ci(trimmed, "~ has flash") {
        let kw = ctx.interner.intern_keyword("flash");
        return Some(make_static(
            StaticBody::SpellGrantKeyword {
                filter: Box::new(ObjectFilter::default()),
                keywords: smallvec![kw],
                evoke_cost: None,
                cascade_count: None,
                param_n: None,
            },
            None,
        ));
    }

    if let Some(st) = parse_restriction_sentence(trimmed, None, ctx) {
        return Some(st);
    }
    if let Some(mut sts) = parse_zone_play_and_visibility_sentence(trimmed, None, ctx)
        && let Some(first) = sts.drain(..).next()
    {
        return Some(first);
    }
    if let Some(st) = parse_cost_modifier_static_sentence(trimmed, None, ctx) {
        return Some(st);
    }

    ctx.interner.rollback(cp);
    None
}

fn parse_zone_change_clause_for_spell_or_etb_impl(
    text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ReplacementDef> {
    let clean = normalize_and_strip(text);
    let trimmed = trim_sentence(strip_leading_ability_word(&clean).trim());
    if trimmed.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();
    if let Some(mut reps) = parse_replacement_sentence(trimmed, None, ctx)
        && let Some(first) = reps.drain(..).next()
    {
        return Some(first);
    }
    ctx.interner.rollback(cp);
    None
}

fn contains_ascii_ci_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let b0_lo = needle[0].to_ascii_lowercase();
    let b0_hi = needle[0].to_ascii_uppercase();
    let limit = haystack.len() - needle.len();
    let mut i = 0usize;
    while i <= limit {
        let b = haystack[i];
        if (b == b0_lo || b == b0_hi) && haystack[i..i + needle.len()].eq_ignore_ascii_case(needle)
        {
            return true;
        }
        i += 1;
    }
    false
}

fn parse_spell_alt_cast_or_flash_sentence_impl(
    sentence: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<StaticDef> {
    let b = sentence.as_bytes();
    if !(contains_ascii_ci_bytes(b, b"rather than pay")
        || contains_ascii_ci_bytes(b, b"as though it had flash"))
    {
        return None;
    }
    let clean = normalize_and_strip(sentence);
    let trimmed = trim_sentence(strip_leading_ability_word(&clean).trim());
    if trimmed.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();

    // 1. Twilight's Call: "You may cast (~|this spell) as though it had flash if you pay {N} more to cast it"
    if let Some(after_may) = strip_prefix_ci(trimmed, "you may cast ")
        && let Some((subj, after_flash)) =
            split_once_ci(after_may, " as though it had flash if you pay ")
        && is_self_phrase(subj.trim())
        && let Some(cost_s) = strip_suffix_ci(after_flash.trim(), " more to cast it")
            .or_else(|| strip_suffix_ci(after_flash.trim(), " more"))
    {
        let cparse = parse_cost_list(cost_s.trim(), 0, ctx);
        if cparse.unparsed.is_empty()
            && cparse.defs.len() == 1
            && let CostDef::Mana { cost, .. } = &cparse.defs[0]
        {
            return Some(make_static(
                StaticBody::AltCastPermission {
                    mana_cost: cost.clone(),
                    use_printed_mana_cost: false,
                    cost_defs: Vec::new(),
                    affected: Affected::SelfObj,
                    allowed_zones: ZoneMask::empty(),
                    once_each_turn: false,
                    during_your_turn_only: false,
                    grant_flash: true,
                    any_player: false,
                    dynamic_mana_cost_from_mv: false,
                    max_mv_by_controlled_count: None,
                    max_mv_by_self_counters: None,
                    enters_with_counters: Vec::new(),
                    additional_only: true,
                    additional_per_metric: None,
                    require_zone_permission: false,
                    pay_life_equal_to_mv: false,
                    slot_mode: None,
                },
                None,
            ));
        }
        ctx.interner.rollback(cp);
    }

    // 2. "[If <cond>, ]You may <cost> rather than pay (~|this spell)'s mana cost[ if <cond>]"
    //    or "[If <cond>, ]rather than pay (~|this spell)'s mana cost, you may <cost>"
    let mut cond_parts: Vec<Condition> = Vec::new();
    let mut body = trimmed;
    if let Some(after_if) = strip_prefix_ci(body, "if ")
        && let Some((cond_s, rest)) = split_once_ci(after_if, ", ")
        && let Some(c) = parse_static_condition(cond_s.trim(), ctx)
    {
        cond_parts.push(c);
        body = rest.trim();
    }
    if let Some((before_if, cond_s)) = split_once_ci(body, " if ")
        && let Some(c) = parse_static_condition(cond_s.trim(), ctx)
    {
        cond_parts.push(c);
        body = before_if.trim();
    }

    let cost_text_opt = if let Some(after_may) = strip_prefix_ci(body, "you may ")
        && let Some((cost_s, target_s)) = split_once_ci(after_may, " rather than pay ")
        && let Some(self_s) = strip_suffix_ci(target_s.trim(), "'s mana cost")
        && is_self_phrase(self_s.trim())
    {
        Some(cost_s.trim())
    } else if let Some(after_rather) = strip_prefix_ci(body, "rather than pay ")
        && let Some((target_s, after_comma)) = split_once_ci(after_rather, ", ")
        && let Some(self_s) = strip_suffix_ci(target_s.trim(), "'s mana cost")
        && is_self_phrase(self_s.trim())
        && let Some(cost_s) = strip_prefix_ci(after_comma.trim(), "you may ")
    {
        Some(cost_s.trim())
    } else {
        None
    };

    if let Some(cost_s) = cost_text_opt {
        let cparse = parse_cost_list(cost_s, 0, ctx);
        if cparse.unparsed.is_empty()
            && !cparse.defs.is_empty()
            && cparse.defs.iter().all(is_spell_alt_cost_compatible)
        {
            let mut mana_cost: Option<ManaCost> = None;
            let mut cost_defs = Vec::new();
            for def in cparse.defs {
                if mana_cost.is_none()
                    && let CostDef::Mana { cost, .. } = &def
                {
                    mana_cost = Some(cost.clone());
                } else {
                    cost_defs.push(def);
                }
            }
            let final_mana = mana_cost.unwrap_or_else(|| ManaCost {
                raw: "{0}".to_string(),
                ..Default::default()
            });
            let cond = match cond_parts.len() {
                0 => None,
                1 => cond_parts.into_iter().next(),
                _ => Some(Condition::And { of: cond_parts }),
            };
            return Some(make_static(
                StaticBody::AltCastPermission {
                    mana_cost: final_mana,
                    use_printed_mana_cost: false,
                    cost_defs,
                    affected: Affected::SelfObj,
                    allowed_zones: ZoneMask::empty(),
                    once_each_turn: false,
                    during_your_turn_only: false,
                    grant_flash: false,
                    any_player: false,
                    dynamic_mana_cost_from_mv: false,
                    max_mv_by_controlled_count: None,
                    max_mv_by_self_counters: None,
                    enters_with_counters: Vec::new(),
                    additional_only: false,
                    additional_per_metric: None,
                    require_zone_permission: false,
                    pay_life_equal_to_mv: false,
                    slot_mode: None,
                },
                cond,
            ));
        }
    }

    ctx.interner.rollback(cp);
    None
}
