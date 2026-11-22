use mtg_ir::{
    Affected, AllowAbilitiesRestriction, AllowSpellsRestriction, Amount, CardTypeMask, ColorMask,
    ColoredCount, CompareOp, Condition, CostDef, CostDelta, CostDeltaMode, CostObjectTarget,
    CostPaidCountKind, CounterId, CounterTarget, FilterFlags, FirstActivationZeroKind, KeywordId,
    LifeCompareWho, LoyaltySign, ManaColor, ManaCost, ManaSourceRestrictionKind, ManaSpendScope,
    ManaSpendingRestriction, MinColoredManaSymbols, ObjectFilter, ObjectPredicate, PlainManaFilter,
    PlayerFilter, PlayerRef, PlayerRelation, Ref, RelativeControl, Span, SpellOrAbilityFilter,
    StackTargetKind, StatKind, StaticBody, StaticDef, Zone, ZoneMask, ZoneScopeWhose,
};
use smallvec::SmallVec;

use crate::intern::{Interner, normalize_counter_name};
use crate::lexer::{ManaSymbolKind, Punct, Token, TokenCursor, TokenKind, tokenize};
use crate::parser::ParseCtx;
use crate::parser::filter::{
    CursorExt, FilterOpts, NullScope, OUTLAW_SUBTYPES, collect_name_tokens,
    object_filter_to_plain_mana_filter, parse_color_word, parse_object_filter,
    parse_plain_mana_filter, singular_subtype,
};
use crate::parser::value::{parse_amount, parse_count_word, parse_small_number};

#[derive(Debug, Clone, Default)]
pub struct CostParse {
    pub defs: Vec<CostDef>,
    pub unparsed: Vec<Span>,
}

#[derive(Debug, Clone)]
pub struct CostPart<'a> {
    pub text: &'a str,
    pub tokens: &'a [Token<'a>],
    pub span: Span,
}

const COST_VERBS: [&str; 22] = [
    "sacrifice",
    "tap",
    "untap",
    "unattach",
    "discard",
    "pay",
    "exile",
    "remove",
    "return",
    "put",
    "reveal",
    "mill",
    "collect",
    "forage",
    "exert",
    "blight",
    "waterbend",
    "behold",
    "have",
    "roll",
    "flip",
    "choose",
];

const DISALLOWED_COUNTER_WORDS: [&str; 47] = [
    "a",
    "an",
    "all",
    "any",
    "other",
    "another",
    "more",
    "fewer",
    "less",
    "or",
    "and",
    "from",
    "among",
    "on",
    "of",
    "the",
    "your",
    "its",
    "their",
    "no",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "x",
    "counter",
    "counters",
    "suspended",
];

const CHOOSE_LIST_KEYWORDS: [&str; 21] = [
    "flying",
    "first strike",
    "double strike",
    "deathtouch",
    "hexproof",
    "indestructible",
    "lifelink",
    "menace",
    "reach",
    "trample",
    "vigilance",
    "haste",
    "defender",
    "flash",
    "fear",
    "intimidate",
    "shadow",
    "horsemanship",
    "skulk",
    "wither",
    "infect",
];

#[inline]
fn is_cost_verb(w: &str) -> bool {
    COST_VERBS.iter().any(|v| w.eq_ignore_ascii_case(v))
}

#[inline]
fn is_cost_starter(tokens: &[Token<'_>]) -> bool {
    let Some(first) = tokens.first() else {
        return false;
    };
    match &first.kind {
        TokenKind::ManaSymbol(_)
        | TokenKind::Tap
        | TokenKind::Untap
        | TokenKind::Energy
        | TokenKind::Pawprint
        | TokenKind::Chaos => true,
        _ => is_cost_verb(first.raw),
    }
}

#[inline]
fn is_pay_its_mana_cost(tokens: &[Token<'_>]) -> bool {
    tokens.len() >= 4
        && tokens[0].eq_ci("pay")
        && tokens[1].eq_ci("its")
        && tokens[2].eq_ci("mana")
        && tokens[3].eq_ci("cost")
}

/// Zero-regex splitter for cost lists (`{2}, {T}, Sacrifice ~ and pay 2 life`).
pub fn split_cost_parts<'a>(
    input: &'a str,
    tokens: &'a [Token<'a>],
    base_offset: u32,
) -> Vec<CostPart<'a>> {
    struct RawRange {
        tok_start: usize,
        tok_end: usize,
    }

    let mut raw_pieces: Vec<RawRange> = Vec::new();
    let mut piece_start = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let is_comma = tokens[i].is_punct(Punct::Comma);
        let is_and_verb = tokens[i].eq_ci("and")
            && i + 1 < tokens.len()
            && is_cost_starter(&tokens[i + 1..])
            && !is_pay_its_mana_cost(&tokens[i + 1..]);
        if is_comma || is_and_verb {
            if i > piece_start {
                raw_pieces.push(RawRange {
                    tok_start: piece_start,
                    tok_end: i,
                });
            }
            piece_start = i + 1;
        }
        i += 1;
    }
    if piece_start < tokens.len() {
        raw_pieces.push(RawRange {
            tok_start: piece_start,
            tok_end: tokens.len(),
        });
    }

    let mut parts: Vec<(usize, usize)> = Vec::new();
    for piece in raw_pieces {
        let sub = &tokens[piece.tok_start..piece.tok_end];
        if sub.is_empty() {
            continue;
        }
        if let Some(prev) = parts.last_mut()
            && !is_cost_starter(sub)
        {
            prev.1 = piece.tok_end;
        } else {
            parts.push((piece.tok_start, piece.tok_end));
        }
    }

    parts
        .into_iter()
        .filter_map(|(ts, te)| {
            let sub = &tokens[ts..te];
            let first = sub.first()?;
            let last = sub.last()?;
            let rel_s = first.span.start.checked_sub(base_offset)? as usize;
            let rel_e = last.span.end.checked_sub(base_offset)? as usize;
            let text = input.get(rel_s..rel_e)?.trim();
            Some(CostPart {
                text,
                tokens: sub,
                span: Span {
                    start: first.span.start,
                    end: last.span.end,
                },
            })
        })
        .collect()
}

pub fn parse_cost_list(input: &str, base_offset: u32, ctx: &mut ParseCtx<'_>) -> CostParse {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return CostParse::default();
    }
    let lead_ws = input.len() - input.trim_start().len();
    let eff_offset = base_offset + lead_ws as u32;
    let tokens = tokenize(trimmed, 0);
    let mut sub_ctx = ctx.with_normalized_text(trimmed);
    let mut parsed = parse_cost_tokens(&tokens, trimmed, 0, &mut sub_ctx);
    for sp in &mut parsed.unparsed {
        sp.start += eff_offset;
        sp.end += eff_offset;
    }
    parsed
}

pub fn parse_cost_tokens(
    tokens: &[Token<'_>],
    input: &str,
    base_offset: u32,
    ctx: &mut ParseCtx<'_>,
) -> CostParse {
    let parts = split_cost_parts(input, tokens, base_offset);
    let mut defs = Vec::new();
    let mut unparsed = Vec::new();

    for part in parts {
        let has_self_ref_part = defs.iter().any(|d| {
            matches!(
                d,
                CostDef::TapSelf
                    | CostDef::UntapSelf { .. }
                    | CostDef::RemoveCounters {
                        from: CostObjectTarget::SelfObj,
                        ..
                    }
                    | CostDef::PutCounters {
                        on: CostObjectTarget::SelfObj,
                        ..
                    }
            )
        });

        // Anaphora rules (`costs.ts:2311-2340`)
        if part.tokens.len() == 2 && part.tokens[0].eq_ci("sacrifice") && part.tokens[1].eq_ci("it")
        {
            if has_self_ref_part {
                defs.push(CostDef::Sacrifice {
                    count: Box::new(Amount::Const { n: 1 }),
                    target: CostObjectTarget::SelfObj,
                    sacrifice_all: false,
                    half_rounded_up: false,
                    attached_creature: false,
                    one_or_more: false,
                    up_to: None,
                    different_names: false,
                });
                continue;
            }
            unparsed.push(part.span);
            continue;
        }
        if part.tokens.len() == 2 && part.tokens[0].eq_ci("exile") && part.tokens[1].eq_ci("it") {
            if let Some(CostDef::Behold { exile, .. }) = defs.last_mut()
                && !*exile
            {
                *exile = true;
                continue;
            }
            if has_self_ref_part {
                defs.push(CostDef::ExileFrom {
                    zone: Zone::Battlefield,
                    count: Box::new(Amount::Const { n: 1 }),
                    target: Some(CostObjectTarget::SelfObj),
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
                });
                continue;
            }
            unparsed.push(part.span);
            continue;
        }
        if part.tokens.len() == 6
            && part.tokens[0].eq_ci("return")
            && part.tokens[1].eq_ci("it")
            && part.tokens[2].eq_ci("to")
            && part.tokens[3].eq_ci("its")
            && part.tokens[4].eq_ci("owner's")
            && part.tokens[5].eq_ci("hand")
        {
            if has_self_ref_part {
                defs.push(CostDef::ReturnToHand {
                    count: Box::new(Amount::Const { n: 1 }),
                    target: CostObjectTarget::SelfObj,
                });
                continue;
            }
            unparsed.push(part.span);
            continue;
        }

        let part_cp = ctx.interner.checkpoint();
        if let Some(parsed) = parse_single_part(part.tokens, part.text, ctx) {
            defs.extend(parsed);
        } else {
            ctx.interner.rollback(part_cp);
            unparsed.push(part.span);
        }
    }

    CostParse { defs, unparsed }
}

/// Parses Planeswalker loyalty cost (`+2`, `-3`, `0`, `+X`, `-X`, `−X`; bare `X` rejected).
pub fn parse_loyalty_cost(
    tokens: &[Token<'_>],
    _raw_cost: &str,
    span: Span,
    is_planeswalker: bool,
) -> Option<CostParse> {
    if !is_planeswalker || tokens.is_empty() {
        return None;
    }
    if tokens.len() == 1 {
        match tokens[0].kind {
            TokenKind::SignedNumber(delta) => {
                return Some(CostParse {
                    defs: vec![CostDef::Loyalty {
                        delta,
                        is_x: false,
                        sign: None,
                    }],
                    unparsed: Vec::new(),
                });
            }
            TokenKind::Number(n) => {
                return Some(CostParse {
                    defs: vec![CostDef::Loyalty {
                        delta: n as i32,
                        is_x: false,
                        sign: None,
                    }],
                    unparsed: Vec::new(),
                });
            }
            TokenKind::X => {
                return Some(CostParse {
                    defs: Vec::new(),
                    unparsed: vec![span],
                });
            }
            _ => return None,
        }
    }
    if tokens.len() == 2 && matches!(tokens[1].kind, TokenKind::X) {
        if tokens[0].is_punct(Punct::Plus) {
            return Some(CostParse {
                defs: vec![CostDef::Loyalty {
                    delta: 0,
                    is_x: true,
                    sign: Some(LoyaltySign::Plus),
                }],
                unparsed: Vec::new(),
            });
        }
        if tokens[0].is_punct(Punct::Hyphen) {
            return Some(CostParse {
                defs: vec![CostDef::Loyalty {
                    delta: 0,
                    is_x: true,
                    sign: Some(LoyaltySign::Minus),
                }],
                unparsed: Vec::new(),
            });
        }
    }
    None
}

#[inline]
fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Parses `"As an additional cost to cast ~, [you may] <cost>."`
pub fn parse_additional_cost_sentence(
    sentence: &str,
    offset: u32,
    from_spell_check: bool,
    ctx: &mut ParseCtx<'_>,
) -> Option<CostParse> {
    let lead_ws = sentence.len() - sentence.trim_start().len();
    let eff_offset = offset + lead_ws as u32;
    let trimmed = sentence.trim().trim_end_matches('.').trim();
    if !trimmed
        .get(..2)
        .is_some_and(|h| h.eq_ignore_ascii_case("as"))
        || !contains_ascii_ci(trimmed.as_bytes(), b"additional")
    {
        return None;
    }
    let tokens = tokenize(trimmed, 0);
    let mut cur = TokenCursor::new(&tokens);

    if !cur.eat_words(&["as", "an", "additional", "cost", "to", "cast"]) {
        return None;
    }
    // Eat "~" or "this spell"
    if cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
    {
        cur.bump();
    } else if !cur.eat_words(&["this", "spell"]) {
        return None;
    }
    if !cur.eat_punct(Punct::Comma) {
        return None;
    }
    let body_tokens = cur.remaining();
    if body_tokens.is_empty() {
        return None;
    }
    let body_start = body_tokens[0].span.start;
    let body_text = trimmed.get(body_start as usize..)?.trim();
    let cp = ctx.interner.checkpoint();
    let mut sub_ctx = ctx.with_normalized_text(trimmed);

    // Check repeatable optional mana: "[you may] pay {mana}+ any number of times"
    {
        let mut c2 = TokenCursor::new(body_tokens);
        let _ = c2.eat_words(&["you", "may"]);
        if let Some(rep) = try_parse_repeatable_optional_mana(c2.remaining()) {
            return Some(CostParse {
                defs: vec![rep],
                unparsed: Vec::new(),
            });
        }
    }

    let is_optional =
        body_tokens.len() >= 2 && body_tokens[0].eq_ci("you") && body_tokens[1].eq_ci("may");
    if is_optional {
        let inner_tokens = &body_tokens[2..];
        if inner_tokens.is_empty() {
            sub_ctx.interner.rollback(cp);
            return None;
        }
        let inner_start = inner_tokens[0].span.start;
        let inner_text = trimmed.get(inner_start as usize..)?.trim();
        let inner = parse_cost_tokens(inner_tokens, inner_text, inner_start, &mut sub_ctx);
        if !inner.unparsed.is_empty() || inner.defs.is_empty() {
            sub_ctx.interner.rollback(cp);
            let mut unparsed = inner.unparsed;
            for sp in &mut unparsed {
                sp.start += eff_offset;
                sp.end += eff_offset;
            }
            return Some(CostParse {
                defs: Vec::new(),
                unparsed,
            });
        }
        let _ = from_spell_check;
        return Some(CostParse {
            defs: vec![CostDef::Optional { cost: inner.defs }],
            unparsed: Vec::new(),
        });
    }

    let mut parsed = parse_cost_tokens(body_tokens, body_text, body_start, &mut sub_ctx);
    for sp in &mut parsed.unparsed {
        sp.start += eff_offset;
        sp.end += eff_offset;
    }
    Some(parsed)
}

fn parse_single_part(
    tokens: &[Token<'_>],
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<CostDef>> {
    let cp = ctx.interner.checkpoint();
    if let Some(defs) = parse_single_part_no_choice(tokens, raw, ctx) {
        return Some(defs);
    }
    ctx.interner.rollback(cp);

    // Choice (`or`) splitting when RHS starts with a cost starter
    let mut split_indices = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        if tok.eq_ci("or") && i > 0 && i + 1 < tokens.len() && is_cost_starter(&tokens[i + 1..]) {
            split_indices.push(i);
        }
    }
    if split_indices.is_empty() {
        return None;
    }

    let choice_cp = ctx.interner.checkpoint();
    let mut options: Vec<Vec<CostDef>> = Vec::with_capacity(split_indices.len() + 1);
    let mut start = 0usize;
    let base = tokens[0].span.start;
    for idx in split_indices
        .into_iter()
        .chain(std::iter::once(tokens.len()))
    {
        let sub = &tokens[start..idx];
        if sub.is_empty() {
            ctx.interner.rollback(choice_cp);
            return None;
        }
        let rel_s = (sub[0].span.start - base) as usize;
        let rel_e = (sub.last()?.span.end - base) as usize;
        let Some(sub_raw) = raw.get(rel_s..rel_e).map(str::trim) else {
            ctx.interner.rollback(choice_cp);
            return None;
        };
        let Some(parsed) = parse_single_part(sub, sub_raw, ctx) else {
            ctx.interner.rollback(choice_cp);
            return None;
        };
        options.push(parsed);
        start = idx + 1;
    }
    if is_degenerate_modified_sacrifice_choice(&options) {
        ctx.interner.rollback(choice_cp);
        return None;
    }
    Some(vec![CostDef::Choice {
        options,
        forage: false,
    }])
}

fn is_degenerate_modified_sacrifice_choice(options: &[Vec<CostDef>]) -> bool {
    if options.len() != 2 || options[0].len() != 1 || options[1].len() != 1 {
        return false;
    }
    let (
        CostDef::Sacrifice {
            target: CostObjectTarget::Filter { filter: f1 },
            ..
        },
        CostDef::Sacrifice {
            target: CostObjectTarget::Filter { filter: f2 },
            ..
        },
    ) = (&options[0][0], &options[1][0])
    else {
        return false;
    };
    let has_mod_1 = f1.flags.contains(FilterFlags::MODIFIED);
    let has_mod_2 = f2.flags.contains(FilterFlags::MODIFIED);
    if has_mod_1 == has_mod_2 {
        return false;
    }
    let mut stripped_1 = (**f1).clone();
    let mut stripped_2 = (**f2).clone();
    stripped_1.flags.remove(FilterFlags::MODIFIED);
    stripped_2.flags.remove(FilterFlags::MODIFIED);
    stripped_1 == stripped_2
}

fn try_parse_repeatable_optional_mana(tokens: &[Token<'_>]) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    let _ = cur.eat_words(&["you", "may"]);
    if !cur.eat_ci("pay") {
        return None;
    }
    let mut raw_mana = String::new();
    while let Some(tok) = cur.peek() {
        if matches!(tok.kind, TokenKind::ManaSymbol(_)) && !is_disallowed_mana_symbol(tok) {
            raw_mana.push_str(tok.raw);
            cur.bump();
        } else {
            break;
        }
    }
    if raw_mana.is_empty() || !cur.eat_words(&["any", "number", "of", "times"]) || !cur.is_empty() {
        return None;
    }
    Some(CostDef::RepeatableOptionalMana {
        cost: ManaCost::parse(&raw_mana),
    })
}

fn is_disallowed_mana_symbol(tok: &Token<'_>) -> bool {
    matches!(
        tok.kind,
        TokenKind::Tap
            | TokenKind::Untap
            | TokenKind::Energy
            | TokenKind::Pawprint
            | TokenKind::Chaos
            | TokenKind::ManaSymbol(ManaSymbolKind::Unknown)
    )
}

fn try_cost_with_rollback<T>(
    ctx: &mut ParseCtx<'_>,
    f: impl FnOnce(&mut ParseCtx<'_>) -> Option<T>,
) -> Option<T> {
    let cp = ctx.interner.checkpoint();
    let res = f(ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_single_part_no_choice(
    tokens: &[Token<'_>],
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<CostDef>> {
    if tokens.is_empty() {
        return None;
    }

    // 1. Pure braced symbol run (`{2}{U}`, `{T}`, `{Q}`, `{2}{T}`, `{S}`) or `"pay {3}{B}"`
    if let Some(defs) = try_parse_mana_symbols_cost(tokens) {
        return Some(defs);
    }

    // 2. Repeatable optional mana
    if let Some(def) = try_parse_repeatable_optional_mana(tokens) {
        return Some(vec![def]);
    }

    // 3. PayManaPerCounter: "pay {mana}+ for each [<counter>] counter[s] on ~"
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_pay_mana_per_counter(tokens, c)) {
        return Some(vec![def]);
    }

    // 4. PayAttachedManaCost
    if let Some(def) = try_parse_pay_attached_mana_cost(tokens) {
        return Some(vec![def]);
    }

    // 5. PayLife
    if let Some(def) = try_parse_pay_life(tokens, ctx) {
        return Some(vec![def]);
    }

    // 6. PayEnergy
    if let Some(def) = try_parse_pay_energy(tokens) {
        return Some(vec![def]);
    }

    // 7. Waterbend
    if let Some(def) = try_parse_waterbend(tokens) {
        return Some(vec![def]);
    }

    // 8 & 9. BeholdOrChoose / Behold
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_behold(tokens, c)) {
        return Some(vec![def]);
    }

    // 10. Forage
    if tokens.len() == 1 && tokens[0].eq_ci("forage") {
        let food_id = ctx.interner.intern_subtype("Food");
        let mut food_filter = ObjectFilter {
            controller: Some(RelativeControl::You),
            ..Default::default()
        };
        food_filter.subtypes_any.push(food_id);
        return Some(vec![CostDef::Choice {
            options: vec![
                vec![CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(Amount::Const { n: 3 }),
                    target: None,
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
                vec![CostDef::Sacrifice {
                    count: Box::new(Amount::Const { n: 1 }),
                    target: CostObjectTarget::Filter {
                        filter: Box::new(food_filter),
                    },
                    sacrifice_all: false,
                    half_rounded_up: false,
                    attached_creature: false,
                    one_or_more: false,
                    up_to: None,
                    different_names: false,
                }],
            ],
            forage: true,
        }]);
    }

    // 11. Collect evidence
    if let Some(def) = try_parse_collect_evidence(tokens) {
        return Some(vec![def]);
    }

    // 12. Blight
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_blight(tokens, c)) {
        return Some(vec![def]);
    }

    // 13..17. RevealSelfFromHand, RevealChosenPlayer, RevealChosenSubType, RevealHand, OpponentGainsLife
    {
        let mut cur = TokenCursor::new(tokens);
        if cur.eat_words(&["reveal"])
            && cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && cur.bump().is_some()
            && cur.eat_words(&["from", "your", "hand"])
            && cur.is_empty()
        {
            return Some(vec![CostDef::RevealSelfFromHand]);
        }
        cur.restore(0);
        if cur.eat_words(&["reveal", "the", "player", "you", "chose"]) && cur.is_empty() {
            return Some(vec![CostDef::RevealChosenPlayer]);
        }
        cur.restore(0);
        if cur.eat_words(&["reveal", "the", "creature", "type", "you", "chose"]) && cur.is_empty() {
            return Some(vec![CostDef::RevealChosenSubType]);
        }
        cur.restore(0);
        if cur.eat_words(&["reveal", "your", "hand"]) && cur.is_empty() {
            return Some(vec![CostDef::RevealHand]);
        }
        cur.restore(0);
        if cur.eat_ci("have") {
            let each_other = if cur.eat_words(&["each", "other", "player"]) {
                Some(true)
            } else if cur.eat_words(&["an", "opponent"]) {
                Some(false)
            } else {
                None
            };
            if let Some(each_other_player) = each_other
                && cur.eat_ci("gain")
                && let Some(amt) = cur.bump().and_then(|t| parse_count_word(t.raw))
                && cur.eat_ci("life")
                && cur.is_empty()
            {
                return Some(vec![CostDef::OpponentGainsLife {
                    amount: Box::new(amt),
                    each_other_player,
                }]);
            }
        }
    }

    // 18..22. PutCounters, PutFromHandOnTopOfLibrary, PutOnBottomOfLibrary, ProcessOpponentExile, PutExiledWithSelfToGraveyard
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_put_costs(tokens, c)) {
        return Some(vec![def]);
    }

    // 23..24. UnattachSelf, UnattachOther
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_unattach(tokens, c)) {
        return Some(vec![def]);
    }

    // 25..30. RollDie, FlipCoin, ChooseCreatureType, ChooseNumberBetween, ChooseFromKeywordList, DiscardLastDrawnThisTurn
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_misc_actions(tokens, c)) {
        return Some(vec![def]);
    }

    // 31. Sacrifice
    if let Some(defs) = try_cost_with_rollback(ctx, |c| try_parse_sacrifice(tokens, c)) {
        return Some(defs);
    }

    // 32..36. Tap / Untap
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_tap_untap(tokens, c)) {
        return Some(vec![def]);
    }

    // 37..38. DiscardHand / Discard
    if let Some(defs) = try_cost_with_rollback(ctx, |c| try_parse_discard(tokens, c)) {
        return Some(defs);
    }

    // 39. ExileFrom
    if let Some(defs) = try_cost_with_rollback(ctx, |c| try_parse_exile(tokens, raw, c)) {
        return Some(defs);
    }

    // 40. RemoveCounters
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_remove_counters(tokens, c)) {
        return Some(vec![def]);
    }

    // 41. ReturnToHand
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_return_to_hand(tokens, c)) {
        return Some(vec![def]);
    }

    // 42. Mill
    {
        let mut cur = TokenCursor::new(tokens);
        if cur.eat_ci("mill")
            && let Some(amt) = cur.bump().and_then(|t| parse_count_word(t.raw))
            && (cur.eat_ci("card") || cur.eat_ci("cards"))
            && cur.is_empty()
        {
            return Some(vec![CostDef::Mill {
                count: Box::new(amt),
            }]);
        }
    }

    // 43. RevealFromHand
    if let Some(def) = try_cost_with_rollback(ctx, |c| try_parse_reveal_from_hand(tokens, c)) {
        return Some(vec![def]);
    }

    // 44. ExertSelf
    if tokens.len() == 2 && tokens[0].eq_ci("exert") && matches!(tokens[1].kind, TokenKind::SelfRef)
    {
        return Some(vec![CostDef::ExertSelf]);
    }

    None
}

fn try_parse_mana_symbols_cost(tokens: &[Token<'_>]) -> Option<Vec<CostDef>> {
    let mut cur = TokenCursor::new(tokens);
    let has_pay = cur.eat_ci("pay");
    if cur.is_empty() {
        return None;
    }
    let mut defs = Vec::new();
    let mut mana_raw = String::new();
    while let Some(tok) = cur.peek() {
        match tok.kind {
            TokenKind::Tap => {
                if !mana_raw.is_empty() {
                    defs.push(CostDef::Mana {
                        cost: ManaCost::parse(&mana_raw),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    });
                    mana_raw.clear();
                }
                defs.push(CostDef::TapSelf);
                cur.bump();
            }
            TokenKind::Untap => {
                if !mana_raw.is_empty() {
                    defs.push(CostDef::Mana {
                        cost: ManaCost::parse(&mana_raw),
                        x_spend_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                    });
                    mana_raw.clear();
                }
                defs.push(CostDef::UntapSelf { via_symbol: true });
                cur.bump();
            }
            TokenKind::ManaSymbol(ManaSymbolKind::Unknown)
            | TokenKind::Energy
            | TokenKind::Pawprint
            | TokenKind::Chaos => return None,
            TokenKind::ManaSymbol(_) => {
                mana_raw.push_str(tok.raw);
                cur.bump();
            }
            _ => return None,
        }
    }
    if !mana_raw.is_empty() {
        defs.push(CostDef::Mana {
            cost: ManaCost::parse(&mana_raw),
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
        });
    }
    if has_pay && defs.iter().any(|d| !matches!(d, CostDef::Mana { .. })) {
        return None;
    }
    if defs.is_empty() { None } else { Some(defs) }
}

fn try_parse_pay_mana_per_counter(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("pay") {
        return None;
    }
    let mut mana_raw = String::new();
    while let Some(tok) = cur.peek() {
        if matches!(tok.kind, TokenKind::ManaSymbol(_)) && !is_disallowed_mana_symbol(tok) {
            mana_raw.push_str(tok.raw);
            cur.bump();
        } else {
            break;
        }
    }
    if mana_raw.is_empty() || !cur.eat_words(&["for", "each"]) {
        return None;
    }
    let counter_name = if cur.eat_ci("counter") || cur.eat_ci("counters") {
        "any".to_string()
    } else {
        let mut parts: Vec<&str> = Vec::new();
        let mut found = false;
        while let Some(tok) = cur.peek() {
            if tok.eq_ci("counter") || tok.eq_ci("counters") {
                cur.bump();
                found = true;
                break;
            }
            if parts.len() >= 2 {
                return None;
            }
            parts.push(tok.raw);
            cur.bump();
        }
        if !found || parts.is_empty() {
            return None;
        }
        parts.join(" ")
    };
    if !cur.eat_ci("on")
        || !cur
            .bump()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        || !cur.is_empty()
    {
        return None;
    }
    let cid = if counter_name == "any" {
        ctx.interner.intern_counter("any")
    } else {
        lookup_counter_id(&counter_name, ctx.interner)?
    };
    Some(CostDef::PayManaPerCounter {
        per_mana: ManaCost::parse(&mana_raw),
        counter: cid,
    })
}

fn try_parse_pay_attached_mana_cost(tokens: &[Token<'_>]) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("pay") {
        return None;
    }
    if !(cur.eat_ci("enchanted") || cur.eat_ci("equipped")) {
        return None;
    }
    if !(cur.eat_ci("creature's") || cur.eat_ci("permanent's")) {
        return None;
    }
    if !cur.eat_words(&["mana", "cost"]) || !cur.is_empty() {
        return None;
    }
    Some(CostDef::PayAttachedManaCost)
}

fn try_parse_pay_life(tokens: &[Token<'_>], _ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("pay") {
        return None;
    }
    if cur.eat_words(&["half", "your", "life", ",", "rounded", "up"])
        || cur.eat_words(&["half", "your", "life", "rounded", "up"])
    {
        if cur.is_empty() {
            return Some(CostDef::PayLife {
                amount: Box::new(Amount::Const { n: 0 }),
                half_life_rounded_up: true,
            });
        }
        return None;
    }
    if cur.eat_words(&["life", "equal", "to"]) {
        if cur
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && cur
                .peek_n(1)
                .is_some_and(|t| t.raw == "'s" || t.raw == "’s")
            && cur.peek_n_ci(2, "power")
            && cur.remaining().len() == 3
        {
            return Some(CostDef::PayLife {
                amount: Box::new(Amount::Stat {
                    of: mtg_ir::Ref::SelfObj,
                    stat: mtg_ir::StatKind::Power,
                    lki: false,
                }),
                half_life_rounded_up: false,
            });
        }
        if cur.eat_words(&["the", "number", "of", "colors", "in", "your"])
            && (cur.eat_ci("commander's")
                || cur.eat_ci("commanders'")
                || (cur.eat_ci("commander")
                    && cur.peek().is_some_and(|t| t.raw == "'s" || t.raw == "’s")
                    && cur.bump().is_some())
                || (cur.eat_ci("commanders")
                    && !(cur.peek().is_some_and(|t| t.raw == "'" || t.raw == "’")
                        && cur.bump().is_none())))
            && cur.eat_words(&["color", "identity"])
            && cur.is_empty()
        {
            return Some(CostDef::PayLife {
                amount: Box::new(Amount::CommanderColorIdentityCount),
                half_life_rounded_up: false,
            });
        }
        return None;
    }
    let tok = cur.bump()?;
    if tok.eq_ci("a") || tok.eq_ci("an") {
        return None;
    }
    let amt = parse_count_word(tok.raw)?;
    if !cur.eat_ci("life") || !cur.is_empty() {
        return None;
    }
    Some(CostDef::PayLife {
        amount: Box::new(amt),
        half_life_rounded_up: false,
    })
}

fn try_parse_pay_energy(tokens: &[Token<'_>]) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("pay") {
        return None;
    }
    let mut e_count = 0i32;
    while cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::Energy))
    {
        cur.bump();
        e_count += 1;
    }
    if e_count > 0 && cur.is_empty() {
        return Some(CostDef::PayEnergy {
            count: Box::new(Amount::Const { n: e_count }),
        });
    }
    if e_count == 0
        && let Some(amt) = cur.bump().and_then(|t| parse_count_word(t.raw))
        && cur
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::Energy))
    {
        cur.bump();
        if cur.is_empty() {
            return Some(CostDef::PayEnergy {
                count: Box::new(amt),
            });
        }
    }
    None
}

fn try_parse_waterbend(tokens: &[Token<'_>]) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("waterbend") {
        return None;
    }
    let mut mana_raw = String::new();
    while let Some(tok) = cur.peek() {
        if matches!(tok.kind, TokenKind::ManaSymbol(_)) && !is_disallowed_mana_symbol(tok) {
            mana_raw.push_str(tok.raw);
            cur.bump();
        } else {
            return None;
        }
    }
    if mana_raw.is_empty() {
        return None;
    }
    Some(CostDef::Waterbend {
        cost: ManaCost::parse(&mana_raw),
    })
}

fn cost_filter_opts() -> FilterOpts {
    FilterOpts {
        default_zone: ZoneMask::empty(),
        allow_plural_and_disjunction: true,
        allow_spell_noun: false,
        strip_trailing_card: true,
        expand_outlaw_subtypes: true,
    }
}

fn try_parse_behold(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    let is_behold = cur.eat_ci("behold");
    let is_choose = !is_behold && cur.eat_ci("choose");
    if !is_behold && !is_choose {
        return None;
    }
    if !(cur.eat_ci("a") || cur.eat_ci("an") || cur.eat_ci("one")) {
        return None;
    }

    // Check BeholdOrChoose: "(behold|choose) (a|an) <phrase> you control or reveal (a|an) <phrase> card (from|in) your hand"
    if let Some(or_pos) = cur.remaining().iter().position(|t| t.eq_ci("or"))
        && cur
            .remaining()
            .get(or_pos + 1)
            .is_some_and(|t| t.eq_ci("reveal"))
    {
        let left_toks = &cur.remaining()[..or_pos];
        let right_toks = &cur.remaining()[or_pos + 2..];
        let _ = (left_toks, right_toks);
        // Strip trailing "you control" from left
        if left_toks.len() >= 3
            && left_toks[left_toks.len() - 2].eq_ci("you")
            && left_toks[left_toks.len() - 1].eq_ci("control")
        {
            let phrase_toks = &left_toks[..left_toks.len() - 2];
            let of_chosen = phrase_toks.len() == 5
                && phrase_toks[0].eq_ci("creature")
                && phrase_toks[1].eq_ci("of")
                && phrase_toks[2].eq_ci("the")
                && phrase_toks[3].eq_ci("chosen")
                && phrase_toks[4].eq_ci("type");
            let filter = if of_chosen {
                ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    ..Default::default()
                }
            } else {
                let (f, used) =
                    parse_object_filter(phrase_toks, ctx, cost_filter_opts(), &NullScope)?;
                if used != phrase_toks.len() || f.controller.is_some() {
                    return None;
                }
                f
            };
            return Some(CostDef::BeholdOrChoose {
                count: Box::new(Amount::Const { n: 1 }),
                filter: Box::new(filter),
                of_chosen_creature_type: of_chosen,
            });
        }
        return None;
    }

    if !is_behold {
        return None;
    }

    // Check trailing "and exile it"
    let rem = cur.remaining();
    let (phrase_toks, exile) = if rem.len() >= 4
        && rem[rem.len() - 3].eq_ci("and")
        && rem[rem.len() - 2].eq_ci("exile")
        && rem[rem.len() - 1].eq_ci("it")
    {
        (&rem[..rem.len() - 3], true)
    } else {
        (rem, false)
    };
    let (filter, used) = parse_object_filter(phrase_toks, ctx, cost_filter_opts(), &NullScope)?;
    if used != phrase_toks.len() || filter.controller.is_some() {
        return None;
    }
    Some(CostDef::Behold {
        count: Box::new(Amount::Const { n: 1 }),
        filter: Box::new(filter),
        exile,
    })
}

fn try_parse_collect_evidence(tokens: &[Token<'_>]) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_words(&["collect", "evidence"]) {
        return None;
    }
    if cur.eat_words(&[
        "x", ",", "where", "x", "is", "the", "total", "mana", "value", "of",
    ]) && (cur.eat_words(&["the", "permanents", "it", "targets"])
        || cur.eat_words(&["its", "targets"]))
        && cur.is_empty()
    {
        return Some(CostDef::ExileFrom {
            zone: Zone::Graveyard,
            count: Box::new(Amount::AnyNumber),
            target: None,
            from_top: false,
            any_graveyard: false,
            single_graveyard: false,
            min_total_mana_value: None,
            min_total_mana_value_amount: Some(Box::new(Amount::TargetsTotalManaValue)),
            min_colored_mana_symbols: None,
            collect_evidence: true,
            one_or_more: false,
            total_mana_value_equals_x: false,
            and_other_same_name_count: None,
        });
    }
    let n = parse_small_number(cur.bump()?.raw)?;
    if !cur.is_empty() {
        return None;
    }
    Some(CostDef::ExileFrom {
        zone: Zone::Graveyard,
        count: Box::new(Amount::AnyNumber),
        target: None,
        from_top: false,
        any_graveyard: false,
        single_graveyard: false,
        min_total_mana_value: Some(n as u16),
        min_total_mana_value_amount: None,
        min_colored_mana_symbols: None,
        collect_evidence: true,
        one_or_more: false,
        total_mana_value_equals_x: false,
        and_other_same_name_count: None,
    })
}

fn try_parse_blight(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("blight") {
        return None;
    }
    let amt = parse_count_word(cur.bump()?.raw)?;
    if !cur.is_empty() {
        return None;
    }
    if let Amount::Const { n } = amt
        && n < 1
    {
        return None;
    }
    let counter = ctx.interner.intern_counter("-1/-1");
    let filter = ObjectFilter {
        types_any: CardTypeMask::CREATURE,
        controller: Some(RelativeControl::You),
        ..Default::default()
    };
    Some(CostDef::PutCounters {
        count: Box::new(amt),
        counter,
        on: CostObjectTarget::Filter {
            filter: Box::new(filter),
        },
    })
}

fn try_parse_put_costs(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("put") {
        return None;
    }
    let rem = cur.remaining();
    if rem.is_empty() {
        return None;
    }

    // PutCounters: "put <N|X> <counter> counter[s] on (~ | [a|an] <phrase> you control)"
    if let Some(on_pos) = rem.iter().position(|t| t.eq_ci("on"))
        && on_pos >= 2
        && (rem[on_pos - 1].eq_ci("counter") || rem[on_pos - 1].eq_ci("counters"))
        && let Some(amt) = parse_count_word(rem[0].raw)
    {
        let counter_words: Vec<&str> = rem[1..on_pos - 1].iter().map(|t| t.raw).collect();
        let counter_str = counter_words.join(" ");
        if let Some(counter) = lookup_counter_id(&counter_str, ctx.interner) {
            let target_toks = &rem[on_pos + 1..];
            if target_toks.len() == 1 && matches!(target_toks[0].kind, TokenKind::SelfRef) {
                return Some(CostDef::PutCounters {
                    count: Box::new(amt),
                    counter,
                    on: CostObjectTarget::SelfObj,
                });
            }
            // Strip leading a/an and trailing "you control"
            let mut tcur = TokenCursor::new(target_toks);
            let _ = tcur.eat_ci("a") || tcur.eat_ci("an");
            let trems = tcur.remaining();
            if trems.len() >= 3
                && trems[trems.len() - 2].eq_ci("you")
                && trems[trems.len() - 1].eq_ci("control")
            {
                let p_toks = &trems[..trems.len() - 2];
                let cp = ctx.interner.checkpoint();
                if let Some((mut filter, used)) =
                    parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)
                    && used == p_toks.len()
                    && filter.controller.is_none()
                {
                    filter.controller = Some(RelativeControl::You);
                    return Some(CostDef::PutCounters {
                        count: Box::new(amt),
                        counter,
                        on: CostObjectTarget::Filter {
                            filter: Box::new(filter),
                        },
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // PutFromHandOnTopOfLibrary: "put <N|word> [cards|<phrase> card(s)] from your hand on top of your library"
    if let Some(from_pos) = rem.iter().position(|t| t.eq_ci("from"))
        && from_pos >= 2
    {
        let tail = &rem[from_pos..];
        let mut tcur = TokenCursor::new(tail);
        if tcur.eat_words(&["from", "your", "hand", "on", "top", "of", "your", "library"])
            && tcur.is_empty()
            && let Some(amt) = parse_count_word(rem[0].raw)
        {
            let mid = &rem[1..from_pos];
            if mid.len() == 1 && (mid[0].eq_ci("card") || mid[0].eq_ci("cards")) {
                return Some(CostDef::PutFromHandOnTopOfLibrary {
                    count: Box::new(amt),
                    filter: None,
                });
            }
            let cp = ctx.interner.checkpoint();
            if let Some((f, used)) = parse_object_filter(mid, ctx, cost_filter_opts(), &NullScope)
                && used == mid.len()
            {
                return Some(CostDef::PutFromHandOnTopOfLibrary {
                    count: Box::new(amt),
                    filter: Some(Box::new(f)),
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    // PutOnBottomOfLibrary: "put (~|this <noun>) [from your graveyard] on the bottom of (your|its owner's) library"
    let self_body = if rem.len() >= 7 && matches!(rem[0].kind, TokenKind::SelfRef) {
        Some(&rem[1..])
    } else if rem.len() >= 8
        && rem[0].eq_ci("this")
        && (rem[1].eq_ci("card")
            || rem[1].eq_ci("creature")
            || rem[1].eq_ci("permanent")
            || rem[1].eq_ci("artifact")
            || rem[1].eq_ci("enchantment")
            || rem[1].eq_ci("land")
            || rem[1].eq_ci("planeswalker"))
    {
        Some(&rem[2..])
    } else {
        None
    };
    if let Some(body) = self_body {
        let mut c2 = TokenCursor::new(body);
        let from = if c2.eat_words(&["from", "your", "graveyard"]) {
            Zone::Graveyard
        } else {
            Zone::Battlefield
        };
        if c2.eat_words(&["on", "the", "bottom", "of"])
            && (c2.eat_words(&["your", "library"]) || c2.eat_words(&["its", "owner's", "library"]))
            && c2.is_empty()
        {
            return Some(CostDef::PutOnBottomOfLibrary {
                from,
                count: Box::new(Amount::Const { n: 1 }),
                target: Some(CostObjectTarget::SelfObj),
            });
        }
    }

    // PutExiledWithSelfToGraveyard: "put <N> [<p>] card[s] exiled with ~ into (its owner's|their owners') graveyard[s]"
    if let Some(ex_pos) = rem.iter().position(|t| t.eq_ci("exiled"))
        && ex_pos >= 2
        && rem.get(ex_pos + 1).is_some_and(|t| t.eq_ci("with"))
        && rem
            .get(ex_pos + 2)
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
    {
        let mut tail = TokenCursor::new(&rem[ex_pos + 3..]);
        if (tail.eat_words(&["into", "its", "owner's", "graveyard"])
            || tail.eat_words(&["into", "their", "owners'", "graveyards"]))
            && tail.is_empty()
            && let Some(amt) = parse_count_word(rem[0].raw)
        {
            let mid = &rem[1..ex_pos];
            if mid.len() == 1 && (mid[0].eq_ci("card") || mid[0].eq_ci("cards")) {
                return Some(CostDef::PutExiledWithSelfToGraveyard {
                    count: Box::new(amt),
                    filter: None,
                });
            }
            let cp = ctx.interner.checkpoint();
            if let Some((f, used)) = parse_object_filter(mid, ctx, cost_filter_opts(), &NullScope)
                && used == mid.len()
            {
                return Some(CostDef::PutExiledWithSelfToGraveyard {
                    count: Box::new(amt),
                    filter: Some(Box::new(f)),
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    // ProcessOpponentExile: "put <N> [<p>] card[s] an opponent owns from exile into that player's graveyard"
    if let Some(an_pos) = rem.iter().position(|t| t.eq_ci("an"))
        && an_pos >= 2
    {
        let mut tail = TokenCursor::new(&rem[an_pos..]);
        if tail.eat_words(&[
            "an",
            "opponent",
            "owns",
            "from",
            "exile",
            "into",
            "that",
            "player's",
            "graveyard",
        ]) && tail.is_empty()
            && let Some(amt) = parse_count_word(rem[0].raw)
        {
            let mid = &rem[1..an_pos];
            if mid.len() == 1 && (mid[0].eq_ci("card") || mid[0].eq_ci("cards")) {
                return Some(CostDef::ProcessOpponentExile {
                    count: Box::new(amt),
                    filter: None,
                });
            }
            let cp = ctx.interner.checkpoint();
            if let Some((f, used)) = parse_object_filter(mid, ctx, cost_filter_opts(), &NullScope)
                && used == mid.len()
            {
                return Some(CostDef::ProcessOpponentExile {
                    count: Box::new(amt),
                    filter: Some(Box::new(f)),
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    // PutOnBottomOfLibrary with phrase: "put <N> <p> from your (graveyard|hand) on the bottom of ..." or "<p> you control on the bottom of ..."
    if let Some(on_pos) = rem.iter().position(|t| t.eq_ci("on"))
        && on_pos >= 2
    {
        let mut tail = TokenCursor::new(&rem[on_pos..]);
        if tail.eat_words(&["on", "the", "bottom", "of"])
            && (tail.eat_words(&["your", "library"])
                || tail.eat_words(&["its", "owner's", "library"])
                || tail.eat_words(&["their", "owners'", "libraries"]))
            && tail.is_empty()
        {
            let head = &rem[..on_pos];
            let (amt, body) = if let Some(a) = parse_count_word(head[0].raw) {
                (a, &head[1..])
            } else {
                (Amount::Const { n: 1 }, head)
            };
            if body.len() >= 4
                && body[body.len() - 3].eq_ci("from")
                && body[body.len() - 2].eq_ci("your")
                && (body[body.len() - 1].eq_ci("graveyard") || body[body.len() - 1].eq_ci("hand"))
            {
                let from = if body[body.len() - 1].eq_ci("graveyard") {
                    Zone::Graveyard
                } else {
                    Zone::Hand
                };
                let p_toks = &body[..body.len() - 3];
                if p_toks.len() == 1 && (p_toks[0].eq_ci("card") || p_toks[0].eq_ci("cards")) {
                    return Some(CostDef::PutOnBottomOfLibrary {
                        from,
                        count: Box::new(amt),
                        target: None,
                    });
                }
                let cp = ctx.interner.checkpoint();
                if let Some((f, used)) =
                    parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)
                    && used == p_toks.len()
                {
                    return Some(CostDef::PutOnBottomOfLibrary {
                        from,
                        count: Box::new(amt),
                        target: Some(CostObjectTarget::Filter {
                            filter: Box::new(f),
                        }),
                    });
                }
                ctx.interner.rollback(cp);
            }
            if body.len() >= 3
                && body[body.len() - 2].eq_ci("you")
                && body[body.len() - 1].eq_ci("control")
            {
                let p_toks = &body[..body.len() - 2];
                let cp = ctx.interner.checkpoint();
                if let Some((mut f, used)) =
                    parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)
                    && used == p_toks.len()
                    && f.controller.is_none()
                {
                    f.controller = Some(RelativeControl::You);
                    return Some(CostDef::PutOnBottomOfLibrary {
                        from: Zone::Battlefield,
                        count: Box::new(amt),
                        target: Some(CostObjectTarget::Filter {
                            filter: Box::new(f),
                        }),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    None
}

fn try_parse_unattach(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("unattach") {
        return None;
    }
    if cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        && cur.remaining().len() == 1
    {
        return Some(CostDef::UnattachSelf);
    }
    let rem = cur.remaining();
    if rem.len() >= 4
        && rem[rem.len() - 2].eq_ci("from")
        && matches!(rem[rem.len() - 1].kind, TokenKind::SelfRef)
    {
        let amt = parse_count_word(rem[0].raw)?;
        let p_toks = &rem[1..rem.len() - 2];
        let (filter, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
        if used == p_toks.len() && filter.controller.is_none() {
            return Some(CostDef::UnattachOther {
                count: Box::new(amt),
                filter: Box::new(filter),
                from_self: true,
            });
        }
    }
    None
}

fn try_parse_misc_actions(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_words(&["roll", "a"])
        && let Some(tok) = cur.bump()
        && let Some(sides_str) = tok.raw.strip_prefix(['d', 'D'])
        && let Ok(sides) = sides_str.parse::<u16>()
        && sides >= 2
        && cur.is_empty()
    {
        return Some(CostDef::RollDie { sides });
    }
    cur.restore(0);
    if cur.eat_words(&["flip", "a", "coin"]) && cur.is_empty() {
        return Some(CostDef::FlipCoin);
    }
    cur.restore(0);
    if cur.eat_words(&["choose", "a", "creature", "type"]) && cur.is_empty() {
        return Some(CostDef::ChooseCreatureType);
    }
    cur.restore(0);
    if cur.eat_words(&["choose", "a", "number", "between"])
        && let Some(min) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_ci("and")
        && let Some(max) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && min <= max
        && cur.is_empty()
    {
        return Some(CostDef::ChooseNumberBetween { min, max });
    }
    cur.restore(0);
    if cur.eat_words(&[
        "discard", "the", "last", "card", "you", "drew", "this", "turn",
    ]) && cur.is_empty()
    {
        return Some(CostDef::DiscardLastDrawnThisTurn);
    }
    cur.restore(0);
    if cur.eat_ci("choose") {
        let mut keywords: SmallVec<[KeywordId; 4]> = SmallVec::new();
        let mut saw_or = false;
        loop {
            let matched = if cur.eat_words(&["first", "strike"]) {
                Some("first strike")
            } else if cur.eat_words(&["double", "strike"]) {
                Some("double strike")
            } else if let Some(tok) = cur.peek() {
                let low = tok.raw.to_ascii_lowercase();
                if let Some(&kw) = CHOOSE_LIST_KEYWORDS.iter().find(|&&k| k == low) {
                    cur.bump();
                    Some(kw)
                } else {
                    None
                }
            } else {
                None
            };
            let kw = matched?;
            keywords.push(ctx.interner.intern_keyword(kw));
            if cur.is_empty() {
                break;
            }
            if cur.eat_punct(Punct::Comma) {
                if cur.eat_ci("or") {
                    saw_or = true;
                }
                continue;
            }
            if cur.eat_ci("or") {
                saw_or = true;
                continue;
            }
            return None;
        }
        if saw_or && keywords.len() >= 2 {
            return Some(CostDef::ChooseFromKeywordList { keywords });
        }
    }
    None
}

fn split_sacrifice_list<'a>(tokens: &'a [Token<'a>]) -> Option<Vec<&'a [Token<'a>]>> {
    let has_disjunctive_or = (0..tokens.len()).any(|idx| {
        tokens[idx].eq_ci("or")
            && !(idx >= 1
                && tokens[idx - 1].eq_ci("one")
                && tokens.get(idx + 1).is_some_and(|t| t.eq_ci("more")))
    });
    if has_disjunctive_or {
        return None;
    }
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        // Skip over "named (~ | <Card Name>)" so card-name tokens containing
        // "and" / "," (e.g. "Alpha and Omega", "Rakdos, the Showstopper")
        // are not split as separate sacrifice items.
        if tokens[i].eq_ci("named") {
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
        if tokens[i].is_punct(Punct::Comma) {
            // Do not split ", rounded up"
            if tokens.get(i + 1).is_some_and(|t| t.eq_ci("rounded")) {
                i += 1;
                continue;
            }
            if i > start {
                parts.push(&tokens[start..i]);
            }
            if tokens.get(i + 1).is_some_and(|t| t.eq_ci("and")) {
                start = i + 2;
                i += 2;
            } else {
                start = i + 1;
                i += 1;
            }
            continue;
        }
        if tokens[i].eq_ci("and") {
            if i > start {
                parts.push(&tokens[start..i]);
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    if parts.len() >= 2 { Some(parts) } else { None }
}

fn try_parse_sacrifice(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<Vec<CostDef>> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("sacrifice") {
        return None;
    }
    let rem = cur.remaining();
    if rem.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();
    if let Some(d) = parse_single_sacrifice_item(rem, ctx) {
        return Some(vec![d]);
    }
    ctx.interner.rollback(cp);

    // Multi-item sacrifice: "Sacrifice A, B, and C" or "Sacrifice A and B"
    if let Some(segs) = split_sacrifice_list(rem) {
        let mut out = Vec::with_capacity(segs.len());
        let list_cp = ctx.interner.checkpoint();
        for seg in segs {
            if let Some(item) = parse_single_sacrifice_item(seg, ctx) {
                out.push(item);
            } else {
                ctx.interner.rollback(list_cp);
                return None;
            }
        }
        return Some(out);
    }

    None
}

fn parse_single_sacrifice_item(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if tokens.len() == 1 && matches!(tokens[0].kind, TokenKind::SelfRef) {
        return Some(CostDef::Sacrifice {
            count: Box::new(Amount::Const { n: 1 }),
            target: CostObjectTarget::SelfObj,
            sacrifice_all: false,
            half_rounded_up: false,
            attached_creature: false,
            one_or_more: false,
            up_to: None,
            different_names: false,
        });
    }
    if cur.eat_ci("this")
        && cur.peek().is_some_and(|t| {
            t.eq_ci("creature")
                || t.eq_ci("permanent")
                || t.eq_ci("artifact")
                || t.eq_ci("enchantment")
                || t.eq_ci("land")
                || t.eq_ci("planeswalker")
        })
    {
        cur.bump();
        if cur.is_empty() {
            return Some(CostDef::Sacrifice {
                count: Box::new(Amount::Const { n: 1 }),
                target: CostObjectTarget::SelfObj,
                sacrifice_all: false,
                half_rounded_up: false,
                attached_creature: false,
                one_or_more: false,
                up_to: None,
                different_names: false,
            });
        }
    }
    cur.restore(0);

    // "(enchanted|equipped) (creature|permanent|...)"
    if (cur.eat_ci("enchanted") || cur.eat_ci("equipped")) && cur.bump().is_some() && cur.is_empty()
    {
        return Some(CostDef::Sacrifice {
            count: Box::new(Amount::Const { n: 1 }),
            target: CostObjectTarget::Filter {
                filter: Box::new(ObjectFilter::default()),
            },
            sacrifice_all: false,
            half_rounded_up: false,
            attached_creature: true,
            one_or_more: false,
            up_to: None,
            different_names: false,
        });
    }
    cur.restore(0);

    let mut sacrifice_all = false;
    let mut half_rounded_up = false;
    let mut one_or_more = false;
    let mut up_to = None;
    let mut different_names = false;
    let mut count = Amount::Const { n: 1 };

    let rem = if cur.eat_ci("all") {
        sacrifice_all = true;
        count = Amount::Const { n: 1 };
        cur.remaining()
    } else if cur.eat_words(&["half", "the"]) {
        half_rounded_up = true;
        count = Amount::Const { n: 1 };
        let r = cur.remaining();
        // Strip trailing ", rounded up"
        if r.len() >= 3 && r[r.len() - 2].eq_ci("rounded") && r[r.len() - 1].eq_ci("up") {
            let end = if r.len() >= 4 && r[r.len() - 3].is_punct(Punct::Comma) {
                r.len() - 3
            } else {
                r.len() - 2
            };
            &r[..end]
        } else {
            return None;
        }
    } else if cur.eat_words(&["any", "number", "of"]) {
        count = Amount::AnyNumber;
        cur.remaining()
    } else if cur.eat_words(&["one", "or", "more"]) {
        one_or_more = true;
        count = Amount::Const { n: 1 };
        cur.remaining()
    } else if cur.eat_words(&["up", "to"]) {
        let n = parse_small_number(cur.bump()?.raw)?;
        up_to = Some(n as u16);
        count = Amount::Const { n: 0 };
        cur.remaining()
    } else if let Some(amt) = cur.peek().and_then(|t| parse_count_word(t.raw)) {
        cur.bump();
        count = amt;
        cur.remaining()
    } else {
        cur.remaining()
    };

    let rem = if rem.len() >= 3
        && rem[rem.len() - 3].eq_ci("of")
        && (rem[rem.len() - 2].eq_ci("their") || rem[rem.len() - 2].eq_ci("your"))
        && rem[rem.len() - 1].eq_ci("choice")
    {
        &rem[..rem.len() - 3]
    } else if rem.len() >= 5
        && rem[rem.len() - 5].eq_ci("of")
        && (rem[rem.len() - 4].eq_ci("his") || rem[rem.len() - 4].eq_ci("her"))
        && rem[rem.len() - 3].eq_ci("or")
        && (rem[rem.len() - 2].eq_ci("her") || rem[rem.len() - 2].eq_ci("his"))
        && rem[rem.len() - 1].eq_ci("choice")
    {
        &rem[..rem.len() - 5]
    } else {
        rem
    };

    let phrase_toks = if rem.len() >= 3
        && rem[rem.len() - 3].eq_ci("with")
        && rem[rem.len() - 2].eq_ci("different")
        && rem[rem.len() - 1].eq_ci("names")
    {
        different_names = true;
        &rem[..rem.len() - 3]
    } else {
        rem
    };

    let allow_and_disj = (one_or_more || up_to.is_some() || different_names)
        && !phrase_toks.windows(2).any(|w| {
            w[0].eq_ci("and")
                && (w[1].eq_ci("a")
                    || w[1].eq_ci("an")
                    || w[1].eq_ci("another")
                    || w[1].eq_ci("other")
                    || w[1].eq_ci("this")
                    || matches!(w[1].kind, TokenKind::SelfRef)
                    || parse_count_word(w[1].raw).is_some())
        });
    let sac_opts = FilterOpts {
        allow_plural_and_disjunction: allow_and_disj,
        ..cost_filter_opts()
    };
    let (mut filter, used) = parse_object_filter(phrase_toks, ctx, sac_opts, &NullScope)?;
    if used != phrase_toks.len() {
        return None;
    }
    if let Some(ctrl) = filter.controller
        && ctrl != RelativeControl::You
    {
        return None;
    }
    filter.controller = Some(RelativeControl::You);

    Some(CostDef::Sacrifice {
        count: Box::new(count),
        target: CostObjectTarget::Filter {
            filter: Box::new(filter),
        },
        sacrifice_all,
        half_rounded_up,
        attached_creature: false,
        one_or_more,
        up_to,
        different_names,
    })
}

fn try_parse_tap_untap(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    let is_tap = cur.eat_ci("tap");
    let is_untap = !is_tap && cur.eat_ci("untap");
    if !is_tap && !is_untap {
        return None;
    }
    if is_untap
        && cur.remaining().len() == 1
        && matches!(cur.remaining()[0].kind, TokenKind::SelfRef)
    {
        return Some(CostDef::UntapSelf { via_symbol: false });
    }
    if is_tap
        && (cur.eat_ci("enchanted") || cur.eat_ci("equipped"))
        && cur.bump().is_some()
        && cur.is_empty()
    {
        return Some(CostDef::TapAttached);
    }
    cur.restore(1);

    // TapSubsetSum: "tap any number of untapped <phrase> with total power <N|X> or (greater|more)"
    if is_tap && cur.eat_words(&["any", "number", "of", "untapped"]) {
        let rem = cur.remaining();
        if let Some(w_pos) = rem.iter().position(|t| t.eq_ci("with"))
            && rem.get(w_pos + 1).is_some_and(|t| t.eq_ci("total"))
            && rem.get(w_pos + 2).is_some_and(|t| t.eq_ci("power"))
            && rem.len() == w_pos + 6
            && rem[w_pos + 4].eq_ci("or")
            && (rem[w_pos + 5].eq_ci("greater") || rem[w_pos + 5].eq_ci("more"))
            && let Some(amt) = parse_count_word(rem[w_pos + 3].raw)
        {
            let p_toks = &rem[..w_pos];
            let (mut filter, used) =
                parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used == p_toks.len() && filter.controller == Some(RelativeControl::You) {
                let had_other = filter.flags.contains(FilterFlags::OTHER);
                filter.flags.remove(FilterFlags::OTHER);
                let not_self_idx = filter
                    .extra
                    .iter()
                    .position(|p| matches!(p, ObjectPredicate::NotSelf));
                let had_not_self = not_self_idx.is_some();
                if let Some(idx) = not_self_idx {
                    filter.extra.remove(idx);
                }
                let exclude_self = had_other || had_not_self;
                filter.flags |= FilterFlags::UNTAPPED;
                return Some(CostDef::TapSubsetSum {
                    min_total: Box::new(amt),
                    filter: Box::new(filter),
                    exclude_self,
                    restriction_tag: None,
                });
            }
        }
        cur.restore(1);
    }

    let req_flag = if is_tap { "untapped" } else { "tapped" };
    let mut amt = Amount::Const { n: 1 };
    let mut other_prefix = false;
    if cur.eat_words(&["any", "number", "of"]) {
        amt = Amount::AnyNumber;
    } else if cur.eat_ci("another") || cur.eat_ci("other") {
        other_prefix = true;
    } else {
        let a = cur.peek().and_then(|t| parse_count_word(t.raw))?;
        cur.bump();
        amt = a;
        if cur.eat_ci("other") {
            other_prefix = true;
        }
    }
    if !cur.eat_ci(req_flag) {
        return None;
    }
    let rem = cur.remaining();
    let (p_toks, share_creature_type) = if rem.len() >= 5
        && rem[rem.len() - 4].eq_ci("that")
        && rem[rem.len() - 3].eq_ci("share")
        && rem[rem.len() - 2].eq_ci("a")
        && rem[rem.len() - 1].eq_ci("creature")
    {
        (&rem[..rem.len() - 4], true)
    } else if rem.len() >= 5
        && rem[rem.len() - 5].eq_ci("that")
        && rem[rem.len() - 4].eq_ci("share")
        && rem[rem.len() - 3].eq_ci("a")
        && rem[rem.len() - 2].eq_ci("creature")
        && rem[rem.len() - 1].eq_ci("type")
    {
        (&rem[..rem.len() - 5], true)
    } else {
        (rem, false)
    };
    let (mut filter, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
    if used != p_toks.len() || filter.controller != Some(RelativeControl::You) {
        return None;
    }
    if other_prefix {
        filter.flags |= FilterFlags::OTHER;
    }
    filter.flags |= if is_tap {
        FilterFlags::UNTAPPED
    } else {
        FilterFlags::TAPPED
    };
    if is_tap {
        Some(CostDef::TapOthers {
            count: Box::new(amt),
            filter: Box::new(filter),
            share_creature_type,
        })
    } else {
        Some(CostDef::UntapOthers {
            count: Box::new(amt),
            filter: Box::new(filter),
        })
    }
}

fn try_parse_discard(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<Vec<CostDef>> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("discard") {
        return None;
    }
    if cur.eat_words(&["your", "hand"]) && cur.is_empty() {
        return Some(vec![CostDef::DiscardHand]);
    }
    let rem = cur.remaining();
    if let Some(and_pos) = rem.iter().position(|t| t.eq_ci("and"))
        && and_pos > 0
        && and_pos + 1 < rem.len()
        && rem[..and_pos]
            .iter()
            .any(|t| t.eq_ci("card") || t.eq_ci("cards"))
    {
        let cp = ctx.interner.checkpoint();
        if let Some(a) = parse_single_discard_item(&rem[..and_pos], ctx)
            && let Some(b) = parse_single_discard_item(&rem[and_pos + 1..], ctx)
        {
            return Some(vec![a, b]);
        }
        ctx.interner.rollback(cp);
    }
    parse_single_discard_item(rem, ctx).map(|d| vec![d])
}

fn parse_single_discard_item(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let cp = ctx.interner.checkpoint();
    let res = parse_single_discard_item_inner(tokens, ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_single_discard_item_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<CostDef> {
    if (tokens.len() == 1 && matches!(tokens[0].kind, TokenKind::SelfRef))
        || (tokens.len() == 2
            && tokens[0].eq_ci("this")
            && (tokens[1].eq_ci("card") || tokens[1].eq_ci("creature") || tokens[1].eq_ci("spell")))
    {
        return Some(CostDef::Discard {
            count: Box::new(Amount::Const { n: 1 }),
            target: Some(CostObjectTarget::SelfObj),
            random: false,
            same_name_pair: false,
            same_name_as_controlled: None,
            different_names: false,
        });
    }
    let cur = TokenCursor::new(tokens);
    let mut rem = cur.remaining();
    let random =
        if rem.len() >= 2 && rem[rem.len() - 2].eq_ci("at") && rem[rem.len() - 1].eq_ci("random") {
            rem = &rem[..rem.len() - 2];
            true
        } else {
            false
        };
    let first_tok = rem.first()?;
    let (count, other_flag) = if first_tok.eq_ci("another") {
        (Amount::Const { n: 1 }, true)
    } else {
        (parse_count_word(first_tok.raw)?, false)
    };
    let body = &rem[1..];
    if body.is_empty() {
        return None;
    }
    let mut same_name_pair = false;
    let mut same_name_as_controlled: Option<Box<ObjectFilter>> = None;
    let mut different_names = false;

    // "with the same name as (a|an) <phrase> you control" (Key to the Side-Door)
    let same_as_pos = body.windows(5).position(|w| {
        w[0].eq_ci("with")
            && w[1].eq_ci("the")
            && w[2].eq_ci("same")
            && w[3].eq_ci("name")
            && w[4].eq_ci("as")
    });
    let phrase = if let Some(pos) = same_as_pos {
        let mut rhs = &body[pos + 5..];
        if rhs.first().is_some_and(|t| t.eq_ci("a") || t.eq_ci("an")) {
            rhs = &rhs[1..];
        }
        let (ctrl_f, ctrl_used) = parse_object_filter(rhs, ctx, cost_filter_opts(), &NullScope)?;
        if ctrl_used != rhs.len() || ctrl_f.controller != Some(RelativeControl::You) {
            return None;
        }
        same_name_as_controlled = Some(Box::new(ctrl_f));
        &body[..pos]
    } else if body.len() >= 4
        && body[body.len() - 4].eq_ci("with")
        && body[body.len() - 3].eq_ci("the")
        && body[body.len() - 2].eq_ci("same")
        && body[body.len() - 1].eq_ci("name")
    {
        same_name_pair = true;
        &body[..body.len() - 4]
    } else if body.len() >= 3
        && body[body.len() - 3].eq_ci("with")
        && body[body.len() - 2].eq_ci("different")
        && body[body.len() - 1].eq_ci("names")
    {
        different_names = true;
        &body[..body.len() - 3]
    } else {
        body
    };
    if !phrase.iter().any(|t| t.eq_ci("card") || t.eq_ci("cards")) {
        return None;
    }
    let target = if phrase.len() == 1 && (phrase[0].eq_ci("card") || phrase[0].eq_ci("cards")) {
        if other_flag {
            Some(CostObjectTarget::Filter {
                filter: Box::new(ObjectFilter {
                    flags: FilterFlags::OTHER,
                    ..Default::default()
                }),
            })
        } else {
            None
        }
    } else {
        let (mut f, used) = parse_object_filter(phrase, ctx, cost_filter_opts(), &NullScope)?;
        if used != phrase.len() {
            return None;
        }
        if other_flag {
            f.flags |= FilterFlags::OTHER;
        }
        Some(CostObjectTarget::Filter {
            filter: Box::new(f),
        })
    };
    Some(CostDef::Discard {
        count: Box::new(count),
        target,
        random,
        same_name_pair,
        same_name_as_controlled,
        different_names,
    })
}

fn is_self_noun_token(t: &Token<'_>) -> bool {
    t.eq_ci("card")
        || t.eq_ci("creature")
        || t.eq_ci("permanent")
        || t.eq_ci("artifact")
        || t.eq_ci("enchantment")
        || t.eq_ci("land")
        || t.eq_ci("planeswalker")
}

fn try_parse_exile(
    tokens: &[Token<'_>],
    _raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<Vec<CostDef>> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("exile") {
        return None;
    }
    let rem = cur.remaining();
    if rem.is_empty() {
        return None;
    }

    // 1. "exile ~"
    if rem.len() == 1 && matches!(rem[0].kind, TokenKind::SelfRef) {
        return Some(vec![CostDef::ExileFrom {
            zone: Zone::Battlefield,
            count: Box::new(Amount::Const { n: 1 }),
            target: Some(CostObjectTarget::SelfObj),
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
        }]);
    }

    // 2. "exile ~ from your (graveyard|hand)"
    if rem.len() == 4
        && matches!(rem[0].kind, TokenKind::SelfRef)
        && rem[1].eq_ci("from")
        && rem[2].eq_ci("your")
        && (rem[3].eq_ci("graveyard") || rem[3].eq_ci("hand"))
    {
        let zone = if rem[3].eq_ci("graveyard") {
            Zone::Graveyard
        } else {
            Zone::Hand
        };
        return Some(vec![CostDef::ExileFrom {
            zone,
            count: Box::new(Amount::Const { n: 1 }),
            target: Some(CostObjectTarget::SelfObj),
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
        }]);
    }

    // 3. Compound: "exile <N> [<p>] cards from your graveyard and (~ | this <noun>)" (Zombie Assassin)
    if rem.len() >= 6 {
        let prefix_len = if rem[rem.len() - 2].eq_ci("and")
            && matches!(rem[rem.len() - 1].kind, TokenKind::SelfRef)
        {
            Some(rem.len() - 2)
        } else if rem.len() >= 7
            && rem[rem.len() - 3].eq_ci("and")
            && rem[rem.len() - 2].eq_ci("this")
            && is_self_noun_token(&rem[rem.len() - 1])
        {
            Some(rem.len() - 3)
        } else {
            None
        };
        if let Some(plen) = prefix_len
            && plen >= 4
            && rem[plen - 3].eq_ci("from")
            && rem[plen - 2].eq_ci("your")
            && rem[plen - 1].eq_ci("graveyard")
        {
            let mut synth = Vec::with_capacity(plen + 1);
            synth.push(tokens[0].clone());
            synth.extend_from_slice(&rem[..plen]);
            if let Some(mut first_defs) = try_parse_exile(&synth, _raw, ctx) {
                first_defs.push(CostDef::ExileFrom {
                    zone: Zone::Battlefield,
                    count: Box::new(Amount::Const { n: 1 }),
                    target: Some(CostObjectTarget::SelfObj),
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
                });
                return Some(first_defs);
            }
        }
    }

    // 4. "exile (~|this <noun>) and N other card[s] named ~ from your graveyard" (Say Its Name)
    {
        let head_len = if matches!(rem[0].kind, TokenKind::SelfRef) {
            Some(1usize)
        } else if rem.len() >= 2 && rem[0].eq_ci("this") && is_self_noun_token(&rem[1]) {
            Some(2usize)
        } else {
            None
        };
        if let Some(hl) = head_len
            && rem.len() == hl + 9
            && rem[hl].eq_ci("and")
            && let Some(n) = parse_small_number(rem[hl + 1].raw)
            && rem[hl + 2].eq_ci("other")
            && (rem[hl + 3].eq_ci("card") || rem[hl + 3].eq_ci("cards"))
            && rem[hl + 4].eq_ci("named")
            && matches!(rem[hl + 5].kind, TokenKind::SelfRef)
            && rem[hl + 6].eq_ci("from")
            && rem[hl + 7].eq_ci("your")
            && rem[hl + 8].eq_ci("graveyard")
        {
            let mut other_filter = ObjectFilter {
                flags: FilterFlags::OTHER,
                ..Default::default()
            };
            other_filter.extra.push(ObjectPredicate::SameNameAsSource);
            return Some(vec![
                CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(Amount::Const { n: 1 }),
                    target: Some(CostObjectTarget::SelfObj),
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
                },
                CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(Amount::Const { n }),
                    target: Some(CostObjectTarget::Filter {
                        filter: Box::new(other_filter),
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
                },
            ]);
        }
    }

    // 8. "exile a face-down permanent you control face up"
    {
        let mut c2 = TokenCursor::new(rem);
        if c2.eat_words(&[
            "a",
            "face",
            "-",
            "down",
            "permanent",
            "you",
            "control",
            "face",
            "up",
        ]) && c2.is_empty()
        {
            return Some(vec![CostDef::ExileFaceDownPermanentFaceUp]);
        }
    }

    // 9. "exile [(a|an) <p>] card[s] from your graveyard and pay its mana cost"
    if let Some(and_pos) = rem.iter().position(|t| t.eq_ci("and"))
        && is_pay_its_mana_cost(&rem[and_pos + 1..])
        && rem.len() == and_pos + 5
        && and_pos >= 3
        && rem[and_pos - 3].eq_ci("from")
        && rem[and_pos - 2].eq_ci("your")
        && rem[and_pos - 1].eq_ci("graveyard")
    {
        let p_toks = &rem[..and_pos - 3];
        let mut pcur = TokenCursor::new(p_toks);
        let _ = pcur.eat_ci("a") || pcur.eat_ci("an");
        let prem = pcur.remaining();
        let filter = if prem.len() == 1 && (prem[0].eq_ci("card") || prem[0].eq_ci("cards")) {
            None
        } else {
            let (f, used) = parse_object_filter(prem, ctx, cost_filter_opts(), &NullScope)?;
            if used != prem.len() {
                return None;
            }
            Some(Box::new(f))
        };
        return Some(vec![CostDef::ExileFromGraveyardAndPayManaCost { filter }]);
    }

    // 11. "exile the top [N] [<p>] card(s) of your (library|graveyard)"
    if rem.len() >= 6
        && rem[0].eq_ci("the")
        && rem[1].eq_ci("top")
        && rem[rem.len() - 3].eq_ci("of")
        && rem[rem.len() - 2].eq_ci("your")
        && (rem[rem.len() - 1].eq_ci("library") || rem[rem.len() - 1].eq_ci("graveyard"))
    {
        let zone = if rem[rem.len() - 1].eq_ci("library") {
            Zone::Library
        } else {
            Zone::Graveyard
        };
        let mid = &rem[2..rem.len() - 3];
        if !mid.is_empty() {
            let (count, p_toks) = if let Some(amt) = parse_count_word(mid[0].raw) {
                (amt, &mid[1..])
            } else {
                (Amount::Const { n: 1 }, mid)
            };
            let target = if p_toks.len() == 1
                && (p_toks[0].eq_ci("card") || p_toks[0].eq_ci("cards"))
            {
                None
            } else {
                let (f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
                if used != p_toks.len() {
                    return None;
                }
                Some(CostObjectTarget::Filter {
                    filter: Box::new(f),
                })
            };
            return Some(vec![CostDef::ExileFrom {
                zone,
                count: Box::new(count),
                target,
                from_top: true,
                any_graveyard: false,
                single_graveyard: false,
                min_total_mana_value: None,
                min_total_mana_value_amount: None,
                min_colored_mana_symbols: None,
                collect_evidence: false,
                one_or_more: false,
                total_mana_value_equals_x: false,
                and_other_same_name_count: None,
            }]);
        }
    }

    // 12. "exile any number of [<p>] cards from your graveyard with total mana value N or (greater|more)"
    if let Some(from_pos) = rem.iter().position(|t| t.eq_ci("from")) {
        let mut tcur = TokenCursor::new(&rem[from_pos + 1..]);
        if tcur.eat_words(&["your", "graveyard", "with", "total", "mana", "value"])
            && let Some(min_n) = tcur.bump().and_then(|t| parse_small_number(t.raw))
            && tcur.eat_ci("or")
            && (tcur.eat_ci("greater") || tcur.eat_ci("more"))
            && tcur.is_empty()
        {
            let mut hcur = TokenCursor::new(&rem[..from_pos]);
            if hcur.eat_words(&["any", "number", "of"]) {
                let p_toks = hcur.remaining();
                let target =
                    if p_toks.len() == 1 && (p_toks[0].eq_ci("card") || p_toks[0].eq_ci("cards")) {
                        None
                    } else {
                        let (f, used) =
                            parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
                        if used != p_toks.len() {
                            return None;
                        }
                        Some(CostObjectTarget::Filter {
                            filter: Box::new(f),
                        })
                    };
                return Some(vec![CostDef::ExileFrom {
                    zone: Zone::Graveyard,
                    count: Box::new(Amount::AnyNumber),
                    target,
                    from_top: false,
                    any_graveyard: false,
                    single_graveyard: false,
                    min_total_mana_value: Some(min_n as u16),
                    min_total_mana_value_amount: None,
                    min_colored_mana_symbols: None,
                    collect_evidence: false,
                    one_or_more: false,
                    total_mana_value_equals_x: false,
                    and_other_same_name_count: None,
                }]);
            }
        }
    }

    // 13. "exile <lead> [<p>] spell[s] you control" (stack - Nivmagus Elemental)
    if rem.len() >= 4
        && (rem[rem.len() - 3].eq_ci("spell") || rem[rem.len() - 3].eq_ci("spells"))
        && rem[rem.len() - 2].eq_ci("you")
        && rem[rem.len() - 1].eq_ci("control")
    {
        let mut hcur = TokenCursor::new(&rem[..rem.len() - 3]);
        if let Some(count) = hcur.bump().and_then(|t| parse_count_word(t.raw)) {
            let p_rem = hcur.remaining();
            let target = if p_rem.is_empty() {
                None
            } else {
                // Re-include the "spell" token for filter parsing (e.g. "instant or sorcery spell")
                let with_spell = &rem[rem.len() - hcur.remaining().len() - 3..rem.len() - 2];
                let (f, used) = parse_object_filter(
                    with_spell,
                    ctx,
                    FilterOpts {
                        allow_spell_noun: true,
                        ..cost_filter_opts()
                    },
                    &NullScope,
                )?;
                if used != with_spell.len() || f.controller.is_some() {
                    return None;
                }
                Some(CostObjectTarget::Filter {
                    filter: Box::new(f),
                })
            };
            return Some(vec![CostDef::ExileFrom {
                zone: Zone::Stack,
                count: Box::new(count),
                target,
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
            }]);
        }
    }

    // 14. "exile <lead> <p> you control [with total mana value X]" (Battlefield / Fabrication Foundry)
    {
        let (bf_rem, total_mv_x) = if rem.len() >= 9
            && rem[rem.len() - 5].eq_ci("with")
            && rem[rem.len() - 4].eq_ci("total")
            && rem[rem.len() - 3].eq_ci("mana")
            && rem[rem.len() - 2].eq_ci("value")
            && rem[rem.len() - 1].eq_ci("x")
        {
            (&rem[..rem.len() - 5], true)
        } else {
            (rem, false)
        };
        if bf_rem.len() >= 4
            && bf_rem[bf_rem.len() - 2].eq_ci("you")
            && bf_rem[bf_rem.len() - 1].eq_ci("control")
        {
            let mut c2 = TokenCursor::new(&bf_rem[..bf_rem.len() - 2]);
            let one_or_more = c2.eat_words(&["one", "or", "more"]);
            let (count, add_other) = if one_or_more {
                (Amount::Const { n: 1 }, false)
            } else if c2.eat_ci("another") {
                (Amount::Const { n: 1 }, true)
            } else {
                (parse_count_word(c2.bump()?.raw)?, false)
            };
            let p_toks = c2.remaining();
            let (mut f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used == p_toks.len() && f.controller.is_none() {
                f.controller = Some(RelativeControl::You);
                if add_other {
                    f.flags |= FilterFlags::OTHER;
                }
                return Some(vec![CostDef::ExileFrom {
                    zone: Zone::Battlefield,
                    count: Box::new(count),
                    target: Some(CostObjectTarget::Filter {
                        filter: Box::new(f),
                    }),
                    from_top: false,
                    any_graveyard: false,
                    single_graveyard: false,
                    min_total_mana_value: None,
                    min_total_mana_value_amount: None,
                    min_colored_mana_symbols: None,
                    collect_evidence: false,
                    one_or_more,
                    total_mana_value_equals_x: total_mv_x,
                    and_other_same_name_count: None,
                }]);
            }
        }
    }

    // 15 & 16. "exile <lead> <p> from (your graveyard | your hand | a graveyard | a single graveyard)"
    if let Some(from_pos) = rem.iter().position(|t| t.eq_ci("from")) {
        let tail = &rem[from_pos + 1..];
        let mut tcur = TokenCursor::new(tail);
        let (zone, any_graveyard, single_graveyard) = if tcur.eat_words(&["your", "graveyard"]) {
            (Zone::Graveyard, false, false)
        } else if tcur.eat_words(&["your", "hand"]) {
            (Zone::Hand, false, false)
        } else if tcur.eat_words(&["a", "single", "graveyard"]) {
            (Zone::Graveyard, false, true)
        } else if tcur.eat_words(&["a", "graveyard"]) {
            (Zone::Graveyard, true, false)
        } else {
            return None;
        };
        if !tcur.is_empty() {
            return None;
        }
        let head = &rem[..from_pos];
        let mut hcur = TokenCursor::new(head);
        let one_or_more = hcur.eat_words(&["one", "or", "more"]);
        let mut add_other = false;
        let count = if one_or_more {
            Amount::Const { n: 1 }
        } else if hcur.eat_words(&["any", "number", "of"]) {
            Amount::AnyNumber
        } else if hcur.eat_ci("another") {
            add_other = true;
            Amount::Const { n: 1 }
        } else {
            parse_count_word(hcur.bump()?.raw)?
        };
        let p_toks = hcur.remaining();
        let target = if p_toks.len() == 1 && (p_toks[0].eq_ci("card") || p_toks[0].eq_ci("cards")) {
            if add_other {
                Some(CostObjectTarget::Filter {
                    filter: Box::new(ObjectFilter {
                        flags: FilterFlags::OTHER,
                        ..Default::default()
                    }),
                })
            } else {
                None
            }
        } else if !add_other
            && p_toks.len() == 2
            && p_toks[0].eq_ci("permanent")
            && (p_toks[1].eq_ci("card") || p_toks[1].eq_ci("cards"))
        {
            // TS Chitinous Crawler parity: "Exile a permanent card from your graveyard" -> target: None
            None
        } else {
            let (mut f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used != p_toks.len() {
                return None;
            }
            if add_other {
                f.flags |= FilterFlags::OTHER;
            }
            Some(CostObjectTarget::Filter {
                filter: Box::new(f),
            })
        };
        return Some(vec![CostDef::ExileFrom {
            zone,
            count: Box::new(count),
            target,
            from_top: false,
            any_graveyard,
            single_graveyard,
            min_total_mana_value: None,
            min_total_mana_value_amount: None,
            min_colored_mana_symbols: None,
            collect_evidence: false,
            one_or_more,
            total_mana_value_equals_x: false,
            and_other_same_name_count: None,
        }]);
    }

    let _ = MinColoredManaSymbols {
        color: ManaColor::W,
        min: 1,
    };
    None
}

fn try_parse_remove_counters(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("remove") {
        return None;
    }
    let rem = cur.remaining();
    let from_pos = rem.iter().position(|t| t.eq_ci("from"))?;
    let head = &rem[..from_pos];
    let tail = &rem[from_pos + 1..];
    if head.is_empty() || tail.is_empty() {
        return None;
    }

    // Parse target after "from" into (from_target, among, or_suspended_you_own)
    let parse_remove_target = |ctx: &mut ParseCtx<'_>| -> Option<(CostObjectTarget, bool, bool)> {
        let mut tcur = TokenCursor::new(tail);
        let mut among = false;
        if tcur.eat_ci("among") {
            among = true;
        }
        if tcur.remaining().len() == 1 && matches!(tcur.remaining()[0].kind, TokenKind::SelfRef) {
            return Some((CostObjectTarget::SelfObj, among, false));
        }
        if tcur.eat_words(&[
            "a",
            "permanent",
            "you",
            "control",
            "or",
            "a",
            "suspended",
            "card",
            "you",
            "own",
        ]) || tcur.eat_words(&[
            "a",
            "permanent",
            "you",
            "control",
            "or",
            "suspended",
            "card",
            "you",
            "own",
        ]) {
            return Some((
                CostObjectTarget::Filter {
                    filter: Box::new(ObjectFilter::default()),
                },
                false,
                true,
            ));
        }
        let _ = tcur.eat_ci("a") || tcur.eat_ci("an");
        let trem = tcur.remaining();
        if trem.len() >= 3
            && trem[trem.len() - 2].eq_ci("you")
            && trem[trem.len() - 1].eq_ci("control")
        {
            let p_toks = &trem[..trem.len() - 2];
            let (mut f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used == p_toks.len() && f.controller.is_none() {
                f.controller = Some(RelativeControl::You);
                return Some((
                    CostObjectTarget::Filter {
                        filter: Box::new(f),
                    },
                    among,
                    false,
                ));
            }
        }
        None
    };

    // Counter choice: "remove (a|an|one) <c1> counter or (a|an|one) <c2> counter from ..." (Ion Storm)
    if let Some(or_pos) = head.iter().position(|t| t.eq_ci("or"))
        && head.len() >= 5
        && (head[0].eq_ci("a") || head[0].eq_ci("an") || head[0].eq_ci("one"))
        && or_pos >= 3
        && (head[or_pos - 1].eq_ci("counter") || head[or_pos - 1].eq_ci("counters"))
        && or_pos + 3 < head.len()
        && (head[or_pos + 1].eq_ci("a")
            || head[or_pos + 1].eq_ci("an")
            || head[or_pos + 1].eq_ci("one"))
        && (head.last()?.eq_ci("counter") || head.last()?.eq_ci("counters"))
    {
        let c1_raw: Vec<&str> = head[1..or_pos - 1].iter().map(|t| t.raw).collect();
        let c2_raw: Vec<&str> = head[or_pos + 2..head.len() - 1]
            .iter()
            .map(|t| t.raw)
            .collect();
        if !c1_raw.is_empty() && !c2_raw.is_empty() {
            let c1 = lookup_counter_target(&c1_raw.join(" "), ctx.interner)?;
            let c2 = lookup_counter_target(&c2_raw.join(" "), ctx.interner)?;
            let (from_target, among, or_susp) = parse_remove_target(ctx)?;
            if !among && !or_susp {
                return Some(CostDef::Choice {
                    options: vec![
                        vec![CostDef::RemoveCounters {
                            counter: c1,
                            count: Box::new(Amount::Const { n: 1 }),
                            remove_all: false,
                            from: from_target.clone(),
                            among: false,
                            one_or_more: false,
                            or_suspended_you_own: false,
                        }],
                        vec![CostDef::RemoveCounters {
                            counter: c2,
                            count: Box::new(Amount::Const { n: 1 }),
                            remove_all: false,
                            from: from_target,
                            among: false,
                            one_or_more: false,
                            or_suspended_you_own: false,
                        }],
                    ],
                    forage: false,
                });
            }
        }
    }

    let mut hcur = TokenCursor::new(head);
    let mut remove_all = false;
    let mut one_or_more = false;
    let count = if hcur.eat_ci("all") {
        remove_all = true;
        Amount::Const { n: 0 }
    } else if hcur.eat_words(&["any", "number", "of"]) {
        Amount::AnyNumber
    } else if hcur.eat_words(&["one", "or", "more"]) {
        one_or_more = true;
        Amount::Const { n: 1 }
    } else {
        parse_count_word(hcur.bump()?.raw)?
    };

    let c_rem = hcur.remaining();
    if c_rem.is_empty() || !(c_rem.last()?.eq_ci("counter") || c_rem.last()?.eq_ci("counters")) {
        return None;
    }
    let counter = if c_rem.len() == 1 {
        CounterTarget::Any
    } else {
        let name_str: Vec<&str> = c_rem[..c_rem.len() - 1].iter().map(|t| t.raw).collect();
        lookup_counter_target(&name_str.join(" "), ctx.interner)?
    };

    let (from, among, or_suspended_you_own) = parse_remove_target(ctx)?;
    Some(CostDef::RemoveCounters {
        counter,
        count: Box::new(count),
        remove_all,
        from,
        among,
        one_or_more,
        or_suspended_you_own,
    })
}

fn try_parse_return_to_hand(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("return") {
        return None;
    }
    let rem = cur.remaining();
    if rem.len() == 5
        && matches!(rem[0].kind, TokenKind::SelfRef)
        && rem[1].eq_ci("to")
        && rem[2].eq_ci("its")
        && rem[3].eq_ci("owner's")
        && rem[4].eq_ci("hand")
    {
        return Some(CostDef::ReturnToHand {
            count: Box::new(Amount::Const { n: 1 }),
            target: CostObjectTarget::SelfObj,
        });
    }
    if let Some(to_pos) = rem.iter().position(|t| t.eq_ci("to")) {
        let mut tail = TokenCursor::new(&rem[to_pos + 1..]);
        if (tail.eat_words(&["its", "owner's", "hand"])
            || tail.eat_words(&["their", "owners'", "hands"])
            || tail.eat_words(&["their", "owner's", "hand"]))
            && tail.is_empty()
        {
            let head = &rem[..to_pos];
            let count = parse_count_word(head.first()?.raw)?;
            let p_toks = &head[1..];
            let (f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used == p_toks.len() && f.controller == Some(RelativeControl::You) {
                return Some(CostDef::ReturnToHand {
                    count: Box::new(count),
                    target: CostObjectTarget::Filter {
                        filter: Box::new(f),
                    },
                });
            }
        }
    }
    None
}

fn try_parse_reveal_from_hand(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<CostDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("reveal") {
        return None;
    }
    let rem = cur.remaining();
    let (body, share_color) = if rem.len() >= 4
        && rem[rem.len() - 4].eq_ci("that")
        && rem[rem.len() - 3].eq_ci("share")
        && rem[rem.len() - 2].eq_ci("a")
        && rem[rem.len() - 1].eq_ci("color")
    {
        (&rem[..rem.len() - 4], true)
    } else {
        (rem, false)
    };
    if body.len() >= 4
        && (body[body.len() - 3].eq_ci("in") || body[body.len() - 3].eq_ci("from"))
        && body[body.len() - 2].eq_ci("your")
        && body[body.len() - 1].eq_ci("hand")
    {
        let head = &body[..body.len() - 3];
        let count = parse_count_word(head.first()?.raw)?;
        let p_toks = &head[1..];
        let target = if p_toks.len() == 1 && (p_toks[0].eq_ci("card") || p_toks[0].eq_ci("cards")) {
            None
        } else {
            let (f, used) = parse_object_filter(p_toks, ctx, cost_filter_opts(), &NullScope)?;
            if used != p_toks.len() {
                return None;
            }
            Some(CostObjectTarget::Filter {
                filter: Box::new(f),
            })
        };
        return Some(CostDef::RevealFromHand {
            count: Box::new(count),
            target,
            share_color,
        });
    }
    None
}

pub fn lookup_counter_id(raw: &str, interner: &mut Interner) -> Option<CounterId> {
    let norm = normalize_counter_text(raw)?;
    Some(interner.intern_counter(&norm))
}

pub fn lookup_counter_target(raw: &str, interner: &mut Interner) -> Option<CounterTarget> {
    let clean = raw.trim();
    if clean.eq_ignore_ascii_case("any") {
        return Some(CounterTarget::Any);
    }
    if clean.eq_ignore_ascii_case("each") {
        return Some(CounterTarget::Each);
    }
    lookup_counter_id(clean, interner).map(CounterTarget::Specific)
}

fn normalize_counter_text(raw: &str) -> Option<String> {
    let clean = raw.trim().replace('−', "-").to_ascii_lowercase();
    let collapsed = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || DISALLOWED_COUNTER_WORDS.contains(&collapsed.as_str()) {
        return None;
    }
    let parts: Vec<&str> = collapsed.split(' ').collect();
    if parts.iter().any(|p| DISALLOWED_COUNTER_WORDS.contains(p)) {
        return None;
    }
    normalize_counter_name(&collapsed)
}

// ---- Cost Payability Predicates ---------------------------------------------------------------

pub fn is_cast_payable_cost(def: &CostDef) -> bool {
    match def {
        CostDef::Mana { .. }
        | CostDef::PayLife { .. }
        | CostDef::PayEnergy { .. }
        | CostDef::TapSubsetSum { .. }
        | CostDef::Mill { .. } => true,
        CostDef::Sacrifice { target, .. } | CostDef::ReturnToHand { target, .. } => {
            !matches!(target, CostObjectTarget::SelfObj)
        }
        CostDef::Discard { target, .. } | CostDef::RevealFromHand { target, .. } => {
            !matches!(target, Some(CostObjectTarget::SelfObj))
        }
        CostDef::TapOthers { .. } => true,
        CostDef::ExileFrom { zone, target, .. } => {
            !matches!(target, Some(CostObjectTarget::SelfObj))
                && matches!(zone, Zone::Graveyard | Zone::Hand | Zone::Battlefield)
        }
        CostDef::Choice { options, .. } => {
            !options.is_empty()
                && options
                    .iter()
                    .all(|opt| !opt.is_empty() && opt.iter().all(is_cast_payable_cost))
        }
        CostDef::Optional { cost } => !cost.is_empty() && cost.iter().all(is_cast_payable_cost),
        _ => false,
    }
}

pub fn is_legacy_cast_payable_cost(def: &CostDef) -> bool {
    is_cast_payable_cost(def)
}

pub fn is_ward_payable_cost(def: &CostDef) -> bool {
    match def {
        CostDef::Mana { cost, .. } => cost.x_count == 0,
        CostDef::PayLife { amount, .. } => matches!(**amount, Amount::Const { .. }),
        CostDef::PayEnergy { count } | CostDef::Mill { count } => {
            matches!(**count, Amount::Const { .. })
        }
        CostDef::TapSubsetSum {
            min_total,
            exclude_self,
            ..
        } => matches!(**min_total, Amount::Const { .. }) && !*exclude_self,
        CostDef::Sacrifice { count, target, .. } | CostDef::ReturnToHand { count, target } => {
            matches!(**count, Amount::Const { .. }) && !matches!(target, CostObjectTarget::SelfObj)
        }
        CostDef::Discard { count, target, .. } | CostDef::RevealFromHand { count, target, .. } => {
            matches!(**count, Amount::Const { .. })
                && !matches!(target, Some(CostObjectTarget::SelfObj))
        }
        CostDef::TapOthers { count, .. } => matches!(**count, Amount::Const { .. }),
        CostDef::ExileFrom {
            zone,
            count,
            target,
            ..
        } => {
            matches!(**count, Amount::Const { .. })
                && !matches!(target, Some(CostObjectTarget::SelfObj))
                && matches!(zone, Zone::Graveyard | Zone::Hand | Zone::Battlefield)
        }
        CostDef::PutCounters { count, on, .. } => {
            matches!(**count, Amount::Const { .. }) && !matches!(on, CostObjectTarget::SelfObj)
        }
        CostDef::Choice { options, .. } => {
            !options.is_empty()
                && options
                    .iter()
                    .all(|opt| !opt.is_empty() && opt.iter().all(is_ward_payable_cost))
        }
        _ => false,
    }
}

pub fn is_equip_payable_cost(def: &CostDef) -> bool {
    match def {
        CostDef::Mana { cost, .. } => cost.x_count == 0,
        CostDef::TapSelf
        | CostDef::UntapSelf { .. }
        | CostDef::ExertSelf
        | CostDef::PayLife { .. }
        | CostDef::PayEnergy { .. }
        | CostDef::TapSubsetSum { .. }
        | CostDef::Sacrifice { .. }
        | CostDef::ReturnToHand { .. }
        | CostDef::Mill { .. } => true,
        CostDef::RemoveCounters { from, .. } => matches!(from, CostObjectTarget::SelfObj),
        CostDef::Discard { target, .. } | CostDef::RevealFromHand { target, .. } => {
            !matches!(target, Some(CostObjectTarget::SelfObj))
        }
        CostDef::TapOthers { .. } => true,
        CostDef::ExileFrom { zone, .. } => {
            matches!(zone, Zone::Graveyard | Zone::Hand | Zone::Battlefield)
        }
        CostDef::Choice { options, .. } => {
            !options.is_empty()
                && options
                    .iter()
                    .all(|opt| !opt.is_empty() && opt.iter().all(is_equip_payable_cost))
        }
        _ => false,
    }
}

pub fn is_spell_alt_cost_compatible(def: &CostDef) -> bool {
    is_cast_payable_cost(def) || matches!(def, CostDef::OpponentGainsLife { .. })
}

pub fn is_supported_optional_cost(def: &CostDef) -> bool {
    match def {
        CostDef::Choice { options, .. } => {
            !options.is_empty()
                && options
                    .iter()
                    .all(|opt| !opt.is_empty() && opt.iter().all(is_supported_optional_cost))
        }
        CostDef::Mana { .. }
        | CostDef::PayLife { .. }
        | CostDef::PayEnergy { .. }
        | CostDef::Sacrifice { .. }
        | CostDef::Discard { .. }
        | CostDef::RemoveCounters { .. }
        | CostDef::ExileFrom { .. }
        | CostDef::TapOthers { .. }
        | CostDef::TapSubsetSum { .. }
        | CostDef::ExertSelf
        | CostDef::ReturnToHand { .. }
        | CostDef::RevealFromHand { .. }
        | CostDef::Behold { .. }
        | CostDef::PutCounters { .. }
        | CostDef::DiscardHand
        | CostDef::PutFromHandOnTopOfLibrary { .. }
        | CostDef::PutOnBottomOfLibrary { .. }
        | CostDef::UntapOthers { .. }
        | CostDef::RevealHand
        | CostDef::OpponentGainsLife { .. }
        | CostDef::ProcessOpponentExile { .. }
        | CostDef::PutExiledWithSelfToGraveyard { .. }
        | CostDef::Mill { .. }
        | CostDef::ChooseCreatureType
        | CostDef::BeholdOrChoose { .. }
        | CostDef::ChooseNumberBetween { .. }
        | CostDef::RevealChosenPlayer
        | CostDef::RevealChosenSubType
        | CostDef::DiscardLastDrawnThisTurn
        | CostDef::ChooseFromKeywordList { .. }
        | CostDef::PayAttachedManaCost
        | CostDef::ExileFromGraveyardAndPayManaCost { .. }
        | CostDef::ExileFaceDownPermanentFaceUp
        | CostDef::PayManaPerCounter { .. }
        | CostDef::RepeatableOptionalMana { .. }
        | CostDef::RevealSelfFromHand
        | CostDef::Waterbend { .. } => true,
        _ => false,
    }
}

// ---- CostDelta, Cost Modifiers & Mana Spending Restrictions -----------------------------------

fn mana_color_ascii_key(c: ManaColor) -> u8 {
    // Lexicographical ASCII order: B, C, G, R, U, W
    match c {
        ManaColor::B => 0,
        ManaColor::C => 1,
        ManaColor::G => 2,
        ManaColor::R => 3,
        ManaColor::U => 4,
        ManaColor::W => 5,
    }
}

pub fn parse_cost_delta(tokens: &[Token<'_>], mode: CostDeltaMode) -> Option<CostDelta> {
    if tokens.is_empty() {
        return None;
    }
    let mut generic = 0u16;
    let mut color_counts = [0u16; 6];
    let mut hybrid_colored: SmallVec<[(ManaColor, ManaColor); 2]> = SmallVec::new();

    for tok in tokens {
        let TokenKind::ManaSymbol(kind) = &tok.kind else {
            return None;
        };
        match *kind {
            ManaSymbolKind::Generic(n) => {
                generic = generic.saturating_add(n as u16);
            }
            ManaSymbolKind::Colored(c) => {
                let idx = mana_color_ascii_key(c) as usize;
                color_counts[idx] = color_counts[idx].saturating_add(1);
            }
            ManaSymbolKind::Hybrid(a, b)
                if mode == CostDeltaMode::Reduce && a != ManaColor::C && b != ManaColor::C =>
            {
                hybrid_colored.push((a, b));
            }
            _ => return None,
        }
    }

    let colors_in_ascii_order = [
        ManaColor::B,
        ManaColor::C,
        ManaColor::G,
        ManaColor::R,
        ManaColor::U,
        ManaColor::W,
    ];
    let mut colored: SmallVec<[ColoredCount; 3]> = SmallVec::new();
    for (idx, &color) in colors_in_ascii_order.iter().enumerate() {
        if color_counts[idx] > 0 {
            colored.push(ColoredCount {
                color,
                count: color_counts[idx],
            });
        }
    }

    Some(CostDelta {
        mode,
        generic,
        per: None,
        additional_costs: Vec::new(),
        colored,
        hybrid_colored,
        colored_only: false,
        min_total_mana: None,
        max_reduction: None,
        per_target_life: None,
    })
}

pub fn parse_where_x_clause_amount(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<Amount> {
    let mut cur = TokenCursor::new(tokens);
    let _ = cur.eat_punct(Punct::Comma);
    if !cur.eat_words(&["where", "x", "is"]) {
        return None;
    }
    if let Some(amt) = parse_where_x_body_tokens(cur.remaining(), ctx) {
        return Some(amt);
    }
    let (amt, used) = parse_amount(cur.remaining(), ctx, &mut NullScope)?;
    if used == cur.remaining().len() {
        Some(amt)
    } else {
        None
    }
}

fn strip_trailing_you_control<'a>(tokens: &'a [Token<'a>]) -> (&'a [Token<'a>], bool) {
    if tokens.len() >= 3
        && tokens[tokens.len() - 2].eq_ci("you")
        && tokens[tokens.len() - 1].eq_ci("control")
    {
        (&tokens[..tokens.len() - 2], true)
    } else {
        (tokens, false)
    }
}

fn parse_where_x_body_tokens(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<Amount> {
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_words(&["the", "greatest"]) {
        let stat = if cur.eat_ci("power") {
            StatKind::Power
        } else if cur.eat_ci("toughness") {
            StatKind::Toughness
        } else if cur.eat_words(&["mana", "value"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        if !cur.eat_ci("among") {
            return None;
        }
        let (group_toks, stripped_you) = strip_trailing_you_control(cur.remaining());
        let (mut f, used) = parse_object_filter(
            group_toks,
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                strip_trailing_card: false,
                ..Default::default()
            },
            &NullScope,
        )?;
        if used != group_toks.len() {
            return None;
        }
        if stripped_you {
            if f.controller.is_some() {
                return None;
            }
            f.controller = Some(RelativeControl::You);
        } else if f.controller != Some(RelativeControl::You) {
            return None;
        }
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::GreatestStatAmong {
            filter: Box::new(f),
            stat,
        });
    }

    cur.restore(0);
    if cur.eat_words(&["the", "total"]) {
        let stat = if cur.eat_ci("power") {
            StatKind::Power
        } else if cur.eat_ci("toughness") {
            StatKind::Toughness
        } else if cur.eat_words(&["mana", "value"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        if !cur.eat_ci("of") {
            return None;
        }
        let (group_toks, stripped_you) = strip_trailing_you_control(cur.remaining());
        let (mut f, used) = parse_object_filter(
            group_toks,
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                strip_trailing_card: false,
                ..Default::default()
            },
            &NullScope,
        )?;
        if used != group_toks.len() {
            return None;
        }
        if stripped_you {
            if f.controller.is_some() {
                return None;
            }
            f.controller = Some(RelativeControl::You);
        } else if f.controller != Some(RelativeControl::You) {
            return None;
        }
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::TotalStatAmong {
            filter: Box::new(f),
            stat,
        });
    }

    cur.restore(0);
    if cur.eat_words(&["your", "devotion", "to"]) {
        let colors = parse_devotion_color_list(cur.remaining())?;
        return Some(Amount::Devotion {
            colors,
            player: Some(PlayerRef::You),
        });
    }

    cur.restore(0);
    if cur.eat_words(&["your", "life", "total"]) && cur.is_empty() {
        return Some(Amount::Life { of: PlayerRef::You });
    }

    cur.restore(0);
    if cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        && cur
            .peek_n(1)
            .is_some_and(|t| t.raw == "'s" || t.raw == "’s")
    {
        cur.bump();
        cur.bump();
        let stat = if cur.eat_ci("power") {
            Some(StatKind::Power)
        } else if cur.eat_ci("toughness") {
            Some(StatKind::Toughness)
        } else if cur.eat_words(&["mana", "value"]) {
            Some(StatKind::ManaValue)
        } else {
            None
        };
        if let Some(s) = stat
            && cur.is_empty()
        {
            return Some(Amount::Stat {
                of: Ref::SelfObj,
                stat: s,
                lki: false,
            });
        }
    }

    cur.restore(0);
    if cur.eat_words(&["equipped", "creature's", "power"]) && cur.is_empty() {
        return Some(Amount::Stat {
            of: Ref::AttachedTo,
            stat: StatKind::Power,
            lki: false,
        });
    }

    cur.restore(0);
    if (cur.eat_words(&[
        "the", "amount", "of", "life", "you", "gained", "this", "turn",
    ]) || cur.eat_words(&[
        "the", "amount", "of", "life", "you've", "gained", "this", "turn",
    ])) && cur.is_empty()
    {
        return Some(Amount::LifeGainedThisTurn);
    }

    cur.restore(0);
    if cur.eat_words(&["your", "speed"]) && cur.is_empty() {
        return Some(Amount::PlayerSpeed {
            player: PlayerRef::You,
        });
    }

    cur.restore(0);
    if cur.eat_words(&["the", "number", "of"]) {
        return parse_cost_metric_amount_tokens(cur.remaining(), ctx);
    }

    None
}

fn parse_devotion_color_list(tokens: &[Token<'_>]) -> Option<ColorMask> {
    let mut mask = ColorMask::empty();
    let mut any = false;
    for tok in tokens {
        if tok.is_punct(Punct::Comma) || tok.eq_ci("and") {
            continue;
        }
        let c = parse_color_word(tok.raw)?;
        mask |= c;
        any = true;
    }
    if any { Some(mask) } else { None }
}

fn parse_cost_object_phrase(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    default_zone: ZoneMask,
    strip_trailing_card: bool,
) -> Option<ObjectFilter> {
    let (f, used) = parse_object_filter(
        tokens,
        ctx,
        FilterOpts {
            default_zone,
            allow_plural_and_disjunction: true,
            allow_spell_noun: false,
            strip_trailing_card,
            expand_outlaw_subtypes: false,
        },
        &NullScope,
    )?;
    if used != tokens.len() {
        return None;
    }
    Some(f)
}

fn parse_cost_metric_amount_tokens(tokens: &[Token<'_>], ctx: &mut ParseCtx<'_>) -> Option<Amount> {
    let cp = ctx.interner.checkpoint();
    let res = parse_cost_metric_amount_tokens_inner(tokens, ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_cost_metric_amount_tokens_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<Amount> {
    let mut toks = tokens;
    if toks.first().is_some_and(|t| t.eq_ci("a") || t.eq_ci("an")) {
        toks = &toks[1..];
    }
    if toks.is_empty() {
        return None;
    }
    let mut cur = TokenCursor::new(toks);

    // "other spell cast this turn" (Thrasta)
    if cur.eat_words(&["other", "spell", "cast", "this", "turn"]) && cur.is_empty() {
        return Some(Amount::StormCount);
    }
    cur.restore(0);

    // "[total | 1 ]life your opponents [have ]lost this turn"
    {
        let mut c2 = TokenCursor::new(toks);
        let _ = c2.eat_ci("total") || c2.eat_ci("1");
        if c2.eat_words(&["life", "your", "opponents"]) {
            let _ = c2.eat_ci("have");
            if c2.eat_words(&["lost", "this", "turn"]) && c2.is_empty() {
                return Some(Amount::OpponentsLifeLostThisTurn);
            }
        }
    }

    // "the total mana value of (its targets|the permanents it targets|target permanents)"
    if (cur.eat_words(&["the", "total", "mana", "value", "of", "its", "targets"])
        || cur.eat_words(&[
            "the",
            "total",
            "mana",
            "value",
            "of",
            "the",
            "permanents",
            "it",
            "targets",
        ])
        || cur.eat_words(&[
            "the",
            "total",
            "mana",
            "value",
            "of",
            "target",
            "permanents",
        ]))
        && cur.is_empty()
    {
        return Some(Amount::TargetsTotalManaValue);
    }
    cur.restore(0);

    // Compound: "<A> and each <B>" (Gargantuan Leech)
    if let Some(pos) = toks
        .windows(2)
        .position(|w| w[0].eq_ci("and") && w[1].eq_ci("each"))
        && pos > 0
        && pos + 2 < toks.len()
        && let Some(a1) = parse_cost_metric_amount_tokens(&toks[..pos], ctx)
        && let Some(a2) = parse_cost_metric_amount_tokens(&toks[pos + 2..], ctx)
    {
        return Some(Amount::Sum { args: vec![a1, a2] });
    }

    // "[<phrase>] card[s] you own in exile and in your graveyard" (Huskburster Swarm)
    if toks.len() >= 8
        && toks[toks.len() - 8].eq_ci("you")
        && toks[toks.len() - 7].eq_ci("own")
        && toks[toks.len() - 6].eq_ci("in")
        && toks[toks.len() - 5].eq_ci("exile")
        && toks[toks.len() - 4].eq_ci("and")
        && toks[toks.len() - 3].eq_ci("in")
        && toks[toks.len() - 2].eq_ci("your")
        && toks[toks.len() - 1].eq_ci("graveyard")
        && let Some(card_tok) = toks.get(toks.len().wrapping_sub(9))
        && (card_tok.eq_ci("card") || card_tok.eq_ci("cards"))
    {
        let prefix = &toks[..toks.len() - 9];
        let base_f = if prefix.is_empty() {
            ObjectFilter::default()
        } else {
            let mut with_card = prefix.to_vec();
            with_card.push(card_tok.clone());
            let f = parse_cost_object_phrase(&with_card, ctx, ZoneMask::empty(), true)?;
            if f.controller.is_some() {
                return None;
            }
            f
        };
        let mut ex_f = base_f.clone();
        ex_f.zones = ZoneMask::EXILE;
        ex_f.owner = Some(RelativeControl::You);
        let mut gy_f = base_f;
        gy_f.zones = ZoneMask::GRAVEYARD;
        gy_f.owner = Some(RelativeControl::You);
        return Some(Amount::Sum {
            args: vec![
                Amount::Count { of: Box::new(ex_f) },
                Amount::Count { of: Box::new(gy_f) },
            ],
        });
    }

    // "target[s]" / "<phrase> (it targets|they target)"
    if toks.len() == 1 && (toks[0].eq_ci("target") || toks[0].eq_ci("targets")) {
        return Some(Amount::TargetsCount);
    }
    if toks.len() >= 3
        && ((toks[toks.len() - 2].eq_ci("it") && toks[toks.len() - 1].eq_ci("targets"))
            || (toks[toks.len() - 2].eq_ci("they") && toks[toks.len() - 1].eq_ci("target")))
    {
        let mut tf =
            parse_cost_object_phrase(&toks[..toks.len() - 2], ctx, ZoneMask::empty(), true)?;
        if tf.controller.is_none()
            && let Some(pos) = toks[..toks.len() - 2]
                .windows(2)
                .position(|w| w[0].eq_ci("an") && w[1].eq_ci("opponent"))
        {
            let _ = pos;
        }
        let _ = &mut tf;
        return Some(Amount::TargetsMatchingCount {
            filter: Box::new(tf),
        });
    }

    // "target[s] beyond the first"
    if (cur.eat_words(&["target", "beyond", "the", "first"])
        || cur.eat_words(&["targets", "beyond", "the", "first"]))
        && cur.is_empty()
    {
        return Some(Amount::TargetsBeyondFirst);
    }
    cur.restore(0);

    // "opponent[s] you have"
    if (cur.eat_words(&["opponent", "you", "have"]) || cur.eat_words(&["opponents", "you", "have"]))
        && cur.is_empty()
    {
        return Some(Amount::OpponentCount);
    }
    cur.restore(0);

    // Cost-paid count: "card[s] exiled this way", "creature/permanent sacrificed this way", "counter[s] removed this way"
    if toks.len() >= 3 && toks[toks.len() - 2].eq_ci("this") && toks[toks.len() - 1].eq_ci("way") {
        let noun = &toks[toks.len() - 3];
        if noun.eq_ci("exiled")
            && toks
                .get(toks.len().wrapping_sub(4))
                .is_some_and(|t| t.eq_ci("card") || t.eq_ci("cards"))
        {
            return Some(Amount::CostPaidCount {
                kind: CostPaidCountKind::Exiled,
            });
        }
        if noun.eq_ci("sacrificed")
            && toks.len() >= 4
            && matches!(
                toks[toks.len() - 4].raw.to_ascii_lowercase().as_str(),
                "creature"
                    | "creatures"
                    | "permanent"
                    | "permanents"
                    | "artifact"
                    | "artifacts"
                    | "land"
                    | "lands"
            )
        {
            return Some(Amount::CostPaidCount {
                kind: CostPaidCountKind::Sacrificed,
            });
        }
        if noun.eq_ci("removed")
            && toks
                .get(toks.len().wrapping_sub(4))
                .is_some_and(|t| t.eq_ci("counter") || t.eq_ci("counters"))
        {
            return Some(Amount::CostPaidCount {
                kind: CostPaidCountKind::RemovedCounters,
            });
        }
    }

    // "time you've cast (your|a) commander from the command zone this game"
    if cur.eat_words(&["time", "you've", "cast"])
        && (cur.eat_ci("your") || cur.eat_ci("a"))
        && cur.eat_words(&[
            "commander",
            "from",
            "the",
            "command",
            "zone",
            "this",
            "game",
        ])
        && cur.is_empty()
    {
        return Some(Amount::CommanderCastCount);
    }
    cur.restore(0);

    // "creature[s] in your party"
    if (cur.eat_words(&["creature", "in", "your", "party"])
        || cur.eat_words(&["creatures", "in", "your", "party"]))
        && cur.is_empty()
    {
        return Some(Amount::PartyCount {
            player: PlayerRef::You,
        });
    }
    cur.restore(0);

    // "basic land type[s] among lands you control"
    if (cur.eat_words(&["basic", "land", "type", "among", "lands", "you", "control"])
        || cur.eat_words(&["basic", "land", "types", "among", "lands", "you", "control"]))
        && cur.is_empty()
    {
        return Some(Amount::Domain {
            player: PlayerRef::You,
        });
    }
    cur.restore(0);

    // "different mana value[s] among [<phrase> ]card[s] in your graveyard"
    if (cur.eat_words(&["different", "mana", "value", "among"])
        || cur.eat_words(&["different", "mana", "values", "among"]))
        && cur.remaining().len() >= 4
    {
        let rem = cur.remaining();
        if rem[rem.len() - 3].eq_ci("in")
            && rem[rem.len() - 2].eq_ci("your")
            && rem[rem.len() - 1].eq_ci("graveyard")
        {
            let mid = &rem[..rem.len() - 3];
            let filter = if mid.len() == 1 && (mid[0].eq_ci("card") || mid[0].eq_ci("cards")) {
                None
            } else {
                let f = parse_cost_object_phrase(mid, ctx, ZoneMask::empty(), true)?;
                if f.controller.is_some() {
                    return None;
                }
                Some(Box::new(f))
            };
            return Some(Amount::DistinctManaValuesInGraveyard { filter });
        }
    }
    cur.restore(0);

    // "creature type[s] among creatures you control"
    if (cur.eat_words(&["creature", "type", "among", "creatures", "you", "control"])
        || cur.eat_words(&["creature", "types", "among", "creatures", "you", "control"]))
        && cur.is_empty()
    {
        return Some(Amount::DistinctCreatureTypesControlled);
    }
    cur.restore(0);

    // "color[s] among permanents you control"
    if (cur.eat_words(&["color", "among", "permanents", "you", "control"])
        || cur.eat_words(&["colors", "among", "permanents", "you", "control"]))
        && cur.is_empty()
    {
        return Some(Amount::ColorsAmongPermanents {
            filter: None,
            player: PlayerRef::You,
        });
    }
    cur.restore(0);

    // "differently named <phrase> you control"
    if cur.eat_words(&["differently", "named"]) {
        let (group_toks, stripped_you) = strip_trailing_you_control(cur.remaining());
        if stripped_you && !group_toks.is_empty() {
            let filter = if group_toks.len() == 1
                && (group_toks[0].eq_ci("permanent") || group_toks[0].eq_ci("permanents"))
            {
                None
            } else {
                let f = parse_cost_object_phrase(group_toks, ctx, ZoneMask::empty(), false)?;
                if f.controller.is_some() {
                    return None;
                }
                Some(Box::new(f))
            };
            return Some(Amount::DistinctNamesControlled { filter });
        }
    }
    cur.restore(0);

    // "counter[s] among players and permanents"
    if (cur.eat_words(&["counter", "among", "players", "and", "permanents"])
        || cur.eat_words(&["counters", "among", "players", "and", "permanents"]))
        && cur.is_empty()
    {
        return Some(Amount::CountersAmongPlayersAndPermanents { counter: None });
    }
    cur.restore(0);

    // "permanent[s] put into a graveyard from the battlefield this turn"
    if cur.remaining().len() >= 10
        && (cur.eat_ci("permanent") || cur.eat_ci("permanents"))
        && cur.eat_words(&[
            "put",
            "into",
            "a",
            "graveyard",
            "from",
            "the",
            "battlefield",
            "this",
            "turn",
        ])
        && cur.is_empty()
    {
        return Some(Amount::GravestormCount);
    }
    cur.restore(0);

    // "creature[s] you attacked with this turn" / "creature[s] that attacked this turn"
    if (cur.eat_words(&["creature", "you", "attacked", "with", "this", "turn"])
        || cur.eat_words(&["creatures", "you", "attacked", "with", "this", "turn"]))
        && cur.is_empty()
    {
        return Some(Amount::CreaturesAttackedWithThisTurn);
    }
    cur.restore(0);
    if (cur.eat_words(&["creature", "that", "attacked", "this", "turn"])
        || cur.eat_words(&["creatures", "that", "attacked", "this", "turn"]))
        && cur.is_empty()
    {
        return Some(Amount::CreaturesAttackedTotalThisTurn);
    }
    cur.restore(0);

    // "land[s] your opponents control that could produce {C}"
    if (cur.eat_ci("land") || cur.eat_ci("lands"))
        && cur.eat_words(&["your", "opponents", "control", "that", "could", "produce"])
        && cur
            .peek()
            .is_some_and(|t| t.raw.eq_ignore_ascii_case("{C}"))
        && cur.remaining().len() == 1
    {
        return Some(Amount::LandsOpponentControlsProducingColorless);
    }
    cur.restore(0);

    // "opponent[s] who (was|were) dealt damage this turn"
    if (cur.eat_ci("opponent") || cur.eat_ci("opponents"))
        && cur.eat_ci("who")
        && (cur.eat_ci("was") || cur.eat_ci("were"))
        && cur.eat_words(&["dealt", "damage", "this", "turn"])
        && cur.is_empty()
    {
        return Some(Amount::OpponentsDealtDamageThisTurn);
    }
    cur.restore(0);

    // "card[s] with the same name as (that|this) spell in your graveyard"
    if (cur.eat_ci("card") || cur.eat_ci("cards"))
        && cur.eat_words(&["with", "the", "same", "name", "as"])
        && (cur.eat_ci("that") || cur.eat_ci("this"))
        && cur.eat_words(&["spell", "in", "your", "graveyard"])
        && cur.is_empty()
    {
        return Some(Amount::SameNameInGraveyardAsSpell);
    }
    cur.restore(0);

    // "[<p>] card[s] you own in exile"
    if toks.len() >= 4
        && (toks[toks.len() - 4].eq_ci("card") || toks[toks.len() - 4].eq_ci("cards"))
        && toks[toks.len() - 3].eq_ci("you")
        && toks[toks.len() - 2].eq_ci("own")
        && toks[toks.len() - 1].eq_ci("in")
    {
        // handled below
    }
    if toks.len() >= 5
        && (toks[toks.len() - 5].eq_ci("card") || toks[toks.len() - 5].eq_ci("cards"))
        && toks[toks.len() - 4].eq_ci("you")
        && toks[toks.len() - 3].eq_ci("own")
        && toks[toks.len() - 2].eq_ci("in")
        && toks[toks.len() - 1].eq_ci("exile")
    {
        let p_toks = &toks[..toks.len() - 4];
        let mut f = if p_toks.len() == 1 {
            ObjectFilter::default()
        } else {
            parse_cost_object_phrase(p_toks, ctx, ZoneMask::empty(), true)?
        };
        if f.controller.is_some() {
            return None;
        }
        f.zones = ZoneMask::EXILE;
        f.owner = Some(RelativeControl::You);
        return Some(Amount::Count { of: Box::new(f) });
    }

    // "card[s] in your graveyard" / "card[s] in your hand" / "card[s] in all graveyards"
    if (cur.eat_words(&["card", "in", "your", "graveyard"])
        || cur.eat_words(&["cards", "in", "your", "graveyard"]))
        && cur.is_empty()
    {
        return Some(Amount::CardsIn {
            zone: Zone::Graveyard,
            of: PlayerRef::You,
        });
    }
    cur.restore(0);
    if (cur.eat_words(&["card", "in", "your", "hand"])
        || cur.eat_words(&["cards", "in", "your", "hand"]))
        && cur.is_empty()
    {
        return Some(Amount::CardsIn {
            zone: Zone::Hand,
            of: PlayerRef::You,
        });
    }
    cur.restore(0);
    if (cur.eat_words(&["card", "in", "all", "graveyards"])
        || cur.eat_words(&["cards", "in", "all", "graveyards"]))
        && cur.is_empty()
    {
        return Some(Amount::Count {
            of: Box::new(ObjectFilter {
                zones: ZoneMask::GRAVEYARD,
                ..Default::default()
            }),
        });
    }
    cur.restore(0);

    // "card type[s] among cards in your graveyard"
    if (cur.eat_words(&["card", "type", "among", "cards", "in", "your", "graveyard"])
        || cur.eat_words(&["card", "types", "among", "cards", "in", "your", "graveyard"]))
        && cur.is_empty()
    {
        return Some(Amount::DistinctCardTypesInZone {
            zone: Zone::Graveyard,
            whose: ZoneScopeWhose::You,
        });
    }
    cur.restore(0);

    // "[<counter>] counter[s] on ~"
    if toks.len() >= 3
        && toks[toks.len() - 2].eq_ci("on")
        && matches!(toks[toks.len() - 1].kind, TokenKind::SelfRef)
        && (toks[toks.len() - 3].eq_ci("counter") || toks[toks.len() - 3].eq_ci("counters"))
    {
        let ct = if toks.len() == 3 {
            CounterTarget::Any
        } else {
            let name = toks[..toks.len() - 3]
                .iter()
                .map(|t| t.raw)
                .collect::<Vec<_>>()
                .join(" ");
            lookup_counter_target(&name, ctx.interner)?
        };
        return Some(Amount::CountersOn {
            counter: ct,
            of: Ref::SelfObj,
        });
    }

    // "<p> card[s] in your graveyard" / "<p> card[s] in all graveyards"
    if toks.len() >= 5
        && toks[toks.len() - 3].eq_ci("in")
        && toks[toks.len() - 2].eq_ci("your")
        && toks[toks.len() - 1].eq_ci("graveyard")
        && (toks[toks.len() - 4].eq_ci("card") || toks[toks.len() - 4].eq_ci("cards"))
    {
        let p_toks = &toks[..toks.len() - 3];
        let mut f = if p_toks.len() == 2 && p_toks[0].eq_ci("permanent") {
            // TS Diamond Weapon parity: "permanent card in your graveyard" -> types_any: 0
            ObjectFilter::default()
        } else {
            parse_cost_object_phrase(p_toks, ctx, ZoneMask::empty(), true)?
        };
        if f.controller.is_some() {
            return None;
        }
        f.zones = ZoneMask::GRAVEYARD;
        f.owner = Some(RelativeControl::You);
        return Some(Amount::Count { of: Box::new(f) });
    }
    if toks.len() >= 5
        && toks[toks.len() - 3].eq_ci("in")
        && toks[toks.len() - 2].eq_ci("all")
        && toks[toks.len() - 1].eq_ci("graveyards")
        && (toks[toks.len() - 4].eq_ci("card") || toks[toks.len() - 4].eq_ci("cards"))
    {
        let p_toks = &toks[..toks.len() - 3];
        let mut f = parse_cost_object_phrase(p_toks, ctx, ZoneMask::empty(), true)?;
        if f.controller.is_some() {
            return None;
        }
        f.zones = ZoneMask::GRAVEYARD;
        return Some(Amount::Count { of: Box::new(f) });
    }

    // "<p> you control" / "<p> (your opponents control|an opponent controls)" / "<p> on the battlefield"
    if toks.len() >= 3 && toks[toks.len() - 2].eq_ci("you") && toks[toks.len() - 1].eq_ci("control")
    {
        let mut f =
            parse_cost_object_phrase(&toks[..toks.len() - 2], ctx, ZoneMask::empty(), false)?;
        if f.controller.is_some() {
            return None;
        }
        f.controller = Some(RelativeControl::You);
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::Count { of: Box::new(f) });
    }
    if toks.len() >= 4
        && ((toks[toks.len() - 3].eq_ci("your")
            && toks[toks.len() - 2].eq_ci("opponents")
            && toks[toks.len() - 1].eq_ci("control"))
            || (toks[toks.len() - 3].eq_ci("an")
                && toks[toks.len() - 2].eq_ci("opponent")
                && toks[toks.len() - 1].eq_ci("controls")))
    {
        let mut f =
            parse_cost_object_phrase(&toks[..toks.len() - 3], ctx, ZoneMask::empty(), false)?;
        if f.controller.is_some() {
            return None;
        }
        f.controller = Some(RelativeControl::Opponent);
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::Count { of: Box::new(f) });
    }
    if toks.len() >= 4
        && toks[toks.len() - 3].eq_ci("on")
        && toks[toks.len() - 2].eq_ci("the")
        && toks[toks.len() - 1].eq_ci("battlefield")
    {
        let mut f =
            parse_cost_object_phrase(&toks[..toks.len() - 3], ctx, ZoneMask::empty(), false)?;
        if f.controller.is_some() {
            return None;
        }
        f.zones = ZoneMask::BATTLEFIELD;
        return Some(Amount::Count { of: Box::new(f) });
    }

    // Direct controller / combat / tapped qualified phrase (e.g. "attacking creature", "creature your opponents control with ...")
    if let Some(mut f) = parse_cost_object_phrase(toks, ctx, ZoneMask::empty(), false) {
        if f.controller.is_some() {
            f.zones = ZoneMask::BATTLEFIELD;
            return Some(Amount::Count { of: Box::new(f) });
        }
        if f.zones.is_empty()
            && f.flags.intersects(
                FilterFlags::ATTACKING
                    | FilterFlags::NOT_ATTACKING
                    | FilterFlags::BLOCKING
                    | FilterFlags::NOT_BLOCKING
                    | FilterFlags::ATTACKING_OR_BLOCKING
                    | FilterFlags::TAPPED
                    | FilterFlags::UNTAPPED,
            )
        {
            f.zones = ZoneMask::BATTLEFIELD;
            return Some(Amount::Count { of: Box::new(f) });
        }
    }

    None
}

fn parse_spell_filter_phrase_tokens(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<(ObjectFilter, bool)> {
    let cp = ctx.interner.checkpoint();
    let res = parse_spell_filter_phrase_tokens_inner(tokens, ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_spell_filter_phrase_tokens_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<(ObjectFilter, bool)> {
    if tokens.is_empty() {
        return Some((ObjectFilter::default(), false));
    }
    let mut toks = tokens;
    let mut other = false;
    if toks.first().is_some_and(|t| t.eq_ci("other")) {
        other = true;
        toks = &toks[1..];
    }
    if toks.is_empty() {
        return Some((ObjectFilter::default(), other));
    }

    // Remove intermediate "spell"/"spells" tokens when followed by "with"/"without"/"that" or "and"
    // or at end of phrase, and note if the head is "nonpermanent(s)" or "permanent(s)".
    let mut cleaned: Vec<Token<'_>> = Vec::with_capacity(toks.len());
    for (i, t) in toks.iter().enumerate() {
        if (t.eq_ci("spell") || t.eq_ci("spells"))
            && (i + 1 == toks.len()
                || toks[i + 1].eq_ci("with")
                || toks[i + 1].eq_ci("without")
                || toks[i + 1].eq_ci("that")
                || toks[i + 1].eq_ci("and"))
        {
            continue;
        }
        cleaned.push(t.clone());
    }
    if cleaned.is_empty() {
        return Some((ObjectFilter::default(), other));
    }

    // "nonpermanent(s) [with ...]" -> instant or sorcery
    if cleaned[0].eq_ci("nonpermanent") || cleaned[0].eq_ci("nonpermanents") {
        let mut rest_filter = ObjectFilter {
            types_any: CardTypeMask::INSTANT | CardTypeMask::SORCERY,
            ..Default::default()
        };
        if cleaned.len() > 1 {
            // prepend "card" or parse qualifier directly via filter
            let mut sub = vec![Token {
                kind: TokenKind::Word("card"),
                raw: "card",
                span: cleaned[0].span,
            }];
            sub.extend_from_slice(&cleaned[1..]);
            let mut f = parse_cost_object_phrase(&sub, ctx, ZoneMask::empty(), true)?;
            if f.controller.is_some() || f.flags.contains(FilterFlags::OTHER) {
                return None;
            }
            f.types_any |= CardTypeMask::INSTANT | CardTypeMask::SORCERY;
            rest_filter = f;
        }
        return Some((rest_filter, other));
    }

    // "instant and sorcery [with ...]" / "sorcery and instant [with ...]"
    if cleaned.len() >= 3
        && ((cleaned[0].eq_ci("instant") && cleaned[1].eq_ci("and") && cleaned[2].eq_ci("sorcery"))
            || (cleaned[0].eq_ci("sorcery")
                && cleaned[1].eq_ci("and")
                && cleaned[2].eq_ci("instant")))
    {
        let mut rest_filter = ObjectFilter {
            types_any: CardTypeMask::INSTANT | CardTypeMask::SORCERY,
            ..Default::default()
        };
        if cleaned.len() > 3 {
            let mut sub = vec![Token {
                kind: TokenKind::Word("card"),
                raw: "card",
                span: cleaned[2].span,
            }];
            sub.extend_from_slice(&cleaned[3..]);
            let mut f = parse_cost_object_phrase(&sub, ctx, ZoneMask::empty(), true)?;
            if f.controller.is_some() || f.flags.contains(FilterFlags::OTHER) {
                return None;
            }
            f.types_any |= CardTypeMask::INSTANT | CardTypeMask::SORCERY;
            rest_filter = f;
        }
        return Some((rest_filter, other));
    }

    // Check if head noun is "permanent(s)"
    let qual_pos = cleaned
        .iter()
        .position(|t| t.eq_ci("with") || t.eq_ci("without") || t.eq_ci("that"))
        .unwrap_or(cleaned.len());
    let is_perm_head = qual_pos > 0
        && (cleaned[qual_pos - 1].eq_ci("permanent") || cleaned[qual_pos - 1].eq_ci("permanents"));

    // Parse phrase directly or with "card" inserted before qualifiers
    let cp = ctx.interner.checkpoint();
    let f_opt = parse_cost_object_phrase(&cleaned, ctx, ZoneMask::empty(), true).or_else(|| {
        ctx.interner.rollback(cp);
        let mut inserted = Vec::with_capacity(cleaned.len() + 1);
        inserted.extend_from_slice(&cleaned[..qual_pos]);
        inserted.push(Token {
            kind: TokenKind::Word("card"),
            raw: "card",
            span: cleaned[qual_pos.saturating_sub(1)].span,
        });
        inserted.extend_from_slice(&cleaned[qual_pos..]);
        parse_cost_object_phrase(&inserted, ctx, ZoneMask::empty(), true)
    });
    let mut base_filter = f_opt?;
    if base_filter.controller.is_some() || base_filter.flags.contains(FilterFlags::OTHER) {
        return None;
    }
    if is_perm_head && base_filter.types_any.is_empty() && base_filter.types_all.is_empty() {
        base_filter.types_any = CardTypeMask::ARTIFACT
            | CardTypeMask::BATTLE
            | CardTypeMask::CREATURE
            | CardTypeMask::ENCHANTMENT
            | CardTypeMask::PLANESWALKER;
    }
    Some((base_filter, other))
}

enum TargetClauseMatch {
    SelfObj,
    Attached,
    YouOrFilter(ObjectFilter),
    Filter(ObjectFilter),
}

fn parse_spell_target_clause_filter_tokens(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<TargetClauseMatch> {
    let cp = ctx.interner.checkpoint();
    let res = parse_spell_target_clause_filter_tokens_inner(tokens, ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_spell_target_clause_filter_tokens_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
) -> Option<TargetClauseMatch> {
    if tokens.len() == 1 && matches!(tokens[0].kind, TokenKind::SelfRef) {
        return Some(TargetClauseMatch::SelfObj);
    }
    let mut cur = TokenCursor::new(tokens);
    if (cur.eat_ci("enchanted") || cur.eat_ci("equipped"))
        && cur.bump().is_some_and(|t| {
            t.eq_ci("creature")
                || t.eq_ci("permanent")
                || t.eq_ci("artifact")
                || t.eq_ci("land")
                || t.eq_ci("planeswalker")
        })
        && cur.is_empty()
    {
        return Some(TargetClauseMatch::Attached);
    }
    cur.restore(0);

    // "you or (a|an) <phrase> you control"
    if cur.eat_words(&["you", "or"]) && (cur.eat_ci("a") || cur.eat_ci("an")) {
        let (inner, stripped_you) = strip_trailing_you_control(cur.remaining());
        if stripped_you && !inner.is_empty() {
            let mut f = if inner.len() == 1 && inner[0].eq_ci("permanent") {
                ObjectFilter::default()
            } else {
                let f0 = parse_cost_object_phrase(inner, ctx, ZoneMask::empty(), false)?;
                if f0.controller.is_some() {
                    return None;
                }
                f0
            };
            f.controller = Some(RelativeControl::You);
            return Some(TargetClauseMatch::YouOrFilter(f));
        }
    }
    cur.restore(0);

    let _ = cur.eat_words(&["one", "or", "more"]) || cur.eat_ci("a") || cur.eat_ci("an");
    let rem = cur.remaining();
    if rem.is_empty() {
        return None;
    }
    // Check trailing "an opponent controls" / "your opponents control"
    let (body, opp_ctrl) = if rem.len() >= 4
        && ((rem[rem.len() - 3].eq_ci("an")
            && rem[rem.len() - 2].eq_ci("opponent")
            && rem[rem.len() - 1].eq_ci("controls"))
            || (rem[rem.len() - 3].eq_ci("your")
                && rem[rem.len() - 2].eq_ci("opponents")
                && rem[rem.len() - 1].eq_ci("control")))
    {
        (&rem[..rem.len() - 3], true)
    } else {
        (rem, false)
    };

    let mut f = if body.len() == 1 && (body[0].eq_ci("permanent") || body[0].eq_ci("permanents")) {
        ObjectFilter::default()
    } else {
        parse_cost_object_phrase(body, ctx, ZoneMask::empty(), false)?
    };
    if opp_ctrl {
        f.controller = Some(RelativeControl::Opponent);
    }
    Some(TargetClauseMatch::Filter(f))
}

fn parse_self_cost_condition_tokens(
    tokens: &[Token<'_>],
    raw: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<Condition> {
    if tokens.is_empty() {
        return None;
    }
    // Note: strip trailing "or" / "and" condition combinators at top level cleanly
    // 1. "you have no other <p> cards in [your] hand or if the only other <p> cards in your hand are named ~"
    if let Some(or_idx) = tokens
        .windows(2)
        .position(|w| w[0].eq_ci("or") && w[1].eq_ci("if"))
    {
        let left = &tokens[..or_idx];
        let right = &tokens[or_idx + 2..];
        let mut lc = TokenCursor::new(left);
        let mut rc = TokenCursor::new(right);
        if lc.eat_words(&["you", "have", "no", "other"]) && rc.eat_words(&["the", "only", "other"])
        {
            let lrem = lc.remaining();
            let rrem = rc.remaining();
            let l_in = lrem.iter().position(|t| t.eq_ci("in"));
            let r_in = rrem.iter().position(|t| t.eq_ci("in"));
            if let (Some(li), Some(ri)) = (l_in, r_in)
                && li >= 2
                && (lrem[li - 1].eq_ci("card") || lrem[li - 1].eq_ci("cards"))
                && rrem.get(ri + 1).is_some_and(|t| t.eq_ci("your"))
                && rrem.get(ri + 2).is_some_and(|t| t.eq_ci("hand"))
                && rrem.get(ri + 3).is_some_and(|t| t.eq_ci("are"))
                && rrem.get(ri + 4).is_some_and(|t| t.eq_ci("named"))
                && rrem
                    .get(ri + 5)
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && rrem.len() == ri + 6
            {
                let cp = ctx.interner.checkpoint();
                if let Some(f) = parse_cost_object_phrase(&lrem[..li], ctx, ZoneMask::empty(), true)
                    && f.controller.is_none()
                {
                    return Some(Condition::OnlyNamedOrNoMatchingInHand {
                        filter: Box::new(f),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    // 2. Compound OR / AND clauses
    if let Some(or_pos) = tokens.iter().position(|t| t.eq_ci("or"))
        && or_pos > 0
        && or_pos + 1 < tokens.len()
    {
        let rhs = if tokens[or_pos + 1].eq_ci("if") {
            &tokens[or_pos + 2..]
        } else {
            &tokens[or_pos + 1..]
        };
        let is_clause_head = |t: &Token<'_>| {
            t.eq_ci("you")
                || t.eq_ci("an")
                || t.eq_ci("it")
                || t.eq_ci("there")
                || t.eq_ci("a")
                || t.eq_ci("your")
                || t.eq_ci("no")
                || t.as_u32().is_some()
        };
        if is_clause_head(&tokens[0]) && !rhs.is_empty() && is_clause_head(&rhs[0]) {
            let cp = ctx.interner.checkpoint();
            if let (Some(c1), Some(c2)) = (
                parse_self_cost_condition_tokens(&tokens[..or_pos], raw, ctx),
                parse_self_cost_condition_tokens(rhs, raw, ctx),
            ) {
                return Some(Condition::Or { of: vec![c1, c2] });
            }
            ctx.interner.rollback(cp);
        }
    }

    // "you control (a|an) <f1> and (a|an) <f2>" (Of One Mind)
    if let Some(and_pos) = tokens.iter().position(|t| t.eq_ci("and")) {
        let mut lc = TokenCursor::new(&tokens[..and_pos]);
        let mut rc = TokenCursor::new(&tokens[and_pos + 1..]);
        if lc.eat_words(&["you", "control"])
            && (lc.eat_ci("a") || lc.eat_ci("an"))
            && (rc.eat_ci("a") || rc.eat_ci("an"))
            && !lc.is_empty()
            && !rc.is_empty()
        {
            let cp = ctx.interner.checkpoint();
            if let (Some(mut f1), Some(mut f2)) = (
                parse_cost_object_phrase(lc.remaining(), ctx, ZoneMask::empty(), false),
                parse_cost_object_phrase(rc.remaining(), ctx, ZoneMask::empty(), false),
            ) && f1.controller.is_none()
                && f2.controller.is_none()
            {
                f1.controller = Some(RelativeControl::You);
                f1.zones = ZoneMask::BATTLEFIELD;
                f2.controller = Some(RelativeControl::You);
                f2.zones = ZoneMask::BATTLEFIELD;
                return Some(Condition::And {
                    of: vec![
                        Condition::Compare {
                            left: Box::new(Amount::Count { of: Box::new(f1) }),
                            op: CompareOp::Ge,
                            right: Box::new(Amount::Const { n: 1 }),
                        },
                        Condition::Compare {
                            left: Box::new(Amount::Count { of: Box::new(f2) }),
                            op: CompareOp::Ge,
                            right: Box::new(Amount::Const { n: 1 }),
                        },
                    ],
                });
            }
            ctx.interner.rollback(cp);
        }
    }

    let mut cur = TokenCursor::new(tokens);
    if cur.eat_words(&[
        "it", "targets", "a", "spell", "or", "ability", "that", "targets",
    ]) && !cur.is_empty()
    {
        let cp = ctx.interner.checkpoint();
        let rem = cur.remaining();
        let (f, used) = parse_object_filter(
            rem,
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            &NullScope,
        )?;
        if used == rem.len() {
            return Some(Condition::TargetsStackSpell {
                filter: Some(Box::new(ObjectFilter {
                    zones: ZoneMask::STACK,
                    stack_kind: Some(StackTargetKind::SpellOrAbility),
                    extra: vec![ObjectPredicate::TargetsFilter {
                        filter: Box::new(f),
                    }],
                    ..Default::default()
                })),
            });
        }
        ctx.interner.rollback(cp);
    }
    cur.restore(0);
    if cur.eat_words(&["a", "player's", "life", "total", "is"])
        && (cur.eat_words(&["less", "than", "or", "equal", "to"])
            || cur.eat_words(&["greater", "than", "or", "equal", "to"])
            || cur.eat_words(&["less", "than"])
            || cur.eat_words(&["greater", "than"]))
    {
        let op = if tokens[..cur.pos()].iter().any(|t| t.eq_ci("less")) {
            if tokens[..cur.pos()].iter().any(|t| t.eq_ci("equal")) {
                CompareOp::Le
            } else {
                CompareOp::Lt
            }
        } else if tokens[..cur.pos()].iter().any(|t| t.eq_ci("equal")) {
            CompareOp::Ge
        } else {
            CompareOp::Gt
        };
        if (cur.eat_words(&["half", "their", "starting", "life", "total"])
            || cur.eat_words(&["half", "his", "or", "her", "starting", "life", "total"]))
            && cur.is_empty()
        {
            return Some(Condition::Or {
                of: vec![
                    Condition::LifeCompareHalfStarting {
                        who: LifeCompareWho::You,
                        op,
                    },
                    Condition::LifeCompareHalfStarting {
                        who: LifeCompareWho::AnyOpponent,
                        op,
                    },
                ],
            });
        }
    }
    cur.restore(0);
    if cur.eat_words(&["you", "weren't", "the", "starting", "player"]) && cur.is_empty() {
        return Some(Condition::NotStartingPlayer);
    }
    cur.restore(0);
    if cur.eat_words(&["an", "opponent", "gained", "life", "this", "turn"]) && cur.is_empty() {
        return Some(Condition::OpponentGainedLifeThisTurn);
    }
    cur.restore(0);
    if cur.eat_words(&["an", "opponent", "cast"])
        && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_words(&["or", "more", "spells", "this", "turn"])
        && cur.is_empty()
    {
        return Some(Condition::OpponentCastSpellsAtLeast {
            n: n as u16,
            filter: None,
        });
    }
    cur.restore(0);
    if cur.eat_words(&["an", "opponent", "controls", "at", "least"])
        && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_ci("more")
    {
        let rem = cur.remaining();
        if rem.len() >= 3 && rem[rem.len() - 2].eq_ci("than") && rem[rem.len() - 1].eq_ci("you") {
            let mid = &rem[..rem.len() - 2];
            let cp = ctx.interner.checkpoint();
            let f_opt =
                if mid.len() == 1 && (mid[0].eq_ci("permanent") || mid[0].eq_ci("permanents")) {
                    Some(ObjectFilter::default())
                } else {
                    parse_cost_object_phrase(mid, ctx, ZoneMask::empty(), false)
                };
            if let Some(f) = f_opt
                && f.controller.is_none()
            {
                return Some(Condition::OpponentControlsAtLeastNMore {
                    filter: Box::new(f),
                    n: n as u16,
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    cur.restore(0);
    if cur.eat_words(&["your", "opponents", "control"])
        && let Some(min_n) = cur.bump().and_then(|t| parse_small_number(t.raw))
    {
        let _ = cur.eat_words(&["or", "more"]);
        let cp = ctx.interner.checkpoint();
        if let Some(mut f) =
            parse_cost_object_phrase(cur.remaining(), ctx, ZoneMask::empty(), false)
            && f.controller.is_none()
        {
            f.controller = Some(RelativeControl::Opponent);
            f.zones = ZoneMask::BATTLEFIELD;
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: Box::new(Amount::Const { n: min_n }),
            });
        }
        ctx.interner.rollback(cp);
    }
    cur.restore(0);
    if cur.eat_words(&["creatures", "you", "control", "have", "total", "toughness"])
        && let Some(min_n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_words(&["or", "greater"])
        && cur.is_empty()
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::TotalStatAmong {
                filter: Box::new(ObjectFilter {
                    types_any: CardTypeMask::CREATURE,
                    controller: Some(RelativeControl::You),
                    zones: ZoneMask::BATTLEFIELD,
                    ..Default::default()
                }),
                stat: StatKind::Toughness,
            }),
            op: CompareOp::Ge,
            right: Box::new(Amount::Const { n: min_n }),
        });
    }
    cur.restore(0);
    if (cur.eat_words(&["there", "are", "no"]) || cur.eat_words(&["you", "have", "no"]))
        && cur.remaining().len() >= 4
    {
        let rem = cur.remaining();
        let in_hand = if rem.len() >= 4
            && rem[rem.len() - 3].eq_ci("in")
            && rem[rem.len() - 2].eq_ci("your")
            && rem[rem.len() - 1].eq_ci("hand")
        {
            Some(&rem[..rem.len() - 3])
        } else if rem.len() >= 3
            && rem[rem.len() - 2].eq_ci("in")
            && rem[rem.len() - 1].eq_ci("hand")
        {
            Some(&rem[..rem.len() - 2])
        } else {
            None
        };
        if let Some(p) = in_hand
            && p.last()
                .is_some_and(|t| t.eq_ci("card") || t.eq_ci("cards"))
        {
            let cp = ctx.interner.checkpoint();
            if let Some(f) = parse_cost_object_phrase(p, ctx, ZoneMask::empty(), true)
                && f.controller.is_none()
            {
                return Some(Condition::NoMatchingCardsInHand {
                    filter: Box::new(f),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    cur.restore(0);
    if (cur.eat_words(&["it's", "bargained"])
        || cur.eat_words(&["this", "spell", "is", "bargained"]))
        && cur.is_empty()
    {
        return Some(Condition::SpellWasBargained);
    }
    cur.restore(0);
    if cur.eat_words(&["a", "creature", "is", "attacking", "you"]) && cur.is_empty() {
        return Some(Condition::AttackingYouCount {
            op: CompareOp::Ge,
            n: 1,
        });
    }
    cur.restore(0);
    if cur.eat_words(&["you've", "drawn"])
        && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_words(&["or", "more", "cards", "this", "turn"])
        && cur.is_empty()
    {
        return Some(Condition::CardsDrawnThisTurnAtLeast { min: n as u16 });
    }
    cur.restore(0);
    if (cur.eat_words(&["you", "control", "a", "full", "party"])
        || cur.eat_words(&["you", "have", "a", "full", "party"]))
        && cur.is_empty()
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::PartyCount {
                player: PlayerRef::You,
            }),
            op: CompareOp::Ge,
            right: Box::new(Amount::Const { n: 4 }),
        });
    }
    cur.restore(0);

    // "it targets (a|an) <phrase>"
    if cur.eat_words(&["it", "targets"]) && (cur.eat_ci("a") || cur.eat_ci("an")) {
        let (rem_t, opp_ctrl) = if cur.remaining().len() >= 4
            && ((cur.remaining()[cur.remaining().len() - 3].eq_ci("an")
                && cur.remaining()[cur.remaining().len() - 2].eq_ci("opponent")
                && cur.remaining()[cur.remaining().len() - 1].eq_ci("controls"))
                || (cur.remaining()[cur.remaining().len() - 3].eq_ci("your")
                    && cur.remaining()[cur.remaining().len() - 2].eq_ci("opponents")
                    && cur.remaining()[cur.remaining().len() - 1].eq_ci("control")))
        {
            (&cur.remaining()[..cur.remaining().len() - 3], true)
        } else {
            (cur.remaining(), false)
        };
        let cp = ctx.interner.checkpoint();
        if !rem_t.is_empty()
            && (rem_t.last().unwrap().eq_ci("spell") || rem_t.last().unwrap().eq_ci("spells"))
        {
            if let Some((sf, other)) = parse_spell_filter_phrase_tokens(rem_t, ctx)
                && !other
            {
                let mut f = sf;
                if opp_ctrl {
                    f.controller = Some(RelativeControl::Opponent);
                }
                return Some(Condition::TargetsStackSpell {
                    filter: Some(Box::new(f)),
                });
            }
        } else if let Some(mut tf) = parse_cost_object_phrase(rem_t, ctx, ZoneMask::empty(), false)
        {
            if opp_ctrl {
                tf.controller = Some(RelativeControl::Opponent);
            }
            let mut mf = ObjectFilter::default();
            mf.extra.push(ObjectPredicate::TargetsFilter {
                filter: Box::new(tf),
            });
            return Some(Condition::MatchesFilter {
                target: None,
                filter: Box::new(mf),
                any: false,
            });
        }
        ctx.interner.rollback(cp);
    }
    cur.restore(0);

    // "there is (a|an) <p> card in your graveyard" / "there are <n> or more [<p>] cards [total] in all graveyards" / "there are <n> or more <p> on the battlefield"
    if cur.eat_words(&["there", "is"]) && (cur.eat_ci("a") || cur.eat_ci("an")) {
        let rem = cur.remaining();
        if rem.len() >= 5
            && rem[rem.len() - 4].eq_ci("card")
            && rem[rem.len() - 3].eq_ci("in")
            && rem[rem.len() - 2].eq_ci("your")
            && rem[rem.len() - 1].eq_ci("graveyard")
        {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) =
                parse_cost_object_phrase(&rem[..rem.len() - 3], ctx, ZoneMask::empty(), true)
                && f.controller.is_none()
            {
                f.zones = ZoneMask::GRAVEYARD;
                f.owner = Some(RelativeControl::You);
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: Box::new(Amount::Const { n: 1 }),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    cur.restore(0);
    if cur.eat_words(&["there", "are"])
        && let Some(mn) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_words(&["or", "more"])
    {
        let rem = cur.remaining();
        if rem.len() >= 4
            && rem[rem.len() - 3].eq_ci("in")
            && rem[rem.len() - 2].eq_ci("your")
            && rem[rem.len() - 1].eq_ci("graveyard")
            && (rem[rem.len() - 4].eq_ci("card") || rem[rem.len() - 4].eq_ci("cards"))
            && rem.len() >= 5
        {
            let cp = ctx.interner.checkpoint();
            if let Some(mut f) =
                parse_cost_object_phrase(&rem[..rem.len() - 3], ctx, ZoneMask::empty(), true)
                && f.controller.is_none()
            {
                f.zones = ZoneMask::GRAVEYARD;
                f.owner = Some(RelativeControl::You);
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: Box::new(Amount::Const { n: mn }),
                });
            }
            ctx.interner.rollback(cp);
        }
        if rem.len() >= 4
            && rem[rem.len() - 3].eq_ci("in")
            && rem[rem.len() - 2].eq_ci("all")
            && rem[rem.len() - 1].eq_ci("graveyards")
        {
            let without_zone = &rem[..rem.len() - 3];
            let without_total = if without_zone.last().is_some_and(|t| t.eq_ci("total")) {
                &without_zone[..without_zone.len() - 1]
            } else {
                without_zone
            };
            if without_total
                .last()
                .is_some_and(|t| t.eq_ci("card") || t.eq_ci("cards"))
            {
                let cp = ctx.interner.checkpoint();
                let f_opt = if without_total.len() == 1 {
                    Some(ObjectFilter::default())
                } else {
                    parse_cost_object_phrase(without_total, ctx, ZoneMask::empty(), true)
                };
                if let Some(mut f) = f_opt
                    && f.controller.is_none()
                {
                    f.zones = ZoneMask::GRAVEYARD;
                    return Some(Condition::Compare {
                        left: Box::new(Amount::Count { of: Box::new(f) }),
                        op: CompareOp::Ge,
                        right: Box::new(Amount::Const { n: mn }),
                    });
                }
                ctx.interner.rollback(cp);
            }
        }
        if rem.len() >= 4
            && rem[rem.len() - 3].eq_ci("on")
            && rem[rem.len() - 2].eq_ci("the")
            && rem[rem.len() - 1].eq_ci("battlefield")
        {
            let body = &rem[..rem.len() - 3];
            let cp = ctx.interner.checkpoint();
            let f_opt =
                if body.len() == 1 && (body[0].eq_ci("permanent") || body[0].eq_ci("permanents")) {
                    Some(ObjectFilter::default())
                } else {
                    parse_cost_object_phrase(body, ctx, ZoneMask::empty(), false)
                };
            if let Some(mut f) = f_opt
                && f.controller.is_none()
            {
                f.zones = ZoneMask::BATTLEFIELD;
                return Some(Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: CompareOp::Ge,
                    right: Box::new(Amount::Const { n: mn }),
                });
            }
            ctx.interner.rollback(cp);
        }
    }
    cur.restore(0);

    // Fallback to general `value::parse_condition`!
    let cp = ctx.interner.checkpoint();
    let mut sub_ctx = ctx.with_normalized_text(raw);
    if let Some((cond, used)) =
        crate::parser::value::parse_condition(tokens, &mut sub_ctx, &mut NullScope)
        && used == tokens.len()
    {
        return Some(cond);
    }
    ctx.interner.rollback(cp);

    None
}

fn strip_ability_word_prefix(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.first() == Some(&b'{') {
        return s;
    }
    if let Some(dash_idx) = s.find('—') {
        let head = s[..dash_idx].trim();
        if !head.is_empty()
            && head.len() <= 30
            && head
                .chars()
                .all(|c| c.is_ascii_alphabetic() || matches!(c, ' ' | '\'' | '-'))
        {
            return s[dash_idx + '—'.len_utf8()..].trim();
        }
    }
    s
}

fn combine_opt_cond(a: Option<Condition>, b: Option<Condition>) -> Option<Condition> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(Condition::And { of: vec![x, y] }),
    }
}

pub fn parse_cost_modifier_sentence(sentence: &str, ctx: &mut ParseCtx<'_>) -> Option<StaticDef> {
    let b = sentence.as_bytes();
    if !(contains_ascii_ci(b, b"cost")
        || contains_ascii_ci(b, b"spend")
        || contains_ascii_ci(b, b"rather")
        || contains_ascii_ci(b, b"deck")
        || contains_ascii_ci(b, b"equip"))
    {
        return None;
    }
    let cp = ctx.interner.checkpoint();
    let res = parse_cost_modifier_sentence_inner(sentence, ctx);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn parse_cost_modifier_sentence_inner(sentence: &str, ctx: &mut ParseCtx<'_>) -> Option<StaticDef> {
    let raw_trimmed = sentence
        .trim()
        .trim_end_matches('.')
        .trim()
        .replace('’', "'");
    if raw_trimmed.is_empty() {
        return None;
    }
    let mut text = strip_ability_word_prefix(&raw_trimmed).to_string();
    if let Some((prefix, rest)) = text.split_at_checked("it also costs".len())
        && prefix.eq_ignore_ascii_case("it also costs")
    {
        text = format!("~ costs{rest}");
    }

    let mut turn_prefix_cond: Option<Condition> = None;
    let turn_prefix = "during turns other than yours,";
    if text.len() > turn_prefix.len()
        && let Some((prefix, rest)) = text.split_at_checked(turn_prefix.len())
        && prefix.eq_ignore_ascii_case(turn_prefix)
    {
        turn_prefix_cond = Some(Condition::Not {
            of: Box::new(Condition::IsTurnOf {
                player: PlayerRef::You,
            }),
        });
        text = rest.trim().to_string();
    }

    let mut colored_only = false;
    let mut min_total_mana: Option<u16> = None;
    let mut max_reduction: Option<u16> = None;

    for _ in 0..4 {
        let low = text.to_ascii_lowercase();
        if let Some(dot) = low.rfind('.') {
            let head = text[..dot].trim();
            let tail = low[dot + 1..].trim();
            if tail == "this effect reduces only the amount of colored mana you pay" {
                colored_only = true;
                text = head.to_string();
                continue;
            }
            if tail == "this cost can't be reduced to less than {1}"
                || (tail.starts_with("this effect can't reduce the ")
                    && tail.ends_with("to less than one mana"))
            {
                min_total_mana = Some(1);
                text = head.to_string();
                continue;
            }
            if let Some(by_pos) = tail.rfind(" by more than {")
                && tail.starts_with("this effect can't reduce the ")
                && tail.ends_with('}')
            {
                let num_s = &tail[by_pos + " by more than {".len()..tail.len() - 1];
                if let Ok(cap) = num_s.parse::<u16>() {
                    max_reduction = Some(cap);
                    text = head.to_string();
                    continue;
                }
            }
        }
        break;
    }

    let with_floor_and_colored = |mut delta: CostDelta| -> Option<CostDelta> {
        if (colored_only || min_total_mana.is_some() || max_reduction.is_some())
            && delta.mode != CostDeltaMode::Reduce
        {
            return None;
        }
        delta.colored_only = colored_only;
        delta.min_total_mana = min_total_mana;
        delta.max_reduction = max_reduction;
        Some(delta)
    };

    let mut sub_ctx = ctx.with_normalized_text(&text);
    let tokens = tokenize(&text, 0);
    let mut cur = TokenCursor::new(&tokens);

    // 1. "If you control (a|your) commander, you may cast (this spell|~) without paying its mana cost"
    if cur.eat_words(&["if", "you", "control"])
        && (cur.eat_ci("a") || cur.eat_ci("your"))
        && cur.eat_words(&["commander", ",", "you", "may", "cast"])
        && (cur.eat_words(&["this", "spell"])
            || (cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.bump().is_some()))
        && cur.eat_words(&["without", "paying", "its", "mana", "cost"])
        && cur.is_empty()
    {
        if colored_only || min_total_mana.is_some() || max_reduction.is_some() {
            return None;
        }
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta: CostDelta {
                    mode: CostDeltaMode::SetZero,
                    generic: 0,
                    per: None,
                    additional_costs: Vec::new(),
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: None,
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: combine_opt_cond(turn_prefix_cond, Some(Condition::ControlCommander))
                .map(Box::new),
        });
    }
    cur.restore(0);

    // 2. Stack-active spell target cost modifier ("As long as ~ is on the stack, spells [you|your opponents cast ]that target (it|~) cost {N} less/more to cast")
    if cur.eat_words(&["as", "long", "as"])
        && cur
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        && cur.bump().is_some()
        && cur.eat_words(&["is", "on", "the", "stack", ",", "spells"])
    {
        let who = if cur.eat_words(&["you", "cast"]) {
            PlayerRelation::You
        } else if cur.eat_words(&["your", "opponents", "cast"]) {
            PlayerRelation::EachOpponent
        } else {
            PlayerRelation::EachPlayer
        };
        if cur.eat_words(&["that", "target"])
            && (cur.eat_ci("it")
                || (cur
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                    && cur.bump().is_some()))
            && (cur.eat_ci("cost") || cur.eat_ci("costs"))
            && let Some((raw_delta, mode_rest)) = take_delta_and_mode(cur.remaining())
            && let Some(delta) = with_floor_and_colored(raw_delta)
            && mode_rest.len() == 2
            && mode_rest[0].eq_ci("to")
            && mode_rest[1].eq_ci("cast")
        {
            let mut sf = ObjectFilter::default();
            sf.extra.push(ObjectPredicate::TargetsSelf);
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(sf),
                        alt_cost_keyword: None,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::STACK,
                },
                condition: turn_prefix_cond.map(Box::new),
            });
        }
    }
    cur.restore(0);

    // 3. Multi-sentence or non-modifier block (`!colored_only && min_total_mana.is_none() && max_reduction.is_none()`)
    if !colored_only && min_total_mana.is_none() && max_reduction.is_none() {
        // 3a. Defiler cycle (3 sentences)
        if let Some(d_static) = try_parse_defiler_static(&tokens, &mut sub_ctx, &turn_prefix_cond) {
            return Some(d_static);
        }

        // 3b. Combined pitch + reduction: "As an additional cost to cast (~|this spell), ... . (~|this spell) costs {N} less to cast for each <metric>"
        if let Some(dot_tok_idx) = tokens.iter().position(|t| t.is_punct(Punct::Period))
            && dot_tok_idx > 0
            && dot_tok_idx + 1 < tokens.len()
        {
            let dot_byte = tokens[dot_tok_idx].span.start as usize;
            let sent1 = text[..dot_byte].trim();
            let sent2_toks = &tokens[dot_tok_idx + 1..];
            let cp_3b = sub_ctx.interner.checkpoint();
            if let Some(add_cp) = parse_additional_cost_sentence(sent1, 0, false, &mut sub_ctx)
                && add_cp.unparsed.is_empty()
                && !add_cp.defs.is_empty()
            {
                let mut s2c = TokenCursor::new(sent2_toks);
                let is_self = s2c.eat_words(&["this", "spell"])
                    || (s2c
                        .peek()
                        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                        && s2c.bump().is_some());
                if is_self
                    && s2c.eat_ci("costs")
                    && let Some((mut delta, rest)) = take_delta_and_mode(s2c.remaining())
                    && delta.mode == CostDeltaMode::Reduce
                {
                    let mut rc = TokenCursor::new(rest);
                    if rc.eat_words(&["to", "cast", "for", "each"])
                        && let Some(per) =
                            parse_cost_metric_amount_tokens(rc.remaining(), &mut sub_ctx)
                    {
                        delta.per = Some(Box::new(per));
                        delta.additional_costs = add_cp.defs;
                        return Some(StaticDef {
                            body: StaticBody::CostModifier {
                                affects: SpellOrAbilityFilter::SelfObj,
                                delta,
                                who: PlayerFilter {
                                    relation: PlayerRelation::You,
                                },
                                who_enchanted_player: false,
                                active_zones: ZoneMask::empty(),
                            },
                            condition: turn_prefix_cond.map(Box::new),
                        });
                    }
                }
            }
            sub_ctx.interner.rollback(cp_3b);
        }

        // 3c. Single-sentence non-legacy AdditionalCastCost ("As an additional cost to cast [~|this spell], ...")
        if !tokens.iter().any(|t| t.is_punct(Punct::Period)) {
            let cp_3c = sub_ctx.interner.checkpoint();
            if let Some(cp) = parse_additional_cost_sentence(&text, 0, false, &mut sub_ctx)
                && cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().any(|d| !is_legacy_cast_payable_cost(d))
            {
                return Some(StaticDef {
                    body: StaticBody::AdditionalCastCost { costs: cp.defs },
                    condition: turn_prefix_cond.map(Box::new),
                });
            }
            sub_ctx.interner.rollback(cp_3c);
        }

        // 3d. ManaSpendRestriction
        if let Some(msr) = try_parse_mana_spend_static(&tokens, &turn_prefix_cond) {
            return Some(msr);
        }

        // 3e. FirstActivationZeroCost:
        // "[During your turn, ]you may pay {0} rather than pay the (equip|power-up|cycling) cost of the first (equip ability you activate | power-up ability you activate | card you cycle) (each turn | during each of your turns)"
        {
            let saved = cur.save();
            let prefix_your_turn = cur.eat_words(&["during", "your", "turn", ","]);
            if !colored_only
                && min_total_mana.is_none()
                && max_reduction.is_none()
                && cur.eat_words(&["you", "may", "pay"])
                && cur.peek().is_some_and(|t| t.raw == "{0}")
                && cur.bump().is_some()
                && cur.eat_words(&["rather", "than", "pay", "the"])
            {
                let kw = if cur.eat_ci("equip") {
                    Some(FirstActivationZeroKind::Equip)
                } else if cur.eat_ci("cycling") {
                    Some(FirstActivationZeroKind::Cycling)
                } else if cur.eat_hyphenated("power", "up") {
                    Some(FirstActivationZeroKind::PowerUp)
                } else {
                    None
                };
                if let Some(ability_keyword) = kw
                    && cur.eat_words(&["cost", "of", "the", "first"])
                {
                    let matched_target = match ability_keyword {
                        FirstActivationZeroKind::Equip => {
                            cur.eat_words(&["equip", "ability", "you", "activate"])
                        }
                        FirstActivationZeroKind::PowerUp => {
                            cur.eat_hyphenated("power", "up")
                                && cur.eat_words(&["ability", "you", "activate"])
                        }
                        FirstActivationZeroKind::Cycling => {
                            cur.eat_words(&["card", "you", "cycle"])
                        }
                    };
                    if matched_target {
                        let suffix_your_turn =
                            if cur.eat_words(&["during", "each", "of", "your", "turns"]) {
                                Some(true)
                            } else if cur.eat_words(&["each", "turn"]) {
                                Some(false)
                            } else {
                                None
                            };
                        if let Some(suffix_yt) = suffix_your_turn
                            && cur.is_empty()
                        {
                            return Some(StaticDef {
                                body: StaticBody::FirstActivationZeroCost {
                                    ability_keyword,
                                    during_your_turn_only: prefix_your_turn || suffix_yt,
                                },
                                condition: turn_prefix_cond.map(Box::new),
                            });
                        }
                    }
                }
            }
            cur.restore(saved);
        }

        // 3f. CommanderTaxAsLife (Liesa)
        if cur.eat_words(&[
            "rather", "than", "pay", "{2}", "for", "each", "previous", "time", "you've", "cast",
        ]) && (cur.eat_words(&["this", "spell"])
            || (cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.bump().is_some()))
            && cur.eat_words(&["from", "the", "command", "zone", "this", "game", ",", "pay"])
            && let Some(life_per_cast) = cur.bump().and_then(|t| parse_small_number(t.raw))
            && cur.eat_words(&["life", "that", "many", "times"])
            && cur.is_empty()
            && life_per_cast >= 1
        {
            return Some(StaticDef {
                body: StaticBody::CommanderTaxAsLife {
                    life_per_cast: life_per_cast as u16,
                },
                condition: turn_prefix_cond.map(Box::new),
            });
        }
        cur.restore(0);

        // 3g. DeckCopyLimit ("A deck can have (any number of|up to N) cards named ~")
        if cur.eat_words(&["a", "deck", "can", "have"]) {
            if cur.eat_words(&["any", "number", "of", "cards", "named"])
                && cur
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.remaining().len() == 1
            {
                return Some(StaticDef {
                    body: StaticBody::DeckCopyLimit { limit: None },
                    condition: turn_prefix_cond.map(Box::new),
                });
            }
            if cur.eat_words(&["up", "to"])
                && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
                && cur.eat_words(&["cards", "named"])
                && cur
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.remaining().len() == 1
                && n >= 1
            {
                return Some(StaticDef {
                    body: StaticBody::DeckCopyLimit {
                        limit: Some(n as u16),
                    },
                    condition: turn_prefix_cond.map(Box::new),
                });
            }
            return None;
        }
    }

    // 4. Shadow of Mortality ("If your life total is less than your starting life total, (this spell|~) costs {X} less to cast, where X is the difference")
    if cur.eat_words(&[
        "if", "your", "life", "total", "is", "less", "than", "your", "starting", "life", "total",
        ",",
    ]) && (cur.eat_words(&["this", "spell"])
        || (cur
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && cur.bump().is_some()))
        && cur.eat_words(&[
            "costs",
            "{X}",
            "less",
            "to",
            "cast",
            ",",
            "where",
            "x",
            "is",
            "the",
            "difference",
        ])
        && cur.is_empty()
    {
        let delta = with_floor_and_colored(CostDelta {
            mode: CostDeltaMode::Reduce,
            generic: 1,
            per: Some(Box::new(Amount::StartingLifeDifference)),
            additional_costs: Vec::new(),
            colored: SmallVec::new(),
            hybrid_colored: SmallVec::new(),
            colored_only: false,
            min_total_mana: None,
            max_reduction: None,
            per_target_life: None,
        })?;
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: turn_prefix_cond.map(Box::new),
        });
    }
    cur.restore(0);

    // 5. Self "{X} (less|more) to cast, where X is ..."
    {
        let saved = cur.save();
        let is_during = cur.eat_words(&["during", "your", "turn", ","]);
        if (cur.eat_words(&["this", "spell"])
            || (cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.bump().is_some()))
            && cur.eat_ci("costs")
            && cur.peek().is_some_and(|t| {
                matches!(t.kind, TokenKind::ManaSymbol(ManaSymbolKind::Variable('X')))
            })
            && cur.bump().is_some()
        {
            let mode = if cur.eat_ci("less") {
                Some(CostDeltaMode::Reduce)
            } else if cur.eat_ci("more") {
                Some(CostDeltaMode::Increase)
            } else {
                None
            };
            if let Some(m) = mode
                && cur.eat_words(&["to", "cast", ",", "where", "x", "is"])
                && let Some(per) = parse_where_x_body_tokens(cur.remaining(), &mut sub_ctx)
            {
                let delta = with_floor_and_colored(CostDelta {
                    mode: m,
                    generic: 1,
                    per: Some(Box::new(per)),
                    additional_costs: Vec::new(),
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: None,
                })?;
                let c = combine_opt_cond(
                    turn_prefix_cond,
                    is_during.then_some(Condition::IsTurnOf {
                        player: PlayerRef::You,
                    }),
                );
                return Some(StaticDef {
                    body: StaticBody::CostModifier {
                        affects: SpellOrAbilityFilter::SelfObj,
                        delta,
                        who: PlayerFilter {
                            relation: PlayerRelation::You,
                        },
                        who_enchanted_player: false,
                        active_zones: ZoneMask::empty(),
                    },
                    condition: c.map(Box::new),
                });
            }
        }
        cur.restore(saved);
    }

    // 6. Battlefield spell "{X} (less|more) to cast, where X is ..."
    if let Some(res) = try_parse_spell_where_x(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 7. Outside-hand and from-zone spell modifiers
    if let Some(res) = try_parse_zone_spell_mod(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 8. Prefix "If <cond>, (this spell|~) costs <delta> (less|more) to cast[ for each <metric>]"
    if cur.eat_ci("if")
        && let Some(comma_idx) = cur
            .remaining()
            .iter()
            .position(|t| t.is_punct(Punct::Comma))
    {
        let cond_toks = &cur.remaining()[..comma_idx];
        let mut after = TokenCursor::new(&cur.remaining()[comma_idx + 1..]);
        if (after.eat_words(&["this", "spell"])
            || (after
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && after.bump().is_some()))
            && after.eat_ci("costs")
            && let Some((raw_delta, rest)) = take_delta_and_mode(after.remaining())
            && let Some(mut delta) = with_floor_and_colored(raw_delta)
        {
            let cond_str = tokens_slice_text(&text, cond_toks);
            let cp_8 = sub_ctx.interner.checkpoint();
            if let Some(cond) = parse_self_cost_condition_tokens(cond_toks, cond_str, &mut sub_ctx)
            {
                let mut rc = TokenCursor::new(rest);
                if rc.eat_words(&["to", "cast"]) {
                    let ok = if rc.is_empty() {
                        true
                    } else if rc.eat_words(&["for", "each"])
                        && let Some(per) =
                            parse_cost_metric_amount_tokens(rc.remaining(), &mut sub_ctx)
                    {
                        delta.per = Some(Box::new(per));
                        true
                    } else {
                        false
                    };
                    if ok {
                        return Some(StaticDef {
                            body: StaticBody::CostModifier {
                                affects: SpellOrAbilityFilter::SelfObj,
                                delta,
                                who: PlayerFilter {
                                    relation: PlayerRelation::You,
                                },
                                who_enchanted_player: false,
                                active_zones: ZoneMask::empty(),
                            },
                            condition: combine_opt_cond(turn_prefix_cond, Some(cond)).map(Box::new),
                        });
                    }
                }
            }
            sub_ctx.interner.rollback(cp_8);
        }
    }
    cur.restore(0);

    // 9. "The (first|second) [<phrase> ]spell [...] you cast (each turn|during each [of your] turn[s]) costs <delta> (less|more) to cast"
    if let Some(res) = try_parse_nth_spell_mod(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 10. Self per-target life cost ("(this spell|~) costs N life more to cast for each target")
    if !colored_only
        && min_total_mana.is_none()
        && max_reduction.is_none()
        && (cur.eat_words(&["this", "spell"])
            || (cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && cur.bump().is_some()))
        && cur.eat_ci("costs")
        && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_words(&["life", "more", "to", "cast", "for", "each", "target"])
        && cur.is_empty()
        && n >= 1
    {
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta: CostDelta {
                    mode: CostDeltaMode::Increase,
                    generic: 0,
                    per: None,
                    additional_costs: Vec::new(),
                    colored: SmallVec::new(),
                    hybrid_colored: SmallVec::new(),
                    colored_only: false,
                    min_total_mana: None,
                    max_reduction: None,
                    per_target_life: Some(n as u16),
                },
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: turn_prefix_cond.map(Box::new),
        });
    }
    cur.restore(0);

    // 11. Self cost modifier ("[during your turn, ](this spell|~) costs <delta> (less|more) to cast [for each ... | if ... | as long as ...]")
    if let Some(res) = try_parse_self_cost_mod(
        &tokens,
        &text,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 12. Targeting life tax ("Spells your opponents cast that target (~|(a|an) <f>) cost an additional N life to cast")
    if !colored_only
        && min_total_mana.is_none()
        && max_reduction.is_none()
        && cur.eat_words(&["spells", "your", "opponents", "cast", "that", "target"])
    {
        let rem = cur.remaining();
        if let Some(cost_pos) = rem.iter().position(|t| t.eq_ci("cost")) {
            let tgt_toks = &rem[..cost_pos];
            let mut tc = TokenCursor::new(&rem[cost_pos + 1..]);
            if tc.eat_words(&["an", "additional"])
                && let Some(life) = tc.bump().and_then(|t| parse_small_number(t.raw))
                && tc.eat_words(&["life", "to", "cast"])
                && tc.is_empty()
                && let Some(tmatch) =
                    parse_spell_target_clause_filter_tokens(tgt_toks, &mut sub_ctx)
            {
                let mut f = ObjectFilter::default();
                match tmatch {
                    TargetClauseMatch::SelfObj => f.extra.push(ObjectPredicate::TargetsSelf),
                    TargetClauseMatch::Attached => f.extra.push(ObjectPredicate::TargetsAttached),
                    TargetClauseMatch::YouOrFilter(yf) => {
                        f.extra.push(ObjectPredicate::TargetsYouOrFilter {
                            filter: Box::new(yf),
                        });
                    }
                    TargetClauseMatch::Filter(tf) => {
                        f.extra.push(ObjectPredicate::TargetsFilter {
                            filter: Box::new(tf),
                        });
                    }
                }
                return Some(StaticDef {
                    body: StaticBody::CostModifier {
                        affects: SpellOrAbilityFilter::Spell {
                            filter: Box::new(f),
                            alt_cost_keyword: None,
                        },
                        delta: CostDelta {
                            mode: CostDeltaMode::Increase,
                            generic: 0,
                            per: None,
                            additional_costs: vec![CostDef::PayLife {
                                amount: Box::new(Amount::Const { n: life }),
                                half_life_rounded_up: false,
                            }],
                            colored: SmallVec::new(),
                            hybrid_colored: SmallVec::new(),
                            colored_only: false,
                            min_total_mana: None,
                            max_reduction: None,
                            per_target_life: None,
                        },
                        who: PlayerFilter {
                            relation: PlayerRelation::EachOpponent,
                        },
                        who_enchanted_player: false,
                        active_zones: ZoneMask::empty(),
                    },
                    condition: turn_prefix_cond.map(Box::new),
                });
            }
        }
    }
    cur.restore(0);

    // 13. Targeting mana cost modifier
    if let Some(res) = try_parse_targeting_spell_mod(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 14. Alt-cost keyword modifiers ("(Flashback|Dash) costs [you|your opponents pay] cost <delta> (less|more)")
    if (cur.eat_ci("flashback") || cur.eat_ci("dash")) && cur.eat_ci("costs") {
        let kw_name = tokens[0].raw.to_ascii_lowercase();
        let who = if cur.eat_words(&["you", "pay"]) {
            PlayerRelation::You
        } else if cur.eat_words(&["your", "opponents", "pay"]) {
            PlayerRelation::EachOpponent
        } else {
            PlayerRelation::EachPlayer
        };
        if cur.eat_ci("cost")
            && let Some((raw_delta, rest)) = take_delta_and_mode(cur.remaining())
            && rest.is_empty()
            && let Some(delta) = with_floor_and_colored(raw_delta)
        {
            let kw_id = sub_ctx.interner.intern_keyword(&kw_name);
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(ObjectFilter::default()),
                        alt_cost_keyword: Some(kw_id),
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.map(Box::new),
            });
        }
    }
    cur.restore(0);

    // 15. "Each [<p>] spell [you|your opponents cast] costs <delta> (less|more) to cast ..." & "Each spell (you|your opponents) cast that's ..."
    if let Some(res) = try_parse_each_spell_mod(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 16. Enchanted player / chosen name / shares card type with exiled
    if let Some(res) = try_parse_chosen_and_exiled_spell_mod(
        &tokens,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    // 17. Activated ability cost modifiers (all 11 patterns)
    if let Some(res) = try_parse_activated_ability_cost_mod(
        &tokens,
        &text,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
        colored_only || min_total_mana.is_some() || max_reduction.is_some(),
    ) {
        return Some(res);
    }

    // 18. General spell cost modifiers (with optional "During your turn," / "As long as ..." / "If ...")
    if let Some(res) = try_parse_general_spell_mod(
        &tokens,
        &text,
        &mut sub_ctx,
        &turn_prefix_cond,
        &with_floor_and_colored,
    ) {
        return Some(res);
    }

    let _ = (
        ManaSpendScope::SelfSpell,
        Affected::SelfObj,
        OUTLAW_SUBTYPES,
        singular_subtype("Elves"),
    );
    None
}

fn tokens_slice_text<'a>(full: &'a str, slice: &[Token<'_>]) -> &'a str {
    if let (Some(f), Some(l)) = (slice.first(), slice.last()) {
        full.get(f.span.start as usize..l.span.end as usize)
            .unwrap_or("")
    } else {
        ""
    }
}

fn take_delta_and_mode<'a>(tokens: &'a [Token<'a>]) -> Option<(CostDelta, &'a [Token<'a>])> {
    let mut mana_end = 0usize;
    for tok in tokens {
        if matches!(tok.kind, TokenKind::ManaSymbol(_)) && !is_disallowed_mana_symbol(tok) {
            mana_end += 1;
        } else {
            break;
        }
    }
    if mana_end == 0 || mana_end >= tokens.len() {
        return None;
    }
    let mode = if tokens[mana_end].eq_ci("less") {
        CostDeltaMode::Reduce
    } else if tokens[mana_end].eq_ci("more") {
        CostDeltaMode::Increase
    } else {
        return None;
    };
    let delta = parse_cost_delta(&tokens[..mana_end], mode)?;
    Some((delta, &tokens[mana_end + 1..]))
}

fn try_parse_defiler_static(
    tokens: &[Token<'_>],
    _ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
) -> Option<StaticDef> {
    // "As an additional cost to cast (white|blue|black|red|green) permanent spells, you may pay N life.
    //  Those spells cost {C} less to cast if you paid life this way.
    //  This effect reduces only the amount of <color> mana you pay"
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_words(&["as", "an", "additional", "cost", "to", "cast"]) {
        return None;
    }
    let col_word_tok = cur.bump()?;
    let col_mask = parse_color_word(col_word_tok.raw)?;
    let color = color_mask_to_mana_color(col_mask)?;
    if !cur.eat_words(&["permanent", "spells", ",", "you", "may", "pay"]) {
        return None;
    }
    let life = cur.bump().and_then(|t| parse_small_number(t.raw))?;
    if life < 1 || !cur.eat_words(&["life", ".", "those", "spells", "cost"]) {
        return None;
    }
    let mana_tok = cur.bump()?;
    let TokenKind::ManaSymbol(ManaSymbolKind::Colored(sym_col)) = mana_tok.kind else {
        return None;
    };
    if sym_col != color {
        return None;
    }
    if !cur.eat_words(&[
        "less", "to", "cast", "if", "you", "paid", "life", "this", "way", ".", "this", "effect",
        "reduces", "only", "the", "amount", "of",
    ]) {
        return None;
    }
    let col_word2 = cur.bump()?;
    if !col_word2.eq_ci(col_word_tok.raw)
        || !cur.eat_words(&["mana", "you", "pay"])
        || !cur.is_empty()
    {
        return None;
    }
    Some(StaticDef {
        body: StaticBody::DefilerCostReduction {
            color,
            life_amount: life as u16,
            filter: Box::new(ObjectFilter {
                colors_any: col_mask,
                types_any: CardTypeMask::ARTIFACT
                    | CardTypeMask::BATTLE
                    | CardTypeMask::CREATURE
                    | CardTypeMask::ENCHANTMENT
                    | CardTypeMask::PLANESWALKER,
                ..Default::default()
            }),
        },
        condition: turn_prefix_cond.clone().map(Box::new),
    })
}

fn color_mask_to_mana_color(m: ColorMask) -> Option<ManaColor> {
    if m == ColorMask::W {
        Some(ManaColor::W)
    } else if m == ColorMask::U {
        Some(ManaColor::U)
    } else if m == ColorMask::B {
        Some(ManaColor::B)
    } else if m == ColorMask::R {
        Some(ManaColor::R)
    } else if m == ColorMask::G {
        Some(ManaColor::G)
    } else {
        None
    }
}

fn try_parse_mana_spend_static(
    tokens: &[Token<'_>],
    turn_prefix_cond: &Option<Condition>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_words(&["spend", "only", "colored", "mana", "on", "x"]) {
        let mut x_max_one_per_color = false;
        if cur.is_empty()
            || (cur.eat_words(&[
                ".", "no", "more", "than", "one", "mana", "of", "each", "color", "may", "be",
                "spent", "this", "way",
            ]) && cur.is_empty()
                && {
                    x_max_one_per_color = true;
                    true
                })
        {
            return Some(StaticDef {
                body: StaticBody::ManaSpendRestriction {
                    scope: ManaSpendScope::SelfBoth,
                    x_colors: ColorMask::empty(),
                    x_colored_only: true,
                    x_max_one_per_color,
                    source_kind: None,
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            });
        }
    }
    cur.restore(0);
    if cur.eat_words(&["spend", "only"]) {
        let rem = cur.remaining();
        if rem.len() >= 4
            && rem[rem.len() - 3].eq_ci("mana")
            && rem[rem.len() - 2].eq_ci("on")
            && rem[rem.len() - 1].eq_ci("x")
        {
            let mut mask = ColorMask::empty();
            let mut any = false;
            for tok in &rem[..rem.len() - 3] {
                if tok.is_punct(Punct::Comma) || tok.eq_ci("or") || tok.eq_ci("and") {
                    continue;
                }
                if tok.eq_ci("and/or") {
                    continue;
                }
                let c = parse_color_word(tok.raw)?;
                mask |= c;
                any = true;
            }
            if any {
                return Some(StaticDef {
                    body: StaticBody::ManaSpendRestriction {
                        scope: ManaSpendScope::SelfBoth,
                        x_colors: mask,
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        source_kind: None,
                    },
                    condition: turn_prefix_cond.clone().map(Box::new),
                });
            }
        }
    }
    cur.restore(0);
    if cur.eat_words(&["spend", "only", "mana", "produced", "by"]) {
        let kind = if cur.eat_ci("creatures") {
            Some(ManaSourceRestrictionKind::Creature)
        } else if cur.eat_words(&["basic", "lands"]) {
            Some(ManaSourceRestrictionKind::BasicLand)
        } else if cur.eat_words(&["snow", "sources"]) {
            Some(ManaSourceRestrictionKind::Snow)
        } else if cur.eat_ci("treasures") {
            Some(ManaSourceRestrictionKind::Treasure)
        } else if cur.eat_words(&["sources", "of", "the", "chosen", "color"]) {
            Some(ManaSourceRestrictionKind::ChosenColor)
        } else {
            None
        };
        if let Some(sk) = kind
            && cur.eat_ci("to")
            && cur.eat_ci("cast")
            && (cur.eat_words(&["this", "spell"])
                || (cur
                    .peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                    && cur.bump().is_some()))
        {
            let _ = cur.eat_words(&["this", "way"]);
            if cur.is_empty() {
                return Some(StaticDef {
                    body: StaticBody::ManaSpendRestriction {
                        scope: ManaSpendScope::SelfSpell,
                        x_colors: ColorMask::empty(),
                        x_colored_only: false,
                        x_max_one_per_color: false,
                        source_kind: Some(sk),
                    },
                    condition: turn_prefix_cond.clone().map(Box::new),
                });
            }
        }
    }
    None
}

fn try_parse_spell_where_x(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_spell_where_x_inner(tokens, ctx, turn_prefix_cond, with_floor_and_colored);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_spell_where_x_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    let is_during = cur.eat_words(&["during", "your", "turn", ","]);
    let rem = cur.remaining();
    // Look for "spells ... (you|your opponents) cast ... cost {X} (less|more) to cast , where X is ..."
    let wx_pos = rem.windows(4).position(|w| {
        w[0].is_punct(Punct::Comma) && w[1].eq_ci("where") && w[2].eq_ci("x") && w[3].eq_ci("is")
    })?;
    let head = &rem[..wx_pos];
    if head.len() < 7 {
        return None;
    }
    if !head[head.len() - 2].eq_ci("to") || !head[head.len() - 1].eq_ci("cast") {
        return None;
    }
    let mode = if head[head.len() - 3].eq_ci("less") {
        CostDeltaMode::Reduce
    } else if head[head.len() - 3].eq_ci("more") {
        CostDeltaMode::Increase
    } else {
        return None;
    };
    if !matches!(
        head[head.len() - 4].kind,
        TokenKind::ManaSymbol(ManaSymbolKind::Variable('X'))
    ) || !head[head.len() - 5].eq_ci("cost")
    {
        return None;
    }
    let prefix = &head[..head.len() - 5];
    // Find "spells"
    let sp_pos = prefix
        .iter()
        .position(|t| t.eq_ci("spell") || t.eq_ci("spells"))?;
    // Find "(you|your opponents) cast"
    let (who, who_start, who_end) = if let Some(p) = prefix
        .windows(2)
        .position(|w| w[0].eq_ci("you") && w[1].eq_ci("cast"))
    {
        (PlayerRelation::You, p, p + 2)
    } else {
        let p = prefix
            .windows(3)
            .position(|w| w[0].eq_ci("your") && w[1].eq_ci("opponents") && w[2].eq_ci("cast"))?;
        (PlayerRelation::EachOpponent, p, p + 3)
    };
    if sp_pos >= who_start {
        return None;
    }
    let pre_spell = &prefix[..sp_pos];
    let mid_qual = &prefix[sp_pos + 1..who_start];
    let post_qual = &prefix[who_end..];
    if !mid_qual.is_empty() && !post_qual.is_empty() {
        return None;
    }
    let qual = if !mid_qual.is_empty() {
        mid_qual
    } else {
        post_qual
    };
    let mut combined_phrase = pre_spell.to_vec();
    combined_phrase.extend_from_slice(qual);
    let (mut f, other) = parse_spell_filter_phrase_tokens(&combined_phrase, ctx)?;
    if other {
        f.flags |= FilterFlags::OTHER;
    }
    let per = parse_where_x_body_tokens(&rem[wx_pos + 4..], ctx)?;
    let delta = with_floor_and_colored(CostDelta {
        mode,
        generic: 1,
        per: Some(Box::new(per)),
        additional_costs: Vec::new(),
        colored: SmallVec::new(),
        hybrid_colored: SmallVec::new(),
        colored_only: false,
        min_total_mana: None,
        max_reduction: None,
        per_target_life: None,
    })?;
    let c = combine_opt_cond(
        turn_prefix_cond.clone(),
        is_during.then_some(Condition::IsTurnOf {
            player: PlayerRef::You,
        }),
    );
    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter { relation: who },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: c.map(Box::new),
    })
}

fn try_parse_zone_spell_mod(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let sp_pos = tokens.iter().position(|t| t.eq_ci("spells"))?;
    let mut cur = TokenCursor::new(&tokens[sp_pos + 1..]);
    let mut who = if cur.eat_words(&["you", "cast", "from"]) {
        PlayerRelation::You
    } else if cur.eat_words(&["your", "opponents", "cast", "from"]) {
        PlayerRelation::EachOpponent
    } else {
        return None;
    };
    let mut flag_outside = false;
    let mut cast_zone = ZoneMask::empty();
    if cur.eat_words(&["anywhere", "other", "than"])
        && (cur.eat_words(&["your", "hand"])
            || cur.eat_words(&["their", "hands"])
            || cur.eat_words(&["their", "hand"])
            || cur.eat_words(&["a", "player's", "hand"]))
    {
        flag_outside = true;
    } else if cur.eat_words(&["your", "graveyard"])
        || cur.eat_words(&["their", "graveyards"])
        || cur.eat_words(&["a", "graveyard"])
    {
        cast_zone = ZoneMask::GRAVEYARD;
        // Handle "from your graveyard or from exile" (Doc Aurlock)
        if cur.eat_words(&["or", "from", "exile"]) {
            cast_zone |= ZoneMask::EXILE;
        }
    } else if cur.eat_ci("graveyards") {
        cast_zone = ZoneMask::GRAVEYARD;
        if who == PlayerRelation::EachOpponent {
            who = PlayerRelation::Opponent;
        }
        if cur.eat_words(&["or", "from", "exile"]) {
            cast_zone |= ZoneMask::EXILE;
        }
    } else if cur.eat_ci("exile") {
        cast_zone = ZoneMask::EXILE;
    } else {
        return None;
    }
    if !cur.eat_ci("cost") {
        return None;
    }
    let (raw_delta, rest) = take_delta_and_mode(cur.remaining())?;
    if rest.len() != 2 || !rest[0].eq_ci("to") || !rest[1].eq_ci("cast") {
        return None;
    }
    let delta = with_floor_and_colored(raw_delta)?;
    let (mut f, other) = parse_spell_filter_phrase_tokens(&tokens[..sp_pos], ctx)?;
    if other {
        f.flags |= FilterFlags::OTHER;
    }
    if flag_outside {
        f.flags |= FilterFlags::CAST_OUTSIDE_HAND;
    }
    if !cast_zone.is_empty() {
        f.extra
            .push(ObjectPredicate::CastFromZones { zones: cast_zone });
    }
    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter { relation: who },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: turn_prefix_cond.clone().map(Box::new),
    })
}

fn try_parse_nth_spell_mod(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_nth_spell_mod_inner(tokens, ctx, turn_prefix_cond, with_floor_and_colored);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_nth_spell_mod_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    let prefix_during = cur.eat_words(&["during", "your", "turn", ","]);
    if !cur.eat_ci("the") {
        return None;
    }
    let ordinal: u8 = if cur.eat_ci("first") {
        1
    } else if cur.eat_ci("second") {
        2
    } else {
        return None;
    };
    let rem = cur.remaining();
    let you_cast_pos = rem
        .windows(2)
        .position(|w| w[0].eq_ci("you") && w[1].eq_ci("cast"))?;
    let pre = &rem[..you_cast_pos];
    let sp_pos = pre
        .iter()
        .position(|t| t.eq_ci("spell") || t.eq_ci("spells"))?;
    let pre_noun = &pre[..sp_pos];
    let mid_qual = &pre[sp_pos + 1..];

    let mut after = TokenCursor::new(&rem[you_cast_pos + 2..]);
    let during_each = if after.eat_words(&["each", "turn"]) {
        false
    } else if after.eat_words(&["during", "each", "of", "your", "turns"])
        || after.eat_words(&["during", "each", "of", "your", "turn"])
    {
        true
    } else if after.eat_words(&["during", "each", "turn"]) {
        false
    } else {
        return None;
    };
    let post_rem = after.remaining();
    let costs_pos = post_rem.iter().position(|t| t.eq_ci("costs"))?;
    let post_qual = &post_rem[..costs_pos];
    if !mid_qual.is_empty() && !post_qual.is_empty() {
        return None;
    }
    let qual = if !mid_qual.is_empty() {
        mid_qual
    } else {
        post_qual
    };
    let mut combined_phrase = pre_noun.to_vec();
    combined_phrase.extend_from_slice(qual);
    let (f, other) = parse_spell_filter_phrase_tokens(&combined_phrase, ctx)?;
    if other {
        return None;
    }

    let (raw_delta, tail) = take_delta_and_mode(&post_rem[costs_pos + 1..])?;
    if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("cast") {
        return None;
    }
    let delta = with_floor_and_colored(raw_delta)?;
    let has_filter = f != ObjectFilter::default();
    let nth_cond = Condition::NthMatchingSpellThisTurn {
        ordinal,
        filter: has_filter.then(|| Box::new(f.clone())),
    };
    let full_cond = combine_opt_cond(
        combine_opt_cond(
            turn_prefix_cond.clone(),
            (prefix_during || during_each).then_some(Condition::IsTurnOf {
                player: PlayerRef::You,
            }),
        ),
        Some(nth_cond),
    );
    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter {
                relation: PlayerRelation::You,
            },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: full_cond.map(Box::new),
    })
}

fn try_parse_self_cost_mod(
    tokens: &[Token<'_>],
    full_text: &str,
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_self_cost_mod_inner(
        tokens,
        full_text,
        ctx,
        turn_prefix_cond,
        with_floor_and_colored,
    );
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_self_cost_mod_inner(
    tokens: &[Token<'_>],
    full_text: &str,
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    let prefix_during = cur.eat_words(&["during", "your", "turn", ","]);
    let timing_cond = combine_opt_cond(
        turn_prefix_cond.clone(),
        prefix_during.then_some(Condition::IsTurnOf {
            player: PlayerRef::You,
        }),
    );
    if !(cur.eat_words(&["this", "spell"])
        || (cur
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && cur.bump().is_some()))
    {
        return None;
    }
    if !(cur.eat_ci("costs") || cur.eat_ci("cost")) {
        return None;
    }
    let (raw_delta, rest) = take_delta_and_mode(cur.remaining())?;
    let mut delta = with_floor_and_colored(raw_delta)?;
    let mut rc = TokenCursor::new(rest);
    if !rc.eat_words(&["to", "cast"]) {
        return None;
    }
    if rc.is_empty() {
        timing_cond.as_ref()?;
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: timing_cond.map(Box::new),
        });
    }
    if rc.eat_words(&["for", "each"]) {
        let per = parse_cost_metric_amount_tokens(rc.remaining(), ctx)?;
        delta.per = Some(Box::new(per));
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: timing_cond.map(Box::new),
        });
    }
    if rc.eat_ci("if") || rc.eat_words(&["as", "long", "as"]) {
        let cond_toks = rc.remaining();
        let cond_str = tokens_slice_text(full_text, cond_toks);
        let cond = parse_self_cost_condition_tokens(cond_toks, cond_str, ctx)?;
        return Some(StaticDef {
            body: StaticBody::CostModifier {
                affects: SpellOrAbilityFilter::SelfObj,
                delta,
                who: PlayerFilter {
                    relation: PlayerRelation::You,
                },
                who_enchanted_player: false,
                active_zones: ZoneMask::empty(),
            },
            condition: combine_opt_cond(timing_cond, Some(cond)).map(Box::new),
        });
    }
    None
}

fn try_parse_targeting_spell_mod(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res =
        try_parse_targeting_spell_mod_inner(tokens, ctx, turn_prefix_cond, with_floor_and_colored);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_targeting_spell_mod_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let sp_pos = tokens.iter().position(|t| t.eq_ci("spells"))?;
    let tt_pos = tokens[sp_pos + 1..]
        .windows(2)
        .position(|w| w[0].eq_ci("that") && w[1].eq_ci("target"))
        .map(|p| sp_pos + 1 + p)?;

    let who = if tt_pos == sp_pos + 1 {
        PlayerRelation::EachPlayer
    } else {
        let mid = &tokens[sp_pos + 1..tt_pos];
        let mut mc = TokenCursor::new(mid);
        if mc.eat_ci("you") && (mc.eat_ci("cast") || mc.eat_ci("control")) && mc.is_empty() {
            PlayerRelation::You
        } else if mc.eat_words(&["your", "opponents"])
            && (mc.eat_ci("cast") || mc.eat_ci("control"))
            && mc.is_empty()
        {
            PlayerRelation::EachOpponent
        } else {
            return None;
        }
    };

    let after_tt = &tokens[tt_pos + 2..];
    let cost_pos = after_tt.iter().position(|t| t.eq_ci("cost"))?;
    let tgt_toks = &after_tt[..cost_pos];
    let (raw_delta, tail) = take_delta_and_mode(&after_tt[cost_pos + 1..])?;
    if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("cast") {
        return None;
    }
    let delta = with_floor_and_colored(raw_delta)?;
    let (mut f, other) = parse_spell_filter_phrase_tokens(&tokens[..sp_pos], ctx)?;
    if other {
        f.flags |= FilterFlags::OTHER;
    }
    let tmatch = parse_spell_target_clause_filter_tokens(tgt_toks, ctx)?;
    match tmatch {
        TargetClauseMatch::SelfObj => f.extra.push(ObjectPredicate::TargetsSelf),
        TargetClauseMatch::Attached => f.extra.push(ObjectPredicate::TargetsAttached),
        TargetClauseMatch::YouOrFilter(yf) => {
            f.extra.push(ObjectPredicate::TargetsYouOrFilter {
                filter: Box::new(yf),
            });
        }
        TargetClauseMatch::Filter(tf) => {
            f.extra.push(ObjectPredicate::TargetsFilter {
                filter: Box::new(tf),
            });
        }
    }
    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter { relation: who },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: turn_prefix_cond.clone().map(Box::new),
    })
}

fn try_parse_each_spell_mod(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_each_spell_mod_inner(tokens, ctx, turn_prefix_cond, with_floor_and_colored);
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_each_spell_mod_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    if !cur.eat_ci("each") {
        return None;
    }
    // Pattern B: "Each spell (you|your opponents) cast that's <phrase> costs {N} (less|more) to cast"
    if cur.eat_ci("spell") {
        let cp_b = ctx.interner.checkpoint();
        let pat_b = (|| -> Option<StaticDef> {
            let who = if cur.eat_words(&["you", "cast"]) {
                PlayerRelation::You
            } else if cur.eat_words(&["your", "opponents", "cast"]) {
                PlayerRelation::EachOpponent
            } else {
                return None;
            };
            if !cur.eat_ci("that's") {
                return None;
            }
            let rem = cur.remaining();
            let cpos = rem.iter().position(|t| t.eq_ci("costs"))?;
            if cpos == 0 {
                return None;
            }
            let mut f_toks = rem[..cpos].to_vec();
            f_toks.push(Token {
                kind: TokenKind::Word("card"),
                raw: "card",
                span: rem[cpos - 1].span,
            });
            let f = parse_cost_object_phrase(&f_toks, ctx, ZoneMask::empty(), true)?;
            if f.controller.is_some() {
                return None;
            }
            let (raw_delta, tail) = take_delta_and_mode(&rem[cpos + 1..])?;
            if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("cast") {
                return None;
            }
            let delta = with_floor_and_colored(raw_delta)?;
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(f),
                        alt_cost_keyword: None,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            })
        })();
        if let Some(st) = pat_b {
            return Some(st);
        }
        ctx.interner.rollback(cp_b);
        cur.restore(1);
    }

    // Pattern A: "Each [<phrase>] spell [you|your opponents cast] costs {N} (less|more) to cast [except during its controller's turn] [for each <metric>]"
    let rem = cur.remaining();
    let sp_pos = rem.iter().position(|t| t.eq_ci("spell"))?;
    let (mut f, other) = parse_spell_filter_phrase_tokens(&rem[..sp_pos], ctx)?;
    if other {
        return None;
    }
    let _ = &mut f;
    let mut c2 = TokenCursor::new(&rem[sp_pos + 1..]);
    let who = if c2.eat_words(&["you", "cast"]) {
        PlayerRelation::You
    } else if c2.eat_words(&["your", "opponents", "cast"]) {
        PlayerRelation::EachOpponent
    } else {
        PlayerRelation::EachPlayer
    };
    if !c2.eat_ci("costs") {
        return None;
    }
    let (raw_delta, tail) = take_delta_and_mode(c2.remaining())?;
    let mut delta = with_floor_and_colored(raw_delta)?;
    let mut tc = TokenCursor::new(tail);
    if !tc.eat_words(&["to", "cast"]) {
        return None;
    }
    let except_turn = tc.eat_words(&["except", "during", "its", "controller's", "turn"]);
    if tc.eat_words(&["for", "each"]) {
        let per = parse_cost_metric_amount_tokens(tc.remaining(), ctx)?;
        delta.per = Some(Box::new(per));
    } else if !tc.is_empty() {
        return None;
    }
    let cond = combine_opt_cond(
        turn_prefix_cond.clone(),
        except_turn.then_some(Condition::NotSpellControllerTurn),
    );
    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter { relation: who },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: cond.map(Box::new),
    })
}

fn try_parse_chosen_and_exiled_spell_mod(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_chosen_and_exiled_spell_mod_inner(
        tokens,
        ctx,
        turn_prefix_cond,
        with_floor_and_colored,
    );
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_chosen_and_exiled_spell_mod_inner(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    // 1. "[<p> ]spells [with the chosen name] the enchanted player casts [with the chosen name] cost <delta> (less|more) to cast"
    if let Some(ep_pos) = tokens
        .windows(3)
        .position(|w| w[0].eq_ci("the") && w[1].eq_ci("enchanted") && w[2].eq_ci("player"))
        && tokens.get(ep_pos + 3).is_some_and(|t| t.eq_ci("casts"))
    {
        let cp1 = ctx.interner.checkpoint();
        let pre_ep = &tokens[..ep_pos];
        if let Some(sp_pos) = pre_ep.iter().position(|t| t.eq_ci("spells"))
            && let Some((mut f, other)) = parse_spell_filter_phrase_tokens(&pre_ep[..sp_pos], ctx)
        {
            if other {
                f.flags |= FilterFlags::OTHER;
            }
            let mid = &pre_ep[sp_pos + 1..];
            let chosen_mid = mid.len() == 4
                && mid[0].eq_ci("with")
                && mid[1].eq_ci("the")
                && mid[2].eq_ci("chosen")
                && mid[3].eq_ci("name");
            if mid.is_empty() || chosen_mid {
                let mut ac = TokenCursor::new(&tokens[ep_pos + 4..]);
                let chosen_post = ac.eat_words(&["with", "the", "chosen", "name"]);
                if ac.eat_ci("cost")
                    && let Some((raw_delta, tail)) = take_delta_and_mode(ac.remaining())
                    && tail.len() == 2
                    && tail[0].eq_ci("to")
                    && tail[1].eq_ci("cast")
                    && let Some(delta) = with_floor_and_colored(raw_delta)
                {
                    if chosen_mid || chosen_post {
                        f.extra.push(ObjectPredicate::OfChosenName);
                    }
                    return Some(StaticDef {
                        body: StaticBody::CostModifier {
                            affects: SpellOrAbilityFilter::Spell {
                                filter: Box::new(f),
                                alt_cost_keyword: None,
                            },
                            delta,
                            who: PlayerFilter {
                                relation: PlayerRelation::Any,
                            },
                            who_enchanted_player: true,
                            active_zones: ZoneMask::empty(),
                        },
                        condition: turn_prefix_cond.clone().map(Box::new),
                    });
                }
            }
        }
        ctx.interner.rollback(cp1);
    }

    // 2. "Spells with the chosen name [you cast] cost <delta> (less|more) to cast"
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_words(&["spells", "with", "the", "chosen", "name"]) {
        let who = if cur.eat_words(&["you", "cast"]) {
            PlayerRelation::You
        } else if cur.eat_words(&["your", "opponents", "cast"]) {
            PlayerRelation::EachOpponent
        } else {
            PlayerRelation::EachPlayer
        };
        if cur.eat_ci("cost")
            && let Some((raw_delta, tail)) = take_delta_and_mode(cur.remaining())
            && tail.len() == 2
            && tail[0].eq_ci("to")
            && tail[1].eq_ci("cast")
        {
            let delta = with_floor_and_colored(raw_delta)?;
            let mut f = ObjectFilter::default();
            f.extra.push(ObjectPredicate::OfChosenName);
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(f),
                        alt_cost_keyword: None,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            });
        }
    }
    cur.restore(0);

    // 3. "[<p> ]spells (you|your opponents) cast that share a card type with the exiled card cost <delta> (less|more) to cast" (Semblance Anvil)
    if let Some(sp_pos) = tokens.iter().position(|t| t.eq_ci("spells")) {
        let (mut f, other) = parse_spell_filter_phrase_tokens(&tokens[..sp_pos], ctx)?;
        if other {
            f.flags |= FilterFlags::OTHER;
        }
        let mut ac = TokenCursor::new(&tokens[sp_pos + 1..]);
        let who = if ac.eat_words(&["you", "cast"]) {
            Some(PlayerRelation::You)
        } else if ac.eat_words(&["your", "opponents", "cast"]) {
            Some(PlayerRelation::EachOpponent)
        } else {
            None
        };
        if let Some(w) = who
            && ac.eat_words(&[
                "that", "share", "a", "card", "type", "with", "the", "exiled", "card", "cost",
            ])
            && let Some((raw_delta, tail)) = take_delta_and_mode(ac.remaining())
            && tail.len() == 2
            && tail[0].eq_ci("to")
            && tail[1].eq_ci("cast")
        {
            let delta = with_floor_and_colored(raw_delta)?;
            f.extra.push(ObjectPredicate::SharesCardTypeWithExiled);
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Spell {
                        filter: Box::new(f),
                        alt_cost_keyword: None,
                    },
                    delta,
                    who: PlayerFilter { relation: w },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            });
        }
    }

    None
}

fn split_ctrl_tail<'a>(tokens: &'a [Token<'a>]) -> (&'a [Token<'a>], Option<RelativeControl>) {
    if tokens.len() >= 4
        && ((tokens[tokens.len() - 3].eq_ci("an")
            && tokens[tokens.len() - 2].eq_ci("opponent")
            && tokens[tokens.len() - 1].eq_ci("controls"))
            || (tokens[tokens.len() - 3].eq_ci("your")
                && tokens[tokens.len() - 2].eq_ci("opponents")
                && tokens[tokens.len() - 1].eq_ci("control")))
    {
        (&tokens[..tokens.len() - 3], Some(RelativeControl::Opponent))
    } else {
        (tokens, None)
    }
}

fn filter_who(f: Option<&ObjectFilter>) -> PlayerRelation {
    match f.and_then(|x| x.controller) {
        Some(RelativeControl::You) => PlayerRelation::You,
        Some(RelativeControl::Opponent) => PlayerRelation::EachOpponent,
        _ => PlayerRelation::EachPlayer,
    }
}

fn try_parse_activated_ability_cost_mod(
    tokens: &[Token<'_>],
    full_text: &str,
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
    has_rider: bool,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);

    // 1. "Activated abilities cost <delta> (less|more) to activate unless they're mana abilities"
    if cur.eat_words(&["activated", "abilities", "cost"])
        && let Some((raw_delta, tail)) = take_delta_and_mode(cur.remaining())
    {
        let mut tc = TokenCursor::new(tail);
        if tc.eat_words(&["to", "activate", "unless", "they're", "mana", "abilities"])
            && tc.is_empty()
        {
            let delta = with_floor_and_colored(raw_delta)?;
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: None,
                        non_mana_only: true,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter {
                        relation: PlayerRelation::EachPlayer,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            });
        }
    }
    cur.restore(0);

    // 2. "Loyalty abilities of planeswalkers [you|your opponents control] cost <delta> (less|more) to activate"
    if cur.eat_words(&["loyalty", "abilities", "of", "planeswalkers"]) {
        let ctrl = if cur.eat_words(&["you", "control"]) {
            Some(RelativeControl::You)
        } else if cur.eat_words(&["your", "opponents", "control"]) {
            Some(RelativeControl::Opponent)
        } else {
            None
        };
        if cur.eat_ci("cost")
            && let Some((raw_delta, tail)) = take_delta_and_mode(cur.remaining())
            && tail.len() == 2
            && tail[0].eq_ci("to")
            && tail[1].eq_ci("activate")
        {
            let delta = with_floor_and_colored(raw_delta)?;
            let who = match ctrl {
                Some(RelativeControl::You) => PlayerRelation::You,
                Some(RelativeControl::Opponent) => PlayerRelation::EachOpponent,
                _ => PlayerRelation::EachPlayer,
            };
            return Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: Some(Box::new(ObjectFilter {
                            types_any: CardTypeMask::PLANESWALKER,
                            controller: ctrl,
                            ..Default::default()
                        })),
                        non_mana_only: false,
                        loyalty_only: true,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            });
        }
    }
    cur.restore(0);

    // 3. "(Enchanted|Equipped) (creature|permanent|artifact|land|planeswalker)'s activated abilities cost [up to] <delta> (less|more) to activate[ unless they're mana abilities]"
    if (cur.eat_ci("enchanted") || cur.eat_ci("equipped"))
        && cur.bump().is_some_and(|t| {
            matches!(
                t.raw.to_ascii_lowercase().as_str(),
                "creature's" | "permanent's" | "artifact's" | "land's" | "planeswalker's"
            )
        })
        && cur.eat_words(&["activated", "abilities", "cost"])
    {
        let up_to = cur.eat_words(&["up", "to"]);
        if let Some((raw_delta, tail)) = take_delta_and_mode(cur.remaining())
            && (!up_to || (raw_delta.mode == CostDeltaMode::Reduce && raw_delta.colored.is_empty()))
        {
            let mut tc = TokenCursor::new(tail);
            if tc.eat_words(&["to", "activate"]) {
                let non_mana_only = tc.eat_words(&["unless", "they're", "mana", "abilities"]);
                if tc.is_empty() {
                    let delta = with_floor_and_colored(raw_delta)?;
                    return Some(StaticDef {
                        body: StaticBody::CostModifier {
                            affects: SpellOrAbilityFilter::Ability {
                                keyword: None,
                                filter: None,
                                non_mana_only,
                                loyalty_only: false,
                                source_zone: None,
                                self_source_only: false,
                                attached_source_only: true,
                                targets_self: false,
                                targets_attached: false,
                                targets_filter: None,
                                first_matching_this_turn: false,
                            },
                            delta,
                            who: PlayerFilter {
                                relation: PlayerRelation::EachPlayer,
                            },
                            who_enchanted_player: false,
                            active_zones: ZoneMask::empty(),
                        },
                        condition: turn_prefix_cond.clone().map(Box::new),
                    });
                }
            }
        }
    }
    cur.restore(0);

    // 4. "[As long as <cond>, ]activated abilities of [<p> ]cards in your graveyard cost [up to ]<delta> (less|more) to activate"
    {
        let saved = cur.save();
        let cp4 = ctx.interner.checkpoint();
        let arm4 = (|| -> Option<StaticDef> {
            let as_long_cond = if cur.eat_words(&["as", "long", "as"]) {
                let comma = cur
                    .remaining()
                    .iter()
                    .position(|t| t.is_punct(Punct::Comma))?;
                let cond_toks = &cur.remaining()[..comma];
                let cond_str = tokens_slice_text(full_text, cond_toks);
                let c = parse_self_cost_condition_tokens(cond_toks, cond_str, ctx)?;
                for _ in 0..=comma {
                    cur.bump();
                }
                Some(c)
            } else {
                None
            };
            if !cur.eat_words(&["activated", "abilities", "of"]) {
                return None;
            }
            let in_pos = cur
                .remaining()
                .windows(3)
                .position(|w| w[0].eq_ci("in") && w[1].eq_ci("your") && w[2].eq_ci("graveyard"))?;
            if in_pos < 1
                || (!cur.remaining()[in_pos - 1].eq_ci("card")
                    && !cur.remaining()[in_pos - 1].eq_ci("cards"))
            {
                return None;
            }
            let f_toks = &cur.remaining()[..in_pos];
            let mut ac = TokenCursor::new(&cur.remaining()[in_pos + 3..]);
            if !ac.eat_ci("cost") {
                return None;
            }
            let up_to = ac.eat_words(&["up", "to"]);
            let (raw_delta, tail) = take_delta_and_mode(ac.remaining())?;
            if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("activate") {
                return None;
            }
            if up_to && (raw_delta.mode != CostDeltaMode::Reduce || !raw_delta.colored.is_empty()) {
                return None;
            }
            let f = if f_toks.len() == 1 {
                None
            } else {
                let parsed = parse_cost_object_phrase(f_toks, ctx, ZoneMask::empty(), true)?;
                if parsed.controller.is_some() {
                    return None;
                }
                Some(Box::new(parsed))
            };
            let delta = with_floor_and_colored(raw_delta)?;
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: f,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: Some(Zone::Graveyard),
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: combine_opt_cond(turn_prefix_cond.clone(), as_long_cond).map(Box::new),
            })
        })();
        if let Some(st) = arm4 {
            return Some(st);
        }
        ctx.interner.rollback(cp4);
        cur.restore(saved);
    }

    // 5. "Activated abilities of sources with the chosen name cost <delta> (less|more) to activate[ unless they're mana abilities]"
    if cur.eat_words(&[
        "activated",
        "abilities",
        "of",
        "sources",
        "with",
        "the",
        "chosen",
        "name",
        "cost",
    ]) && let Some((raw_delta, tail)) = take_delta_and_mode(cur.remaining())
    {
        let mut tc = TokenCursor::new(tail);
        if tc.eat_words(&["to", "activate"]) {
            let non_mana_only = tc.eat_words(&["unless", "they're", "mana", "abilities"]);
            if tc.is_empty() {
                let delta = with_floor_and_colored(raw_delta)?;
                let mut f = ObjectFilter::default();
                f.extra.push(ObjectPredicate::OfChosenName);
                return Some(StaticDef {
                    body: StaticBody::CostModifier {
                        affects: SpellOrAbilityFilter::Ability {
                            keyword: None,
                            filter: Some(Box::new(f)),
                            non_mana_only,
                            loyalty_only: false,
                            source_zone: None,
                            self_source_only: false,
                            attached_source_only: false,
                            targets_self: false,
                            targets_attached: false,
                            targets_filter: None,
                            first_matching_this_turn: false,
                        },
                        delta,
                        who: PlayerFilter {
                            relation: PlayerRelation::EachPlayer,
                        },
                        who_enchanted_player: false,
                        active_zones: ZoneMask::empty(),
                    },
                    condition: turn_prefix_cond.clone().map(Box::new),
                });
            }
        }
    }
    cur.restore(0);

    // 6. "Activated abilities of <phrase> cost an additional \"<cost>\" to activate" (Brutal Suppression)
    if !has_rider {
        let cp6 = ctx.interner.checkpoint();
        let arm6 = (|| -> Option<StaticDef> {
            if !cur.eat_words(&["activated", "abilities", "of"]) {
                return None;
            }
            let rem = cur.remaining();
            let cpos = rem.iter().position(|t| t.eq_ci("cost"))?;
            if !rem.get(cpos + 1).is_some_and(|t| t.eq_ci("an"))
                || !rem.get(cpos + 2).is_some_and(|t| t.eq_ci("additional"))
            {
                return None;
            }
            let qtok = rem.get(cpos + 3)?;
            let TokenKind::QuotedSpan(inner_cost) = qtok.kind else {
                return None;
            };
            if !rem.get(cpos + 4).is_some_and(|t| t.eq_ci("to"))
                || !rem.get(cpos + 5).is_some_and(|t| t.eq_ci("activate"))
                || rem.len() != cpos + 6
            {
                return None;
            }
            let (raw_src, opp_ctrl) = split_ctrl_tail(&rem[..cpos]);
            let mut src_f = parse_cost_object_phrase(raw_src, ctx, ZoneMask::empty(), false)?;
            if let Some(c) = opp_ctrl {
                src_f.controller = Some(c);
            }
            let parsed_c = parse_cost_list(inner_cost, 0, ctx);
            if !parsed_c.unparsed.is_empty() || parsed_c.defs.is_empty() {
                return None;
            }
            let who = filter_who(Some(&src_f));
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: Some(Box::new(src_f)),
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta: CostDelta {
                        mode: CostDeltaMode::Increase,
                        generic: 0,
                        per: None,
                        additional_costs: parsed_c.defs,
                        colored: SmallVec::new(),
                        hybrid_colored: SmallVec::new(),
                        colored_only: false,
                        min_total_mana: None,
                        max_reduction: None,
                        per_target_life: None,
                    },
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            })
        })();
        if let Some(st) = arm6 {
            return Some(st);
        }
        ctx.interner.rollback(cp6);
        cur.restore(0);
    }

    // 7. "Activated abilities of <phrase> cost {X} (less|more) to activate, where X is ..." (Agatha)
    {
        let cp7 = ctx.interner.checkpoint();
        let arm7 = (|| -> Option<StaticDef> {
            if !cur.eat_words(&["activated", "abilities", "of"]) {
                return None;
            }
            let wx_pos = cur.remaining().windows(4).position(|w| {
                w[0].is_punct(Punct::Comma)
                    && w[1].eq_ci("where")
                    && w[2].eq_ci("x")
                    && w[3].eq_ci("is")
            })?;
            let head = &cur.remaining()[..wx_pos];
            if head.len() < 6
                || !head[head.len() - 2].eq_ci("to")
                || !head[head.len() - 1].eq_ci("activate")
            {
                return None;
            }
            let mode = if head[head.len() - 3].eq_ci("less") {
                CostDeltaMode::Reduce
            } else if head[head.len() - 3].eq_ci("more") {
                CostDeltaMode::Increase
            } else {
                return None;
            };
            if !matches!(
                head[head.len() - 4].kind,
                TokenKind::ManaSymbol(ManaSymbolKind::Variable('X'))
            ) || !head[head.len() - 5].eq_ci("cost")
            {
                return None;
            }
            let (raw_src, opp_ctrl) = split_ctrl_tail(&head[..head.len() - 5]);
            let mut src_f = parse_cost_object_phrase(raw_src, ctx, ZoneMask::empty(), false)?;
            if let Some(c) = opp_ctrl {
                src_f.controller = Some(c);
            }
            let per = parse_where_x_body_tokens(&cur.remaining()[wx_pos + 4..], ctx)?;
            let delta = with_floor_and_colored(CostDelta {
                mode,
                generic: 1,
                per: Some(Box::new(per)),
                additional_costs: Vec::new(),
                colored: SmallVec::new(),
                hybrid_colored: SmallVec::new(),
                colored_only: false,
                min_total_mana: None,
                max_reduction: None,
                per_target_life: None,
            })?;
            let who = filter_who(Some(&src_f));
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: Some(Box::new(src_f)),
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            })
        })();
        if let Some(st) = arm7 {
            return Some(st);
        }
        ctx.interner.rollback(cp7);
        cur.restore(0);
    }

    // 7b. "The first activated ability you activate during your turn that targets a creature you control costs {2} less to activate" (Professor Hojo)
    {
        let cp7b = ctx.interner.checkpoint();
        let arm7b = (|| -> Option<StaticDef> {
            if !cur.eat_words(&[
                "the",
                "first",
                "activated",
                "ability",
                "you",
                "activate",
                "during",
                "your",
                "turn",
                "that",
                "targets",
            ]) {
                return None;
            }
            let cpos = cur.remaining().iter().position(|t| t.eq_ci("costs"))?;
            let tgt_toks = &cur.remaining()[..cpos];
            let (raw_delta, tail) = take_delta_and_mode(&cur.remaining()[cpos + 1..])?;
            if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("activate") {
                return None;
            }
            let delta = with_floor_and_colored(raw_delta)?;
            let TargetClauseMatch::Filter(tf) =
                parse_spell_target_clause_filter_tokens(tgt_toks, ctx)?
            else {
                return None;
            };
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: None,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: Some(Box::new(tf)),
                        first_matching_this_turn: true,
                    },
                    delta,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: combine_opt_cond(
                    turn_prefix_cond.clone(),
                    Some(Condition::IsTurnOf {
                        player: PlayerRef::You,
                    }),
                )
                .map(Box::new),
            })
        })();
        if let Some(st) = arm7b {
            return Some(st);
        }
        ctx.interner.rollback(cp7b);
        cur.restore(0);
    }

    // 8. "~'s (equip|boast|exhaust|ninjutsu|power-up) abilit(y|ies) cost(s) [up to] <delta> (less|more) to activate [for each ... | if ...]"
    {
        let cp8 = ctx.interner.checkpoint();
        let arm8 = (|| -> Option<StaticDef> {
            if !cur
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                || !cur
                    .peek_n(1)
                    .is_some_and(|t| t.raw == "'s" || t.raw == "’s")
            {
                return None;
            }
            cur.bump();
            cur.bump();
            let kw = eat_ability_keyword(&mut cur)?;
            if !(cur.eat_ci("ability") || cur.eat_ci("abilities"))
                || !(cur.eat_ci("cost") || cur.eat_ci("costs"))
            {
                return None;
            }
            let up_to = cur.eat_words(&["up", "to"]);
            let (raw_delta, tail) = take_delta_and_mode(cur.remaining())?;
            if up_to && (raw_delta.mode != CostDeltaMode::Reduce || !raw_delta.colored.is_empty()) {
                return None;
            }
            let mut delta = with_floor_and_colored(raw_delta)?;
            let mut tc = TokenCursor::new(tail);
            if !tc.eat_words(&["to", "activate"]) {
                return None;
            }
            let mut if_cond = None;
            let mut targets_filter = None;
            if tc.eat_words(&["for", "each"]) {
                let per = parse_cost_metric_amount_tokens(tc.remaining(), ctx)?;
                delta.per = Some(Box::new(per));
            } else if tc.eat_ci("if") {
                let rem = tc.remaining();
                if rem.len() >= 4
                    && rem[0].eq_ci("it")
                    && rem[1].eq_ci("targets")
                    && (rem[2].eq_ci("a") || rem[2].eq_ci("an"))
                {
                    let TargetClauseMatch::Filter(tf) =
                        parse_spell_target_clause_filter_tokens(&rem[2..], ctx)?
                    else {
                        return None;
                    };
                    targets_filter = Some(Box::new(tf));
                } else {
                    let cstr = tokens_slice_text(full_text, rem);
                    if_cond = Some(parse_self_cost_condition_tokens(rem, cstr, ctx)?);
                }
            } else if !tc.is_empty() {
                return None;
            }
            let kw_id = ctx.interner.intern_keyword(kw);
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: Some(kw_id),
                        filter: None,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: true,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: combine_opt_cond(turn_prefix_cond.clone(), if_cond).map(Box::new),
            })
        })();
        if let Some(st) = arm8 {
            return Some(st);
        }
        ctx.interner.rollback(cp8);
        cur.restore(0);
    }

    // 9. "(Equip|Boast|Exhaust|Ninjutsu|Power-up|Cycling) abilities [you|your opponents activate] [of <p>] [that target ...] cost [up to] <delta> (less|more) to activate ..."
    {
        let cp9 = ctx.interner.checkpoint();
        let arm9 = (|| -> Option<StaticDef> {
            let kw = eat_ability_keyword(&mut cur)?;
            if !cur.eat_ci("abilities") {
                return None;
            }
            let who_act = if cur.eat_words(&["you", "activate"]) {
                Some(PlayerRelation::You)
            } else if cur.eat_words(&["your", "opponents", "activate"]) {
                Some(PlayerRelation::EachOpponent)
            } else {
                None
            };
            let rem = cur.remaining();
            let cost_pos = rem.iter().position(|t| t.eq_ci("cost"))?;
            let mid = &rem[..cost_pos];
            let (of_toks, tgt_toks) = if let Some(tt_pos) = mid
                .windows(2)
                .position(|w| w[0].eq_ci("that") && w[1].eq_ci("target"))
            {
                (&mid[..tt_pos], Some(&mid[tt_pos + 2..]))
            } else {
                (mid, None)
            };
            let (mut src_filter, mut src_zone) = (None, None);
            if !of_toks.is_empty() {
                if !of_toks[0].eq_ci("of") || of_toks.len() < 2 {
                    return None;
                }
                let raw_of = &of_toks[1..];
                if raw_of.len() == 4
                    && raw_of[0].eq_ci("cards")
                    && raw_of[1].eq_ci("in")
                    && raw_of[2].eq_ci("your")
                    && raw_of[3].eq_ci("hand")
                {
                    src_zone = Some(Zone::Hand);
                } else {
                    let (inner, opp_ctrl) = split_ctrl_tail(raw_of);
                    let mut f = parse_cost_object_phrase(inner, ctx, ZoneMask::empty(), false)?;
                    if let Some(c) = opp_ctrl {
                        f.controller = Some(c);
                    }
                    src_filter = Some(Box::new(f));
                }
            }
            let (mut t_self, mut t_att, mut t_filt) = (false, false, None);
            if let Some(tt) = tgt_toks {
                match parse_spell_target_clause_filter_tokens(tt, ctx)? {
                    TargetClauseMatch::SelfObj => t_self = true,
                    TargetClauseMatch::Attached => t_att = true,
                    TargetClauseMatch::Filter(tf) => t_filt = Some(Box::new(tf)),
                    TargetClauseMatch::YouOrFilter(_) => return None,
                }
            }
            let mut dc = TokenCursor::new(&rem[cost_pos + 1..]);
            let up_to = dc.eat_words(&["up", "to"]);
            let (raw_delta, tail) = take_delta_and_mode(dc.remaining())?;
            if up_to && (raw_delta.mode != CostDeltaMode::Reduce || !raw_delta.colored.is_empty()) {
                return None;
            }
            let mut delta = with_floor_and_colored(raw_delta)?;
            let mut tc = TokenCursor::new(tail);
            if !tc.eat_words(&["to", "activate"]) {
                return None;
            }
            let mut if_cond = None;
            if tc.eat_words(&["for", "each"]) {
                delta.per = Some(Box::new(parse_cost_metric_amount_tokens(
                    tc.remaining(),
                    ctx,
                )?));
            } else if tc.eat_ci("if") {
                let cstr = tokens_slice_text(full_text, tc.remaining());
                if_cond = Some(parse_self_cost_condition_tokens(tc.remaining(), cstr, ctx)?);
            } else if !tc.is_empty() {
                return None;
            }
            let who = who_act.unwrap_or_else(|| {
                if src_zone == Some(Zone::Hand) {
                    PlayerRelation::You
                } else {
                    filter_who(src_filter.as_deref())
                }
            });
            let kw_id = ctx.interner.intern_keyword(kw);
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: Some(kw_id),
                        filter: src_filter,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: src_zone,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: t_self,
                        targets_attached: t_att,
                        targets_filter: t_filt,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: combine_opt_cond(turn_prefix_cond.clone(), if_cond).map(Box::new),
            })
        })();
        if let Some(st) = arm9 {
            return Some(st);
        }
        ctx.interner.rollback(cp9);
        cur.restore(0);
    }

    // 10. "(Activated )?abilities [you|your opponents activate | of <p>] that target (~ | enchanted/equipped creature | a/an <p>) cost <delta> (less|more) to activate"
    {
        let cp10 = ctx.interner.checkpoint();
        let arm10 = (|| -> Option<StaticDef> {
            let _ = cur.eat_ci("activated");
            if !cur.eat_ci("abilities") {
                return None;
            }
            let rem = cur.remaining();
            let tt_pos = rem
                .windows(2)
                .position(|w| w[0].eq_ci("that") && w[1].eq_ci("target"))?;
            let mid = &rem[..tt_pos];
            let mut who_act = None;
            let mut src_filter = None;
            if mid.is_empty() {
                // ok
            } else if mid.len() == 2 && mid[0].eq_ci("you") && mid[1].eq_ci("activate") {
                who_act = Some(PlayerRelation::You);
            } else if mid.len() == 3
                && mid[0].eq_ci("your")
                && mid[1].eq_ci("opponents")
                && mid[2].eq_ci("activate")
            {
                who_act = Some(PlayerRelation::EachOpponent);
            } else if mid.len() >= 2 && mid[0].eq_ci("of") {
                let (inner, opp_ctrl) = split_ctrl_tail(&mid[1..]);
                let mut f = parse_cost_object_phrase(inner, ctx, ZoneMask::empty(), false)?;
                if let Some(c) = opp_ctrl {
                    f.controller = Some(c);
                }
                src_filter = Some(Box::new(f));
            } else {
                return None;
            }
            let after = &rem[tt_pos + 2..];
            let cost_pos = after.iter().position(|t| t.eq_ci("cost"))?;
            let (raw_delta, tail) = take_delta_and_mode(&after[cost_pos + 1..])?;
            if tail.len() != 2 || !tail[0].eq_ci("to") || !tail[1].eq_ci("activate") {
                return None;
            }
            let delta = with_floor_and_colored(raw_delta)?;
            let (mut t_self, mut t_att, mut t_filt) = (false, false, None);
            match parse_spell_target_clause_filter_tokens(&after[..cost_pos], ctx)? {
                TargetClauseMatch::SelfObj => t_self = true,
                TargetClauseMatch::Attached => t_att = true,
                TargetClauseMatch::Filter(tf) => t_filt = Some(Box::new(tf)),
                TargetClauseMatch::YouOrFilter(_) => return None,
            }
            let who = who_act.unwrap_or_else(|| filter_who(src_filter.as_deref()));
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: src_filter,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: t_self,
                        targets_attached: t_att,
                        targets_filter: t_filt,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            })
        })();
        if let Some(st) = arm10 {
            return Some(st);
        }
        ctx.interner.rollback(cp10);
        cur.restore(0);
    }

    // 11. "Activated abilities of <phrase> cost [up to] <delta> (less|more) to activate [unless they're mana abilities] [for each <metric>]"
    {
        let cp11 = ctx.interner.checkpoint();
        let arm11 = (|| -> Option<StaticDef> {
            if !cur.eat_words(&["activated", "abilities", "of"]) {
                return None;
            }
            let rem = cur.remaining();
            let cost_pos = rem.iter().position(|t| t.eq_ci("cost"))?;
            if cost_pos == 0 {
                return None;
            }
            let tgt_toks = &rem[..cost_pos];
            let mut dc = TokenCursor::new(&rem[cost_pos + 1..]);
            let up_to = dc.eat_words(&["up", "to"]);
            let (raw_delta, tail) = take_delta_and_mode(dc.remaining())?;
            if up_to && (raw_delta.mode != CostDeltaMode::Reduce || !raw_delta.colored.is_empty()) {
                return None;
            }
            let mut delta = with_floor_and_colored(raw_delta)?;
            let mut tc = TokenCursor::new(tail);
            if !tc.eat_words(&["to", "activate"]) {
                return None;
            }
            let non_mana_only = tc.eat_words(&["unless", "they're", "mana", "abilities"]);
            if tc.eat_words(&["for", "each"]) {
                delta.per = Some(Box::new(parse_cost_metric_amount_tokens(
                    tc.remaining(),
                    ctx,
                )?));
            } else if !tc.is_empty() {
                return None;
            }
            if tgt_toks.len() == 1 && matches!(tgt_toks[0].kind, TokenKind::SelfRef) {
                return Some(StaticDef {
                    body: StaticBody::CostModifier {
                        affects: SpellOrAbilityFilter::Ability {
                            keyword: None,
                            filter: None,
                            non_mana_only,
                            loyalty_only: false,
                            source_zone: None,
                            self_source_only: true,
                            attached_source_only: false,
                            targets_self: false,
                            targets_attached: false,
                            targets_filter: None,
                            first_matching_this_turn: false,
                        },
                        delta,
                        who: PlayerFilter {
                            relation: PlayerRelation::You,
                        },
                        who_enchanted_player: false,
                        active_zones: ZoneMask::empty(),
                    },
                    condition: turn_prefix_cond.clone().map(Box::new),
                });
            }
            let (inner, opp_ctrl) = split_ctrl_tail(tgt_toks);
            let mut src_f = parse_cost_object_phrase(inner, ctx, ZoneMask::empty(), false)?;
            if let Some(c) = opp_ctrl {
                src_f.controller = Some(c);
            }
            let who = filter_who(Some(&src_f));
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: None,
                        filter: Some(Box::new(src_f)),
                        non_mana_only,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter { relation: who },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: turn_prefix_cond.clone().map(Box::new),
            })
        })();
        if let Some(st) = arm11 {
            return Some(st);
        }
        ctx.interner.rollback(cp11);
        cur.restore(0);
    }

    // 12. "Equipment you control have equip {0} [as long as <cond> | if <cond>]" (Puresteel Paladin)
    if !has_rider {
        let cp12 = ctx.interner.checkpoint();
        let arm12 = (|| -> Option<StaticDef> {
            if !cur.eat_words(&["equipment", "you", "control", "have", "equip"])
                || !cur.peek().is_some_and(|t| {
                    matches!(t.kind, TokenKind::ManaSymbol(ManaSymbolKind::Generic(0)))
                })
            {
                return None;
            }
            cur.bump();
            let cond = if cur.is_empty() {
                None
            } else if cur.eat_words(&["as", "long", "as"]) || cur.eat_ci("if") {
                let rem = cur.remaining();
                let mut cc = TokenCursor::new(rem);
                if cc.eat_words(&["you", "control"])
                    && let Some(min_n) = cc.bump().and_then(|t| parse_small_number(t.raw))
                    && cc.eat_words(&["or", "more"])
                    && !cc.is_empty()
                {
                    let mut f =
                        parse_cost_object_phrase(cc.remaining(), ctx, ZoneMask::empty(), false)?;
                    if f.controller.is_some() {
                        return None;
                    }
                    f.controller = Some(RelativeControl::You);
                    Some(Condition::Compare {
                        left: Box::new(Amount::Count { of: Box::new(f) }),
                        op: CompareOp::Ge,
                        right: Box::new(Amount::Const { n: min_n }),
                    })
                } else {
                    let cstr = tokens_slice_text(full_text, rem);
                    Some(parse_self_cost_condition_tokens(rem, cstr, ctx)?)
                }
            } else {
                return None;
            };
            let delta = with_floor_and_colored(CostDelta {
                mode: CostDeltaMode::SetZero,
                generic: 0,
                per: None,
                additional_costs: Vec::new(),
                colored: SmallVec::new(),
                hybrid_colored: SmallVec::new(),
                colored_only: false,
                min_total_mana: None,
                max_reduction: None,
                per_target_life: None,
            })?;
            let kw_id = ctx.interner.intern_keyword("equip");
            Some(StaticDef {
                body: StaticBody::CostModifier {
                    affects: SpellOrAbilityFilter::Ability {
                        keyword: Some(kw_id),
                        filter: None,
                        non_mana_only: false,
                        loyalty_only: false,
                        source_zone: None,
                        self_source_only: false,
                        attached_source_only: false,
                        targets_self: false,
                        targets_attached: false,
                        targets_filter: None,
                        first_matching_this_turn: false,
                    },
                    delta,
                    who: PlayerFilter {
                        relation: PlayerRelation::You,
                    },
                    who_enchanted_player: false,
                    active_zones: ZoneMask::empty(),
                },
                condition: combine_opt_cond(turn_prefix_cond.clone(), cond).map(Box::new),
            })
        })();
        if let Some(st) = arm12 {
            return Some(st);
        }
        ctx.interner.rollback(cp12);
    }

    None
}

fn eat_ability_keyword(cur: &mut TokenCursor<'_>) -> Option<&'static str> {
    if cur.eat_ci("equip") {
        Some("equip")
    } else if cur.eat_ci("boast") {
        Some("boast")
    } else if cur.eat_ci("exhaust") {
        Some("exhaust")
    } else if cur.eat_ci("ninjutsu") {
        Some("ninjutsu")
    } else if cur.eat_ci("cycling") {
        Some("cycling")
    } else if cur.eat_hyphenated("power", "up") {
        Some("power-up")
    } else {
        None
    }
}

fn try_parse_general_spell_mod(
    tokens: &[Token<'_>],
    full_text: &str,
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let cp = ctx.interner.checkpoint();
    let res = try_parse_general_spell_mod_inner(
        tokens,
        full_text,
        ctx,
        turn_prefix_cond,
        with_floor_and_colored,
    );
    if res.is_none() {
        ctx.interner.rollback(cp);
    }
    res
}

fn try_parse_general_spell_mod_inner(
    tokens: &[Token<'_>],
    full_text: &str,
    ctx: &mut ParseCtx<'_>,
    turn_prefix_cond: &Option<Condition>,
    with_floor_and_colored: &dyn Fn(CostDelta) -> Option<CostDelta>,
) -> Option<StaticDef> {
    let mut cur = TokenCursor::new(tokens);
    let prefix_during = cur.eat_words(&["during", "your", "turn", ","]);
    let mut extra_cond: Option<Condition> = None;
    if cur.eat_words(&["as", "long", "as"]) {
        if cur.eat_words(&[
            "~",
            "is",
            "in",
            "the",
            "command",
            "zone",
            "or",
            "on",
            "the",
            "battlefield",
            ",",
        ]) {
            cur.restore(if prefix_during { 4 } else { 0 });
        } else {
            let comma_idx = cur
                .remaining()
                .iter()
                .position(|t| t.is_punct(Punct::Comma))?;
            let cond_toks = &cur.remaining()[..comma_idx];
            let cond_str = tokens_slice_text(full_text, cond_toks);
            let parsed_c = parse_self_cost_condition_tokens(cond_toks, cond_str, ctx)?;
            extra_cond = Some(parsed_c);
            for _ in 0..=comma_idx {
                cur.bump();
            }
        }
    }
    let active_cmd_bf = cur.eat_words(&[
        "as",
        "long",
        "as",
        "~",
        "is",
        "in",
        "the",
        "command",
        "zone",
        "or",
        "on",
        "the",
        "battlefield",
        ",",
    ]);

    let rem = cur.remaining();
    let sp_pos = rem.iter().position(|t| t.eq_ci("spells"))?;
    let cost_pos = rem[sp_pos + 1..]
        .iter()
        .position(|t| t.eq_ci("cost"))
        .map(|p| sp_pos + 1 + p)?;

    let pre_spell = &rem[..sp_pos];
    let mc = TokenCursor::new(&rem[sp_pos + 1..cost_pos]);

    // Check optional post-spell qualifiers before/after "of the chosen type/color" and "(you|your opponents) cast"
    let mut chosen_type = false;
    let mut chosen_color = false;
    let mut mid_qual: &[Token<'_>] = &[];
    let mut post_qual: &[Token<'_>] = &[];

    // Find "(you|your opponents) cast" if present inside mc.remaining()
    let mrem = mc.remaining();
    let (who, pre_who_end, post_who_start) = if let Some(p) = mrem
        .windows(2)
        .position(|w| w[0].eq_ci("you") && w[1].eq_ci("cast"))
    {
        (PlayerRelation::You, p, p + 2)
    } else if let Some(p) = mrem
        .windows(3)
        .position(|w| w[0].eq_ci("your") && w[1].eq_ci("opponents") && w[2].eq_ci("cast"))
    {
        (PlayerRelation::EachOpponent, p, p + 3)
    } else {
        (PlayerRelation::EachPlayer, mrem.len(), mrem.len())
    };

    // Strip optional "of the chosen type/color" from pre-who and post-who slices
    let mut pre_who_slice = &mrem[..pre_who_end];
    if pre_who_slice.len() >= 4
        && pre_who_slice[pre_who_slice.len() - 4].eq_ci("of")
        && pre_who_slice[pre_who_slice.len() - 3].eq_ci("the")
        && pre_who_slice[pre_who_slice.len() - 2].eq_ci("chosen")
    {
        if pre_who_slice[pre_who_slice.len() - 1].eq_ci("type") {
            chosen_type = true;
            pre_who_slice = &pre_who_slice[..pre_who_slice.len() - 4];
        } else if pre_who_slice[pre_who_slice.len() - 1].eq_ci("color") {
            chosen_color = true;
            pre_who_slice = &pre_who_slice[..pre_who_slice.len() - 4];
        }
    }
    if !pre_who_slice.is_empty() {
        mid_qual = pre_who_slice;
    }

    let mut post_who_slice = &mrem[post_who_start..];
    if post_who_slice.len() >= 4
        && post_who_slice[0].eq_ci("of")
        && post_who_slice[1].eq_ci("the")
        && post_who_slice[2].eq_ci("chosen")
    {
        if post_who_slice[3].eq_ci("type") {
            if chosen_type {
                return None;
            }
            chosen_type = true;
            post_who_slice = &post_who_slice[4..];
        } else if post_who_slice[3].eq_ci("color") {
            if chosen_color {
                return None;
            }
            chosen_color = true;
            post_who_slice = &post_who_slice[4..];
        }
    }
    if !post_who_slice.is_empty() {
        post_qual = post_who_slice;
    }
    if !mid_qual.is_empty() && !post_qual.is_empty() {
        return None;
    }

    let qual = if !mid_qual.is_empty() {
        mid_qual
    } else {
        post_qual
    };
    let mut combined_phrase = pre_spell.to_vec();
    combined_phrase.extend_from_slice(qual);
    let (mut f, other) = parse_spell_filter_phrase_tokens(&combined_phrase, ctx)?;
    if other {
        f.flags |= FilterFlags::OTHER;
    }
    if chosen_type {
        f.extra.push(ObjectPredicate::OfChosenType);
    }
    if chosen_color {
        f.extra.push(ObjectPredicate::OfChosenColor);
    }

    let (raw_delta, tail) = take_delta_and_mode(&rem[cost_pos + 1..])?;
    let mut delta = with_floor_and_colored(raw_delta)?;
    let mut tc = TokenCursor::new(tail);
    if !tc.eat_words(&["to", "cast"]) {
        return None;
    }
    if tc.eat_words(&["for", "each"]) {
        let per = parse_cost_metric_amount_tokens(tc.remaining(), ctx)?;
        delta.per = Some(Box::new(per));
    } else if tc.eat_words(&["as", "long", "as"]) || tc.eat_ci("if") {
        let rem_c = tc.remaining();
        let cstr = tokens_slice_text(full_text, rem_c);
        let parsed_c = parse_self_cost_condition_tokens(rem_c, cstr, ctx)?;
        extra_cond = combine_opt_cond(extra_cond, Some(parsed_c));
    } else if !tc.is_empty() {
        return None;
    }

    let full_cond = combine_opt_cond(
        combine_opt_cond(
            turn_prefix_cond.clone(),
            prefix_during.then_some(Condition::IsTurnOf {
                player: PlayerRef::You,
            }),
        ),
        extra_cond,
    );

    Some(StaticDef {
        body: StaticBody::CostModifier {
            affects: SpellOrAbilityFilter::Spell {
                filter: Box::new(f),
                alt_cost_keyword: None,
            },
            delta,
            who: PlayerFilter { relation: who },
            who_enchanted_player: false,
            active_zones: if active_cmd_bf {
                ZoneMask::BATTLEFIELD | ZoneMask::COMMAND
            } else {
                ZoneMask::empty()
            },
        },
        condition: full_cond.map(Box::new),
    })
}

pub fn parse_mana_spending_restriction(
    sentence: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ManaSpendingRestriction> {
    let trimmed = sentence.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        return None;
    }
    let cp = ctx.interner.checkpoint();
    let mut sub_ctx = ctx.with_normalized_text(trimmed);
    let tokens = tokenize(trimmed, 0);
    let mut cur = TokenCursor::new(&tokens);

    if cur.eat_words(&[
        "this", "mana", "can't", "be", "spent", "to", "pay", "generic", "mana", "costs",
    ]) && cur.is_empty()
    {
        return Some(ManaSpendingRestriction {
            disallow_generic: true,
            ..Default::default()
        });
    }
    cur.restore(0);

    if cur.eat_words(&["spend", "this", "mana", "only", "to", "foretell", "cards"])
        && cur.is_empty()
    {
        return Some(ManaSpendingRestriction {
            allow_abilities: Some(AllowAbilitiesRestriction {
                any_ability: false,
                source_filter: None,
                foretell: true,
            }),
            ..Default::default()
        });
    }
    cur.restore(0);

    if cur.eat_words(&["spend", "this", "mana", "only", "to", "cast"]) {
        let rem = cur.remaining();
        // Check "a creature spell of the chosen type, and that spell can't be countered"
        let mut c2 = TokenCursor::new(rem);
        if c2.eat_words(&[
            "a",
            "creature",
            "spell",
            "of",
            "the",
            "chosen",
            "type",
            ",",
            "and",
            "that",
            "spell",
            "can't",
            "be",
            "countered",
        ]) && c2.is_empty()
        {
            return Some(ManaSpendingRestriction {
                allow_spells: Some(AllowSpellsRestriction {
                    filter: Some(PlainManaFilter {
                        types_any: CardTypeMask::CREATURE,
                        of_chosen_type: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                on_spend_cant_be_countered: true,
                ..Default::default()
            });
        }

        if rem.len() >= 2 {
            let (trim_len, tail_zone) = if rem.len() >= 3
                && rem[rem.len() - 3].eq_ci("from")
                && (rem[rem.len() - 2].eq_ci("your") || rem[rem.len() - 2].eq_ci("a"))
                && rem[rem.len() - 1].eq_ci("graveyard")
            {
                (3, Some(Zone::Graveyard))
            } else if rem.len() >= 2
                && rem[rem.len() - 2].eq_ci("from")
                && rem[rem.len() - 1].eq_ci("exile")
            {
                (2, Some(Zone::Exile))
            } else if rem.len() >= 3
                && rem[rem.len() - 3].eq_ci("from")
                && rem[rem.len() - 2].eq_ci("your")
                && rem[rem.len() - 1].eq_ci("hand")
            {
                (3, Some(Zone::Hand))
            } else {
                (0, None)
            };
            if let Some(z) = tail_zone
                && let Some((mut pmf, used)) =
                    parse_plain_mana_filter(&rem[..rem.len() - trim_len], &mut sub_ctx)
                && used == rem.len() - trim_len
            {
                pmf.cast_from_zone = Some(z);
                return Some(ManaSpendingRestriction {
                    allow_spells: Some(AllowSpellsRestriction {
                        filter: Some(pmf),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
        if let Some((mut pmf, used)) = parse_plain_mana_filter(rem, &mut sub_ctx) {
            if used == rem.len() {
                return Some(ManaSpendingRestriction {
                    allow_spells: Some(AllowSpellsRestriction {
                        filter: Some(pmf),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            let after = &rem[used..];
            let mut zc = TokenCursor::new(after);
            let zone_from = if zc.eat_words(&["from", "your", "graveyard"])
                || zc.eat_words(&["from", "a", "graveyard"])
            {
                Some(Zone::Graveyard)
            } else if zc.eat_words(&["from", "exile"]) {
                Some(Zone::Exile)
            } else if zc.eat_words(&["from", "your", "hand"]) {
                Some(Zone::Hand)
            } else {
                None
            };
            if let Some(z) = zone_from
                && zc.is_empty()
            {
                pmf.cast_from_zone = Some(z);
                return Some(ManaSpendingRestriction {
                    allow_spells: Some(AllowSpellsRestriction {
                        filter: Some(pmf),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            let mut ac = TokenCursor::new(after);
            if (ac.eat_words(&["or", "activate", "an", "ability", "of"])
                || ac.eat_words(&["or", "to", "activate", "an", "ability", "of"])
                || ac.eat_words(&["or", "activate", "abilities", "of"])
                || ac.eat_words(&["or", "to", "activate", "abilities", "of"]))
                && {
                    let ab_rem = ac.remaining();
                    let ab_slice = if ab_rem
                        .last()
                        .is_some_and(|t| t.eq_ci("source") || t.eq_ci("sources"))
                        && ab_rem.len() > 1
                    {
                        &ab_rem[..ab_rem.len() - 1]
                    } else {
                        ab_rem
                    };
                    if let Some((ab_pmf, ab_used)) = parse_plain_mana_filter(ab_slice, &mut sub_ctx)
                        && ab_used == ab_slice.len()
                    {
                        return Some(ManaSpendingRestriction {
                            allow_spells: Some(AllowSpellsRestriction {
                                filter: Some(pmf),
                                ..Default::default()
                            }),
                            allow_abilities: Some(AllowAbilitiesRestriction {
                                any_ability: false,
                                source_filter: Some(ab_pmf),
                                foretell: false,
                            }),
                            ..Default::default()
                        });
                    }
                    false
                }
            {}
        }
    }

    let _ = object_filter_to_plain_mana_filter;
    sub_ctx.interner.rollback(cp);
    None
}
