use mtg_ir::{
    Amount, AttachedHostRef, CardTypeMask, ColorMask, Compare, CompareOp, CounterTarget,
    FilterFlags, KeywordId, ManaColor, ObjectFilter, ObjectPredicate, ParityFilter,
    PlainManaFilter, PlayerFilter, PlayerRef, PlayerRelation, Ref, RelativeControl,
    StackTargetKind, StatKind, SubtypeId, SupertypeMask, TargetKind, TargetSpec, Zone, ZoneMask,
};
use smallvec::{SmallVec, smallvec};

use crate::intern::normalize_counter_name;
use crate::lexer::{ManaSymbolKind, Punct, Token, TokenCursor, TokenKind, tokenize};
use crate::parser::ParseCtx;
use crate::parser::value::{
    is_self_noun, parse_amount, parse_compare, parse_condition, parse_small_number,
};

// CR 700.12: Assassins, Mercenaries, Pirates, Rogues, and Warlocks are outlaws.
pub const OUTLAW_SUBTYPES: [&str; 5] = ["Assassin", "Mercenary", "Pirate", "Rogue", "Warlock"];

const FILTER_KEYWORDS: [&str; 35] = [
    "flying",
    "first strike",
    "double strike",
    "deathtouch",
    "defender",
    "flash",
    "haste",
    "hexproof",
    "indestructible",
    "lifelink",
    "menace",
    "reach",
    "shroud",
    "trample",
    "vigilance",
    "ward",
    "fear",
    "intimidate",
    "shadow",
    "horsemanship",
    "skulk",
    "flanking",
    "infect",
    "wither",
    "changeling",
    "prowess",
    "toxic",
    "undying",
    "persist",
    "devoid",
    "islandwalk",
    "swampwalk",
    "mountainwalk",
    "forestwalk",
    "plainswalk",
];

const SINGULAR_S: [&str; 16] = [
    "Plains",
    "Fungus",
    "Octopus",
    "Homunculus",
    "Atog",
    "Nephilim",
    "Astartes",
    "Cyclops",
    "Sphinx",
    "Locus",
    "Pegasus",
    "Aurochs",
    "Nautilus",
    "Platypus",
    "Walrus",
    "Bolas",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct FilterOpts {
    /// ZoneMask applied ONLY if the phrase specifies no zone (e.g. `ZoneMask::empty()` vs `BATTLEFIELD`).
    pub default_zone: ZoneMask,
    /// Allow `A and B` plural subtype/type unions in static affected clauses (`"Elves and Warriors"`).
    pub allow_plural_and_disjunction: bool,
    /// Allow `"spell"` head noun stripping (for trigger/cast filters).
    pub allow_spell_noun: bool,
    /// Allow trailing `"card"` / `"cards"` to be stripped when `default_zone` is non-battlefield.
    pub strip_trailing_card: bool,
    /// Interpret `"outlaw(s)"` as `OUTLAW_SUBTYPES` instead of `FilterFlags::OUTLAW` (cost phrases).
    pub expand_outlaw_subtypes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PronounSubject {
    SelfObj,
    AttachedTo,
}

/// Abstract scope passed into `filter.rs` and `value.rs` so they work uniformly
/// for costs, triggers, statics, keywords, and imperative effects.
pub trait ParserScope {
    fn in_trigger_context(&self) -> bool {
        false
    }
    fn has_prior_step(&self) -> bool {
        false
    }
    fn trigger_event_is_exile(&self) -> bool {
        false
    }
    fn trigger_targets_self(&self) -> bool {
        false
    }
    fn trigger_matches_that_noun(&self, _noun: &str) -> bool {
        false
    }
    fn trigger_is_ltb(&self) -> bool {
        false
    }
    fn has_sac_step_or_trigger(&self) -> bool {
        false
    }
    fn has_mill_step(&self) -> bool {
        false
    }
    fn it_ref(&self) -> Option<mtg_ir::Ref> {
        None
    }
    fn has_reveal_drawn(&self) -> bool {
        false
    }
    fn last_player_ref(&self) -> Option<PlayerRef> {
        None
    }
    fn defined_x(&self) -> Option<mtg_ir::Amount> {
        None
    }
    fn default_pronoun_subject(&self) -> PronounSubject {
        PronounSubject::SelfObj
    }
    fn has_non_self_pronoun_target(&self) -> bool {
        false
    }
    fn is_each_loop_player(&self) -> bool {
        false
    }
    fn difference_is_player_counter(&self) -> Option<(PlayerRef, mtg_ir::PlayerCounterKind, i32)> {
        None
    }
    fn difference_hand_size(&self) -> Option<(Option<PlayerRef>, Option<i32>)> {
        None
    }
    fn alloc_target(&mut self, _spec: TargetSpec) -> Option<u16> {
        None
    }
    fn get_target(&self, _idx: u16) -> Option<&TargetSpec> {
        None
    }
    fn prior_targets_len(&self) -> usize {
        0
    }
    fn truncate_targets(&mut self, _len: usize) {}
    fn record_last_player(&mut self, _p: PlayerRef) {}
    fn record_last_target(&mut self, _r: mtg_ir::Ref) {}
    fn ensure_last_step_bound(&mut self) -> Option<mtg_ir::Ref> {
        None
    }
}

pub struct NullScope;
impl ParserScope for NullScope {}

struct ScopeRef<'a>(&'a dyn ParserScope);

impl ParserScope for ScopeRef<'_> {
    fn in_trigger_context(&self) -> bool {
        self.0.in_trigger_context()
    }
    fn has_prior_step(&self) -> bool {
        self.0.has_prior_step()
    }
    fn trigger_event_is_exile(&self) -> bool {
        self.0.trigger_event_is_exile()
    }
    fn trigger_targets_self(&self) -> bool {
        self.0.trigger_targets_self()
    }
    fn trigger_matches_that_noun(&self, noun: &str) -> bool {
        self.0.trigger_matches_that_noun(noun)
    }
    fn trigger_is_ltb(&self) -> bool {
        self.0.trigger_is_ltb()
    }
    fn has_sac_step_or_trigger(&self) -> bool {
        self.0.has_sac_step_or_trigger()
    }
    fn it_ref(&self) -> Option<mtg_ir::Ref> {
        self.0.it_ref()
    }
    fn has_reveal_drawn(&self) -> bool {
        self.0.has_reveal_drawn()
    }
    fn last_player_ref(&self) -> Option<PlayerRef> {
        self.0.last_player_ref()
    }
    fn defined_x(&self) -> Option<mtg_ir::Amount> {
        self.0.defined_x()
    }
    fn default_pronoun_subject(&self) -> PronounSubject {
        self.0.default_pronoun_subject()
    }
    fn has_non_self_pronoun_target(&self) -> bool {
        self.0.has_non_self_pronoun_target()
    }
    fn is_each_loop_player(&self) -> bool {
        self.0.is_each_loop_player()
    }
    fn difference_is_player_counter(&self) -> Option<(PlayerRef, mtg_ir::PlayerCounterKind, i32)> {
        self.0.difference_is_player_counter()
    }
    fn difference_hand_size(&self) -> Option<(Option<PlayerRef>, Option<i32>)> {
        self.0.difference_hand_size()
    }
    fn get_target(&self, idx: u16) -> Option<&TargetSpec> {
        self.0.get_target(idx)
    }
    fn prior_targets_len(&self) -> usize {
        self.0.prior_targets_len()
    }
}

/// Zero-allocation helper primitives over [`TokenCursor`].
pub(crate) trait CursorExt<'a> {
    fn peek_ci(&self, word: &str) -> bool;
    fn peek_n_ci(&self, offset: usize, word: &str) -> bool;
    fn eat_words(&mut self, words: &[&str]) -> bool;
    fn eat_hyphenated(&mut self, left: &str, right: &str) -> bool;
    fn eat_and_or(&mut self) -> bool;
    fn eat_comma_or_and_or(&mut self) -> bool;
    fn eat_possessive_s(&mut self) -> bool;
    fn try_parse<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T>;
}

impl<'a> CursorExt<'a> for TokenCursor<'a> {
    #[inline]
    fn peek_ci(&self, word: &str) -> bool {
        self.peek().is_some_and(|t| t.eq_ci(word))
    }

    #[inline]
    fn peek_n_ci(&self, offset: usize, word: &str) -> bool {
        self.peek_n(offset).is_some_and(|t| t.eq_ci(word))
    }

    fn eat_words(&mut self, words: &[&str]) -> bool {
        let saved = self.save();
        for w in words {
            if !self.eat_ci(w) {
                self.restore(saved);
                return false;
            }
        }
        true
    }

    fn eat_hyphenated(&mut self, left: &str, right: &str) -> bool {
        let saved = self.save();
        if self.eat_ci(left) && self.eat_punct(Punct::Hyphen) && self.eat_ci(right) {
            true
        } else {
            self.restore(saved);
            false
        }
    }

    fn eat_and_or(&mut self) -> bool {
        if self.eat_ci("or") {
            return true;
        }
        let saved = self.save();
        if self.eat_ci("and") && self.eat_punct(Punct::Slash) && self.eat_ci("or") {
            true
        } else {
            self.restore(saved);
            false
        }
    }

    /// Consumes `,`, `, or`, `, and/or`, bare `or`, and bare `and/or` (never bare `and`).
    /// Callers that must reject `or` / `and/or` (e.g. `with <kw>`) should NOT call this.
    fn eat_comma_or_and_or(&mut self) -> bool {
        let saved = self.save();
        let had_comma = self.eat_punct(Punct::Comma);
        if self.eat_and_or() {
            return true;
        }
        if had_comma {
            return true;
        }
        self.restore(saved);
        false
    }

    fn eat_possessive_s(&mut self) -> bool {
        if let Some(tok) = self.peek()
            && (tok.raw == "'s" || tok.raw == "’s" || tok.raw == "'")
        {
            self.bump();
            return true;
        }
        false
    }

    fn try_parse<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let saved = self.save();
        let res = f(self);
        if res.is_none() {
            self.restore(saved);
        }
        res
    }
}

#[inline]
#[allow(dead_code)]
pub(crate) fn slice_tokens_text<'a>(tokens: &[Token<'a>], normalized_text: &'a str) -> &'a str {
    match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => normalized_text
            .get(first.span.start as usize..last.span.end as usize)
            .unwrap_or(""),
        _ => "",
    }
}

/// Reconstructs surface text from `tokens` using token `span` gaps (`tok.span.start > prev.span.end`),
/// independent of `ctx.normalized_text` offsets or multi-line UTF-8 content.
pub(crate) fn reconstruct_tokens_text(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    let mut prev_end: Option<u32> = None;
    for tok in tokens {
        if let Some(pe) = prev_end
            && tok.span.start > pe
        {
            out.push(' ');
        }
        out.push_str(tok.raw);
        prev_end = Some(tok.span.end);
    }
    out
}

#[inline]
pub fn is_known_card_type_word(w: &str) -> CardTypeMask {
    let low = w.to_ascii_lowercase();
    let s = match low.as_str() {
        "artifacts" => "artifact",
        "battles" => "battle",
        "creatures" => "creature",
        "enchantments" => "enchantment",
        "instants" => "instant",
        "lands" => "land",
        "planeswalkers" => "planeswalker",
        "sorceries" => "sorcery",
        "permanents" => "permanent",
        other => other,
    };
    CardTypeMask::parse_name(s)
}

#[inline]
pub fn is_known_supertype_word(w: &str) -> SupertypeMask {
    SupertypeMask::parse_name(w)
}

#[inline]
pub fn parse_color_word(w: &str) -> Option<ColorMask> {
    if w.eq_ignore_ascii_case("white") {
        Some(ColorMask::W)
    } else if w.eq_ignore_ascii_case("blue") {
        Some(ColorMask::U)
    } else if w.eq_ignore_ascii_case("black") {
        Some(ColorMask::B)
    } else if w.eq_ignore_ascii_case("red") {
        Some(ColorMask::R)
    } else if w.eq_ignore_ascii_case("green") {
        Some(ColorMask::G)
    } else if w.eq_ignore_ascii_case("colorless") {
        Some(ColorMask::C)
    } else {
        None
    }
}

#[inline]
pub fn parse_non_color_word(w: &str) -> Option<ColorMask> {
    if w.eq_ignore_ascii_case("nonwhite") {
        Some(ColorMask::W)
    } else if w.eq_ignore_ascii_case("nonblue") {
        Some(ColorMask::U)
    } else if w.eq_ignore_ascii_case("nonblack") {
        Some(ColorMask::B)
    } else if w.eq_ignore_ascii_case("nonred") {
        Some(ColorMask::R)
    } else if w.eq_ignore_ascii_case("nongreen") {
        Some(ColorMask::G)
    } else {
        None
    }
}

#[inline]
pub fn parse_non_type_word(w: &str) -> Option<CardTypeMask> {
    let low = w.to_ascii_lowercase();
    let rest = low.strip_prefix("non")?;
    let mask = is_known_card_type_word(rest);
    if mask.is_empty() { None } else { Some(mask) }
}

#[inline]
pub fn parse_non_supertype_word(w: &str) -> Option<SupertypeMask> {
    let low = w.to_ascii_lowercase();
    let rest = low.strip_prefix("non")?;
    let mask = is_known_supertype_word(rest);
    if mask.is_empty() { None } else { Some(mask) }
}

fn irregular_singular(word: &str) -> Option<&'static str> {
    match word {
        "Elves" => Some("Elf"),
        "Dwarves" => Some("Dwarf"),
        "Wolves" => Some("Wolf"),
        "Werewolves" => Some("Werewolf"),
        "Zombies" => Some("Zombie"),
        "Faeries" => Some("Faerie"),
        "Valkyries" => Some("Valkyrie"),
        "Mice" => Some("Mouse"),
        "Fungi" => Some("Fungus"),
        "Allies" => Some("Ally"),
        "Mercenaries" => Some("Mercenary"),
        "Heroes" => Some("Hero"),
        "Loci" => Some("Locus"),
        "Octopuses" => Some("Octopus"),
        "Sorceries" => Some("Sorcery"),
        "sorceries" => Some("sorcery"),
        "Time Lords" => Some("Time Lord"),
        "Time Lord" => Some("Time Lord"),
        _ => None,
    }
}

pub fn singular_word(word: &str) -> String {
    if let Some(irr) = irregular_singular(word) {
        return irr.to_string();
    }
    if SINGULAR_S.contains(&word) {
        return word.to_string();
    }
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if word.ends_with("sses")
        || word.ends_with("shes")
        || word.ends_with("ches")
        || word.ends_with("xes")
    {
        return word[..word.len() - 2].to_string();
    }
    if let Some(stem) = word.strip_suffix('s')
        && !stem.is_empty()
    {
        return stem.to_string();
    }
    word.to_string()
}

pub fn singular_subtype(word: &str) -> Option<String> {
    let trimmed = word.trim();
    let first = trimmed.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphabetic() || matches!(c, '\'' | '’' | '-' | ' '))
    {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("Time Lords") || trimmed.eq_ignore_ascii_case("Time Lord") {
        return Some("Time Lord".to_string());
    }
    let s = singular_word(trimmed);
    if !is_known_card_type_word(&s).is_empty() || !is_known_supertype_word(&s).is_empty() {
        return None;
    }
    Some(s)
}

const NON_SUBTYPE_CAPITALIZED_WORDS: &[&str] = &[
    "If",
    "When",
    "Whenever",
    "At",
    "During",
    "Each",
    "Every",
    "All",
    "Any",
    "Your",
    "A",
    "An",
    "Once",
    "While",
    "For",
    "Prevent",
    "Skip",
    "Double",
    "Combat",
    "Play",
    "Player",
    "Players",
    "Activated",
    "Cycling",
    "Equip",
    "Mobilize",
    "Firebending",
    "Untap",
    "Damage",
    "This",
    "Playtest",
    "As",
    "By",
    "From",
    "In",
    "Into",
    "Is",
    "Of",
    "On",
    "Or",
    "That",
    "The",
    "Their",
    "Them",
    "These",
    "They",
    "Those",
    "To",
    "Until",
    "With",
    "You",
    "It",
    "Its",
    "He",
    "She",
    "His",
    "Her",
    "Such",
];

fn is_capitalized_subtype_token(tok: &Token<'_>) -> bool {
    matches!(tok.kind, TokenKind::Word(_))
        && tok
            .raw
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        && !NON_SUBTYPE_CAPITALIZED_WORDS.contains(&tok.raw)
        && is_known_card_type_word(tok.raw).is_empty()
        && is_known_supertype_word(tok.raw).is_empty()
        && parse_color_word(tok.raw).is_none()
}

/// Public entry point: parses an [`ObjectFilter`] prefix from `tokens`.
pub fn parse_object_filter(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
    scope: &dyn ParserScope,
) -> Option<(ObjectFilter, usize)> {
    parse_object_filter_depth(tokens, ctx, opts, scope, 0)
}

/// Parses an [`ObjectFilter`] requiring full token consumption (or trailing period).
pub fn parse_object_filter_exact(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
) -> Option<ObjectFilter> {
    let cp = ctx.interner.checkpoint();
    let Some((filter, used)) = parse_object_filter(tokens, ctx, opts, &NullScope) else {
        ctx.interner.rollback(cp);
        return None;
    };
    if used == tokens.len() {
        Some(filter)
    } else {
        ctx.interner.rollback(cp);
        None
    }
}

/// Convenience helper parsing a raw string phrase into an [`ObjectFilter`].
pub fn parse_object_phrase(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
) -> Option<ObjectFilter> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tokens = tokenize(trimmed, 0);
    let mut sub_ctx = ctx.with_normalized_text(trimmed);
    let res = parse_object_filter_exact(&tokens, &mut sub_ctx, opts);
    ctx.has_read_ahead = sub_ctx.has_read_ahead;
    res
}

/// Parses `A, B, or C` / `A or B` object phrases (used by `Champion`, `Offering`, `Splice`, `Equip`, etc.).
pub fn parse_multi_or_object_phrase(
    raw: &str,
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
) -> Option<ObjectFilter> {
    parse_object_phrase(raw, ctx, opts)
}

fn is_disjunction_excluded_pair(tokens: &[Token<'_>], or_idx: usize) -> bool {
    if or_idx == 0 || or_idx + 1 >= tokens.len() {
        return false;
    }
    let prev = &tokens[or_idx - 1];
    let next = &tokens[or_idx + 1];
    // "attacking or blocking", "enchanted or equipped", "equipped or enchanted", "power or toughness"
    if (prev.eq_ci("attacking") && next.eq_ci("blocking"))
        || (prev.eq_ci("enchanted") && next.eq_ci("equipped"))
        || (prev.eq_ci("equipped") && next.eq_ci("enchanted"))
        || (prev.eq_ci("power") && next.eq_ci("toughness"))
        || (prev.eq_ci("one") && next.eq_ci("more"))
        || (prev.eq_ci("two") && next.eq_ci("more"))
        || (prev.eq_ci("three") && next.eq_ci("more"))
        || (prev.eq_ci("greater") && next.eq_ci("less"))
        || (prev.eq_ci("less") && next.eq_ci("greater"))
    {
        return true;
    }
    // Check "white or blue" color list: if prev is color and next is color, only exclude if
    // followed by a head noun/type/color/etc., i.e. let leading color disjunction handle pure color runs.
    if parse_color_word(prev.raw).is_some()
        && !prev.eq_ci("colorless")
        && parse_color_word(next.raw).is_some()
        && !next.eq_ci("colorless")
    {
        return true;
    }
    false
}

fn parse_object_filter_depth(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
    scope: &dyn ParserScope,
    depth: u8,
) -> Option<(ObjectFilter, usize)> {
    if tokens.is_empty() {
        return None;
    }

    // Special shorthand: "artifact, enchantment, or token"
    {
        let mut cur = TokenCursor::new(tokens);
        if cur.eat_ci("artifact")
            && cur.eat_punct(Punct::Comma)
            && cur.eat_ci("enchantment")
            && cur.eat_comma_or_and_or()
            && cur.eat_ci("token")
        {
            let mut f = ObjectFilter {
                zones: opts.default_zone,
                flags: FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN,
                ..Default::default()
            };
            let after_trail = parse_trailing_qualifiers(&tokens[cur.pos()..], &mut f, ctx, scope);
            return Some((f, cur.pos() + after_trail));
        }
    }

    if depth < 2 {
        // Attempt top-level disjunction split on `or` / `and/or` / `, or` / `,` list.
        let cp = ctx.interner.checkpoint();
        if let Some((disj, used)) = try_parse_top_level_disjunction(tokens, ctx, opts, scope, depth)
        {
            return Some((disj, used));
        }
        ctx.interner.rollback(cp);
    }

    let cp = ctx.interner.checkpoint();
    if let Some(res) = parse_single_object_filter(tokens, ctx, opts, scope) {
        Some(res)
    } else {
        ctx.interner.rollback(cp);
        None
    }
}

fn try_parse_top_level_disjunction(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
    scope: &dyn ParserScope,
    depth: u8,
) -> Option<(ObjectFilter, usize)> {
    // Only consider top-level disjunction when we see `,` / `or` / `and/or` (or `and` if allow_plural_and_disjunction)
    // before any trailing qualifier ("you control", "in your graveyard", "with", "that", etc.).
    // Skip past a leading color disjunction prefix (e.g. "white, blue, or black creature") so it is
    // handled as a flat `colors_any` mask by `parse_single_object_filter`.
    let mut prefix_cur = TokenCursor::new(tokens);
    let _ = eat_leading_article(&mut prefix_cur);
    let mut scan_start = 0usize;
    if prefix_cur
        .try_parse(try_parse_leading_color_disjunction)
        .is_some()
    {
        scan_start = prefix_cur.pos();
    }

    let mut paren_depth = 0usize;
    let mut split_points: Vec<(usize, usize)> = Vec::new(); // (sep_start, sep_end)
    let mut saw_or = false;
    let mut saw_and_only = false;
    let mut i = scan_start;

    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.is_punct(Punct::LParen) {
            paren_depth += 1;
            i += 1;
            continue;
        }
        if tok.is_punct(Punct::RParen) {
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if paren_depth > 0 {
            i += 1;
            continue;
        }
        // Stop scanning at clause/verb boundaries
        if tok.is_punct(Punct::Period)
            || tok.is_punct(Punct::Colon)
            || tok.is_punct(Punct::Semicolon)
            || tok.eq_ci("you")
            || tok.eq_ci("an") && tokens.get(i + 1).is_some_and(|n| n.eq_ci("opponent"))
            || tok.eq_ci("in")
            || tok.eq_ci("on")
            || tok.eq_ci("from")
            || tok.eq_ci("with")
            || tok.eq_ci("without")
            || tok.eq_ci("that")
            || tok.eq_ci("whose")
            || tok.eq_ci("attached")
            || tok.eq_ci("exiled")
            || tok.eq_ci("owned")
            || tok.eq_ci("controlled")
            || tok.eq_ci("cast")
            || tok.eq_ci("put")
            || tok.eq_ci("for")
            || tok.eq_ci("to")
            || tok.eq_ci("deals")
            || tok.eq_ci("gains")
            || tok.eq_ci("gets")
        {
            break;
        }

        // Skip over "named (~ | <Card Name>)" so card-name tokens containing
        // "and" / "," (e.g. "Alpha and Omega", "Rin and Seri, Inseparable")
        // are not split as top-level disjunctions, while still allowing
        // disjunctions after the name (e.g. "creatures named Foo or artifacts").
        if tok.eq_ci("named") {
            if tokens
                .get(i + 1)
                .is_some_and(|n| matches!(n.kind, TokenKind::SelfRef))
            {
                i += 2;
                continue;
            }
            let mut name_cur = TokenCursor::new(&tokens[i + 1..]);
            let name_tokens = collect_name_tokens(&mut name_cur);
            if !name_tokens.is_empty() {
                i += 1 + name_tokens.len();
                continue;
            }
            break;
        }

        if tok.eq_ci("or") && !is_disjunction_excluded_pair(tokens, i) {
            // Do not split "or more" / "or less" / "or greater" / "or fewer"
            if tokens.get(i + 1).is_some_and(|n| {
                n.eq_ci("more") || n.eq_ci("less") || n.eq_ci("greater") || n.eq_ci("fewer")
            }) {
                i += 1;
                continue;
            }
            saw_or = true;
            split_points.push((i, i + 1));
            i += 1;
            continue;
        }
        if tok.eq_ci("and")
            && tokens.get(i + 1).is_some_and(|n| n.is_punct(Punct::Slash))
            && tokens.get(i + 2).is_some_and(|n| n.eq_ci("or"))
        {
            saw_or = true;
            split_points.push((i, i + 3));
            i += 3;
            continue;
        }
        if tok.is_punct(Punct::Comma) {
            // `, or ` / `, and/or ` / `, `
            if tokens.get(i + 1).is_some_and(|n| n.eq_ci("or")) {
                saw_or = true;
                split_points.push((i, i + 2));
                i += 2;
                continue;
            }
            if tokens.get(i + 1).is_some_and(|n| n.eq_ci("and"))
                && tokens.get(i + 2).is_some_and(|n| n.is_punct(Punct::Slash))
                && tokens.get(i + 3).is_some_and(|n| n.eq_ci("or"))
            {
                saw_or = true;
                split_points.push((i, i + 4));
                i += 4;
                continue;
            }
            if opts.allow_plural_and_disjunction
                && tokens.get(i + 1).is_some_and(|n| n.eq_ci("and"))
            {
                saw_or = true;
                saw_and_only = true;
                split_points.push((i, i + 2));
                i += 2;
                continue;
            }
            split_points.push((i, i + 1));
            i += 1;
            continue;
        }
        if opts.allow_plural_and_disjunction
            && tok.eq_ci("and")
            && !tokens.get(i + 1).is_some_and(|n| n.is_punct(Punct::Slash))
        {
            // Check if both sides look like plural types/subtypes or simple filters
            saw_or = true;
            saw_and_only = true;
            split_points.push((i, i + 1));
            i += 1;
            continue;
        }
        i += 1;
    }

    if !saw_or || split_points.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();

    // Build branch token slices up to end of last branch head
    let last_sep_end = split_points.last()?.1;
    let tail_tokens = &tokens[last_sep_end..];
    // Parse final branch using parse_single_object_filter so we know exact end of the entire filter!
    let mut no_default_opts = opts;
    no_default_opts.default_zone = ZoneMask::empty();
    let Some((last_branch, last_used)) =
        parse_single_object_filter(tail_tokens, ctx, no_default_opts, scope)
    else {
        ctx.interner.rollback(cp);
        return None;
    };
    if last_used == 0 {
        ctx.interner.rollback(cp);
        return None;
    }
    let total_used = last_sep_end + last_used;

    // Now extract all preceding branches
    let mut branch_slices: Vec<&[Token<'_>]> = Vec::with_capacity(split_points.len() + 1);
    let mut prev_start = 0usize;
    for &(sep_start, sep_end) in &split_points {
        if sep_start <= prev_start {
            ctx.interner.rollback(cp);
            return None;
        }
        branch_slices.push(&tokens[prev_start..sep_start]);
        prev_start = sep_end;
    }

    let mut parsed_branches: Vec<ObjectFilter> = Vec::with_capacity(branch_slices.len() + 1);
    let mut branch_has_noun: Vec<bool> = Vec::with_capacity(branch_slices.len() + 1);

    for sl in &branch_slices {
        let Some((bf, bu, has_noun)) =
            parse_branch_allow_modifier_only(sl, ctx, no_default_opts, scope, depth + 1)
        else {
            ctx.interner.rollback(cp);
            return None;
        };
        if bu != sl.len() {
            ctx.interner.rollback(cp);
            return None;
        }
        parsed_branches.push(bf);
        branch_has_noun.push(has_noun);
    }

    // Shared trailing head noun propagation ("black or artifact creature", "red or white instant")
    let last_has_head_noun = !last_branch.types_any.is_empty()
        || !last_branch.types_all.is_empty()
        || !last_branch.subtypes_any.is_empty()
        || !last_branch.subtypes_all.is_empty();
    if last_has_head_noun && branch_has_noun.iter().any(|&b| !b) {
        for (bf, has_noun) in parsed_branches.iter_mut().zip(&branch_has_noun) {
            if !*has_noun {
                bf.types_any |= last_branch.types_any;
                bf.types_all |= last_branch.types_all;
                for &st in &last_branch.subtypes_any {
                    if !bf.subtypes_any.contains(&st) {
                        bf.subtypes_any.push(st);
                    }
                }
                for &st in &last_branch.subtypes_all {
                    if !bf.subtypes_all.contains(&st) {
                        bf.subtypes_all.push(st);
                    }
                }
            }
        }
    }

    // Shared trailing qualifiers from last_branch (e.g., "a creature, Spacecraft, or Planet you control",
    // "artifact or enchantment that player controls", "artifact or enchantment card with mana value X or less from your graveyard")
    let shared_controller = last_branch.controller;
    let shared_owner = last_branch.owner;
    let shared_zones = last_branch.zones;
    let shared_mana_value = last_branch.mana_value.clone();
    let shared_power = last_branch.power.clone();
    let shared_toughness = last_branch.toughness.clone();
    let shared_ctrl_preds: Vec<ObjectPredicate> = last_branch
        .extra
        .iter()
        .filter(|p| {
            matches!(
                p,
                ObjectPredicate::ControllerRef { .. }
                    | ObjectPredicate::OwnerRef { .. }
                    | ObjectPredicate::ControlledByEnchantedPlayer
                    | ObjectPredicate::ControlledByChosenPlayer
                    | ObjectPredicate::ControllerOfTarget { .. }
                    | ObjectPredicate::ControllerOfEventObject
                    | ObjectPredicate::ManaValueLtRevealedCostCard
                    | ObjectPredicate::ManaValueLteEventAmount
                    | ObjectPredicate::PowerLteRevealedCostCard
                    | ObjectPredicate::ManaValueEqualsSourcePower
                    | ObjectPredicate::ManaValueEqualsChosenNumber
                    | ObjectPredicate::ManaValueLteTotalCounters
                    | ObjectPredicate::ManaValueParity { .. }
                    | ObjectPredicate::EnteredGraveyardFromBattlefieldThisTurn
            )
        })
        .cloned()
        .collect();

    parsed_branches.push(last_branch);

    let can_hoist_mv = shared_mana_value.is_some()
        && parsed_branches[..branch_slices.len()]
            .iter()
            .all(|b| b.mana_value.is_none());
    let can_hoist_pow = shared_power.is_some()
        && parsed_branches[..branch_slices.len()]
            .iter()
            .all(|b| b.power.is_none());
    let can_hoist_tou = shared_toughness.is_some()
        && parsed_branches[..branch_slices.len()]
            .iter()
            .all(|b| b.toughness.is_none());
    let has_shared_ctrl = shared_ctrl_preds.iter().any(|p| {
        matches!(
            p,
            ObjectPredicate::ControllerRef { .. }
                | ObjectPredicate::OwnerRef { .. }
                | ObjectPredicate::ControlledByEnchantedPlayer
                | ObjectPredicate::ControlledByChosenPlayer
                | ObjectPredicate::ControllerOfTarget { .. }
                | ObjectPredicate::ControllerOfEventObject
        )
    });
    let can_hoist_ctrl_preds = !shared_ctrl_preds.is_empty()
        && parsed_branches[..branch_slices.len()].iter().all(|b| {
            (!has_shared_ctrl || b.controller.is_none())
                && !b.extra.iter().any(|p| shared_ctrl_preds.contains(p))
        });

    let strip_shared_for_pure_check = |b: &ObjectFilter| -> ObjectFilter {
        let mut stripped = b.clone();
        stripped.flags = FilterFlags::empty();
        if can_hoist_mv {
            stripped.mana_value = None;
        }
        if can_hoist_pow {
            stripped.power = None;
        }
        if can_hoist_tou {
            stripped.toughness = None;
        }
        if can_hoist_ctrl_preds {
            stripped.extra.retain(|p| !shared_ctrl_preds.contains(p));
        }
        stripped
    };

    // Leading prefix flags (e.g. "another creature or artifact", "other artifacts, creatures, and planeswalkers")
    let leading_flags = parsed_branches[0].flags;
    let compatible_leading_flags = parsed_branches[1..]
        .iter()
        .all(|b| b.flags.is_empty() || b.flags == leading_flags);

    if compatible_leading_flags {
        let all_pure_types = parsed_branches
            .iter()
            .all(|b| is_pure_types_filter(&strip_shared_for_pure_check(b)));
        if all_pure_types {
            let mut combined = ObjectFilter {
                zones: if !shared_zones.is_empty() {
                    shared_zones
                } else {
                    opts.default_zone
                },
                controller: shared_controller,
                owner: shared_owner,
                flags: leading_flags,
                mana_value: if can_hoist_mv {
                    shared_mana_value
                } else {
                    None
                },
                power: if can_hoist_pow { shared_power } else { None },
                toughness: if can_hoist_tou {
                    shared_toughness
                } else {
                    None
                },
                extra: if can_hoist_ctrl_preds {
                    shared_ctrl_preds
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            for b in &parsed_branches {
                combined.types_any |= b.types_any;
                combined.types_any |= b.types_all;
            }
            return Some((combined, total_used));
        }

        // Also handle plural subtype `and`-disjunction with trailing `creatures` ("other Elf and Warrior creatures")
        let last_idx = parsed_branches.len() - 1;
        let trailing_and_type = if saw_and_only
            && parsed_branches[..last_idx]
                .iter()
                .all(|b| b.types_any.is_empty() && b.types_all.is_empty())
            && !parsed_branches[last_idx].subtypes_any.is_empty()
            && parsed_branches[last_idx].types_any == CardTypeMask::CREATURE
            && parsed_branches[last_idx].types_all.is_empty()
        {
            parsed_branches[last_idx].types_any
        } else {
            CardTypeMask::empty()
        };

        let all_pure_subtypes = parsed_branches.iter().enumerate().all(|(idx, b)| {
            let mut stripped = strip_shared_for_pure_check(b);
            if idx == last_idx && !trailing_and_type.is_empty() {
                stripped.types_any = CardTypeMask::empty();
            }
            is_pure_subtypes_filter(&stripped)
        });
        if all_pure_subtypes {
            let mut combined = ObjectFilter {
                zones: if !shared_zones.is_empty() {
                    shared_zones
                } else {
                    opts.default_zone
                },
                controller: shared_controller,
                owner: shared_owner,
                flags: leading_flags,
                types_any: trailing_and_type,
                mana_value: if can_hoist_mv {
                    shared_mana_value
                } else {
                    None
                },
                power: if can_hoist_pow { shared_power } else { None },
                toughness: if can_hoist_tou {
                    shared_toughness
                } else {
                    None
                },
                extra: if can_hoist_ctrl_preds {
                    shared_ctrl_preds
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            for b in &parsed_branches {
                for &st in b.subtypes_any.iter().chain(b.subtypes_all.iter()) {
                    if !combined.subtypes_any.contains(&st) {
                        combined.subtypes_any.push(st);
                    }
                }
            }
            return Some((combined, total_used));
        }

        let all_pure_colors = parsed_branches
            .iter()
            .all(|b| is_pure_colors_filter(&strip_shared_for_pure_check(b)));
        if all_pure_colors {
            let mut combined = ObjectFilter {
                zones: if !shared_zones.is_empty() {
                    shared_zones
                } else {
                    opts.default_zone
                },
                controller: shared_controller,
                owner: shared_owner,
                flags: leading_flags,
                mana_value: if can_hoist_mv {
                    shared_mana_value
                } else {
                    None
                },
                power: if can_hoist_pow { shared_power } else { None },
                toughness: if can_hoist_tou {
                    shared_toughness
                } else {
                    None
                },
                extra: if can_hoist_ctrl_preds {
                    shared_ctrl_preds
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            for b in &parsed_branches {
                combined.colors_any |= b.colors_any;
            }
            return Some((combined, total_used));
        }
    }

    // Distribute shared trailing qualifiers when earlier branches lacked them
    if shared_controller.is_some()
        || shared_owner.is_some()
        || !shared_zones.is_empty()
        || can_hoist_mv
        || can_hoist_pow
        || can_hoist_tou
        || can_hoist_ctrl_preds
    {
        for b in parsed_branches.iter_mut().take(branch_slices.len()) {
            if b.controller.is_none() {
                b.controller = shared_controller;
            }
            if b.owner.is_none() {
                b.owner = shared_owner;
            }
            if b.zones.is_empty() {
                b.zones = shared_zones;
            }
            if can_hoist_mv && b.mana_value.is_none() {
                b.mana_value = shared_mana_value.clone();
            }
            if can_hoist_pow && b.power.is_none() {
                b.power = shared_power.clone();
            }
            if can_hoist_tou && b.toughness.is_none() {
                b.toughness = shared_toughness.clone();
            }
            if can_hoist_ctrl_preds {
                for p in &shared_ctrl_preds {
                    if !b.extra.contains(p) {
                        b.extra.push(p.clone());
                    }
                }
            }
        }
    }

    let outer = ObjectFilter {
        zones: opts.default_zone,
        extra: vec![ObjectPredicate::AnyOf {
            filters: parsed_branches,
        }],
        ..Default::default()
    };
    Some((outer, total_used))
}

fn is_pure_types_filter(f: &ObjectFilter) -> bool {
    (!f.types_any.is_empty() || !f.types_all.is_empty())
        && f.types_none.is_empty()
        && f.supertypes_any.is_empty()
        && f.supertypes_none.is_empty()
        && f.colors_any.is_empty()
        && f.colors_all.is_empty()
        && f.colors_none.is_empty()
        && f.subtypes_any.is_empty()
        && f.subtypes_all.is_empty()
        && f.subtypes_none.is_empty()
        && f.flags.is_empty()
        && f.mana_value.is_none()
        && f.power.is_none()
        && f.toughness.is_none()
        && f.extra.is_empty()
        && f.stack_kind.is_none()
}

fn is_pure_subtypes_filter(f: &ObjectFilter) -> bool {
    (!f.subtypes_any.is_empty() || !f.subtypes_all.is_empty())
        && f.types_any.is_empty()
        && f.types_all.is_empty()
        && f.types_none.is_empty()
        && f.supertypes_any.is_empty()
        && f.supertypes_none.is_empty()
        && f.colors_any.is_empty()
        && f.colors_all.is_empty()
        && f.colors_none.is_empty()
        && f.subtypes_none.is_empty()
        && f.flags.is_empty()
        && f.mana_value.is_none()
        && f.power.is_none()
        && f.toughness.is_none()
        && f.extra.is_empty()
        && f.stack_kind.is_none()
}

fn is_pure_colors_filter(f: &ObjectFilter) -> bool {
    !f.colors_any.is_empty()
        && f.colors_all.is_empty()
        && f.colors_none.is_empty()
        && f.types_any.is_empty()
        && f.types_all.is_empty()
        && f.types_none.is_empty()
        && f.supertypes_any.is_empty()
        && f.supertypes_none.is_empty()
        && f.subtypes_any.is_empty()
        && f.subtypes_all.is_empty()
        && f.subtypes_none.is_empty()
        && f.flags.is_empty()
        && f.mana_value.is_none()
        && f.power.is_none()
        && f.toughness.is_none()
        && f.extra.is_empty()
        && f.stack_kind.is_none()
}

fn parse_branch_allow_modifier_only(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
    scope: &dyn ParserScope,
    depth: u8,
) -> Option<(ObjectFilter, usize, bool)> {
    let _ = depth;
    let cp = ctx.interner.checkpoint();
    if let Some((f, used)) = parse_single_object_filter(tokens, ctx, opts, scope) {
        let has_noun = !f.types_any.is_empty()
            || !f.types_all.is_empty()
            || !f.subtypes_any.is_empty()
            || !f.subtypes_all.is_empty()
            || !f.supertypes_any.is_empty()
            || f.flags.contains(FilterFlags::TOKEN)
            || f.flags.contains(FilterFlags::IS_COMMANDER);
        return Some((f, used, has_noun));
    }
    ctx.interner.rollback(cp);
    // Try modifier-only branch (e.g. "black" in "black or artifact creature")
    let cp2 = ctx.interner.checkpoint();
    let mut cur = TokenCursor::new(tokens);
    let mut filter = ObjectFilter::default();
    let _ = eat_leading_article(&mut cur);
    let mod_count = parse_prefix_modifiers(&mut cur, &mut filter, ctx, opts);
    if mod_count > 0 && cur.pos() == tokens.len() {
        return Some((filter, cur.pos(), false));
    }
    ctx.interner.rollback(cp2);
    None
}

fn eat_leading_article(cur: &mut TokenCursor<'_>) -> bool {
    cur.eat_ci("a") || cur.eat_ci("an") || cur.eat_ci("any")
}

fn is_negated_modifier_token(cur: &TokenCursor<'_>, offset: usize) -> bool {
    let Some(t) = cur.peek_n(offset) else {
        return false;
    };
    if t.eq_ci("non")
        && cur
            .peek_n(offset + 1)
            .is_some_and(|p| p.is_punct(Punct::Hyphen))
    {
        return true;
    }
    parse_non_type_word(t.raw).is_some()
        || parse_non_color_word(t.raw).is_some()
        || parse_non_supertype_word(t.raw).is_some()
        || t.eq_ci("nontoken")
        || t.eq_ci("noncommander")
        || t.eq_ci("nonmulticolored")
        || t.eq_ci("nonmonocolored")
}

fn parse_prefix_modifiers(
    cur: &mut TokenCursor<'_>,
    filter: &mut ObjectFilter,
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
) -> usize {
    let mut count = 0usize;
    loop {
        if cur.is_empty() {
            break;
        }

        // Consume `,` separating consecutive negated modifiers (e.g. "nonartifact, nonblack creature",
        // "non-Angel, non-Demon, non-Devil, non-Dragon creature", "nonland, nontoken permanent")
        if count > 0
            && cur.peek().is_some_and(|t| t.is_punct(Punct::Comma))
            && is_negated_modifier_token(cur, 1)
        {
            cur.bump();
            continue;
        }

        // Multi-word prefix modifiers
        if cur.eat_words(&["attacking", "or", "blocking"]) {
            filter.flags |= FilterFlags::ATTACKING_OR_BLOCKING;
            count += 1;
            continue;
        }
        if cur.eat_words(&["enchanted", "or", "equipped"])
            || cur.eat_words(&["equipped", "or", "enchanted"])
        {
            filter.flags |= FilterFlags::ENCHANTED_OR_EQUIPPED;
            count += 1;
            continue;
        }
        if cur.eat_hyphenated("face", "down") {
            filter.flags |= FilterFlags::FACE_DOWN;
            count += 1;
            continue;
        }
        if cur.eat_hyphenated("face", "up") {
            filter.flags |= FilterFlags::FACE_UP;
            count += 1;
            continue;
        }

        // non-Time Lord(s), non-outlaw(s), non-<Subtype>
        if cur
            .try_parse(|c| {
                if !c.eat_ci("non") || !c.eat_punct(Punct::Hyphen) {
                    return None;
                }
                if c.eat_words(&["Time", "Lord"]) || c.eat_words(&["Time", "Lords"]) {
                    let id = ctx.interner.intern_subtype("Time Lord");
                    if !filter.subtypes_none.contains(&id) {
                        filter.subtypes_none.push(id);
                    }
                    return Some(());
                }
                let tok = c.peek()?;
                if tok.eq_ci("outlaw") || tok.eq_ci("outlaws") {
                    c.bump();
                    for sub in OUTLAW_SUBTYPES {
                        let id = ctx.interner.intern_subtype(sub);
                        if !filter.subtypes_none.contains(&id) {
                            filter.subtypes_none.push(id);
                        }
                    }
                    return Some(());
                }
                if is_capitalized_subtype_token(tok) {
                    let s = singular_subtype(tok.raw)?;
                    c.bump();
                    let id = ctx.interner.intern_subtype(&s);
                    if !filter.subtypes_none.contains(&id) {
                        filter.subtypes_none.push(id);
                    }
                    return Some(());
                }
                None
            })
            .is_some()
        {
            count += 1;
            continue;
        }

        // P/T prefix like "2/2" (Number Slash Number)
        if let Some((p, t)) = cur.try_parse(|c| {
            let p_tok = c.bump()?;
            let p = match p_tok.kind {
                TokenKind::Number(n) => n as i32,
                TokenKind::SignedNumber(n) => n,
                _ => return None,
            };
            if !c.eat_punct(Punct::Slash) {
                return None;
            }
            let t_tok = c.bump()?;
            let t = match t_tok.kind {
                TokenKind::Number(n) => n as i32,
                TokenKind::SignedNumber(n) => n,
                _ => return None,
            };
            Some((p, t))
        }) {
            filter.power = Some(Compare {
                op: CompareOp::Eq,
                value: Box::new(mtg_ir::Amount::Const { n: p }),
            });
            filter.toughness = Some(Compare {
                op: CompareOp::Eq,
                value: Box::new(mtg_ir::Amount::Const { n: t }),
            });
            count += 1;
            continue;
        }

        // Color disjunction prefix: "white or blue ...", "white, blue, or black ..."
        if let Some(cols) = cur.try_parse(try_parse_leading_color_disjunction) {
            for col in cols {
                filter.colors_any |= col;
            }
            count += 1;
            continue;
        }

        let Some(tok) = cur.peek() else {
            break;
        };

        // Single-word FilterFlags
        if tok.eq_ci("another") || tok.eq_ci("other") {
            cur.bump();
            filter.flags |= FilterFlags::OTHER;
            count += 1;
            continue;
        }
        if tok.eq_ci("tapped") {
            cur.bump();
            filter.flags |= FilterFlags::TAPPED;
            count += 1;
            continue;
        }
        if tok.eq_ci("untapped") {
            cur.bump();
            filter.flags |= FilterFlags::UNTAPPED;
            count += 1;
            continue;
        }
        if tok.eq_ci("exiled") {
            cur.bump();
            filter.zones = ZoneMask::EXILE;
            count += 1;
            continue;
        }
        if tok.eq_ci("nontoken") {
            cur.bump();
            filter.flags |= FilterFlags::NONTOKEN;
            count += 1;
            continue;
        }
        if tok.eq_ci("attacking") {
            cur.bump();
            filter.flags |= FilterFlags::ATTACKING;
            count += 1;
            continue;
        }
        if tok.eq_ci("blocking") {
            cur.bump();
            filter.flags |= FilterFlags::BLOCKING;
            count += 1;
            continue;
        }
        if tok.eq_ci("unblocked") {
            cur.bump();
            filter.flags |= FilterFlags::UNBLOCKED_ATTACKER;
            count += 1;
            continue;
        }
        if tok.eq_ci("noncommander") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_COMMANDER;
            count += 1;
            continue;
        }
        if tok.eq_ci("multicolored") {
            cur.bump();
            filter.flags |= FilterFlags::MULTICOLORED;
            count += 1;
            continue;
        }
        if tok.eq_ci("nonmulticolored") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_MULTICOLORED;
            count += 1;
            continue;
        }
        if tok.eq_ci("monocolored") {
            cur.bump();
            filter.flags |= FilterFlags::MONOCOLORED;
            count += 1;
            continue;
        }
        if tok.eq_ci("nonmonocolored") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_MONOCOLORED;
            count += 1;
            continue;
        }
        if tok.eq_ci("colored") {
            cur.bump();
            filter.flags |= FilterFlags::IS_COLORED;
            count += 1;
            continue;
        }
        if tok.eq_ci("historic") {
            cur.bump();
            filter.flags |= FilterFlags::HISTORIC;
            count += 1;
            continue;
        }
        if tok.eq_ci("modified") {
            cur.bump();
            filter.flags |= FilterFlags::MODIFIED;
            count += 1;
            continue;
        }
        if tok.eq_ci("unmodified") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_MODIFIED;
            count += 1;
            continue;
        }
        if tok.eq_ci("equipped") {
            cur.bump();
            filter.flags |= FilterFlags::EQUIPPED;
            count += 1;
            continue;
        }
        if tok.eq_ci("unequipped") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_EQUIPPED;
            count += 1;
            continue;
        }
        if tok.eq_ci("enchanted") {
            cur.bump();
            filter.flags |= FilterFlags::ENCHANTED;
            count += 1;
            continue;
        }
        if tok.eq_ci("unenchanted") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_ENCHANTED;
            count += 1;
            continue;
        }
        if tok.eq_ci("saddled") {
            cur.bump();
            filter.flags |= FilterFlags::SADDLED;
            count += 1;
            continue;
        }
        if tok.eq_ci("monstrous") {
            cur.bump();
            filter.flags |= FilterFlags::MONSTROUS;
            count += 1;
            continue;
        }
        if tok.eq_ci("suspected") {
            cur.bump();
            filter.flags |= FilterFlags::SUSPECTED;
            count += 1;
            continue;
        }
        if tok.eq_ci("unsuspected") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_SUSPECTED;
            count += 1;
            continue;
        }
        if tok.eq_ci("prepared") {
            cur.bump();
            filter.flags |= FilterFlags::PREPARED;
            count += 1;
            continue;
        }
        if tok.eq_ci("unprepared") {
            cur.bump();
            filter.flags |= FilterFlags::NOT_PREPARED;
            count += 1;
            continue;
        }
        if tok.eq_ci("renowned") {
            cur.bump();
            filter.flags |= FilterFlags::RENOWNED;
            count += 1;
            continue;
        }
        if tok.eq_ci("goaded") {
            cur.bump();
            filter.flags |= FilterFlags::GOADED;
            count += 1;
            continue;
        }
        if tok.eq_ci("kicked") {
            cur.bump();
            filter.flags |= FilterFlags::KICKED_SPELL;
            count += 1;
            continue;
        }
        if tok.eq_ci("bargained") {
            cur.bump();
            filter.flags |= FilterFlags::BARGAINED_SPELL;
            count += 1;
            continue;
        }

        // Token/commander as modifier before a noun (e.g. "token creature", "commander creature")
        if (tok.eq_ci("token") || tok.eq_ci("tokens"))
            && cur.peek_n(1).is_some_and(|n| {
                !is_known_card_type_word(n.raw).is_empty()
                    || is_capitalized_subtype_token(n)
                    || n.eq_ci("permanent")
                    || n.eq_ci("permanents")
            })
        {
            cur.bump();
            filter.flags |= FilterFlags::TOKEN;
            count += 1;
            continue;
        }
        if (tok.eq_ci("commander") || tok.eq_ci("commanders"))
            && cur.peek_n(1).is_some_and(|n| {
                !is_known_card_type_word(n.raw).is_empty()
                    || is_capitalized_subtype_token(n)
                    || n.eq_ci("creature")
                    || n.eq_ci("creatures")
                    || n.eq_ci("spell")
                    || n.eq_ci("spells")
            })
        {
            cur.bump();
            filter.flags |= FilterFlags::IS_COMMANDER;
            count += 1;
            continue;
        }

        // Supertypes & non-supertypes
        let sup = is_known_supertype_word(tok.raw);
        if !sup.is_empty() {
            cur.bump();
            filter.supertypes_any |= sup;
            count += 1;
            continue;
        }
        if let Some(nsup) = parse_non_supertype_word(tok.raw) {
            cur.bump();
            filter.supertypes_none |= nsup;
            count += 1;
            continue;
        }

        // non-<type>
        if let Some(ntype) = parse_non_type_word(tok.raw) {
            cur.bump();
            if ntype == CardTypeMask::PERMANENT {
                filter.types_none |= CardTypeMask::PERMANENT;
            } else {
                filter.types_none |= ntype;
            }
            count += 1;
            continue;
        }

        // Colors & non-colors
        if let Some(col) = parse_color_word(tok.raw) {
            // Do not consume `"black"` in `"black or artifact creature"` — that's a branch disjunction!
            let next_is_or_noncolor = cur.peek_n_ci(1, "or")
                && cur
                    .peek_n(2)
                    .is_some_and(|n| parse_color_word(n.raw).is_none());
            if next_is_or_noncolor {
                break;
            }
            cur.bump();
            if !filter.colors_any.is_empty() && cur.peek_ci("and") {
                // Handled below if `and <color>` follows
            }
            filter.colors_any |= col;
            // Handle "white and blue <noun>"
            if cur.peek_ci("and")
                && cur
                    .peek_n(1)
                    .and_then(|n| parse_color_word(n.raw))
                    .is_some()
            {
                let mut all_cols = filter.colors_any;
                while cur.peek_ci("and")
                    && let Some(c2) = cur.peek_n(1).and_then(|n| parse_color_word(n.raw))
                {
                    cur.bump();
                    cur.bump();
                    all_cols |= c2;
                }
                filter.colors_any = all_cols;
            }
            count += 1;
            continue;
        }
        if let Some(ncol) = parse_non_color_word(tok.raw) {
            cur.bump();
            filter.colors_none |= ncol;
            count += 1;
            continue;
        }

        if opts.expand_outlaw_subtypes && (tok.eq_ci("outlaw") || tok.eq_ci("outlaws")) {
            // Only consume as prefix if followed by head noun
            if cur.peek_n(1).is_some_and(|n| {
                !is_known_card_type_word(n.raw).is_empty()
                    || n.eq_ci("card")
                    || n.eq_ci("cards")
                    || n.eq_ci("spell")
                    || n.eq_ci("spells")
                    || n.eq_ci("permanent")
                    || n.eq_ci("permanents")
            }) {
                cur.bump();
                for sub in OUTLAW_SUBTYPES {
                    let id = ctx.interner.intern_subtype(sub);
                    if !filter.subtypes_any.contains(&id) {
                        filter.subtypes_any.push(id);
                    }
                }
                count += 1;
                continue;
            }
        }

        break;
    }
    count
}

fn try_parse_leading_color_disjunction(cur: &mut TokenCursor<'_>) -> Option<Vec<ColorMask>> {
    let first = cur.bump()?;
    let c0 = parse_color_word(first.raw)?;
    if c0 == ColorMask::C {
        return None;
    }
    let mut colors = vec![c0];
    let mut saw_or = false;
    for _ in 0..8 {
        let is_and_all = |c: &mut TokenCursor<'_>| {
            let sv = c.save();
            if c.eat_ci("and") && c.eat_ci("all") {
                true
            } else {
                c.restore(sv);
                false
            }
        };
        if cur.eat_and_or() || is_and_all(cur) {
            let next = cur.bump()?;
            let c = parse_color_word(next.raw)?;
            if c == ColorMask::C {
                return None;
            }
            if !colors.contains(&c) {
                colors.push(c);
            }
            saw_or = true;
            break;
        }
        if cur.eat_punct(Punct::Comma) {
            if cur.eat_and_or() || is_and_all(cur) {
                let next = cur.bump()?;
                let c = parse_color_word(next.raw)?;
                if c == ColorMask::C {
                    return None;
                }
                if !colors.contains(&c) {
                    colors.push(c);
                }
                saw_or = true;
                break;
            }
            let next = cur.bump()?;
            let c = parse_color_word(next.raw)?;
            if c == ColorMask::C {
                return None;
            }
            if !colors.contains(&c) {
                colors.push(c);
            }
            continue;
        }
        return None;
    }
    if !saw_or || cur.is_empty() {
        return None;
    }
    Some(colors)
}

pub fn parse_single_object_filter(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    opts: FilterOpts,
    scope: &dyn ParserScope,
) -> Option<(ObjectFilter, usize)> {
    let cp = ctx.interner.checkpoint();
    let mut cur = TokenCursor::new(tokens);
    let mut filter = ObjectFilter {
        zones: opts.default_zone,
        ..Default::default()
    };

    let _article = eat_leading_article(&mut cur);
    let mod_count = parse_prefix_modifiers(&mut cur, &mut filter, ctx, opts);

    // Core Head Noun Group
    let mut head_types: Vec<CardTypeMask> = Vec::new();
    let mut head_subtypes: Vec<SubtypeId> = Vec::new();
    let mut saw_head_noun = false;

    loop {
        if cur.eat_words(&["Time", "Lord"]) || cur.eat_words(&["Time", "Lords"]) {
            if !head_subtypes.is_empty() || !filter.subtypes_all.is_empty() {
                ctx.interner.rollback(cp);
                return None;
            }
            let id = ctx.interner.intern_subtype("Time Lord");
            head_subtypes.push(id);
            saw_head_noun = true;
            continue;
        }
        if cur.eat_words(&["Eldrazi", "Scion"]) || cur.eat_words(&["Eldrazi", "Scions"]) {
            if !head_subtypes.is_empty() || !filter.subtypes_all.is_empty() {
                ctx.interner.rollback(cp);
                return None;
            }
            let e_id = ctx.interner.intern_subtype("Eldrazi");
            let s_id = ctx.interner.intern_subtype("Scion");
            filter.subtypes_all.push(e_id);
            filter.subtypes_all.push(s_id);
            saw_head_noun = true;
            continue;
        }
        if cur.eat_words(&["Eldrazi", "Spawn"]) || cur.eat_words(&["Eldrazi", "Spawns"]) {
            if !head_subtypes.is_empty() || !filter.subtypes_all.is_empty() {
                ctx.interner.rollback(cp);
                return None;
            }
            let e_id = ctx.interner.intern_subtype("Eldrazi");
            let s_id = ctx.interner.intern_subtype("Spawn");
            filter.subtypes_all.push(e_id);
            filter.subtypes_all.push(s_id);
            saw_head_noun = true;
            continue;
        }
        let Some(tok) = cur.peek() else {
            break;
        };

        // Generic nouns
        if tok.eq_ci("permanent") || tok.eq_ci("permanents") {
            cur.bump();
            saw_head_noun = true;
            // Check "permanent card(s)"
            if cur
                .peek()
                .is_some_and(|n| n.eq_ci("card") || n.eq_ci("cards"))
            {
                cur.bump();
                if filter.types_any.is_empty() && filter.types_none.is_empty() {
                    filter.types_none |= CardTypeMask::INSTANT | CardTypeMask::SORCERY;
                }
            }
            continue;
        }
        if tok.eq_ci("card") || tok.eq_ci("cards") {
            cur.bump();
            saw_head_noun = true;
            if opts.strip_trailing_card && filter.zones == opts.default_zone {
                // Keep valid head noun; strip_trailing_card just allows it to be consumed
            }
            continue;
        }
        if tok.eq_ci("spell") || tok.eq_ci("spells") {
            cur.bump();
            saw_head_noun = true;
            if !opts.allow_spell_noun && filter.zones == opts.default_zone {
                filter.zones = ZoneMask::STACK;
            }
            continue;
        }
        if tok.eq_ci("source") || tok.eq_ci("sources") {
            cur.bump();
            saw_head_noun = true;
            continue;
        }
        if tok.eq_ci("token") || tok.eq_ci("tokens") {
            cur.bump();
            filter.flags |= FilterFlags::TOKEN;
            saw_head_noun = true;
            continue;
        }
        if tok.eq_ci("commander") || tok.eq_ci("commanders") {
            cur.bump();
            filter.flags |= FilterFlags::IS_COMMANDER;
            saw_head_noun = true;
            continue;
        }
        if tok.eq_ci("outlaw") || tok.eq_ci("outlaws") {
            cur.bump();
            if opts.expand_outlaw_subtypes {
                for sub in OUTLAW_SUBTYPES {
                    let id = ctx.interner.intern_subtype(sub);
                    if !filter.subtypes_any.contains(&id) {
                        filter.subtypes_any.push(id);
                    }
                }
            } else {
                filter.flags |= FilterFlags::OUTLAW;
            }
            saw_head_noun = true;
            continue;
        }

        // Card types
        let ctype = is_known_card_type_word(tok.raw);
        if !ctype.is_empty() {
            cur.bump();
            head_types.push(ctype);
            saw_head_noun = true;
            continue;
        }

        // Capitalized subtype(s), including hyphenated like "Assembly-Worker"
        // TS `costs.ts:942` rejects >1 subtype token (only `Time Lord` and `Eldrazi Scion/Spawn` are multi-word).
        if is_capitalized_subtype_token(tok) {
            if !head_subtypes.is_empty() || !filter.subtypes_all.is_empty() {
                ctx.interner.rollback(cp);
                return None;
            }
            if let Some(hyph) = cur.try_parse(|c| {
                let left = c.bump()?;
                if !c.eat_punct(Punct::Hyphen) {
                    return None;
                }
                let right = c.bump()?;
                if !is_capitalized_subtype_token(right) {
                    return None;
                }
                let combined = format!("{}-{}", left.raw, singular_word(right.raw));
                Some(combined)
            }) {
                let id = ctx.interner.intern_subtype(&hyph);
                head_subtypes.push(id);
                saw_head_noun = true;
                continue;
            }
            if let Some(sub) = singular_subtype(tok.raw) {
                cur.bump();
                let id = ctx.interner.intern_subtype(&sub);
                head_subtypes.push(id);
                saw_head_noun = true;
                continue;
            }
        }

        break;
    }

    // Must have either a head noun OR at least one non-empty modifier (e.g., "nonland" before "permanents"
    // already handled, or "nontoken", "attacking", "legendary")
    if !saw_head_noun {
        // Allow standalone modifier-only phrases when they include non-empty type/supertype/color/flag exclusions
        let has_standalone_modifier = mod_count > 0
            && (!filter.types_none.is_empty()
                || !filter.supertypes_none.is_empty()
                || !filter.colors_none.is_empty()
                || !filter.subtypes_none.is_empty()
                || filter.flags.contains(FilterFlags::OTHER)
                || filter.flags.contains(FilterFlags::HISTORIC));
        if !has_standalone_modifier {
            ctx.interner.rollback(cp);
            return None;
        }
    }

    if head_types.len() == 1 {
        filter.types_any |= head_types[0];
    } else if head_types.len() >= 2 {
        for t in head_types {
            filter.types_all |= t;
        }
    }

    if head_subtypes.len() == 1 {
        filter.subtypes_any.push(head_subtypes[0]);
    } else if head_subtypes.len() >= 2 {
        for st in head_subtypes {
            if !filter.subtypes_all.contains(&st) {
                filter.subtypes_all.push(st);
            }
        }
    }

    let consumed_trail = parse_trailing_qualifiers(&tokens[cur.pos()..], &mut filter, ctx, scope);
    Some((filter, cur.pos() + consumed_trail))
}

fn parse_trailing_qualifiers(
    tokens: &[Token<'_>],
    filter: &mut ObjectFilter,
    ctx: &mut ParseCtx<'_>,
    scope: &dyn ParserScope,
) -> usize {
    let mut cur = TokenCursor::new(tokens);
    let mut seen_explicit_zone = false;
    let mut add_explicit_zone = |filter: &mut ObjectFilter, z: ZoneMask| {
        if !seen_explicit_zone {
            filter.zones = z;
            seen_explicit_zone = true;
        } else {
            filter.zones |= z;
        }
    };
    loop {
        if cur.is_empty() {
            break;
        }

        // 1. Zones
        if cur.eat_words(&["on", "the", "battlefield"]) {
            add_explicit_zone(filter, ZoneMask::BATTLEFIELD);
            continue;
        }
        if cur.eat_words(&["in", "exile"]) {
            add_explicit_zone(filter, ZoneMask::EXILE);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["exiled", "with"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            add_explicit_zone(filter, ZoneMask::EXILE);
            filter.extra.push(ObjectPredicate::ExiledWithSelf);
            continue;
        }
        if cur.eat_words(&["exiled", "by", "any", "player"]) {
            add_explicit_zone(filter, ZoneMask::EXILE);
            filter.extra.push(ObjectPredicate::ExiledByAnyPlayer);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["assimilated", "with"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::AssimilatedWithSelf);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["created", "with"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::CreatedWithSelf);
            continue;
        }
        if cur.eat_words(&["in", "your", "hand"]) || cur.eat_words(&["from", "your", "hand"]) {
            add_explicit_zone(filter, ZoneMask::HAND);
            filter.owner = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["in", "your", "graveyard"])
            || cur.eat_words(&["from", "your", "graveyard"])
        {
            add_explicit_zone(filter, ZoneMask::GRAVEYARD);
            filter.owner = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["in", "all", "graveyards"]) || cur.eat_words(&["in", "graveyards"]) {
            add_explicit_zone(filter, ZoneMask::GRAVEYARD);
            filter.owner = Some(RelativeControl::All);
            continue;
        }
        if cur.eat_words(&["in", "a", "graveyard"])
            || cur.eat_words(&["from", "a", "graveyard"])
            || cur.eat_words(&["from", "graveyards"])
        {
            add_explicit_zone(filter, ZoneMask::GRAVEYARD);
            continue;
        }
        if cur.eat_words(&["in", "an", "opponent's", "graveyard"])
            || cur.eat_words(&["in", "opponents'", "graveyards"])
        {
            add_explicit_zone(filter, ZoneMask::GRAVEYARD);
            filter.owner = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["in", "that", "player's", "graveyard"])
            || cur.eat_words(&["from", "that", "player's", "graveyard"])
            || cur.eat_words(&["in", "their", "graveyard"])
            || cur.eat_words(&["from", "their", "graveyard"])
        {
            let pref = scope.last_player_ref().unwrap_or_else(|| {
                if scope.is_each_loop_player() {
                    PlayerRef::EachLoopPlayer
                } else if !scope.in_trigger_context()
                    && scope
                        .get_target(0)
                        .is_some_and(|t| t.kind == TargetKind::Player)
                {
                    PlayerRef::Target { index: 0 }
                } else {
                    PlayerRef::EventPlayer
                }
            });
            add_explicit_zone(filter, ZoneMask::GRAVEYARD);
            filter
                .extra
                .push(ObjectPredicate::OwnerRef { player: pref });
            continue;
        }
        if cur.eat_words(&["from", "anywhere", "other", "than", "your", "hand"])
            || cur.eat_words(&["cast", "from", "anywhere", "other", "than", "your", "hand"])
        {
            filter.flags |= FilterFlags::CAST_OUTSIDE_HAND;
            continue;
        }
        if cur.eat_words(&["cast", "from", "your", "graveyard"]) {
            filter.extra.push(ObjectPredicate::CastFromZones {
                zones: ZoneMask::GRAVEYARD,
            });
            filter.owner = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["cast", "from", "exile"]) {
            filter.extra.push(ObjectPredicate::CastFromZones {
                zones: ZoneMask::EXILE,
            });
            continue;
        }

        // 2. Controller / Owner
        if cur.eat_words(&["you", "control", "but", "don't", "own"]) {
            filter.extra.push(ObjectPredicate::ControllerAndNotOwner {
                relation: RelativeControl::You,
            });
            continue;
        }
        if cur.eat_words(&["you", "control"]) {
            filter.controller = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["an", "opponent", "controls"])
            || cur.eat_words(&["you", "don't", "control"])
        {
            filter.controller = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["your", "opponents", "control"])
            || cur.eat_words(&["opponents", "control"])
        {
            filter.controller = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["attacking", "you"]) {
            filter.flags |= FilterFlags::ATTACKING;
            filter.controller = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["that's", "attacking", "one", "of", "your", "opponents"])
            || cur.eat_words(&["attacking", "one", "of", "your", "opponents"])
            || cur.eat_words(&["that's", "attacking", "an", "opponent"])
            || cur.eat_words(&["attacking", "an", "opponent"])
        {
            filter.flags |= FilterFlags::ATTACKING;
            continue;
        }
        if cur.eat_words(&["you", "own"]) || cur.eat_words(&["owned", "by", "you"]) {
            filter.owner = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["you", "don't", "own"])
            || cur.eat_words(&["owned", "by", "another", "player"])
            || cur.eat_words(&["your", "opponents", "own"])
            || cur.eat_words(&["your", "opponent", "owns"])
            || cur.eat_words(&["an", "opponent", "owns"])
            || cur.eat_words(&["opponents", "own"])
            || cur.eat_words(&["owned", "by", "an", "opponent"])
            || cur.eat_words(&["owned", "by", "your", "opponents"])
            || cur.eat_words(&["owned", "by", "opponents"])
        {
            filter.owner = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["controlled", "by", "the", "chosen", "player"]) {
            filter.extra.push(ObjectPredicate::ControlledByChosenPlayer);
            continue;
        }
        if cur.eat_words(&["enchanted", "player", "controls"])
            || cur.eat_words(&["controlled", "by", "enchanted", "player"])
        {
            filter
                .extra
                .push(ObjectPredicate::ControlledByEnchantedPlayer);
            continue;
        }
        if cur.eat_words(&["exiled", "this", "way"]) {
            filter.zones = ZoneMask::EXILE;
            continue;
        }
        if cur.eat_words(&["defending", "player", "controls"]) {
            filter.extra.push(ObjectPredicate::ControllerRef {
                player: PlayerRef::DefendingPlayer,
            });
            continue;
        }
        if cur.eat_words(&["active", "player", "controls"]) {
            filter.extra.push(ObjectPredicate::ControllerRef {
                player: PlayerRef::ActivePlayer,
            });
            continue;
        }
        if cur.eat_words(&[
            "that",
            "player",
            "or",
            "that",
            "planeswalker's",
            "controller",
            "controls",
        ]) {
            filter
                .extra
                .push(ObjectPredicate::ControllerOfTarget { index: 0 });
            continue;
        }
        if cur.eat_words(&["that", "one", "of", "your", "opponents", "controls"])
            || cur.eat_words(&["one", "of", "your", "opponents", "controls"])
        {
            filter.controller = Some(RelativeControl::Opponent);
            continue;
        }
        if cur.eat_words(&["that", "player", "controls"])
            || cur.eat_words(&["that", "opponent", "controls"])
            || cur.eat_words(&["they", "control"])
        {
            let pref = scope.last_player_ref().unwrap_or_else(|| {
                if scope.is_each_loop_player() {
                    PlayerRef::EachLoopPlayer
                } else if !scope.in_trigger_context()
                    && scope
                        .get_target(0)
                        .is_some_and(|t| t.kind == TargetKind::Player)
                {
                    PlayerRef::Target { index: 0 }
                } else {
                    PlayerRef::EventPlayer
                }
            });
            filter
                .extra
                .push(ObjectPredicate::ControllerRef { player: pref });
            continue;
        }
        if cur.eat_words(&["that", "player", "owns"])
            || cur.eat_words(&["that", "opponent", "owns"])
            || cur.eat_words(&["they", "own"])
            || cur.eat_words(&["owned", "by", "that", "player"])
        {
            let pref = scope.last_player_ref().unwrap_or_else(|| {
                if scope.is_each_loop_player() {
                    PlayerRef::EachLoopPlayer
                } else if scope
                    .get_target(0)
                    .is_some_and(|t| t.kind == TargetKind::Player)
                {
                    PlayerRef::Target { index: 0 }
                } else {
                    PlayerRef::EventPlayer
                }
            });
            filter
                .extra
                .push(ObjectPredicate::OwnerRef { player: pref });
            continue;
        }
        if cur.eat_words(&["its", "controller", "controls"])
            || cur.eat_words(&["their", "controller", "controls"])
        {
            let target_ref = scope.it_ref().unwrap_or_else(|| {
                if scope.default_pronoun_subject() == PronounSubject::AttachedTo {
                    mtg_ir::Ref::AttachedTo
                } else {
                    mtg_ir::Ref::EventObject
                }
            });
            filter.extra.push(ObjectPredicate::ControllerRef {
                player: PlayerRef::ControllerOf {
                    of: Box::new(target_ref),
                },
            });
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["with", "the", "same", "owner", "as"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::SameOwnerAsSource);
            continue;
        }
        if cur.eat_words(&["controlled", "by", "that", "target's", "controller"]) {
            filter
                .extra
                .push(ObjectPredicate::ControllerOfTarget { index: 0 });
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&[
                    "whose",
                    "controller",
                    "was",
                    "dealt",
                    "combat",
                    "damage",
                    "by",
                ]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                let _ = c.eat_words(&["this", "turn"]);
                Some(())
            })
            .is_some()
        {
            filter
                .extra
                .push(ObjectPredicate::ControllerDealtCombatDamageBySelfThisTurn);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["exiled", "with"]) {
                    return None;
                }
                if c.peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef) || t.eq_ci("it"))
                {
                    c.bump();
                    return Some(());
                }
                if c.eat_ci("this")
                    && c.peek().is_some_and(|t| {
                        matches!(
                            t.raw.to_ascii_lowercase().as_str(),
                            "artifact"
                                | "creature"
                                | "enchantment"
                                | "planeswalker"
                                | "permanent"
                                | "card"
                                | "land"
                        )
                    })
                {
                    c.bump();
                    return Some(());
                }
                None
            })
            .is_some()
        {
            filter.zones = ZoneMask::EXILE;
            if !filter
                .extra
                .iter()
                .any(|p| matches!(p, ObjectPredicate::ExiledWithSelf))
            {
                filter.extra.push(ObjectPredicate::ExiledWithSelf);
            }
            continue;
        }

        // 3. Counters, Keywords, Adventure, {X}
        if cur.eat_words(&["with", "no", "counters", "on", "it"])
            || cur.eat_words(&["with", "no", "counters", "on", "them"])
            || cur.eat_words(&["with", "no", "counters"])
        {
            filter.flags |= FilterFlags::HAS_NO_COUNTER;
            continue;
        }
        if cur.eat_words(&["with", "a", "counter", "on", "it"])
            || cur.eat_words(&["with", "one", "or", "more", "counters", "on", "it"])
            || cur.eat_words(&["with", "one", "or", "more", "counters", "on", "them"])
            || cur.eat_words(&["with", "counters", "on", "it"])
            || cur.eat_words(&["with", "counters", "on", "them"])
        {
            filter.flags |= FilterFlags::HAS_ANY_COUNTER;
            continue;
        }
        if let Some(pred) = cur.try_parse(|c| {
            let cp = ctx.interner.checkpoint();
            let res = parse_counter_predicate(c, ctx);
            if res.is_none() {
                ctx.interner.rollback(cp);
            }
            res
        }) {
            filter.extra.push(pred);
            continue;
        }
        if cur.eat_words(&["with", "cycling", "abilities"]) || cur.eat_words(&["with", "cycling"]) {
            filter.flags |= FilterFlags::WITH_CYCLING_ABILITY;
            continue;
        }
        if cur.eat_words(&["with", "an", "adventure"]) {
            filter.flags |= FilterFlags::HAS_ADVENTURE;
            continue;
        }
        if cur.eat_words(&["with", "a", "basic", "land", "type"]) {
            let subs = ["Plains", "Island", "Swamp", "Mountain", "Forest"]
                .into_iter()
                .map(|s| ObjectFilter {
                    subtypes_any: smallvec![ctx.interner.intern_subtype(s)],
                    ..Default::default()
                })
                .collect();
            filter.extra.push(ObjectPredicate::AnyOf { filters: subs });
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["with"]) {
                    return None;
                }
                if !c.peek().is_some_and(|t| {
                    matches!(t.kind, TokenKind::ManaSymbol(ManaSymbolKind::Variable('X')))
                }) {
                    return None;
                }
                c.bump();
                if c.eat_words(&["in", "its", "mana", "cost"]) {
                    Some(())
                } else {
                    None
                }
            })
            .is_some()
        {
            filter.flags |= FilterFlags::HAS_X_IN_MANA_COST;
            continue;
        }
        if let Some(pred) = cur.try_parse(|c| {
            let cp = ctx.interner.checkpoint();
            let res = parse_keyword_predicate(c, ctx);
            if res.is_none() {
                ctx.interner.rollback(cp);
            }
            res
        }) {
            filter.extra.push(pred);
            continue;
        }

        // 4. Stats, Power/Toughness/MV Comparisons, Parity
        if let Some(()) = cur.try_parse(|c| {
            let _ = c.eat_ci("each");
            if !c.eat_ci("with") {
                return None;
            }
            let _ = c.eat_ci("a") || c.eat_ci("an");
            if c.eat_words(&["even", "mana", "value"]) {
                filter.extra.push(ObjectPredicate::ManaValueParity {
                    parity: ParityFilter::Even,
                });
                return Some(());
            }
            if c.eat_words(&["odd", "mana", "value"]) {
                filter.extra.push(ObjectPredicate::ManaValueParity {
                    parity: ParityFilter::Odd,
                });
                return Some(());
            }
            let mv_saved = c.pos();
            if c.eat_words(&["mana", "value", "equal", "to"]) {
                if c.peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                    && c.peek_n(1).is_some_and(|t| t.raw == "'s" || t.raw == "’s")
                    && c.peek_n_ci(2, "power")
                {
                    c.bump();
                    c.bump();
                    c.bump();
                    filter
                        .extra
                        .push(ObjectPredicate::ManaValueEqualsSourcePower);
                    return Some(());
                }
                if c.eat_words(&["the", "chosen", "number"]) {
                    filter
                        .extra
                        .push(ObjectPredicate::ManaValueEqualsChosenNumber);
                    return Some(());
                }
                c.restore(mv_saved);
            }
            if c.eat_words(&["equal", "or", "lesser", "mana", "value"]) {
                filter.mana_value = Some(Compare {
                    op: CompareOp::Le,
                    value: Box::new(Amount::Stat {
                        of: Ref::EventObject,
                        stat: StatKind::ManaValue,
                        lki: false,
                    }),
                });
                return Some(());
            }
            let mut scope_ref = ScopeRef(scope);
            if c.eat_words(&["base", "power", "or", "toughness"]) {
                let (cmp, used) = parse_compare(c.remaining(), ctx, &mut scope_ref)?;
                for _ in 0..used {
                    c.bump();
                }
                filter
                    .extra
                    .push(ObjectPredicate::BasePowerOrToughness { cmp });
                return Some(());
            }
            if c.eat_words(&["power", "or", "toughness"]) {
                let (cmp, used) = parse_compare(c.remaining(), ctx, &mut scope_ref)?;
                for _ in 0..used {
                    c.bump();
                }
                filter.extra.push(ObjectPredicate::PowerOrToughness { cmp });
                return Some(());
            }
            if c.eat_words(&["mana", "value"]) {
                if c.eat_words(&[
                    "less",
                    "than",
                    "or",
                    "equal",
                    "to",
                    "the",
                    "number",
                    "of",
                    "counters",
                    "among",
                    "permanents",
                    "you",
                    "control",
                ]) {
                    filter
                        .extra
                        .push(ObjectPredicate::ManaValueLteTotalCounters);
                    return Some(());
                }
                if c.eat_words(&["equal", "to", "that", "spell's", "mana", "value"])
                    || c.eat_words(&["equal", "to", "that", "card's", "mana", "value"])
                {
                    let of_ref = scope.it_ref().unwrap_or(Ref::EventObject);
                    filter.mana_value = Some(Compare {
                        op: CompareOp::Eq,
                        value: Box::new(Amount::Stat {
                            of: of_ref,
                            stat: StatKind::ManaValue,
                            lki: false,
                        }),
                    });
                    return Some(());
                }
                if c.eat_words(&["less", "than", "the", "revealed", "card's", "mana", "value"]) {
                    filter
                        .extra
                        .push(ObjectPredicate::ManaValueLtRevealedCostCard);
                    return Some(());
                }
                if c.eat_words(&["less", "than", "or", "equal", "to", "that", "damage"]) {
                    filter.extra.push(ObjectPredicate::ManaValueLteEventAmount);
                    return Some(());
                }
                if let Some((n1, n2)) = c.try_parse(|tc| {
                    let t1 = tc.bump()?;
                    let v1 = parse_small_number(t1.raw)?;
                    if !tc.eat_ci("or") {
                        return None;
                    }
                    let t2 = tc.bump()?;
                    let v2 = parse_small_number(t2.raw)?;
                    Some((v1, v2))
                }) {
                    filter.extra.push(ObjectPredicate::AnyOf {
                        filters: vec![
                            ObjectFilter {
                                mana_value: Some(Compare {
                                    op: CompareOp::Eq,
                                    value: Box::new(Amount::Const { n: n1 }),
                                }),
                                ..Default::default()
                            },
                            ObjectFilter {
                                mana_value: Some(Compare {
                                    op: CompareOp::Eq,
                                    value: Box::new(Amount::Const { n: n2 }),
                                }),
                                ..Default::default()
                            },
                        ],
                    });
                    return Some(());
                }
                let (cmp, used) = parse_compare(c.remaining(), ctx, &mut scope_ref)?;
                for _ in 0..used {
                    c.bump();
                }
                filter.mana_value = Some(cmp);
                return Some(());
            }
            if c.eat_ci("power") {
                if c.eat_words(&["greater", "than", "its", "base", "power"]) {
                    filter.extra.push(ObjectPredicate::PowerGreaterThanBase);
                    return Some(());
                }
                if c.eat_words(&["equal", "to", "its", "toughness"]) {
                    filter.extra.push(ObjectPredicate::PowerEqualsToughness);
                    return Some(());
                }
                let (cmp, used) = parse_compare(c.remaining(), ctx, &mut scope_ref)?;
                for _ in 0..used {
                    c.bump();
                }
                filter.power = Some(cmp);
                return Some(());
            }
            if c.eat_ci("toughness") {
                if c.eat_words(&["greater", "than", "its", "power"]) {
                    filter
                        .extra
                        .push(ObjectPredicate::ToughnessGreaterThanPower);
                    return Some(());
                }
                let (cmp, used) = parse_compare(c.remaining(), ctx, &mut scope_ref)?;
                for _ in 0..used {
                    c.bump();
                }
                filter.toughness = Some(cmp);
                return Some(());
            }
            None
        }) {
            continue;
        }

        // 5. Names, Chosen Qualities & Sharing
        if let Some(pred) = cur.try_parse(|c| {
            if !c.eat_words(&["named"]) && !c.eat_words(&["with", "the", "same", "name", "as"]) {
                return None;
            }
            if c.peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            {
                c.bump();
                return Some(ObjectPredicate::SameNameAsSource);
            }
            if c.eat_ci("that") && c.peek().is_some_and(|t| is_self_noun(t.raw)) {
                c.bump();
                let idx = match scope.it_ref() {
                    Some(Ref::Target { index }) => index,
                    _ => 0,
                };
                return Some(ObjectPredicate::SameNameAsTarget { index: idx });
            }
            let name_tokens = collect_name_tokens(c);
            if !name_tokens.is_empty() {
                let name = reconstruct_tokens_text(name_tokens);
                return Some(ObjectPredicate::Named { name });
            }
            None
        }) {
            filter.extra.push(pred);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["not", "named"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::NotSameNameAsSource);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["other", "than"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.flags |= FilterFlags::OTHER;
            filter.extra.push(ObjectPredicate::NotSelf);
            continue;
        }
        if cur.eat_words(&["of", "the", "chosen", "type"])
            || cur.eat_words(&["of", "that", "type"])
            || cur.eat_words(&["of", "a", "type", "chosen", "this", "way"])
        {
            filter.extra.push(ObjectPredicate::OfChosenType);
            continue;
        }
        if cur.eat_words(&["not", "of", "the", "chosen", "type"])
            || cur.eat_words(&["that", "aren't", "of", "the", "chosen", "type"])
            || cur.eat_words(&["that", "isn't", "of", "the", "chosen", "type"])
        {
            filter.extra.push(ObjectPredicate::NotOfChosenType);
            continue;
        }
        if cur.eat_words(&["of", "the", "chosen", "color"]) {
            filter.extra.push(ObjectPredicate::OfChosenColor);
            continue;
        }
        if cur.eat_words(&["of", "the", "chosen", "name"]) {
            filter.extra.push(ObjectPredicate::OfChosenName);
            continue;
        }
        if let Some(pred) = cur.try_parse(|c| {
            if !c.eat_words(&["that", "shares", "a", "color", "with"])
                && !c.eat_words(&["that", "share", "a", "color", "with"])
            {
                return None;
            }
            if c.peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            {
                c.bump();
                return Some(ObjectPredicate::SharesColorWithSelf);
            }
            if c.eat_words(&["that", "spell"]) {
                return Some(ObjectPredicate::SharesColorWithSpell);
            }
            None
        }) {
            filter.extra.push(pred);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["that", "shares", "a", "creature", "type", "with"])
                    && !c.eat_words(&["that", "share", "a", "creature", "type", "with"])
                {
                    return None;
                }
                if c.eat_words(&["the", "sacrificed", "creature"])
                    || c.eat_words(&["that", "creature"])
                {
                    return Some(());
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef) || t.eq_ci("it"))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter
                .extra
                .push(ObjectPredicate::SharesCreatureTypeWithSelf);
            continue;
        }
        if cur.eat_words(&[
            "that",
            "shares",
            "a",
            "card",
            "type",
            "with",
            "your",
            "commander",
        ]) || cur.eat_words(&[
            "that",
            "shares",
            "a",
            "creature",
            "type",
            "with",
            "your",
            "commander",
        ]) {
            filter.extra.push(ObjectPredicate::SharesTypeWithCommander);
            continue;
        }

        // 6. State / Combat / History / Attachment
        if cur.eat_words(&["that", "entered", "the", "battlefield", "this", "turn"])
            || cur.eat_words(&["that", "entered", "this", "turn"])
        {
            filter.flags |= FilterFlags::ENTERED_THIS_TURN;
            continue;
        }
        if cur.eat_words(&[
            "that",
            "didn't",
            "enter",
            "the",
            "battlefield",
            "this",
            "turn",
        ]) || cur.eat_words(&["that", "didn't", "enter", "this", "turn"])
        {
            filter.flags |= FilterFlags::NOT_ENTERED_THIS_TURN;
            continue;
        }
        if cur.eat_words(&["that", "was", "dealt", "damage", "this", "turn"])
            || cur.eat_words(&["that", "were", "dealt", "damage", "this", "turn"])
        {
            filter.flags |= FilterFlags::DEALT_DAMAGE_THIS_TURN;
            continue;
        }
        if cur.eat_words(&["that", "wasn't", "cast"]) {
            filter.flags |= FilterFlags::WAS_NOT_CAST;
            continue;
        }
        if let Some(pred) = cur.try_parse(|c| {
            if !c.eat_words(&["attached", "to"]) {
                return None;
            }
            if c.peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            {
                c.bump();
                return Some(ObjectPredicate::AttachedToRef {
                    host: AttachedHostRef::SelfObj,
                });
            }
            if c.eat_ci("it")
                || c.eat_words(&["enchanted", "creature"])
                || c.eat_words(&["equipped", "creature"])
            {
                return Some(ObjectPredicate::AttachedToRef {
                    host: AttachedHostRef::AttachedTo,
                });
            }
            None
        }) {
            filter.extra.push(pred);
            continue;
        }
        if cur.eat_words(&["with", "another", "aura", "attached", "to", "it"])
            || cur.eat_words(&["with", "another", "aura", "attached"])
        {
            filter.extra.push(ObjectPredicate::HasAnotherAuraAttached);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["paired", "with"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::PairedWithSource);
            continue;
        }
        if cur.eat_words(&["with", "a", "single", "target"]) {
            filter.extra.push(ObjectPredicate::WithSingleTarget);
            continue;
        }
        if cur
            .try_parse(|c| {
                if !c.eat_words(&["that", "targets"]) {
                    return None;
                }
                if !c
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                {
                    return None;
                }
                c.bump();
                Some(())
            })
            .is_some()
        {
            filter.extra.push(ObjectPredicate::TargetsSelf);
            continue;
        }
        if cur.eat_words(&["that's", "tapped"]) || cur.eat_words(&["that", "are", "tapped"]) {
            filter.flags |= FilterFlags::TAPPED;
            continue;
        }
        if cur.eat_words(&["that's", "untapped"]) || cur.eat_words(&["that", "are", "untapped"]) {
            filter.flags |= FilterFlags::UNTAPPED;
            continue;
        }
        if cur.eat_words(&["that's", "attacking"]) || cur.eat_words(&["that", "are", "attacking"]) {
            filter.flags |= FilterFlags::ATTACKING;
            continue;
        }
        if cur.eat_words(&["that's", "colorless"]) || cur.eat_words(&["that", "are", "colorless"]) {
            filter.flags |= FilterFlags::IS_COLORLESS;
            continue;
        }
        if cur.eat_words(&["that", "are", "one", "or", "more", "colors"])
            || cur.eat_words(&["that", "is", "one", "or", "more", "colors"])
            || cur.eat_words(&["that's", "one", "or", "more", "colors"])
            || cur.eat_words(&["that", "aren't", "colorless"])
            || cur.eat_words(&["that", "isn't", "colorless"])
        {
            filter.flags |= FilterFlags::IS_COLORED;
            continue;
        }
        if cur.eat_words(&["with", "different", "controllers"]) {
            filter.extra.push(ObjectPredicate::DifferentControllers);
            continue;
        }
        if cur.eat_words(&[
            "put",
            "into",
            "your",
            "graveyard",
            "from",
            "the",
            "battlefield",
            "this",
            "turn",
        ]) || cur.eat_words(&[
            "that",
            "were",
            "put",
            "there",
            "from",
            "the",
            "battlefield",
            "this",
            "turn",
        ]) || cur.eat_words(&["put", "there", "from", "the", "battlefield", "this", "turn"])
        {
            filter
                .extra
                .push(ObjectPredicate::EnteredGraveyardFromBattlefieldThisTurn);
            continue;
        }

        // "that's <subtype>" / "except for <subtypes>"
        if let Some(()) = cur.try_parse(|c| {
            let cp = ctx.interner.checkpoint();
            if !c.eat_words(&["except", "for"]) && !c.eat_ci("except") {
                return None;
            }
            let Some(first_sub) = c.peek().and_then(|tok| {
                is_capitalized_subtype_token(tok)
                    .then(|| singular_subtype(tok.raw))
                    .flatten()
            }) else {
                ctx.interner.rollback(cp);
                return None;
            };
            c.bump();
            let first_id = ctx.interner.intern_subtype(&first_sub);
            if !filter.subtypes_none.contains(&first_id) {
                filter.subtypes_none.push(first_id);
            }
            while let Some(next_sub) = c.try_parse(|tc| {
                let sep_ok = if tc.eat_comma_or_and_or() {
                    let _ = tc.eat_ci("and") || tc.eat_ci("or");
                    true
                } else {
                    tc.eat_ci("and") || tc.eat_ci("or")
                };
                if !sep_ok {
                    return None;
                }
                let tok = tc.peek()?;
                if is_capitalized_subtype_token(tok)
                    && let Some(sub) = singular_subtype(tok.raw)
                {
                    tc.bump();
                    Some(sub)
                } else {
                    None
                }
            }) {
                let id = ctx.interner.intern_subtype(&next_sub);
                if !filter.subtypes_none.contains(&id) {
                    filter.subtypes_none.push(id);
                }
            }
            Some(())
        }) {
            continue;
        }
        if let Some(()) = cur.try_parse(|c| {
            let cp = ctx.interner.checkpoint();
            let neg = if c.eat_words(&["that", "isn't"])
                || c.eat_words(&["that", "is", "not"])
                || c.eat_words(&["that", "aren't"])
                || c.eat_words(&["that", "are", "not"])
            {
                true
            } else if c.eat_words(&["that's"])
                || c.eat_words(&["that", "is"])
                || c.eat_words(&["that", "are"])
            {
                false
            } else {
                return None;
            };
            let _ = c.eat_ci("a") || c.eat_ci("an");
            let Some(first_sub) = c.peek().and_then(|tok| {
                is_capitalized_subtype_token(tok)
                    .then(|| singular_subtype(tok.raw))
                    .flatten()
            }) else {
                ctx.interner.rollback(cp);
                return None;
            };
            c.bump();
            let mut ids = vec![ctx.interner.intern_subtype(&first_sub)];
            while let Some(next_sub) = c.try_parse(|tc| {
                let sep_ok = if tc.eat_comma_or_and_or() {
                    let _ = tc.eat_ci("and") || tc.eat_ci("or");
                    true
                } else {
                    tc.eat_ci("and") || tc.eat_ci("or")
                };
                if !sep_ok {
                    return None;
                }
                let _ = tc.eat_ci("a") || tc.eat_ci("an");
                let tok = tc.peek()?;
                if is_capitalized_subtype_token(tok)
                    && let Some(sub) = singular_subtype(tok.raw)
                {
                    tc.bump();
                    Some(sub)
                } else {
                    None
                }
            }) {
                ids.push(ctx.interner.intern_subtype(&next_sub));
            }
            if neg {
                for id in ids {
                    if !filter.subtypes_none.contains(&id) {
                        filter.subtypes_none.push(id);
                    }
                }
            } else {
                for id in ids {
                    if !filter.subtypes_any.contains(&id) {
                        filter.subtypes_any.push(id);
                    }
                }
            }
            Some(())
        }) {
            continue;
        }

        break;
    }
    cur.pos()
}

#[inline]
fn is_name_stop_word(t: &Token<'_>) -> bool {
    t.eq_ci("from")
        || t.eq_ci("in")
        || t.eq_ci("on")
        || t.eq_ci("you")
        || t.eq_ci("with")
        || t.eq_ci("or")
}

#[inline]
fn is_cap_name_word(t: &Token<'_>) -> bool {
    matches!(t.kind, TokenKind::Word(_))
        && t.raw.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !is_name_stop_word(t)
}

/// Returns the token count of a valid lowercase connector bridge ending in a
/// capitalized non-stop word (`is_cap_name_word`), guaranteeing that connectors
/// are never consumed without their target capitalized word.
fn connector_bridge_len(slice: &[Token<'_>], allow_and: bool) -> Option<usize> {
    let t0 = slice.first()?;
    if !matches!(t0.kind, TokenKind::Word(_)) {
        return None;
    }
    let is_single_conn =
        matches!(t0.raw, "of" | "the" | "to" | "a" | "an" | "at") || (allow_and && t0.raw == "and");
    if is_single_conn && slice.get(1).is_some_and(is_cap_name_word) {
        return Some(2);
    }
    let is_the_bridge = (matches!(t0.raw, "of" | "to" | "at") || (allow_and && t0.raw == "and"))
        && slice
            .get(1)
            .is_some_and(|t1| matches!(t1.kind, TokenKind::Word(_)) && t1.raw == "the")
        && slice.get(2).is_some_and(is_cap_name_word);
    if is_the_bridge {
        return Some(3);
    }
    if t0.raw == "of"
        && slice
            .get(1)
            .is_some_and(|t1| matches!(t1.kind, TokenKind::Word(_)) && matches!(t1.raw, "a" | "an"))
        && slice.get(2).is_some_and(is_cap_name_word)
    {
        return Some(3);
    }
    None
}

pub(super) fn collect_name_tokens<'a>(cur: &mut TokenCursor<'a>) -> &'a [Token<'a>] {
    let rem = cur.remaining();
    let mut len = 0usize;
    let mut seen_comma = false;
    while len < rem.len() {
        let t = &rem[len];
        if t.is_punct(Punct::Period)
            || t.is_punct(Punct::Colon)
            || t.is_punct(Punct::Semicolon)
            || is_name_stop_word(t)
        {
            break;
        }
        if t.is_punct(Punct::Comma) {
            if len > 0 && !seen_comma {
                let tail = &rem[len + 1..];
                if tail.first().is_some_and(is_cap_name_word) {
                    seen_comma = true;
                    len += 2;
                    continue;
                }
                if let Some(bridge) = connector_bridge_len(tail, false) {
                    seen_comma = true;
                    len += 1 + bridge;
                    continue;
                }
            }
            break;
        }
        if is_cap_name_word(t) {
            len += 1;
            continue;
        }
        if len > 0
            && let Some(bridge) = connector_bridge_len(&rem[len..], true)
        {
            len += bridge;
            continue;
        }
        break;
    }
    for _ in 0..len {
        cur.bump();
    }
    &rem[..len]
}

fn parse_counter_predicate(
    cur: &mut TokenCursor<'_>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ObjectPredicate> {
    let is_with = cur.eat_ci("with");
    let is_without = !is_with && cur.eat_ci("without");
    if !is_with && !is_without {
        return None;
    }
    let _ = cur.eat_ci("a") || cur.eat_ci("an") || cur.eat_words(&["one", "or", "more"]);
    let mut parts: Vec<&str> = Vec::new();
    let mut saw_counter_word = false;
    while let Some(tok) = cur.peek() {
        if tok.eq_ci("counter") || tok.eq_ci("counters") {
            cur.bump();
            saw_counter_word = true;
            break;
        }
        if parts.len() >= 2 {
            return None;
        }
        parts.push(tok.raw);
        cur.bump();
    }
    if !saw_counter_word || parts.is_empty() {
        return None;
    }
    let _ = cur.eat_words(&["on", "it"]) || cur.eat_words(&["on", "them"]);
    let raw_name = parts.join(" ");
    let norm = normalize_counter_name(&raw_name)?;
    let cid = ctx.interner.intern_counter(&norm);
    let ct = CounterTarget::Specific(cid);
    if is_with {
        Some(ObjectPredicate::HasCounter { counter: ct })
    } else {
        Some(ObjectPredicate::LacksCounter { counter: ct })
    }
}

fn eat_one_filter_keyword(cur: &mut TokenCursor<'_>) -> Option<&'static str> {
    if cur.eat_words(&["first", "strike"]) {
        Some("first strike")
    } else if cur.eat_words(&["double", "strike"]) {
        Some("double strike")
    } else if let Some(tok) = cur.peek() {
        let low = tok.raw.to_ascii_lowercase();
        if let Some(&kw) = FILTER_KEYWORDS.iter().find(|&&k| k == low) {
            cur.bump();
            Some(kw)
        } else {
            None
        }
    } else {
        None
    }
}

fn parse_keyword_predicate(
    cur: &mut TokenCursor<'_>,
    ctx: &mut ParseCtx<'_>,
) -> Option<ObjectPredicate> {
    let is_with = cur.eat_ci("with");
    let is_without = !is_with && cur.eat_ci("without");
    if !is_with && !is_without {
        return None;
    }
    let first_kw = eat_one_filter_keyword(cur)?;
    let mut kws: SmallVec<[KeywordId; 2]> = SmallVec::new();
    kws.push(ctx.interner.intern_keyword(first_kw));
    while let Some(next_kw) = cur.try_parse(|tc| {
        let eat_plain_and = |c: &mut TokenCursor<'_>| -> bool {
            c.peek_ci("and")
                && !c.peek_n(1).is_some_and(|t| t.is_punct(Punct::Slash))
                && c.eat_ci("and")
        };
        let sep_ok = if tc.eat_punct(Punct::Comma) {
            let _ = (is_without && tc.eat_and_or()) || eat_plain_and(tc);
            true
        } else {
            (is_without && tc.eat_and_or()) || eat_plain_and(tc)
        };
        if !sep_ok {
            return None;
        }
        eat_one_filter_keyword(tc)
    }) {
        kws.push(ctx.interner.intern_keyword(next_kw));
    }
    if is_with {
        Some(ObjectPredicate::HasKeywords { keywords: kws })
    } else {
        Some(ObjectPredicate::LacksKeywords { keywords: kws })
    }
}

/// Converts an [`ObjectFilter`] into a [`PlainManaFilter`] for mana spending restrictions (CR 106.6).
pub fn object_filter_to_plain_mana_filter(f: &ObjectFilter) -> Option<PlainManaFilter> {
    let disallowed_flags = FilterFlags::OTHER
        | FilterFlags::TAPPED
        | FilterFlags::UNTAPPED
        | FilterFlags::ATTACKING
        | FilterFlags::BLOCKING
        | FilterFlags::UNBLOCKED_ATTACKER
        | FilterFlags::SUSPECTED
        | FilterFlags::WAS_NOT_CAST;
    if f.flags.intersects(disallowed_flags)
        || f.controller.is_some()
        || f.owner.is_some()
        || !f.zones.is_empty()
        || f.power.is_some()
        || f.toughness.is_some()
    {
        return None;
    }
    let mut of_chosen_type = false;
    let mut has_keywords: SmallVec<[KeywordId; 2]> = SmallVec::new();
    let mut any_of: Vec<PlainManaFilter> = Vec::new();
    let mut cast_from_zone: Option<Zone> = None;
    for pred in &f.extra {
        match pred {
            ObjectPredicate::OfChosenType => of_chosen_type = true,
            ObjectPredicate::HasKeywords { keywords } => {
                has_keywords.extend(keywords.iter().copied());
            }
            ObjectPredicate::AnyOf { filters } => {
                for sub in filters {
                    any_of.push(object_filter_to_plain_mana_filter(sub)?);
                }
            }
            ObjectPredicate::CastFromZones { zones } if cast_from_zone.is_none() => {
                cast_from_zone = if *zones == ZoneMask::GRAVEYARD {
                    Some(Zone::Graveyard)
                } else if *zones == ZoneMask::EXILE {
                    Some(Zone::Exile)
                } else if *zones == ZoneMask::HAND {
                    Some(Zone::Hand)
                } else {
                    return None;
                };
            }
            _ => return None,
        }
    }

    let (min_mv, max_mv) = match &f.mana_value {
        Some(Compare { op, value }) => {
            let mtg_ir::Amount::Const { n } = **value else {
                return None;
            };
            let n_u16 = u16::try_from(n).ok()?;
            match op {
                CompareOp::Ge => (Some(n_u16), None),
                CompareOp::Le => (None, Some(n_u16)),
                CompareOp::Eq => (Some(n_u16), Some(n_u16)),
                CompareOp::Gt => (Some(n_u16.saturating_add(1)), None),
                CompareOp::Lt => (None, Some(n_u16.saturating_sub(1))),
            }
        }
        None => (None, None),
    };

    let colorless =
        if f.colors_any.contains(ColorMask::C) || f.flags.contains(FilterFlags::IS_COLORLESS) {
            Some(true)
        } else {
            None
        };

    Some(PlainManaFilter {
        types_any: f.types_any,
        types_all: f.types_all,
        types_none: f.types_none,
        supertypes_any: f.supertypes_any,
        supertypes_none: f.supertypes_none,
        subtypes_any: f.subtypes_any.clone(),
        colors_any: f.colors_any & !ColorMask::C,
        colors_none: f.colors_none,
        multicolored: f.flags.contains(FilterFlags::MULTICOLORED).then_some(true),
        monocolored: f.flags.contains(FilterFlags::MONOCOLORED).then_some(true),
        colorless,
        min_mana_value: min_mv,
        max_mana_value: max_mv,
        has_keywords,
        cast_from_zone,
        of_chosen_type,
        any_of,
    })
}

pub fn parse_plain_mana_filter(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<(PlainManaFilter, usize)> {
    let opts = FilterOpts {
        default_zone: ZoneMask::empty(),
        allow_plural_and_disjunction: true,
        allow_spell_noun: true,
        strip_trailing_card: true,
        expand_outlaw_subtypes: false,
    };
    let (obj, used) = parse_object_filter(tokens, ctx, opts, &NullScope)?;
    let pmf = object_filter_to_plain_mana_filter(&obj)?;
    Some((pmf, used))
}

pub fn parse_player_filter(tokens: &[Token<'_>]) -> Option<(PlayerFilter, usize)> {
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_ci("you") {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["each", "opponent"]) {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::EachOpponent,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["each", "player"]) {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::EachPlayer,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["target", "opponent"]) {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::Target,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["target", "player"]) {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::Target,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["an", "opponent"]) || cur.eat_ci("opponent") {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::Opponent,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["any", "player"]) || cur.eat_words(&["a", "player"]) || cur.eat_ci("player")
    {
        return Some((
            PlayerFilter {
                relation: PlayerRelation::Any,
            },
            cur.pos(),
        ));
    }
    None
}

/// Parses a [`TargetSpec`] prefix from `tokens`.
fn parse_stack_target_qualifiers(
    cur: &mut TokenCursor<'_>,
    filter: &mut ObjectFilter,
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) {
    loop {
        if cur.eat_words(&["you", "control"]) {
            filter.controller = Some(RelativeControl::You);
            continue;
        }
        if cur.eat_words(&["an", "opponent", "controls"])
            || cur.eat_words(&["opponents", "control"])
            || cur.eat_words(&["you", "don't", "control"])
        {
            filter.controller = Some(RelativeControl::Opponent);
            continue;
        }
        if let Some(src_f) = cur.try_parse(|c| {
            if !c.eat_ci("from") {
                return None;
            }
            let _ = c.eat_ci("a") || c.eat_ci("an");
            let opts = FilterOpts {
                default_zone: ZoneMask::empty(),
                allow_plural_and_disjunction: true,
                allow_spell_noun: false,
                strip_trailing_card: false,
                expand_outlaw_subtypes: false,
            };
            let (f, used) = parse_object_filter(c.remaining(), ctx, opts, scope)?;
            for _ in 0..used {
                c.bump();
            }
            Some(f)
        }) {
            filter.extra.push(ObjectPredicate::AbilitySourceFilter {
                filter: Box::new(src_f),
            });
            continue;
        }
        if let Some(tgt_f) = cur.try_parse(|c| {
            if !c.eat_words(&["that", "targets"]) {
                return None;
            }
            let _ = c.eat_ci("a") || c.eat_ci("an");
            let opts = FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                allow_spell_noun: false,
                strip_trailing_card: false,
                expand_outlaw_subtypes: false,
            };
            let (f, used) = parse_object_filter(c.remaining(), ctx, opts, scope)?;
            for _ in 0..used {
                c.bump();
            }
            Some(f)
        }) {
            filter.extra.push(ObjectPredicate::TargetsFilter {
                filter: Box::new(tgt_f),
            });
            continue;
        }
        break;
    }
}

pub fn parse_target_spec(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(TargetSpec, usize)> {
    let mut cur = TokenCursor::new(tokens);

    // 1. "any target" / "any other target"
    if cur.eat_words(&["any", "other", "target"]) {
        return Some((
            TargetSpec {
                kind: TargetKind::ObjectOrPlayer,
                filter: ObjectFilter {
                    zones: ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE
                        | CardTypeMask::PLANESWALKER
                        | CardTypeMask::BATTLE,
                    flags: FilterFlags::OTHER,
                    ..Default::default()
                },
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Any,
                }),
                min: 1,
                max: 1,
                same_owner: false,
                condition: None,
                other_target: true,
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
                zones: ZoneMask::empty(),
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["any", "target"]) {
        return Some((
            TargetSpec {
                kind: TargetKind::ObjectOrPlayer,
                filter: ObjectFilter {
                    zones: ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE
                        | CardTypeMask::PLANESWALKER
                        | CardTypeMask::BATTLE,
                    ..Default::default()
                },
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Any,
                }),
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
                zones: ZoneMask::empty(),
            },
            cur.pos(),
        ));
    }

    // 2. Quantifier prefix
    let mut min: u16 = 1;
    let mut max: u16 = 1;
    let mut other_target = false;
    let mut dynamic_count = None;
    let mut dynamic_up_to = false;
    let mut is_one_two_three = false;

    if cur.eat_words(&["any", "number", "of"]) {
        min = 0;
        max = 99;
    } else if cur.eat_words(&["one", ",", "two", ",", "or", "three"])
        || cur.eat_words(&["one", "two", "or", "three"])
    {
        min = 1;
        max = 3;
        is_one_two_three = true;
    } else if cur.eat_words(&["one", "or", "more"]) {
        min = 1;
        max = 99;
    } else if cur.eat_words(&["one", "or", "two"]) {
        min = 1;
        max = 2;
    } else if cur.eat_words(&["up", "to"]) {
        min = 0;
        if cur.peek().is_some_and(|t| matches!(t.kind, TokenKind::X)) {
            cur.bump();
            max = 99;
            dynamic_count = Some(Box::new(mtg_ir::Amount::X));
            dynamic_up_to = true;
        } else if cur.eat_words(&["that", "many"]) {
            max = 99;
            let dc = scope.defined_x().unwrap_or_else(|| {
                if scope.has_sac_step_or_trigger() {
                    mtg_ir::Amount::CountedThisWay {
                        kind: mtg_ir::CountedThisWayKind::SacrificedThisWay,
                    }
                } else {
                    mtg_ir::Amount::CountedThisWay {
                        kind: mtg_ir::CountedThisWayKind::DiscardedThisWay,
                    }
                }
            });
            dynamic_count = Some(Box::new(dc));
            dynamic_up_to = true;
        } else {
            let n = cur.peek().and_then(|t| parse_small_number(t.raw))?;
            cur.bump();
            max = n as u16;
        }
    } else if cur.peek().is_some_and(|t| matches!(t.kind, TokenKind::X))
        && cur.peek_n_ci(1, "target")
    {
        cur.bump();
        min = 0;
        max = 99;
        dynamic_count = Some(Box::new(mtg_ir::Amount::X));
    } else if cur
        .peek()
        .is_some_and(|t| !t.eq_ci("a") && !t.eq_ci("an") && !t.eq_ci("one"))
        && let Some(n) = cur.peek().and_then(|t| parse_small_number(t.raw))
        && (cur.peek_n_ci(1, "target") || cur.peek_n_ci(1, "other"))
    {
        cur.bump();
        min = n as u16;
        max = n as u16;
    } else if (cur.eat_words(&["a", "second"])
        || cur.eat_words(&["a", "third"])
        || cur.eat_words(&["the", "second"])
        || cur.eat_words(&["the", "third"])
        || cur.eat_ci("second")
        || cur.eat_ci("third"))
        && cur.peek_ci("target")
    {
        other_target = true;
    }

    if cur.eat_ci("another") {
        other_target = true;
    }
    if (min == 0 || max > 1) && cur.eat_ci("targets") {
        return Some((
            TargetSpec {
                kind: TargetKind::ObjectOrPlayer,
                filter: ObjectFilter {
                    zones: ZoneMask::BATTLEFIELD,
                    types_any: CardTypeMask::CREATURE
                        | CardTypeMask::PLANESWALKER
                        | CardTypeMask::BATTLE,
                    ..Default::default()
                },
                player: Some(PlayerFilter {
                    relation: PlayerRelation::Any,
                }),
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
                different_controllers: false,
                chosen_by_event_player: false,
                shares_card_type_with_target: None,
                zones: ZoneMask::empty(),
            },
            cur.pos(),
        ));
    }
    if cur.eat_ci("other") {
        other_target = true;
    }
    if !cur.eat_ci("target") {
        return None;
    }
    if cur.eat_ci("other") {
        other_target = true;
    }

    // 3. Target kind & body
    let mut kind = TargetKind::Object;
    let mut player: Option<PlayerFilter> = None;
    let mut also_spells = false;
    let mut filter = ObjectFilter {
        zones: ZoneMask::BATTLEFIELD,
        ..Default::default()
    };
    let mut spec_zones = ZoneMask::empty();

    if cur.eat_words(&["spell", "or", "permanent"])
        || cur.eat_words(&["spell", "or", "nonland", "permanent"])
    {
        let nonland = tokens[..cur.pos()].iter().any(|t| t.eq_ci("nonland"));
        also_spells = true;
        filter.zones = ZoneMask::BATTLEFIELD;
        if nonland {
            filter.types_none |= CardTypeMask::LAND;
        }
    } else if cur.eat_words(&[
        "spell",
        ",",
        "nonland",
        "permanent",
        ",",
        "or",
        "card",
        "in",
        "a",
        "graveyard",
    ]) {
        also_spells = true;
        spec_zones = ZoneMask::BATTLEFIELD | ZoneMask::GRAVEYARD | ZoneMask::STACK;
        filter.zones = spec_zones;
        filter.types_none |= CardTypeMask::LAND;
    } else if cur.eat_words(&[
        "spell",
        ",",
        "permanent",
        ",",
        "or",
        "card",
        "in",
        "a",
        "graveyard",
    ]) {
        spec_zones = ZoneMask::BATTLEFIELD | ZoneMask::GRAVEYARD | ZoneMask::STACK;
        filter.zones = spec_zones;
    } else if cur.eat_words(&[
        "spell",
        ",",
        "activated",
        "ability",
        ",",
        "or",
        "triggered",
        "ability",
    ]) {
        filter.zones = ZoneMask::STACK;
        filter.stack_kind = Some(StackTargetKind::SpellOrAbility);
        parse_stack_target_qualifiers(&mut cur, &mut filter, ctx, scope);
    } else if cur.eat_words(&[
        "opponent",
        ",",
        "creature",
        "an",
        "opponent",
        "controls",
        ",",
        "or",
        "planeswalker",
        "an",
        "opponent",
        "controls",
    ]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.types_any |= CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER;
        filter.controller = Some(RelativeControl::Opponent);
        player = Some(PlayerFilter {
            relation: PlayerRelation::Opponent,
        });
    } else if cur.eat_words(&[
        "creatures",
        ",",
        "planeswalkers",
        ",",
        "and",
        "/",
        "or",
        "players",
    ]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.types_any |= CardTypeMask::CREATURE | CardTypeMask::PLANESWALKER;
        player = Some(PlayerFilter {
            relation: PlayerRelation::Any,
        });
    } else if cur.eat_words(&["creature", "or", "player"]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.types_any |= CardTypeMask::CREATURE;
        player = Some(PlayerFilter {
            relation: PlayerRelation::Any,
        });
    } else if cur.eat_words(&["player", "or", "planeswalker"]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.types_any |= CardTypeMask::PLANESWALKER;
        player = Some(PlayerFilter {
            relation: PlayerRelation::Any,
        });
    } else if cur.eat_words(&["permanent", "or", "opponent"]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.zones = ZoneMask::BATTLEFIELD;
        player = Some(PlayerFilter {
            relation: PlayerRelation::Opponent,
        });
    } else if cur.eat_words(&["opponent", "or", "planeswalker"]) {
        kind = TargetKind::ObjectOrPlayer;
        filter.types_any |= CardTypeMask::PLANESWALKER;
        player = Some(PlayerFilter {
            relation: PlayerRelation::Opponent,
        });
    } else if cur.eat_words(&["activated", "or", "triggered", "ability"]) {
        filter.zones = ZoneMask::STACK;
        filter.stack_kind = Some(StackTargetKind::Ability);
        parse_stack_target_qualifiers(&mut cur, &mut filter, ctx, scope);
    } else if cur.eat_words(&["activated", "ability"]) {
        filter.zones = ZoneMask::STACK;
        filter.stack_kind = Some(StackTargetKind::ActivatedAbility);
        parse_stack_target_qualifiers(&mut cur, &mut filter, ctx, scope);
    } else if cur.eat_words(&["triggered", "ability"]) {
        filter.zones = ZoneMask::STACK;
        filter.stack_kind = Some(StackTargetKind::TriggeredAbility);
        parse_stack_target_qualifiers(&mut cur, &mut filter, ctx, scope);
    } else if cur.eat_words(&["spell", "or", "ability"]) {
        filter.zones = ZoneMask::STACK;
        filter.stack_kind = Some(StackTargetKind::SpellOrAbility);
        parse_stack_target_qualifiers(&mut cur, &mut filter, ctx, scope);
    } else if cur.eat_words(&["permanent", "or", "suspended", "card"]) {
        spec_zones = ZoneMask::BATTLEFIELD | ZoneMask::EXILE;
        filter.zones = spec_zones;
    } else if cur.eat_ci("player") || cur.eat_ci("players") {
        kind = TargetKind::Player;
        filter = ObjectFilter::default();
        player = Some(PlayerFilter {
            relation: PlayerRelation::Any,
        });
    } else if cur.eat_ci("opponent") || cur.eat_ci("opponents") {
        kind = TargetKind::Player;
        filter = ObjectFilter::default();
        player = Some(PlayerFilter {
            relation: PlayerRelation::Opponent,
        });
    } else {
        let obj_opts = FilterOpts {
            default_zone: ZoneMask::BATTLEFIELD,
            allow_plural_and_disjunction: true,
            allow_spell_noun: false,
            strip_trailing_card: false,
            expand_outlaw_subtypes: false,
        };
        let (mut parsed_f, used) = parse_object_filter(&tokens[cur.pos()..], ctx, obj_opts, scope)?;
        for _ in 0..used {
            cur.bump();
        }
        if parsed_f.zones == ZoneMask::STACK {
            parse_stack_target_qualifiers(&mut cur, &mut parsed_f, ctx, scope);
            if parsed_f.stack_kind.is_none()
                && parsed_f.types_any.is_empty()
                && parsed_f.types_all.is_empty()
                && parsed_f.types_none.is_empty()
            {
                parsed_f.stack_kind = Some(StackTargetKind::Spell);
            }
        }
        filter = parsed_f;
    }

    if other_target && scope.prior_targets_len() == 0 && kind == TargetKind::Object {
        filter.flags |= FilterFlags::OTHER;
        other_target = false;
    }

    // Trailing TargetSpec modifiers
    let mut same_owner = false;
    let mut with_single_target = false;
    let mut max_total_mana_value = None;
    let mut per_opponent = false;
    let mut different_controllers = false;
    let mut chosen_by_event_player = false;
    let mut condition = None;

    if let Some(pos) = filter
        .extra
        .iter()
        .position(|p| matches!(p, ObjectPredicate::DifferentControllers))
    {
        filter.extra.remove(pos);
        different_controllers = true;
    }

    loop {
        if cur.eat_words(&["for", "each", "opponent"]) {
            per_opponent = true;
            min = 0;
            max = 99;
            continue;
        }
        if cur.eat_words(&["with", "different", "controllers"]) {
            different_controllers = true;
            continue;
        }
        if cur.eat_words(&["with", "a", "single", "target"]) {
            with_single_target = true;
            continue;
        }
        if cur.eat_words(&["of", "that", "player's", "choice"]) {
            chosen_by_event_player = true;
            let trail = parse_trailing_qualifiers(&tokens[cur.pos()..], &mut filter, ctx, scope);
            for _ in 0..trail {
                cur.bump();
            }
            continue;
        }
        if cur.eat_words(&["from", "the", "same", "graveyard"])
            || cur.eat_words(&["in", "the", "same", "graveyard"])
            || cur.eat_words(&["from", "a", "single", "graveyard"])
            || cur.eat_words(&["in", "a", "single", "graveyard"])
        {
            same_owner = true;
            filter.zones = ZoneMask::GRAVEYARD;
            continue;
        }
        if let Some(mv_cap) = cur.try_parse(|c| {
            if !c.eat_words(&["with", "total", "mana", "value"]) {
                return None;
            }
            let tok = c.bump()?;
            let mv_val = if matches!(tok.kind, TokenKind::X) {
                99u16
            } else {
                parse_small_number(tok.raw)? as u16
            };
            if !c.eat_words(&["or", "less"]) {
                return None;
            }
            Some(mv_val)
        }) {
            max_total_mana_value = Some(mv_cap);
            let trail = parse_trailing_qualifiers(&tokens[cur.pos()..], &mut filter, ctx, scope);
            for _ in 0..trail {
                cur.bump();
            }
            continue;
        }
        if let Some(cond) = cur.try_parse(|c| {
            let cp = ctx.interner.checkpoint();
            if !c.eat_ci("if") {
                return None;
            }
            let Some((cond, used)) = parse_condition(c.remaining(), ctx, scope) else {
                ctx.interner.rollback(cp);
                return None;
            };
            for _ in 0..used {
                c.bump();
            }
            Some(cond)
        }) {
            condition = Some(Box::new(cond));
            continue;
        }
        break;
    }

    if !same_owner
        && filter.zones == ZoneMask::GRAVEYARD
        && (max > 1 || dynamic_count.is_some())
        && !is_one_two_three
        && tokens[..cur.pos()].iter().any(|t| t.eq_ci("graveyard"))
        && !tokens[..cur.pos()].iter().any(|t| t.eq_ci("graveyards"))
    {
        same_owner = true;
    }

    Some((
        TargetSpec {
            kind,
            filter,
            player,
            min,
            max,
            same_owner,
            condition,
            other_target,
            divide_total: None,
            with_single_target,
            dynamic_count,
            dynamic_up_to,
            max_total_mana_value,
            also_spells,
            per_opponent,
            different_controllers,
            chosen_by_event_player,
            shares_card_type_with_target: None,
            zones: spec_zones,
        },
        cur.pos(),
    ))
}

/// Parses `"<amt> damage/counters divided as you choose among <target_spec>"`.
pub fn parse_divided_targets(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(TargetSpec, usize)> {
    let mut cur = TokenCursor::new(tokens);
    let (amt, used_amt) = parse_amount(tokens, ctx, scope)?;
    for _ in 0..used_amt {
        cur.bump();
    }
    if !(cur.eat_words(&["divided", "as", "you", "choose", "among"])
        || cur.eat_words(&["damage", "divided", "as", "you", "choose", "among"])
        || cur.eat_words(&["counters", "divided", "as", "you", "choose", "among"]))
    {
        return None;
    }
    let (mut spec, used_spec) = parse_target_spec(&tokens[cur.pos()..], ctx, scope)?;
    spec.divide_total = Some(Box::new(amt));
    Some((spec, cur.pos() + used_spec))
}

#[allow(dead_code)]
fn zone_from_word(w: &str) -> Option<Zone> {
    if w.eq_ignore_ascii_case("library") {
        Some(Zone::Library)
    } else if w.eq_ignore_ascii_case("hand") {
        Some(Zone::Hand)
    } else if w.eq_ignore_ascii_case("battlefield") {
        Some(Zone::Battlefield)
    } else if w.eq_ignore_ascii_case("graveyard") || w.eq_ignore_ascii_case("graveyards") {
        Some(Zone::Graveyard)
    } else if w.eq_ignore_ascii_case("exile") {
        Some(Zone::Exile)
    } else if w.eq_ignore_ascii_case("command") {
        Some(Zone::Command)
    } else if w.eq_ignore_ascii_case("stack") {
        Some(Zone::Stack)
    } else {
        None
    }
}

#[allow(dead_code)]
pub(crate) fn parse_mana_color_word(w: &str) -> Option<ManaColor> {
    if w.eq_ignore_ascii_case("white") {
        Some(ManaColor::W)
    } else if w.eq_ignore_ascii_case("blue") {
        Some(ManaColor::U)
    } else if w.eq_ignore_ascii_case("black") {
        Some(ManaColor::B)
    } else if w.eq_ignore_ascii_case("red") {
        Some(ManaColor::R)
    } else if w.eq_ignore_ascii_case("green") {
        Some(ManaColor::G)
    } else if w.eq_ignore_ascii_case("colorless") {
        Some(ManaColor::C)
    } else {
        None
    }
}
