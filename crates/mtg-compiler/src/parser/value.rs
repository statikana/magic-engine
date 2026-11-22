use mtg_ir::{
    Amount, AttachmentOwnerTarget, CardTypeMask, ColorMask, Compare, CompareOp, Condition,
    CostPaidCountKind, CountedThisWayKind, CounterTarget, LifeCompareWho, ManaColor,
    ManaSourceKind, NamedAltCostKind, ObjectColorTarget, ObjectFilter, OpponentComparisonMetric,
    OpponentMetricKind, PlayerCounterKind, PlayerCounterScope, PlayerFilter, PlayerRef,
    PlayerRelation, Ref, RelativeControl, RoundMode, SacrificedStatKind, SpellsCastWho,
    SpellsLastTurnMode, StatKind, TargetKind, TargetSpec, Zone, ZoneMask, ZoneScopeWhose,
};

use crate::intern::normalize_counter_name;
use crate::lexer::{ManaSymbolKind, Punct, Token, TokenCursor, TokenKind, tokenize};
use crate::parser::ParseCtx;
use crate::parser::filter::{
    CursorExt, FilterOpts, ParserScope, PronounSubject, parse_color_word, parse_mana_color_word,
    parse_object_filter, singular_subtype,
};

/// Parses an unsigned small number from digits or English number words (`zero`..`fifty`, `a`, `an`).
pub fn parse_small_number(raw: &str) -> Option<i32> {
    if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
        return raw.parse::<i32>().ok();
    }
    if raw.len() > 16 || !raw.is_ascii() {
        return None;
    }
    let mut buf = [0u8; 16];
    let len = raw.len();
    buf[..len].copy_from_slice(raw.as_bytes());
    buf[..len].make_ascii_lowercase();
    match &buf[..len] {
        b"zero" => Some(0),
        b"a" | b"an" | b"one" => Some(1),
        b"two" => Some(2),
        b"three" => Some(3),
        b"four" => Some(4),
        b"five" => Some(5),
        b"six" => Some(6),
        b"seven" => Some(7),
        b"eight" => Some(8),
        b"nine" => Some(9),
        b"ten" => Some(10),
        b"eleven" => Some(11),
        b"twelve" => Some(12),
        b"thirteen" => Some(13),
        b"fourteen" => Some(14),
        b"fifteen" => Some(15),
        b"sixteen" => Some(16),
        b"seventeen" => Some(17),
        b"eighteen" => Some(18),
        b"nineteen" => Some(19),
        b"twenty" => Some(20),
        b"thirty" => Some(30),
        b"forty" => Some(40),
        b"fifty" => Some(50),
        _ => None,
    }
}

/// Parses a count token (`N`, number word, `a`/`an`, or `X` -> `Amount::X`).
pub fn parse_count_word(raw: &str) -> Option<Amount> {
    if raw.eq_ignore_ascii_case("x") {
        return Some(Amount::X);
    }
    parse_small_number(raw).map(|n| Amount::Const { n })
}

pub fn parse_devotion_color_list(raw: &str) -> Option<ColorMask> {
    let tokens = tokenize(raw, 0);
    let mut mask = ColorMask::empty();
    let mut cur = TokenCursor::new(&tokens);
    while !cur.is_empty() {
        if cur.eat_ci("and") || cur.eat_ci("to") || cur.eat_punct(Punct::Comma) {
            continue;
        }
        let tok = cur.bump()?;
        let col = parse_color_word(tok.raw)?;
        if col == ColorMask::C {
            return None;
        }
        mask |= col;
    }
    if mask.is_empty() { None } else { Some(mask) }
}

/// Parses a [`Ref`] (all 7 variants).
pub fn parse_ref(tokens: &[Token<'_>], scope: &dyn ParserScope) -> Option<(Ref, usize)> {
    let mut cur = TokenCursor::new(tokens);
    if let Some(tok) = cur.peek()
        && matches!(tok.kind, TokenKind::SelfRef)
    {
        cur.bump();
        return Some((Ref::SelfObj, cur.pos()));
    }
    if cur.eat_ci("this")
        && let Some(noun) = cur.peek()
        && is_self_noun(noun.raw)
    {
        cur.bump();
        return Some((Ref::SelfObj, cur.pos()));
    }
    if (cur.eat_ci("equipped") || cur.eat_ci("enchanted"))
        && let Some(noun) = cur.peek()
        && (noun.eq_ci("creature")
            || noun.eq_ci("permanent")
            || noun.eq_ci("artifact")
            || noun.eq_ci("land")
            || noun.eq_ci("planeswalker")
            || noun.eq_ci("equipment")
            || noun.eq_ci("aura")
            || noun.eq_ci("vehicle"))
    {
        cur.bump();
        return Some((Ref::AttachedTo, cur.pos()));
    }
    if cur.eat_words(&["the", "exiled", "card"])
        && scope.in_trigger_context()
        && !scope.trigger_event_is_exile()
        && scope.it_ref().is_none()
    {
        return Some((Ref::LinkedExile, cur.pos()));
    }
    if cur.eat_ci("it") || cur.eat_ci("them") || cur.eat_ci("they") {
        let r = scope
            .it_ref()
            .unwrap_or(match scope.default_pronoun_subject() {
                PronounSubject::AttachedTo => Ref::AttachedTo,
                PronounSubject::SelfObj => Ref::SelfObj,
            });
        return Some((r, cur.pos()));
    }
    if let Some(it) = scope.it_ref()
        && cur
            .try_parse(|c| {
                if c.eat_ci("the")
                    && let Some(noun) = c.peek()
                    && is_self_noun(noun.raw)
                {
                    c.bump();
                    Some(it.clone())
                } else {
                    None
                }
            })
            .is_some()
    {
        return Some((it, cur.pos()));
    }
    if (cur.eat_ci("that") || cur.eat_ci("those"))
        && let Some(noun) = cur.peek()
        && is_self_noun(noun.raw)
    {
        cur.bump();
        let it_opt = scope.it_ref().filter(|r| {
            !(matches!(r, Ref::SelfObj | Ref::AttachedTo)
                && scope.in_trigger_context()
                && scope.trigger_matches_that_noun(noun.raw))
        });
        let r = it_opt.unwrap_or_else(|| {
            if scope.in_trigger_context()
                && (!scope.trigger_targets_self() || noun.eq_ci("spell") || noun.eq_ci("ability"))
            {
                Ref::EventObject
            } else {
                Ref::SelfObj
            }
        });
        return Some((r, cur.pos()));
    }
    if cur.eat_ci("target") {
        // Basic fallback for "target creature" when referencing target 0
        if let Some(noun) = cur.peek()
            && is_self_noun(noun.raw)
        {
            cur.bump();
            return Some((Ref::Target { index: 0 }, cur.pos()));
        }
    }
    None
}

pub(crate) fn is_self_noun(w: &str) -> bool {
    if w.len() > 16 || !w.is_ascii() {
        return false;
    }
    let mut buf = [0u8; 16];
    let len = w.len();
    buf[..len].copy_from_slice(w.as_bytes());
    buf[..len].make_ascii_lowercase();
    matches!(
        &buf[..len],
        b"creature"
            | b"permanent"
            | b"artifact"
            | b"enchantment"
            | b"land"
            | b"planeswalker"
            | b"battle"
            | b"card"
            | b"spell"
            | b"ability"
            | b"source"
            | b"object"
            | b"token"
            | b"equipment"
            | b"aura"
            | b"vehicle"
            | b"spacecraft"
            | b"planet"
            | b"saga"
            | b"class"
            | b"case"
    )
}

/// Parses a [`PlayerRef`] (all 11 variants).
pub fn parse_player_ref(
    tokens: &[Token<'_>],
    scope: &mut dyn ParserScope,
) -> Option<(PlayerRef, usize)> {
    let mut cur = TokenCursor::new(tokens);
    if cur.eat_ci("you") || cur.eat_ci("your") {
        return Some((PlayerRef::You, cur.pos()));
    }
    if cur.eat_words(&["the", "player", "to", "your", "right"]) {
        return Some((PlayerRef::PlayerToYourRight, cur.pos()));
    }
    if cur.eat_words(&["the", "chosen", "opponent"]) || cur.eat_words(&["chosen", "opponent"]) {
        return Some((PlayerRef::ChosenOpponent, cur.pos()));
    }
    if cur.eat_words(&["the", "player", "whose", "turn", "it", "is"])
        || cur.eat_words(&["the", "active", "player"])
        || cur.eat_words(&["active", "player"])
    {
        return Some((PlayerRef::ActivePlayer, cur.pos()));
    }
    if cur.eat_words(&["the", "defending", "player"]) || cur.eat_words(&["defending", "player"]) {
        return Some((PlayerRef::DefendingPlayer, cur.pos()));
    }
    if cur.eat_words(&[
        "its",
        "controller",
        "or",
        "the",
        "controller",
        "of",
        "that",
        "planeswalker",
    ]) || cur.eat_words(&[
        "that",
        "player",
        "or",
        "that",
        "planeswalker's",
        "controller",
    ]) {
        return Some((
            PlayerRef::ControllerOrControllerOfPlaneswalker { target_index: 0 },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["its", "controller"]) {
        let base = scope.it_ref().unwrap_or(Ref::SelfObj);
        return Some((PlayerRef::ControllerOf { of: Box::new(base) }, cur.pos()));
    }
    if cur.eat_words(&["its", "owner"]) {
        let base = scope.it_ref().unwrap_or(Ref::SelfObj);
        return Some((PlayerRef::OwnerOf { of: Box::new(base) }, cur.pos()));
    }
    if let Some((r, used)) = parse_ref(tokens, scope) {
        let mut c2 = TokenCursor::new(&tokens[used..]);
        if c2.eat_possessive_s() {
            if c2.eat_ci("controller") {
                return Some((PlayerRef::ControllerOf { of: Box::new(r) }, used + c2.pos()));
            }
            if c2.eat_ci("owner") {
                return Some((PlayerRef::OwnerOf { of: Box::new(r) }, used + c2.pos()));
            }
        }
    }
    if cur.eat_words(&["target", "player"]) || cur.eat_words(&["target", "opponent"]) {
        let rel = if tokens[cur.pos() - 1].eq_ci("opponent") {
            PlayerRelation::Opponent
        } else {
            PlayerRelation::Any
        };
        let idx = scope
            .alloc_target(TargetSpec {
                kind: TargetKind::Player,
                filter: ObjectFilter::default(),
                player: Some(PlayerFilter { relation: rel }),
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
            })
            .unwrap_or(0);
        return Some((PlayerRef::Target { index: idx }, cur.pos()));
    }
    if cur.eat_words(&["that", "player"])
        || cur.eat_words(&["that", "opponent"])
        || cur.eat_ci("they")
    {
        let pref = if let Some(PlayerRef::Target { index }) = scope.last_player_ref() {
            PlayerRef::Target { index }
        } else if scope.is_each_loop_player() {
            PlayerRef::EachLoopPlayer
        } else if scope.in_trigger_context() {
            PlayerRef::EventPlayer
        } else {
            scope.last_player_ref().unwrap_or(PlayerRef::EventPlayer)
        };
        return Some((pref, cur.pos()));
    }
    None
}

/// Parses a [`Compare`] operator + RHS [`Amount`].
pub fn parse_compare(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Compare, usize)> {
    let mut cur = TokenCursor::new(tokens);
    let _ = cur.eat_ci("is") || cur.eat_ci("are") || cur.eat_ci("was") || cur.eat_ci("were");
    let _ = cur.eat_words(&["equal", "to"]);

    // Prefix forms: "at least N", "at most N", "greater than N", "more than N", "less than N", "fewer than N"
    if cur.eat_words(&["at", "least"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Ge,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }
    if cur.eat_words(&["at", "most"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Le,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }
    if cur.eat_words(&["greater", "than", "or", "equal", "to"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Ge,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }
    if cur.eat_words(&["less", "than", "or", "equal", "to"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Le,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }
    if cur.eat_words(&["greater", "than"]) || cur.eat_words(&["more", "than"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Gt,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }
    if cur.eat_words(&["less", "than"]) || cur.eat_words(&["fewer", "than"]) {
        let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
        return Some((
            Compare {
                op: CompareOp::Lt,
                value: Box::new(amt),
            },
            cur.pos() + used,
        ));
    }

    // Postfix forms: "<amt> or greater", "<amt> or more", "<amt> or less", "<amt> or fewer", or "<amt>"
    let (amt, used) = parse_amount(&tokens[cur.pos()..], ctx, scope)?;
    let mut after = TokenCursor::new(&tokens[cur.pos() + used..]);
    let op = if after.eat_words(&["or", "greater"]) || after.eat_words(&["or", "more"]) {
        CompareOp::Ge
    } else if after.eat_words(&["or", "less"]) || after.eat_words(&["or", "fewer"]) {
        CompareOp::Le
    } else {
        CompareOp::Eq
    };
    Some((
        Compare {
            op,
            value: Box::new(amt),
        },
        cur.pos() + used + after.pos(),
    ))
}

/// Parses an [`Amount`] expression (all 109 variants).
pub fn parse_amount(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Amount, usize)> {
    if tokens.is_empty() {
        return None;
    }
    let (left, used) = parse_amount_term(tokens, ctx, scope)?;
    let mut cur = TokenCursor::new(&tokens[used..]);

    if let Some((right, r_used)) = cur.try_parse(|c| {
        if !c.eat_ci("plus") {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((right, r_used)) = parse_amount(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((right, r_used))
    }) {
        return Some((
            Amount::Sum {
                args: vec![left, right],
            },
            used + cur.pos() + r_used,
        ));
    }
    if let Some((right, r_used)) = cur.try_parse(|c| {
        if !c.eat_ci("minus") {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((right, r_used)) = parse_amount(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((right, r_used))
    }) {
        if let (
            Amount::CardsIn {
                zone: Zone::Hand,
                of: tp,
            },
            Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            },
        ) = (&left, &right)
        {
            return Some((
                Amount::HandSizeDifference {
                    target_player: Some(tp.clone()),
                    cap: None,
                },
                used + cur.pos() + r_used,
            ));
        }
        return Some((
            Amount::Difference {
                left: Box::new(left),
                right: Box::new(right),
            },
            used + cur.pos() + r_used,
        ));
    }
    // "<then> instead if <cond>" is handled by callers; return `left` without speculatively interning `<cond>`.
    Some((left, used))
}

fn parse_amount_term(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Amount, usize)> {
    let mut cur = TokenCursor::new(tokens);

    // "twice <amt>" / "two times <amt>"
    if let Some((inner, used)) = cur.try_parse(|c| {
        if !(c.eat_ci("twice") || c.eat_words(&["two", "times"])) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((inner, used)) = parse_amount_term(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((inner, used))
    }) {
        return Some((
            Amount::Mul {
                factor: 2,
                of: Box::new(inner),
            },
            cur.pos() + used,
        ));
    }

    // "half [of] <amt>, rounded up/down"
    if let Some((amt, total_used)) = cur.try_parse(|c| {
        if !c.eat_ci("half") {
            return None;
        }
        let _ = c.eat_ci("of");
        let cp = ctx.interner.checkpoint();
        let Some((inner, used)) = parse_amount_term(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        let mut tail = TokenCursor::new(&c.remaining()[used..]);
        let round = if tail
            .try_parse(|tc| {
                let _ = tc.eat_punct(Punct::Comma);
                tc.eat_words(&["rounded", "up"]).then_some(())
            })
            .is_some()
        {
            RoundMode::Up
        } else {
            let _ = tail.try_parse(|tc| {
                let _ = tc.eat_punct(Punct::Comma);
                tc.eat_words(&["rounded", "down"]).then_some(())
            });
            RoundMode::Down
        };
        Some((
            Amount::Half {
                of: Box::new(inner),
                round,
            },
            c.pos() + used + tail.pos(),
        ))
    }) {
        return Some((amt, total_used));
    }

    // "any number"
    if cur.eat_words(&["any", "number"]) {
        return Some((Amount::AnyNumber, cur.pos()));
    }

    // "the difference"
    if let Some((amt, total_used)) = cur.try_parse(|c| {
        if !c.eat_words(&["the", "difference"]) {
            return None;
        }
        if let Some((player, counter, target)) = scope.difference_is_player_counter() {
            return Some((
                Amount::PlayerCounterDifference {
                    player,
                    counter,
                    target,
                },
                c.pos(),
            ));
        }
        if let Some((target_player, cap)) = scope.difference_hand_size() {
            return Some((Amount::HandSizeDifference { target_player, cap }, c.pos()));
        }
        None
    }) {
        return Some((amt, total_used));
    }

    // "your devotion to <color(s)>"
    if let Some((mask, total_used)) = cur.try_parse(|c| {
        if !c.eat_words(&["your", "devotion", "to"]) {
            return None;
        }
        let mut mask = ColorMask::empty();
        while let Some(tok) = c.peek() {
            if tok.eq_ci("and") {
                if c.peek_n(1).and_then(|n| parse_color_word(n.raw)).is_some() {
                    c.bump();
                    continue;
                }
                break;
            }
            if let Some(col) = parse_color_word(tok.raw)
                && col != ColorMask::C
            {
                c.bump();
                mask |= col;
                continue;
            }
            break;
        }
        if mask.is_empty() {
            None
        } else {
            Some((mask, c.pos()))
        }
    }) {
        return Some((
            Amount::Devotion {
                colors: mask,
                player: Some(PlayerRef::You),
            },
            total_used,
        ));
    }

    // "your speed" / "<player>'s speed"
    if cur.eat_words(&["your", "speed"]) {
        return Some((
            Amount::PlayerSpeed {
                player: PlayerRef::You,
            },
            cur.pos(),
        ));
    }

    // "excess damage"
    if cur.eat_words(&["excess", "damage"]) {
        return Some((Amount::ExcessDamage, cur.pos()));
    }

    // "that much" / "that many" / "that amount [of]" / "the [amount of] life lost this way" / "the [amount of] damage dealt this way"
    if cur.eat_words(&["that", "much"])
        || cur.eat_words(&["that", "many"])
        || cur.eat_words(&["that", "amount"])
        || cur.eat_words(&["the", "life", "lost", "this", "way"])
        || cur.eat_words(&["the", "amount", "of", "life", "lost", "this", "way"])
        || cur.eat_words(&["the", "damage", "dealt", "this", "way"])
        || cur.eat_words(&["the", "amount", "of", "damage", "dealt", "this", "way"])
    {
        if cur.remaining().len() == 1 && cur.peek_ci("of") {
            cur.bump();
        }
        if scope.in_trigger_context() && !scope.has_prior_step() {
            return Some((Amount::EventAmount, cur.pos()));
        }
        return Some((Amount::LastEffectAmount, cur.pos()));
    }

    if let Some(amt) = cur.try_parse(|c| {
        let _ = c.eat_words(&["the", "amount", "of"]);
        if !c.eat_words(&["damage", "dealt", "to"]) {
            return None;
        }
        if !c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        {
            return None;
        }
        c.bump();
        if !c.eat_words(&["this", "turn", "by", "sources"]) {
            return None;
        }
        let player = if c.eat_words(&["they", "controlled"]) || c.eat_words(&["they", "control"]) {
            PlayerRef::EachLoopPlayer
        } else if c.eat_words(&["you", "controlled"]) || c.eat_words(&["you", "control"]) {
            PlayerRef::You
        } else {
            return None;
        };
        Some(Amount::DamageDealtToSelfByPlayerThisTurn { player })
    }) {
        return Some((amt, cur.pos()));
    }

    if cur.eat_words(&[
        "the",
        "total",
        "amount",
        "of",
        "life",
        "your",
        "opponents",
        "lost",
        "this",
        "turn",
    ]) || cur.eat_words(&[
        "total",
        "amount",
        "of",
        "life",
        "your",
        "opponents",
        "lost",
        "this",
        "turn",
    ]) || cur.eat_words(&[
        "the",
        "amount",
        "of",
        "life",
        "your",
        "opponents",
        "lost",
        "this",
        "turn",
    ]) || cur.eat_words(&[
        "amount",
        "of",
        "life",
        "your",
        "opponents",
        "lost",
        "this",
        "turn",
    ]) {
        return Some((Amount::OpponentsLifeLostThisTurn, cur.pos()));
    }

    // "the number of ..." / "for each ..."
    if cur.eat_words(&["the", "number", "of"]) {
        let cp = ctx.interner.checkpoint();
        if let Some((amt, used)) = parse_the_number_of(&tokens[cur.pos()..], ctx, scope) {
            return Some((amt, cur.pos() + used));
        }
        ctx.interner.rollback(cp);
        if let Some((amt, used)) = parse_player_counters(&tokens[cur.pos()..], scope) {
            return Some((amt, cur.pos() + used));
        }
        return None;
    }
    if cur.eat_words(&["for", "each"]) {
        let cp = ctx.interner.checkpoint();
        let tlen = scope.prior_targets_len();
        if let Some((amt, used)) = parse_this_way_metric(&tokens[cur.pos()..]) {
            return Some((amt, cur.pos() + used));
        }
        ctx.interner.rollback(cp);
        scope.truncate_targets(tlen);
        if let Some((amt, used)) = parse_cards_in_zone(&tokens[cur.pos()..], scope) {
            return Some((amt, cur.pos() + used));
        }
        ctx.interner.rollback(cp);
        scope.truncate_targets(tlen);
        return None;
    }

    // "the sacrificed (creature|permanent|...)'s (power|toughness|mana value)"
    if let Some((amt, sac_used)) = cur.try_parse(|c| {
        if !c.eat_words(&["the", "sacrificed"]) && !c.eat_ci("sacrificed") {
            return None;
        }
        let noun = c.peek()?.raw;
        let is_sac_noun = noun.eq_ignore_ascii_case("creature's")
            || noun.eq_ignore_ascii_case("permanent's")
            || noun.eq_ignore_ascii_case("artifact's")
            || noun.eq_ignore_ascii_case("enchantment's")
            || noun.eq_ignore_ascii_case("land's")
            || noun.eq_ignore_ascii_case("planeswalker's")
            || noun.eq_ignore_ascii_case("card's");
        if is_sac_noun {
            c.bump();
        } else {
            let is_base = noun.eq_ignore_ascii_case("creature")
                || noun.eq_ignore_ascii_case("permanent")
                || noun.eq_ignore_ascii_case("artifact")
                || noun.eq_ignore_ascii_case("enchantment")
                || noun.eq_ignore_ascii_case("land")
                || noun.eq_ignore_ascii_case("planeswalker")
                || noun.eq_ignore_ascii_case("card");
            if !is_base {
                return None;
            }
            c.bump();
            if !c.eat_ci("'s") {
                return None;
            }
        }
        let stat = if c.eat_ci("power") {
            mtg_ir::SacrificedStatKind::Power
        } else if c.eat_ci("toughness") {
            mtg_ir::SacrificedStatKind::Toughness
        } else if c.eat_words(&["mana", "value"]) || c.eat_words(&["converted", "mana", "cost"]) {
            mtg_ir::SacrificedStatKind::ManaValue
        } else {
            return None;
        };
        Some((Amount::SacrificedCostStat { stat }, c.pos()))
    }) {
        return Some((amt, sac_used));
    }

    // "the total (power|toughness|mana value) of/among ..."
    if let Some((amt, total_used)) = cur.try_parse(|c| {
        if !c.eat_words(&["the", "total"]) && !c.eat_ci("total") {
            return None;
        }
        let stat = if c.eat_ci("power") {
            StatKind::Power
        } else if c.eat_ci("toughness") {
            StatKind::Toughness
        } else if c.eat_words(&["mana", "value"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        if !c.eat_ci("of") && !c.eat_ci("among") {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((f, used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((
            Amount::TotalStatAmong {
                filter: Box::new(f),
                stat,
            },
            c.pos() + used,
        ))
    }) {
        return Some((amt, total_used));
    }

    // "the greatest/highest/least/lowest (power|toughness|mana value) among ..."
    if let Some((amt, total_used)) = cur.try_parse(|c| {
        let _ = c.eat_ci("the");
        let is_greatest = if c.eat_ci("greatest") || c.eat_ci("highest") {
            true
        } else if c.eat_ci("least") || c.eat_ci("lowest") {
            false
        } else {
            return None;
        };
        let stat = if c.eat_ci("power") {
            StatKind::Power
        } else if c.eat_ci("toughness") {
            StatKind::Toughness
        } else if c.eat_words(&["mana", "value"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        if !c.eat_ci("among") {
            return None;
        }
        let rem = c.remaining();
        let (slice, as_cast_you) = if is_greatest
            && rem.len() >= 8
            && rem[rem.len() - 7].eq_ci("you")
            && rem[rem.len() - 6].eq_ci("controlled")
            && rem[rem.len() - 5].eq_ci("as")
            && rem[rem.len() - 4].eq_ci("you")
            && rem[rem.len() - 3].eq_ci("cast")
            && rem[rem.len() - 2].eq_ci("this")
            && rem[rem.len() - 1].eq_ci("spell")
        {
            (&rem[..rem.len() - 7], Some(rem.len()))
        } else if is_greatest
            && rem.len() >= 7
            && rem[rem.len() - 6].eq_ci("you")
            && rem[rem.len() - 5].eq_ci("controlled")
            && rem[rem.len() - 4].eq_ci("as")
            && rem[rem.len() - 3].eq_ci("you")
            && rem[rem.len() - 2].eq_ci("cast")
            && matches!(rem[rem.len() - 1].kind, TokenKind::SelfRef)
        {
            (&rem[..rem.len() - 6], Some(rem.len()))
        } else {
            (rem, None)
        };
        let cp = ctx.interner.checkpoint();
        let Some((mut f, used)) = parse_object_filter(
            slice,
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        if let Some(full_len) = as_cast_you {
            if used != slice.len() {
                ctx.interner.rollback(cp);
                return None;
            }
            f.controller = Some(RelativeControl::You);
            return Some((
                Amount::GreatestStatAmongControlledAsCast {
                    stat,
                    filter: Box::new(f),
                },
                c.pos() + full_len,
            ));
        }
        let amt = if is_greatest {
            Amount::GreatestStatAmong {
                filter: Box::new(f),
                stat,
            }
        } else {
            Amount::LeastStatAmong {
                filter: Box::new(f),
                stat,
            }
        };
        Some((amt, c.pos() + used))
    }) {
        return Some((amt, total_used));
    }

    // "<ref>'s (power|toughness|mana value)" and "its/their (power|toughness|mana value)"
    let poss_ref = if let Some(t0) = tokens.first()
        && (t0.eq_ci("its") || t0.eq_ci("their"))
        && let Some(r) = scope.it_ref()
    {
        Some((r, 1))
    } else if let Some((r, used_ref)) = parse_ref(tokens, scope)
        && let Some(t_next) = tokens.get(used_ref)
        && (t_next.raw == "'s" || t_next.raw == "’s" || t_next.raw == "'")
    {
        Some((r, used_ref + 1))
    } else if !tokens.first().is_some_and(|t| t.eq_ci("target")) {
        (0..tokens.len().min(4)).find_map(|idx| {
            let tok = &tokens[idx];
            let base = tok
                .raw
                .strip_suffix("'s")
                .or_else(|| tok.raw.strip_suffix("’s"))
                .or_else(|| tok.raw.strip_suffix("'S"))
                .or_else(|| tok.raw.strip_suffix("’S"))?;
            let mut tmp: Vec<Token<'_>> = tokens[..=idx].to_vec();
            tmp[idx].raw = base;
            tmp[idx].kind = TokenKind::Word(base);
            let (r, used_ref) = parse_ref(&tmp, scope)?;
            (used_ref == tmp.len()).then_some((r, idx + 1))
        })
    } else {
        None
    };
    if let Some((r, after_poss)) = poss_ref {
        let mut c2 = TokenCursor::new(&tokens[after_poss..]);
        let stat = if c2.eat_ci("power") {
            Some(StatKind::Power)
        } else if c2.eat_ci("toughness") {
            Some(StatKind::Toughness)
        } else if c2.eat_words(&["mana", "value"]) {
            Some(StatKind::ManaValue)
        } else {
            None
        };
        if let Some(stat) = stat {
            let lki = scope.has_sac_step_or_trigger()
                || (!matches!(r, Ref::SelfObj)
                    && (scope.it_ref().is_some() || scope.in_trigger_context()));
            return Some((Amount::Stat { of: r, stat, lki }, after_poss + c2.pos()));
        }
    }

    // "the (power|toughness|mana value) of <ref>"
    if let Some(stat) = cur.try_parse(|c| {
        if !c.eat_ci("the") {
            return None;
        }
        let stat = if c.eat_ci("power") {
            StatKind::Power
        } else if c.eat_ci("toughness") {
            StatKind::Toughness
        } else if c.eat_words(&["mana", "value"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        if !c.eat_ci("of") {
            return None;
        }
        Some(stat)
    }) {
        let ref_res = parse_ref(&tokens[cur.pos()..], scope);
        let prefer_target_spec = tokens[cur.pos()..]
            .first()
            .is_some_and(|t| t.eq_ci("target"))
            && (scope.get_target(0).is_none()
                || ref_res
                    .as_ref()
                    .is_none_or(|(_, u)| cur.pos() + *u < tokens.len()));
        if prefer_target_spec {
            let cp = ctx.interner.checkpoint();
            if let Some((ts, used_t)) =
                crate::parser::filter::parse_target_spec(&tokens[cur.pos()..], ctx, scope)
                && let Some(tidx) = scope.alloc_target(ts)
            {
                return Some((
                    Amount::Stat {
                        of: Ref::Target { index: tidx },
                        stat,
                        lki: false,
                    },
                    cur.pos() + used_t,
                ));
            }
            ctx.interner.rollback(cp);
        }
        if let Some((r, used_ref)) = ref_res {
            return Some((
                Amount::Stat {
                    of: r,
                    stat,
                    lki: false,
                },
                cur.pos() + used_ref,
            ));
        }
        let cp = ctx.interner.checkpoint();
        if let Some((ts, used_t)) =
            crate::parser::filter::parse_target_spec(&tokens[cur.pos()..], ctx, scope)
            && let Some(tidx) = scope.alloc_target(ts)
        {
            return Some((
                Amount::Stat {
                    of: Ref::Target { index: tidx },
                    stat,
                    lki: false,
                },
                cur.pos() + used_t,
            ));
        }
        ctx.interner.rollback(cp);
        cur.restore(0);
    }

    // "<player>'s life total" / "your life total"
    if cur.eat_words(&["your", "life", "total"]) {
        return Some((Amount::Life { of: PlayerRef::You }, cur.pos()));
    }
    if let Some((p, used_p)) = parse_player_ref(tokens, scope) {
        let mut c2 = TokenCursor::new(&tokens[used_p..]);
        if c2.eat_possessive_s() && c2.eat_words(&["life", "total"]) {
            return Some((Amount::Life { of: p }, used_p + c2.pos()));
        }
    }

    // Atomic literals: X, {X}, Number, NumberWord, SignedNumber
    let tok = cur.peek()?;
    match tok.kind {
        TokenKind::X | TokenKind::ManaSymbol(ManaSymbolKind::Variable('X')) => {
            cur.bump();
            return Some((scope.defined_x().unwrap_or(Amount::X), cur.pos()));
        }
        TokenKind::Number(n) | TokenKind::NumberWord(n) => {
            cur.bump();
            return Some((Amount::Const { n: n as i32 }, cur.pos()));
        }
        TokenKind::SignedNumber(n) => {
            cur.bump();
            return Some((Amount::Const { n }, cur.pos()));
        }
        _ => {
            if let Some(n) = parse_small_number(tok.raw) {
                cur.bump();
                return Some((Amount::Const { n }, cur.pos()));
            }
        }
    }

    None
}

fn parse_player_counters(
    tokens: &[Token<'_>],
    scope: &mut dyn ParserScope,
) -> Option<(Amount, usize)> {
    let mut cur = TokenCursor::new(tokens);
    let kind = if cur.eat_ci("experience") {
        mtg_ir::PlayerCounterKind::Experience
    } else if cur.eat_ci("poison") {
        mtg_ir::PlayerCounterKind::Poison
    } else if cur.eat_ci("energy") {
        mtg_ir::PlayerCounterKind::Energy
    } else if cur.eat_ci("rad") {
        mtg_ir::PlayerCounterKind::Rad
    } else if cur.eat_ci("ticket") || cur.eat_ci("tickets") {
        mtg_ir::PlayerCounterKind::Ticket
    } else {
        return None;
    };
    if !(cur.eat_ci("counters") || cur.eat_ci("counter")) {
        return None;
    }
    let p_rem = cur.remaining();
    let (pref, p_used) = parse_player_ref(p_rem, scope)?;
    for _ in 0..p_used {
        cur.bump();
    }
    if !(cur.eat_ci("have") || cur.eat_ci("has")) {
        return None;
    }
    Some((
        Amount::PlayerCounters {
            counter: kind,
            player: pref,
            players: None,
        },
        cur.pos(),
    ))
}

fn parse_cards_in_zone(
    tokens: &[Token<'_>],
    scope: &mut dyn ParserScope,
) -> Option<(Amount, usize)> {
    let mut cur = TokenCursor::new(tokens);
    if !(cur.eat_ci("cards") || cur.eat_ci("card")) || !cur.eat_ci("in") {
        return None;
    }
    if cur.eat_words(&["your", "hand"]) {
        return Some((
            Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["your", "graveyard"]) {
        return Some((
            Amount::CardsIn {
                zone: Zone::Graveyard,
                of: PlayerRef::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["your", "library"]) {
        return Some((
            Amount::CardsIn {
                zone: Zone::Library,
                of: PlayerRef::You,
            },
            cur.pos(),
        ));
    }
    let rem = cur.remaining();
    let (player, after_p_pos) = if let Some((p, used)) = parse_player_ref(rem, scope) {
        let mut c2 = TokenCursor::new(&rem[used..]);
        if !c2.eat_possessive_s() {
            return None;
        }
        (p, cur.pos() + used + c2.pos())
    } else if rem.len() >= 3
        && let Some(poss_pos) = (0..rem.len().min(3)).find(|&i| {
            rem[i].raw.ends_with("'s")
                || rem[i].raw.ends_with("’s")
                || rem[i].raw.ends_with("'S")
                || rem[i].raw.ends_with("’S")
        })
    {
        let tok = &rem[poss_pos];
        let base = tok
            .raw
            .strip_suffix("'s")
            .or_else(|| tok.raw.strip_suffix("’s"))
            .or_else(|| tok.raw.strip_suffix("'S"))
            .or_else(|| tok.raw.strip_suffix("’S"))?;
        let mut tmp: Vec<Token<'_>> = rem[..=poss_pos].to_vec();
        tmp[poss_pos].raw = base;
        tmp[poss_pos].kind = TokenKind::Word(base);
        let (p, used) = parse_player_ref(&tmp, scope)?;
        if used != tmp.len() {
            return None;
        }
        (p, cur.pos() + poss_pos + 1)
    } else {
        return None;
    };
    let mut zcur = TokenCursor::new(&tokens[after_p_pos..]);
    let zone = if zcur.eat_ci("hand") {
        Zone::Hand
    } else if zcur.eat_ci("graveyard") {
        Zone::Graveyard
    } else if zcur.eat_ci("library") {
        Zone::Library
    } else {
        return None;
    };
    Some((
        Amount::CardsIn { zone, of: player },
        after_p_pos + zcur.pos(),
    ))
}

fn parse_the_number_of(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Amount, usize)> {
    let mut cur = TokenCursor::new(tokens);

    // Special turn/game history counts
    if cur.eat_words(&["opponents", "you", "have"]) || cur.eat_words(&["opponent", "you", "have"]) {
        return Some((Amount::OpponentCount, cur.pos()));
    }
    if let Some((amt, used)) = cur.try_parse(|c| {
        if !(c.eat_words(&[
            "your",
            "opponents",
            "who",
            "were",
            "dealt",
            "combat",
            "damage",
            "by",
        ]) || c.eat_words(&[
            "opponents",
            "who",
            "were",
            "dealt",
            "combat",
            "damage",
            "by",
        ])) {
            return None;
        }
        let has_self = c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && {
                c.bump();
                true
            }
            || c.eat_words(&["this", "creature"]);
        if !has_self || !c.eat_ci("or") {
            return None;
        }
        let _ = c.eat_ci("a") || c.eat_ci("an");
        let cp = ctx.interner.checkpoint();
        let tok = c.bump()?;
        let Some(sub_name) = singular_subtype(tok.raw) else {
            ctx.interner.rollback(cp);
            return None;
        };
        if !c.eat_words(&["this", "turn"]) {
            ctx.interner.rollback(cp);
            return None;
        }
        let subtype = ctx.interner.intern_subtype(&sub_name);
        Some((
            Amount::OpponentsDealtCombatDamageBySelfOrSubtypeThisTurn { subtype },
            c.pos(),
        ))
    }) {
        return Some((amt, used));
    }
    if cur.eat_words(&["cards", "you've", "discarded", "this", "turn"])
        || cur.eat_words(&["card", "you've", "discarded", "this", "turn"])
    {
        return Some((Amount::CardsDiscardedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["permanents", "you've", "sacrificed", "this", "turn"])
        || cur.eat_words(&["permanent", "you've", "sacrificed", "this", "turn"])
    {
        return Some((Amount::SacrificedPermsThisTurn, cur.pos()));
    }
    if let Some((amt, used)) = cur.try_parse(|c| {
        let stat = if c.eat_words(&["greatest", "power", "among"]) {
            StatKind::Power
        } else if c.eat_words(&["greatest", "toughness", "among"]) {
            StatKind::Toughness
        } else if c.eat_words(&["greatest", "mana", "value", "among"]) {
            StatKind::ManaValue
        } else {
            return None;
        };
        let rem = c.remaining();
        let pos = rem.windows(4).position(|w| {
            w[0].eq_ci("you") && w[1].eq_ci("controlled") && w[2].eq_ci("as") && w[3].eq_ci("you")
        })?;
        if pos == 0 {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((mut f, f_used)) = parse_object_filter(
            &rem[..pos],
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        if f_used != pos {
            ctx.interner.rollback(cp);
            return None;
        }
        for _ in 0..pos {
            c.bump();
        }
        if !c.eat_words(&["you", "controlled", "as", "you", "cast"]) {
            ctx.interner.rollback(cp);
            return None;
        }
        if !(c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && {
                c.bump();
                true
            }
            || c.eat_words(&["this", "spell"]))
        {
            ctx.interner.rollback(cp);
            return None;
        }
        f.zones = ZoneMask::BATTLEFIELD;
        if f.controller.is_none() {
            f.controller = Some(RelativeControl::You);
        }
        Some((
            Amount::GreatestStatAmongControlledAsCast {
                stat,
                filter: Box::new(f),
            },
            c.pos(),
        ))
    }) {
        return Some((amt, used));
    }
    if let Some((amt, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["opponents", "who", "control"]) {
            return None;
        }
        let _ = c.eat_ci("a") || c.eat_ci("an");
        let cp = ctx.interner.checkpoint();
        let Some((f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((
            Amount::OpponentsControllingFilterCount {
                filter: Box::new(f),
            },
            c.pos() + f_used,
        ))
    }) {
        return Some((amt, used));
    }
    if let Some((amt, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["opponents", "who", "control", "more"]) {
            return None;
        }
        let rem = c.remaining();
        let than_idx = rem
            .windows(2)
            .position(|w| w[0].eq_ci("than") && w[1].eq_ci("you"))?;
        if than_idx == 0 {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((f, f_used)) = parse_object_filter(
            &rem[..than_idx],
            ctx,
            FilterOpts {
                default_zone: ZoneMask::empty(),
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        if f_used != than_idx {
            ctx.interner.rollback(cp);
            return None;
        }
        Some((
            Amount::OpponentsControllingMoreCount {
                filter: Box::new(f),
            },
            c.pos() + than_idx + 2,
        ))
    }) {
        return Some((amt, used));
    }
    if let Some((amt, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["opponents", "who", "have"]) {
            return None;
        }
        let num_tok = c.bump()?;
        let n = parse_small_number(num_tok.raw)?;
        if !c.eat_words(&["or", "fewer", "cards", "in", "hand"]) {
            return None;
        }
        Some((Amount::OpponentsWithHandMaxCount { max: n as u16 }, c.pos()))
    }) {
        return Some((amt, used));
    }
    if cur.eat_words(&["colors", "in", "your", "commander's", "color", "identity"]) {
        return Some((Amount::CommanderColorIdentityCount, cur.pos()));
    }
    if cur.eat_words(&[
        "times",
        "you've",
        "cast",
        "your",
        "commander",
        "from",
        "the",
        "command",
        "zone",
        "this",
        "game",
    ]) || cur.eat_words(&["times", "you've", "cast", "your", "commander"])
    {
        return Some((Amount::CommanderCastCount, cur.pos()));
    }
    if cur.eat_words(&["basic", "land", "types", "among", "lands", "you", "control"]) {
        return Some((
            Amount::Domain {
                player: PlayerRef::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["creatures", "that", "died", "this", "turn"]) {
        return Some((
            Amount::CreaturesDiedThisTurn { controller: None },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["creatures", "attacking", "you"]) {
        return Some((
            Amount::AttackingCountToPlayer {
                player: PlayerRef::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["creatures", "attacking", "them"])
        || cur.eat_words(&["creatures", "attacking", "that", "player"])
    {
        let player = scope.last_player_ref().unwrap_or(PlayerRef::EachLoopPlayer);
        return Some((Amount::AttackingCountToPlayer { player }, cur.pos()));
    }
    if cur.eat_words(&["spells", "cast", "this", "turn"]) {
        return Some((
            Amount::SpellsCastThisTurnCount {
                filter: None,
                who: None,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["spells", "you've", "cast", "this", "turn"]) {
        return Some((
            Amount::SpellsCastThisTurnCount {
                filter: None,
                who: Some(SpellsCastWho::You),
            },
            cur.pos(),
        ));
    }

    // "card(s) in your hand" / "card(s) in <player>'s <zone>"
    if let Some(res) = parse_cards_in_zone(tokens, scope) {
        return Some(res);
    }

    // "card types among cards in your graveyard"
    if cur.eat_words(&["card", "types", "among", "cards", "in", "your", "graveyard"]) {
        return Some((
            Amount::DistinctCardTypesInZone {
                zone: Zone::Graveyard,
                whose: ZoneScopeWhose::You,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["card", "types", "among", "cards", "in", "all", "graveyards"])
        || cur.eat_words(&["card", "types", "among", "cards", "in", "graveyards"])
    {
        return Some((
            Amount::DistinctCardTypesInZone {
                zone: Zone::Graveyard,
                whose: ZoneScopeWhose::All,
            },
            cur.pos(),
        ));
    }

    // "colors among permanents you control"
    if cur.eat_words(&["colors", "among", "permanents", "you", "control"]) {
        return Some((
            Amount::ColorsAmongPermanents {
                filter: None,
                player: PlayerRef::You,
            },
            cur.pos(),
        ));
    }

    // "<counter> counters on <ref>" / "counters on <ref>"
    if let Some((ct, r)) = cur.try_parse(|c| {
        let cp = ctx.interner.checkpoint();
        let ct = if c.eat_ci("counters") {
            CounterTarget::Any
        } else {
            let mut parts: Vec<&str> = Vec::new();
            let mut found = false;
            while let Some(tok) = c.peek() {
                if tok.eq_ci("counter") || tok.eq_ci("counters") {
                    c.bump();
                    found = true;
                    break;
                }
                if parts.len() >= 2 {
                    return None;
                }
                parts.push(tok.raw);
                c.bump();
            }
            if !found || parts.is_empty() {
                return None;
            }
            let norm = normalize_counter_name(&parts.join(" "))?;
            CounterTarget::Specific(ctx.interner.intern_counter(&norm))
        };
        if !c.eat_ci("on") {
            ctx.interner.rollback(cp);
            return None;
        }
        let Some((r, used)) = parse_ref(c.remaining(), scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..used {
            c.bump();
        }
        Some((ct, r))
    }) {
        if scope.in_trigger_context()
            && !scope.has_prior_step()
            && ((r == Ref::EventObject && !scope.trigger_targets_self())
                || (r == Ref::SelfObj && scope.trigger_is_ltb()))
        {
            return Some((Amount::LkiCounterCount { counter: ct }, cur.pos()));
        }
        return Some((Amount::CountersOn { counter: ct, of: r }, cur.pos()));
    }

    if let Some(res) = parse_player_counters(tokens, scope) {
        return Some(res);
    }

    if let Some(res) = parse_this_way_metric(tokens) {
        return Some(res);
    }

    // Fallback: object filter -> Amount::Count
    if let Some((mut f, used)) = parse_object_filter(
        tokens,
        ctx,
        FilterOpts {
            default_zone: ZoneMask::BATTLEFIELD,
            allow_plural_and_disjunction: true,
            ..Default::default()
        },
        scope,
    ) {
        let mut tail_cur = TokenCursor::new(&tokens[used..]);
        let rel = if tail_cur.eat_words(&["target", "opponent", "controls"]) {
            Some(PlayerRelation::Opponent)
        } else if tail_cur.eat_words(&["target", "player", "controls"]) {
            Some(PlayerRelation::Any)
        } else {
            None
        };
        if let Some(rel) = rel
            && let Some(idx) = scope.alloc_target(TargetSpec {
                kind: TargetKind::Player,
                filter: ObjectFilter::default(),
                player: Some(PlayerFilter { relation: rel }),
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
            })
        {
            let pref = PlayerRef::Target { index: idx };
            scope.record_last_player(pref.clone());
            f.extra
                .push(mtg_ir::ObjectPredicate::ControllerRef { player: pref });
            return Some((Amount::Count { of: Box::new(f) }, used + tail_cur.pos()));
        }
        return Some((Amount::Count { of: Box::new(f) }, used));
    }

    None
}

fn parse_this_way_metric(tokens: &[Token<'_>]) -> Option<(Amount, usize)> {
    let mut cur = TokenCursor::new(tokens);
    // "<counter> counter(s) removed this way" / "counters removed this way"
    if cur.eat_words(&["counters", "removed", "this", "way"])
        || cur.eat_words(&["counter", "removed", "this", "way"])
    {
        return Some((Amount::CountersRemovedThisWay, cur.pos()));
    }
    if cur
        .try_parse(|c| {
            let mut saw_word = false;
            while let Some(tok) = c.peek() {
                if tok.eq_ci("counter") || tok.eq_ci("counters") {
                    c.bump();
                    if saw_word && c.eat_words(&["removed", "this", "way"]) {
                        return Some(());
                    }
                    return None;
                }
                if matches!(tok.kind, crate::lexer::TokenKind::Word(_)) && !saw_word {
                    saw_word = true;
                    c.bump();
                } else {
                    return None;
                }
            }
            None
        })
        .is_some()
    {
        return Some((Amount::CountersRemovedThisWay, cur.pos()));
    }

    // "opponent(s) who drew a card this way" / "<noun> <verb>ed this way"
    if cur.eat_words(&["opponent", "who", "drew", "a", "card", "this", "way"])
        || cur.eat_words(&["opponents", "who", "drew", "a", "card", "this", "way"])
    {
        return Some((
            Amount::CountedThisWay {
                kind: CountedThisWayKind::OpponentsDrewThisWay,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&[
        "creatures",
        "they",
        "controlled",
        "that",
        "were",
        "destroyed",
        "this",
        "way",
    ]) || cur.eat_words(&[
        "creature",
        "they",
        "controlled",
        "that",
        "was",
        "destroyed",
        "this",
        "way",
    ]) || cur.eat_words(&[
        "permanents",
        "they",
        "controlled",
        "that",
        "were",
        "destroyed",
        "this",
        "way",
    ]) {
        return Some((
            Amount::CountedThisWay {
                kind: CountedThisWayKind::DestroyedControlledByPlayerThisWay,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["cards", "you've", "discarded", "this", "turn"])
        || cur.eat_words(&["card", "you've", "discarded", "this", "turn"])
    {
        return Some((Amount::CardsDiscardedThisTurn, cur.pos()));
    }
    if cur.eat_words(&[
        "land",
        "card",
        "put",
        "into",
        "a",
        "graveyard",
        "this",
        "way",
    ]) || cur.eat_words(&[
        "land",
        "cards",
        "put",
        "into",
        "a",
        "graveyard",
        "this",
        "way",
    ]) {
        return Some((
            Amount::CountedThisWay {
                kind: CountedThisWayKind::LandCardsPutIntoGraveyardThisWay,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["nonland", "card", "revealed", "this", "way"])
        || cur.eat_words(&["nonland", "cards", "revealed", "this", "way"])
    {
        return Some((
            Amount::CountedThisWay {
                kind: CountedThisWayKind::NonlandRevealedThisWay,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["creature", "dealt", "damage", "this", "way"])
        || cur.eat_words(&["creatures", "dealt", "damage", "this", "way"])
    {
        return Some((
            Amount::CountedThisWay {
                kind: CountedThisWayKind::CreaturesDealtDamageThisWay,
            },
            cur.pos(),
        ));
    }
    if let Some(kind) = cur.try_parse(|c| {
        let noun = c.bump()?.raw.to_ascii_lowercase();
        let is_obj_noun = matches!(
            noun.as_str(),
            "card"
                | "cards"
                | "creature"
                | "creatures"
                | "permanent"
                | "permanents"
                | "artifact"
                | "artifacts"
                | "enchantment"
                | "enchantments"
                | "land"
                | "lands"
                | "planeswalker"
                | "planeswalkers"
                | "spell"
                | "spells"
        );
        if !is_obj_noun {
            return None;
        }
        if c.eat_words(&["destroyed", "this", "way"]) {
            Some(CountedThisWayKind::DestroyedThisWay)
        } else if c.eat_words(&["exiled", "this", "way"]) {
            Some(CountedThisWayKind::ExiledThisWay)
        } else if c.eat_words(&["discarded", "this", "way"]) {
            Some(CountedThisWayKind::DiscardedThisWay)
        } else if c.eat_words(&["drawn", "this", "way"]) {
            Some(CountedThisWayKind::DrawnThisWay)
        } else if c.eat_words(&["sacrificed", "this", "way"]) {
            Some(CountedThisWayKind::SacrificedThisWay)
        } else if c.eat_words(&["milled", "this", "way"]) {
            Some(CountedThisWayKind::MilledThisWay)
        } else if c.eat_words(&["revealed", "this", "way"]) {
            Some(CountedThisWayKind::RevealedThisWay)
        } else if c.eat_words(&["tapped", "this", "way"]) {
            Some(CountedThisWayKind::TappedThisWay)
        } else if c.eat_words(&["returned", "to", "your", "hand", "this", "way"])
            || c.eat_words(&["returned", "to", "its", "owner's", "hand", "this", "way"])
            || c.eat_words(&["returned", "to", "their", "owners'", "hands", "this", "way"])
        {
            Some(CountedThisWayKind::ReturnedToHandThisWay)
        } else {
            None
        }
    }) {
        return Some((Amount::CountedThisWay { kind }, cur.pos()));
    }

    if let Some(res) = cur.try_parse(|c| {
        if !c.eat_words(&["opponent", "who", "has"]) && !c.eat_words(&["opponents", "who", "have"])
        {
            return None;
        }
        let num_tok = c.bump()?;
        let n = parse_small_number(num_tok.raw)?;
        if !c.eat_words(&["or", "fewer", "cards", "in", "hand"]) {
            return None;
        }
        Some((Amount::OpponentsWithHandMaxCount { max: n as u16 }, c.pos()))
    }) {
        return Some(res);
    }
    if let Some(res) = cur.try_parse(|c| {
        c.eat_words(&["coins", "that", "came", "up", "heads"])
            .then(|| (Amount::CoinHeadsCount, c.pos()))
    }) {
        return Some(res);
    }

    None
}

/// Parses a [`Condition`] (covering compound boolean combinators + leaf conditions + `Compare`).
pub fn parse_condition(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Condition, usize)> {
    parse_condition_depth(tokens, ctx, scope, 0)
}

fn parse_condition_depth(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
    depth: u8,
) -> Option<(Condition, usize)> {
    let mut cur = TokenCursor::new(tokens);
    let _ = cur.eat_ci("if") || cur.eat_ci("while") || cur.eat_words(&["as", "long", "as"]);
    let prefix_len = cur.pos();
    let body = &tokens[prefix_len..];
    if body.is_empty() {
        return None;
    }

    if depth < 2 {
        // Check compound `or` / `and` at clause level
        let cp = ctx.interner.checkpoint();
        let tlen = scope.prior_targets_len();
        if let Some((compound, used)) = try_parse_compound_condition(body, ctx, scope, depth) {
            return Some((compound, prefix_len + used));
        }
        scope.truncate_targets(tlen);
        ctx.interner.rollback(cp);
    }

    let cp = ctx.interner.checkpoint();
    let tlen = scope.prior_targets_len();
    let Some((leaf, used)) = parse_leaf_condition(body, ctx, scope) else {
        scope.truncate_targets(tlen);
        ctx.interner.rollback(cp);
        return None;
    };
    Some((leaf, prefix_len + used))
}

fn is_condition_subject_starter(tok: &Token<'_>) -> bool {
    tok.eq_ci("you")
        || tok.eq_ci("an")
        || tok.eq_ci("a")
        || tok.eq_ci("there")
        || tok.eq_ci("it")
        || tok.eq_ci("it's")
        || tok.eq_ci("this")
        || tok.eq_ci("enchanted")
        || tok.eq_ci("equipped")
        || tok.eq_ci("that")
        || tok.eq_ci("no")
        || tok.eq_ci("your")
        || tok.eq_ci("each")
        || tok.eq_ci("both")
        || tok.eq_ci("at")
        || matches!(tok.kind, TokenKind::SelfRef | TokenKind::X)
}

fn try_parse_compound_condition(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
    depth: u8,
) -> Option<(Condition, usize)> {
    // Check "it's neither day nor night"
    {
        let mut cur = TokenCursor::new(tokens);
        if cur.eat_words(&["it's", "neither", "day", "nor", "night"]) {
            return Some((
                Condition::And {
                    of: vec![
                        Condition::Not {
                            of: Box::new(Condition::IsDay),
                        },
                        Condition::Not {
                            of: Box::new(Condition::IsNight),
                        },
                    ],
                },
                cur.pos(),
            ));
        }
    }

    // Scan for "or [if]" or "and [if]" where RHS starts with a valid subject
    for i in 1..tokens.len().saturating_sub(1) {
        let tok = &tokens[i];
        let is_or = tok.eq_ci("or");
        let is_and = tok.eq_ci("and");
        if !is_or && !is_and {
            continue;
        }
        let rhs_start = if tokens.get(i + 1).is_some_and(|n| n.eq_ci("if")) {
            i + 2
        } else {
            i + 1
        };
        let Some(rhs_first) = tokens.get(rhs_start) else {
            continue;
        };
        if !is_condition_subject_starter(rhs_first) {
            continue;
        }
        let lhs_tokens = &tokens[..i];
        let rhs_tokens = &tokens[rhs_start..];
        let cp = ctx.interner.checkpoint();
        let Some((lhs, lhs_used)) = parse_leaf_condition(lhs_tokens, ctx, scope) else {
            ctx.interner.rollback(cp);
            continue;
        };
        if lhs_used != lhs_tokens.len() {
            ctx.interner.rollback(cp);
            continue;
        }
        let Some((rhs, rhs_used)) = parse_condition_depth(rhs_tokens, ctx, scope, depth + 1) else {
            ctx.interner.rollback(cp);
            continue;
        };
        let combined = if is_or {
            Condition::Or { of: vec![lhs, rhs] }
        } else {
            Condition::And { of: vec![lhs, rhs] }
        };
        return Some((combined, rhs_start + rhs_used));
    }
    None
}

fn parse_object_state_word_cond(of: AttachmentOwnerTarget, word: &str) -> Option<Condition> {
    if word.eq_ignore_ascii_case("tapped") {
        Some(Condition::ObjectState {
            of,
            tapped: Some(true),
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: None,
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("untapped") {
        Some(Condition::ObjectState {
            of,
            tapped: Some(false),
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: None,
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("attacking") {
        Some(Condition::ObjectState {
            of,
            tapped: None,
            attacking: Some(true),
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: None,
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("blocking") {
        Some(Condition::ObjectState {
            of,
            tapped: None,
            attacking: None,
            blocking: Some(true),
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: None,
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("equipped") {
        Some(Condition::ObjectState {
            of,
            tapped: None,
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: Some(true),
            enchanted: None,
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("enchanted") {
        Some(Condition::ObjectState {
            of,
            tapped: None,
            attacking: None,
            blocking: None,
            monstrous: None,
            saddled: None,
            equipped: None,
            enchanted: Some(true),
            filter: None,
        })
    } else if word.eq_ignore_ascii_case("monstrous") {
        if of == AttachmentOwnerTarget::SelfObj {
            Some(Condition::Monstrous)
        } else {
            Some(Condition::ObjectState {
                of,
                tapped: None,
                attacking: None,
                blocking: None,
                monstrous: Some(true),
                saddled: None,
                equipped: None,
                enchanted: None,
                filter: None,
            })
        }
    } else if word.eq_ignore_ascii_case("saddled") {
        if of == AttachmentOwnerTarget::SelfObj {
            Some(Condition::Saddled)
        } else {
            Some(Condition::ObjectState {
                of,
                tapped: None,
                attacking: None,
                blocking: None,
                monstrous: None,
                saddled: Some(true),
                equipped: None,
                enchanted: None,
                filter: None,
            })
        }
    } else {
        None
    }
}

fn parse_leaf_condition(
    tokens: &[Token<'_>],
    ctx: &mut ParseCtx<'_>,
    scope: &mut dyn ParserScope,
) -> Option<(Condition, usize)> {
    let mut cur = TokenCursor::new(tokens);

    // 1. Turn / Phase / Global State
    if cur.eat_words(&["it's", "your", "turn"]) || cur.eat_words(&["during", "your", "turn"]) {
        return Some((Condition::IsYourTurn, cur.pos()));
    }
    if cur.eat_words(&["it's", "not", "your", "turn"])
        || cur.eat_words(&["during", "turns", "other", "than", "yours"])
    {
        return Some((Condition::NotYourTurn, cur.pos()));
    }
    if cur.eat_words(&["it's", "your", "main", "phase"]) {
        return Some((Condition::IsYourMainPhase, cur.pos()));
    }
    if cur.eat_words(&["it's", "day"]) {
        return Some((Condition::IsDay, cur.pos()));
    }
    if cur.eat_words(&["it's", "night"]) {
        return Some((Condition::IsNight, cur.pos()));
    }
    if cur.eat_words(&["you", "have", "the", "city's", "blessing"]) {
        return Some((Condition::HasCitysBlessing, cur.pos()));
    }
    if cur.eat_words(&["you", "have", "the", "initiative"]) {
        return Some((Condition::HasInitiative, cur.pos()));
    }
    if cur.eat_words(&["you've", "completed", "a", "dungeon"]) {
        return Some((Condition::HasCompletedDungeon, cur.pos()));
    }
    if cur.eat_words(&["you", "are", "the", "monarch"])
        || cur.eat_words(&["you're", "the", "monarch"])
    {
        return Some((
            Condition::IsMonarch {
                player: Some(PlayerRef::You),
                relation: None,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["you", "have", "max", "speed"]) {
        return Some((Condition::HasMaxSpeed, cur.pos()));
    }
    if cur.eat_words(&["this", "case", "is", "solved"]) {
        return Some((Condition::CaseSolved, cur.pos()));
    }
    if cur.eat_words(&["this", "case", "is", "not", "solved"]) {
        return Some((Condition::CaseUnsolved, cur.pos()));
    }

    // 1b. Opponent Comparison & Opponent Metric / Control
    if cur.eat_words(&["an", "opponent", "controls", "more", "lands", "than", "you"]) {
        return Some((
            Condition::OpponentComparison {
                metric: OpponentComparisonMetric::Lands,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&[
        "an",
        "opponent",
        "controls",
        "more",
        "creatures",
        "than",
        "you",
    ]) {
        return Some((
            Condition::OpponentComparison {
                metric: OpponentComparisonMetric::Creatures,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["an", "opponent", "has", "more", "life", "than", "you"]) {
        return Some((
            Condition::OpponentComparison {
                metric: OpponentComparisonMetric::Life,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&[
        "an", "opponent", "has", "more", "cards", "in", "hand", "than", "you",
    ]) {
        return Some((
            Condition::OpponentComparison {
                metric: OpponentComparisonMetric::Hand,
            },
            cur.pos(),
        ));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["an", "opponent", "controls"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        if let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope)
            && !matches!(
                cmp,
                Compare {
                    op: CompareOp::Eq,
                    ref value
                } if matches!(**value, Amount::Const { n: 1 })
            )
        {
            for _ in 0..c_used {
                c.bump();
            }
            let Some((mut f, f_used)) = parse_object_filter(
                c.remaining(),
                ctx,
                FilterOpts {
                    default_zone: ZoneMask::empty(),
                    allow_plural_and_disjunction: true,
                    ..Default::default()
                },
                scope,
            ) else {
                ctx.interner.rollback(cp);
                return None;
            };
            for _ in 0..f_used {
                c.bump();
            }
            if f.controller.is_none() {
                f.controller = Some(RelativeControl::Opponent);
            }
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: cmp.op,
                    right: cmp.value,
                },
                c.pos(),
            ));
        }
        ctx.interner.rollback(cp);
        let _ = c.eat_ci("a") || c.eat_ci("an");
        let Some((mut f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..f_used {
            c.bump();
        }
        if f.controller.is_none() {
            f.controller = Some(RelativeControl::Opponent);
        }
        Some((
            Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: Box::new(Amount::Const { n: 1 }),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }
    if cur.eat_words(&["an", "opponent", "is", "poisoned"]) {
        return Some((Condition::OpponentPoisonAtLeast { min: 1 }, cur.pos()));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["an", "opponent", "has"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        let Amount::Const { n } = *cmp.value else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..c_used {
            c.bump();
        }
        if (cmp.op == CompareOp::Ge || (cmp.op == CompareOp::Eq && n == 1))
            && n >= 1
            && (c.eat_words(&["poison", "counters"]) || c.eat_words(&["poison", "counter"]))
        {
            let Ok(min) = u16::try_from(n) else {
                ctx.interner.rollback(cp);
                return None;
            };
            return Some((Condition::OpponentPoisonAtLeast { min }, c.pos()));
        }
        let metric = if c.eat_words(&["cards", "in", "their", "graveyard"]) {
            OpponentMetricKind::Graveyard
        } else if c.eat_words(&["cards", "in", "their", "hand"])
            || c.eat_words(&["cards", "in", "hand"])
        {
            OpponentMetricKind::Hand
        } else if c.eat_ci("life") {
            OpponentMetricKind::Life
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((
            Condition::AnyOpponentMetric {
                metric,
                op: cmp.op,
                count: n,
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }

    // 2. Self / Object Cast Mode & State
    if cur.eat_words(&["it", "was", "kicked"])
        || cur.eat_words(&["~", "was", "kicked"])
        || cur.eat_words(&["this", "spell", "was", "kicked"])
    {
        return Some((
            Condition::Kicked {
                tag: None,
                min_count: None,
            },
            cur.pos(),
        ));
    }
    if cur.eat_words(&["this", "spell", "was", "bargained"])
        || cur.eat_words(&["it", "was", "bargained"])
    {
        return Some((Condition::SpellWasBargained, cur.pos()));
    }
    if cur.eat_words(&["it", "was", "evoked"]) {
        return Some((Condition::Evoked, cur.pos()));
    }
    if cur.eat_words(&["it", "escaped"]) {
        return Some((Condition::Escaped, cur.pos()));
    }
    if cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        && cur.peek_n(1).is_some_and(|t| t.eq_ci("escaped"))
    {
        cur.bump();
        cur.bump();
        return Some((Condition::Escaped, cur.pos()));
    }
    if cur.eat_words(&["it", "was", "foretold"]) {
        return Some((Condition::Foretold, cur.pos()));
    }
    if cur
        .peek()
        .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        && cur.peek_n(1).is_some_and(|t| t.eq_ci("was"))
        && cur.peek_n(2).is_some_and(|t| t.eq_ci("foretold"))
    {
        cur.bump();
        cur.bump();
        cur.bump();
        return Some((Condition::Foretold, cur.pos()));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if c.eat_ci("its") {
            // consumed possessive
        } else if c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        {
            c.bump();
            if !c.eat_possessive_s() {
                return None;
            }
        } else if c
            .peek()
            .is_some_and(|t| t.raw.eq_ignore_ascii_case("~'s") || t.raw.eq_ignore_ascii_case("~’s"))
        {
            c.bump();
        } else {
            return None;
        }
        if c.eat_words(&["additional", "cost", "was", "paid"]) {
            return Some((Condition::AdditionalCostPaid { mana_cost: None }, c.pos()));
        }
        let alt_kind = if c.eat_ci("prowl") {
            Some(NamedAltCostKind::Prowl)
        } else if c.eat_ci("surge") {
            Some(NamedAltCostKind::Surge)
        } else if c.eat_ci("spectacle") {
            Some(NamedAltCostKind::Spectacle)
        } else {
            None
        }?;
        if c.eat_words(&["cost", "was", "paid"]) {
            return Some((Condition::PaidNamedAltCost { kind: alt_kind }, c.pos()));
        }
        None
    }) {
        return Some((cond, used));
    }
    if cur.eat_words(&["its", "blitz", "cost", "was", "paid"]) {
        return Some((Condition::Blitzed, cur.pos()));
    }
    if cur.eat_words(&["it's", "saddled"]) {
        return Some((Condition::Saddled, cur.pos()));
    }
    if cur.eat_words(&["it's", "monstrous"]) {
        return Some((Condition::Monstrous, cur.pos()));
    }
    if cur.eat_words(&["it's", "renowned"]) {
        return Some((Condition::IsRenowned, cur.pos()));
    }
    if cur.eat_words(&["the", "gift", "was", "promised"]) {
        return Some((Condition::GiftPromised, cur.pos()));
    }
    if cur.eat_words(&[
        "this", "is", "the", "second", "time", "this", "ability", "has", "resolved", "this", "turn",
    ]) {
        return Some((
            Condition::AbilityActivatedCountThisTurn { min: 2 },
            cur.pos(),
        ));
    }
    if cur.eat_words(&[
        "this", "is", "the", "third", "time", "this", "ability", "has", "resolved", "this", "turn",
    ]) {
        return Some((
            Condition::AbilityActivatedCountThisTurn { min: 3 },
            cur.pos(),
        ));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["at", "least"]) {
            return None;
        }
        let n_tok = c.bump()?;
        let n_i32 = parse_small_number(n_tok.raw)?;
        let min = u8::try_from(n_i32).ok()?;
        let color = if c.eat_ci("colorless") {
            Some(mtg_ir::ManaColor::C)
        } else if c.eat_ci("white") {
            Some(mtg_ir::ManaColor::W)
        } else if c.eat_ci("blue") {
            Some(mtg_ir::ManaColor::U)
        } else if c.eat_ci("black") {
            Some(mtg_ir::ManaColor::B)
        } else if c.eat_ci("red") {
            Some(mtg_ir::ManaColor::R)
        } else if c.eat_ci("green") {
            Some(mtg_ir::ManaColor::G)
        } else {
            None
        }?;
        if !c.eat_words(&["mana", "was", "spent", "to", "cast"]) {
            return None;
        }
        if !(c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && {
                c.bump();
                true
            }
            || c.eat_words(&["this", "spell"])
            || c.eat_ci("it"))
        {
            return None;
        }
        Some((
            Condition::SameColorManaSpentAtLeast {
                min: min.into(),
                color: Some(color),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        let tok = c.bump()?;
        let TokenKind::ManaSymbol(ManaSymbolKind::Colored(color)) = tok.kind else {
            return None;
        };
        if !c.eat_words(&["was", "spent", "to", "cast"]) {
            return None;
        }
        if !(c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && {
                c.bump();
                true
            }
            || c.eat_words(&["this", "spell"]))
        {
            return None;
        }
        Some((
            Condition::SameColorManaSpentAtLeast {
                min: 1,
                color: Some(color),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !(c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            && {
                c.bump();
                true
            }
            || c.eat_ci("it"))
        {
            return None;
        }
        if !c.eat_words(&["is", "your"]) {
            return None;
        }
        if !(c.eat_words(&["ring-bearer"]) || c.eat_words(&["ring", "-", "bearer"])) {
            return None;
        }
        if !c.eat_words(&["and", "the", "ring", "has", "tempted", "you"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let (cmp, c_used) = parse_compare(c.remaining(), ctx, scope)?;
        if cmp.op != CompareOp::Ge {
            ctx.interner.rollback(cp);
            return None;
        }
        let Amount::Const { n } = *cmp.value else {
            ctx.interner.rollback(cp);
            return None;
        };
        let Ok(min) = u8::try_from(n) else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..c_used {
            c.bump();
        }
        if !(c.eat_words(&["times", "this", "game"]) || c.eat_words(&["time", "this", "game"])) {
            ctx.interner.rollback(cp);
            return None;
        }
        Some((Condition::RingBearerAndTemptedAtLeast { min }, c.pos()))
    }) {
        return Some((cond, used));
    }
    if cur.eat_words(&[
        "who", "didn't", "discard", "a", "nonland", "card", "this", "way",
    ]) || cur.eat_words(&["didn't", "discard", "a", "nonland", "card", "this", "way"])
        || cur.eat_words(&[
            "that", "player", "didn't", "discard", "a", "nonland", "card", "this", "way",
        ])
        || cur.eat_words(&[
            "they", "didn't", "discard", "a", "nonland", "card", "this", "way",
        ])
    {
        return Some((Condition::PlayerDidntDiscardNonlandThisWay, cur.pos()));
    }
    // "~ has a +1/+1 counter on it" / "this creature has N or more <counter> counters on it"
    if let Some((cond, used)) = cur.try_parse(|c| {
        let mut is_pronoun = false;
        if c.peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        {
            c.bump();
        } else if c.eat_ci("it") {
            is_pronoun = true;
        } else if c.eat_ci("this") && c.peek().is_some_and(|t| is_self_noun(t.raw)) {
            c.bump();
        } else {
            return None;
        }
        if !c.eat_ci("has") {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let (op, val) = if c.eat_ci("no") {
            (CompareOp::Eq, Amount::Const { n: 0 })
        } else if c.eat_ci("a") || c.eat_ci("an") {
            (CompareOp::Ge, Amount::Const { n: 1 })
        } else if let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope) {
            for _ in 0..c_used {
                c.bump();
            }
            (cmp.op, *cmp.value)
        } else {
            return None;
        };
        let ct = if c.eat_ci("counters") || c.eat_ci("counter") {
            CounterTarget::Any
        } else {
            let mut parts: Vec<&str> = Vec::new();
            let mut found = false;
            while let Some(tok) = c.peek() {
                if tok.eq_ci("counter") || tok.eq_ci("counters") {
                    c.bump();
                    found = true;
                    break;
                }
                if parts.len() >= 2 {
                    ctx.interner.rollback(cp);
                    return None;
                }
                parts.push(tok.raw);
                c.bump();
            }
            if !found || parts.is_empty() {
                ctx.interner.rollback(cp);
                return None;
            }
            let Some(norm) = normalize_counter_name(&parts.join(" ")) else {
                ctx.interner.rollback(cp);
                return None;
            };
            CounterTarget::Specific(ctx.interner.intern_counter(&norm))
        };
        if !c.eat_ci("on") {
            ctx.interner.rollback(cp);
            return None;
        }
        if !(c.eat_ci("it")
            || (c
                .peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && {
                    c.bump();
                    true
                }))
        {
            ctx.interner.rollback(cp);
            return None;
        }
        let target_ref = if is_pronoun {
            scope
                .it_ref()
                .unwrap_or(match scope.default_pronoun_subject() {
                    PronounSubject::AttachedTo => Ref::AttachedTo,
                    PronounSubject::SelfObj => Ref::SelfObj,
                })
        } else {
            Ref::SelfObj
        };
        Some((
            Condition::Compare {
                left: Box::new(Amount::CountersOn {
                    counter: ct,
                    of: target_ref,
                }),
                op,
                right: Box::new(val),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }
    // ObjectState: "~ is enchanted", "~ is equipped", "enchanted creature is tapped",
    // and AttachedPermanentMatches ("enchanted creature is white and untapped")
    if let Some((cond, used)) = cur.try_parse(|c| {
        let mut neg = false;
        let of = if c.eat_ci("it's") {
            neg = c.eat_ci("not");
            AttachmentOwnerTarget::SelfObj
        } else if c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            || c.eat_ci("it")
        {
            if c.peek()
                .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
            {
                c.bump();
            }
            if c.eat_ci("isn't") || c.eat_words(&["is", "not"]) {
                neg = true;
            } else if !c.eat_ci("is") {
                return None;
            }
            AttachmentOwnerTarget::SelfObj
        } else if c.eat_ci("this") && c.peek().is_some_and(|t| is_self_noun(t.raw)) {
            c.bump();
            if c.eat_ci("isn't") || c.eat_words(&["is", "not"]) {
                neg = true;
            } else if !c.eat_ci("is") {
                return None;
            }
            AttachmentOwnerTarget::SelfObj
        } else if (c.eat_ci("enchanted") || c.eat_ci("equipped"))
            && c.peek().is_some_and(|t| is_self_noun(t.raw))
        {
            c.bump();
            if c.eat_ci("isn't") || c.eat_words(&["is", "not"]) {
                neg = true;
            } else if !c.eat_ci("is") {
                return None;
            }
            AttachmentOwnerTarget::AttachedTo
        } else {
            return None;
        };

        if !neg
            && of == AttachmentOwnerTarget::AttachedTo
            && let Some(col_tok) = c.peek()
            && let Some(colors) = parse_color_word(col_tok.raw)
        {
            c.bump();
            let untapped = if c.eat_words(&["and", "untapped"]) {
                Some(true)
            } else if c.eat_words(&["and", "tapped"]) {
                Some(false)
            } else {
                None
            };
            return Some((
                Condition::AttachedPermanentMatches { colors, untapped },
                c.pos(),
            ));
        }

        let word_tok = c.bump()?;
        let st = if let Some(s) = parse_object_state_word_cond(of, word_tok.raw) {
            s
        } else if of == AttachmentOwnerTarget::AttachedTo
            && let Some(sup) = match word_tok.raw.to_ascii_lowercase().as_str() {
                "legendary" => Some(mtg_ir::SupertypeMask::LEGENDARY),
                "basic" => Some(mtg_ir::SupertypeMask::BASIC),
                "snow" => Some(mtg_ir::SupertypeMask::SNOW),
                "world" => Some(mtg_ir::SupertypeMask::WORLD),
                _ => None,
            }
        {
            Condition::ObjectState {
                of,
                tapped: None,
                attacking: None,
                blocking: None,
                monstrous: None,
                saddled: None,
                equipped: None,
                enchanted: None,
                filter: Some(Box::new(ObjectFilter {
                    supertypes_any: sup,
                    ..Default::default()
                })),
            }
        } else {
            return None;
        };
        let final_cond = if neg {
            Condition::Not { of: Box::new(st) }
        } else {
            st
        };
        Some((final_cond, c.pos()))
    }) {
        return Some((cond, used));
    }
    // SelfMatchesFilter: "(~|it|this <noun>|that <noun>) (is|was|is not|isn't|was not|wasn't) (a|an) <filter>" / "it's [not] (a|an) <filter>"
    if let Some((cond, used)) = cur.try_parse(|c| {
        let mut neg = false;
        let mut target_override: Option<Ref> = None;
        let mut explicit_none_target = false;
        if c.eat_ci("it's") {
            neg = c.eat_ci("not");
            if let Some(it) = scope.it_ref() {
                target_override = Some(it);
            } else if scope.has_reveal_drawn() {
                explicit_none_target = true;
            } else {
                target_override = Some(match scope.default_pronoun_subject() {
                    PronounSubject::AttachedTo => Ref::AttachedTo,
                    PronounSubject::SelfObj => Ref::SelfObj,
                });
            }
        } else if c
            .peek()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        {
            c.bump();
            if c.eat_ci("isn't")
                || c.eat_ci("wasn't")
                || c.eat_words(&["is", "not"])
                || c.eat_words(&["was", "not"])
            {
                neg = true;
            } else if !(c.eat_ci("is") || c.eat_ci("was")) {
                return None;
            }
        } else if c.eat_ci("it") {
            if let Some(it) = scope.it_ref() {
                target_override = Some(it);
            } else if scope.has_reveal_drawn() {
                explicit_none_target = true;
            } else {
                target_override = Some(match scope.default_pronoun_subject() {
                    PronounSubject::AttachedTo => Ref::AttachedTo,
                    PronounSubject::SelfObj => Ref::SelfObj,
                });
            }
            if c.eat_ci("isn't")
                || c.eat_ci("wasn't")
                || c.eat_words(&["is", "not"])
                || c.eat_words(&["was", "not"])
            {
                neg = true;
            } else if !(c.eat_ci("is") || c.eat_ci("was")) {
                return None;
            }
        } else if c.eat_ci("this") && c.peek().is_some_and(|t| is_self_noun(t.raw)) {
            c.bump();
            if c.eat_ci("isn't")
                || c.eat_ci("wasn't")
                || c.eat_words(&["is", "not"])
                || c.eat_words(&["was", "not"])
            {
                neg = true;
            } else if !(c.eat_ci("is") || c.eat_ci("was")) {
                return None;
            }
        } else if c.eat_ci("that") && c.peek().is_some_and(|t| is_self_noun(t.raw)) {
            let noun_raw = c.peek().map(|t| t.raw).unwrap_or("");
            let noun_is_spell = noun_raw.eq_ignore_ascii_case("spell");
            let trig_noun_match = scope.trigger_matches_that_noun(noun_raw);
            c.bump();
            target_override = Some(if trig_noun_match {
                Ref::EventObject
            } else {
                scope.it_ref().unwrap_or_else(|| {
                    if scope.in_trigger_context()
                        && (!scope.trigger_targets_self() || noun_is_spell)
                    {
                        Ref::EventObject
                    } else {
                        Ref::SelfObj
                    }
                })
            });
            if c.eat_ci("isn't")
                || c.eat_ci("wasn't")
                || c.eat_words(&["is", "not"])
                || c.eat_words(&["was", "not"])
            {
                neg = true;
            } else if !(c.eat_ci("is") || c.eat_ci("was")) {
                return None;
            }
        } else {
            return None;
        }
        let had_article = c.eat_ci("a") || c.eat_ci("an");
        let is_bare_supertype = !had_article
            && c.peek().is_some_and(|t| {
                matches!(
                    t.raw.to_ascii_lowercase().as_str(),
                    "legendary" | "basic" | "snow" | "world"
                )
            });
        if !had_article && !is_bare_supertype {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::empty(),
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        if is_bare_supertype && f_used != 1 {
            ctx.interner.rollback(cp);
            return None;
        }
        for _ in 0..f_used {
            c.bump();
        }
        let base = Condition::MatchesFilter {
            target: if explicit_none_target {
                None
            } else {
                Some(target_override.unwrap_or(Ref::SelfObj))
            },
            filter: Box::new(f),
            any: false,
        };
        let final_cond = if neg {
            Condition::Not { of: Box::new(base) }
        } else {
            base
        };
        Some((final_cond, c.pos()))
    }) {
        return Some((cond, used));
    }
    if cur.eat_words(&["you", "control", "a", "commander"])
        || cur.eat_words(&["you", "control", "your", "commander"])
    {
        return Some((Condition::ControlCommander, cur.pos()));
    }
    if cur.eat_words(&["you", "control", "an", "attacking", "legendary", "creature"]) {
        return Some((Condition::ControlAttackingLegendary, cur.pos()));
    }
    if cur.eat_words(&["you", "control", "no"]) {
        let cp = ctx.interner.checkpoint();
        if let Some((f, used)) = parse_object_filter(
            &tokens[cur.pos()..],
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) {
            return Some((
                Condition::Not {
                    of: Box::new(Condition::ControlsFilter {
                        filter: Box::new(f),
                    }),
                },
                cur.pos() + used,
            ));
        }
        ctx.interner.rollback(cp);
        cur.restore(0);
    }

    // 3. Turn-history conditions
    if cur.eat_words(&["you've", "been", "attacked", "this", "step"])
        || cur.eat_words(&["you've", "been", "attacked", "this", "combat"])
        || cur.eat_words(&["you", "were", "attacked", "this", "step"])
        || cur.eat_words(&["you", "were", "attacked", "this", "combat"])
        || cur.eat_words(&["you're", "being", "attacked"])
        || cur.eat_words(&["a", "creature", "is", "attacking", "you"])
    {
        return Some((Condition::YouWereAttackedThisCombat, cur.pos()));
    }
    if cur.eat_words(&["a", "creature", "died", "this", "turn"]) {
        return Some((Condition::CreatureDiedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["a", "creature", "you", "controlled", "died", "this", "turn"]) {
        return Some((Condition::CreatureDiedUnderYourControlThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "gained", "life", "this", "turn"]) {
        return Some((Condition::GainedLifeThisTurn { min: None }, cur.pos()));
    }
    if let Some((min_amt, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["you", "gained"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let (cmp, c_used) = parse_compare(c.remaining(), ctx, scope)?;
        if cmp.op != CompareOp::Ge {
            ctx.interner.rollback(cp);
            return None;
        }
        for _ in 0..c_used {
            c.bump();
        }
        if !c.eat_words(&["life", "this", "turn"]) {
            ctx.interner.rollback(cp);
            return None;
        }
        Some((cmp.value, c.pos()))
    }) {
        return Some((Condition::GainedLifeThisTurn { min: Some(min_amt) }, used));
    }
    if cur.eat_words(&["an", "opponent", "lost", "life", "this", "turn"]) {
        return Some((Condition::OpponentLostLifeThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "lost", "life", "this", "turn"]) {
        return Some((Condition::YouLostLifeThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "descended", "this", "turn"]) {
        return Some((Condition::DescendedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "committed", "a", "crime", "this", "turn"]) {
        return Some((Condition::CommittedCrimeThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "attacked", "this", "turn"]) {
        return Some((Condition::AttackedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["no", "creatures", "attacked", "this", "turn"]) {
        return Some((Condition::NoCreaturesAttackedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["no", "spells", "were", "cast", "this", "turn"]) {
        return Some((Condition::NoSpellCastThisTurn, cur.pos()));
    }
    if cur.eat_words(&[
        "you", "haven't", "cast", "a", "spell", "from", "your", "hand", "this", "turn",
    ]) {
        return Some((Condition::NoSpellCastFromHandThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "played", "a", "land", "this", "turn"]) {
        return Some((Condition::PlayedLandThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "created", "a", "token", "this", "turn"])
        || cur.eat_words(&["you've", "created", "a", "token", "this", "turn"])
    {
        return Some((Condition::CreatedTokenThisTurn, cur.pos()));
    }
    if cur.eat_words(&["you", "discarded", "a", "card", "this", "turn"]) {
        return Some((Condition::YouDiscardedThisTurn, cur.pos()));
    }
    if cur.eat_words(&["a", "card", "left", "your", "graveyard", "this", "turn"]) {
        return Some((Condition::CardLeftYourGraveyardThisTurn, cur.pos()));
    }
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !(c.eat_words(&["you've", "cast"]) || c.eat_words(&["you", "cast"])) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let min = if c.eat_ci("a") || c.eat_ci("an") {
            1u16
        } else if let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope)
            && cmp.op == CompareOp::Ge
            && let Amount::Const { n } = *cmp.value
            && n >= 1
            && let Ok(min_u16) = u16::try_from(n)
        {
            for _ in 0..c_used {
                c.bump();
            }
            min_u16
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        if c.eat_words(&["spell", "this", "turn"]) || c.eat_words(&["spells", "this", "turn"]) {
            return Some((
                Condition::SpellsCastThisTurnAtLeast { min, filter: None },
                c.pos(),
            ));
        }
        let Some((mut f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::empty(),
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..f_used {
            c.bump();
        }
        if !(c.eat_words(&["spell", "this", "turn"])
            || c.eat_words(&["spells", "this", "turn"])
            || c.eat_words(&["this", "turn"]))
        {
            ctx.interner.rollback(cp);
            return None;
        }
        f.zones = ZoneMask::empty();
        Some((
            Condition::SpellsCastThisTurnAtLeast {
                min,
                filter: Some(Box::new(f)),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }

    // 4. "you have N or more cards in hand / graveyard / life" / "you have no cards in hand"
    if cur.eat_words(&["you", "have", "no", "cards", "in", "hand"])
        || cur.eat_words(&["you", "have", "no", "cards", "in", "your", "hand"])
    {
        return Some((
            Condition::Compare {
                left: Box::new(Amount::CardsIn {
                    zone: Zone::Hand,
                    of: PlayerRef::You,
                }),
                op: CompareOp::Eq,
                right: Box::new(Amount::Const { n: 0 }),
            },
            cur.pos(),
        ));
    }
    if let Some((cond, used_cmp)) = cur.try_parse(|c| {
        if !c.eat_words(&["you", "have"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let Some((cmp, used)) = parse_compare(c.remaining(), ctx, scope) else {
            ctx.interner.rollback(cp);
            return None;
        };
        for _ in 0..used {
            c.bump();
        }
        if c.eat_ci("life") {
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::Life { of: PlayerRef::You }),
                    op: cmp.op,
                    right: cmp.value,
                },
                c.pos(),
            ));
        }
        let zone = if c.eat_words(&["cards", "in", "hand"])
            || c.eat_words(&["cards", "in", "your", "hand"])
        {
            Zone::Hand
        } else if c.eat_words(&["cards", "in", "your", "graveyard"]) {
            Zone::Graveyard
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        Some((
            Condition::Compare {
                left: Box::new(Amount::CardsIn {
                    zone,
                    of: PlayerRef::You,
                }),
                op: cmp.op,
                right: cmp.value,
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used_cmp));
    }

    // 5. "you control <N or more> <filter>" / "you control [a|an] <filter>"
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !c.eat_words(&["you", "control"]) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        // Try numeric comparison first: "you control three or more artifacts"
        if let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope)
            && !matches!(
                cmp,
                Compare {
                    op: CompareOp::Eq,
                    ref value
                } if matches!(**value, Amount::Const { n: 1 })
            )
        {
            for _ in 0..c_used {
                c.bump();
            }
            if cmp.op == CompareOp::Ge
                && let Amount::Const { n } = *cmp.value
                && n >= 1
                && c.eat_words(&["lands", "with", "the", "same", "name"])
            {
                let Ok(min) = u16::try_from(n) else {
                    ctx.interner.rollback(cp);
                    return None;
                };
                return Some((Condition::SameNameLandsControlledAtLeast { min }, c.pos()));
            }
            let Some((f, f_used)) = parse_object_filter(
                c.remaining(),
                ctx,
                FilterOpts {
                    default_zone: ZoneMask::BATTLEFIELD,
                    allow_plural_and_disjunction: true,
                    ..Default::default()
                },
                scope,
            ) else {
                ctx.interner.rollback(cp);
                return None;
            };
            for _ in 0..f_used {
                c.bump();
            }
            let mut count_filter = f;
            if count_filter.controller.is_none() {
                count_filter.controller = Some(RelativeControl::You);
            }
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::Count {
                        of: Box::new(count_filter),
                    }),
                    op: cmp.op,
                    right: cmp.value,
                },
                c.pos(),
            ));
        }
        ctx.interner.rollback(cp);

        // Plain "you control [a|an] <filter>"
        let cp2 = ctx.interner.checkpoint();
        let _ = c.eat_ci("a") || c.eat_ci("an");
        let Some((mut f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp2);
            return None;
        };
        for _ in 0..f_used {
            c.bump();
        }
        if f.controller.is_none() {
            f.controller = Some(RelativeControl::You);
        }
        Some((
            Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: Box::new(Amount::Const { n: 1 }),
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }

    // 6. "there are <cmp> card types among cards in your graveyard" (Delirium) /
    //    "there are <cmp> cards in your graveyard" / "there are <cmp> <filter>"
    if let Some((cond, used)) = cur.try_parse(|c| {
        if !(c.eat_words(&["there", "are"]) || c.eat_words(&["there", "is"])) {
            return None;
        }
        let cp = ctx.interner.checkpoint();
        let (cmp_op, cmp_val) = if c.eat_ci("a") || c.eat_ci("an") {
            (CompareOp::Ge, Box::new(Amount::Const { n: 1 }))
        } else if let Some((cmp, c_used)) = parse_compare(c.remaining(), ctx, scope) {
            for _ in 0..c_used {
                c.bump();
            }
            (cmp.op, cmp.value)
        } else {
            ctx.interner.rollback(cp);
            return None;
        };
        if c.eat_words(&["card", "types"]) {
            let _ = c.eat_ci("are");
            if c.eat_words(&["among", "cards", "in", "your", "graveyard"]) {
                return Some((
                    Condition::Compare {
                        left: Box::new(Amount::DistinctCardTypesInZone {
                            zone: Zone::Graveyard,
                            whose: ZoneScopeWhose::You,
                        }),
                        op: cmp_op,
                        right: cmp_val,
                    },
                    c.pos(),
                ));
            }
            if c.eat_words(&["among", "cards", "in", "all", "graveyards"])
                || c.eat_words(&["among", "cards", "in", "graveyards"])
            {
                return Some((
                    Condition::Compare {
                        left: Box::new(Amount::DistinctCardTypesInZone {
                            zone: Zone::Graveyard,
                            whose: ZoneScopeWhose::All,
                        }),
                        op: cmp_op,
                        right: cmp_val,
                    },
                    c.pos(),
                ));
            }
            ctx.interner.rollback(cp);
            return None;
        }
        if c.eat_words(&["cards", "in", "your", "graveyard"]) {
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::CardsIn {
                        zone: Zone::Graveyard,
                        of: PlayerRef::You,
                    }),
                    op: cmp_op,
                    right: cmp_val,
                },
                c.pos(),
            ));
        }
        if c.eat_words(&["counters", "on", "it"])
            || c.eat_words(&["counter", "on", "it"])
            || (c.eat_ci("counters")
                && c.eat_ci("on")
                && c.peek()
                    .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
                && {
                    c.bump();
                    true
                })
        {
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::CountersOn {
                        counter: CounterTarget::Any,
                        of: scope.it_ref().unwrap_or(Ref::SelfObj),
                    }),
                    op: cmp_op,
                    right: cmp_val,
                },
                c.pos(),
            ));
        }
        if let Some((f, f_used)) = parse_object_filter(
            c.remaining(),
            ctx,
            FilterOpts {
                default_zone: ZoneMask::BATTLEFIELD,
                allow_plural_and_disjunction: true,
                ..Default::default()
            },
            scope,
        ) {
            for _ in 0..f_used {
                c.bump();
            }
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::Count { of: Box::new(f) }),
                    op: cmp_op,
                    right: cmp_val,
                },
                c.pos(),
            ));
        }
        ctx.interner.rollback(cp);
        None
    }) {
        return Some((cond, used));
    }

    if let Some((cond, used)) = cur.try_parse(|c| {
        let cp = ctx.interner.checkpoint();
        let tlen = scope.prior_targets_len();
        let (cmp, c_used) = parse_compare(c.remaining(), ctx, scope)?;
        if matches!(cmp.op, CompareOp::Eq) && matches!(*cmp.value, Amount::Const { n: 1 }) {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        }
        for _ in 0..c_used {
            c.bump();
        }
        if c.eat_words(&["cards", "are", "in", "your", "graveyard"]) {
            return Some((
                Condition::Compare {
                    left: Box::new(Amount::CardsIn {
                        zone: Zone::Graveyard,
                        of: PlayerRef::You,
                    }),
                    op: cmp.op,
                    right: cmp.value,
                },
                c.pos(),
            ));
        }
        let rem = c.remaining();
        let Some(are_pos) = rem.windows(3).position(|w| {
            (w[0].eq_ci("are") || w[0].eq_ci("is")) && w[1].eq_ci("in") && w[2].eq_ci("your")
        }) else {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        };
        if are_pos == 0 {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        }
        let Some((mut f, f_used)) = parse_object_filter(
            &rem[..are_pos],
            ctx,
            FilterOpts {
                default_zone: ZoneMask::GRAVEYARD,
                allow_plural_and_disjunction: true,
                strip_trailing_card: true,
                ..Default::default()
            },
            scope,
        ) else {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        };
        if f_used != are_pos {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        }
        for _ in 0..are_pos {
            c.bump();
        }
        let _ = c.eat_ci("are") || c.eat_ci("is");
        if !c.eat_words(&["in", "your", "graveyard"]) {
            ctx.interner.rollback(cp);
            scope.truncate_targets(tlen);
            return None;
        }
        f.zones = ZoneMask::GRAVEYARD;
        if f.owner.is_none() && f.controller.is_none() {
            f.owner = Some(RelativeControl::You);
        }
        Some((
            Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: cmp.op,
                right: cmp.value,
            },
            c.pos(),
        ))
    }) {
        return Some((cond, used));
    }

    // 7. Generic `Condition::Compare`: "<lhs_amount> <compare_op> <rhs_amount>"
    let cp = ctx.interner.checkpoint();
    if let Some((lhs, l_used)) = parse_amount(tokens, ctx, scope)
        && let Some((cmp, r_used)) = parse_compare(&tokens[l_used..], ctx, scope)
    {
        return Some((
            Condition::Compare {
                left: Box::new(lhs),
                op: cmp.op,
                right: cmp.value,
            },
            l_used + r_used,
        ));
    }
    ctx.interner.rollback(cp);

    None
}

#[allow(dead_code)]
fn _keep_unused_imports_referenced() {
    let _ = (
        AttachmentOwnerTarget::SelfObj,
        CardTypeMask::empty(),
        CostPaidCountKind::Exiled,
        CountedThisWayKind::RevealedThisWay,
        LifeCompareWho::You,
        ManaColor::W,
        ManaSourceKind::Treasure,
        NamedAltCostKind::Surge,
        ObjectColorTarget::SelfObj,
        OpponentComparisonMetric::Life,
        OpponentMetricKind::Life,
        PlayerCounterKind::Poison,
        PlayerCounterScope::You,
        SacrificedStatKind::Power,
        SpellsLastTurnMode::None,
        ZoneScopeWhose::You,
        singular_subtype("Elves"),
        parse_mana_color_word("white"),
    );
}
